//! Join-risk delivery runtime (decisions in `docs/raid-port.md`
//! and `docs/containment.md`).
//!
//! The gateway pipeline hands every non-bot join to the [`JoinObserver`] seam
//! after the funnel has recorded it. The always-on raid watch (R1) owns that
//! single slot, so this runtime chains behind it: [`chain_from_env`] returns
//! the raid watch alone unless the anti-nuke fences pass, else a fan-out that
//! feeds both runtimes. An observer problem can never cost the join row it
//! reports on (legacy `client.ts`: risk recording runs last).
//!
//! Fences, in order: the runtime is constructed only under exact
//! `TWO_ANTI_NUKE=1` on the staging guild; otherwise the chain is the raid
//! watch alone and no join-risk row is ever claimed. Inside the fences,
//! dry-run (`TWO_ANTI_NUKE_DRY_RUN` anything but explicit `0`) and session
//! onboarding mode do **not** suppress risk evidence or staff messages —
//! proposals are not authorizations, and legacy never gated them. They do
//! refuse arming: [`AntiNukeFences::armed`] is false unless dry-run is
//! explicitly `0` outside session mode, and R2 has no armed path at all, so
//! there is nothing to arm. R3 (containment) consumes `armed`.
//!
//! Per observation the worker builds the [`JoinRiskPolicy`] from live Hot
//! tuning, claims the event through [`JoinRiskStore::record`] (atomic
//! check-duplicate / count / score / insert under the guild advisory lock),
//! logs the evidence first, then delivers `staff_message(persisted)` through
//! the shared [`ActionExecutor`]: only to the live staff channel the bot
//! can View and Send in, with empty allowed mentions. Duplicates,
//! undeliverable channels and failed sends are never retried: a failed send
//! must not turn a replay into a second alert.
//!
//! Account age comes from the member snowflake (`(id >> 22) + epoch`), the
//! same derivation legacy `snowflakeToDate` used; the pipeline only observes
//! non-bot joins, so `member_is_bot` is always false here.

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::FutureExt as _;
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};
use two_bot_core::join_risk_store::{JoinRiskClaim, JoinRiskStore};
use two_bot_core::metrics::{self, JoinQueueDropReason};
use two_bot_core::onboarding::MentionPolicy;
use two_bot_core::raid::{
    JoinRiskInput, JoinRiskPolicy, DEFAULT_JOIN_RISK_THRESHOLD, DEFAULT_JOIN_RISK_WINDOW_SECONDS,
};
use two_bot_core::settings::SettingsCache;
use two_bot_core::RaidTuning;
use two_bot_cutover::parse::snowflake_to_date_ms;
use two_bot_cutover::settings::SettingsStore;
use two_bot_cutover::STAGING_GUILD_ID;
use two_bot_discord::{ActionExecutor, JoinObservation, JoinObserver};

use crate::raid_runtime::{Delivery, RaidDelivery, RaidRuntime, QUEUE_CAPACITY};

/// Re-checked at most this often (legacy 15 s poll).
const SETTINGS_MAX_AGE: Duration = Duration::from_secs(15);
const SETTINGS_READ_MAX: Duration = Duration::from_millis(1500);

const ENABLE_KEY: &str = "TWO_ANTI_NUKE";
const DRY_RUN_KEY: &str = "TWO_ANTI_NUKE_DRY_RUN";
const MODE_KEY: &str = "TWO_ONBOARDING_MODE";
const THRESHOLD_KEY: &str = "TWO_JOIN_RISK_THRESHOLD";
const WINDOW_KEY: &str = "TWO_JOIN_RISK_WINDOW_SECONDS";
const BULK_UNTIL_KEY: &str = "TWO_BULK_JOIN_WINDOW_UNTIL";
const CHANNEL_KEY: &str = "DISCORD_STAFF_ALERT_CHANNEL_ID";
const HOT_KEYS: [&str; 4] = [THRESHOLD_KEY, WINDOW_KEY, BULK_UNTIL_KEY, CHANNEL_KEY];

fn is_snowflake(value: &str) -> bool {
    (17..=20).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_digit())
}

/// The containment fences for one boot. Cold reads: evaluated once at
/// construction, never re-read per observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AntiNukeFences {
    /// Exact `TWO_ANTI_NUKE=1` on the staging guild. Without it the chain is
    /// the raid watch alone and no claim is ever recorded.
    pub enabled: bool,
    /// Dry-run explicitly `0` outside session mode. R2 has no armed path;
    /// R3 refuses every armed action while this is false.
    pub armed: bool,
}

impl AntiNukeFences {
    pub fn from_vars(vars: &HashMap<String, String>, guild_id: u64) -> Self {
        let staging = guild_id.to_string() == STAGING_GUILD_ID;
        let flag = vars.get(ENABLE_KEY).is_some_and(|value| value == "1");
        let session = vars
            .get(MODE_KEY)
            .is_some_and(|value| value.trim() == "session");
        let dry_run_off = vars.get(DRY_RUN_KEY).is_some_and(|value| value == "0");
        Self {
            enabled: flag && staging,
            armed: flag && staging && dry_run_off && !session,
        }
    }
}

/// The join-risk configuration for one observation.
#[derive(Debug, Clone, PartialEq)]
pub struct JoinRiskSettings {
    pub tuning: RaidTuning,
    pub bulk_join_window_until_ms: Option<i64>,
    /// `None` means log only: no channel is guessed on a live server.
    pub staff_channel: Option<String>,
}

/// A present-but-unusable tuning key that refuses scoring and arming.
///
/// The refusal names the key family ([`JoinRiskTuningRefusal::key`]), never
/// the value, so the operator-visible signal identifies the bad key without
/// echoing a possibly secret value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinRiskTuningRefusal {
    key: &'static str,
}

impl JoinRiskTuningRefusal {
    pub fn key(&self) -> &'static str {
        self.key
    }
}

impl std::fmt::Display for JoinRiskTuningRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "join-risk tuning refused: {} present but unusable; scoring disabled",
            self.key
        )
    }
}

impl std::error::Error for JoinRiskTuningRefusal {}

impl JoinRiskSettings {
    /// Fallible reader for the live settings path. A missing key keeps its
    /// shipped default (tuning 60 s / 5.0, no bulk window, log-only staff
    /// channel); so does a present-but-empty value. A present-but-unusable
    /// value (unparsable, non-finite, non-positive, or a rejected tuning
    /// pair) refuses the whole read: no default is substituted and no
    /// settings value is produced.
    pub fn try_from_vars(vars: &HashMap<String, String>) -> Result<Self, JoinRiskTuningRefusal> {
        let number = |key: &'static str, default: f64| -> Result<f64, JoinRiskTuningRefusal> {
            match vars.get(key).map(|raw| raw.trim()) {
                None | Some("") => Ok(default),
                Some(raw) => match raw.parse::<f64>() {
                    Ok(value) if value.is_finite() && value > 0.0 => Ok(value),
                    _ => Err(JoinRiskTuningRefusal { key }),
                },
            }
        };
        // Window is evaluated first, as in `RaidTuning::new`: when both
        // tuning numbers are unusable the window key is named.
        let window = number(WINDOW_KEY, DEFAULT_JOIN_RISK_WINDOW_SECONDS)?;
        let threshold = number(THRESHOLD_KEY, DEFAULT_JOIN_RISK_THRESHOLD)?;
        let tuning = RaidTuning::new(window, threshold)
            .map_err(|_| JoinRiskTuningRefusal { key: WINDOW_KEY })?;
        let bulk_join_window_until_ms = match vars.get(BULK_UNTIL_KEY).map(|raw| raw.trim()) {
            None | Some("") => None,
            Some(raw) => match raw.parse::<i64>() {
                Ok(value) if value > 0 => Some(value),
                _ => {
                    return Err(JoinRiskTuningRefusal {
                        key: BULK_UNTIL_KEY,
                    })
                }
            },
        };
        let staff_channel = match vars.get(CHANNEL_KEY).map(|raw| raw.trim()) {
            None | Some("") => None,
            Some(raw) if is_snowflake(raw) => Some(raw.to_owned()),
            Some(_) => return Err(JoinRiskTuningRefusal { key: CHANNEL_KEY }),
        };
        Ok(Self {
            tuning,
            bulk_join_window_until_ms,
            staff_channel,
        })
    }
}

/// Where the live [`JoinRiskSettings`] come from. A present-but-unusable
/// key refuses the read; the worker drops the observation with no scoring
/// side effects.
pub trait SettingsSource: Send + 'static {
    fn current(
        &mut self,
    ) -> impl Future<Output = Result<JoinRiskSettings, JoinRiskTuningRefusal>> + Send;
}

/// Tuning, the bulk window and the staff channel are store-first (hot rows win
/// over the deployment environment) and re-read at most every
/// `SETTINGS_MAX_AGE`; a failed refresh keeps the last outcome (good
/// settings or the cached refusal).
pub struct StoreSettings {
    pool: PgPool,
    guild_id: String,
    deployment: HashMap<String, String>,
    max_age: Duration,
    cache: Option<SettingsCache>,
    checked: Option<Instant>,
    last: Option<Result<JoinRiskSettings, JoinRiskTuningRefusal>>,
}

impl StoreSettings {
    pub fn new(pool: PgPool, guild_id: u64, deployment: HashMap<String, String>) -> Self {
        Self {
            pool,
            guild_id: guild_id.to_string(),
            deployment,
            max_age: SETTINGS_MAX_AGE,
            cache: None,
            checked: None,
            last: None,
        }
    }

    /// Test seam: force the next [`SettingsSource::current`] call to re-read
    /// instead of returning the cached outcome, so state transitions
    /// (good to refused, refused to steady refused, tuning changes) are
    /// coverable without waiting out [`SETTINGS_MAX_AGE`].
    #[cfg(test)]
    pub fn expire_for_tests(&mut self) {
        self.checked = None;
    }

    /// Test seam: replace the boot-time deployment values under test, so one
    /// [`StoreSettings`] can walk through deployment transitions.
    #[cfg(test)]
    pub fn set_deployment_for_tests(&mut self, deployment: HashMap<String, String>) {
        self.deployment = deployment;
    }

    async fn refresh(&mut self) {
        let store = SettingsStore::new(&self.pool);
        let stale = self.cache.as_ref();
        // Bound to a local so the timeout future is dropped before the borrows
        // above end (edition-2021 tail-expression temporaries).
        let loaded = tokio::time::timeout(SETTINGS_READ_MAX, async {
            let (revision, rows) = store.poll_guild_marks(&self.guild_id).await?;
            if stale.is_some_and(|cache| !cache.needs_refresh(revision, rows)) {
                return Ok::<_, sqlx::Error>(None);
            }
            let snapshot = store.load_guild_snapshot(&self.guild_id).await?;
            Ok(Some(SettingsCache::load(&snapshot)))
        })
        .await;
        match loaded {
            Ok(Ok(Some(cache))) => self.cache = Some(cache),
            Ok(Ok(None)) => {}
            Ok(Err(_)) | Err(_) => {
                warn!("join-risk settings refresh failed; keeping the last good values");
            }
        }
    }
}

impl SettingsSource for StoreSettings {
    async fn current(&mut self) -> Result<JoinRiskSettings, JoinRiskTuningRefusal> {
        // The cached outcome covers good settings and refusals alike, so a
        // misconfigured key cannot turn per-15 s polling into per-join load.
        if let (Some(at), Some(last)) = (self.checked, &self.last) {
            if at.elapsed() < self.max_age {
                return last.clone();
            }
        }
        self.checked = Some(Instant::now());
        self.refresh().await;
        let mut vars = self.deployment.clone();
        if let Some(cache) = &self.cache {
            // Tuning, the bulk window and the staff channel are live
            // (`HOT_WIRED`): a stored row moves the next observation, a
            // delete hands the key back to the boot-time deployment value.
            vars.extend(
                cache
                    .env_snapshot(Some(self.guild_id.as_str()))
                    .into_iter()
                    .filter(|(key, _)| HOT_KEYS.contains(&key.as_str())),
            );
        }
        let settings = match JoinRiskSettings::try_from_vars(&vars) {
            Ok(settings) => settings,
            Err(refusal) => {
                // Log the refusal at error level only on a state change
                // (good -> refused, or a different key); a steady refused
                // state stays cached and quiet until the max age elapses.
                let changed =
                    !matches!(&self.last, Some(Err(previous)) if previous.key() == refusal.key());
                if changed {
                    error!(
                        error_class = "join_risk_tuning_refused",
                        key = refusal.key(),
                        "join_risk_tuning_refused: present-but-unusable tuning; scoring disabled"
                    );
                } else {
                    debug!(
                        key = refusal.key(),
                        "join_risk_tuning_refused: still refused; scoring disabled"
                    );
                }
                // Fail closed: never fall back to stale good values or ship
                // defaults the operator did not ask for.
                self.last = Some(Err(refusal));
                return Err(refusal);
            }
        };
        match &self.last {
            Some(Ok(previous)) if previous.tuning != settings.tuning => info!(
                from_threshold = previous.tuning.threshold(),
                to_threshold = settings.tuning.threshold(),
                from_window_seconds = previous.tuning.window_seconds(),
                to_window_seconds = settings.tuning.window_seconds(),
                "setting_changed: join-risk tuning"
            ),
            Some(Ok(_)) => {}
            Some(Err(_)) | None => info!(
                threshold = settings.tuning.threshold(),
                window_seconds = settings.tuning.window_seconds(),
                alert_target = settings
                    .staff_channel
                    .as_deref()
                    .unwrap_or("log only (DISCORD_STAFF_ALERT_CHANNEL_ID unset)"),
                "join_risk_enabled"
            ),
        }
        self.last = Some(Ok(settings.clone()));
        Ok(settings)
    }
}

fn processing_now_ms(fallback_ms: i64) -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|age| age.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(fallback_ms)
}

struct JoinRiskWorker<S> {
    guild_id: u64,
    settings: S,
    store: JoinRiskStore,
    delivery: RaidDelivery,
    joins: mpsc::Receiver<JoinObservation>,
    shutdown: Arc<crate::raid_runtime::WorkerShutdown>,
}

impl<S: SettingsSource> JoinRiskWorker<S> {
    async fn run(mut self) {
        loop {
            tokio::select! {
                biased;
                () = self.shutdown.notified() => {
                    let dropped = crate::raid_runtime::drain_queued(&mut self.joins);
                    self.shutdown.note_dropped(dropped);
                    crate::raid_runtime::worker_shutdown_drain(self.shutdown.finished(), dropped);
                    return;
                }
                msg = self.joins.recv() => {
                    let Some(join) = msg else {
                        // Unlatched sender-drop: every buffered join already
                        // yielded above, so the legacy full drain exits here.
                        return;
                    };
                    // An internal failure must never end scoring: log it and carry on.
                    if AssertUnwindSafe(self.handle(join))
                        .catch_unwind()
                        .await
                        .is_err()
                    {
                        error!(
                            error_class = "join_risk_failed",
                            "join_risk_failed: observation panicked"
                        );
                    }
                    self.shutdown.note_finished();
                }
            }
        }
    }

    async fn handle(&mut self, join: JoinObservation) {
        // The shard serves one configured guild; other guilds are not scored.
        if join.guild_id != self.guild_id {
            return;
        }
        // Fail closed on present-but-unusable tuning: drop the observation
        // with no scoring side effects (no row, no staff post, never a
        // Discord message). The refusal is already logged at the source on
        // state changes, so the per-observation drop stays at debug.
        let settings = match self.settings.current().await {
            Ok(settings) => settings,
            Err(refusal) => {
                debug!(
                    key = refusal.key(),
                    member_id = join.member_id,
                    "join_risk_tuning_refused: observation dropped; no row claimed, no post attempted"
                );
                return;
            }
        };
        let policy = match JoinRiskPolicy::new(
            self.guild_id.to_string(),
            settings.tuning.window_seconds(),
            settings.tuning.threshold(),
            settings.bulk_join_window_until_ms,
        ) {
            Ok(policy) => policy,
            Err(_) => {
                error!(
                    error_class = "join_risk_policy_rejected",
                    "join_risk_policy_rejected: live tuning unusable"
                );
                return;
            }
        };
        // The pipeline only observes non-bot joins, so the bot flag is always
        // false here; the account age decodes from the member snowflake, the
        // same derivation legacy `snowflakeToDate` used.
        let Some(created_at_ms) =
            snowflake_to_date_ms(&join.member_id.to_string()).map(|ms| ms as i64)
        else {
            error!(
                error_class = "join_risk_no_account_age",
                member_id = join.member_id,
                "join_risk_no_account_age"
            );
            return;
        };
        let input = JoinRiskInput {
            guild_id: join.guild_id.to_string(),
            member_id: join.member_id.to_string(),
            member_is_bot: false,
            account_created_at_ms: created_at_ms,
            joined_at_ms: Some(join.joined_at_ms),
            source: join.source.clone(),
        };
        let Some(observation) = policy.prepare(&input, join.joined_at_ms) else {
            return;
        };
        let claim = match self
            .store
            .record(observation, processing_now_ms(join.joined_at_ms))
            .await
        {
            Ok(claim) => claim,
            Err(_) => {
                error!(
                    error_class = "join_risk_store_failed",
                    "join_risk_store_failed: claim not recorded"
                );
                return;
            }
        };
        let JoinRiskClaim::Persisted {
            evidence,
            join_count,
        } = claim
        else {
            // Already claimed: a replay of a failed send must not alert twice.
            debug!(member_id = join.member_id, "join_risk_duplicate");
            return;
        };
        // Always log first. If the post fails the evidence still exists.
        error!(
            error_class = "join_risk_evidence",
            member_id = %evidence.observation.member_id,
            score = evidence.score,
            join_count,
            flagged = evidence.flagged,
            reasons = ?evidence.reasons,
            "join_risk_evidence"
        );
        let Some(message) = evidence.staff_message(true) else {
            return;
        };
        let Some(channel) = settings.staff_channel.as_deref() else {
            return;
        };
        // The shared executor always sends empty allowed mentions; refuse a
        // proposal that asks for anything else rather than widen that boundary.
        if message.mentions != MentionPolicy::None {
            error!(
                error_class = "join_risk_refused",
                channel, "join_risk_refused: mention policy is not empty"
            );
            return;
        }
        match self
            .delivery
            .post_staff_message(channel, &message.content)
            .await
        {
            Delivery::Posted => info!(
                channel,
                member_id = %evidence.observation.member_id,
                "join_risk_posted"
            ),
            Delivery::Undeliverable => error!(
                error_class = "join_risk_undeliverable",
                channel,
                "join_risk_undeliverable: channel missing, not text, or bot lacks View/Send"
            ),
            Delivery::Failed => error!(
                error_class = "join_risk_post_failed",
                channel, "join_risk_post_failed"
            ),
        }
    }
}

/// The pipeline-facing half: queue only, never block.
pub struct JoinRiskRuntime {
    joins: mpsc::Sender<JoinObservation>,
    shutdown: Arc<crate::raid_runtime::WorkerShutdown>,
}

impl JoinRiskRuntime {
    /// Latch the bounded shutdown drain: the worker finishes its in-flight
    /// join, drops the queued inbox with a count, logs
    /// `worker_shutdown_drain`, and exits so its handle joins.
    pub fn shutdown(&self) {
        self.shutdown.latch();
    }

    /// Dequeued joins handled to completion, caught panics included.
    #[cfg(test)]
    pub fn finished(&self) -> u64 {
        self.shutdown.finished()
    }

    /// Queued joins dropped without starting after a latched shutdown.
    #[cfg(test)]
    pub fn dropped_shutdown(&self) -> u64 {
        self.shutdown.dropped()
    }
}

impl JoinObserver for JoinRiskRuntime {
    fn observe_join(&self, join: JoinObservation) {
        match self.joins.try_send(join) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(join)) => {
                metrics::global().join_risk_dropped_join(JoinQueueDropReason::QueueFull);
                warn!(
                    member_id = join.member_id,
                    "join-risk queue full; join not scored"
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                metrics::global().join_risk_dropped_join(JoinQueueDropReason::QueueClosed);
                error!(
                    error_class = "join_risk_queue_closed",
                    "join-risk queue closed; join not scored"
                );
            }
        }
    }
}

/// One pipeline slot, two runtimes: the raid watch always observes, and the
/// join-risk runtime additionally observes when constructed.
pub struct Fanout {
    pub first: Arc<dyn JoinObserver>,
    pub second: Arc<dyn JoinObserver>,
}

impl JoinObserver for Fanout {
    fn observe_join(&self, join: JoinObservation) {
        self.first.observe_join(join.clone());
        self.second.observe_join(join);
    }
}

/// Spawn the worker. [`JoinRiskRuntime::shutdown`] latches the bounded drain
/// (finish the in-flight join, drop the queue with a count); an unlatched
/// worker ends when the last [`JoinRiskRuntime`] reference drops, draining
/// fully as before.
pub fn start<S: SettingsSource>(
    guild_id: u64,
    settings: S,
    store: JoinRiskStore,
    executor: ActionExecutor,
) -> (Arc<JoinRiskRuntime>, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
    let shutdown = Arc::new(crate::raid_runtime::WorkerShutdown::default());
    let worker = JoinRiskWorker {
        guild_id,
        settings,
        store,
        delivery: RaidDelivery::new(executor, guild_id),
        joins: receiver,
        shutdown: Arc::clone(&shutdown),
    };
    (
        Arc::new(JoinRiskRuntime {
            joins: sender,
            shutdown,
        }),
        tokio::spawn(worker.run()),
    )
}

/// Production wiring: the raid watch always, plus join-risk delivery only
/// inside the anti-nuke fences. Cold reads (`TWO_ANTI_NUKE`,
/// `TWO_ANTI_NUKE_DRY_RUN`, `TWO_ONBOARDING_MODE`) come from the deployment
/// environment; Hot tuning comes from the settings store. The service
/// shutdown receiver latches the risk worker drain through a bounded
/// supervisor that holds the worker handle and joins-or-aborts it; the
/// supervisor handle is returned for the `main` shutdown join (`None` when
/// the fences refuse and no risk worker spawns) and must not be detached.
pub fn chain_from_env(
    pool: PgPool,
    executor: ActionExecutor,
    guild_id: u64,
    raid: Arc<RaidRuntime>,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> (Arc<dyn JoinObserver>, Option<JoinHandle<()>>) {
    let vars: HashMap<String, String> = std::env::vars().collect();
    let fences = AntiNukeFences::from_vars(&vars, guild_id);
    if !fences.enabled {
        info!(
            anti_nuke = vars.get(ENABLE_KEY).is_some_and(|value| value == "1"),
            staging_guild = guild_id.to_string() == STAGING_GUILD_ID,
            "join_risk_disabled: chain is the raid watch alone"
        );
        let observer: Arc<dyn JoinObserver> = raid;
        return (observer, None);
    }
    info!(
        armed = fences.armed,
        session_mode = vars
            .get(MODE_KEY)
            .is_some_and(|value| value.trim() == "session"),
        "join_risk_fences: staff messages flow in every enabled mode; arming stays refused without explicit dry-run 0 outside session mode"
    );
    let deployment: HashMap<String, String> = HOT_KEYS
        .iter()
        .filter_map(|key| {
            vars.get(*key)
                .map(|value| ((*key).to_owned(), value.clone()))
        })
        .collect();
    let (risk, worker) = start(
        guild_id,
        StoreSettings::new(pool.clone(), guild_id, deployment),
        JoinRiskStore::from_pool(pool),
        executor,
    );
    let supervisor = crate::raid_runtime::join_on_shutdown(
        worker,
        shutdown,
        crate::raid_runtime::WORKER_JOIN_BUDGET,
        {
            let risk = Arc::clone(&risk);
            move || risk.shutdown()
        },
    );
    let observer: Arc<dyn JoinObserver> = Arc::new(Fanout {
        first: raid,
        second: risk,
    });
    (observer, Some(supervisor))
}

#[cfg(test)]
mod store_settings_cache_tests {
    //! Non-ignored coverage for [`StoreSettings::current`]: the 15 s outcome
    //! cache must hold good settings and refusals alike, so a misconfigured
    //! key cannot turn per-join reads into per-join database load. The pool
    //! below points at a closed loopback port, so [`StoreSettings::refresh`]
    //! fails fast and the deployment values alone decide each read.

    use super::*;
    use sqlx::postgres::PgPoolOptions;

    fn test_pool() -> PgPool {
        PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(2))
            .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
            .expect("lazy pool never fails to construct")
    }

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[tokio::test]
    async fn good_outcome_caches_within_window_and_covers_tuning_branches() {
        let mut source = StoreSettings::new(
            test_pool(),
            22,
            vars(&[(WINDOW_KEY, "30.5"), (THRESHOLD_KEY, "8")]),
        );
        // First read: the Ok arm, the first-enable log arm, store and return.
        let first = source.current().await.expect("valid deployment reads");
        assert_eq!(first.tuning.threshold(), 8.0);
        // Immediate second read: the cached outcome, no re-read.
        let cached = source.current().await.expect("cached good reads");
        assert_eq!(cached, first);
        // Same tuning after expiry: the unchanged-tuning arm.
        source.expire_for_tests();
        let same = source.current().await.expect("same tuning reads");
        assert_eq!(same, first);
        // Different tuning after expiry: the tuning-changed arm.
        source.expire_for_tests();
        source.set_deployment_for_tests(vars(&[(WINDOW_KEY, "30.5"), (THRESHOLD_KEY, "9")]));
        let changed = source.current().await.expect("changed tuning reads");
        assert_eq!(changed.tuning.threshold(), 9.0);
    }

    #[tokio::test]
    async fn refusal_caches_within_window() {
        let mut source = StoreSettings::new(test_pool(), 22, vars(&[(THRESHOLD_KEY, "abc")]));
        // First read: the refusal names the key (error-on-change arm).
        let refused = source
            .current()
            .await
            .expect_err("unusable threshold refuses");
        assert_eq!(refused.key(), THRESHOLD_KEY);
        // Immediate second read: the cached refusal, same key, no re-read.
        let cached = source.current().await.expect_err("cached refusal refuses");
        assert_eq!(cached.key(), THRESHOLD_KEY);
    }

    #[tokio::test]
    async fn steady_refusal_takes_debug_arm_and_key_change_errors_again() {
        let mut source = StoreSettings::new(test_pool(), 22, vars(&[(THRESHOLD_KEY, "abc")]));
        let first = source.current().await.expect_err("first refusal refuses");
        assert_eq!(first.key(), THRESHOLD_KEY);
        // Expire the window but keep the same bad key: the steady refused
        // state takes the debug (not error) arm and returns the same refusal.
        source.expire_for_tests();
        let steady = source.current().await.expect_err("steady refusal refuses");
        assert_eq!(steady.key(), THRESHOLD_KEY);
        // A different bad key is a state change: the error arm again.
        source.expire_for_tests();
        source.set_deployment_for_tests(vars(&[(WINDOW_KEY, "0")]));
        let changed = source.current().await.expect_err("different key refuses");
        assert_eq!(changed.key(), WINDOW_KEY);
    }
}
