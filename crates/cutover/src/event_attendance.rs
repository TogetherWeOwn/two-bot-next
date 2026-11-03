//! Event Echoes attendance log: per-event windows plus raw voice sessions.
//!
//! The SQL primitives live here; the worker hook and the read projection live
//! in the bot crate (`crate::event_attendance`). `event_attendance_windows`
//! holds one row per mapped (guild, event_key) with the upsert-confirmed
//! start/end instants, and `event_voice_sessions` holds one row per
//! (member, channel, joined_at, left_at). Rows open only while a window is
//! open for the guild; closes finalize open rows whenever the leave/move
//! arrives. Each row carries the union of windows overlapping
//! `[joined_at, left_at]` in `event_keys`, computed by the caller from the
//! stored windows, so the read filters by tag and never by live overlap.
//!
//! Raw sessions roll off 90 days after `joined_at` through the retention job
//! ([`delete_attendance_sessions_before`]); per-member erasure and guild
//! purges run through the member-erasure plan. Snowflakes are TEXT, matching
//! the cutover convention.

use time::OffsetDateTime;

use crate::voice_rooms::PgRoomStore;

/// Recording window grace on each side of `[starts_at, ends_at]`, minutes.
pub const ATTENDANCE_WINDOW_GRACE_MINUTES: i64 = 15;
/// Raw sessions older than this roll off through the retention job.
pub const ATTENDANCE_SESSION_RETENTION_DAYS: i32 = 90;

/// One mapped event window for a guild.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttendanceWindow {
    pub event_key: String,
    pub starts_at: OffsetDateTime,
    pub ends_at: OffsetDateTime,
}

/// One raw voice-session row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttendanceSession {
    pub id: i64,
    pub member_id: String,
    pub channel_id: String,
    pub channel_name: String,
    pub is_bot: bool,
    pub afk: bool,
    pub joined_at: OffsetDateTime,
    pub left_at: Option<OffsetDateTime>,
    pub event_keys: Vec<String>,
}

/// Register (or re-point) one event window in one guild. `event.upsert` calls
/// this only after Discord confirms the create/update the window names, next
/// to the key-mapping registration.
pub async fn put_attendance_window(
    pool: &sqlx::PgPool,
    guild_id: &str,
    event_key: &str,
    starts_at: OffsetDateTime,
    ends_at: OffsetDateTime,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO event_attendance_windows (guild_id, event_key, starts_at, ends_at) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (guild_id, event_key) DO UPDATE \
         SET starts_at = EXCLUDED.starts_at, ends_at = EXCLUDED.ends_at, \
             updated_at = clock_timestamp()",
    )
    .bind(guild_id)
    .bind(event_key)
    .bind(starts_at)
    .bind(ends_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Every mapped window for one guild. The table is small (one row per mapped
/// event); callers compute containment/overlap in memory.
pub async fn attendance_windows_for_guild(
    pool: &sqlx::PgPool,
    guild_id: &str,
) -> Result<Vec<AttendanceWindow>, sqlx::Error> {
    sqlx::query_as::<_, (String, OffsetDateTime, OffsetDateTime)>(
        "SELECT event_key, starts_at, ends_at FROM event_attendance_windows \
         WHERE guild_id = $1 ORDER BY starts_at, event_key",
    )
    .bind(guild_id)
    .fetch_all(pool)
    .await
    .map(|rows| {
        rows.into_iter()
            .map(|(event_key, starts_at, ends_at)| AttendanceWindow {
                event_key,
                starts_at,
                ends_at,
            })
            .collect()
    })
}

/// One mapped window by key. `Ok(None)` is an unmapped key.
pub async fn attendance_window_for_key(
    pool: &sqlx::PgPool,
    guild_id: &str,
    event_key: &str,
) -> Result<Option<AttendanceWindow>, sqlx::Error> {
    sqlx::query_as::<_, (OffsetDateTime, OffsetDateTime)>(
        "SELECT starts_at, ends_at FROM event_attendance_windows \
         WHERE guild_id = $1 AND event_key = $2",
    )
    .bind(guild_id)
    .bind(event_key)
    .fetch_optional(pool)
    .await
    .map(|row| {
        row.map(|(starts_at, ends_at)| AttendanceWindow {
            event_key: event_key.to_owned(),
            starts_at,
            ends_at,
        })
    })
}

/// The member's still-open session rows in the guild, oldest first.
pub async fn open_attendance_sessions(
    pool: &sqlx::PgPool,
    guild_id: &str,
    member_id: &str,
) -> Result<Vec<AttendanceSession>, sqlx::Error> {
    sqlx::query_as::<
        _,
        (
            i64,
            String,
            String,
            String,
            bool,
            bool,
            OffsetDateTime,
            Option<OffsetDateTime>,
            Vec<String>,
        ),
    >(
        "SELECT id, member_id, channel_id, channel_name, is_bot, afk, \
                joined_at, left_at, event_keys \
         FROM event_voice_sessions \
         WHERE guild_id = $1 AND member_id = $2 AND left_at IS NULL \
         ORDER BY joined_at, id",
    )
    .bind(guild_id)
    .bind(member_id)
    .fetch_all(pool)
    .await
    .map(|rows| {
        rows.into_iter()
            .map(
                |(
                    id,
                    member_id,
                    channel_id,
                    channel_name,
                    is_bot,
                    afk,
                    joined_at,
                    left_at,
                    event_keys,
                )| AttendanceSession {
                    id,
                    member_id,
                    channel_id,
                    channel_name,
                    is_bot,
                    afk,
                    joined_at,
                    left_at,
                    event_keys,
                },
            )
            .collect()
    })
}

/// Open one session row tagged with the windows containing the join. Returns
/// the new row id.
pub async fn insert_attendance_session(
    pool: &sqlx::PgPool,
    guild_id: &str,
    member_id: &str,
    channel_id: &str,
    channel_name: &str,
    is_bot: bool,
    afk: bool,
    joined_at: OffsetDateTime,
    event_keys: &[String],
) -> Result<i64, sqlx::Error> {
    Ok(sqlx::query_scalar(
        "INSERT INTO event_voice_sessions \
           (guild_id, member_id, channel_id, channel_name, is_bot, afk, \
            joined_at, event_keys) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         RETURNING id",
    )
    .bind(guild_id)
    .bind(member_id)
    .bind(channel_id)
    .bind(channel_name)
    .bind(is_bot)
    .bind(afk)
    .bind(joined_at)
    .bind(event_keys)
    .fetch_one(pool)
    .await?)
}

/// Finalize one open row: stamp `left_at`, refresh the channel name to the
/// latest the logger saw, and widen the tag union. Only touches open rows;
/// returns rows closed (0 when the row already closed under a racing write).
pub async fn close_attendance_session(
    pool: &sqlx::PgPool,
    id: i64,
    left_at: OffsetDateTime,
    channel_name: &str,
    event_keys: &[String],
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE event_voice_sessions \
         SET left_at = $2, channel_name = $3, event_keys = $4 \
         WHERE id = $1 AND left_at IS NULL",
    )
    .bind(id)
    .bind(left_at)
    .bind(channel_name)
    .bind(event_keys)
    .execute(pool)
    .await?
    .rows_affected())
}

/// Every session tagged with one event key in the guild, oldest join first.
pub async fn attendance_sessions_for_event_key(
    pool: &sqlx::PgPool,
    guild_id: &str,
    event_key: &str,
) -> Result<Vec<AttendanceSession>, sqlx::Error> {
    sqlx::query_as::<
        _,
        (
            i64,
            String,
            String,
            String,
            bool,
            bool,
            OffsetDateTime,
            Option<OffsetDateTime>,
            Vec<String>,
        ),
    >(
        "SELECT id, member_id, channel_id, channel_name, is_bot, afk, \
                joined_at, left_at, event_keys \
         FROM event_voice_sessions \
         WHERE guild_id = $1 AND event_keys @> ARRAY[$2] \
         ORDER BY joined_at, id",
    )
    .bind(guild_id)
    .bind(event_key)
    .fetch_all(pool)
    .await
    .map(|rows| {
        rows.into_iter()
            .map(
                |(
                    id,
                    member_id,
                    channel_id,
                    channel_name,
                    is_bot,
                    afk,
                    joined_at,
                    left_at,
                    event_keys,
                )| AttendanceSession {
                    id,
                    member_id,
                    channel_id,
                    channel_name,
                    is_bot,
                    afk,
                    joined_at,
                    left_at,
                    event_keys,
                },
            )
            .collect()
    })
}

/// Delete raw sessions joined before `cutoff`. Returns rows deleted.
pub async fn delete_attendance_sessions_before(
    pool: &sqlx::PgPool,
    cutoff: OffsetDateTime,
) -> Result<u64, sqlx::Error> {
    Ok(
        sqlx::query("DELETE FROM event_voice_sessions WHERE joined_at < $1")
            .bind(cutoff)
            .execute(pool)
            .await?
            .rows_affected(),
    )
}

/// Count raw sessions joined before `cutoff` (retention dry run).
pub async fn count_attendance_sessions_before(
    pool: &sqlx::PgPool,
    cutoff: OffsetDateTime,
) -> Result<i64, sqlx::Error> {
    Ok(
        sqlx::query_scalar("SELECT count(*) FROM event_voice_sessions WHERE joined_at < $1")
            .bind(cutoff)
            .fetch_one(pool)
            .await?,
    )
}

/// Whether the pair holds a guild-scoped opt-out suppression row. The
/// attendance logger consults this before opening a span (contract v2 §9):
/// a suppressed member opens nothing, so they stay absent from the record
/// instead of anonymised in it. Read-only; closes still finalize.
pub async fn is_attendance_suppressed(
    pool: &sqlx::PgPool,
    guild_id: &str,
    member_id: &str,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM privacy_opt_outs WHERE guild_id = $1 AND member_id = $2)",
    )
    .bind(guild_id)
    .bind(member_id)
    .fetch_one(pool)
    .await?)
}

/// Whether guild-wide voice capture is enabled. `guild_analytics_settings`
/// rows are opt-out toggles: a missing row means enabled, matching the
/// pre-toggle behavior. `voice_enabled = off` stops attendance capture
/// guild-wide (contract v2 §9). Read-only; closes still finalize.
pub async fn is_attendance_voice_enabled(
    pool: &sqlx::PgPool,
    guild_id: &str,
) -> Result<bool, sqlx::Error> {
    let enabled: Option<bool> = sqlx::query_scalar(
        "SELECT voice_enabled FROM guild_analytics_settings WHERE guild_id = $1",
    )
    .bind(guild_id)
    .fetch_optional(pool)
    .await?;
    Ok(enabled.unwrap_or(true))
}

impl PgRoomStore {
    /// [`put_attendance_window`] over this store's pool.
    pub async fn put_attendance_window(
        &self,
        guild_id: &str,
        event_key: &str,
        starts_at: OffsetDateTime,
        ends_at: OffsetDateTime,
    ) -> Result<(), sqlx::Error> {
        put_attendance_window(self.pool(), guild_id, event_key, starts_at, ends_at).await
    }

    /// [`attendance_windows_for_guild`] over this store's pool.
    pub async fn attendance_windows_for_guild(
        &self,
        guild_id: &str,
    ) -> Result<Vec<AttendanceWindow>, sqlx::Error> {
        attendance_windows_for_guild(self.pool(), guild_id).await
    }

    /// [`open_attendance_sessions`] over this store's pool.
    pub async fn open_attendance_sessions(
        &self,
        guild_id: &str,
        member_id: &str,
    ) -> Result<Vec<AttendanceSession>, sqlx::Error> {
        open_attendance_sessions(self.pool(), guild_id, member_id).await
    }

    /// [`insert_attendance_session`] over this store's pool.
    pub async fn insert_attendance_session(
        &self,
        guild_id: &str,
        member_id: &str,
        channel_id: &str,
        channel_name: &str,
        is_bot: bool,
        afk: bool,
        joined_at: OffsetDateTime,
        event_keys: &[String],
    ) -> Result<i64, sqlx::Error> {
        insert_attendance_session(
            self.pool(),
            guild_id,
            member_id,
            channel_id,
            channel_name,
            is_bot,
            afk,
            joined_at,
            event_keys,
        )
        .await
    }

    /// [`close_attendance_session`] over this store's pool.
    pub async fn close_attendance_session(
        &self,
        id: i64,
        left_at: OffsetDateTime,
        channel_name: &str,
        event_keys: &[String],
    ) -> Result<u64, sqlx::Error> {
        close_attendance_session(self.pool(), id, left_at, channel_name, event_keys).await
    }

    /// [`attendance_sessions_for_event_key`] over this store's pool.
    pub async fn attendance_sessions_for_event_key(
        &self,
        guild_id: &str,
        event_key: &str,
    ) -> Result<Vec<AttendanceSession>, sqlx::Error> {
        attendance_sessions_for_event_key(self.pool(), guild_id, event_key).await
    }

    /// [`delete_attendance_sessions_before`] over this store's pool.
    pub async fn delete_attendance_sessions_before(
        &self,
        cutoff: OffsetDateTime,
    ) -> Result<u64, sqlx::Error> {
        delete_attendance_sessions_before(self.pool(), cutoff).await
    }

    /// [`count_attendance_sessions_before`] over this store's pool.
    pub async fn count_attendance_sessions_before(
        &self,
        cutoff: OffsetDateTime,
    ) -> Result<i64, sqlx::Error> {
        count_attendance_sessions_before(self.pool(), cutoff).await
    }

    /// [`is_attendance_suppressed`] over this store's pool.
    pub async fn is_attendance_suppressed(
        &self,
        guild_id: &str,
        member_id: &str,
    ) -> Result<bool, sqlx::Error> {
        is_attendance_suppressed(self.pool(), guild_id, member_id).await
    }

    /// [`is_attendance_voice_enabled`] over this store's pool.
    pub async fn is_attendance_voice_enabled(&self, guild_id: &str) -> Result<bool, sqlx::Error> {
        is_attendance_voice_enabled(self.pool(), guild_id).await
    }
}
