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
    pub name_observed_at: OffsetDateTime,
    pub is_bot: bool,
    pub afk: bool,
    pub joined_at: OffsetDateTime,
    pub left_at: Option<OffsetDateTime>,
    pub event_keys: Vec<String>,
}

type AttendanceSessionRow = (
    i64,
    String,
    String,
    String,
    bool,
    bool,
    OffsetDateTime,
    Option<OffsetDateTime>,
    Vec<String>,
    OffsetDateTime,
);

// Share row materialization outside the open/read async state machines.
#[inline(never)]
fn sessions_from_rows(rows: Vec<AttendanceSessionRow>) -> Vec<AttendanceSession> {
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
                name_observed_at,
            )| AttendanceSession {
                id,
                member_id,
                channel_id,
                channel_name,
                name_observed_at,
                is_bot,
                afk,
                joined_at,
                left_at,
                event_keys,
            },
        )
        .collect()
}

/// Bounded, shape-checked read of stored `event_keys`. Any shape SQLx cannot
/// decode as a plain one-dimensional, lower-bound-1 `text[]` (NULL array, more
/// than 128 elements, NULL element, over-budget text, multi-dimensional or
/// non-unit lower bound) reads as one over-budget sentinel tag. The canonical
/// empty array is healthy. The sentinel is never persisted: callers refuse to
/// expand it and drain the row by id with no replacement tags.
macro_rules! bounded_event_keys_sql {
    () => {
        "CASE WHEN cardinality(event_keys) IS NULL OR cardinality(event_keys) > 128
                   OR (cardinality(event_keys) > 0
                       AND (array_ndims(event_keys) <> 1 OR array_lower(event_keys, 1) <> 1))
                   OR EXISTS (SELECT 1 FROM unnest(event_keys) k WHERE k IS NULL)
                   OR (SELECT COALESCE(sum(octet_length(k)), 0) FROM unnest(event_keys) k) > 16384
              THEN ARRAY[repeat('x', 16385)] ELSE event_keys END"
    };
}

/// Protocol-error text of a close that refused to expand stored tags it cannot
/// safely decode (oversized, NULL element, or unsupported array shape).
pub const ATTENDANCE_STORED_TAGS_UNSUPPORTED: &str = "attendance stored tags exceed budget";

/// True when a [`close_attendance_session`] error is the stored-tags refusal.
/// Nothing was mutated; the caller closes by id with no tags and must not open
/// replacement rows from the pre-close read.
pub fn is_unsupported_stored_tags(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Protocol(message) if message == ATTENDANCE_STORED_TAGS_UNSUPPORTED)
}

/// Maximum open rows examined or conservatively finalized per recovery turn.
pub const ATTENDANCE_RECOVERY_PAGE_SIZE: usize = 32;
/// Maximum candidate windows and tags examined by a transition page.
pub const ATTENDANCE_MAX_WINDOWS: usize = 128;
/// Maximum aggregate tag text copied from one stored row.
pub const ATTENDANCE_MAX_TAG_BYTES: usize = 16 * 1024;

/// Minimal open-row identity for bounded presence recovery, without copying
/// room names or tag arrays into the actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttendanceRecoveryRow {
    pub id: i64,
    pub member_id: String,
    pub channel_id: String,
}

// Decode directly into the retained page, without an intermediate tuple vector.
impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for AttendanceRecoveryRow {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;

        Ok(Self {
            id: row.try_get(0)?,
            member_id: row.try_get(1)?,
            channel_id: row.try_get(2)?,
        })
    }
}

/// Read the next bounded recovery page. The cursor advances only after the
/// corresponding finalization commits; failed pages can be retried safely.
pub fn attendance_recovery_page<'a>(
    pool: &'a sqlx::PgPool,
    guild_id: &'a str,
    after_id: i64,
) -> impl std::future::Future<Output = Result<Vec<AttendanceRecoveryRow>, sqlx::Error>> + Send + 'a
{
    recovery_page(pool, guild_id, None, after_id)
}

/// Member-scoped identity-only recovery page: the same bounded SQL as
/// [`attendance_recovery_page`], limited to one member's open rows in the guild.
pub fn attendance_member_recovery_page<'a>(
    pool: &'a sqlx::PgPool,
    guild_id: &'a str,
    member_id: &'a str,
    after_id: i64,
) -> impl std::future::Future<Output = Result<Vec<AttendanceRecoveryRow>, sqlx::Error>> + Send + 'a
{
    recovery_page(pool, guild_id, Some(member_id), after_id)
}

// Both adapters return this future directly: no boxed SQL future or forwarding
// async state machines. Known loss retains its explicit indexed SQL predicate.
async fn recovery_page(
    pool: &sqlx::PgPool,
    guild_id: &str,
    member_id: Option<&str>,
    after_id: i64,
) -> Result<Vec<AttendanceRecoveryRow>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = '900ms'")
        .execute(&mut *tx)
        .await?;
    let sql = if member_id.is_some() {
        "SELECT id,
                CASE WHEN octet_length(member_id) <= 20 THEN member_id ELSE '' END,
                CASE WHEN octet_length(channel_id) <= 20 THEN channel_id ELSE '' END
         FROM event_voice_sessions
         WHERE guild_id = $1 AND left_at IS NULL AND id > $2 AND member_id = $4
         ORDER BY id LIMIT $3"
    } else {
        "SELECT id,
                CASE WHEN octet_length(member_id) <= 20 THEN member_id ELSE '' END,
                CASE WHEN octet_length(channel_id) <= 20 THEN channel_id ELSE '' END
         FROM event_voice_sessions
         WHERE guild_id = $1 AND left_at IS NULL AND id > $2 ORDER BY id LIMIT $3"
    };
    let mut query = sqlx::query_as::<_, AttendanceRecoveryRow>(sql)
        .bind(guild_id)
        .bind(after_id)
        .bind(ATTENDANCE_RECOVERY_PAGE_SIZE as i64);
    if let Some(member_id) = member_id {
        query = query.bind(member_id);
    }
    let rows = query.fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(rows)
}

/// Finalize only the supplied bounded page with no inferred offline credit.
/// An interruption supplies no trustworthy leave boundary, so these rows close
/// at their own join. Keep original tags and name-observation evidence intact;
/// a snapshot never authorizes new capture or historical tags.
pub async fn recover_attendance_sessions(
    pool: &sqlx::PgPool,
    guild_id: &str,
    ids: &[i64],
) -> Result<u64, sqlx::Error> {
    if ids.len() > ATTENDANCE_RECOVERY_PAGE_SIZE {
        return Err(sqlx::Error::Protocol(
            "attendance recovery page too large".into(),
        ));
    }
    if ids.is_empty() {
        return Ok(0);
    }
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = '900ms'")
        .execute(&mut *tx)
        .await?;
    two_bot_core::event_instance_store::lock_guild(&mut tx, guild_id).await?;
    let affected = sqlx::query(
        "UPDATE event_voice_sessions SET left_at = joined_at, changed_at = clock_timestamp()
         WHERE guild_id = $1 AND id = ANY($2) AND left_at IS NULL",
    )
    .bind(guild_id)
    .bind(ids)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(affected)
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

// Only accepted, finite instances can authorize tags. Planning/closing may
// consider expired windows overlapping an already-captured continuous span;
// opening a new span still requires a currently open window ($3 = false).
// A key may alias an instance, but repointing a captured key never combines
// attendance from different occurrences. Channel bindings are not schedules;
// every channel counts, so a bound event room is not required here.
const CAPTURE_WINDOWS_SQL: &str =
    "SELECT CASE WHEN octet_length(k.event_key) <= 128 THEN k.event_key ELSE repeat('x', 129) END, w.starts_at, w.ends_at FROM internal_event_keys k
     JOIN event_instance_windows w ON w.guild_id = k.guild_id AND w.event_id = k.event_id
     WHERE k.guild_id = $1 AND ($4::text[] IS NULL OR k.event_key = ANY($4))
       AND w.is_current AND NOT w.identity_ambiguous
       AND w.revoked_at IS NULL AND w.status IN ('scheduled', 'active', 'completed')
       AND w.ends_at IS NOT NULL AND w.ends_at >= w.starts_at
       AND w.starts_at <= clock_timestamp() + INTERVAL '15 minutes'
       AND w.ends_at >= clock_timestamp() - INTERVAL '90 days'
       AND ($3::boolean OR w.ends_at >= clock_timestamp() - INTERVAL '15 minutes')
       AND ($2::timestamptz IS NULL OR w.attendance_recovered_at IS NULL
            OR w.attendance_recovered_at <= $2)
       AND ($5::text IS NULL OR (
            w.starts_at <= $6::timestamptz + INTERVAL '15 minutes'
            AND w.ends_at >= LEAST($6::timestamptz,
                (SELECT min(s.joined_at) FROM event_voice_sessions s
                 WHERE s.guild_id = $1 AND s.member_id = $5 AND s.channel_id = $7
                   AND s.left_at IS NULL))
                - INTERVAL '15 minutes'))
       AND NOT EXISTS (SELECT 1 FROM event_voice_channels c WHERE c.event_id = w.event_id
                       AND c.instance_starts_at <> w.instance_starts_at)
       AND NOT EXISTS (SELECT 1 FROM event_attendance_windows h
                       WHERE h.guild_id = k.guild_id AND h.event_key = k.event_key
                         AND (h.event_id <> w.event_id OR h.instance_starts_at <> w.instance_starts_at))
     ORDER BY w.starts_at, k.event_key LIMIT 129";

/// Accepted candidate windows within raw retention for transition planning.
/// Expired candidates can tag an existing overlapping span, never open one.
/// Historical attendance records cannot authorize a new span or tag.
pub async fn attendance_windows_for_guild(
    pool: &sqlx::PgPool,
    guild_id: &str,
) -> Result<Vec<AttendanceWindow>, sqlx::Error> {
    planning_windows(pool, guild_id, None, None, None).await
}

/// Candidates relevant to the observed join or the member's continuous open
/// span. Filter before the overlap limit, so unrelated retained occurrences
/// cannot disable current capture. Historical candidates still grant no open.
pub async fn attendance_windows_for_transition(
    pool: &sqlx::PgPool,
    guild_id: &str,
    member_id: &str,
    previous: Option<&str>,
    observed_at: OffsetDateTime,
) -> Result<Vec<AttendanceWindow>, sqlx::Error> {
    planning_windows(pool, guild_id, Some(member_id), previous, Some(observed_at)).await
}

async fn planning_windows(
    pool: &sqlx::PgPool,
    guild_id: &str,
    member_id: Option<&str>,
    previous: Option<&str>,
    observed_at: Option<OffsetDateTime>,
) -> Result<Vec<AttendanceWindow>, sqlx::Error> {
    sqlx::query_as::<_, (String, OffsetDateTime, OffsetDateTime)>(CAPTURE_WINDOWS_SQL)
        .bind(guild_id)
        .bind(None::<OffsetDateTime>)
        .bind(true)
        .bind(None::<Vec<String>>)
        .bind(member_id)
        .bind(observed_at)
        .bind(previous)
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

/// One bounded page of the member's still-open rows, oldest first. Call again
/// only after finalizing this page; no new open may pass unfinished rows.
pub async fn open_attendance_sessions(
    pool: &sqlx::PgPool,
    guild_id: &str,
    member_id: &str,
) -> Result<Vec<AttendanceSession>, sqlx::Error> {
    sqlx::query_as::<_, AttendanceSessionRow>(concat!(
        "SELECT id,
                CASE WHEN octet_length(member_id) <= 20 THEN member_id ELSE repeat('x', 21) END,
                CASE WHEN octet_length(channel_id) <= 20 THEN channel_id ELSE repeat('x', 21) END,
                CASE WHEN octet_length(channel_name) <= 4096 THEN channel_name ELSE repeat('x', 4097) END,
                is_bot, afk, joined_at, left_at,
                ",
        bounded_event_keys_sql!(),
        ",
                COALESCE(name_observed_at, joined_at)
         FROM event_voice_sessions
         WHERE guild_id = $1 AND member_id = $2 AND left_at IS NULL
         ORDER BY joined_at, id LIMIT 32"
    ))
    .bind(guild_id)
    .bind(member_id)
    .fetch_all(pool)
    .await
    .map(sessions_from_rows)
}

// Serialize against settings writes and opt-out/opt-in even when their rows
// do not exist. Always acquire these before lifecycle or session row locks.
async fn lock_capture_in(
    connection: &mut sqlx::PgConnection,
    guild_id: &str,
    member_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *connection)
        .await?;
    crate::privacy::lock_analytics_settings(connection, guild_id).await?;
    crate::privacy::lock_member_change(connection, guild_id, member_id).await?;
    two_bot_core::event_instance_store::lock_guild(connection, guild_id).await
}

// Called under privacy, settings and lifecycle locks. A stale actor read cannot
// grant capture. For first close-time tags, reject spans crossing an omission:
// before the first capture there is no complete history of earlier credit stops.
async fn capture_tags_in(
    connection: &mut sqlx::PgConnection,
    guild_id: &str,
    member_id: &str,
    requested: &[String],
    at: OffsetDateTime,
    since: OffsetDateTime,
    closing: bool,
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
            .bind(since)
            .bind(closing)
            .bind(requested)
            .bind(None::<&str>)
            .bind(None::<OffsetDateTime>)
            .bind(None::<&str>)
            .fetch_all(&mut *connection)
            .await?;
    if windows.len() > ATTENDANCE_MAX_WINDOWS || windows.iter().any(|(key, _, _)| key.len() > 128) {
        return Err(sqlx::Error::Protocol(
            "attendance candidates exceed budget".into(),
        ));
    }
    let grace = time::Duration::minutes(ATTENDANCE_WINDOW_GRACE_MINUTES);
    let tags: Vec<String> = windows
        .into_iter()
        .filter(|(key, start, end)| {
            requested.contains(key) && *start - grace <= at && since <= *end + grace
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
           SET starts_at = EXCLUDED.starts_at, ends_at = EXCLUDED.ends_at, updated_at = clock_timestamp(), changed_at = clock_timestamp()
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
    if new.event_keys.len() > ATTENDANCE_MAX_WINDOWS
        || new.event_keys.iter().any(|key| key.len() > 128)
        || new.channel_name.len() > 4096
    {
        return Err(sqlx::Error::Protocol(
            "attendance insert exceeds budget".into(),
        ));
    }
    let mut tx = pool.begin().await?;
    lock_capture_in(&mut tx, &new.guild_id, &new.member_id).await?;
    let tags = capture_tags_in(
        &mut tx,
        &new.guild_id,
        &new.member_id,
        &new.event_keys,
        new.joined_at,
        new.joined_at,
        false,
    )
    .await?;
    if tags.is_empty() {
        return Err(sqlx::Error::RowNotFound);
    }
    let id = sqlx::query_scalar(
        "INSERT INTO event_voice_sessions \
           (guild_id, member_id, channel_id, channel_name, is_bot, afk, \
            joined_at, event_keys, name_observed_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, clock_timestamp()) \
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
    channel_name: Option<&str>,
    event_keys: &[String],
) -> Result<u64, sqlx::Error> {
    if event_keys.len() > 2 * ATTENDANCE_MAX_WINDOWS
        || event_keys.iter().map(String::len).sum::<usize>() > 2 * ATTENDANCE_MAX_TAG_BYTES
        || channel_name.is_some_and(|name| name.len() > 4096)
    {
        return Err(sqlx::Error::Protocol(
            "attendance close exceeds budget".into(),
        ));
    }
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
    lock_capture_in(&mut tx, &guild_id, &member_id).await?;
    let tags = if event_keys.is_empty() {
        // NULL keeps stored tags server-side, including oversized metadata.
        // Both close paths share the same update and commit state machine.
        None
    } else {
        let existing: Option<(Vec<String>, OffsetDateTime)> = sqlx::query_as(concat!(
            "SELECT ",
            bounded_event_keys_sql!(),
            ", joined_at FROM event_voice_sessions WHERE id = $1 AND left_at IS NULL FOR UPDATE"
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((mut tags, joined_at)) = existing else {
            return Ok(0);
        };
        if tags.iter().map(String::len).sum::<usize>() > ATTENDANCE_MAX_TAG_BYTES {
            return Err(sqlx::Error::Protocol(
                ATTENDANCE_STORED_TAGS_UNSUPPORTED.into(),
            ));
        }
        let requested: Vec<String> = event_keys
            .iter()
            .filter(|key| !tags.contains(key))
            .cloned()
            .collect();
        if !requested.is_empty() {
            for tag in capture_tags_in(
                &mut tx, &guild_id, &member_id, &requested, left_at, joined_at, true,
            )
            .await?
            {
                if !tags.contains(&tag) {
                    tags.push(tag);
                }
            }
        }
        tags.sort();
        Some(tags)
    };
    let affected = sqlx::query(
        "UPDATE event_voice_sessions
         SET left_at = GREATEST(joined_at, $2),
             channel_name = COALESCE($3, channel_name),
             name_observed_at = CASE WHEN $3::text IS NULL THEN name_observed_at ELSE clock_timestamp() END,
             event_keys = COALESCE($4::text[], event_keys), changed_at = clock_timestamp()
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
    let bound: Option<(OffsetDateTime, Option<Vec<OffsetDateTime>>)> =
        sqlx::query_as(two_bot_core::internal_action_store::ATTENDANCE_SESSION_BOUND_SQL)
            .bind(guild_id)
            .bind(event_key)
            .bind(two_bot_core::internal_action_store::MAX_ATTENDANCE_CAPTURE_STOPS as i64)
            .fetch_optional(pool)
            .await?;
    let Some((bound, stops)) = bound else {
        return Ok(Vec::new());
    };
    let stops = stops
        .ok_or_else(|| sqlx::Error::Protocol("attendance capture stops exceed budget".into()))?;
    sqlx::query_as::<_, AttendanceSessionRow>(
        "SELECT id, member_id, channel_id, channel_name, is_bot, afk, \
                joined_at, left_at, event_keys, COALESCE(name_observed_at, joined_at) \
         FROM event_voice_sessions \
         WHERE guild_id = $1 AND event_keys @> ARRAY[$2] \
         ORDER BY joined_at, id",
    )
    .bind(guild_id)
    .bind(event_key)
    .fetch_all(pool)
    .await
    .map(|rows| {
        let mut sessions = sessions_from_rows(rows);
        for session in &mut sessions {
            session.left_at = Some(two_bot_core::internal_action_store::cap_attendance_session(
                session.joined_at,
                session.left_at,
                bound,
                &stops,
            ));
        }
        sessions
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
    /// Bounded recovery over this store's pool, restricted to the supplied
    /// member for known observation loss or the guild for a full discontinuity.
    pub fn attendance_recovery_page<'a>(
        &'a self,
        guild_id: &'a str,
        member_id: Option<&'a str>,
        after_id: i64,
    ) -> impl std::future::Future<Output = Result<Vec<AttendanceRecoveryRow>, sqlx::Error>> + Send + 'a
    {
        recovery_page(self.pool(), guild_id, member_id, after_id)
    }

    /// [`recover_attendance_sessions`] over this store's pool.
    pub async fn recover_attendance_sessions(
        &self,
        guild_id: &str,
        ids: &[i64],
    ) -> Result<u64, sqlx::Error> {
        recover_attendance_sessions(self.pool(), guild_id, ids).await
    }

    /// [`attendance_windows_for_guild`] over this store's pool.
    pub async fn attendance_windows_for_guild(
        &self,
        guild_id: &str,
    ) -> Result<Vec<AttendanceWindow>, sqlx::Error> {
        attendance_windows_for_guild(self.pool(), guild_id).await
    }

    /// [`attendance_windows_for_transition`] over this store's pool.
    pub async fn attendance_windows_for_transition(
        &self,
        guild_id: &str,
        member_id: &str,
        previous: Option<&str>,
        observed_at: OffsetDateTime,
    ) -> Result<Vec<AttendanceWindow>, sqlx::Error> {
        attendance_windows_for_transition(self.pool(), guild_id, member_id, previous, observed_at)
            .await
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
        channel_name: Option<&str>,
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
