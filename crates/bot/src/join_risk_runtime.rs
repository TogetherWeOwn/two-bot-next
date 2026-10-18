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
pub(crate) struct AntiNukeFences {
    /// Exact `TWO_ANTI_NUKE=1` on the staging guild. Without it the chain is
    /// the raid watch alone and no claim is ever recorded.
    pub(crate) enabled: bool,
    /// Dry-run explicitly `0` outside session mode. R2 has no armed path;
    /// R3 refuses every armed action while this is false.
    pub(crate) armed: bool,
}

impl AntiNukeFences {
    pub(crate) fn from_vars(vars: &HashMap<String, String>, guild_id: u64) -> Self {
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
pub(crate) struct JoinRiskSettings {
    pub(crate) tuning: RaidTuning,
    pub(crate) bulk_join_window_until_ms: Option<i64>,
    /// `None` means log only: no channel is guessed on a live server.
    pub(crate) staff_channel: Option<String>,
}

/// A present-but-unusable tuning key that refuses scoring and arming.
///
/// The refusal names the key family ([`JoinRiskTuningRefusal::key`]), never
/// the value, so the operator-visible signal identifies the bad key without
/// echoing a possibly secret value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JoinRiskTuningRefusal {
    key: &'static str,
}

impl JoinRiskTuningRefusal {
    pub(crate) fn key(&self) -> &'static str {
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
    pub(crate) fn try_from_vars(
        vars: &HashMap<String, String>,
    ) -> Result<Self, JoinRiskTuningRefusal> {
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
pub(crate) trait SettingsSource: Send + 'static {
    fn current(
        &mut self,
    ) -> impl Future<Output = Result<JoinRiskSettings, JoinRiskTuningRefusal>> + Send;
}

/// Tuning, the bulk window and the staff channel are store-first (hot rows win
/// over the deployment environment) and re-read at most every
/// [`SETTINGS_MAX_AGE`]; a failed refresh keeps the last outcome (good
/// settings or the cached refusal).
pub(crate) struct StoreSettings {
    pool: PgPool,
    guild_id: String,
    deployment: HashMap<String, String>,
    max_age: Duration,
    cache: Option<SettingsCache>,
    checked: Option<Instant>,
    last: Option<Result<JoinRiskSettings, JoinRiskTuningRefusal>>,
}

impl StoreSettings {
    pub(crate) fn new(pool: PgPool, guild_id: u64, deployment: HashMap<String, String>) -> Self {
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

    async fn refresh(&mut self) {
        let store = SettingsStore::new(&self.pool);
        let stale = self.cache.as_ref();
        // Bound to a local so the timeout future is dropped before the borrows
        // above end (edition-2021 tail-expression temporaries).
        let loaded = tokio::time::timeout(SETTINGS_READ_MAX, async {
            let (revision, rows) = store.poll_marks().await?;
            if stale.is_some_and(|cache| !cache.needs_refresh(revision, rows)) {
                return Ok::<_, sqlx::Error>(None);
            }
            let snapshot = store.load_snapshot().await?;
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
}

impl<S: SettingsSource> JoinRiskWorker<S> {
    async fn run(mut self) {
        while let Some(join) = self.joins.recv().await {
            // An internal failure must never end scoring: log it and carry on.
            if AssertUnwindSafe(self.handle(join))
                .catch_unwind()
                .await
                .is_err()
            {
                error!("join_risk_failed: observation panicked");
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
                error!("join_risk_policy_rejected: live tuning unusable");
                return;
            }
        };
        // The pipeline only observes non-bot joins, so the bot flag is always
        // false here; the account age decodes from the member snowflake, the
        // same derivation legacy `snowflakeToDate` used.
        let Some(created_at_ms) =
            snowflake_to_date_ms(&join.member_id.to_string()).map(|ms| ms as i64)
        else {
            error!(member_id = join.member_id, "join_risk_no_account_age");
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
                error!("join_risk_store_failed: claim not recorded");
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
            error!(channel, "join_risk_refused: mention policy is not empty");
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
                channel,
                "join_risk_undeliverable: channel missing, not text, or bot lacks View/Send"
            ),
            Delivery::Failed => error!(channel, "join_risk_post_failed"),
        }
    }
}

/// The pipeline-facing half: queue only, never block.
pub(crate) struct JoinRiskRuntime {
    joins: mpsc::Sender<JoinObservation>,
}

impl JoinObserver for JoinRiskRuntime {
    fn observe_join(&self, join: JoinObservation) {
        match self.joins.try_send(join) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(join)) => warn!(
                member_id = join.member_id,
                "join-risk queue full; join not scored"
            ),
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }
}

/// One pipeline slot, two runtimes: the raid watch always observes, and the
/// join-risk runtime additionally observes when constructed.
pub(crate) struct Fanout {
    pub(crate) first: Arc<dyn JoinObserver>,
    pub(crate) second: Arc<dyn JoinObserver>,
}

impl JoinObserver for Fanout {
    fn observe_join(&self, join: JoinObservation) {
        self.first.observe_join(join.clone());
        self.second.observe_join(join);
    }
}

/// Spawn the worker. It ends when the last [`JoinRiskRuntime`] reference drops.
pub(crate) fn start<S: SettingsSource>(
    guild_id: u64,
    settings: S,
    store: JoinRiskStore,
    executor: ActionExecutor,
) -> (Arc<JoinRiskRuntime>, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
    let worker = JoinRiskWorker {
        guild_id,
        settings,
        store,
        delivery: RaidDelivery::new(executor, guild_id),
        joins: receiver,
    };
    (
        Arc::new(JoinRiskRuntime { joins: sender }),
        tokio::spawn(worker.run()),
    )
}

/// Production wiring: the raid watch always, plus join-risk delivery only
/// inside the anti-nuke fences. Cold reads (`TWO_ANTI_NUKE`,
/// `TWO_ANTI_NUKE_DRY_RUN`, `TWO_ONBOARDING_MODE`) come from the deployment
/// environment; Hot tuning comes from the settings store.
pub(crate) fn chain_from_env(
    pool: PgPool,
    executor: ActionExecutor,
    guild_id: u64,
    raid: Arc<RaidRuntime>,
) -> Arc<dyn JoinObserver> {
    let vars: HashMap<String, String> = std::env::vars().collect();
    let fences = AntiNukeFences::from_vars(&vars, guild_id);
    if !fences.enabled {
        info!(
            anti_nuke = vars.get(ENABLE_KEY).is_some_and(|value| value == "1"),
            staging_guild = guild_id.to_string() == STAGING_GUILD_ID,
            "join_risk_disabled: chain is the raid watch alone"
        );
        return raid;
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
    let (risk, _worker) = start(
        guild_id,
        StoreSettings::new(pool.clone(), guild_id, deployment),
        JoinRiskStore::from_pool(pool),
        executor,
    );
    Arc::new(Fanout {
        first: raid,
        second: risk,
    })
}
