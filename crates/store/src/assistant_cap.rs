//! Postgres monthly-cap ledger for the template assistant (spec
//! `docs/voice-rooms.md` §V12: "200 builds per guild per month, resetting on
//! the 1st").
//!
//! [`PgAssistantCapLedger`] persists one row per guild in
//! `assistant_monthly_builds` (migration `0446`) and decides through the pure
//! V12a core (`two_bot_core::voice_assistant_cap`, limit
//! [`MONTHLY_BUILD_LIMIT`]):
//! under the cap an attempt increments the persisted count and reports the
//! builds left after this one; at the cap it counts nothing and reports the
//! limit with the next 1st 00:00 UTC. A stored row from an older month resets
//! to zero for the current month, so the first build after the 1st is always
//! allowed. A clock that moved before the stored row's month is refused
//! rather than granting a fresh quota. Snowflakes persist as decimal TEXT,
//! matching the funnel tables.
//!
//! Concurrency: every attempt runs in one transaction that takes the guild
//! row lock, so concurrent attempts for one guild serialize and the persisted
//! count never exceeds the cap. Counts live in the database, not in memory,
//! so a new ledger instance over the same pool sees the same counts. This
//! module performs no endpoint call, holds no credential and publishes no
//! command; it only counts.

use sqlx::{Pool, Postgres};
use two_bot_core::{
    voice_assistant_cap::{CapDecision, CapLedger, MonthKey, MONTHLY_BUILD_LIMIT},
    Snowflake,
};

/// Postgres-backed per-guild monthly-cap ledger. Methods are async and run
/// directly on the pool; they must not run on a `current_thread` runtime
/// with no other thread to drive the I/O.
#[derive(Debug, Clone)]
pub struct PgAssistantCapLedger {
    pool: Pool<Postgres>,
}

impl PgAssistantCapLedger {
    /// Build over `pool`.
    #[must_use]
    pub fn new(pool: Pool<Postgres>) -> Self {
        Self { pool }
    }

    /// Borrow the pool for direct queries.
    #[must_use]
    pub fn pool(&self) -> &Pool<Postgres> {
        &self.pool
    }

    /// Count one build attempt for `guild_id` at `now_secs` (Unix seconds)
    /// against the guild's row for the UTC month containing `now_secs`.
    ///
    /// Under the cap the persisted count increments and the decision is
    /// `Allow` with the builds left after this one (zero on the month's last
    /// allowed build); at the cap nothing is counted and the decision is
    /// `Deny` with the limit and the next 1st 00:00 UTC. A stored row from
    /// an older month resets to the current month on this write. `guild_id`
    /// zero and a `now_secs` in an earlier month than the stored row
    /// (clock rollback) are errors and write nothing.
    pub async fn record_build(
        &self,
        guild_id: Snowflake,
        now_secs: i64,
    ) -> Result<CapDecision, sqlx::Error> {
        if guild_id == 0 {
            return Err(sqlx::Error::InvalidArgument(
                "guild id must be nonzero".to_owned(),
            ));
        }
        let current = month_key(now_secs)?;
        let start = boundary(current.start_unix_secs(), "month start")?;
        let guild = guild_id.to_string();
        let mut tx = self.pool.begin().await?;
        // Ensure a row exists so concurrent first-builds serialize on it: a
        // zero-count insert loses the race harmlessly (`DO NOTHING`) and
        // every attempt then proceeds through the locked row below.
        sqlx::query(
            "INSERT INTO assistant_monthly_builds (guild_id, month_key, build_count) \
             VALUES ($1, $2, 0) ON CONFLICT (guild_id) DO NOTHING",
        )
        .bind(&guild)
        .bind(start)
        .execute(&mut *tx)
        .await?;
        let row: Option<(i64, i64)> = sqlx::query_as(
            "SELECT month_key, build_count FROM assistant_monthly_builds \
             WHERE guild_id = $1 FOR UPDATE",
        )
        .bind(&guild)
        .fetch_optional(&mut *tx)
        .await?;
        let (stored_key, stored_count) = row.ok_or_else(|| {
            sqlx::Error::InvalidArgument("cap row vanished after insert".to_owned())
        })?;
        let decision = decide_build(guild_id, stored_key, stored_count, now_secs)?;
        if let CapDecision::Allow { remaining } = decision {
            sqlx::query(
                "UPDATE assistant_monthly_builds SET month_key = $2, build_count = $3, \
                 updated_at = clock_timestamp() WHERE guild_id = $1",
            )
            .bind(&guild)
            .bind(start)
            .bind(i64::from(MONTHLY_BUILD_LIMIT - remaining))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(decision)
    }

    /// Read-only current-month view: the builds used by `guild_id` in the
    /// UTC month containing `now_secs`, `0` for an unknown guild and `0`
    /// when the stored row belongs to an older month. It never writes.
    /// `guild_id` zero and a `now_secs` in an earlier month than the stored
    /// row are errors, matching [`record_build`](Self::record_build).
    pub async fn builds_used(
        &self,
        guild_id: Snowflake,
        now_secs: i64,
    ) -> Result<u32, sqlx::Error> {
        if guild_id == 0 {
            return Err(sqlx::Error::InvalidArgument(
                "guild id must be nonzero".to_owned(),
            ));
        }
        let current = month_key(now_secs)?;
        let row: Option<(i64, i64)> = sqlx::query_as(
            "SELECT month_key, build_count FROM assistant_monthly_builds WHERE guild_id = $1",
        )
        .bind(guild_id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        match row {
            None => Ok(0),
            Some((stored_key, stored_count)) => {
                let stored = MonthKey::from_unix_secs(stored_key)
                    .map_err(|error| sqlx::Error::InvalidArgument(error.to_string()))?;
                if stored > current {
                    return Err(sqlx::Error::InvalidArgument(
                        "clock moved before the stored month start".to_owned(),
                    ));
                }
                if stored < current {
                    Ok(0)
                } else {
                    u32::try_from(stored_count).map_err(|_| {
                        sqlx::Error::InvalidArgument(
                            "persisted build count out of range".to_owned(),
                        )
                    })
                }
            }
        }
    }
}

fn decide_build(
    guild_id: Snowflake,
    stored_key: i64,
    stored_count: i64,
    now_secs: i64,
) -> Result<CapDecision, sqlx::Error> {
    // Counts at or above the cap are equivalent for the decision. Keep BIGINT
    // over-cap rows denied without narrowing overflow or rewriting their count.
    let used = u32::try_from(stored_count.min(i64::from(MONTHLY_BUILD_LIMIT))).map_err(|_| {
        sqlx::Error::InvalidArgument("persisted build count out of range".to_owned())
    })?;
    CapLedger::new()
        .record(guild_id, stored_key, used, now_secs)
        .map_err(|error| sqlx::Error::InvalidArgument(error.to_string()))
}

/// The UTC year-month containing `now_secs`, as a [`MonthKey`].
fn month_key(now_secs: i64) -> Result<MonthKey, sqlx::Error> {
    MonthKey::from_unix_secs(now_secs)
        .map_err(|error| sqlx::Error::InvalidArgument(error.to_string()))
}

/// A reset-boundary instant from the core month math.
fn boundary(
    boundary: Result<i64, two_bot_core::voice_assistant_cap::CapError>,
    what: &str,
) -> Result<i64, sqlx::Error> {
    boundary.map_err(|error| sqlx::Error::InvalidArgument(format!("{what} out of range: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const JAN: i64 = 1_767_225_600;
    const FEB: i64 = 1_769_904_000;

    #[test]
    fn malformed_month_start_and_clock_rollback_are_refused() {
        for (stored_key, now, reason) in [
            (JAN + 1, JAN + 60, "month start must be the 1st"),
            (FEB, JAN, "clock moved before the stored month start"),
        ] {
            let error = decide_build(1, stored_key, 0, now).unwrap_err();
            assert!(matches!(error, sqlx::Error::InvalidArgument(_)));
            assert!(error.to_string().contains(reason));
        }
        assert!(decide_build(1, JAN, -1, JAN).is_err());
    }

    #[test]
    fn core_decides_last_build_over_cap_and_rollover() {
        assert_eq!(
            decide_build(1, JAN, 199, JAN).unwrap(),
            CapDecision::Allow { remaining: 0 }
        );
        for count in [200, i64::MAX] {
            assert_eq!(
                decide_build(1, JAN, count, JAN).unwrap(),
                CapDecision::Deny {
                    limit: MONTHLY_BUILD_LIMIT,
                    resets_at: FEB,
                }
            );
            assert_eq!(
                decide_build(1, JAN, count, FEB).unwrap(),
                CapDecision::Allow { remaining: 199 }
            );
        }
    }
}
