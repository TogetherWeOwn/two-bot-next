//! Managed feed polling over the pinned connector, fenced ledger and shared REST
//! executor. Unknown acceptance is recovery-required, never a blind retry.

use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

use sqlx::PgPool;
use two_bot_core::{
    feeds::{
        delivery_action, parse_xml_feed, poll_candidates, DeliveryAction, DeliveryClaim, FeedItem,
        FeedPollSchedule, FeedPost, FeedRelay, MAX_FEED_POSTS_PER_POLL,
    },
    feeds_connector::fetch_feed,
    feeds_store::{self as store, FeedAudit},
    funnel::now_millis_for_test,
    FeatureGates,
};
use two_bot_discord::ActionExecutor;

use crate::{
    command_runtime::new_id,
    jobs::{ErrorClass, Job, JobAction},
    website_jobs::Context,
};

pub(crate) const NAME: &str = "feeds";
const RECOVERY_LIMIT: u32 = 20;
const HISTORY_LIMIT: u8 = 100;

type FetchFuture = Pin<Box<dyn Future<Output = Result<Vec<FeedItem>, ErrorClass>> + Send>>;
pub(crate) trait FeedFetch: Send + Sync {
    fn fetch(&self, feed: FeedRelay) -> FetchFuture;
}

struct PublicFeedFetch;
impl FeedFetch for PublicFeedFetch {
    fn fetch(&self, feed: FeedRelay) -> FetchFuture {
        Box::pin(async move {
            let fetched = fetch_feed(&feed.source)
                .await
                .map_err(|_| ErrorClass::Feed)?;
            parse_fetched(fetched)
        })
    }
}

fn parse_fetched(
    fetched: two_bot_core::feeds_connector::FetchedFeed,
) -> Result<Vec<FeedItem>, ErrorClass> {
    if !fetched.status.is_success() {
        return Err(ErrorClass::Feed);
    }
    parse_xml_feed(&fetched.body).map_err(|_| ErrorClass::Feed)
}

/// The existing supervisor owns spawning, timeout, cancellation and join. A
/// schedule guard also finishes on dropped/aborted futures, not just success.
struct ScheduleRun(Arc<Mutex<FeedPollSchedule>>);
impl Drop for ScheduleRun {
    fn drop(&mut self) {
        self.0.lock().expect("feed schedule").finish();
    }
}

pub(crate) fn scheduled_job(seconds: u64, action: JobAction) -> Result<Job, ErrorClass> {
    let schedule = Arc::new(Mutex::new(
        FeedPollSchedule::new(seconds).map_err(|_| ErrorClass::Configuration)?,
    ));
    let origin = tokio::time::Instant::now();
    Ok(Job {
        name: NAME,
        cadence: Duration::from_secs(seconds),
        startup_jitter: Duration::ZERO,
        timeout: Duration::from_secs(120),
        action: Arc::new(move || {
            let action = action.clone();
            let schedule = schedule.clone();
            let now = origin.elapsed().as_millis().min(i64::MAX as u128) as i64;
            // Acquire when polled, not at factory creation: an unpolled future
            // cannot strand the schedule in its running state.
            Box::pin(async move {
                if !schedule.lock().expect("feed schedule").begin(now) {
                    return Ok(());
                }
                let _run = ScheduleRun(schedule);
                action().await
            })
        }),
    })
}

pub(crate) fn register(context: Arc<Context>) -> Option<Job> {
    let gates = match FeatureGates::from_env() {
        Ok(gates) => gates,
        Err(_) => {
            tracing::warn!(
                job = NAME,
                "feed poller parked: invalid feature configuration"
            );
            return None;
        }
    };
    if !gates.announcements {
        return None;
    }
    scheduled_job(
        gates.feed_poll_seconds,
        Arc::new(move || {
            let context = context.clone();
            Box::pin(async move {
                run_once(
                    context.pool().await?,
                    &context.rest,
                    &context.guild,
                    &PublicFeedFetch,
                )
                .await
            })
        }),
    )
    .ok()
}

#[derive(Default)]
struct PollStats {
    attempted: usize,
    delivered: usize,
    reconciled: usize,
    pending: usize,
    deferred: usize,
    failed: usize,
}

pub(crate) async fn run_once(
    pool: &PgPool,
    rest: &ActionExecutor,
    guild: &str,
    fetch: &dyn FeedFetch,
) -> Result<(), ErrorClass> {
    let feeds = store::list_feeds(pool, guild, true)
        .await
        .map_err(|_| ErrorClass::Database)?;
    let mut error = None;
    for feed in feeds {
        let mut stats = PollStats::default();
        let mut recovery_reads = 0;
        // This path runs even if HTTP fails or the item has aged out of XML.
        match store::pending_deliveries(pool, &feed, now_millis_for_test(), RECOVERY_LIMIT).await {
            Ok(pending) => {
                for post in pending {
                    deliver(
                        pool,
                        rest,
                        &feed,
                        &post,
                        &mut stats,
                        &mut recovery_reads,
                        &mut error,
                    )
                    .await;
                }
            }
            Err(_) => error = Some(ErrorClass::Database),
        }
        match fetch.fetch(feed.clone()).await {
            Ok(items) => {
                for candidate in poll_candidates(&feed, &items) {
                    match candidate {
                        Ok(post) => {
                            deliver(
                                pool,
                                rest,
                                &feed,
                                &post,
                                &mut stats,
                                &mut recovery_reads,
                                &mut error,
                            )
                            .await
                        }
                        Err(_) => {
                            stats.failed += 1;
                            error = Some(ErrorClass::Feed);
                            audit(pool, &feed, "item_failed", "invalid_item", &mut error).await;
                        }
                    }
                }
            }
            Err(class) => {
                stats.failed += 1;
                error = Some(class);
                audit(pool, &feed, "poll_failed", "fetch_or_parse", &mut error).await;
            }
        }
        if store::mark_checked(pool, guild, &feed.id, now_millis_for_test())
            .await
            .is_err()
        {
            error = Some(ErrorClass::Database);
        }
        let summary = format!(
            "attempted={} delivered={} reconciled={} pending={} deferred={} failed={}",
            stats.attempted,
            stats.delivered,
            stats.reconciled,
            stats.pending,
            stats.deferred,
            stats.failed
        );
        audit(pool, &feed, "poll_result", &summary, &mut error).await;
        tracing::info!(job = NAME, feed_id = %feed.id, attempted = stats.attempted,
            delivered = stats.delivered, reconciled = stats.reconciled, pending = stats.pending,
            deferred = stats.deferred, failed = stats.failed, "feed poll result");
    }
    error.map_or(Ok(()), Err)
}

async fn deliver(
    pool: &PgPool,
    rest: &ActionExecutor,
    feed: &FeedRelay,
    post: &FeedPost,
    stats: &mut PollStats,
    recovery_reads: &mut u32,
    error: &mut Option<ErrorClass>,
) {
    let token = new_id();
    let claim = match store::claim_delivery(
        pool,
        &feed.guild_id,
        post,
        &token,
        now_millis_for_test(),
    )
    .await
    {
        Ok(Some(claim)) => claim,
        Ok(None) => return,
        Err(_) => {
            stats.failed += 1;
            *error = Some(ErrorClass::Database);
            return;
        }
    };
    if claim == DeliveryClaim::Fresh && post.content.is_empty() {
        // A queued recovery row was removed concurrently. Its placeholder must
        // never become a fresh POST; this acquisition is known never sent.
        if store::release_unposted(pool, &feed.guild_id, post, &token)
            .await
            .is_err()
        {
            *error = Some(ErrorClass::Database);
        }
        return;
    }
    let message = match delivery_action(claim, stats.attempted) {
        DeliveryAction::Defer => {
            stats.deferred += 1;
            if store::release_unposted(pool, &feed.guild_id, post, &token)
                .await
                .is_err()
            {
                *error = Some(ErrorClass::Database);
            }
            return;
        }
        DeliveryAction::ReconcileByNonce => {
            if *recovery_reads >= RECOVERY_LIMIT {
                recovery_required(pool, feed, stats, "history_budget", error).await;
                return;
            }
            *recovery_reads += 1;
            match reconcile(rest, post).await {
                Ok(id) => id,
                Err(_) => {
                    recovery_required(pool, feed, stats, "history_unconfirmed", error).await;
                    return;
                }
            }
        }
        DeliveryAction::Send => {
            // Count attempts, not just successes: failures cannot exceed the
            // legacy per-feed POST budget either.
            stats.attempted += 1;
            debug_assert!(stats.attempted <= MAX_FEED_POSTS_PER_POLL);
            match rest
                .post_message_with_nonce(&post.channel_id, &post.content, &post.nonce)
                .await
            {
                Ok(id) if valid_id(&id) => id,
                Err(err) if err.is_safe_pre_mutation() => {
                    stats.failed += 1;
                    *error = Some(ErrorClass::Rest);
                    if store::release_unposted(pool, &feed.guild_id, post, &token)
                        .await
                        .is_err()
                    {
                        *error = Some(ErrorClass::Database);
                    }
                    audit(pool, feed, "item_failed", "send_rejected", error).await;
                    return;
                }
                _ => {
                    recovery_required(pool, feed, stats, "send_unconfirmed", error).await;
                    return;
                }
            }
        }
    };
    match store::mark_delivered(
        pool,
        &feed.guild_id,
        post,
        &token,
        &message,
        now_millis_for_test(),
    )
    .await
    {
        Ok(true) => {
            if claim == DeliveryClaim::Recovered {
                stats.reconciled += 1;
            } else {
                stats.delivered += 1;
            }
        }
        // A successful POST followed by a DB error or lost fence is not a
        // failed send. Keep the ledger claim, never release or repost it.
        _ => recovery_required(pool, feed, stats, "completion_unconfirmed", error).await,
    }
}

fn valid_id(id: &str) -> bool {
    id.parse::<u64>().is_ok_and(|id| id != 0)
}

/// One bounded history page; a miss is uncertainty, not authoritative absence.
/// Confirm the exact string nonce, channel, authenticated author and message ID.
async fn reconcile(rest: &ActionExecutor, post: &FeedPost) -> Result<String, ErrorClass> {
    let me = rest
        .get_json("/users/@me")
        .await
        .map_err(|_| ErrorClass::Rest)?
        .ok_or(ErrorClass::Rest)?;
    let bot = me["id"]
        .as_str()
        .filter(|id| valid_id(id))
        .ok_or(ErrorClass::Rest)?;
    let rows = rest
        .fetch_channel_messages(&post.channel_id, None, HISTORY_LIMIT)
        .await
        .map_err(|_| ErrorClass::Rest)?;
    if rows.len() > usize::from(HISTORY_LIMIT) {
        return Err(ErrorClass::RecoveryRequired);
    }
    let mut found = None;
    for row in rows {
        if row["nonce"].as_str() != Some(post.nonce.as_str()) {
            continue;
        }
        let id = row["id"]
            .as_str()
            .filter(|id| valid_id(id))
            .ok_or(ErrorClass::RecoveryRequired)?;
        if row["channel_id"].as_str() != Some(post.channel_id.as_str())
            || row["author"]["id"].as_str() != Some(bot)
            || found.is_some()
        {
            return Err(ErrorClass::RecoveryRequired);
        }
        found = Some(id.to_owned());
    }
    found.ok_or(ErrorClass::RecoveryRequired)
}

async fn recovery_required(
    pool: &PgPool,
    feed: &FeedRelay,
    stats: &mut PollStats,
    reason: &str,
    error: &mut Option<ErrorClass>,
) {
    stats.pending += 1;
    *error = Some(ErrorClass::RecoveryRequired);
    audit(pool, feed, "recovery_required", reason, error).await;
}

async fn audit(
    pool: &PgPool,
    feed: &FeedRelay,
    outcome: &str,
    reason: &str,
    error: &mut Option<ErrorClass>,
) {
    if store::write_audit(
        pool,
        &FeedAudit {
            id: &new_id(),
            guild_id: &feed.guild_id,
            actor_id: None,
            action: "feed_poll",
            target_key: &feed.id,
            outcome,
            reason: Some(reason),
            now_ms: now_millis_for_test(),
        },
    )
    .await
    .is_err()
    {
        *error = Some(ErrorClass::Database);
    }
}

#[cfg(test)]
#[path = "feed_jobs_tests.rs"]
mod tests;
