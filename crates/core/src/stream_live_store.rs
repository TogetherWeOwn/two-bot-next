//! SQLx persistence for the stream go-live dedup ledger (`stream_live_claims`).
//!
//! Claim-before-post arbitration for Twitch `stream.online` announcements:
//! [`claim_live`] inserts the go-live row with `ON CONFLICT DO NOTHING`, so
//! concurrent EventSub redeliveries of the same `broadcaster + started_at`
//! instant collapse to exactly one owner. The receiver commits
//! [`mark_live_send_started`] BEFORE the first Discord POST (mirroring
//! `automod_delivery_claims.mutation_started`: no lease automatically retries
//! a mutation whose Discord outcome is uncertain). A send that provably never
//! reached Discord deletes its unposted row ([`release_live`] before the
//! send starts, [`release_started_live`] after it starts) so the Twitch
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
//! [`mark_live_delivered`], [`release_live`] and [`release_started_live`]
//! only act on the row whose token they present. A timed-out-but-running
//! original owner therefore
//! cannot start, stamp or release the row a takeover gave to a new owner:
//! its token no longer matches, so exactly one owner can post.
//! [`release_live`] additionally refuses started rows, so it stays the
//! same-process provably-unsent, never-started path and must never be used
//! as stale recovery or after a stamp failure; [`release_started_live`] is
//! the separate token-gated path for a started row whose Discord send
//! provably never landed, and likewise must never be used on an ambiguous
//! outcome or after a stamp failure. One row per go-live is kept as the
//! dedup fence while it can still matter: delivered rows expire past
//! [`DELIVERED_DEDUP_RETENTION_DAYS`] via [`expire_delivered_live`], which
//! runs on the retention cadence (never on the send path) and deletes only
//! rows no live claim or send can reference. Started and ambiguous rows
//! (`delivered_at IS NULL`) never expire. Rows carry no member identity
//! (Twitch broadcaster id plus the announcements channel), hence no
//! member-erasure entry.

use std::{future::Future, pin::Pin};

use sqlx::{Pool, Postgres};

use crate::stream_funnel::{live_announcement, live_dedup_key, StreamOnline};
use crate::stream_live::{disposition, Disposition, GoLiveOutcome, Stage};
use crate::Secret;

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

/// How long a delivered go-live row stays a dedup fence, in days. Twitch
/// retries and redeliveries arrive within minutes of the event; 90 days keeps
/// the fence far beyond any retry window while bounding table growth to one
/// small row per go-live. This is the default horizon the retention cadence
/// passes to [`expire_delivered_live`]; started and ambiguous rows
/// (`delivered_at IS NULL`) are never eligible, however old.
pub const DELIVERED_DEDUP_RETENTION_DAYS: i32 = 90;

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
    /// must present to [`mark_live_send_started`], [`mark_live_delivered`],
    /// [`release_live`] and [`release_started_live`]. A takeover rotates
    /// it, fencing out the previous owner. Never log it: the [`Secret`]
    /// wrapper redacts it
    /// under `Debug`, and callers reach it only through `expose()`.
    Claimed(Secret<String>),
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
            return Ok(ClaimOutcome::Claimed(token.into()));
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

/// Delete a started, undelivered claim whose Discord send provably never
/// reached Discord, so a Twitch retry can claim again. Returns `true` only
/// when the caller's own started row was released: the claim token must
/// match, `send_started_at` must be set and `delivered_at` must still be
/// NULL. Call this ONLY for the `SendFailed` disposition after
/// [`mark_live_send_started`], where Discord proved non-delivery (an
/// explicit rejection such as 429/4xx, or a connection refused before any
/// response could carry a post). NEVER call this on a timeout or other
/// ambiguous result, or after a stamp failure: a post that landed with a
/// lost response must stay claimed or the retry double-posts. The store
/// cannot verify the proof itself; that classification is the receiver's
/// `SendFailed` contract. Stale retries never use this path:
/// [`reclaim_stale_live`] refuses started rows.
pub async fn release_started_live(
    pool: &Pool<Postgres>,
    dedup_key: &str,
    claim_token: &str,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query(
        "DELETE FROM stream_live_claims
         WHERE dedup_key = $1 AND claim_token = $2
           AND delivered_at IS NULL AND send_started_at IS NOT NULL",
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
/// stale recovery will never reclaim it. Only [`release_started_live`],
/// on a provably-unsent `SendFailed`, may delete it.
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

/// Delete delivered go-live rows older than `horizon_days`, returning how
/// many rows went. Only rows stamped `delivered_at` before the horizon are
/// eligible: anything a live claim or send could still reference is
/// `delivered_at IS NULL` (never-started, or started-but-ambiguous after a
/// stamp failure), so the single-table predicate excludes it atomically with
/// the delete and no second-table join is needed. The delete removes full
/// rows with no tombstone, so no broadcaster or channel id survives in the
/// ledger. Call this from the retention cadence only, never on the send
/// path: a delivery retried inside the horizon still finds its fence and
/// answers `Duplicate` instead of double-posting.
pub async fn expire_delivered_live<'e, E>(
    executor: E,
    horizon_days: i32,
) -> Result<u64, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    Ok(sqlx::query(
        "DELETE FROM stream_live_claims
         WHERE delivered_at IS NOT NULL
           AND delivered_at < clock_timestamp() - make_interval(days => $1)",
    )
    .bind(horizon_days)
    .execute(executor)
    .await?
    .rows_affected())
}

/// Count the delivered rows [`expire_delivered_live`] would delete, without
/// deleting anything. The dry-run retention projection reports this so the
/// live run never surprises.
pub async fn count_expired_delivered_live<'e, E>(
    executor: E,
    horizon_days: i32,
) -> Result<u64, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM stream_live_claims
         WHERE delivered_at IS NOT NULL
           AND delivered_at < clock_timestamp() - make_interval(days => $1)",
    )
    .bind(horizon_days)
    .fetch_one(executor)
    .await?;
    Ok(count as u64)
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
        return Ok(Some(ClaimOutcome::Claimed(token.into())));
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

/// Boxed ledger future; keeps [`LiveLedger`] object-safe so the receiver
/// can hold `Arc<dyn LiveLedger>` and tests can inject an in-memory fake.
pub type LedgerFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, sqlx::Error>> + Send + 'a>>;

/// The durable-ledger operations [`deliver_go_live`] needs, in claim order.
/// The Postgres pool is the production implementation; every method is the
/// token-fenced store call of the same name, so the glue cannot bypass the
/// ownership fences documented on those functions.
pub trait LiveLedger: Send + Sync {
    /// [`claim_live`]
    fn claim<'a>(&'a self, claim: &'a LiveClaim) -> LedgerFuture<'a, ClaimOutcome>;
    /// [`mark_live_send_started`]
    fn mark_started<'a>(
        &'a self,
        dedup_key: &'a str,
        claim_token: &'a str,
    ) -> LedgerFuture<'a, bool>;
    /// [`mark_live_delivered`]
    fn mark_delivered<'a>(
        &'a self,
        dedup_key: &'a str,
        claim_token: &'a str,
        message_id: &'a str,
    ) -> LedgerFuture<'a, bool>;
    /// [`release_started_live`]
    fn release_started<'a>(
        &'a self,
        dedup_key: &'a str,
        claim_token: &'a str,
    ) -> LedgerFuture<'a, bool>;
}

impl LiveLedger for Pool<Postgres> {
    fn claim<'a>(&'a self, claim: &'a LiveClaim) -> LedgerFuture<'a, ClaimOutcome> {
        Box::pin(claim_live(self, claim))
    }

    fn mark_started<'a>(
        &'a self,
        dedup_key: &'a str,
        claim_token: &'a str,
    ) -> LedgerFuture<'a, bool> {
        Box::pin(mark_live_send_started(self, dedup_key, claim_token))
    }

    fn mark_delivered<'a>(
        &'a self,
        dedup_key: &'a str,
        claim_token: &'a str,
        message_id: &'a str,
    ) -> LedgerFuture<'a, bool> {
        Box::pin(mark_live_delivered(
            self,
            dedup_key,
            claim_token,
            message_id,
        ))
    }

    fn release_started<'a>(
        &'a self,
        dedup_key: &'a str,
        claim_token: &'a str,
    ) -> LedgerFuture<'a, bool> {
        Box::pin(release_started_live(self, dedup_key, claim_token))
    }
}

/// Why one Discord send failed, as the settle rules need it. The sender —
/// never the glue — makes this classification, because only it saw the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveSendError {
    /// Discord provably never received the post (an explicit rejection, a
    /// 429, or a local guard refusal before any wire attempt). Only this
    /// variant may release the claim (`Stage::SendFailed`).
    ProvenNoSend,
    /// The post may have landed (timeout, transport failure, unreadable
    /// receipt): the started claim stays fenced forever and the Twitch
    /// retry is answered as a duplicate, so it can never double-post.
    Ambiguous,
}

/// Boxed sender future; keeps [`LiveSender`] object-safe for injection.
pub type SendFuture<'a> = Pin<Box<dyn Future<Output = Result<String, LiveSendError>> + Send + 'a>>;

/// Posts one rendered go-live announcement and returns its Discord message
/// id. Implementations MUST send with `allowed_mentions: { parse: [] }`
/// (the shared executor's `post_message` shape) and MUST NOT retry inside
/// one call: retry ownership belongs to the durable claim, and an in-call
/// retry after an ambiguous first attempt could double-post.
pub trait LiveSender: Send + Sync {
    /// Send `content` to `channel_id`, exactly once.
    fn send_live<'a>(&'a self, channel_id: &'a str, content: &'a str) -> SendFuture<'a>;
}

/// Static delivery inputs for [`deliver_go_live`]: who may be announced,
/// which invite the post links, and where it goes. No secret travels here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveDeliveryConfig {
    /// Twitch broadcaster user ids allowed to announce.
    pub broadcasters: Vec<String>,
    /// Discord invite URL rendered into the post (validated at render).
    pub invite_url: String,
    /// Announcements channel snowflake.
    pub channel_id: String,
}

/// One [`deliver_go_live`] result: what happened, plus the
/// [`stream_live::disposition`](crate::stream_live::disposition) the
/// receiver answers Twitch with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoLiveDelivery {
    pub outcome: GoLiveOutcome,
    pub disposition: Disposition,
}

fn settled(outcome: GoLiveOutcome, stage: Stage) -> GoLiveDelivery {
    GoLiveDelivery {
        outcome,
        disposition: disposition(stage),
    }
}

/// Run one verified `stream.online` through the durable claim → start →
/// send → settle flow (the async glue for
/// [`stream_live::disposition`](crate::stream_live::disposition)).
///
/// Order is the safety contract: allowlist first (a valid signature proves
/// Twitch sent it, not that we asked for it), render before claiming (an
/// invalid invite never burns a claim), claim before posting, commit
/// `mark_started` before the first Discord POST, and settle exactly once.
/// Only a [`LiveSendError::ProvenNoSend`] releases the started claim and
/// answers 5xx for a Twitch retry; an ambiguous send and a stamp failure
/// keep the row fenced, so the retry sees `Duplicate` and cannot
/// double-post. Ledger errors answer 5xx without releasing anything.
pub async fn deliver_go_live<L, S>(
    ledger: &L,
    sender: &S,
    config: &LiveDeliveryConfig,
    stream: &StreamOnline,
) -> GoLiveDelivery
where
    L: LiveLedger + ?Sized,
    S: LiveSender + ?Sized,
{
    if !config.broadcasters.contains(&stream.broadcaster_user_id) {
        return settled(GoLiveOutcome::NotAllowed, Stage::AllowlistDenied);
    }
    // Render first so an invalid invite never burns the claim.
    let content = match live_announcement(stream, None, &config.invite_url) {
        Ok(content) => content,
        Err(_) => {
            return settled(
                GoLiveOutcome::Failed {
                    reason: "invalid_invite",
                },
                Stage::InvalidInvite,
            );
        }
    };
    let claim = LiveClaim {
        dedup_key: live_dedup_key(stream),
        broadcaster_id: stream.broadcaster_user_id.clone(),
        started_at: stream.started_at.clone(),
        channel_id: config.channel_id.clone(),
        content,
    };
    let token = match ledger.claim(&claim).await {
        Ok(ClaimOutcome::Claimed(token)) => token,
        Ok(ClaimOutcome::Duplicate) => {
            return settled(GoLiveOutcome::Duplicate, Stage::Duplicate);
        }
        Ok(ClaimOutcome::InFlight) => {
            return settled(
                GoLiveOutcome::Failed {
                    reason: "in_flight",
                },
                Stage::InFlight,
            );
        }
        Err(_) => {
            return settled(
                GoLiveOutcome::Failed {
                    reason: "claim_failed",
                },
                Stage::ClaimFailed,
            );
        }
    };
    let token = token.expose().to_owned();
    // Commit the start fence BEFORE the first Discord POST. A stale token
    // (a takeover rotated ownership) sends nothing: this delivery lost.
    match ledger.mark_started(&claim.dedup_key, &token).await {
        Ok(true) => {}
        Ok(false) => return settled(GoLiveOutcome::Duplicate, Stage::Duplicate),
        Err(_) => {
            return settled(
                GoLiveOutcome::Failed {
                    reason: "claim_failed",
                },
                Stage::ClaimFailed,
            );
        }
    }
    let message_id = match sender.send_live(&claim.channel_id, &claim.content).await {
        Ok(message_id) => message_id,
        Err(LiveSendError::ProvenNoSend) => {
            // The one releasable failure: Discord provably never got the
            // post, so freeing the started row lets the Twitch retry claim
            // again. If the release itself fails (error or noop) the row
            // stays fenced, the retry answers Duplicate, and the
            // announcement is lost without ever being posted: report that
            // distinctly so it cannot be mistaken for a retryable
            // `send_failed`. Either way nothing here sends twice — the
            // release path never posts.
            let released = ledger.release_started(&claim.dedup_key, &token).await;
            let reason = match released {
                Ok(true) => "send_failed",
                Ok(false) | Err(_) => {
                    // Fixed class, safe to alert on as-is: the message and
                    // fields are literals, never delivery data (no
                    // broadcaster, channel, invite, claim token or dedup
                    // key) and never the database error text.
                    tracing::warn!(
                        target: "two_bot_core::stream_live",
                        reason = "send_failed_unreleased",
                        "send_failed_unreleased: started claim could not be released \
                         after a provably-unsent send; the retry dedupes and the \
                         announcement is lost"
                    );
                    "send_failed_unreleased"
                }
            };
            return settled(GoLiveOutcome::Failed { reason }, Stage::SendFailed);
        }
        Err(LiveSendError::Ambiguous) => {
            // Same settle posture as a stamp failure: the post may have
            // landed, so the started row stays fenced and is never
            // released; the 5xx retry is answered Duplicate.
            return settled(
                GoLiveOutcome::Failed {
                    reason: "ambiguous_send",
                },
                Stage::StampFailed,
            );
        }
    };
    match ledger
        .mark_delivered(&claim.dedup_key, &token, &message_id)
        .await
    {
        Ok(true) => settled(GoLiveOutcome::Posted { message_id }, Stage::Sent),
        Ok(false) | Err(_) => settled(
            GoLiveOutcome::Failed {
                reason: "stamp_failed",
            },
            Stage::StampFailed,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivered_dedup_horizon_outlives_any_retry_window() {
        // Const context: clippy `assertions_on_constants` rejects this as a
        // runtime assert, and a horizon edited below the retry window fails
        // the build here.
        const _: () = {
            assert!(DELIVERED_DEDUP_RETENTION_DAYS >= 30);
        };
    }

    #[test]
    fn claimed_token_is_redacted_in_both_debug_forms() {
        let fixture_token = "fixture-claim-token-sentinel";
        let outcome = ClaimOutcome::Claimed(Secret::new(fixture_token.to_owned()));
        for output in [format!("{outcome:?}"), format!("{outcome:#?}")] {
            assert!(output.contains("[REDACTED]"), "{output}");
            assert!(!output.contains(fixture_token), "{output}");
        }
        let ClaimOutcome::Claimed(token) = outcome else {
            unreachable!()
        };
        assert_eq!(token.expose(), fixture_token);
    }

    // --- deliver_go_live glue over injected fakes (no database, no wire) ---

    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum RowState {
        Claimed,
        Started,
        Delivered,
    }

    /// Distinctive database-error text for the fake release failure.
    /// Shared by the fake and `assert_unreleased_warn` so the forbidden
    /// match in the warn helper cannot go stale if the fake text changes.
    const FAKE_RELEASE_ERROR_SENTINEL: &str = "fake-release-error-sentinel";

    /// In-memory ledger mirroring the store's classification at the trait
    /// boundary: fresh insert claims, a never-started row is `InFlight`, a
    /// started/delivered row is `Duplicate`, release only frees a started
    /// row. The SQL fences themselves are pinned by `stream_live_store`
    /// integration tests against a disposable database.
    #[derive(Default)]
    struct FakeLedger {
        rows: Mutex<HashMap<String, RowState>>,
        releases: Mutex<usize>,
        fail_claim: bool,
        fail_stamp: bool,
        fail_release_err: bool,
        fail_release_noop: bool,
    }

    impl FakeLedger {
        fn row(&self, key: &str) -> Option<RowState> {
            self.rows.lock().expect("rows").get(key).copied()
        }
        fn releases(&self) -> usize {
            *self.releases.lock().expect("releases")
        }
    }

    impl LiveLedger for FakeLedger {
        fn claim<'a>(&'a self, claim: &'a LiveClaim) -> LedgerFuture<'a, ClaimOutcome> {
            Box::pin(async move {
                if self.fail_claim {
                    return Err(sqlx::Error::InvalidArgument("fake claim failure".into()));
                }
                let mut rows = self.rows.lock().expect("rows");
                Ok(match rows.get(&claim.dedup_key) {
                    None => {
                        rows.insert(claim.dedup_key.clone(), RowState::Claimed);
                        ClaimOutcome::Claimed(Secret::new("fake-token".to_owned()))
                    }
                    Some(RowState::Claimed) => ClaimOutcome::InFlight,
                    Some(RowState::Started | RowState::Delivered) => ClaimOutcome::Duplicate,
                })
            })
        }

        fn mark_started<'a>(
            &'a self,
            dedup_key: &'a str,
            _claim_token: &'a str,
        ) -> LedgerFuture<'a, bool> {
            Box::pin(async move {
                let mut rows = self.rows.lock().expect("rows");
                match rows.get_mut(dedup_key) {
                    Some(state @ RowState::Claimed) => {
                        *state = RowState::Started;
                        Ok(true)
                    }
                    _ => Ok(false),
                }
            })
        }

        fn mark_delivered<'a>(
            &'a self,
            dedup_key: &'a str,
            _claim_token: &'a str,
            _message_id: &'a str,
        ) -> LedgerFuture<'a, bool> {
            Box::pin(async move {
                if self.fail_stamp {
                    return Err(sqlx::Error::InvalidArgument("fake stamp failure".into()));
                }
                let mut rows = self.rows.lock().expect("rows");
                match rows.get_mut(dedup_key) {
                    Some(state @ RowState::Started) => {
                        *state = RowState::Delivered;
                        Ok(true)
                    }
                    _ => Ok(false),
                }
            })
        }

        fn release_started<'a>(
            &'a self,
            dedup_key: &'a str,
            _claim_token: &'a str,
        ) -> LedgerFuture<'a, bool> {
            Box::pin(async move {
                if self.fail_release_err {
                    return Err(sqlx::Error::InvalidArgument(
                        FAKE_RELEASE_ERROR_SENTINEL.into(),
                    ));
                }
                if self.fail_release_noop {
                    return Ok(false);
                }
                let mut rows = self.rows.lock().expect("rows");
                if rows.get(dedup_key) == Some(&RowState::Started) {
                    rows.remove(dedup_key);
                    *self.releases.lock().expect("releases") += 1;
                    Ok(true)
                } else {
                    Ok(false)
                }
            })
        }
    }

    #[derive(Default)]
    struct FakeSender {
        calls: Mutex<Vec<(String, String)>>,
        script: Mutex<VecDeque<Result<String, LiveSendError>>>,
    }

    impl FakeSender {
        fn scripted(answers: Vec<Result<String, LiveSendError>>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                script: Mutex::new(answers.into()),
            }
        }
        fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().expect("calls").clone()
        }
    }

    impl LiveSender for FakeSender {
        fn send_live<'a>(&'a self, channel_id: &'a str, content: &'a str) -> SendFuture<'a> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .expect("calls")
                    .push((channel_id.to_owned(), content.to_owned()));
                self.script
                    .lock()
                    .expect("script")
                    .pop_front()
                    .expect("script must answer every send")
            })
        }
    }

    fn stream() -> StreamOnline {
        StreamOnline {
            broadcaster_user_id: "123456".to_owned(),
            broadcaster_login: "ourgamenight".to_owned(),
            started_at: "2026-10-12T20:00:00Z".to_owned(),
        }
    }

    fn delivery_config() -> LiveDeliveryConfig {
        LiveDeliveryConfig {
            broadcasters: vec!["123456".to_owned()],
            invite_url: "https://discord.gg/example".to_owned(),
            channel_id: "777".to_owned(),
        }
    }

    /// Scoped warn capture for the lost-go-live tests. A thread-local
    /// default (not `set_global_default`, which a sibling core test
    /// module already owns) is enough here: `deliver_go_live` never
    /// spawns, so every event is emitted on this test thread, and the
    /// failed-release warn callsite is only ever reached under one of
    /// these captures.
    #[derive(Clone, Default)]
    struct WarnCapture(std::sync::Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for WarnCapture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("capture").extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for WarnCapture {
        type Writer = WarnCapture;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl WarnCapture {
        fn install(&self) -> tracing::subscriber::DefaultGuard {
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_max_level(tracing::Level::WARN)
                .with_writer(self.clone())
                .finish();
            tracing::subscriber::set_default(subscriber)
        }

        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().expect("capture")).into_owned()
        }
    }

    /// The failed-release warn is a fixed class: a WARN line naming the
    /// `send_failed_unreleased` reason and carrying no delivery data
    /// (no broadcaster, login, channel, invite, claim token or dedup
    /// key) and no database error text, so it is safe to alert on as-is.
    fn assert_unreleased_warn(capture: &WarnCapture) {
        let text = capture.text();
        assert!(
            text.lines()
                .any(|line| line.contains("WARN") && line.contains("send_failed_unreleased")),
            "expected an unreleased warn, got: {text}"
        );
        let line = text
            .lines()
            .find(|line| line.contains("WARN") && line.contains("send_failed_unreleased"))
            .expect("warn line present (asserted above)");
        for forbidden in [
            "123456",
            "ourgamenight",
            "777",
            "discord.gg/example",
            "fake-token",
            FAKE_RELEASE_ERROR_SENTINEL,
            &live_dedup_key(&stream()),
        ] {
            assert!(
                !line.contains(forbidden),
                "warn must not carry delivery data or database error text ({forbidden}): {line}"
            );
        }
    }

    #[tokio::test]
    async fn deliver_posts_once_stamps_and_dedupes_the_retry() {
        let ledger = FakeLedger::default();
        let sender = FakeSender::scripted(vec![Ok("msg-1".to_owned())]);
        let config = delivery_config();
        let first = deliver_go_live(&ledger, &sender, &config, &stream()).await;
        assert_eq!(
            first.outcome,
            GoLiveOutcome::Posted {
                message_id: "msg-1".to_owned()
            }
        );
        assert_eq!(first.disposition, disposition(Stage::Sent));
        let calls = sender.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "777");
        assert!(calls[0].1.contains("https://www.twitch.tv/ourgamenight"));
        assert!(calls[0].1.contains("https://discord.gg/example"));
        assert_eq!(
            ledger.row(&live_dedup_key(&stream())),
            Some(RowState::Delivered)
        );

        let retry = deliver_go_live(&ledger, &sender, &config, &stream()).await;
        assert_eq!(retry.outcome, GoLiveOutcome::Duplicate);
        assert_eq!(retry.disposition.status, 200);
        assert_eq!(sender.calls().len(), 1, "a retry must never re-post");
    }

    #[tokio::test]
    async fn deliver_denied_broadcaster_never_claims_or_sends() {
        let ledger = FakeLedger::default();
        let sender = FakeSender::default();
        let mut config = delivery_config();
        config.broadcasters = vec!["999".to_owned()];
        let out = deliver_go_live(&ledger, &sender, &config, &stream()).await;
        assert_eq!(out.outcome, GoLiveOutcome::NotAllowed);
        assert_eq!(out.disposition, disposition(Stage::AllowlistDenied));
        assert_eq!(ledger.row(&live_dedup_key(&stream())), None);
        assert!(sender.calls().is_empty());
    }

    #[tokio::test]
    async fn deliver_invalid_invite_never_claims() {
        let ledger = FakeLedger::default();
        let sender = FakeSender::default();
        let mut config = delivery_config();
        config.invite_url = "http://discord.gg/example".to_owned();
        let out = deliver_go_live(&ledger, &sender, &config, &stream()).await;
        assert_eq!(
            out.outcome,
            GoLiveOutcome::Failed {
                reason: "invalid_invite"
            }
        );
        assert_eq!(out.disposition, disposition(Stage::InvalidInvite));
        assert_eq!(ledger.row(&live_dedup_key(&stream())), None);
        assert!(sender.calls().is_empty());
    }

    #[tokio::test]
    async fn deliver_proven_no_send_releases_started_claim_for_the_retry() {
        let ledger = FakeLedger::default();
        let sender = FakeSender::scripted(vec![
            Err(LiveSendError::ProvenNoSend),
            Ok("msg-2".to_owned()),
        ]);
        let config = delivery_config();
        let first = deliver_go_live(&ledger, &sender, &config, &stream()).await;
        assert_eq!(
            first.outcome,
            GoLiveOutcome::Failed {
                reason: "send_failed"
            }
        );
        assert_eq!(first.disposition, disposition(Stage::SendFailed));
        assert_eq!(ledger.releases(), 1);
        assert_eq!(ledger.row(&live_dedup_key(&stream())), None);

        let retry = deliver_go_live(&ledger, &sender, &config, &stream()).await;
        assert_eq!(
            retry.outcome,
            GoLiveOutcome::Posted {
                message_id: "msg-2".to_owned()
            }
        );
        assert_eq!(sender.calls().len(), 2);
    }

    // The control for the two lost-go-live tests below is
    // `deliver_proven_no_send_releases_started_claim_for_the_retry`
    // above: a successful release stays exactly `send_failed`, frees
    // the row, and the retry may claim and post.

    #[tokio::test]
    async fn deliver_proven_no_send_with_release_error_reports_lost_and_retry_dedupes() {
        let capture = WarnCapture::default();
        let _guard = capture.install();
        let ledger = FakeLedger {
            fail_release_err: true,
            ..FakeLedger::default()
        };
        let sender = FakeSender::scripted(vec![Err(LiveSendError::ProvenNoSend)]);
        let config = delivery_config();
        let first = deliver_go_live(&ledger, &sender, &config, &stream()).await;
        assert_eq!(
            first.outcome,
            GoLiveOutcome::Failed {
                reason: "send_failed_unreleased"
            }
        );
        assert_eq!(first.disposition, disposition(Stage::SendFailed));
        assert_eq!(ledger.releases(), 0);
        assert_eq!(
            ledger.row(&live_dedup_key(&stream())),
            Some(RowState::Started)
        );
        assert_unreleased_warn(&capture);

        let retry = deliver_go_live(&ledger, &sender, &config, &stream()).await;
        assert_eq!(retry.outcome, GoLiveOutcome::Duplicate);
        assert_eq!(retry.disposition.status, 200);
        assert_eq!(sender.calls().len(), 1, "a lost go-live must not re-post");
    }

    #[tokio::test]
    async fn deliver_proven_no_send_with_release_noop_reports_lost_and_retry_dedupes() {
        let capture = WarnCapture::default();
        let _guard = capture.install();
        let ledger = FakeLedger {
            fail_release_noop: true,
            ..FakeLedger::default()
        };
        let sender = FakeSender::scripted(vec![Err(LiveSendError::ProvenNoSend)]);
        let config = delivery_config();
        let first = deliver_go_live(&ledger, &sender, &config, &stream()).await;
        assert_eq!(
            first.outcome,
            GoLiveOutcome::Failed {
                reason: "send_failed_unreleased"
            }
        );
        assert_eq!(first.disposition, disposition(Stage::SendFailed));
        assert_eq!(ledger.releases(), 0);
        assert_eq!(
            ledger.row(&live_dedup_key(&stream())),
            Some(RowState::Started)
        );
        assert_unreleased_warn(&capture);

        let retry = deliver_go_live(&ledger, &sender, &config, &stream()).await;
        assert_eq!(retry.outcome, GoLiveOutcome::Duplicate);
        assert_eq!(retry.disposition.status, 200);
        assert_eq!(sender.calls().len(), 1, "a lost go-live must not re-post");
    }

    #[tokio::test]
    async fn deliver_ambiguous_send_stays_fenced_and_retry_dedupes() {
        let ledger = FakeLedger::default();
        let sender = FakeSender::scripted(vec![Err(LiveSendError::Ambiguous)]);
        let config = delivery_config();
        let first = deliver_go_live(&ledger, &sender, &config, &stream()).await;
        assert_eq!(
            first.outcome,
            GoLiveOutcome::Failed {
                reason: "ambiguous_send"
            }
        );
        assert_eq!(first.disposition.status, 500);
        assert!(!first.disposition.release, "ambiguous sends never release");
        assert_eq!(ledger.releases(), 0);
        assert_eq!(
            ledger.row(&live_dedup_key(&stream())),
            Some(RowState::Started)
        );

        let retry = deliver_go_live(&ledger, &sender, &config, &stream()).await;
        assert_eq!(retry.outcome, GoLiveOutcome::Duplicate);
        assert_eq!(sender.calls().len(), 1, "the retry must not re-post");
    }

    #[tokio::test]
    async fn deliver_stamp_failure_keeps_the_row_fenced() {
        let ledger = FakeLedger {
            fail_stamp: true,
            ..FakeLedger::default()
        };
        let sender = FakeSender::scripted(vec![Ok("msg-3".to_owned())]);
        let config = delivery_config();
        let first = deliver_go_live(&ledger, &sender, &config, &stream()).await;
        assert_eq!(
            first.outcome,
            GoLiveOutcome::Failed {
                reason: "stamp_failed"
            }
        );
        assert_eq!(first.disposition, disposition(Stage::StampFailed));
        assert_eq!(ledger.releases(), 0);
        let retry = deliver_go_live(&ledger, &sender, &config, &stream()).await;
        assert_eq!(retry.outcome, GoLiveOutcome::Duplicate);
        assert_eq!(sender.calls().len(), 1);
    }

    #[tokio::test]
    async fn deliver_claim_failure_and_in_flight_answer_5xx_without_sending() {
        let config = delivery_config();
        let failing = FakeLedger {
            fail_claim: true,
            ..FakeLedger::default()
        };
        let sender = FakeSender::default();
        let out = deliver_go_live(&failing, &sender, &config, &stream()).await;
        assert_eq!(out.disposition, disposition(Stage::ClaimFailed));
        assert!(sender.calls().is_empty());

        // A fresh never-started claim held by someone else: InFlight, no send.
        let ledger = FakeLedger::default();
        ledger
            .rows
            .lock()
            .expect("rows")
            .insert(live_dedup_key(&stream()), RowState::Claimed);
        let out = deliver_go_live(&ledger, &sender, &config, &stream()).await;
        assert_eq!(
            out.outcome,
            GoLiveOutcome::Failed {
                reason: "in_flight"
            }
        );
        assert_eq!(out.disposition, disposition(Stage::InFlight));
        assert!(sender.calls().is_empty());
    }
}
