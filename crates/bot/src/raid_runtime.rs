//! Join-burst watch runtime (TOG-10430; decisions in `docs/raid-port.md`).
//!
//! The gateway pipeline hands every non-bot join to [`RaidRuntime`] after the
//! funnel has recorded it, so an alert problem can never cost the join row it
//! reports on (legacy `client.ts`: "burst check last"). The runtime only
//! queues; a single worker owns the volatile [`RaidWatch`], reads the live
//! tuning per observation and delivers the staff message through the shared
//! [`ActionExecutor`]. There is no private client, timer or retry.
//!
//! Like legacy `makeRaidAnnouncer`, evidence is logged first. A post happens
//! only to a configured guild text channel the bot can View and Send in, and it
//! carries empty allowed mentions: nobody is pinged, DMed or moderated. The
//! cooldown is consumed when the alert is proposed, so a missing channel,
//! denied permission or failed send is never retried.

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::FutureExt as _;
use sqlx::PgPool;
use tokio::sync::{mpsc, OnceCell};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};
use two_bot_core::metrics::{self, JoinQueueDropReason};
use two_bot_core::onboarding::MentionPolicy;
use two_bot_core::settings::SettingsCache;
use two_bot_core::{
    RaidAlert, RaidTuning, RaidWatch, DEFAULT_RAID_THRESHOLD, DEFAULT_RAID_WINDOW_SECONDS,
};
use two_bot_cutover::settings::SettingsStore;
use two_bot_discord::onboarding_permissions::MemberAccess;
use two_bot_discord::{ActionExecutor, JoinObservation, JoinObserver};

/// Joins queued for the worker. A full queue drops the join (logged): the
/// dispatch writer must never wait on this runtime.
pub(crate) const QUEUE_CAPACITY: usize = 2048;
/// Bound on one alert's bot lookup, permission reads and post.
const DELIVERY_MAX: Duration = Duration::from_secs(12);
/// The live settings are re-checked at most this often (legacy 15 s poll).
const SETTINGS_MAX_AGE: Duration = Duration::from_secs(15);
const SETTINGS_READ_MAX: Duration = Duration::from_millis(1500);

const THRESHOLD_KEY: &str = "TWO_RAID_JOIN_THRESHOLD";
const WINDOW_KEY: &str = "TWO_RAID_WINDOW_SECONDS";
const CHANNEL_KEY: &str = "DISCORD_STAFF_ALERT_CHANNEL_ID";
const SETTING_KEYS: [&str; 3] = [THRESHOLD_KEY, WINDOW_KEY, CHANNEL_KEY];

fn is_snowflake(value: &str) -> bool {
    (17..=20).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_digit())
}

/// The raid configuration for one observation.
#[derive(Debug, Clone, PartialEq)]
pub struct RaidSettings {
    pub tuning: RaidTuning,
    /// `None` means log only: no channel is guessed on a live server.
    pub staff_channel: Option<String>,
}

/// Key-only diagnostic: malformed configuration refuses the whole observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RaidTuningRefusal {
    key: &'static str,
}

impl RaidTuningRefusal {
    pub fn key(&self) -> &'static str {
        self.key
    }
}

impl std::fmt::Display for RaidTuningRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "raid tuning refused: {} present but unusable", self.key)
    }
}

impl std::error::Error for RaidTuningRefusal {}

impl RaidSettings {
    /// Live reader: absent/empty keys retain defaults; malformed values refuse
    /// the whole read. No input value is retained in the diagnostic.
    pub fn try_from_vars(vars: &HashMap<String, String>) -> Result<Self, RaidTuningRefusal> {
        let number = |key: &'static str, default: f64| match vars.get(key).map(|raw| raw.trim()) {
            None | Some("") => Ok(default),
            Some(raw) => match raw.parse::<f64>() {
                Ok(value) if value.is_finite() && value > 0.0 => Ok(value),
                _ => Err(RaidTuningRefusal { key }),
            },
        };
        let window = number(WINDOW_KEY, DEFAULT_RAID_WINDOW_SECONDS)?;
        let threshold = number(THRESHOLD_KEY, DEFAULT_RAID_THRESHOLD)?;
        let tuning = RaidTuning::new(window, threshold)
            .map_err(|_| RaidTuningRefusal { key: WINDOW_KEY })?;
        let staff_channel = match vars.get(CHANNEL_KEY).map(|raw| raw.trim()) {
            None | Some("") => None,
            Some(raw) if is_snowflake(raw) => Some(raw.to_owned()),
            Some(_) => return Err(RaidTuningRefusal { key: CHANNEL_KEY }),
        };
        Ok(Self {
            tuning,
            staff_channel,
        })
    }

    /// Legacy compatibility reader, only for tests. Live callers must refuse
    /// malformed values rather than substitute defaults or log-only delivery.
    #[cfg(test)]
    pub fn from_vars(vars: &HashMap<String, String>) -> Self {
        let number = |key: &str, default: f64| match vars.get(key).map(|raw| raw.trim()) {
            None | Some("") => default,
            Some(raw) => match raw.parse::<f64>() {
                Ok(value) if value.is_finite() && value > 0.0 => value,
                _ => {
                    warn!(key, "raid setting unusable; using the default");
                    default
                }
            },
        };
        let tuning = RaidTuning::new(
            number(WINDOW_KEY, DEFAULT_RAID_WINDOW_SECONDS),
            number(THRESHOLD_KEY, DEFAULT_RAID_THRESHOLD),
        )
        .unwrap_or_default();
        let staff_channel = match vars.get(CHANNEL_KEY).map(|raw| raw.trim()) {
            None | Some("") => None,
            Some(raw) if is_snowflake(raw) => Some(raw.to_owned()),
            Some(_) => {
                warn!(key = CHANNEL_KEY, "staff alert channel unusable; log only");
                None
            }
        };
        Self {
            tuning,
            staff_channel,
        }
    }
}

/// Compatibility seam for callers supplying already-constructed settings.
/// Raw live configuration uses [`CheckedSettingsSource`] instead.
pub trait SettingsSource: Send + 'static {
    fn current(&mut self) -> impl Future<Output = RaidSettings> + Send;
}

/// Live settings reads can refuse an observation before the watch changes.
pub trait CheckedSettingsSource: Send + 'static {
    fn try_current(
        &mut self,
    ) -> impl Future<Output = Result<RaidSettings, RaidTuningRefusal>> + Send;
}

impl<S: SettingsSource> CheckedSettingsSource for S {
    async fn try_current(&mut self) -> Result<RaidSettings, RaidTuningRefusal> {
        Ok(self.current().await)
    }
}

/// Tuning and the staff channel are store-first (hot rows win over the
/// deployment environment) and re-read at most every `SETTINGS_MAX_AGE`; a
/// failed refresh keeps the last outcome, including a refusal.
pub struct StoreSettings {
    pool: PgPool,
    guild_id: String,
    deployment: HashMap<String, String>,
    max_age: Duration,
    cache: Option<SettingsCache>,
    checked: Option<Instant>,
    last: Option<Result<RaidSettings, RaidTuningRefusal>>,
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

    /// Re-read on every observation (tests).
    #[cfg(test)]
    pub fn with_max_age(mut self, max_age: Duration) -> Self {
        self.max_age = max_age;
        self
    }

    async fn refresh(&mut self) -> Result<RaidSettings, RaidTuningRefusal> {
        let store = SettingsStore::new(&self.pool);
        let stale = self.cache.as_ref();
        let loaded = tokio::time::timeout(SETTINGS_READ_MAX, async {
            let (revision, rows) = store.poll_marks().await?;
            if stale.is_some_and(|cache| !cache.needs_refresh(revision, rows)) {
                return Ok::<_, sqlx::Error>(None);
            }
            let snapshot = store.load_snapshot().await?;
            Ok(Some(SettingsCache::load(&snapshot)))
        })
        .await;
        self.apply_refresh(match loaded {
            Ok(Ok(cache)) => Ok(cache),
            Ok(Err(_)) | Err(_) => Err(()),
        })
    }

    fn apply_refresh(
        &mut self,
        loaded: Result<Option<SettingsCache>, ()>,
    ) -> Result<RaidSettings, RaidTuningRefusal> {
        match loaded {
            Ok(Some(cache)) => self.cache = Some(cache),
            Ok(None) => {}
            Err(()) => {
                warn!("raid settings refresh failed; keeping the last outcome");
                if let Some(last) = &self.last {
                    return last.clone();
                }
            }
        }
        let mut vars = self.deployment.clone();
        if let Some(cache) = &self.cache {
            // A stored row wins; deletion hands the key back to deployment.
            vars.extend(
                cache
                    .env_snapshot(Some(self.guild_id.as_str()))
                    .into_iter()
                    .filter(|(key, _)| SETTING_KEYS.contains(&key.as_str())),
            );
        }
        let settings = match RaidSettings::try_from_vars(&vars) {
            Ok(settings) => settings,
            Err(refusal) => {
                if !matches!(&self.last, Some(Err(previous)) if previous.key() == refusal.key()) {
                    error!(
                        error_class = "raid_tuning_refused",
                        key = refusal.key(),
                        "raid_tuning_refused: observations disabled"
                    );
                }
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
                "setting_changed: raid tuning"
            ),
            Some(Ok(_)) => {}
            Some(Err(_)) | None => info!(
                threshold = settings.tuning.threshold(),
                window_seconds = settings.tuning.window_seconds(),
                alert_target = settings
                    .staff_channel
                    .as_deref()
                    .unwrap_or("log only (DISCORD_STAFF_ALERT_CHANNEL_ID unset)"),
                "raid_watch_enabled"
            ),
        }
        self.last = Some(Ok(settings.clone()));
        Ok(settings)
    }
}

impl CheckedSettingsSource for StoreSettings {
    async fn try_current(&mut self) -> Result<RaidSettings, RaidTuningRefusal> {
        if let (Some(at), Some(last)) = (self.checked, &self.last) {
            if at.elapsed() < self.max_age {
                return last.clone();
            }
        }
        self.checked = Some(Instant::now());
        self.refresh().await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    Posted,
    /// The channel is missing, not a guild text channel, or the bot lacks
    /// View/Send there: a proven refusal, nothing was sent.
    Undeliverable,
    /// No usable evidence or the send failed. Never retried.
    Failed,
}

pub(crate) struct RaidDelivery {
    executor: ActionExecutor,
    guild_id: u64,
    bot_id: OnceCell<u64>,
}

impl RaidDelivery {
    pub(crate) fn new(executor: ActionExecutor, guild_id: u64) -> Self {
        Self {
            executor,
            guild_id,
            bot_id: OnceCell::new(),
        }
    }

    async fn announce(&self, alert: &RaidAlert, channel: Option<&str>) {
        // Always log first. If the post fails the evidence still exists.
        error!(
            error_class = "raid_alert",
            guild_id = %alert.guild_id,
            count = alert.count,
            span_seconds = alert.span_seconds,
            window_seconds = alert.window_seconds,
            first_join_at = %alert.first_join_at,
            last_join_at = %alert.last_join_at,
            repeat = alert.repeat,
            member_ids = ?alert.member_ids,
            truncated = alert.truncated,
            "raid_alert"
        );
        let Some(channel) = channel else {
            return;
        };
        let message = alert.staff_message();
        // The shared executor always sends empty allowed mentions; refuse a
        // proposal that asks for anything else rather than widen that boundary.
        if message.mentions != MentionPolicy::None {
            error!(
                error_class = "raid_alert_refused",
                channel, "raid_alert_refused: mention policy is not empty"
            );
            return;
        }
        match self.post_staff_message(channel, &message.content).await {
            Delivery::Posted => info!(channel, count = alert.count, "raid_alert_posted"),
            Delivery::Undeliverable => error!(
                error_class = "raid_alert_undeliverable",
                channel,
                "raid_alert_undeliverable: channel missing, not text, or bot lacks View/Send"
            ),
            Delivery::Failed => error!(
                error_class = "raid_alert_post_failed",
                channel, "raid_alert_post_failed"
            ),
        }
    }

    /// Log-first post shared with the join-risk runtime: the caller logs the
    /// evidence and enforces the empty-mentions boundary; this only bounds,
    /// sends and classifies. Never retried by either caller.
    pub(crate) async fn post_staff_message(&self, channel: &str, content: &str) -> Delivery {
        match tokio::time::timeout(DELIVERY_MAX, self.post(channel, content)).await {
            Ok(outcome) => outcome,
            Err(_) => Delivery::Failed,
        }
    }

    async fn post(&self, channel: &str, content: &str) -> Delivery {
        let bot_id = match self
            .bot_id
            .get_or_try_init(|| self.executor.current_bot_user_id())
            .await
        {
            Ok(id) => *id,
            Err(_) => return Delivery::Failed,
        };
        let access = match MemberAccess::load(&self.executor, self.guild_id, bot_id).await {
            Ok(Some(access)) => access,
            Ok(None) => return Delivery::Undeliverable,
            Err(_) => return Delivery::Failed,
        };
        match access.permits(&self.executor, channel, true).await {
            Ok(true) => {}
            Ok(false) => return Delivery::Undeliverable,
            Err(_) => return Delivery::Failed,
        }
        match self.executor.post_message(channel, content, None).await {
            Ok(_) => Delivery::Posted,
            Err(_) => Delivery::Failed,
        }
    }
}

struct RaidWorker<S> {
    guild_id: u64,
    settings: S,
    delivery: RaidDelivery,
    watch: RaidWatch,
    joins: mpsc::Receiver<JoinObservation>,
}

impl<S: CheckedSettingsSource> RaidWorker<S> {
    async fn run(mut self) {
        while let Some(join) = self.joins.recv().await {
            // An internal failure must never end the watch: log it and carry on.
            if AssertUnwindSafe(self.handle(join))
                .catch_unwind()
                .await
                .is_err()
            {
                error!(
                    error_class = "raid_watch_failed",
                    "raid_watch_failed: observation panicked"
                );
            }
        }
    }

    async fn handle(&mut self, join: JoinObservation) {
        // The shard serves one configured guild; other guilds are not watched.
        if join.guild_id != self.guild_id {
            return;
        }
        // The source logs state changes. Drop before updating the watch, so a
        // refused join cannot contribute to a later alert or consume cooldown.
        let Ok(settings) = self.settings.try_current().await else {
            return;
        };
        let Some(alert) = self.watch.observe(
            &join.guild_id.to_string(),
            &join.member_id.to_string(),
            join.joined_at_ms,
            settings.tuning,
        ) else {
            return;
        };
        self.delivery
            .announce(&alert, settings.staff_channel.as_deref())
            .await;
    }
}

/// The pipeline-facing half: queue only, never block.
pub struct RaidRuntime {
    joins: mpsc::Sender<JoinObservation>,
}

impl JoinObserver for RaidRuntime {
    fn observe_join(&self, join: JoinObservation) {
        match self.joins.try_send(join) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(join)) => {
                metrics::global().raid_dropped_join(JoinQueueDropReason::QueueFull);
                warn!(
                    member_id = join.member_id,
                    "raid watch queue full; join not observed"
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                metrics::global().raid_dropped_join(JoinQueueDropReason::QueueClosed);
                error!(
                    error_class = "raid_queue_closed",
                    "raid watch queue closed; join not observed"
                );
            }
        }
    }
}

/// Spawn the worker. It ends when the last [`RaidRuntime`] reference drops.
pub fn start<S: CheckedSettingsSource>(
    guild_id: u64,
    settings: S,
    executor: ActionExecutor,
) -> (Arc<RaidRuntime>, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
    let worker = RaidWorker {
        guild_id,
        settings,
        delivery: RaidDelivery::new(executor, guild_id),
        watch: RaidWatch::default(),
        joins: receiver,
    };
    (
        Arc::new(RaidRuntime { joins: sender }),
        tokio::spawn(worker.run()),
    )
}

/// Production wiring: deployment values from the environment, hot rows from the
/// settings store. Always on, as legacy raid watch was (TWO-56).
pub fn start_from_env(pool: PgPool, executor: ActionExecutor, guild_id: u64) -> Arc<RaidRuntime> {
    let deployment = SETTING_KEYS
        .iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .map(|value| ((*key).to_owned(), value))
        })
        .collect();
    let (runtime, _worker) = start(
        guild_id,
        StoreSettings::new(pool, guild_id, deployment),
        executor,
    );
    runtime
}

#[cfg(test)]
#[path = "raid_tuning_runtime_tests.rs"]
mod tuning_runtime_tests;
