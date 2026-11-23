//! Bounded session finalization. Callers own continuity and turn deadlines.

use sqlx::{PgPool, Postgres, Transaction};
use time::OffsetDateTime;

pub const PAGE_SIZE: usize = 32;
pub const CLOSES_PER_TURN: usize = 8;

// The outer CASE gates both expansions. SQL boolean conditions alone do not
// guarantee evaluation order. NULL here means unsupported; '{}' is valid.
macro_rules! bounded_tags {
    () => {
        "CASE
            WHEN cardinality(event_keys) = 0 THEN event_keys
            WHEN cardinality(event_keys) BETWEEN 1 AND 128
                 AND array_ndims(event_keys) = 1 AND array_lower(event_keys, 1) = 1
            THEN CASE WHEN (SELECT count(tag) = cardinality(event_keys)
                                  AND sum(octet_length(tag)) <= 16384
                           FROM unnest(event_keys) AS tags(tag))
                      THEN event_keys END
         END"
    };
}

#[derive(sqlx::FromRow)]
pub struct OpenAttendanceRow {
    pub id: i64,
    pub channel_id: Option<String>,
    pub joined_at: Option<OffsetDateTime>,
    /// Unsupported arrays return None before SQLx attempts to decode them.
    pub event_keys: Option<Vec<String>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CloseOutcome {
    AlreadyClosedOrAbsent,
    Closed,
    ClosedWithUnsupportedTags,
}

async fn begin(pool: &PgPool) -> Result<Transaction<'static, Postgres>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = '900ms'")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// One member's open rows in stable ID order. Never decode shape-invalid tags
/// or timestamps outside the calendar range supported by the attendance reader.
pub async fn open_page(
    pool: &PgPool,
    guild: &str,
    member: &str,
    after_id: i64,
) -> Result<Vec<OpenAttendanceRow>, sqlx::Error> {
    let mut tx = begin(pool).await?;
    let rows = sqlx::query_as(concat!(
        "SELECT id, CASE WHEN octet_length(channel_id) <= 20 THEN channel_id END AS channel_id,
                CASE WHEN joined_at >= TIMESTAMPTZ '0001-01-01 00:00:00+00'
                           AND joined_at < TIMESTAMPTZ '10000-01-01 00:00:00+00'
                     THEN joined_at END AS joined_at, ",
        bounded_tags!(),
        " AS event_keys
         FROM event_voice_sessions
         WHERE guild_id = $1 AND member_id = $2 AND left_at IS NULL AND id > $3
         ORDER BY id LIMIT $4"
    ))
    .bind(guild)
    .bind(member)
    .bind(after_id)
    .bind(PAGE_SIZE as i64)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows)
}

/// Finalize a known ID at its observed boundary, keeping the stored name and
/// tags even if the tags changed shape after planning. The caller must suppress
/// any new open after an unsupported result. A repeat keeps the first close.
pub async fn close_observed(
    pool: &PgPool,
    guild: &str,
    member: &str,
    id: i64,
    observed_at: OffsetDateTime,
) -> Result<CloseOutcome, sqlx::Error> {
    let mut tx = begin(pool).await?;
    let supported: Option<bool> = sqlx::query_scalar(concat!(
        "WITH locked AS MATERIALIZED (
             SELECT id, (",
        bounded_tags!(),
        ") IS NOT NULL AS supported
             FROM event_voice_sessions
             WHERE guild_id = $1 AND member_id = $2 AND id = $3 AND left_at IS NULL
             FOR UPDATE
         )
         UPDATE event_voice_sessions s
         SET left_at = GREATEST(s.joined_at, $4), changed_at = clock_timestamp()
         FROM locked WHERE s.id = locked.id
         RETURNING locked.supported"
    ))
    .bind(guild)
    .bind(member)
    .bind(id)
    .bind(observed_at)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(match supported {
        None => CloseOutcome::AlreadyClosedOrAbsent,
        Some(true) => CloseOutcome::Closed,
        Some(false) => CloseOutcome::ClosedWithUnsupportedTags,
    })
}

/// Identity-only loss page. The actor must keep its cursor until a close
/// commits and restart it if the member loses continuity again.
pub async fn member_loss_page(
    pool: &PgPool,
    guild: &str,
    member: &str,
    after_id: i64,
) -> Result<Vec<i64>, sqlx::Error> {
    let mut tx = begin(pool).await?;
    let ids = sqlx::query_scalar(
        "SELECT id FROM event_voice_sessions
         WHERE guild_id = $1 AND member_id = $2 AND left_at IS NULL AND id > $3
         ORDER BY id LIMIT $4",
    )
    .bind(guild)
    .bind(member)
    .bind(after_id)
    .bind(PAGE_SIZE as i64)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(ids)
}

/// Close one member's supplied open IDs at their own join, without decoding
/// tags, awarding continuity credit, or modifying an existing close.
pub async fn close_after_member_loss(
    pool: &PgPool,
    guild: &str,
    member: &str,
    ids: &[i64],
) -> Result<u64, sqlx::Error> {
    if ids.len() > CLOSES_PER_TURN {
        return Err(sqlx::Error::Protocol(
            "attendance close batch too large".into(),
        ));
    }
    if ids.is_empty() {
        return Ok(0);
    }
    let mut tx = begin(pool).await?;
    let affected = sqlx::query(
        "UPDATE event_voice_sessions SET left_at = joined_at, changed_at = clock_timestamp()
         WHERE guild_id = $1 AND member_id = $2 AND id = ANY($3) AND left_at IS NULL",
    )
    .bind(guild)
    .bind(member)
    .bind(ids)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(affected)
}
