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
//! [`reclaim_stale_live`] atomically takes over one stale unposted row whose
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

/// Bounded lease for a never-started claim, in seconds. A normal
/// claim → start → send → stamp cycle completes in seconds; 300 s gives the
/// live owner ample headroom while ensuring a crash-before-send recovers
/// inside the EventSub retry horizon. Matches the existing 300 s delivery
/// lease (`audit_store::DELIVERY_LEASE_SECONDS`).
pub const STALE_CLAIM_LEASE_SECS: i64 = 300;

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
    /// The go-live is already claimed or delivered; acknowledge and skip.
    Duplicate,
}

/// Insert the go-live row. `Claimed` means this delivery owns the post;
/// `Duplicate` means a retry, replay, or restart raced us to it.
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
    Ok(if inserted == 1 {
        ClaimOutcome::Claimed
    } else {
        ClaimOutcome::Duplicate
    })
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
/// channel/content refresh to this retry's values); `Duplicate` otherwise.
///
/// Only rows with `delivered_at IS NULL AND send_started_at IS NULL AND
/// claimed_at` older than `stale_secs` are eligible, in a single `UPDATE`.
/// Concurrent retries cannot both win: the first refreshes `claimed_at` and
/// the second sees a fresh lease. Delivered rows and ambiguous started rows
/// (a Discord send may have succeeded while [`mark_live_delivered`] failed)
/// are never reclaimed, so a retry can never double-post them.
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
    Ok(if updated == 1 {
        ClaimOutcome::Claimed
    } else {
        ClaimOutcome::Duplicate
    })
}
