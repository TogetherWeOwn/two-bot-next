//! SQLx persistence for the stream go-live dedup ledger (`stream_live_claims`).
//!
//! Claim-before-post arbitration for Twitch `stream.online` announcements:
//! [`claim_live`] inserts the go-live row with `ON CONFLICT DO NOTHING`, so
//! concurrent EventSub redeliveries of the same `broadcaster + started_at`
//! instant collapse to exactly one owner. The receiver commits
//! [`mark_live_send_started`] BEFORE the first Discord POST (mirroring
//! `automod_delivery_claims.mutation_started`: no lease automatically retries
//! a mutation whose Discord outcome is uncertain). A send that provably never
//! reached Discord deletes its unposted row ([`release_live`]) so the Twitch
//! retry can claim again; a delivered row is stamped
//! ([`mark_live_delivered`]) and never deleted, so retries after a successful
//! stamp cannot double-post.
//!
//! Crash recovery is a bounded lease over never-started claims only:
//! [`claim_live`] itself classifies a conflicting row (and
//! [`reclaim_stale_live`] exposes the same takeover directly): it atomically
//! takes over one stale unposted row whose
//! `claimed_at` is older than [`STALE_CLAIM_LEASE_SECS`] and whose
//! `send_started_at IS NULL`. Started (`send_started_at NOT NULL`) and
//! delivered (`delivered_at NOT NULL`) rows are never reclaimed, even when
//! stale: a Discord send can succeed while [`mark_live_delivered`] fails,
//! leaving an ambiguous unstamped row, and bulk-deleting every row with
//! `delivered_at IS NULL` would double-post it.
//!
//! Ownership is fenced by a claim token (migration 0443, mirroring
//! `automod_delivery_claims.claim_token`): a fresh insert generates one and
//! a takeover rotates it, and [`mark_live_send_started`],
//! [`mark_live_delivered`] and [`release_live`] only act on the row whose
//! token they present. A timed-out-but-running original owner therefore
//! cannot start, stamp or release the row a takeover gave to a new owner:
//! its token no longer matches, so exactly one owner can post.
//! [`release_live`] additionally refuses started rows, so it stays the
//! same-process provably-unsent, never-started path and must never be used
//! as stale recovery or after a stamp failure. One row per go-live is
//! retained indefinitely as the dedup fence; this slice adds no purge.
//! Rows carry no member identity (Twitch broadcaster id plus the
//! announcements channel), hence no member-erasure entry.

use sqlx::{Pool, Postgres};

/// Bounded lease for a never-started claim, in seconds. The window between
/// claiming and [`mark_live_send_started`] is a few in-process database
/// statements, so 30 s is ample headroom for a live owner while keeping a
/// takeover reachable soon after a crash. Lease length does not affect
/// safety: only the `send_started_at IS NULL` start fence decides who may
/// post, so at most one Discord POST happens however short the lease is.
/// A retry that finds the lease still fresh gets
/// [`ClaimOutcome::InFlight`] (answered 5xx, which asks Twitch to retry
/// instead of treating the go-live as delivered). Twitch does not document
/// its retry schedule, so no horizon is promised: recovery happens only if
/// a retry arrives after the lease expires, and this slice has no sweeper
/// to recover a go-live that is never retried.
pub const STALE_CLAIM_LEASE_SECS: i64 = 30;

/// One go-live claim: the `live_dedup_key` plus the rendered post it owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveClaim {
    /// 64 lowercase hex chars from `stream_funnel::live_dedup_key`.
    pub dedup_key: String,
    /// Twitch broadcaster user id (numeric string).
    pub broadcaster_id: String,
    /// Go-live instant from the event payload.
    pub started_at: String,
    /// Announcements channel snowflake the LIVE post goes to.
    pub channel_id: String,
    /// Rendered post from `stream_funnel::live_announcement`.
    pub content: String,
}

/// What one claim attempt won.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// This delivery owns the post; send it, then stamp or release. The
    /// payload is the row's claim token: an ownership capability the owner
    /// must present to [`mark_live_send_started`], [`mark_live_delivered`]
    /// and [`release_live`]. A takeover rotates it, fencing out the
    /// previous owner. Never log it.
    Claimed(String),
    /// A never-started claim exists but its lease is still fresh: its owner
    /// may still be sending, or may have crashed before starting. Answer
    /// 5xx, which asks Twitch to retry; a retry after the lease expires can
    /// take the claim over, and a retry after the owner starts or delivers
    /// sees `Duplicate` instead.
    InFlight,
    /// The go-live is started or delivered; acknowledge and skip.
    Duplicate,
}

/// Insert the go-live row, classifying any conflicting row in the same
/// call so the outcome is final and cannot be misread: `Claimed(token)`
/// means this delivery owns the post (a fresh insert, or an atomic
/// takeover of a stale never-started claim); `InFlight` means a fresh
/// never-started claim exists (answer 5xx); `Duplicate` means the row is
/// started or delivered (acknowledge and skip). Callers must NOT map a
/// conflicting insert straight to `Duplicate` — that two-step trap is
/// exactly what this classification removes; [`reclaim_stale_live`]
/// remains available for callers that already hold a claim attempt.
///
/// If the conflicting row disappears between the insert and the takeover
/// (its owner released a provably-unsent claim), the insert is retried
/// instead of answering `Duplicate`: that 2xx would tell Twitch the
/// go-live was delivered when nobody posted it. The loop is bounded; a
/// claim still churning after a few rounds answers `InFlight` (5xx).
pub async fn claim_live(
    pool: &Pool<Postgres>,
    claim: &LiveClaim,
) -> Result<ClaimOutcome, sqlx::Error> {
    for _ in 0..4 {
        let inserted: Option<(Option<String>,)> = sqlx::query_as(
            "INSERT INTO stream_live_claims
             (dedup_key, broadcaster_id, started_at, channel_id, content, claim_token)
             VALUES ($1, $2, $3, $4, $5, gen_random_uuid()::text)
             ON CONFLICT DO NOTHING RETURNING claim_token",
        )
        .bind(&claim.dedup_key)
        .bind(&claim.broadcaster_id)
        .bind(&claim.started_at)
        .bind(&claim.channel_id)
        .bind(&claim.content)
        .fetch_optional(pool)
        .await?;
        if let Some((Some(token),)) = inserted {
            return Ok(ClaimOutcome::Claimed(token));
        }
        if inserted.is_some() {
            // A tokenless row can only come from a pre-token writer; treat
            // it as owned elsewhere rather than inventing ownership.
            return Ok(ClaimOutcome::InFlight);
        }
        // Classify the conflict (and take it over if it is a stale
        // never-started claim) so `claim_live` alone is the receiver's
        // entry point and its outcome needs no follow-up call. `None`
        // means the row vanished mid-classification: loop back to the
        // insert rather than acknowledging a go-live nobody posted.
        if let Some(outcome) = classify_conflict(pool, claim, STALE_CLAIM_LEASE_SECS).await? {
            return Ok(outcome);
        }
    }
    Ok(ClaimOutcome::InFlight)
}

/// Delete an unposted claim whose send provably never reached Discord, so
/// a Twitch retry can claim again. Returns `true` only when the caller's
/// own, never-started row was released: the claim token must match and
/// `send_started_at` must still be NULL. Call this only in the same
/// process that attempted the send and proved non-delivery before the
/// send started (`SendFailed` disposition). NEVER call this on a timeout
/// or ambiguous result after sending, after [`mark_live_send_started`],
/// after a stamp failure, or as stale recovery: a started row stays
/// fenced even for its owner, because a post that landed with a lost
/// response must stay claimed or the retry double-posts. Stale retries
/// use [`reclaim_stale_live`], which refuses started rows.
pub async fn release_live(
    pool: &Pool<Postgres>,
    dedup_key: &str,
    claim_token: &str,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query(
        "DELETE FROM stream_live_claims
         WHERE dedup_key = $1 AND claim_token = $2
           AND delivered_at IS NULL AND send_started_at IS NULL",
    )
    .bind(dedup_key)
    .bind(claim_token)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

/// Stamp a posted claim with its Discord message receipt. Only the
/// current owner (matching claim token) can stamp. Returns `true` when a
/// pending row was stamped. The row is kept: its existence is the
/// restart-proof dedup fence.
pub async fn mark_live_delivered(
    pool: &Pool<Postgres>,
    dedup_key: &str,
    claim_token: &str,
    message_id: &str,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE stream_live_claims SET delivered_at = clock_timestamp(), message_id = $3
         WHERE dedup_key = $1 AND claim_token = $2 AND delivered_at IS NULL",
    )
    .bind(dedup_key)
    .bind(claim_token)
    .bind(message_id)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

/// Commit this fence BEFORE sending the first Discord POST. Only the
/// current owner (matching claim token) can start. On `false` the caller
/// sends nothing: the token is stale (a takeover rotated ownership), or
/// the row is already started or delivered, so this delivery is a
/// `Duplicate`. After `true` only [`mark_live_delivered`] with the same
/// token can settle the row; [`release_live`] can no longer free it and
/// stale recovery will never reclaim it.
pub async fn mark_live_send_started(
    pool: &Pool<Postgres>,
    dedup_key: &str,
    claim_token: &str,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE stream_live_claims SET send_started_at = clock_timestamp()
         WHERE dedup_key = $1 AND claim_token = $2
           AND delivered_at IS NULL AND send_started_at IS NULL",
    )
    .bind(dedup_key)
    .bind(claim_token)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

/// Atomically take over one stale never-sent claim. Returns
/// `Claimed(token)` when this retry now owns the post (its `claimed_at`
/// lease restarts, its channel/content refresh to this retry's values and
/// its claim token rotates, fencing out the previous owner); `InFlight`
/// when a never-started row exists but its lease is still fresh, so the
/// caller must answer 5xx and let Twitch retry again later; `Duplicate`
/// when the row is started or delivered. If the row disappears before the
/// takeover lands (its owner released a provably-unsent claim), this
/// falls back to a fresh [`claim_live`] insert instead of answering
/// `Duplicate` for a go-live nobody posted.
///
/// Only rows with `delivered_at IS NULL AND send_started_at IS NULL AND
/// claimed_at` older than `stale_secs` are eligible, in a single `UPDATE`.
/// Concurrent retries cannot both win: the first refreshes `claimed_at`
/// and rotates the token, and the second sees a fresh lease (`InFlight`).
/// Delivered rows and ambiguous started rows (a Discord send may have
/// succeeded while [`mark_live_delivered`] failed) are never reclaimed,
/// so a retry can never double-post them.
pub async fn reclaim_stale_live(
    pool: &Pool<Postgres>,
    claim: &LiveClaim,
    stale_secs: i64,
) -> Result<ClaimOutcome, sqlx::Error> {
    match classify_conflict(pool, claim, stale_secs).await? {
        Some(outcome) => Ok(outcome),
        // The conflicting row vanished: claim it fresh.
        None => claim_live(pool, claim).await,
    }
}

/// Take over a conflicting row if it is a stale never-started claim, else
/// classify it. Returns `None` when no row exists (any more), so callers
/// can loop back to the insert instead of misreading the absence as
/// `Duplicate`.
async fn classify_conflict(
    pool: &Pool<Postgres>,
    claim: &LiveClaim,
    stale_secs: i64,
) -> Result<Option<ClaimOutcome>, sqlx::Error> {
    let updated: Option<(Option<String>,)> = sqlx::query_as(
        "UPDATE stream_live_claims
         SET broadcaster_id = $2, started_at = $3, channel_id = $4, content = $5,
             claimed_at = clock_timestamp(), claim_token = gen_random_uuid()::text
         WHERE dedup_key = $1 AND delivered_at IS NULL AND send_started_at IS NULL
           AND claimed_at < clock_timestamp() - make_interval(secs => $6)
         RETURNING claim_token",
    )
    .bind(&claim.dedup_key)
    .bind(&claim.broadcaster_id)
    .bind(&claim.started_at)
    .bind(&claim.channel_id)
    .bind(&claim.content)
    .bind(stale_secs as f64)
    .fetch_optional(pool)
    .await?;
    if let Some((Some(token),)) = updated {
        return Ok(Some(ClaimOutcome::Claimed(token)));
    }
    if updated.is_some() {
        // Tokenless row (pre-token writer): owned elsewhere, never ours.
        return Ok(Some(ClaimOutcome::InFlight));
    }
    // The takeover lost: distinguish a fresh never-started claim (retry
    // later, answer 5xx) from a permanently fenced started/delivered row
    // (acknowledge, 200) — and from a row that is gone entirely (retry
    // the insert). A row that turns stale between the UPDATE and this
    // read still reports `InFlight`, which is safe: the next retry takes
    // it over.
    let row: Option<(bool,)> = sqlx::query_as(
        "SELECT delivered_at IS NULL AND send_started_at IS NULL
         FROM stream_live_claims WHERE dedup_key = $1",
    )
    .bind(&claim.dedup_key)
    .fetch_optional(pool)
    .await?;
    Ok(match row {
        None => None,
        Some((true,)) => Some(ClaimOutcome::InFlight),
        Some((false,)) => Some(ClaimOutcome::Duplicate),
    })
}
