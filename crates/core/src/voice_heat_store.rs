//! Persistence and retention for dormant hourly voice heat aggregates.

use sqlx::{PgExecutor, PgPool};

use crate::voice_heat::HourBucket;

/// Rolling days to retain heat, with a one-week buffer for a 28-day window.
pub const VOICE_HEAT_RETENTION_DAYS: i32 = 35;

// Keep interval arithmetic bounded even for callers using a non-policy horizon.
const MAX_VOICE_HEAT_RETENTION_DAYS: i32 = 36_500;

#[derive(Debug, thiserror::Error)]
pub enum VoiceHeatStoreError {
    #[error("voice_heat_invalid_horizon")]
    InvalidHorizon,
    #[error("voice_heat_retention_db")]
    Db(#[from] sqlx::Error),
}

fn check_horizon(retention_days: i32) -> Result<(), VoiceHeatStoreError> {
    if !(1..=MAX_VOICE_HEAT_RETENTION_DAYS).contains(&retention_days) {
        return Err(VoiceHeatStoreError::InvalidHorizon);
    }
    Ok(())
}

// Bind retention days at $1. Use elapsed 24-hour days even in non-UTC sessions.
macro_rules! voice_heat_cutoff_sql {
    ($clock:literal) => {
        concat!($clock, " - make_interval(hours => $1 * 24)")
    };
}

macro_rules! stale_voice_heat_sql {
    ($operation:literal, $cutoff:expr) => {
        concat!(
            $operation,
            " FROM public.voice_heat_hours WHERE hour_start_utc < ",
            $cutoff
        )
    };
}

/// Prune expired hours and insert completed buckets in one transaction.
///
/// Return the number of newly inserted hours after commit. Already stored hours
/// stay immutable, and expired backlog is discarded rather than reinserted.
/// Empty input returns zero without opening a transaction or pruning.
pub async fn flush_hour_buckets(
    pool: &PgPool,
    buckets: &[HourBucket],
) -> Result<u64, VoiceHeatStoreError> {
    if buckets.is_empty() {
        return Ok(0);
    }

    // Fixed bind shape and one insert round trip, even for a full retry backlog.
    let hours: Vec<_> = buckets.iter().map(|b| b.hour_start).collect();
    let samples: Vec<_> = buckets.iter().map(|b| b.samples).collect();
    let sums: Vec<_> = buckets.iter().map(|b| b.occupancy_sum).collect();
    let observed: Vec<_> = buckets.iter().map(|b| b.last_observed_at).collect();

    let mut tx = pool.begin().await?;
    // Conflict with a sweep's DELETE and with other flushes, but not readers.
    // The next statement samples the cutoff after the wait, not at BEGIN.
    sqlx::query("LOCK TABLE public.voice_heat_hours IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    let cutoff: time::OffsetDateTime = sqlx::query_scalar(concat!(
        "SELECT ",
        voice_heat_cutoff_sql!("statement_timestamp()")
    ))
    .bind(VOICE_HEAT_RETENTION_DAYS)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(stale_voice_heat_sql!("DELETE", "$1"))
        .bind(cutoff)
        .execute(&mut *tx)
        .await?;
    let written = sqlx::query(
        "INSERT INTO public.voice_heat_hours (hour_start_utc, samples, occupancy_sum, last_observed_at)
         SELECT hour_start_utc, samples, occupancy_sum, last_observed_at
         FROM UNNEST($2::timestamptz[], $3::bigint[], $4::bigint[], $5::timestamptz[])
             AS incoming(hour_start_utc, samples, occupancy_sum, last_observed_at)
         WHERE hour_start_utc >= $1 ON CONFLICT (hour_start_utc) DO NOTHING",
    )
    .bind(cutoff)
    .bind(hours)
    .bind(samples)
    .bind(sums)
    .bind(observed)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(written)
}

/// Count rows strictly older than the horizon, without changing them.
/// Use `&mut *tx` to share the transaction's clock with a subsequent prune.
pub async fn count_stale_voice_heat<'e>(
    executor: impl PgExecutor<'e>,
    retention_days: i32,
) -> Result<u64, VoiceHeatStoreError> {
    check_horizon(retention_days)?;
    let stale: i64 = sqlx::query_scalar(stale_voice_heat_sql!(
        "SELECT count(*)",
        voice_heat_cutoff_sql!("now()")
    ))
    .bind(retention_days)
    .fetch_one(executor)
    .await?;
    Ok(stale as u64)
}

/// Delete rows matching the same cutoff as [`count_stale_voice_heat`].
/// Use `&mut *tx` to make the deletion part of a caller-owned transaction.
pub async fn prune_voice_heat<'e>(
    executor: impl PgExecutor<'e>,
    retention_days: i32,
) -> Result<u64, VoiceHeatStoreError> {
    check_horizon(retention_days)?;
    let deleted = sqlx::query(stale_voice_heat_sql!(
        "DELETE",
        voice_heat_cutoff_sql!("now()")
    ))
    .bind(retention_days)
    .execute(executor)
    .await?;
    Ok(deleted.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_out_of_range_horizons() {
        for days in [i32::MIN, -1, 0, MAX_VOICE_HEAT_RETENTION_DAYS + 1, i32::MAX] {
            assert!(matches!(
                check_horizon(days),
                Err(VoiceHeatStoreError::InvalidHorizon)
            ));
        }
        for days in [1, VOICE_HEAT_RETENTION_DAYS, MAX_VOICE_HEAT_RETENTION_DAYS] {
            assert!(check_horizon(days).is_ok());
        }
    }
}
