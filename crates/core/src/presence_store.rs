//! Presence-probe sqlx store (TOG-10092).
//!
//! Ports the `presence_probe` row moves from legacy `src/jobs/presenceProbe.ts`
//! (`recordReading`, `readSeries`, `lastBotFloorAt`) onto the `0310` table.
//! The pure cycle decision stays in [`crate::presence`]: this module only
//! moves rows, so the SQL is a thin transliteration of the legacy queries.
//!
//! Behind the `db` feature so domain unit tests never need a Postgres driver.
//! The S4 REST executor (TOG-10076) supplies the guild-counts reading and the
//! S4 interaction router (TOG-10075) owns scheduling; until they land, the
//! outcome types ([`ProbeDecision`]) are the integration surface — no private
//! dispatcher or HTTP client lives here.
//!
//! Legacy table/column names are kept exactly. `observed_at` is TEXT holding
//! ISO-8601 UTC, so ordering is lexicographic (correct for that format).

use sqlx::{Pool, Postgres};

use super::presence::{PresenceReading, ProbeDecision};

/// Presence store failure: transport only. A malformed count can never come
/// back — the column CHECK plus the domain's
/// [`sanitize_presence_count`](crate::presence::sanitize_presence_count) gate keep rows clean.
#[derive(Debug, thiserror::Error)]
pub enum PresenceStoreError {
    #[error("presence store database error: {0}")]
    Db(#[from] sqlx::Error),
}

/// Insert one reading (legacy `recordReading`): `ON CONFLICT DO NOTHING` on
/// `(guild_id, observed_at)` so a retried cycle never double-counts.
pub async fn record_reading(
    pool: &Pool<Postgres>,
    guild_id: &str,
    decision: ProbeDecision,
    observed_at: &str,
) -> Result<bool, PresenceStoreError> {
    let (presence, bot_floor) = match decision {
        ProbeDecision::Skip => return Ok(false),
        ProbeDecision::Record {
            presence,
            bot_floor,
        } => (presence, bot_floor),
    };
    let inserted = sqlx::query(
        "INSERT INTO presence_probe (guild_id, observed_at, approximate_presence_count, bot_floor)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (guild_id, observed_at) DO NOTHING",
    )
    .bind(guild_id)
    .bind(observed_at)
    .bind(presence)
    .bind(bot_floor)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(inserted == 1)
}

/// The whole series for a guild, oldest first (legacy `readSeries`).
pub async fn read_series(
    pool: &Pool<Postgres>,
    guild_id: &str,
) -> Result<Vec<PresenceReading>, PresenceStoreError> {
    let rows: Vec<(String, i32, Option<i32>)> = sqlx::query_as(
        "SELECT observed_at, approximate_presence_count, bot_floor
           FROM presence_probe
          WHERE guild_id = $1
          ORDER BY observed_at ASC",
    )
    .bind(guild_id)
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for (observed_at, presence, bot_floor) in rows {
        let Some(observed_at_ms) = super::funnel::parse_iso_millis(&observed_at) else {
            continue;
        };
        out.push(PresenceReading {
            observed_at_ms,
            presence: i64::from(presence),
            bot_floor: bot_floor.map(i64::from),
        });
    }
    Ok(out)
}

/// When the newest actually-observed bot floor was taken, or `None` if never
/// (legacy `lastBotFloorAt`).
pub async fn last_bot_floor_at(
    pool: &Pool<Postgres>,
    guild_id: &str,
) -> Result<Option<String>, PresenceStoreError> {
    let at: Option<String> = sqlx::query_scalar(
        "SELECT MAX(observed_at) FROM presence_probe
          WHERE guild_id = $1 AND bot_floor IS NOT NULL",
    )
    .bind(guild_id)
    .fetch_one(pool)
    .await?;
    Ok(at)
}

#[cfg(test)]
mod tests {
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

    use super::super::presence::{
        decide_probe_cycle, evaluate_trigger, TriggerOptions, REOPEN_PEAK_THRESHOLD,
    };
    use super::*;

    /// Only an absent URL skips the test. Configured setup failures are errors.
    async fn test_pool(prefix: &str) -> Result<Option<(Pool<Postgres>, String)>, sqlx::Error> {
        let url = match std::env::var("TWO_TEST_DATABASE_URL") {
            Ok(url) => url,
            Err(std::env::VarError::NotPresent) => return Ok(None),
            Err(_) => panic!("TWO_TEST_DATABASE_URL must be Unicode"),
        };
        test_pool_with_url(prefix, &url).await.map(Some)
    }

    async fn test_pool_with_url(
        prefix: &str,
        url: &str,
    ) -> Result<(Pool<Postgres>, String), sqlx::Error> {
        let options: PgConnectOptions = url.parse()?;
        // No DATABASE_URL or inherited credentials: tests accept only the
        // approved agent container or the ephemeral Postgres service in
        // GitHub Actions (see the `community-db` CI job).
        let host = options.get_host();
        assert!(
            host == "agent-testdb"
                || (host == "localhost"
                    && std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true")),
            "test-container-only URL"
        );
        assert_eq!(
            options.get_username(),
            "agent_test",
            "test-container-only user"
        );
        // No shared fixed-name fixtures: each invocation owns its schema,
        // including concurrent invocations of the same test in other processes.
        static NEXT_SCHEMA: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let schema = format!(
            "{prefix}_{}_{nonce}_{}",
            std::process::id(),
            NEXT_SCHEMA.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        assert!(is_test_schema_name(&schema), "safe unique test schema");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await?;
        // SAFETY: only the guarded identifier interpolates; values bind.
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await?;
        admin.close().await;
        let pool = PgPoolOptions::new()
            .max_connections(3)
            .connect_with(options.options([("search_path", schema.as_str())]))
            .await?;
        // Execute the entire migration, including multi-statement DDL.
        sqlx::raw_sql(include_str!(
            "../../cutover/migrations/0310_presence_probe.sql"
        ))
        .execute(&pool)
        .await?;
        Ok((pool, schema))
    }

    /// Test-only schema names: lowercase identifier characters, so DDL
    /// interpolation cannot break out of the identifier position.
    fn is_test_schema_name(schema: &str) -> bool {
        !schema.is_empty()
            && schema.len() <= 63
            && schema
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    }

    async fn drop_schema(pool: &Pool<Postgres>, schema: &str) {
        assert!(is_test_schema_name(schema));
        // SAFETY: guarded by `is_test_schema_name` (see `test_pool`).
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(pool)
            .await
            .expect("clean up owned test schema");
        pool.close().await;
    }

    #[tokio::test]
    async fn probe_cycle_round_trips_through_postgres() {
        let Some((pool, schema)) = test_pool("tog_10092_presence")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping presence_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        // A skipped decision writes nothing — the series reads as the times
        // we successfully looked.
        assert!(!record_reading(
            &pool,
            "guild-a",
            decide_probe_cycle(None, None, None, 1_000),
            "2026-09-07T06:15:00.000Z",
        )
        .await
        .expect("skip writes nothing"));
        assert_eq!(
            last_bot_floor_at(&pool, "guild-a")
                .await
                .expect("floor lookup"),
            None
        );
        // First cycle: no floor observed yet, so the rescan lands on the row.
        let first = decide_probe_cycle(Some(42), None, Some(23), 1_000);
        assert!(
            record_reading(&pool, "guild-a", first, "2026-09-07T06:15:00.000Z")
                .await
                .expect("record first")
        );
        // Retried cycle: the (guild, observed_at) key dedupes.
        assert!(
            !record_reading(&pool, "guild-a", first, "2026-09-07T06:15:00.000Z")
                .await
                .expect("retry dedupes")
        );
        // Hourly tick without a rescan: floor stays NULL, not zero.
        let second = decide_probe_cycle(Some(30), Some(1_000), Some(99), 2_000);
        assert!(
            record_reading(&pool, "guild-a", second, "2026-09-07T07:15:00.000Z")
                .await
                .expect("record second")
        );
        let series = read_series(&pool, "guild-a").await.expect("read back");
        assert_eq!(series.len(), 2);
        assert_eq!((series[0].presence, series[0].bot_floor), (42, Some(23)));
        assert_eq!((series[1].presence, series[1].bot_floor), (30, None));
        assert_eq!(
            last_bot_floor_at(&pool, "guild-a")
                .await
                .expect("floor lookup"),
            Some("2026-09-07T06:15:00.000Z".to_owned())
        );
        // Guild isolation: another guild reads an empty series.
        assert!(read_series(&pool, "guild-b")
            .await
            .expect("isolated")
            .is_empty());
        drop_schema(&pool, &schema).await;
    }

    #[tokio::test]
    async fn stored_series_feeds_the_reopen_trigger() {
        let Some((pool, schema)) = test_pool("tog_10092_trigger")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping presence_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        // Ten daily readings, every third day at the threshold: 4 qualifying
        // days in the window, so the trigger arms (web_v1 not live).
        for day in 0..10 {
            let presence = if day % 3 == 0 {
                REOPEN_PEAK_THRESHOLD
            } else {
                30
            };
            let at = format!("2026-09-{:02}T12:00:00.000Z", day + 1);
            record_reading(
                &pool,
                "guild-a",
                decide_probe_cycle(Some(presence), Some(0), None, i64::from(day)),
                &at,
            )
            .await
            .expect("record day");
        }
        let series = read_series(&pool, "guild-a").await.expect("read back");
        assert_eq!(series.len(), 10);
        let verdict = evaluate_trigger(
            &series,
            super::super::funnel::parse_iso_millis("2026-09-12T00:00:00.000Z")
                .expect("window end parses"),
            TriggerOptions::default(),
        );
        assert_eq!(verdict.status, super::super::presence::TriggerStatus::Armed);
        assert_eq!(verdict.qualifying_days, 4);
        drop_schema(&pool, &schema).await;
    }
}
