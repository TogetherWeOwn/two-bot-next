//! sqlx store for scheduled messages + automation audit (TOG-10081).
//!
//! Ports `src/automations/store.ts`'s scheduled-message and audit section from
//! legacy `two-bot` (frozen `main`) to sqlx/Postgres, behind the crate's `db`
//! feature so pure-domain unit tests never need a driver. Same discipline as
//! the legacy store: NOTHING HERE DECIDES ANYTHING — one SQL statement (or a
//! read-modify-write the database serialises per row) per method; validation
//! lives in [`crate::scheduled`]; audit rows carry ids and outcomes, never
//! message content.
//!
//! Table: `crates/cutover/migrations/0140_scheduled_messages.sql` (reserved
//! block 0140–0149). Keeps legacy table/column names and the legacy
//! `TEXT` ISO-8601 timestamps (so methods share [`crate::scheduled`]'s
//! formatter/parser instead of depending on a timestamp mapping).
//!
//! Concurrency contract (legacy `claimDueScheduled` + `markScheduledRun` +
//! `retryScheduled`): `claim_due` leases due rows with a single atomic
//! `UPDATE … RETURNING`, `SKIP LOCKED` letting parallel schedulers divide the
//! queue without blocking. A claimed row's `next_run_at` is parked at the
//! lease horizon so a restarted process sees it as not-due and posts exactly
//! once. `complete_run` only writes when the claim token still matches — if
//! the definition changed or was cancelled mid-post the completion loses and
//! the caller cleans up the orphan (legacy `stale_completion`).

use sqlx::{Pool, Postgres};

use crate::scheduled::advance_next_run_iso;

/// One scheduled message row (legacy `ScheduledMessageRow`).
#[derive(Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ScheduledMessageRow {
    pub id: String,
    pub guild_id: String,
    pub channel_id: String,
    pub body: String,
    pub next_run_at: String,
    pub interval_seconds: Option<i64>,
    pub enabled: bool,
    pub last_run_at: Option<String>,
    pub last_message_id: Option<String>,
    pub created_by: String,
    pub created_at: String,
    pub updated_by: String,
    pub updated_at: String,
    pub claim_token: Option<String>,
    pub claimed_at: Option<String>,
    pub occurrence_nonce: Option<String>,
}

/// A scheduled-message definition write (legacy `putScheduled` input row).
/// `interval_seconds`: `None` = one-shot; `Some(s)` = recurring. Validation
/// (body 1–2000, interval 60–31_536_000) is the caller's job —
/// [`crate::scheduled::validate_schedule`].
///
/// The caller owns the get-then-write: for a fresh id pass `created_by` /
/// `created_at` as the actor and now; for a replace pass the existing row's
/// creator fields through unchanged (like legacy's
/// `existing?.createdBy ?? input.actorId`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledWrite {
    pub id: String,
    pub guild_id: String,
    pub channel_id: String,
    pub body: String,
    pub next_run_at: String,
    pub interval_seconds: Option<i64>,
    pub enabled: bool,
    pub created_by: String,
    pub created_at: String,
    pub updated_by: String,
    pub updated_at: String,
}

/// Automation audit input (legacy `AutomationAuditInput`; `actor_id` is NULL
/// for system actors — the scheduler ticker).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledAuditInput {
    pub guild_id: String,
    pub actor_id: Option<String>,
    pub action: String,
    pub target_key: Option<String>,
    pub outcome: String,
    pub reason: Option<String>,
}

/// Store failures that are the caller's bug or a corrupt row, not a
/// connectivity blip the ticker should swallow.
#[derive(Debug, thiserror::Error)]
pub enum ScheduledStoreError {
    #[error("scheduled_messages row {id:?} has an unparsable next_run_at: {next_run_at:?}")]
    CorruptNextRunAt { id: String, next_run_at: String },
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
}

/// Fetch one definition by id within its guild.
pub async fn get_scheduled(
    pool: &Pool<Postgres>,
    guild_id: &str,
    id: &str,
) -> Result<Option<ScheduledMessageRow>, sqlx::Error> {
    sqlx::query_as::<_, ScheduledMessageRow>(
        "SELECT id, guild_id, channel_id, body, next_run_at, interval_seconds, enabled,
                last_run_at, last_message_id, created_by, created_at, updated_by, updated_at,
                claim_token, claimed_at, occurrence_nonce
           FROM scheduled_messages WHERE guild_id = $1 AND id = $2",
    )
    .bind(guild_id)
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// Literal prefix-resolve an id within one guild, matching the domain's
/// `starts_with` semantics. Ordered, first two decide — zero or ambiguous
/// both refuse downstream; SQL LIKE metacharacters have no special meaning.
pub async fn resolve_scheduled_id(
    pool: &Pool<Postgres>,
    guild_id: &str,
    id_or_prefix: &str,
) -> Result<Option<String>, sqlx::Error> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT id FROM scheduled_messages
          WHERE guild_id = $1 AND starts_with(id, $2)
          ORDER BY id LIMIT 2",
    )
    .bind(guild_id)
    .bind(id_or_prefix)
    .fetch_all(pool)
    .await?;
    if rows.len() == 1 {
        Ok(rows.into_iter().next().map(|(id,)| id))
    } else {
        Ok(None)
    }
}

/// One guild's definitions in ticker order (legacy `listScheduled`).
pub async fn list_scheduled(
    pool: &Pool<Postgres>,
    guild_id: &str,
) -> Result<Vec<ScheduledMessageRow>, sqlx::Error> {
    sqlx::query_as::<_, ScheduledMessageRow>(
        "SELECT id, guild_id, channel_id, body, next_run_at, interval_seconds, enabled,
                last_run_at, last_message_id, created_by, created_at, updated_by, updated_at,
                claim_token, claimed_at, occurrence_nonce
           FROM scheduled_messages WHERE guild_id = $1 ORDER BY next_run_at",
    )
    .bind(guild_id)
    .fetch_all(pool)
    .await
}

/// Atomically lease at most one due occurrence before its outbound post
/// (legacy `claimDueScheduled`). `SKIP LOCKED` lets a second scheduler take
/// another row instead of blocking. The ticker loops up to
/// [`crate::scheduled::TICKER_BATCH_LIMIT`], supplying a fresh claim token and
/// nonce on each call; a nonce must never be shared by distinct occurrences.
/// The lease parks `next_run_at` at the horizon so a restarted process sees
/// the row as not-due until the lease expires. A fresh claim takes the row's
/// `occurrence_nonce` only when none is set — retries of an ambiguous post
/// reuse the same nonce (legacy `COALESCE(occurrence_nonce, ?)`), so the
/// execute-then-verify path stays idempotent across restarts.
pub async fn claim_due(
    pool: &Pool<Postgres>,
    guild_id: &str,
    now_iso: &str,
    claim_token: &str,
    lease_until_iso: &str,
    occurrence_nonce: &str,
) -> Result<Vec<ScheduledMessageRow>, sqlx::Error> {
    sqlx::query_as::<_, ScheduledMessageRow>(
        "WITH due AS (
           SELECT id FROM scheduled_messages
            WHERE guild_id = $1 AND enabled AND next_run_at <= $2
            ORDER BY next_run_at, id
            LIMIT 1
            FOR UPDATE SKIP LOCKED
         )
         UPDATE scheduled_messages
            SET next_run_at = $4, claim_token = $3, claimed_at = $2,
                occurrence_nonce = COALESCE(occurrence_nonce, $5)
          WHERE guild_id = $1 AND id IN (SELECT id FROM due)
            AND next_run_at <= $2
          RETURNING id, guild_id, channel_id, body, next_run_at, interval_seconds,
                    enabled, last_run_at, last_message_id, created_by, created_at,
                    updated_by, updated_at, claim_token, claimed_at, occurrence_nonce",
    )
    .bind(guild_id)
    .bind(now_iso)
    .bind(claim_token)
    .bind(lease_until_iso)
    .bind(occurrence_nonce)
    .fetch_all(pool)
    .await
}

/// Re-queue a claimed occurrence after a retryable failure (legacy
/// `retryScheduled`): only the claim holder rewrites the row, always clearing
/// the claim. `preserve_nonce = true` keeps the occurrence nonce so the retry
/// cannot double-post; the orphan-cleanup path passes `false`.
pub async fn retry_scheduled(
    pool: &Pool<Postgres>,
    guild_id: &str,
    id: &str,
    claim_token: &str,
    next_run_at_iso: &str,
    preserve_nonce: bool,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE scheduled_messages
            SET next_run_at = $4, claim_token = NULL, claimed_at = NULL,
                occurrence_nonce = CASE WHEN $5 THEN occurrence_nonce ELSE NULL END
          WHERE guild_id = $1 AND id = $2 AND claim_token = $3",
    )
    .bind(guild_id)
    .bind(id)
    .bind(claim_token)
    .bind(next_run_at_iso)
    .bind(preserve_nonce)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Insert or replace a scheduled-message definition (legacy `putScheduled`
/// upsert). Like legacy, a replace clears any live claim and nonce — the old
/// occurrence is cancelled by definition — and preserves the original
/// creator/creation time plus the last run facts. Returns `false` when the id
/// belongs to another guild (legacy guild-scoped `WHERE` no-match).
pub async fn put_scheduled(
    pool: &Pool<Postgres>,
    row: &ScheduledWrite,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO scheduled_messages
           (id, guild_id, channel_id, body, next_run_at, interval_seconds, enabled,
            last_run_at, last_message_id, created_by, created_at, updated_by, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, NULL, NULL, $8, $9, $10, $11)
         ON CONFLICT (id) DO UPDATE SET
           channel_id       = excluded.channel_id,
           body             = excluded.body,
           next_run_at      = excluded.next_run_at,
           interval_seconds = excluded.interval_seconds,
           enabled          = excluded.enabled,
           updated_by       = excluded.updated_by,
           updated_at       = excluded.updated_at,
           claim_token      = NULL,
           claimed_at       = NULL,
           occurrence_nonce = NULL
         WHERE scheduled_messages.guild_id = excluded.guild_id",
    )
    .bind(&row.id)
    .bind(&row.guild_id)
    .bind(&row.channel_id)
    .bind(&row.body)
    .bind(&row.next_run_at)
    .bind(row.interval_seconds)
    .bind(row.enabled)
    .bind(&row.created_by)
    .bind(&row.created_at)
    .bind(&row.updated_by)
    .bind(&row.updated_at)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Record a run and compute what happens next (legacy `markScheduledRun`).
/// A one-shot disables itself; a recurring row advances `next_run_at` from
/// the run instant so no catch-up burst fires after downtime. Only the claim
/// holder's completion lands (`claim_token` match) — otherwise `None`, and
/// the caller treats the post as a stale completion (orphan cleanup + audit).
/// Errors [`ScheduledStoreError::CorruptNextRunAt`] when the claimed row's
/// `next_run_at` no longer parses, so a hand-edited row fails loudly instead
/// of silently re-queuing at the old time.
pub async fn complete_run(
    pool: &Pool<Postgres>,
    guild_id: &str,
    id: &str,
    ran_at_iso: &str,
    message_id: Option<&str>,
    claim_token: &str,
) -> Result<Option<ScheduledMessageRow>, ScheduledStoreError> {
    let current = get_scheduled(pool, guild_id, id).await?;
    let Some(current) = current else {
        return Ok(None);
    };
    if current.claim_token.as_deref() != Some(claim_token) {
        return Ok(None);
    }
    let next_run_at = match current.interval_seconds {
        Some(interval) => advance_next_run_iso(ran_at_iso, interval).ok_or_else(|| {
            ScheduledStoreError::CorruptNextRunAt {
                id: id.to_owned(),
                next_run_at: current.next_run_at.clone(),
            }
        })?,
        None => current.next_run_at.clone(),
    };
    let row = sqlx::query_as::<_, ScheduledMessageRow>(
        "UPDATE scheduled_messages SET
           last_run_at      = $3,
           last_message_id  = $4,
           enabled          = CASE WHEN interval_seconds IS NULL THEN FALSE ELSE TRUE END,
           next_run_at      = $5,
           occurrence_nonce = NULL,
           claim_token      = NULL,
           claimed_at       = NULL
         WHERE guild_id = $1 AND id = $2 AND claim_token = $6
         RETURNING id, guild_id, channel_id, body, next_run_at, interval_seconds,
                   enabled, last_run_at, last_message_id, created_by, created_at,
                   updated_by, updated_at, claim_token, claimed_at, occurrence_nonce",
    )
    .bind(guild_id)
    .bind(id)
    .bind(ran_at_iso)
    .bind(message_id)
    .bind(&next_run_at)
    .bind(claim_token)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Delete one definition within its guild (legacy `deleteScheduled`).
pub async fn delete_scheduled(
    pool: &Pool<Postgres>,
    guild_id: &str,
    id: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM scheduled_messages WHERE guild_id = $1 AND id = $2")
        .bind(guild_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Append an automation audit row (legacy `AutomationStore.audit`). Ids and
/// outcomes only — never message content.
pub async fn audit_scheduled(
    pool: &Pool<Postgres>,
    row: &ScheduledAuditInput,
    created_at: &str,
    id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO automation_audit_log
           (id, guild_id, actor_id, action, target_key, outcome, reason, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8::TEXT::TIMESTAMPTZ)",
    )
    .bind(id)
    .bind(&row.guild_id)
    .bind(&row.actor_id)
    .bind(&row.action)
    .bind(&row.target_key)
    .bind(&row.outcome)
    .bind(&row.reason)
    .bind(created_at)
    .execute(pool)
    .await?;
    Ok(())
}
