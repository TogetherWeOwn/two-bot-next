//! Dormant attendance storage for mapped event windows.
//!
//! Writers must tag each session in `event_voice_sessions.event_keys` with
//! `event_key/event_id/instance_start_unix_micros` (signed decimal, no padding).
//! The start is the immutable occurrence identity, not the scheduled start.
//! Bare keys cannot prove occurrence membership and are never read as evidence.

use sqlx::PgPool;
use time::OffsetDateTime;
use two_bot_core::{
    internal_action_store::InternalStoreError,
    internal_actions::{is_snowflake, valid_event_key},
    Snowflake,
};

pub mod finalization;
pub mod writer;

const MAX_SESSIONS: usize = 4096;

pub struct FinalizedAttendance {
    pub starts_at: OffsetDateTime,
    pub ends_at: OffsetDateTime,
    pub sessions: Vec<AttendanceSession>,
}

pub struct AttendanceSession {
    pub member_id: Snowflake,
    pub channel_id: Snowflake,
    pub channel_name: String,
    /// `None` means the room name's observation time is unknown.
    pub name_observed_at: Option<OffsetDateTime>,
    pub is_bot: bool,
    pub afk: bool,
    pub joined_at: OffsetDateTime,
    pub left_at: OffsetDateTime,
}

// The mapping, lifecycle, suppression and tagged sessions share one snapshot.
// Historical capture never substitutes for a missing or repointed mapping.
// Limit before decoding; oversized cells become invalid sentinels, not prefixes.
// Only send timestamps in the supported calendar range to SQLx's decoder.
// NULL in a required timestamp turns any invalid row into a whole-read error.
const FINALIZED_SQL: &str = "WITH spans AS MATERIALIZED (
    SELECT s.id, w.starts_at, w.ends_at,
           CASE WHEN octet_length(s.member_id) <= 20 THEN s.member_id ELSE '' END AS member_id,
           CASE WHEN octet_length(s.channel_id) <= 20 THEN s.channel_id ELSE '' END AS channel_id,
           CASE WHEN octet_length(s.channel_name) <= 65536 THEN s.channel_name END AS channel_name,
           s.name_observed_at, s.is_bot, s.afk, s.joined_at, clipped.left_at,
           w.starts_at <@ supported.times AND w.ends_at <@ supported.times
               AND s.joined_at <@ supported.times AND clipped.left_at <@ supported.times
               AND (s.name_observed_at IS NULL OR s.name_observed_at <@ supported.times)
               AND (s.left_at IS NULL OR (s.left_at <@ supported.times AND s.left_at >= s.joined_at))
               AND clipped.left_at >= s.joined_at
               AND cardinality(h.capture_stops) <= $4 AND COALESCE(stops.valid, TRUE) AS valid_times
    FROM internal_event_keys k
    JOIN event_attendance_windows h USING (guild_id, event_key, event_id)
    JOIN event_instance_windows w USING (guild_id, event_id, instance_starts_at)
    CROSS JOIN (SELECT tstzrange(TIMESTAMPTZ '0001-01-01 00:00:00+00',
                                 TIMESTAMPTZ '10000-01-01 00:00:00+00') AS times) supported
    JOIN event_voice_sessions s ON s.guild_id = k.guild_id AND s.event_keys @> ARRAY[
        CASE WHEN h.instance_starts_at <@ supported.times THEN
            k.event_key || '/' || k.event_id || '/' ||
                trunc(EXTRACT(EPOCH FROM h.instance_starts_at) * 1000000)::text
        END]
    CROSS JOIN LATERAL (
        SELECT MIN(stop) FILTER (WHERE stop >= s.joined_at) AS next_stop,
               BOOL_AND(COALESCE(stop <@ supported.times, FALSE)) AS valid
        FROM unnest(CASE WHEN cardinality(h.capture_stops) <= $4 THEN h.capture_stops END) AS t(stop)
    ) stops
    CROSS JOIN LATERAL (
        SELECT LEAST(s.left_at, w.ends_at + INTERVAL '15 minutes', stops.next_stop) AS left_at
    ) clipped
    WHERE k.guild_id = $1 AND k.event_key = $2
      AND w.is_current AND NOT w.identity_ambiguous AND w.revoked_at IS NULL
      AND w.status = 'completed' AND isfinite(w.starts_at) AND isfinite(w.ends_at)
      AND w.starts_at < w.ends_at AND w.ends_at <= statement_timestamp() - INTERVAL '15 minutes'
      AND NOT EXISTS (SELECT 1 FROM privacy_opt_outs p
                      WHERE p.guild_id = s.guild_id AND p.member_id = s.member_id)
    LIMIT $3
)
SELECT CASE WHEN valid_times THEN starts_at END AS starts_at,
       CASE WHEN valid_times THEN ends_at END AS ends_at,
       CASE WHEN valid_times THEN name_observed_at END AS name_observed_at,
       CASE WHEN valid_times THEN joined_at END AS joined_at,
       CASE WHEN valid_times THEN left_at END AS left_at,
       member_id, channel_id, is_bot, afk,
       CASE WHEN SUM(octet_length(channel_name)) OVER () <= 1048576 THEN channel_name END AS name
FROM spans ORDER BY joined_at, id";

#[derive(sqlx::FromRow)]
struct Span {
    starts_at: OffsetDateTime,
    ends_at: OffsetDateTime,
    member_id: String,
    channel_id: String,
    name: Option<String>,
    name_observed_at: Option<OffsetDateTime>,
    is_bot: bool,
    afk: bool,
    joined_at: OffsetDateTime,
    left_at: OffsetDateTime,
}

fn snowflake(raw: &str) -> Result<Snowflake, InternalStoreError> {
    if raw.starts_with('0') || !is_snowflake(raw) {
        return Err(InternalStoreError::Unavailable);
    }
    raw.parse().map_err(|_| InternalStoreError::Unavailable)
}

/// Unknown, unfinalized and unrecorded keys all return `None`. No partial result.
/// This is a storage primitive; callers must establish authorization and readiness.
pub async fn finalized_attendance(
    pool: &PgPool,
    guild_id: Snowflake,
    event_key: &str,
) -> Result<Option<FinalizedAttendance>, InternalStoreError> {
    let guild_id = guild_id.to_string();
    if !is_snowflake(&guild_id) || !valid_event_key(event_key) {
        return Err(InternalStoreError::InvalidInput);
    }
    let rows = sqlx::query_as::<_, Span>(FINALIZED_SQL)
        .bind(guild_id)
        .bind(event_key)
        .bind((MAX_SESSIONS + 1) as i64)
        .bind(4096_i32) // maximum capture-stop history examined per row
        .fetch_all(pool)
        .await?;
    let Some(first) = rows.first() else {
        return Ok(None);
    };
    let (starts_at, ends_at) = (first.starts_at, first.ends_at);
    if rows.len() > MAX_SESSIONS {
        return Err(InternalStoreError::Unavailable);
    }
    let sessions = rows
        .into_iter()
        .map(|row| {
            Ok(AttendanceSession {
                member_id: snowflake(&row.member_id)?,
                channel_id: snowflake(&row.channel_id)?,
                channel_name: row.name.ok_or(InternalStoreError::Unavailable)?,
                name_observed_at: row.name_observed_at,
                is_bot: row.is_bot,
                afk: row.afk,
                joined_at: row.joined_at,
                left_at: row.left_at,
            })
        })
        .collect::<Result<Vec<_>, InternalStoreError>>()?;
    Ok(Some(FinalizedAttendance {
        starts_at,
        ends_at,
        sessions,
    }))
}
