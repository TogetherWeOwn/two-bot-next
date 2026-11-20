//! Retention primitives for dormant hourly voice heat aggregates.

use sqlx::PgExecutor;

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

// Share the strict cutoff; use elapsed 24-hour days even in non-UTC sessions.
macro_rules! stale_voice_heat_sql {
    ($operation:literal) => {
        concat!(
            $operation,
            " FROM public.voice_heat_hours ",
            "WHERE hour_start_utc < now() - make_interval(hours => $1 * 24)"
        )
    };
}

/// Count rows strictly older than the horizon, without changing them.
/// Use `&mut *tx` to share the transaction's clock with a subsequent prune.
pub async fn count_stale_voice_heat<'e>(
    executor: impl PgExecutor<'e>,
    retention_days: i32,
) -> Result<u64, VoiceHeatStoreError> {
    check_horizon(retention_days)?;
    let stale: i64 = sqlx::query_scalar(stale_voice_heat_sql!("SELECT count(*)"))
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
    let deleted = sqlx::query(stale_voice_heat_sql!("DELETE"))
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
