//! Inactivity sweep sqlx store (TOG-10092).
//!
//! Ports the `flagInactive()` write path from legacy `src/jobs/inactivity.ts`
//! onto the funnel `events`/`members` tables (0001, S6-owned): select the
//! quiet members, record one `member_inactive` event each, project
//! `inactive_flagged_at`. The pure selection stays in [`crate::inactivity`]:
//! this module only moves rows, so the SQL is a thin transliteration of the
//! legacy query.
//!
//! Behind the `db` feature so domain unit tests never need a Postgres driver.
//! The hourly scheduler calls [`run_sweep`]; until the S4 router slice lands,
//! the outcome type is the integration surface — no private dispatcher lives
//! here, and nothing here messages anybody (read-only contract, see
//! [`crate::inactivity`]).
//!
//! Legacy table/column names are kept exactly. Timestamps bind as ISO-8601
//! UTC with `::timestamptz` casts (repo convention from 0001/0002).

use sqlx::{Pool, Postgres};

use super::inactivity::{
    flag_inactive, InactivityCandidate, InactivityOutcome, INACTIVITY_EVENT_SOURCE,
};

/// Inactivity store failure: transport only.
#[derive(Debug, thiserror::Error)]
pub enum InactivityStoreError {
    #[error("inactivity store database error: {0}")]
    Db(#[from] sqlx::Error),
}

/// One hourly sweep (legacy `flagInactive`): select members still present,
/// not bots, quiet since before the cutoff, and either never flagged or last
/// flagged before the cutoff; record one `member_inactive` event each and
/// return their ids. `days` is the `TWO_INACTIVITY_DAYS` threshold, `now` the
/// sweep time as ISO-8601 UTC.
pub async fn run_sweep(
    pool: &Pool<Postgres>,
    now: &str,
    days: u64,
) -> Result<InactivityOutcome, InactivityStoreError> {
    let parse = super::funnel::parse_iso_millis;
    let format = super::funnel::format_iso_millis;
    let now_ms = parse(now).unwrap_or(0);
    // The cutoff is computed in Rust (legacy `Date.now() - days * 86_400_000`)
    // and binds as one ISO timestamp: no interval arithmetic in SQL, and the
    // same millisecond feeds both the query and the outcome stamp.
    let cutoff = format(super::inactivity::inactivity_cutoff_ms(now_ms, days));
    /// One sweep row: guild, member, last-seen, flagged-at, bot flag.
    /// Timestamps decode as `OffsetDateTime` and format in Rust (the
    /// `rsvp_store` `iso_millis` pattern): no `to_char` quoting in SQL.
    type SweepRow = (
        String,
        String,
        Option<sqlx::types::time::OffsetDateTime>,
        Option<sqlx::types::time::OffsetDateTime>,
        bool,
    );
    let rows: Vec<SweepRow> = sqlx::query_as(
        "SELECT guild_id, member_id, COALESCE(last_active_at, joined_at),
                inactive_flagged_at, is_bot
           FROM members
          WHERE left_at IS NULL
            AND NOT is_bot
            AND COALESCE(last_active_at, joined_at) < $1::timestamptz
            AND (inactive_flagged_at IS NULL OR inactive_flagged_at < $1::timestamptz)",
    )
    .bind(&cutoff)
    .fetch_all(pool)
    .await?;
    /// `OffsetDateTime` to epoch millis for the shared ISO formatter.
    fn epoch_millis(dt: &sqlx::types::time::OffsetDateTime) -> i64 {
        dt.unix_timestamp() * 1000 + i64::from(dt.millisecond())
    }
    let candidates: Vec<InactivityCandidate> = rows
        .into_iter()
        .map(
            |(guild_id, member_id, last_seen, flagged_at, is_bot)| InactivityCandidate {
                guild_id,
                member_id,
                last_seen_ms: last_seen.as_ref().map(epoch_millis),
                flagged_at_ms: flagged_at.as_ref().map(epoch_millis),
                is_bot,
                has_left: false,
            },
        )
        .collect();
    let outcome = flag_inactive(&candidates, now_ms, days);
    for flagged in &outcome.flagged {
        let key = super::inactivity::member_inactive_event_key(
            &flagged.guild_id,
            &flagged.member_id,
            &flagged.occurred_at,
        );
        let mut tx = pool.begin().await?;
        let inserted: Option<(i64,)> = sqlx::query_as(
            "INSERT INTO events (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key)
             VALUES ('member_inactive', $1, $2, $3::timestamptz, $4, $5, $6)
             ON CONFLICT (idempotency_key) DO NOTHING RETURNING id",
        )
        .bind(&flagged.member_id)
        .bind(&flagged.guild_id)
        .bind(&flagged.occurred_at)
        .bind(INACTIVITY_EVENT_SOURCE)
        .bind(format!("{{\"thresholdDays\":{days}}}"))
        .bind(&key)
        .fetch_optional(&mut *tx)
        .await?;
        if inserted.is_some() {
            sqlx::query(
                "UPDATE members SET inactive_flagged_at = $1::timestamptz
                  WHERE guild_id = $2 AND member_id = $3",
            )
            .bind(&flagged.occurred_at)
            .bind(&flagged.guild_id)
            .bind(&flagged.member_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

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
        // The sweep reads/writes the S6-owned funnel tables; this test needs
        // 0001 only (0310/0311 are unrelated). `members.is_bot` defaults
        // FALSE per 0001, matching legacy.
        sqlx::raw_sql(include_str!("../../cutover/migrations/0001_funnel.sql"))
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

    async fn seed_member(
        pool: &Pool<Postgres>,
        member: &str,
        joined_at: &str,
        last_active_at: Option<&str>,
        is_bot: bool,
        left_at: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO members (guild_id, member_id, joined_at, last_active_at, is_bot, left_at)
             VALUES ('guild-a', $1, $2::timestamptz, $3::timestamptz, $4, $5::timestamptz)",
        )
        .bind(member)
        .bind(joined_at)
        .bind(last_active_at)
        .bind(is_bot)
        .bind(left_at)
        .execute(pool)
        .await
        .expect("seed member");
    }

    #[tokio::test]
    async fn sweep_flags_quiet_members_once_and_never_messages() {
        let Some((pool, schema)) = test_pool("tog_10092_inactivity")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping inactivity_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        seed_member(
            &pool,
            "quiet",
            "2026-07-01T00:00:00.000Z",
            None,
            false,
            None,
        )
        .await;
        seed_member(
            &pool,
            "active",
            "2026-07-01T00:00:00.000Z",
            Some("2026-09-06T00:00:00.000Z"),
            false,
            None,
        )
        .await;
        seed_member(&pool, "bot", "2026-07-01T00:00:00.000Z", None, true, None).await;
        seed_member(
            &pool,
            "left",
            "2026-07-01T00:00:00.000Z",
            None,
            false,
            Some("2026-08-01T00:00:00.000Z"),
        )
        .await;
        let first = run_sweep(&pool, "2026-09-07T00:00:00.000Z", 14)
            .await
            .expect("first sweep");
        assert_eq!(
            first
                .flagged
                .iter()
                .map(|f| f.member_id.as_str())
                .collect::<Vec<_>>(),
            ["quiet"]
        );
        // The event row exists with the legacy repeatable key shape; the
        // projection stamps `inactive_flagged_at`.
        let events: Vec<(String,)> = sqlx::query_as(
            "SELECT idempotency_key FROM events WHERE event_type = 'member_inactive'",
        )
        .fetch_all(&pool)
        .await
        .expect("event rows");
        assert_eq!(events.len(), 1);
        let flagged_at: (Option<String>,) = sqlx::query_as(
            "SELECT to_char(inactive_flagged_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')
               FROM members WHERE guild_id = 'guild-a' AND member_id = 'quiet'",
        )
        .fetch_one(&pool)
        .await
        .expect("flag timestamp");
        assert_eq!(flagged_at.0.as_deref(), Some("2026-09-07T00:00:00.000Z"));
        // Second sweep: no double-flag, no new rows. Read-only apart from the
        // first flag — and no message, DM, or ping is ever emitted (the
        // outcome type carries no notification surface; assert the events
        // table holds exactly the one flag row).
        let second = run_sweep(&pool, "2026-09-07T01:00:00.000Z", 14)
            .await
            .expect("second sweep");
        assert!(second.flagged.is_empty());
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events")
            .fetch_one(&pool)
            .await
            .expect("event count");
        assert_eq!(count.0, 1);
        drop_schema(&pool, &schema).await;
    }

    #[tokio::test]
    async fn sweep_with_empty_window_writes_no_events() {
        let Some((pool, schema)) = test_pool("tog_15759_inactivity_empty")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping inactivity_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        // Nobody is quiet: one recently active member and one bot. The legacy
        // `flagInactive` still selects zero rows and writes zero events — an
        // empty window is a successful no-op, not an error.
        seed_member(
            &pool,
            "active",
            "2026-07-01T00:00:00.000Z",
            Some("2026-09-06T00:00:00.000Z"),
            false,
            None,
        )
        .await;
        seed_member(&pool, "bot", "2026-07-01T00:00:00.000Z", None, true, None).await;
        let outcome = run_sweep(&pool, "2026-09-07T00:00:00.000Z", 14)
            .await
            .expect("empty sweep");
        assert!(outcome.flagged.is_empty());
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events")
            .fetch_one(&pool)
            .await
            .expect("event count");
        assert_eq!(count.0, 0);
        drop_schema(&pool, &schema).await;
    }
}
