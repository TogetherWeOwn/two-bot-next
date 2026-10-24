//! sqlx store for scheduled messages + automation audit (TOG-10081).
//!
//! Ports `src/automations/store.ts`'s scheduled-message and audit section from
//! legacy `two-bot` (frozen `main`) to sqlx/Postgres, behind the crate's `db`
//! feature so pure-domain unit tests never need a driver. Definition writes
//! serialize guild capacity admission with their upsert; other methods use
//! one SQL statement or a database-serialized read-modify-write. Validation
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
//! lease horizon. Sending separately arms a durable uncertainty fence; a
//! reclaimed armed occurrence is never POSTed again. `complete_run` only
//! writes when the claim token still matches — if
//! the definition changed or was cancelled mid-post the completion loses and
//! the caller cleans up the orphan (legacy `stale_completion`).

use sha2::{Digest, Sha256};
use sqlx::{Pool, Postgres};

use crate::automation_quota::{lock_quota, AutomationQuota, QuotaWriteError};
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
    /// Armed before POST; expiry/restart never proves the occurrence unsent.
    pub delivery_uncertain: bool,
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
                claim_token, claimed_at, occurrence_nonce,
                COALESCE(delivery_uncertain, TRUE) AS delivery_uncertain
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
    if id_or_prefix.is_empty()
        || id_or_prefix
            .encode_utf16()
            .take(crate::commands::MAX_RESOURCE_ID_CHARS + 1)
            .count()
            > crate::commands::MAX_RESOURCE_ID_CHARS
    {
        return Ok(None);
    }
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

/// Guild-listing read cap: a long-lived guild can hold more rows than the
/// quota-admission write path allows (legacy rows, direct repairs), so the
/// list read stays bounded at exactly the per-guild schedule quota. Every
/// guild whose rows came through [`put_scheduled`] lists in full; callers
/// render `(truncated, N more)` (see [`crate::scheduled`]) when the guild
/// holds more. Pinned by acceptance: raising this breaks the cap tests.
pub const SCHEDULE_LIST_CAP: i64 = AutomationQuota::Schedules.limit();

/// One guild's definitions in list order: ascending (`next_run_at`, `id`),
/// so tied run times keep a deterministic id order for paged discovery.
/// Capped at [`SCHEDULE_LIST_CAP`] earliest rows — pair with
/// [`count_scheduled`] to size the overflow line.
pub async fn list_scheduled(
    pool: &Pool<Postgres>,
    guild_id: &str,
) -> Result<Vec<ScheduledMessageRow>, sqlx::Error> {
    sqlx::query_as::<_, ScheduledMessageRow>(
        "SELECT id, guild_id, channel_id, body, next_run_at, interval_seconds, enabled,
                last_run_at, last_message_id, created_by, created_at, updated_by, updated_at,
                claim_token, claimed_at, occurrence_nonce,
                COALESCE(delivery_uncertain, TRUE) AS delivery_uncertain
           FROM scheduled_messages WHERE guild_id = $1 ORDER BY next_run_at, id LIMIT $2",
    )
    .bind(guild_id)
    .bind(SCHEDULE_LIST_CAP)
    .fetch_all(pool)
    .await
}

/// Count one guild's definitions: the overflow-line denominator for a
/// [`SCHEDULE_LIST_CAP`]-capped [`list_scheduled`] read. Only call it when
/// the capped read came back full — under-cap guilds need no second query.
pub async fn count_scheduled(pool: &Pool<Postgres>, guild_id: &str) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM scheduled_messages WHERE guild_id = $1")
        .bind(guild_id)
        .fetch_one(pool)
        .await
}

/// One hidden window past the capped read, in the same list order:
/// ascending (`next_run_at`, `id`). The `/schedule-list` pager calls this
/// only when the capped read came back full and the count proves more rows
/// exist, and only for `page` past the first window's packed pages — page 1
/// never pays for it. `limit` stays bounded (callers pass
/// [`SCHEDULE_LIST_CAP`]); `offset` is the capped prefix to skip.
pub async fn list_scheduled_window(
    pool: &Pool<Postgres>,
    guild_id: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<ScheduledMessageRow>, sqlx::Error> {
    sqlx::query_as::<_, ScheduledMessageRow>(
        "SELECT id, guild_id, channel_id, body, next_run_at, interval_seconds, enabled,
                last_run_at, last_message_id, created_by, created_at, updated_by, updated_at,
                claim_token, claimed_at, occurrence_nonce,
                COALESCE(delivery_uncertain, TRUE) AS delivery_uncertain
           FROM scheduled_messages WHERE guild_id = $1 ORDER BY next_run_at, id LIMIT $2 OFFSET $3",
    )
    .bind(guild_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
}

/// Atomically lease at most one due occurrence before its outbound post
/// (legacy `claimDueScheduled`). `SKIP LOCKED` lets a second scheduler take
/// another row instead of blocking. The ticker loops up to
/// [`crate::scheduled::TICKER_BATCH_LIMIT`], supplying a fresh claim token and
/// nonce on each call; a nonce must never be shared by distinct occurrences.
/// The lease parks `next_run_at` at the horizon. Reclaim preserves both the
/// nonce and `delivery_uncertain`: expiry is not evidence of an unsent POST.
/// Callers must obtain [`begin_scheduled_delivery`] before sending; an armed
/// row may only be inspected/held, never blindly replayed.
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
                    updated_by, updated_at, claim_token, claimed_at, occurrence_nonce,
                    COALESCE(delivery_uncertain, TRUE) AS delivery_uncertain",
    )
    .bind(guild_id)
    .bind(now_iso)
    .bind(claim_token)
    .bind(lease_until_iso)
    .bind(occurrence_nonce)
    .fetch_all(pool)
    .await
}

/// Arm the durable uncertainty fence immediately before a single POST.
/// A stale claim, explicit replacement, or already-armed occurrence authorizes
/// no send. A lost response to this write must likewise authorize no send.
/// NULL is deliberately not FALSE: an unreadable fence fails closed.
pub async fn begin_scheduled_delivery(
    pool: &Pool<Postgres>,
    guild_id: &str,
    id: &str,
    claim_token: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE scheduled_messages SET delivery_uncertain = TRUE
          WHERE guild_id = $1 AND id = $2 AND claim_token = $3
            AND enabled AND delivery_uncertain = FALSE AND occurrence_nonce IS NOT NULL",
    )
    .bind(guild_id)
    .bind(id)
    .bind(claim_token)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Re-queue a claimed occurrence: only the claim holder rewrites the row,
/// always clearing the claim. `preserve_nonce = true` retains both identity and
/// any uncertainty fence; it NEVER authorizes another POST of a fenced row.
/// Pass `false` only after proven no effect or confirmed orphan cleanup.
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
                occurrence_nonce = CASE WHEN $5 THEN occurrence_nonce ELSE NULL END,
                delivery_uncertain = CASE WHEN $5 THEN delivery_uncertain ELSE FALSE END
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
/// belongs to another guild (legacy guild-scoped `WHERE` no-match). New
/// definitions are capped at 25 per guild, including disabled/completed rows;
/// replacing an existing definition remains allowed at or above capacity.
pub async fn put_scheduled(
    pool: &Pool<Postgres>,
    row: &ScheduledWrite,
) -> Result<bool, QuotaWriteError> {
    let mut tx = pool.begin().await?;
    lock_quota(&mut tx, &row.guild_id, AutomationQuota::Schedules).await?;
    let owner: Option<String> =
        sqlx::query_scalar("SELECT guild_id FROM scheduled_messages WHERE id = $1")
            .bind(&row.id)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(owner) = owner {
        if owner != row.guild_id {
            return Ok(false);
        }
    } else {
        // Disabled and completed definitions still consume storage capacity.
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM scheduled_messages WHERE guild_id = $1")
                .bind(&row.guild_id)
                .fetch_one(&mut *tx)
                .await?;
        if count >= AutomationQuota::Schedules.limit() {
            return Err(QuotaWriteError::Capacity(AutomationQuota::Schedules));
        }
    }
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
           occurrence_nonce = NULL,
           delivery_uncertain = FALSE
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
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result.rows_affected() > 0)
}

/// Opaque optimistic-concurrency token for one saved definition: the hex
/// SHA-256 over every field an edit must not silently overwrite — body,
/// destination, due instant, recurrence, enabled state, creator/updater
/// facts, last-run evidence and every claim/nonce/uncertainty fence.
/// Length-prefixed parts keep field boundaries unambiguous; the 64 hex
/// chars fit [`crate::scheduled::MAX_SCHEDULE_REVISION_CHARS`] and the
/// ASCII-alphabet inspect contract. Derived, never stored: a restart
/// keeps every token valid because the inputs are the row itself.
#[must_use]
pub fn schedule_revision_token(row: &ScheduledMessageRow) -> String {
    let mut hasher = Sha256::new();
    let mut part = |bytes: &[u8]| {
        hasher.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(bytes);
    };
    part(row.id.as_bytes());
    part(row.guild_id.as_bytes());
    part(row.channel_id.as_bytes());
    part(row.body.as_bytes());
    part(row.next_run_at.as_bytes());
    match row.interval_seconds {
        None => part(b"\0one-shot"),
        Some(seconds) => part(seconds.to_string().as_bytes()),
    }
    part(if row.enabled { b"\x01" } else { b"\x00" });
    match row.last_run_at.as_deref() {
        None => part(b"\0none"),
        Some(ran) => part(ran.as_bytes()),
    }
    match row.last_message_id.as_deref() {
        None => part(b"\0none"),
        Some(message) => part(message.as_bytes()),
    }
    part(row.created_by.as_bytes());
    part(row.created_at.as_bytes());
    part(row.updated_by.as_bytes());
    part(row.updated_at.as_bytes());
    match row.claim_token.as_deref() {
        None => part(b"\0none"),
        Some(token) => part(token.as_bytes()),
    }
    match row.claimed_at.as_deref() {
        None => part(b"\0none"),
        Some(claimed) => part(claimed.as_bytes()),
    }
    match row.occurrence_nonce.as_deref() {
        None => part(b"\0none"),
        Some(nonce) => part(nonce.as_bytes()),
    }
    part(if row.delivery_uncertain {
        b"\x01"
    } else {
        b"\x00"
    });
    format!("{:x}", hasher.finalize())
}

/// Delivery fence that refuses a body-only correction: an unconsumed
/// claim (live or expired — expiry never proves the occurrence unsent), a
/// pending occurrence nonce, or armed/legacy-unknown delivery
/// uncertainty. The caller renders the matching inspect/recovery
/// instruction; the row stays untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyEditFence {
    Claim,
    Nonce,
    Uncertain,
}

impl BodyEditFence {
    /// Member-visible fence detail for the edit refusal.
    #[must_use]
    pub fn detail(self) -> &'static str {
        match self {
            Self::Claim => "a delivery claim is active",
            Self::Nonce => "an occurrence nonce is still pending",
            Self::Uncertain => "delivery is uncertain",
        }
    }
}

/// Outcome of [`try_update_scheduled_body`]: exactly one of these per
/// call, decided under the row lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyEditOutcome {
    /// The body, updater and timestamp changed and the `scheduled.edit`
    /// audit committed in the same transaction; carries the saved row so
    /// the caller can issue a fresh revision.
    Saved { row: ScheduledMessageRow },
    /// The inspected revision no longer matches the locked definition or
    /// its delivery evidence: another writer (or the ticker) moved first.
    Stale,
    /// A delivery fence is armed; nothing changed.
    Fenced { fence: BodyEditFence },
    /// The replacement equals the stored body; no row, timestamp or audit
    /// churn happened.
    Unchanged,
    /// The resolved id is already gone.
    Missing,
}

/// One body-only correction attempt (the `/schedule-edit` write path).
/// Never [`put_scheduled`]: that upsert clears claim/nonce/uncertainty
/// fences, while this changes only `body`, `updated_by` and `updated_at`
/// plus the content-free `scheduled.edit` / `ok` audit, atomically.
///
/// Linearization: the definition row is locked (`SELECT … FOR UPDATE`)
/// before the revision is compared, so a concurrent ticker either claims
/// the old body first (the edit then sees the armed fence and refuses) or
/// skips the locked row (`SKIP LOCKED`) and posts exactly the committed
/// new body. Two racing edits serialize on the same lock: the first
/// committer moves the revision, the second reads stale and loses without
/// touching the winner. `expected_revision` must come from
/// [`schedule_revision_token`]; `body` must already be validated and
/// neutralized ([`crate::scheduled::validate_schedule_body`]).
pub async fn try_update_scheduled_body(
    pool: &Pool<Postgres>,
    edit: &BodyEdit<'_>,
) -> Result<BodyEditOutcome, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let locked: Option<ScheduledMessageRow> = sqlx::query_as::<_, ScheduledMessageRow>(
        "SELECT id, guild_id, channel_id, body, next_run_at, interval_seconds, enabled,
                last_run_at, last_message_id, created_by, created_at, updated_by, updated_at,
                claim_token, claimed_at, occurrence_nonce,
                COALESCE(delivery_uncertain, TRUE) AS delivery_uncertain
           FROM scheduled_messages WHERE guild_id = $1 AND id = $2 FOR UPDATE",
    )
    .bind(edit.guild_id)
    .bind(edit.id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(locked) = locked else {
        tx.rollback().await?;
        return Ok(BodyEditOutcome::Missing);
    };
    if schedule_revision_token(&locked) != edit.expected_revision {
        tx.rollback().await?;
        return Ok(BodyEditOutcome::Stale);
    }
    if let Some(fence) = edit_fence(&locked) {
        tx.rollback().await?;
        return Ok(BodyEditOutcome::Fenced { fence });
    }
    if locked.body == edit.body {
        tx.rollback().await?;
        return Ok(BodyEditOutcome::Unchanged);
    }
    let saved: ScheduledMessageRow = sqlx::query_as::<_, ScheduledMessageRow>(
        "UPDATE scheduled_messages
            SET body = $3, updated_by = $4, updated_at = $5
          WHERE guild_id = $1 AND id = $2
         RETURNING id, guild_id, channel_id, body, next_run_at, interval_seconds, enabled,
                   last_run_at, last_message_id, created_by, created_at, updated_by, updated_at,
                   claim_token, claimed_at, occurrence_nonce,
                   COALESCE(delivery_uncertain, TRUE) AS delivery_uncertain",
    )
    .bind(edit.guild_id)
    .bind(edit.id)
    .bind(edit.body)
    .bind(edit.actor)
    .bind(edit.now_iso)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO automation_audit_log
           (id, guild_id, actor_id, action, target_key, outcome, reason, created_at)
         VALUES ($1, $2, $3, 'scheduled.edit', $4, 'ok', NULL, $5::TEXT::TIMESTAMPTZ)",
    )
    .bind(edit.audit_id)
    .bind(edit.guild_id)
    .bind(edit.actor)
    .bind(edit.id)
    .bind(edit.now_iso)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(BodyEditOutcome::Saved { row: saved })
}

/// One body-only correction: validated replacement plus the inspect
/// revision it must still match. Bundled so the write path takes one
/// argument instead of growing a new waiver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyEdit<'a> {
    pub guild_id: &'a str,
    pub id: &'a str,
    pub expected_revision: &'a str,
    pub body: &'a str,
    pub actor: &'a str,
    pub now_iso: &'a str,
    pub audit_id: &'a str,
}

/// The armed delivery fence on a locked row, if any. Claim wins over
/// uncertainty, uncertainty over a bare nonce — the same priority the
/// inspect view reports, so the refusal always matches what inspect
/// showed. Legacy NULL uncertainty reads fenced (the readers
/// `COALESCE` it to TRUE): an unreadable fence fails closed.
fn edit_fence(row: &ScheduledMessageRow) -> Option<BodyEditFence> {
    if row.claim_token.is_some() {
        Some(BodyEditFence::Claim)
    } else if row.delivery_uncertain {
        Some(BodyEditFence::Uncertain)
    } else if row.occurrence_nonce.is_some() {
        Some(BodyEditFence::Nonce)
    } else {
        None
    }
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
           delivery_uncertain = FALSE,
           claim_token      = NULL,
           claimed_at       = NULL
         WHERE guild_id = $1 AND id = $2 AND claim_token = $6
         RETURNING id, guild_id, channel_id, body, next_run_at, interval_seconds,
                   enabled, last_run_at, last_message_id, created_by, created_at,
                   updated_by, updated_at, claim_token, claimed_at, occurrence_nonce,
                    COALESCE(delivery_uncertain, TRUE) AS delivery_uncertain",
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

#[cfg(test)]
mod tests {
    use super::*;

    fn idle_row() -> ScheduledMessageRow {
        ScheduledMessageRow {
            id: "aabbccdd001122334455667788990001".to_owned(),
            guild_id: "2222".to_owned(),
            channel_id: "3333".to_owned(),
            body: "Original announcement.".to_owned(),
            next_run_at: "2030-01-01T00:00:00.000Z".to_owned(),
            interval_seconds: Some(3600),
            enabled: true,
            last_run_at: Some("2026-09-30T00:00:00.000Z".to_owned()),
            last_message_id: Some("9000".to_owned()),
            created_by: "77".to_owned(),
            created_at: "2026-10-01T00:00:00.000Z".to_owned(),
            updated_by: "77".to_owned(),
            updated_at: "2026-10-01T00:00:00.000Z".to_owned(),
            claim_token: None,
            claimed_at: None,
            occurrence_nonce: None,
            delivery_uncertain: false,
        }
    }

    #[test]
    fn revision_token_is_opaque_bounded_and_binds_definition_and_fences() {
        let token = schedule_revision_token(&idle_row());
        assert_eq!(token.len(), 64);
        assert!(token.bytes().all(|b| b.is_ascii_alphanumeric()));
        assert_eq!(token, schedule_revision_token(&idle_row()));
        // Every bound field moves the token: body, destination, timing,
        // recurrence, enabled state, updater facts, last-run evidence and
        // each delivery fence.
        let mut moved = idle_row();
        moved.body = "Corrected announcement.".to_owned();
        moved.channel_id = "4444".to_owned();
        moved.next_run_at = "2030-01-01T01:00:00.000Z".to_owned();
        moved.interval_seconds = Some(7200);
        moved.enabled = false;
        moved.updated_by = "88".to_owned();
        moved.updated_at = "2026-10-24T00:00:00.000Z".to_owned();
        moved.last_run_at = Some("2026-10-02T00:00:00.000Z".to_owned());
        moved.last_message_id = Some("9009".to_owned());
        moved.claim_token = Some("live-claim".to_owned());
        moved.claimed_at = Some("2030-01-01T00:00:00.000Z".to_owned());
        moved.occurrence_nonce = Some("live-nonce".to_owned());
        moved.delivery_uncertain = true;
        assert_ne!(schedule_revision_token(&moved), token);
        // One field at a time: no bound field is silently ignored.
        let mutations: [fn(&mut ScheduledMessageRow); 10] = [
            |row: &mut ScheduledMessageRow| row.body.push('!'),
            |row: &mut ScheduledMessageRow| row.channel_id.push('4'),
            |row: &mut ScheduledMessageRow| row.next_run_at.push('Z'),
            |row: &mut ScheduledMessageRow| row.interval_seconds = None,
            |row: &mut ScheduledMessageRow| row.enabled = false,
            |row: &mut ScheduledMessageRow| row.updated_at.push('Z'),
            |row: &mut ScheduledMessageRow| row.last_message_id = None,
            |row: &mut ScheduledMessageRow| {
                row.claim_token = Some("c".to_owned());
            },
            |row: &mut ScheduledMessageRow| {
                row.occurrence_nonce = Some("n".to_owned());
            },
            |row: &mut ScheduledMessageRow| row.delivery_uncertain = true,
        ];
        for mutate in mutations {
            let mut row = idle_row();
            mutate(&mut row);
            assert_ne!(
                schedule_revision_token(&row),
                token,
                "revision must bind the mutated field"
            );
        }
    }

    #[test]
    fn fence_details_name_the_armed_delivery_state() {
        assert_eq!(BodyEditFence::Claim.detail(), "a delivery claim is active");
        assert_eq!(
            BodyEditFence::Nonce.detail(),
            "an occurrence nonce is still pending"
        );
        assert_eq!(BodyEditFence::Uncertain.detail(), "delivery is uncertain");
        assert!(edit_fence(&idle_row()).is_none());
        let mut claimed = idle_row();
        claimed.claim_token = Some("c".to_owned());
        claimed.delivery_uncertain = true;
        claimed.occurrence_nonce = Some("n".to_owned());
        assert_eq!(edit_fence(&claimed), Some(BodyEditFence::Claim));
        let mut uncertain = idle_row();
        uncertain.delivery_uncertain = true;
        uncertain.occurrence_nonce = Some("n".to_owned());
        assert_eq!(edit_fence(&uncertain), Some(BodyEditFence::Uncertain));
        let mut nonce = idle_row();
        nonce.occurrence_nonce = Some("n".to_owned());
        assert_eq!(edit_fence(&nonce), Some(BodyEditFence::Nonce));
    }

    #[tokio::test]
    async fn empty_and_overlong_prefixes_refuse_before_database_access() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
            .expect("lazy pool");

        assert_eq!(
            resolve_scheduled_id(&pool, "guild", "").await.unwrap(),
            None
        );
        assert_eq!(
            resolve_scheduled_id(
                &pool,
                "guild",
                &"x".repeat(crate::commands::MAX_RESOURCE_ID_CHARS + 1)
            )
            .await
            .unwrap(),
            None
        );
    }
}
