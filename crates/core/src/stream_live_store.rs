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
//! `delivered_at IS NULL` would double-post it. [`release_live`] stays the
//! same-process provably-unsent path and must never be used as stale
//! recovery. One row per go-live is retained indefinitely as the dedup fence;
//! this slice adds no purge. Rows carry no member identity (Twitch
//! broadcaster id plus the announcements channel), hence no member-erasure
//! entry.

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// This delivery owns the post; send it, then stamp or release.
    Claimed,
    /// A never-started claim exists but its lease is still fresh: its owner
    /// may still be sending, or may have crashed before starting. Answer
    /// 5xx, which asks Twitch to retry; a retry after the lease expires can
    /// take the claim over, and a retry after the owner starts or delivers
    /// sees `Duplicate` instead.
    InFlight,
    /// The go-live is started or delivered; acknowledge and skip. Also the
    /// answer when no row exists to reclaim.
    Duplicate,
}

/// Insert the go-live row, classifying any conflicting row in the same
/// call so the outcome is final and cannot be misread: `Claimed` means
/// this delivery owns the post (a fresh insert, or an atomic takeover of a
/// stale never-started claim); `InFlight` means a fresh never-started
/// claim exists (answer 5xx); `Duplicate` means the row is started or
/// delivered (acknowledge and skip). Callers must NOT map a conflicting
/// insert straight to `Duplicate` — that two-step trap is exactly what
/// this classification removes; [`reclaim_stale_live`] remains available
/// for callers that already hold a claim attempt.
pub async fn claim_live(
    pool: &Pool<Postgres>,
    claim: &LiveClaim,
) -> Result<ClaimOutcome, sqlx::Error> {
    let inserted = sqlx::query(
        "INSERT INTO stream_live_claims
         (dedup_key, broadcaster_id, started_at, channel_id, content)
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
    )
    .bind(&claim.dedup_key)
    .bind(&claim.broadcaster_id)
    .bind(&claim.started_at)
    .bind(&claim.channel_id)
    .bind(&claim.content)
    .execute(pool)
    .await?
    .rows_affected();
    if inserted == 1 {
        Ok(ClaimOutcome::Claimed)
    } else {
        // Classify the conflict (and take it over if it is a stale
        // never-started claim) so `claim_live` alone is the receiver's
        // entry point and its outcome needs no follow-up call.
        reclaim_stale_live(pool, claim, STALE_CLAIM_LEASE_SECS).await
    }
}

/// Delete an unposted claim whose send provably never reached Discord, so a
/// Twitch retry can claim again. Returns `true` when a pending row was
/// released. Call this only in the same process that attempted the send and
/// proved non-delivery (`SendFailed` disposition). NEVER call this on a
/// timeout or ambiguous result after sending, and NEVER as stale recovery:
/// a post that landed with a lost response must stay claimed, or the retry
/// double-posts. Stale retries use [`reclaim_stale_live`], which refuses
/// started rows.
pub async fn release_live(pool: &Pool<Postgres>, dedup_key: &str) -> Result<bool, sqlx::Error> {
    Ok(
        sqlx::query("DELETE FROM stream_live_claims WHERE dedup_key = $1 AND delivered_at IS NULL")
            .bind(dedup_key)
            .execute(pool)
            .await?
            .rows_affected()
            == 1,
    )
}

/// Stamp a posted claim with its Discord message receipt. Returns `true`
/// when a pending row was stamped. The row is kept: its existence is the
/// restart-proof dedup fence.
pub async fn mark_live_delivered(
    pool: &Pool<Postgres>,
    dedup_key: &str,
    message_id: &str,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE stream_live_claims SET delivered_at = clock_timestamp(), message_id = $2
         WHERE dedup_key = $1 AND delivered_at IS NULL",
    )
    .bind(dedup_key)
    .bind(message_id)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

/// Commit this fence BEFORE sending the first Discord POST. On `false` the
/// caller sends nothing: the row is already started or delivered, so this
/// delivery is a `Duplicate`. After `true` only [`mark_live_delivered`] can
/// settle the row and only [`release_live`] (on provable send failure) can
/// free it; stale recovery will never reclaim it.
pub async fn mark_live_send_started(
    pool: &Pool<Postgres>,
    dedup_key: &str,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE stream_live_claims SET send_started_at = clock_timestamp()
         WHERE dedup_key = $1 AND delivered_at IS NULL AND send_started_at IS NULL",
    )
    .bind(dedup_key)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

/// Atomically take over one stale never-sent claim. Returns `Claimed` when
/// this retry now owns the post (its `claimed_at` lease restarts and its
/// channel/content refresh to this retry's values); `InFlight` when a
/// never-started row exists but its lease is still fresh, so the caller
/// must answer 5xx and let Twitch retry again later; `Duplicate` when the
/// row is started or delivered (or does not exist).
///
/// Only rows with `delivered_at IS NULL AND send_started_at IS NULL AND
/// claimed_at` older than `stale_secs` are eligible, in a single `UPDATE`.
/// Concurrent retries cannot both win: the first refreshes `claimed_at` and
/// the second sees a fresh lease (`InFlight`). Delivered rows and ambiguous
/// started rows (a Discord send may have succeeded while
/// [`mark_live_delivered`] failed) are never reclaimed, so a retry can
/// never double-post them.
pub async fn reclaim_stale_live(
    pool: &Pool<Postgres>,
    claim: &LiveClaim,
    stale_secs: i64,
) -> Result<ClaimOutcome, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE stream_live_claims
         SET broadcaster_id = $2, started_at = $3, channel_id = $4, content = $5,
             claimed_at = clock_timestamp()
         WHERE dedup_key = $1 AND delivered_at IS NULL AND send_started_at IS NULL
           AND claimed_at < clock_timestamp() - make_interval(secs => $6)",
    )
    .bind(&claim.dedup_key)
    .bind(&claim.broadcaster_id)
    .bind(&claim.started_at)
    .bind(&claim.channel_id)
    .bind(&claim.content)
    .bind(stale_secs as f64)
    .execute(pool)
    .await?
    .rows_affected();
    if updated == 1 {
        return Ok(ClaimOutcome::Claimed);
    }
    // The takeover lost: distinguish a fresh never-started claim (retry
    // later, answer 5xx) from a permanently fenced started/delivered row
    // (acknowledge, 200). A row that turns stale between the UPDATE and
    // this read still reports `InFlight`, which is safe: the next retry
    // takes it over.
    let in_flight: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM stream_live_claims
         WHERE dedup_key = $1 AND delivered_at IS NULL AND send_started_at IS NULL)",
    )
    .bind(&claim.dedup_key)
    .fetch_one(pool)
    .await?;
    Ok(if in_flight {
        ClaimOutcome::InFlight
    } else {
        ClaimOutcome::Duplicate
    })
}
