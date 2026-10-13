//! SQLx persistence for the stream go-live dedup ledger (`stream_live_claims`).
//!
//! Claim-before-post arbitration for Twitch `stream.online` announcements:
//! [`claim_live`] inserts the go-live row with `ON CONFLICT DO NOTHING`, so
//! concurrent EventSub redeliveries of the same `broadcaster + started_at`
//! instant collapse to exactly one owner. A send that provably never reached
//! Discord deletes its unposted row ([`release_live`]) so the Twitch retry
//! can claim again; a delivered row is stamped ([`mark_live_delivered`]) and
//! never deleted, so a restart between post and acknowledgement cannot
//! double-post. Rows carry no member identity (Twitch broadcaster id plus
//! the announcements channel), hence no member-erasure entry.

use sqlx::{Pool, Postgres};

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
/// released. NEVER call this on a timeout or ambiguous result after sending:
/// a post that landed with a lost response must stay claimed, or the retry
/// double-posts.
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
