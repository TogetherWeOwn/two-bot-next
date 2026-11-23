//! Atomic open admission under privacy and lifecycle ordering.

use sqlx::{pool::PoolConnection, Connection, PgPool, Postgres};
use time::OffsetDateTime;
use two_bot_core::{
    event_instance_store::lock_guild,
    internal_actions::is_snowflake,
    privacy::{category_off_in, suppressed_in, AnalyticsCategory},
    Snowflake,
};

use crate::privacy::{lock_analytics_settings, lock_member_change};

pub struct NewAttendanceSession<'a> {
    pub guild_id: Snowflake,
    pub member_id: Snowflake,
    pub channel_id: Snowflake,
    pub channel_name: &'a str,
    pub is_bot: bool,
    pub afk: bool,
    pub joined_at: OffsetDateTime,
}

struct PendingBegin(PoolConnection<Postgres>, bool);

impl Drop for PendingBegin {
    fn drop(&mut self) {
        if self.1 {
            // A cancelled BEGIN may have reached PostgreSQL before SQLx
            // increments transaction depth and can queue a rollback.
            self.0.close_on_drop();
        }
    }
}

// Use the observation, at PostgreSQL precision, for the half-open grace
// check. A late or future frame cannot backfill. A replay or unclosed
// member span cannot write even the historical window. Keep mapping rows
// locked through commit because mapping writers can repoint independently
// of the lifecycle lock. A captured key never changes occurrence.
const OPEN_SQL: &str = "
WITH eligible AS MATERIALIZED (
    SELECT k.event_key, w.event_id, w.instance_starts_at, w.starts_at, w.ends_at,
           k.event_key || '/' || w.event_id || '/' ||
               trunc(EXTRACT(EPOCH FROM w.instance_starts_at) * 1000000)::text AS tag
    FROM internal_event_keys k
    JOIN event_instance_windows w USING (guild_id, event_id)
    WHERE k.guild_id = $1 AND w.is_current AND NOT w.identity_ambiguous
      AND w.revoked_at IS NULL AND w.status IN ('scheduled', 'active', 'completed')
      AND w.instance_starts_at >= TIMESTAMPTZ '0001-01-01 00:00:00+00'
      AND w.instance_starts_at < TIMESTAMPTZ '10000-01-01 00:00:00+00'
      AND w.starts_at >= TIMESTAMPTZ '0001-01-01 00:00:00+00'
      AND w.ends_at < TIMESTAMPTZ '10000-01-01 00:00:00+00'
      AND w.starts_at < w.ends_at
      AND w.starts_at <= $4::timestamptz + INTERVAL '15 minutes'
      AND w.ends_at > $4::timestamptz - INTERVAL '15 minutes'
      AND w.ends_at > statement_timestamp() - INTERVAL '15 minutes'
      AND (w.attendance_recovered_at IS NULL OR w.attendance_recovered_at <= $4)
      AND $4 BETWEEN statement_timestamp() - INTERVAL '1 minute' AND statement_timestamp()
      AND NOT EXISTS (
          SELECT 1 FROM event_voice_channels c
          WHERE c.event_id = w.event_id AND c.instance_starts_at <> w.instance_starts_at
      )
      AND NOT EXISTS (
          SELECT 1 FROM event_attendance_windows h
          WHERE h.guild_id = k.guild_id AND h.event_key = k.event_key
            AND (h.event_id <> w.event_id OR h.instance_starts_at <> w.instance_starts_at)
      )
      AND NOT EXISTS (
          SELECT 1 FROM event_voice_sessions s
          WHERE s.guild_id = $1 AND s.member_id = $2
            AND (s.left_at IS NULL OR s.joined_at >= $4)
      )
    ORDER BY k.event_key LIMIT 129 FOR SHARE OF k
), captured AS (
    INSERT INTO event_attendance_windows AS h
        (guild_id, event_key, event_id, instance_starts_at, starts_at, ends_at)
    SELECT $1, event_key, event_id, instance_starts_at, starts_at, ends_at
    FROM eligible
    WHERE (SELECT count(*) <= 128 AND sum(octet_length(tag)) <= 16384 FROM eligible)
    ON CONFLICT (guild_id, event_key) DO UPDATE SET
        starts_at = EXCLUDED.starts_at, ends_at = EXCLUDED.ends_at,
        updated_at = clock_timestamp(), changed_at = clock_timestamp()
    WHERE h.event_id = EXCLUDED.event_id AND h.instance_starts_at = EXCLUDED.instance_starts_at
    RETURNING event_key
)
INSERT INTO event_voice_sessions
    (guild_id, member_id, channel_id, channel_name, is_bot, afk, joined_at, name_observed_at, event_keys)
SELECT $1, $2, $3, $5, $6, $7, $4, $4, array_agg(tag ORDER BY event_key)
FROM eligible JOIN captured USING (event_key)
HAVING count(*) > 0";

/// Insert one observed span and its occurrence tags, or return false for a
/// refused observation. Callers must establish capture enablement and apply
/// an end-to-end deadline. This primitive has no gateway caller.
pub async fn open_mapped_session(
    pool: &PgPool,
    new: &NewAttendanceSession<'_>,
) -> Result<bool, sqlx::Error> {
    let guild = new.guild_id.to_string();
    let member = new.member_id.to_string();
    let channel = new.channel_id.to_string();
    if !is_snowflake(&guild)
        || !is_snowflake(&member)
        || !is_snowflake(&channel)
        || new.channel_name.len() > 4096
    {
        return Ok(false);
    }

    let mut connection = PendingBegin(pool.acquire().await?, true);
    // A fresh snapshot after each ordering lock observes the previous
    // holder's commit even when the database default uses snapshot isolation.
    const BEGIN: &str = "BEGIN ISOLATION LEVEL READ COMMITTED";
    let mut tx = connection.0.begin_with(BEGIN).await?;
    connection.1 = false;
    sqlx::query("SET LOCAL statement_timeout = '900ms'")
        .execute(&mut *tx)
        .await?;
    lock_analytics_settings(&mut tx, &guild).await?;
    lock_member_change(&mut tx, &guild, &member).await?;
    if category_off_in(&mut tx, &guild, AnalyticsCategory::Voice).await?
        || suppressed_in(&mut tx, &guild, &member).await?
    {
        tx.commit().await?;
        return Ok(false);
    }

    lock_guild(&mut tx, &guild).await?;
    let written = sqlx::query(OPEN_SQL)
        .bind(&guild)
        .bind(&member)
        .bind(&channel)
        .bind(new.joined_at)
        .bind(new.channel_name)
        .bind(new.is_bot)
        .bind(new.afk)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(written == 1)
}
