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
    /// Latest actual valid observation included in the hour, not flush time.
    /// Present iff `samples > 0`, including true zero-occupancy observations.
    pub last_observed_at: Option<OffsetDateTime>,
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
                "INSERT INTO voice_heat_hours (hour_start_utc, samples, occupancy_sum, last_observed_at)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (hour_start_utc) DO NOTHING",
            )
            .bind(bucket.hour_start)
            .bind(bucket.samples)
            .bind(bucket.occupancy_sum)
            .bind(bucket.last_observed_at)
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
}
