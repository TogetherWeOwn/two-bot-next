//! SQLx persistence for feeds. Claims are fenced and guild-scoped; uncertain
//! sends stay pending for nonce reconciliation rather than being blindly retried.
//! Relay removal and delivery admission share one bounded guild/feed-scoped
//! database fence, so a held send can never outlive a confirmed removal and
//! removal can never cascade away unresolved send evidence.

use crate::automation_quota::{lock_quota, AutomationQuota};
use crate::feeds::{
    DeliveryClaim, FeedError, FeedKind, FeedPost, FeedRelay, DELIVERY_CLAIM_LEASE_MS,
};
use sqlx::{Pool, Postgres, Row};

#[derive(Debug, thiserror::Error)]
pub enum FeedStoreError {
    #[error("feed store database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Domain(#[from] FeedError),
    #[error("Feed claim token must not be empty.")]
    EmptyClaimToken,
    #[error("{0}")]
    Capacity(AutomationQuota),
}

/// Add is insert-only: an externally supplied id cannot hijack an existing
/// relay, or change its source while retaining that source's delivery ledger.
/// The guild's 25-definition cap includes disabled feeds, and is checked in
/// the insert transaction under a guild-scoped capacity lock.
pub async fn add_feed(pool: &Pool<Postgres>, feed: &FeedRelay) -> Result<(), FeedStoreError> {
    crate::feeds::normalize_source(feed.kind, &feed.source)?;
    let mut tx = pool.begin().await?;
    lock_quota(&mut tx, &feed.guild_id, AutomationQuota::Feeds).await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM feed_relays WHERE guild_id = $1")
        .bind(&feed.guild_id)
        .fetch_one(&mut *tx)
        .await?;
    if count >= AutomationQuota::Feeds.limit() {
        return Err(FeedStoreError::Capacity(AutomationQuota::Feeds));
    }
    sqlx::query(
        "INSERT INTO feed_relays
         (id, guild_id, channel_id, kind, source, enabled, last_checked_at, created_by, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, to_timestamp($7::bigint::double precision / 1000),
                 $8, to_timestamp($9::bigint::double precision / 1000), to_timestamp($10::bigint::double precision / 1000))"
    )
    .bind(&feed.id).bind(&feed.guild_id).bind(&feed.channel_id).bind(feed.kind.as_str())
    .bind(&feed.source).bind(feed.enabled).bind(feed.last_checked_at).bind(&feed.created_by)
    .bind(feed.created_at).bind(feed.updated_at).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn list_feeds(
    pool: &Pool<Postgres>,
    guild_id: &str,
    enabled_only: bool,
) -> Result<Vec<FeedRelay>, FeedStoreError> {
    let rows = sqlx::query(
        "SELECT id, guild_id, channel_id, kind, source, enabled, created_by,
         (extract(epoch FROM last_checked_at) * 1000)::bigint AS last_checked_ms,
         (extract(epoch FROM created_at) * 1000)::bigint AS created_ms,
         (extract(epoch FROM updated_at) * 1000)::bigint AS updated_ms
         FROM feed_relays WHERE guild_id = $1 AND (NOT $2 OR enabled) ORDER BY created_at, id",
    )
    .bind(guild_id)
    .bind(enabled_only)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(FeedRelay {
                id: row.try_get("id")?,
                guild_id: row.try_get("guild_id")?,
                channel_id: row.try_get("channel_id")?,
                kind: FeedKind::parse(row.try_get("kind")?)?,
                source: row.try_get("source")?,
                enabled: row.try_get("enabled")?,
                last_checked_at: row.try_get("last_checked_ms")?,
                created_by: row.try_get("created_by")?,
                created_at: row.try_get("created_ms")?,
                updated_at: row.try_get("updated_ms")?,
            })
        })
        .collect()
}

pub async fn remove_feed(
    pool: &Pool<Postgres>,
    guild_id: &str,
    id: &str,
) -> Result<FeedRemoval, FeedStoreError> {
    match remove_feed_with_evidence(pool, guild_id, id).await? {
        FeedRemovalEvidence::Removed => Ok(FeedRemoval::Removed),
        FeedRemovalEvidence::Missing => Ok(FeedRemoval::Missing),
        FeedRemovalEvidence::RecoveryRequired { .. } => Ok(FeedRemoval::RecoveryRequired),
    }
}

/// Guild/relay-scoped removal carrying the fenced pending-evidence count.
///
/// Same fence, lock, audit and state semantics as [`remove_feed`]; the
/// refusal additionally reports how many unresolved `pending` rows the
/// fenced decision observed, so the runtime can render a snapshot without a
/// second read. The projection is one bounded `count(*)`; no ledger rows,
/// bodies, URLs, nonces or tokens leave the store.
pub async fn remove_feed_with_evidence(
    pool: &Pool<Postgres>,
    guild_id: &str,
    id: &str,
) -> Result<FeedRemovalEvidence, FeedStoreError> {
    let mut tx = pool.begin().await?;
    lock_feed_delivery(&mut tx, guild_id, id).await?;
    // Under the fence: refuse while unresolved pending delivery evidence
    // remains, so the `ON DELETE CASCADE` ledger can never delete
    // uncertainty. Only settled or never-pending relays are deleted.
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM feed_deliveries d JOIN feed_relays f ON f.id = d.feed_id
         WHERE d.feed_id = $1 AND f.guild_id = $2 AND d.state = 'pending'",
    )
    .bind(id)
    .bind(guild_id)
    .fetch_one(&mut *tx)
    .await?;
    if pending > 0 {
        tx.rollback().await?;
        let count = usize::try_from(pending).unwrap_or(usize::MAX);
        return Ok(FeedRemovalEvidence::RecoveryRequired { pending: count });
    }
    let removed = sqlx::query("DELETE FROM feed_relays WHERE guild_id = $1 AND id = $2")
        .bind(guild_id)
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 1;
    tx.commit().await?;
    Ok(if removed {
        FeedRemovalEvidence::Removed
    } else {
        FeedRemovalEvidence::Missing
    })
}

/// Outcome of an attempted relay removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedRemoval {
    /// The relay existed, carried no unresolved pending delivery evidence,
    /// and was deleted; its settled ledger cascades with it.
    Removed,
    /// No relay with that id exists in this guild; nothing was deleted.
    Missing,
    /// The relay exists but carries unresolved pending delivery evidence.
    /// Relay and ledger are retained for nonce reconciliation; the member
    /// waits for recovery instead of reposting or removing.
    RecoveryRequired,
}

/// Outcome of an attempted relay removal with pending-evidence count.
///
/// Same variants as [`FeedRemoval`]; the refusal carries the guild/relay-
/// scoped `pending` rows observed under the delivery fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedRemovalEvidence {
    /// The relay existed, carried no unresolved pending delivery evidence,
    /// and was deleted; its settled ledger cascades with it.
    Removed,
    /// No relay with that id exists in this guild; nothing was deleted.
    Missing,
    /// The relay exists but carries `pending` unresolved rows. Relay and
    /// ledger are retained for nonce reconciliation; the member waits for
    /// recovery instead of reposting or removing.
    RecoveryRequired {
        /// Unresolved `pending` rows for this guild/relay under the fence.
        pending: usize,
    },
}

/// Outcome of an attempted relay pause. Only the `enabled`/`updated_at`
/// columns ever change: URL, destination, seen-item/high-water state and the
/// delivery/nonce ledger are preserved verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedPauseOutcome {
    /// The relay was enabled and is now paused.
    Paused,
    /// The relay was already paused; no columns changed.
    AlreadyPaused,
    /// No relay with that id exists in this guild; nothing changed.
    Missing,
    /// The relay stays enabled because unresolved pending delivery evidence
    /// exists. Pausing would strand recovery behind disabled discovery
    /// (`pending_deliveries` only reads enabled relays), so the member
    /// retries after recovery settles. Already-admitted work is untouched.
    RecoveryRequired,
}

/// Outcome of an attempted relay pause with pending-evidence count.
///
/// Same variants as [`FeedPauseOutcome`]; the enabled-to-paused refusal
/// carries the guild/relay-scoped `pending` rows observed under the fence.
/// Missing and already-paused return before the pending check, so restored
/// paused+pending rows stay an idempotent no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedPauseEvidence {
    /// The relay was enabled and is now paused.
    Paused,
    /// The relay was already paused; no columns changed.
    AlreadyPaused,
    /// No relay with that id exists in this guild; nothing changed.
    Missing,
    /// The relay stays enabled because `pending` unresolved rows exist.
    /// Pausing would strand recovery behind disabled discovery, so the
    /// member retries after recovery settles. Already-admitted work is
    /// untouched.
    RecoveryRequired {
        /// Unresolved `pending` rows for this guild/relay under the fence.
        pending: usize,
    },
}

/// Outcome of an attempted relay resume. Only the `enabled`/`updated_at`
/// columns ever change.
///
/// Bounded catch-up policy: resume re-enables polling with the existing
/// per-poll budget (`MAX_FEED_POSTS_PER_POLL` attempts); it never reposts an
/// accepted nonce, never drains an unbounded backlog, and never rewrites
/// seen-item/high-water or delivery history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedResumeOutcome {
    /// The relay was paused and is now enabled.
    Resumed,
    /// The relay was already enabled; no columns changed.
    AlreadyResumed,
    /// No relay with that id exists in this guild; nothing changed.
    Missing,
}

/// Pause one relay without deleting its history.
///
/// Serialized against delivery admission under the same per-relay fence as
/// removal and fresh claims: either an in-flight claim lands first and this
/// pause sees its pending row (refusal), or this pause commits first and
/// later admission finds a disabled relay (no claim, no POST).
/// Already-admitted work is never cancelled.
pub async fn pause_feed(
    pool: &Pool<Postgres>,
    guild_id: &str,
    id: &str,
    now_ms: i64,
) -> Result<FeedPauseOutcome, FeedStoreError> {
    match pause_feed_with_evidence(pool, guild_id, id, now_ms).await? {
        FeedPauseEvidence::Paused => Ok(FeedPauseOutcome::Paused),
        FeedPauseEvidence::AlreadyPaused => Ok(FeedPauseOutcome::AlreadyPaused),
        FeedPauseEvidence::Missing => Ok(FeedPauseOutcome::Missing),
        FeedPauseEvidence::RecoveryRequired { .. } => Ok(FeedPauseOutcome::RecoveryRequired),
    }
}

/// Guild/relay-scoped pause carrying the fenced pending-evidence count.
///
/// Same fence, lock, audit and state semantics as [`pause_feed`]; only the
/// enabled-to-paused refusal additionally reports how many unresolved
/// `pending` rows the fenced decision observed. The projection is one
/// bounded `count(*)`; no ledger content leaves the store.
pub async fn pause_feed_with_evidence(
    pool: &Pool<Postgres>,
    guild_id: &str,
    id: &str,
    now_ms: i64,
) -> Result<FeedPauseEvidence, FeedStoreError> {
    let mut tx = pool.begin().await?;
    lock_feed_delivery(&mut tx, guild_id, id).await?;
    let current: Option<bool> =
        sqlx::query_scalar("SELECT enabled FROM feed_relays WHERE guild_id = $1 AND id = $2")
            .bind(guild_id)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    match current {
        None => {
            tx.rollback().await?;
            Ok(FeedPauseEvidence::Missing)
        }
        Some(false) => {
            tx.rollback().await?;
            Ok(FeedPauseEvidence::AlreadyPaused)
        }
        Some(true) => {
            // Only an enabled-to-paused transition can strand recovery.
            // Missing/already-paused relays return above without changing
            // pending evidence, including restored paused+pending state.
            let pending: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM feed_deliveries d JOIN feed_relays f ON f.id = d.feed_id
                 WHERE d.feed_id = $1 AND f.guild_id = $2 AND d.state = 'pending'",
            )
            .bind(id)
            .bind(guild_id)
            .fetch_one(&mut *tx)
            .await?;
            if pending > 0 {
                tx.rollback().await?;
                let count = usize::try_from(pending).unwrap_or(usize::MAX);
                return Ok(FeedPauseEvidence::RecoveryRequired { pending: count });
            }
            sqlx::query(
                "UPDATE feed_relays SET enabled = false,
                 updated_at = to_timestamp($3::bigint::double precision / 1000)
                 WHERE guild_id = $1 AND id = $2",
            )
            .bind(guild_id)
            .bind(id)
            .bind(now_ms)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok(FeedPauseEvidence::Paused)
        }
    }
}

/// Resume one paused relay, preserving its seen-item/high-water and
/// nonce/dedup history. Resume never refuses on pending evidence: pending
/// recovery requires the enabled relay to stay discoverable.
pub async fn resume_feed(
    pool: &Pool<Postgres>,
    guild_id: &str,
    id: &str,
    now_ms: i64,
) -> Result<FeedResumeOutcome, FeedStoreError> {
    let mut tx = pool.begin().await?;
    lock_feed_delivery(&mut tx, guild_id, id).await?;
    let current: Option<bool> =
        sqlx::query_scalar("SELECT enabled FROM feed_relays WHERE guild_id = $1 AND id = $2")
            .bind(guild_id)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    match current {
        None => {
            tx.rollback().await?;
            Ok(FeedResumeOutcome::Missing)
        }
        Some(true) => {
            tx.rollback().await?;
            Ok(FeedResumeOutcome::AlreadyResumed)
        }
        Some(false) => {
            sqlx::query(
                "UPDATE feed_relays SET enabled = true,
                 updated_at = to_timestamp($3::bigint::double precision / 1000)
                 WHERE guild_id = $1 AND id = $2",
            )
            .bind(guild_id)
            .bind(id)
            .bind(now_ms)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok(FeedResumeOutcome::Resumed)
        }
    }
}

/// Serialize relay removal and pause/resume against delivery admission for
/// one guild/feed.
///
/// Every side holds this transaction-scoped advisory lock only across short
/// database statements — never across network IO. Removal's pending check
/// plus delete run atomically under it; pause's pending check plus disable
/// does too; a fresh claim's relay-existence revalidation runs under it as
/// well. The lock orders them: either the claim lands first and removal or
/// pause sees its pending row (refusal), or removal/pause commits first and
/// admission finds no enabled relay (no claim, no POST). A held send can
/// therefore never outlive a confirmed removal, removal can never cascade
/// away a pending claim's recovery evidence, and no fresh post claim may
/// begin after a successful pause. Takeover of an existing pending row and
/// token-fenced settlement need no lock: a pending row already forces
/// refusal, and settlement only flips such a row.
///
/// `lock_timeout`/`statement_timeout` mirror the cutover retention `SET
/// LOCAL` precedent: a stuck holder fails this attempt fast as an isolated
/// retryable store error instead of pinning a pooled connection.
async fn lock_feed_delivery(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    guild_id: &str,
    feed_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut **tx)
        .await?;
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut **tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout = '15s'")
        .execute(&mut **tx)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("feed-delivery:{guild_id}:{feed_id}"))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Single-statement conflict arbitration. The arbiter is intentionally
/// omitted: `feed_deliveries` carries two unique keys (the PK on
/// `(feed_id, item_key)` plus `UNIQUE (feed_id, nonce)`), and an arbiter
/// pinned to only one of them still raises `unique_violation` on the other
/// under concurrent identical inserts — index entries land non-atomically,
/// so a racer can miss the arbiter conflict yet hit the sibling one (seen
/// on CI: 16 parallel claims colliding on the nonce key). Bare
/// `ON CONFLICT DO NOTHING` arbitrates on both; any conflict means this
/// item (or its deterministic nonce) is already claimed, so fall through to
/// the lease-takeover attempt below.
/// Only expired pending claims can be taken over; completed rows remain
/// permanent across process/pool restarts.
/// Tokens must be unique for each acquisition attempt (generated by adapter).
pub async fn claim_delivery(
    pool: &Pool<Postgres>,
    guild_id: &str,
    post: &FeedPost,
    token: &str,
    now_ms: i64,
) -> Result<Option<DeliveryClaim>, FeedStoreError> {
    if let Some(claim) = claim_fresh_delivery(pool, guild_id, post, token, now_ms).await? {
        return Ok(Some(claim));
    }
    let reclaimed = sqlx::query(
        "UPDATE feed_deliveries d SET claim_token = $3, claimed_at = to_timestamp($4::bigint::double precision / 1000)
         FROM feed_relays f WHERE d.feed_id = $1 AND d.item_key = $2 AND d.state = 'pending'
         AND (d.claimed_at IS NULL OR d.claimed_at <= to_timestamp($5::bigint::double precision / 1000))
         AND f.id = d.feed_id AND f.guild_id = $6 AND f.channel_id = $7 AND f.enabled"
    ).bind(&post.feed_id).bind(&post.item_key).bind(token).bind(now_ms)
        .bind(now_ms.saturating_sub(DELIVERY_CLAIM_LEASE_MS)).bind(guild_id).bind(&post.channel_id)
        .execute(pool).await?.rows_affected();
    Ok((reclaimed == 1).then_some(DeliveryClaim::Recovered))
}

/// XML candidates may acquire only never-claimed items. Recovery has its own
/// bounded oldest-first queue: taking over skipped rows here would rotate their
/// leases without examining history, starving them behind the same first page.
pub async fn claim_fresh_delivery(
    pool: &Pool<Postgres>,
    guild_id: &str,
    post: &FeedPost,
    token: &str,
    now_ms: i64,
) -> Result<Option<DeliveryClaim>, FeedStoreError> {
    if token.is_empty() {
        return Err(FeedStoreError::EmptyClaimToken);
    }
    // Admission shares the removal fence: the relay-existence revalidation
    // in the INSERT below runs mutually excluded with removal's pending
    // check plus delete, so a stale polled row can never claim (and POST)
    // after removal has confirmed.
    let mut tx = pool.begin().await?;
    lock_feed_delivery(&mut tx, guild_id, &post.feed_id).await?;
    let inserted = sqlx::query(
        "INSERT INTO feed_deliveries
         (feed_id, item_key, nonce, state, first_seen_at, claim_token, claimed_at)
         SELECT id, $2, $3, 'pending', to_timestamp($5::bigint::double precision / 1000),
                $4, to_timestamp($5::bigint::double precision / 1000)
         FROM feed_relays WHERE id = $1 AND guild_id = $6 AND channel_id = $7 AND enabled
         ON CONFLICT DO NOTHING",
    )
    .bind(&post.feed_id)
    .bind(&post.item_key)
    .bind(&post.nonce)
    .bind(token)
    .bind(now_ms)
    .bind(guild_id)
    .bind(&post.channel_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok((inserted == 1).then_some(DeliveryClaim::Fresh))
}

/// Recovery must not depend on an item remaining in the current feed document.
/// Oldest leases are examined first; reacquisition moves an unresolved row to
/// the back of this bounded queue instead of starving later pending claims.
pub async fn pending_deliveries(
    pool: &Pool<Postgres>,
    feed: &FeedRelay,
    now_ms: i64,
    limit: u32,
) -> Result<Vec<FeedPost>, FeedStoreError> {
    let rows = sqlx::query(
        "SELECT d.item_key, d.nonce FROM feed_deliveries d
         JOIN feed_relays f ON f.id = d.feed_id
         WHERE f.id = $1 AND f.guild_id = $2 AND f.channel_id = $3 AND f.enabled
         AND d.state = 'pending'
         AND (d.claimed_at IS NULL OR d.claimed_at <= to_timestamp($4::bigint::double precision / 1000))
         ORDER BY d.claimed_at NULLS FIRST, d.item_key LIMIT $5",
    )
    .bind(&feed.id)
    .bind(&feed.guild_id)
    .bind(&feed.channel_id)
    .bind(now_ms.saturating_sub(DELIVERY_CLAIM_LEASE_MS))
    .bind(i64::from(limit.min(200)))
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(FeedPost {
                feed_id: feed.id.clone(),
                channel_id: feed.channel_id.clone(),
                item_key: row.try_get("item_key")?,
                nonce: row.try_get("nonce")?,
                // Recovered claims only reconcile history; this is never sent.
                content: String::new(),
                suppress_mentions: true,
                enforce_nonce: true,
            })
        })
        .collect()
}

pub async fn mark_delivered(
    pool: &Pool<Postgres>,
    guild_id: &str,
    post: &FeedPost,
    token: &str,
    message_id: &str,
    now_ms: i64,
) -> Result<bool, FeedStoreError> {
    Ok(sqlx::query(
        "UPDATE feed_deliveries d SET state = 'delivered', message_id = $4,
         delivered_at = to_timestamp($5::bigint::double precision / 1000), claim_token = NULL, claimed_at = NULL
         FROM feed_relays f WHERE d.feed_id = $1 AND d.item_key = $2 AND d.claim_token = $3
         AND d.state = 'pending' AND f.id = d.feed_id AND f.guild_id = $6"
    ).bind(&post.feed_id).bind(&post.item_key).bind(token).bind(message_id).bind(now_ms)
        .bind(guild_id).execute(pool).await?.rows_affected() == 1)
}

/// Only for an acquired item known never to have been sent (e.g. poll-cap
/// deferral). NEVER call this on a timeout, ambiguous HTTP result or DB error
/// after sending: retain the row and reconcile its nonce on a later poll.
pub async fn release_unposted(
    pool: &Pool<Postgres>,
    guild_id: &str,
    post: &FeedPost,
    token: &str,
) -> Result<bool, FeedStoreError> {
    Ok(sqlx::query(
        "DELETE FROM feed_deliveries d USING feed_relays f WHERE d.feed_id = $1
         AND d.item_key = $2 AND d.claim_token = $3 AND d.state = 'pending'
         AND f.id = d.feed_id AND f.guild_id = $4",
    )
    .bind(&post.feed_id)
    .bind(&post.item_key)
    .bind(token)
    .bind(guild_id)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

pub async fn mark_checked(
    pool: &Pool<Postgres>,
    guild_id: &str,
    id: &str,
    now_ms: i64,
) -> Result<(), FeedStoreError> {
    sqlx::query("UPDATE feed_relays SET last_checked_at = to_timestamp($3::bigint::double precision / 1000) WHERE guild_id = $1 AND id = $2")
        .bind(guild_id).bind(id).bind(now_ms).execute(pool).await?;
    Ok(())
}

pub struct FeedAudit<'a> {
    pub id: &'a str,
    pub guild_id: &'a str,
    pub actor_id: Option<&'a str>,
    pub action: &'a str,
    pub target_key: &'a str,
    pub outcome: &'a str,
    pub reason: Option<&'a str>,
    pub now_ms: i64,
}

pub async fn write_audit(
    pool: &Pool<Postgres>,
    audit: &FeedAudit<'_>,
) -> Result<(), FeedStoreError> {
    sqlx::query(
        "INSERT INTO announcements_audit_log (id, guild_id, actor_id, action, target_key, outcome, reason, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, to_timestamp($8::bigint::double precision / 1000))"
    ).bind(audit.id).bind(audit.guild_id).bind(audit.actor_id).bind(audit.action)
        .bind(audit.target_key).bind(audit.outcome).bind(audit.reason).bind(audit.now_ms)
        .execute(pool).await?;
    Ok(())
}
