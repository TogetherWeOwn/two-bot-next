//! Voice-heat hourly aggregate sqlx store (contract v2 B5).
//!
//! Durable half of the voice-heat pipeline: the bot sampler records
//! guild-wide `total_in_voice` (AFK- and bot-excluded, the `voice.snapshot`
//! definition) every five minutes and flushes completed UTC hours here as
//! [`HourBucket`] rows. The table carries no member id, channel id or
//! per-room split by construction, so there is nothing to erase and no
//! per-member reader.
//!
//! Behind the `db` feature so domain unit tests never need a Postgres
//! driver. Rows are immutable once flushed: the insert claims the hour with
//! `ON CONFLICT DO NOTHING`, so a retried flush after a drained accumulator
//! writes nothing and never double-counts. Retention prunes rows older than
//! [`VOICE_HEAT_RETENTION_DAYS`]; the `web_v1.voice_heat` view publishes the
//! trailing 28 complete days.

use sqlx::{Pool, Postgres};
use time::OffsetDateTime;

/// One completed UTC hour of sampler output: how many five-minute samples
/// the hour holds and their summed guild-wide occupancy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HourBucket {
    /// Truncated to the UTC hour start before flushing.
    pub hour_start: OffsetDateTime,
    /// Five-minute samples recorded in the hour (at most 12).
    pub samples: i64,
    /// Summed `total_in_voice` across those samples.
    pub occupancy_sum: i64,
}

/// How long flushed hourly rows survive: 35 rolling days, so the 28-day
/// publication window keeps a one-week buffer against late retention ticks.
pub const VOICE_HEAT_RETENTION_DAYS: i32 = 35;

/// Widest horizon the prune accepts: a century, far inside the
/// `timestamptz` range, so an absurd value can never push
/// `now() - make_interval(days => $1)` out of range and fail every
/// retention tick.
pub const MAX_VOICE_HEAT_RETENTION_DAYS: i32 = 36_500;

/// Voice-heat store failure: transport only. A malformed bucket can never
/// come back — the column CHECKs refuse negative counts.
#[derive(Debug, thiserror::Error)]
pub enum VoiceHeatStoreError {
    #[error("voice-heat store database error: {0}")]
    Db(#[from] sqlx::Error),
}

/// Truncate a sample timestamp to its UTC hour start. Pure over the clock so
/// bucketing pins without a database.
#[must_use]
pub fn truncate_to_hour(sample_at: OffsetDateTime) -> OffsetDateTime {
    let hour = sample_at.unix_timestamp().div_euclid(3_600) * 3_600;
    OffsetDateTime::from_unix_timestamp(hour).expect("hour truncation stays in range")
}

/// Flush completed hourly buckets. Each row claims its hour with
/// `ON CONFLICT DO NOTHING`: the insert is idempotent, so a retried flush
/// after a drained accumulator writes nothing and never double-counts.
/// Empty input writes nothing and reports zero rows.
pub async fn flush_hour_buckets(
    pool: &Pool<Postgres>,
    buckets: &[HourBucket],
) -> Result<u64, VoiceHeatStoreError> {
    if buckets.is_empty() {
        return Ok(0);
    }
    let mut written = 0u64;
    for bucket in buckets {
        written = written.saturating_add(
            sqlx::query(
                "INSERT INTO voice_heat_hours (hour_start_utc, samples, occupancy_sum)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (hour_start_utc) DO NOTHING",
            )
            .bind(bucket.hour_start)
            .bind(bucket.samples.max(0))
            .bind(bucket.occupancy_sum.max(0))
            .execute(pool)
            .await?
            .rows_affected(),
        );
    }
    Ok(written)
}

/// Refuse horizons outside `1..=MAX_VOICE_HEAT_RETENTION_DAYS`
/// (fail-closed): a sub-day value must not widen into a full-table wipe,
/// and an unrepresentable one must not push the interval arithmetic out of
/// range.
fn check_horizon(retention_days: i32) -> Result<(), VoiceHeatStoreError> {
    if (1..=MAX_VOICE_HEAT_RETENTION_DAYS).contains(&retention_days) {
        Ok(())
    } else {
        Err(VoiceHeatStoreError::Db(sqlx::Error::InvalidArgument(
            "bad horizon".into(),
        )))
    }
}

/// Rows already past the retention horizon, oldest first. Same predicate as
/// the delete in [`prune_voice_heat`], so the count is the dry-run preview
/// behind it.
pub async fn count_stale_voice_heat(
    pool: &Pool<Postgres>,
    retention_days: i32,
) -> Result<u64, VoiceHeatStoreError> {
    check_horizon(retention_days)?;
    let stale: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM voice_heat_hours
          WHERE hour_start_utc < now() - make_interval(days => $1)",
    )
    .bind(retention_days)
    .fetch_one(pool)
    .await?;
    Ok(stale.max(0) as u64)
}

/// Delete rows older than `retention_days`, reporting how many went. The
/// predicate matches [`count_stale_voice_heat`], so dry runs preview the
/// same rows the execute pass deletes.
pub async fn prune_voice_heat(
    pool: &Pool<Postgres>,
    retention_days: i32,
) -> Result<u64, VoiceHeatStoreError> {
    check_horizon(retention_days)?;
    let deleted = sqlx::query(
        "DELETE FROM voice_heat_hours
          WHERE hour_start_utc < now() - make_interval(days => $1)",
    )
    .bind(retention_days)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

    #[test]
    fn truncates_samples_to_the_utc_hour() {
        let hour_start = OffsetDateTime::from_unix_timestamp(19 * 3_600).expect("fixture base");
        assert_eq!(
            truncate_to_hour(
                hour_start + time::Duration::minutes(37) + time::Duration::seconds(42)
            ),
            hour_start
        );
        assert_eq!(truncate_to_hour(hour_start), hour_start);
    }

    #[test]
    fn rejects_absurd_horizons_fail_closed() {
        for days in [0, -1, MAX_VOICE_HEAT_RETENTION_DAYS + 1] {
            assert!(
                check_horizon(days).is_err(),
                "horizon {days} must refuse, never widen"
            );
        }
        assert!(check_horizon(VOICE_HEAT_RETENTION_DAYS).is_ok());
    }

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
        // The migration also carries the guarded 1.0 -> 1.1 contract-version
        // bump, so the fixture owns the version row it updates.
        sqlx::query(
            "CREATE TABLE web_contract_meta (
               singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
               contract_version TEXT NOT NULL
             );
             INSERT INTO web_contract_meta (singleton, contract_version)
             VALUES (TRUE, '1.0')",
        )
        .execute(&pool)
        .await?;
        sqlx::raw_sql(include_str!(
            "../../cutover/migrations/0472_voice_heat_hours.sql"
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

    async fn drop_schema(pool: Pool<Postgres>, schema: &str) {
        assert!(is_test_schema_name(schema));
        let schema = schema.to_owned();
        // SAFETY: guarded by `is_test_schema_name` (see `test_pool`).
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&pool)
            .await
            .expect("clean up owned test schema");
        pool.close().await;
    }

    fn bucket(hours_ago: i64, samples: i64, sum: i64) -> HourBucket {
        use time::Duration;
        HourBucket {
            hour_start: truncate_to_hour(OffsetDateTime::now_utc() - Duration::hours(hours_ago)),
            samples,
            occupancy_sum: sum,
        }
    }

    #[tokio::test]
    async fn flush_is_idempotent_and_empty_writes_nothing() {
        let Some((pool, schema)) = test_pool("tog_voice_heat")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping voice_heat_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        assert_eq!(
            flush_hour_buckets(&pool, &[]).await.expect("empty write"),
            0
        );
        let buckets = vec![bucket(3, 12, 48), bucket(4, 11, 22)];
        assert_eq!(
            flush_hour_buckets(&pool, &buckets)
                .await
                .expect("first flush"),
            2
        );
        // A retried flush after the drain claims nothing, never double-counts.
        assert_eq!(
            flush_hour_buckets(&pool, &buckets)
                .await
                .expect("retry flush"),
            0
        );
        let stored: Vec<(i64, i64)> =
            sqlx::query_as("SELECT samples, occupancy_sum FROM voice_heat_hours")
                .fetch_all(&pool)
                .await
                .expect("read rows");
        assert_eq!(stored.len(), 2);
        drop_schema(pool, &schema).await;
    }

    #[tokio::test]
    async fn prune_keeps_fresh_rows_and_counts_stale_ones() {
        let Some((pool, schema)) = test_pool("tog_voice_heat_prune")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping voice_heat_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        sqlx::query(
            "INSERT INTO voice_heat_hours (hour_start_utc, samples, occupancy_sum)
             VALUES (now() - make_interval(days => 40), 12, 30),
                    (now() - make_interval(days => 10), 12, 24)",
        )
        .execute(&pool)
        .await
        .expect("seed rows");
        assert_eq!(
            count_stale_voice_heat(&pool, VOICE_HEAT_RETENTION_DAYS)
                .await
                .expect("dry-run count"),
            1
        );
        // A refused horizon deletes nothing, not everything.
        assert!(
            prune_voice_heat(&pool, 0).await.is_err(),
            "zero-day horizon must refuse"
        );
        assert_eq!(
            prune_voice_heat(&pool, VOICE_HEAT_RETENTION_DAYS)
                .await
                .expect("prune"),
            1
        );
        let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM voice_heat_hours")
            .fetch_one(&pool)
            .await
            .expect("count kept");
        assert_eq!(kept, 1, "fresh rows survive");
        drop_schema(pool, &schema).await;
    }
}
