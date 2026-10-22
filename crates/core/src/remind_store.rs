//! Owner-scoped one-shots in the reserved `remind-` schedule namespace.
//! Disabled rows are replay tombstones: neither cancel nor completion makes an
//! old invocation eligible to create again. Existing member erasure covers
//! `scheduled_messages.created_by` and `updated_by`, including these rows.

use sqlx::PgPool;

use crate::{remind::PENDING_LIMIT, scheduled_store::ScheduledWrite};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Created,
    Existing,
    Capacity,
}

#[derive(sqlx::FromRow)]
pub struct Reminder {
    pub id: String,
    pub channel_id: String,
    pub body: String,
    pub next_run_at: String,
    pub delivery_uncertain: bool,
}

/// Replay detection, per-member capacity and insertion share a serialized
/// transaction. A separate lock statement gives waiters a fresh count snapshot.
pub async fn create(pool: &PgPool, row: &ScheduledWrite) -> Result<Admission, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!(
            "personal-reminders:{}:{}",
            row.guild_id, row.created_by
        ))
        .execute(&mut *tx)
        .await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM scheduled_messages WHERE id = $1)")
            .bind(&row.id)
            .fetch_one(&mut *tx)
            .await?;
    if exists {
        return Ok(Admission::Existing);
    }
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM scheduled_messages
         WHERE guild_id = $1 AND created_by = $2 AND enabled AND starts_with(id, 'remind-')",
    )
    .bind(&row.guild_id)
    .bind(&row.created_by)
    .fetch_one(&mut *tx)
    .await?;
    if count >= PENDING_LIMIT {
        return Ok(Admission::Capacity);
    }
    sqlx::query(
        "INSERT INTO scheduled_messages
         (id, guild_id, channel_id, body, next_run_at, interval_seconds, enabled,
          created_by, created_at, updated_by, updated_at)
         VALUES ($1, $2, $3, $4, $5, NULL, TRUE, $6, $7, $6, $7)",
    )
    .bind(&row.id)
    .bind(&row.guild_id)
    .bind(&row.channel_id)
    .bind(&row.body)
    .bind(&row.next_run_at)
    .bind(&row.created_by)
    .bind(&row.created_at)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Admission::Created)
}

pub async fn list(pool: &PgPool, guild: &str, owner: &str) -> Result<Vec<Reminder>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, channel_id, body, next_run_at,
                COALESCE(delivery_uncertain, TRUE) AS delivery_uncertain
         FROM scheduled_messages WHERE guild_id = $1 AND created_by = $2
         AND enabled AND starts_with(id, 'remind-') ORDER BY next_run_at, id LIMIT $3",
    )
    .bind(guild)
    .bind(owner)
    .bind(PENDING_LIMIT)
    .fetch_all(pool)
    .await
}

/// Resolve a unique id/prefix/full-text match and cancel in one statement.
/// Tombstones retain idempotency, and invalidating the claim makes an in-flight
/// sender's completion stale so the shared ticker cleans up its orphan.
pub async fn cancel(
    pool: &PgPool,
    guild: &str,
    owner: &str,
    target: &str,
    now: &str,
) -> Result<bool, sqlx::Error> {
    if target.trim().is_empty()
        || crate::message_safety::text_len(target) > crate::remind::TEXT_LIMIT
    {
        return Ok(false);
    }
    let result = sqlx::query(
        "WITH matches AS (
           SELECT id FROM scheduled_messages
           WHERE guild_id = $1 AND created_by = $2 AND enabled AND starts_with(id, 'remind-')
             AND (starts_with(id, $3) OR body = $4) LIMIT 2
         )
         UPDATE scheduled_messages SET enabled = FALSE, claim_token = NULL, claimed_at = NULL,
           body = '[cancelled reminder]', updated_at = $5
         WHERE guild_id = $1 AND created_by = $2 AND enabled
           AND id IN (SELECT id FROM matches)
           AND (SELECT count(*) FROM matches) = 1",
    )
    .bind(guild)
    .bind(owner)
    .bind(target)
    .bind(crate::remind::delivery_body(
        owner,
        &crate::scheduled::neutralize_scheduled_body(target),
    ))
    .bind(now)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}
