//! Event Echoes attendance log: per-event windows plus raw voice sessions.
//!
//! The SQL primitives live here; the worker hook and the read projection live
//! in the bot crate (`crate::event_attendance`). `event_instance_windows`
//! supplies accepted lifecycle/instance evidence; key mappings supply addresses.
//! `event_attendance_windows` is historical capture/finalization state only,
//! created atomically with a session/tag, never at upsert or reconciliation.
//! Each new span or tag revalidates lifecycle and privacy under the shared
//! guild ordering lock. Closes still finalize when no live window remains.
//! `event_voice_sessions` holds the raw (member, channel, joined_at, left_at)
//! spans; historical reads are capped by finite windows and revocation.
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

/// One session row to open, tagged with the windows containing the join.
/// Owned Strings so the caller's Snowflake-to-text conversion happens once
/// at the boundary; bundling the row keeps the insert surface under the
/// arity lint without a checker waiver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAttendanceSession {
    pub guild_id: String,
    pub member_id: String,
    pub channel_id: String,
    pub channel_name: String,
    pub is_bot: bool,
    pub afk: bool,
    pub joined_at: OffsetDateTime,
    pub event_keys: Vec<String>,
}

// Only accepted, finite, currently open instances can authorize capture.
// A key may alias an instance, but repointing a captured key never combines
// attendance from different occurrences. Channel bindings are not schedules;
// every channel counts, so a bound event room is not required here.
const CAPTURE_WINDOWS_SQL: &str =
    "SELECT k.event_key, w.starts_at, w.ends_at FROM internal_event_keys k
     JOIN event_instance_windows w ON w.guild_id = k.guild_id AND w.event_id = k.event_id
     WHERE k.guild_id = $1 AND w.is_current AND NOT w.identity_ambiguous
       AND w.revoked_at IS NULL AND w.status IN ('scheduled', 'active', 'completed')
       AND w.ends_at IS NOT NULL AND w.ends_at >= w.starts_at
       AND w.starts_at <= clock_timestamp() + INTERVAL '15 minutes'
       AND w.ends_at >= clock_timestamp() - INTERVAL '15 minutes'
       AND NOT EXISTS (SELECT 1 FROM event_voice_channels c WHERE c.event_id = w.event_id
                       AND c.instance_starts_at <> w.instance_starts_at)
       AND NOT EXISTS (SELECT 1 FROM event_attendance_windows h
                       WHERE h.guild_id = k.guild_id AND h.event_key = k.event_key
                         AND (h.event_id <> w.event_id OR h.instance_starts_at <> w.instance_starts_at))
     ORDER BY w.starts_at, k.event_key";

/// Currently capturable mapped windows from shared accepted-instance evidence.
/// Historical attendance records cannot authorize a new span or tag.
pub async fn attendance_windows_for_guild(
    pool: &sqlx::PgPool,
    guild_id: &str,
) -> Result<Vec<AttendanceWindow>, sqlx::Error> {
    sqlx::query_as::<_, (String, OffsetDateTime, OffsetDateTime)>(CAPTURE_WINDOWS_SQL)
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
        two_bot_core::internal_action_store::ATTENDANCE_WINDOW_FOR_KEY_SQL,
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

// Called under the same guild lock as mirror/lifecycle writers. Revalidate
// privacy at the write boundary too: a stale actor read cannot grant capture.
async fn capture_tags_in(
    connection: &mut sqlx::PgConnection,
    guild_id: &str,
    member_id: &str,
    requested: &[String],
    at: OffsetDateTime,
) -> Result<Vec<String>, sqlx::Error> {
    let permitted: bool = sqlx::query_scalar(
        "SELECT NOT EXISTS (SELECT 1 FROM privacy_opt_outs WHERE guild_id = $1 AND member_id = $2)
           AND COALESCE((SELECT voice_enabled FROM guild_analytics_settings WHERE guild_id = $1), TRUE)",
    )
    .bind(guild_id)
    .bind(member_id)
    .fetch_one(&mut *connection)
    .await?;
    if !permitted {
        return Ok(Vec::new());
    }
    let windows: Vec<(String, OffsetDateTime, OffsetDateTime)> =
        sqlx::query_as(CAPTURE_WINDOWS_SQL)
            .bind(guild_id)
            .fetch_all(&mut *connection)
            .await?;
    let grace = time::Duration::minutes(ATTENDANCE_WINDOW_GRACE_MINUTES);
    let tags: Vec<String> = windows
        .into_iter()
        .filter(|(key, start, end)| {
            requested.contains(key) && *start - grace <= at && at <= *end + grace
        })
        .map(|(key, _, _)| key)
        .collect();
    sqlx::query(
        "INSERT INTO event_attendance_windows AS h
           (guild_id, event_key, event_id, instance_starts_at, starts_at, ends_at)
         SELECT k.guild_id, k.event_key, w.event_id, w.instance_starts_at, w.starts_at, w.ends_at
         FROM internal_event_keys k JOIN event_instance_windows w
           ON w.guild_id = k.guild_id AND w.event_id = k.event_id AND w.is_current
         WHERE k.guild_id = $1 AND k.event_key = ANY($2)
         ON CONFLICT (guild_id, event_key) DO UPDATE
           SET starts_at = EXCLUDED.starts_at, ends_at = EXCLUDED.ends_at, updated_at = clock_timestamp()
         WHERE h.event_id = EXCLUDED.event_id AND h.instance_starts_at = EXCLUDED.instance_starts_at",
    )
    .bind(guild_id)
    .bind(&tags)
    .execute(&mut *connection)
    .await?;
    Ok(tags)
}

/// Open one session row after revalidating shared lifecycle and privacy evidence
/// under the writer ordering lock. Persist its historical schedule atomically.
/// No eligible tags means no row (including pre-enable/closed windows).
pub async fn insert_attendance_session(
    pool: &sqlx::PgPool,
    new: &NewAttendanceSession,
) -> Result<i64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    two_bot_core::event_instance_store::lock_guild(&mut tx, &new.guild_id).await?;
    let tags = capture_tags_in(
        &mut tx,
        &new.guild_id,
        &new.member_id,
        &new.event_keys,
        new.joined_at,
    )
    .await?;
    if tags.is_empty() {
        return Err(sqlx::Error::RowNotFound);
    }
    let id = sqlx::query_scalar(
        "INSERT INTO event_voice_sessions \
           (guild_id, member_id, channel_id, channel_name, is_bot, afk, \
            joined_at, event_keys) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         RETURNING id",
    )
    .bind(&new.guild_id)
    .bind(&new.member_id)
    .bind(&new.channel_id)
    .bind(&new.channel_name)
    .bind(new.is_bot)
    .bind(new.afk)
    .bind(new.joined_at)
    .bind(&tags)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
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
    let mut tx = pool.begin().await?;
    let owner: Option<(String, String)> = sqlx::query_as(
        "SELECT guild_id, member_id FROM event_voice_sessions WHERE id = $1 AND left_at IS NULL",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((guild_id, member_id)) = owner else {
        return Ok(0);
    };
    two_bot_core::event_instance_store::lock_guild(&mut tx, &guild_id).await?;
    let existing: Option<(Vec<String>,)> = sqlx::query_as(
        "SELECT event_keys FROM event_voice_sessions WHERE id = $1 AND left_at IS NULL FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((mut tags,)) = existing else {
        return Ok(0);
    };
    let requested: Vec<String> = event_keys
        .iter()
        .filter(|key| !tags.contains(key))
        .cloned()
        .collect();
    if !requested.is_empty() {
        for tag in capture_tags_in(&mut tx, &guild_id, &member_id, &requested, left_at).await? {
            if !tags.contains(&tag) {
                tags.push(tag);
            }
        }
    }
    tags.sort();
    let affected = sqlx::query(
        "UPDATE event_voice_sessions
         SET left_at = GREATEST(joined_at, $2), channel_name = $3, event_keys = $4
         WHERE id = $1 AND left_at IS NULL",
    )
    .bind(id)
    .bind(left_at)
    .bind(channel_name)
    .bind(&tags)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(affected)
}

/// Every session tagged with one event key in the guild, oldest join first.
pub async fn attendance_sessions_for_event_key(
    pool: &sqlx::PgPool,
    guild_id: &str,
    event_key: &str,
) -> Result<Vec<AttendanceSession>, sqlx::Error> {
    let bound: Option<(OffsetDateTime, Vec<OffsetDateTime>)> =
        sqlx::query_as(two_bot_core::internal_action_store::ATTENDANCE_SESSION_BOUND_SQL)
            .bind(guild_id)
            .bind(event_key)
            .fetch_optional(pool)
            .await?;
    let Some((bound, stops)) = bound else {
        return Ok(Vec::new());
    };
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
                    left_at: Some(two_bot_core::internal_action_store::cap_attendance_session(
                        joined_at, left_at, bound, &stops,
                    )),
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
    sqlx::query_scalar("SELECT count(*) FROM event_voice_sessions WHERE joined_at < $1")
        .bind(cutoff)
        .fetch_one(pool)
        .await
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
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM privacy_opt_outs WHERE guild_id = $1 AND member_id = $2)",
    )
    .bind(guild_id)
    .bind(member_id)
    .fetch_one(pool)
    .await
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
        new: &NewAttendanceSession,
    ) -> Result<i64, sqlx::Error> {
        insert_attendance_session(self.pool(), new).await
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
