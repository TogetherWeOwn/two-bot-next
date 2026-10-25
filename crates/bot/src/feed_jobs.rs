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
    activation::LiveCapability,
    feeds::{
        delivery_action, parse_xml_feed_report, poll_candidates, DeliveryAction, DeliveryClaim,
        FeedItem, FeedPollSchedule, FeedPost, FeedRelay, MAX_FEED_POSTS_PER_POLL,
    },
    feeds_connector::{fetch_feed, record_feed_fetch},
    feeds_store::{self as store, FeedAudit},
    funnel::now_millis_for_test,
    metrics, FeatureGates,
};
use two_bot_discord::{ActionExecutor, DiscordError};

use crate::{
    activation::BootActivation,
    command_runtime::new_id,
    jobs::{ErrorClass, Job, JobAction},
    website_jobs::Context,
};

pub(crate) const NAME: &str = "feeds";
const RECOVERY_LIMIT: u32 = 20;
const HISTORY_LIMIT: u8 = 100;
const RELAY_TIMEOUT: Duration = Duration::from_secs(30);
const RECOVERY_BUDGET: Duration = Duration::from_secs(12);
// Reserve the executor's five-second read deadline plus a scheduling margin.
// Do not routinely cancel reads mid-wire and hold send admission while fetching.
const RECOVERY_READ_RESERVE: Duration = Duration::from_secs(6);
const PASS_BUDGET: Duration = Duration::from_secs(100);
const JOB_TIMEOUT: Duration = Duration::from_secs(120);

/// The registered job retains this cursor across passes, including cancellation.
/// Match the store's stable `(created_at, id)` order even if the last row is removed.
struct FeedPoller {
    cursor: Mutex<Option<(i64, String)>>,
    relay_timeout: Duration,
    pass_budget: Duration,
}

impl Default for FeedPoller {
    fn default() -> Self {
        Self {
            cursor: Mutex::new(None),
            relay_timeout: RELAY_TIMEOUT,
            pass_budget: PASS_BUDGET,
        }
    }
}

impl FeedPoller {
    fn start_index(&self, feeds: &[FeedRelay]) -> usize {
        let cursor = self.cursor.lock().expect("feed cursor");
        cursor.as_ref().map_or(0, |(created_at, id)| {
            feeds
                .iter()
                .position(|feed| (feed.created_at, &feed.id) > (*created_at, id))
                .unwrap_or(0)
        })
    }

    fn advance(&self, feed: &FeedRelay) {
        *self.cursor.lock().expect("feed cursor") = Some((feed.created_at, feed.id.clone()));
    }

    async fn run_once(
        &self,
        pool: &PgPool,
        rest: &ActionExecutor,
        guild: &str,
        fetch: &dyn FeedFetch,
    ) -> Result<(), ErrorClass> {
        let deadline = tokio::time::Instant::now() + self.pass_budget;
        let mut feeds = tokio::time::timeout_at(deadline, store::list_feeds(pool, guild, true))
            .await
            .map_err(|_| ErrorClass::Timeout)?
            .map_err(|_| ErrorClass::Database)?;
        let start = self.start_index(&feeds);
        feeds.rotate_left(start);
        let mut error = None;
        for feed in feeds {
            let now = tokio::time::Instant::now();
            let relay_deadline = now + self.relay_timeout;
            // Reserve a full slot, rather than repeatedly giving the same
            // boundary relay a residual budget too short to make progress.
            if relay_deadline > deadline {
                return Err(ErrorClass::Timeout);
            }
            // Advance BEFORE polling: an aborted or timed-out relay must not
            // put the next pass back at the head of the same slow prefix.
            self.advance(&feed);
            // No spawned work: expiration drops the entire inline relay before
            // another starts. Claims survive cancellation for nonce recovery.
            // https://docs.rs/tokio/1.53.1/tokio/time/fn.timeout_at.html#cancellation
            match tokio::time::timeout_at(relay_deadline, poll_relay(pool, rest, &feed, fetch))
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(class)) => error = Some(class),
                Err(_) => {
                    error = Some(ErrorClass::Timeout);
                    // Do not add unbounded DB cleanup/audit after the deadline.
                    tracing::warn!(job = NAME, feed_id = %feed.id, "feed poll deadline exceeded");
                }
            }
        }
        error.map_or(Ok(()), Err)
    }
}

type FetchFuture = Pin<Box<dyn Future<Output = Result<Vec<FeedItem>, ErrorClass>> + Send>>;
pub(crate) trait FeedFetch: Send + Sync {
    fn fetch(&self, feed: FeedRelay) -> FetchFuture;
}

struct PublicFeedFetch;
impl FeedFetch for PublicFeedFetch {
    fn fetch(&self, feed: FeedRelay) -> FetchFuture {
        Box::pin(async move {
            let fetched = fetch_public_feed(metrics::global(), &feed.source).await?;
            parse_fetched(fetched, &feed)
        })
    }
}

/// Production fetch with metric recording, split from [`PublicFeedFetch`] so
/// hermetic tests can pass a runtime-local registry: exactly one outcome
/// sample plus (on failure) one error-class sample per call, never per-feed,
/// so no source URL reaches a label.
async fn fetch_public_feed(
    metrics: &metrics::Metrics,
    source: &str,
) -> Result<two_bot_core::feeds_connector::FetchedFeed, ErrorClass> {
    let result = fetch_feed(source).await;
    record_feed_fetch(metrics, &result);
    result.map_err(|_| ErrorClass::Feed)
}

fn parse_fetched(
    fetched: two_bot_core::feeds_connector::FetchedFeed,
    feed: &FeedRelay,
) -> Result<Vec<FeedItem>, ErrorClass> {
    if !fetched.status.is_success() {
        return Err(ErrorClass::Feed);
    }
    let parsed = parse_xml_feed_report(&fetched.body, feed.kind).map_err(|_| ErrorClass::Feed)?;
    if parsed.unsafe_urls_filtered > 0 {
        // Not an audit row: the entries stay in the feed, so a row per pass
        // would repeat. One line per pass lets an operator find a silent relay.
        tracing::warn!(job = NAME, feed_id = %feed.id, filtered = parsed.unsafe_urls_filtered,
            "feed items skipped: item URL has mass-mention text or masked-link syntax");
    }
    Ok(parsed.items)
}

/// The existing supervisor owns spawning, timeout, cancellation and join. A
/// schedule guard also finishes on dropped/aborted futures, not just success.
struct ScheduleRun(Arc<Mutex<FeedPollSchedule>>);
impl Drop for ScheduleRun {
    fn drop(&mut self) {
        two_bot_core::metrics::recover_drop_poison(self.0.lock(), "schedule_run").finish();
    }
}

/// Fixed-cadence constructor, kept for unit tests; production registers the
/// live-aware [`scheduled_job_live`] through [`register_fenced`].
#[cfg(test)]
pub(crate) fn scheduled_job(seconds: u64, action: JobAction) -> Result<Job, ErrorClass> {
    let schedule = Arc::new(Mutex::new(
        FeedPollSchedule::new(seconds).map_err(|_| ErrorClass::Configuration)?,
    ));
    let origin = tokio::time::Instant::now();
    Ok(Job {
        name: NAME,
        cadence: Duration::from_secs(seconds),
        cadence_source: None,
        startup_jitter: Duration::ZERO,
        timeout: JOB_TIMEOUT,
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

/// The feed interval key served from the live settings snapshot (TOG-19027).
pub(crate) const INTERVAL_KEY: &str = "TWO_FEED_POLL_SECONDS";

/// Effective poll interval: the live snapshot wins over the boot value, an
/// absent/unparsable/out-of-range live value falls back (store-first, like the
/// raid/join-risk/containment runtimes). Pure for tests via `live`.
pub(crate) fn live_poll_seconds_with(
    guild: &str,
    fallback: u64,
    live: Option<&two_bot_core::settings::LiveSettings>,
) -> u64 {
    let Some(live) = live else {
        return fallback;
    };
    let Some(value) = live.get(guild, INTERVAL_KEY) else {
        return fallback;
    };
    let raw = match &value {
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        _ => return fallback,
    };
    raw.parse::<u64>()
        .ok()
        .filter(|n| (60..=86400).contains(n))
        .unwrap_or(fallback)
}

/// Production reader: the poller-published snapshot, if any.
pub(crate) fn live_poll_seconds(guild: &str, fallback: u64) -> u64 {
    live_poll_seconds_with(guild, fallback, crate::settings_jobs::live().as_ref())
}

/// The announcements gate key served from the live settings snapshot. Job
/// existence stays boot-gated (registration and the live-identity fence read
/// the boot value); each tick re-checks the live value, so storing `0` parks
/// a boot-enabled poller within one poll while enabling from a boot-off
/// deployment needs a restart (no job exists to tick).
pub(crate) const ANNOUNCEMENTS_KEY: &str = "TWO_ANNOUNCEMENTS";

/// Effective announcements gate: the live snapshot wins over the boot value,
/// an absent row falls back to boot, and any rendering other than exactly
/// `"1"` reads off (fail-closed, like `FeatureGates::from_map`). Pure for
/// tests via `live`.
pub(crate) fn live_announcements_with(
    guild: &str,
    boot_on: bool,
    live: Option<&two_bot_core::settings::LiveSettings>,
) -> bool {
    let Some(live) = live else {
        return boot_on;
    };
    let Some(value) = live.get(guild, ANNOUNCEMENTS_KEY) else {
        return boot_on;
    };
    two_bot_core::settings::to_env_string(&value).is_some_and(|rendered| rendered == "1")
}

/// Production reader: the poller-published snapshot, if any.
pub(crate) fn live_announcements_on(guild: &str, boot_on: bool) -> bool {
    live_announcements_with(guild, boot_on, crate::settings_jobs::live().as_ref())
}

/// Live-aware job: the supervisor's own cadence follows the live interval,
/// and the non-overlapping schedule gate re-reads the same interval before
/// every tick. A stored change reschedules the next run to last-run + new
/// interval on both layers (see `jobs::Job::cadence_source` and
/// `FeedPollSchedule::set_interval_seconds`), so a decrease below the boot
/// cadence no longer waits for the pending boot-cadence slot and the
/// effective interval is the stored value, never rounded to a boot multiple.
pub(crate) fn scheduled_job_live(
    seconds: u64,
    action: JobAction,
    guild: String,
) -> Result<Job, ErrorClass> {
    let interval: Arc<dyn Fn() -> u64 + Send + Sync> =
        Arc::new(move || live_poll_seconds(&guild, seconds));
    scheduled_job_dynamic(seconds, action, interval)
}

/// [`scheduled_job_live`] over an injectable interval reader, for tests.
/// `seconds` is the boot value: it validates the job and seeds the cadence
/// until the reader's first value is taken at construction.
pub(crate) fn scheduled_job_dynamic(
    seconds: u64,
    action: JobAction,
    interval: Arc<dyn Fn() -> u64 + Send + Sync>,
) -> Result<Job, ErrorClass> {
    // Validate the boot value even when a live value is already published:
    // a misconfigured boot interval must refuse registration like before.
    FeedPollSchedule::new(seconds).map_err(|_| ErrorClass::Configuration)?;
    let initial = interval();
    let schedule = Arc::new(Mutex::new(
        FeedPollSchedule::new(initial).map_err(|_| ErrorClass::Configuration)?,
    ));
    let origin = tokio::time::Instant::now();
    let gate_interval = interval.clone();
    Ok(Job {
        name: NAME,
        cadence: Duration::from_secs(initial),
        cadence_source: Some(Arc::new(move || Duration::from_secs(interval()))),
        startup_jitter: Duration::ZERO,
        timeout: JOB_TIMEOUT,
        action: Arc::new(move || {
            let action = action.clone();
            let schedule = schedule.clone();
            let interval = gate_interval.clone();
            let now = origin.elapsed().as_millis().min(i64::MAX as u128) as i64;
            Box::pin(async move {
                {
                    let mut schedule = schedule.lock().expect("feed schedule");
                    schedule.set_interval_seconds(interval());
                    if !schedule.begin(now) {
                        return Ok(());
                    }
                }
                let _run = ScheduleRun(schedule);
                action().await
            })
        }),
    })
}

/// The feed poller, or `None` while `TWO_ANNOUNCEMENTS` is off or the boot
/// identity does not permit announcements (the live-identity fence, TOG-15758).
/// The poll interval reads the live settings snapshot (TOG-19027): the
/// supervisor cadence and the per-tick schedule gate both follow the stored
/// value, so a changed interval reschedules the next run without a restart.
/// Each tick also re-checks the announcements gate against the live snapshot,
/// so storing `0` parks a boot-enabled poller within one poll.
pub(crate) fn register(context: Arc<Context>, activation: &BootActivation) -> Option<Job> {
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
    let poller = Arc::new(FeedPoller::default());
    let guild = context.guild.clone();
    let action: JobAction = Arc::new(move || {
        let context = context.clone();
        let poller = poller.clone();
        Box::pin(async move {
            poller
                .run_once(
                    context.pool().await?,
                    &context.rest,
                    &context.guild,
                    &PublicFeedFetch,
                )
                .await
        })
    });
    register_fenced(gates, activation, action, guild)
}

/// Identity can only narrow the env gate: the poller posts under the token's
/// identity, exactly like the announcement verbs the router refuses there.
pub(crate) fn register_fenced(
    gates: FeatureGates,
    activation: &BootActivation,
    action: JobAction,
    guild: String,
) -> Option<Job> {
    let fenced = activation.constrain_features(gates);
    if gates.announcements && !fenced.announcements {
        tracing::warn!(
            job = NAME,
            capability = LiveCapability::Announcements.as_str(),
            "feed poller parked: live activation refused"
        );
    }
    register_gated(fenced, action, guild)
}

fn register_gated(gates: FeatureGates, action: JobAction, guild: String) -> Option<Job> {
    if !gates.announcements {
        return None;
    }
    let gate_guild = guild.clone();
    register_gated_with(
        gates.feed_poll_seconds,
        action,
        guild,
        Arc::new(move || live_announcements_on(&gate_guild, true)),
    )
}

/// [`register_gated`] over an injectable per-tick gate, for tests. The
/// production gate reads the live announcements snapshot store-first over the
/// boot value; a closed gate skips the tick without error (like the schedule
/// gate below it), silently so a parked poller does not log every interval.
fn register_gated_with(
    feed_poll_seconds: u64,
    action: JobAction,
    guild: String,
    gate: Arc<dyn Fn() -> bool + Send + Sync>,
) -> Option<Job> {
    let gated: JobAction = Arc::new(move || {
        let inner = action.clone();
        let gate = gate.clone();
        Box::pin(async move {
            if !gate() {
                return Ok(());
            }
            inner().await
        })
    });
    scheduled_job_live(feed_poll_seconds, gated, guild).ok()
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

async fn poll_relay(
    pool: &PgPool,
    rest: &ActionExecutor,
    feed: &FeedRelay,
    fetch: &dyn FeedFetch,
) -> Result<(), ErrorClass> {
    // A paused or removed relay stays silent even when this pass polled a
    // stale enabled snapshot: re-read live lifecycle state before any fetch,
    // recovery, high-water or audit write. Claim admission revalidates too,
    // so a fetch admitted before a racing pause still cannot post after it.
    let live: Option<bool> =
        sqlx::query_scalar("SELECT enabled FROM feed_relays WHERE guild_id = $1 AND id = $2")
            .bind(&feed.guild_id)
            .bind(&feed.id)
            .fetch_optional(pool)
            .await
            .map_err(|_| ErrorClass::Database)?;
    if live != Some(true) {
        return Ok(());
    }
    let mut error = None;
    let mut stats = PollStats::default();
    let mut recovery = RecoveryBudget {
        deadline: tokio::time::Instant::now() + RECOVERY_BUDGET,
        reads: 0,
    };
    // Leave at least eighteen seconds for fresh polling even when unresolved
    // history or recovery DB work is slow. Cancellation retains all claims.
    if tokio::time::timeout_at(
        recovery.deadline,
        recover_pending(pool, rest, feed, &mut stats, &mut recovery, &mut error),
    )
    .await
    .is_err()
    {
        error = Some(ErrorClass::Timeout);
        tracing::warn!(job = NAME, feed_id = %feed.id, "feed recovery deadline exceeded");
    }
    match fetch.fetch(feed.clone()).await {
        Ok(items) => {
            for candidate in poll_candidates(feed, &items) {
                match candidate {
                    Ok(post) => {
                        deliver(
                            pool,
                            rest,
                            feed,
                            &post,
                            &mut stats,
                            &mut recovery,
                            &mut error,
                        )
                        .await
                    }
                    Err(_) => {
                        stats.failed += 1;
                        error = Some(ErrorClass::Feed);
                        audit(pool, feed, "item_failed", "invalid_item", &mut error).await;
                    }
                }
            }
        }
        Err(class) => {
            stats.failed += 1;
            error = Some(class);
            audit(pool, feed, "poll_failed", "fetch_or_parse", &mut error).await;
        }
    }
    if store::mark_checked(pool, &feed.guild_id, &feed.id, now_millis_for_test())
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
    audit(pool, feed, "poll_result", &summary, &mut error).await;
    tracing::info!(job = NAME, feed_id = %feed.id, attempted = stats.attempted,
        delivered = stats.delivered, reconciled = stats.reconciled, pending = stats.pending,
        deferred = stats.deferred, failed = stats.failed, "feed poll result");
    error.map_or(Ok(()), Err)
}

struct RecoveryBudget {
    deadline: tokio::time::Instant,
    reads: u32,
}

fn recovery_read_fits(deadline: tokio::time::Instant) -> bool {
    tokio::time::Instant::now() + RECOVERY_READ_RESERVE <= deadline
}

async fn recover_pending(
    pool: &PgPool,
    rest: &ActionExecutor,
    feed: &FeedRelay,
    stats: &mut PollStats,
    recovery: &mut RecoveryBudget,
    error: &mut Option<ErrorClass>,
) {
    // This path runs even if HTTP fails or the item has aged out of XML.
    match store::pending_deliveries(pool, feed, now_millis_for_test(), RECOVERY_LIMIT).await {
        Ok(pending) => {
            for post in pending {
                if !recovery_read_fits(recovery.deadline) {
                    *error = Some(ErrorClass::Timeout);
                    break;
                }
                deliver(pool, rest, feed, &post, stats, recovery, error).await;
            }
        }
        Err(_) => *error = Some(ErrorClass::Database),
    }
}

async fn deliver(
    pool: &PgPool,
    rest: &ActionExecutor,
    feed: &FeedRelay,
    post: &FeedPost,
    stats: &mut PollStats,
    recovery: &mut RecoveryBudget,
    error: &mut Option<ErrorClass>,
) {
    // Observe recovery before renewing its lease. A budget-refused identity-only
    // read must not rotate an unsearched row forever behind its neighbours.
    // Reads are non-mutating; the later claim still arbitrates completion.
    let reconciled = if post.content.is_empty() {
        let reads = recovery.reads;
        let result = reconcile(rest, post, recovery).await;
        if recovery.reads == reads {
            let budget_refused = result == Err(ErrorClass::Timeout);
            let reason = if budget_refused {
                "history_budget"
            } else {
                "history_unconfirmed"
            };
            recovery_required(pool, feed, stats, reason, error).await;
            if budget_refused && *error == Some(ErrorClass::RecoveryRequired) {
                *error = Some(ErrorClass::Timeout);
            }
            return;
        }
        Some(result)
    } else {
        None
    };
    let token = new_id();
    // Empty content denotes a ledger-only recovery placeholder, never an XML
    // send. Only the bounded pending queue may take over an expired claim.
    let acquired = if post.content.is_empty() {
        store::claim_delivery(pool, &feed.guild_id, post, &token, now_millis_for_test()).await
    } else {
        store::claim_fresh_delivery(pool, &feed.guild_id, post, &token, now_millis_for_test()).await
    };
    let claim = match acquired {
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
        DeliveryAction::ReconcileByNonce => match reconciled {
            Some(Ok(id)) => id,
            _ => {
                recovery_required(pool, feed, stats, "history_unconfirmed", error).await;
                return;
            }
        },
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
                // Only a pre-wire guard refusal proves nothing reached the
                // transport. A wire rejection (even a definitive-looking
                // 403) stays pending: the stable per-item nonce has only a
                // short dedupe window, so a later repost is not safe.
                Err(DiscordError::Guard(_)) => {
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
async fn reconcile(
    rest: &ActionExecutor,
    post: &FeedPost,
    recovery: &mut RecoveryBudget,
) -> Result<String, ErrorClass> {
    if recovery.reads >= RECOVERY_LIMIT || !recovery_read_fits(recovery.deadline) {
        return Err(ErrorClass::Timeout);
    }
    let me = rest
        .get_json_strict("/users/@me")
        .await
        .map_err(|_| ErrorClass::Rest)?
        .ok_or(ErrorClass::Rest)?;
    let bot = me["id"]
        .as_str()
        .filter(|id| valid_id(id))
        .ok_or(ErrorClass::Rest)?;
    if !recovery_read_fits(recovery.deadline) {
        return Err(ErrorClass::Timeout);
    }
    recovery.reads += 1;
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

#[cfg(test)]
#[path = "feed_jobs_poison_acceptance.rs"]
mod schedule_poison_acceptance;
