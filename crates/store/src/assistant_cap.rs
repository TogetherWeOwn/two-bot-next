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
    voice_assistant_cap::{CapDecision, MonthKey, MONTHLY_BUILD_LIMIT},
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
        let resets_at = boundary(current.next_start_unix_secs(), "next month start")?;
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
        let stored = MonthKey::from_unix_secs(stored_key)
            .map_err(|error| sqlx::Error::InvalidArgument(error.to_string()))?;
        if stored > current {
            tx.rollback().await?;
            return Err(sqlx::Error::InvalidArgument(
                "clock moved before the stored month start".to_owned(),
            ));
        }
        let decision = if stored < current {
            sqlx::query(
                "UPDATE assistant_monthly_builds SET month_key = $2, build_count = 1, \
                 updated_at = clock_timestamp() WHERE guild_id = $1",
            )
            .bind(&guild)
            .bind(start)
            .execute(&mut *tx)
            .await?;
            CapDecision::Allow {
                remaining: MONTHLY_BUILD_LIMIT - 1,
            }
        } else if stored_count >= i64::from(MONTHLY_BUILD_LIMIT) {
            CapDecision::Deny {
                limit: MONTHLY_BUILD_LIMIT,
                resets_at,
            }
        } else {
            let used = u32::try_from(stored_count).map_err(|_| {
                sqlx::Error::InvalidArgument("persisted build count out of range".to_owned())
            })? + 1;
            sqlx::query(
                "UPDATE assistant_monthly_builds SET build_count = $2, \
                 updated_at = clock_timestamp() WHERE guild_id = $1",
            )
            .bind(&guild)
            .bind(i64::from(used))
            .execute(&mut *tx)
            .await?;
            CapDecision::Allow {
                remaining: MONTHLY_BUILD_LIMIT - used,
            }
        };
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
