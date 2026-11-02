//! Containment delivery runtime (TOG-10430 R3; decisions in
//! `docs/containment.md` and `docs/raid-port.md`).
//!
//! The gateway pipeline hands destructive-potential audit-log entries to the
//! [`AuditEntryObserver`] seam with no funnel row. This runtime claims each
//! entry through [`ContainmentStore::claim_event`] (atomic disposition +
//! occurrence heat under the guild/executor advisory lock), opens an incident
//! when heat reaches threshold, plans the quarantine and delivers the outcome
//! through the shared [`ActionExecutor`]. Dry-run is the default: without an
//! explicitly armed configuration the worker logs the plan and posts the
//! staff alert, and no role is ever touched.
//!
//! Fences, in order: the runtime is constructed only under exact
//! `TWO_ANTI_NUKE=1` on the staging guild (reusing the join-risk fences);
//! the worker then verifies the bot application identity before processing
//! any entry. Arming additionally requires `TWO_ANTI_NUKE_DRY_RUN` explicitly
//! `0` outside session mode — the same refusal R2 reports. Only an armed
//! worker executes `RemoveRoles`, sequentially and stopping on the first
//! failure; every other outcome (dry-run, refused, failed snapshot) only
//! logs, completes the incident durably and posts the flag-only staff alert
//! with empty allowed mentions. Duplicates, blocked (cooling-down) incidents
//! and failed sends are never retried.
//!
//! Entry timestamps decode from the audit-entry snowflake, the same
//! derivation legacy used; an undecodable ID is stale by definition.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::FutureExt as _;
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};
use twilight_model::guild::audit_log::AuditLogEventType;
use two_bot_core::backup::guild_config::STAGING_BOT_APPLICATION_ID;
use two_bot_core::containment::{
    plan_quarantine, quarantine_outcome, ContainmentAlert, ContainmentDisposition,
    ContainmentIncident, ContainmentIncidentState, ContainmentPolicy, ContainmentReason,
    ContainmentRole, DestructiveAction, DestructiveAuditEvent, QuarantineFailure, QuarantinePlan,
    QuarantineRefusal, DEFAULT_CONTAINMENT_HEAT_THRESHOLD, DEFAULT_CONTAINMENT_MAX_AGE_MS,
    DEFAULT_CONTAINMENT_WINDOW_MS,
};
use two_bot_core::containment_store::{ContainmentStore, EventClaim, IncidentClaim};
use two_bot_core::onboarding::MentionPolicy;
use two_bot_core::settings::SettingsCache;
use two_bot_cutover::parse::snowflake_to_date_ms;
use two_bot_cutover::settings::SettingsStore;
use two_bot_discord::{ActionExecutor, AuditEntryObserver, AuditLogObservation, DiscordError};

use crate::join_risk_runtime::AntiNukeFences;
use crate::raid_runtime::{Delivery, RaidDelivery, QUEUE_CAPACITY};

/// Re-checked at most this often (legacy 15 s poll).
const SETTINGS_MAX_AGE: Duration = Duration::from_secs(15);
pub(crate) const SETTINGS_READ_MAX: Duration = Duration::from_millis(1500);

const WINDOW_KEY: &str = "TWO_ANTI_NUKE_WINDOW_SECONDS";
const MAX_AGE_KEY: &str = "TWO_ANTI_NUKE_EVENT_MAX_AGE_SECONDS";
const HEAT_KEY: &str = "TWO_ANTI_NUKE_HEAT_THRESHOLD";
const TRUSTED_KEY: &str = "TWO_ANTI_NUKE_TRUSTED_USER_IDS";
const PROTECTED_KEY: &str = "TWO_ANTI_NUKE_PROTECTED_USER_IDS";
const CHANNEL_KEY: &str = "DISCORD_STAFF_ALERT_CHANNEL_ID";
const HOT_KEYS: [&str; 4] = [WINDOW_KEY, MAX_AGE_KEY, HEAT_KEY, CHANNEL_KEY];

/// Discord audit-log action to legacy destructive weight. Unsupported actions
/// never acquire a weight and are skipped before any claim.
pub fn map_action(action: &AuditLogEventType) -> Option<DestructiveAction> {
    match action {
        AuditLogEventType::MemberKick => Some(DestructiveAction::MemberKick),
        AuditLogEventType::MemberBanAdd => Some(DestructiveAction::MemberBan),
        AuditLogEventType::ChannelDelete => Some(DestructiveAction::ChannelDelete),
        AuditLogEventType::RoleDelete => Some(DestructiveAction::RoleDelete),
        AuditLogEventType::WebhookCreate => Some(DestructiveAction::WebhookCreate),
        AuditLogEventType::WebhookUpdate => Some(DestructiveAction::WebhookUpdate),
        AuditLogEventType::WebhookDelete => Some(DestructiveAction::WebhookDelete),
        _ => None,
    }
}

/// The containment configuration for one observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainmentSettings {
    pub window_ms: i64,
    pub max_age_ms: i64,
    pub heat_threshold: u64,
    /// `None` means log only: no channel is guessed on a live server.
    pub staff_channel: Option<String>,
}

/// Key-only diagnostic: malformed configuration refuses the whole observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainmentTuningRefusal {
    key: String,
}

impl ContainmentTuningRefusal {
    fn new(key: &str) -> Self {
        Self {
            key: key.to_owned(),
        }
    }

    pub fn key(&self) -> &str {
        &self.key
    }
}

impl std::fmt::Display for ContainmentTuningRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "containment tuning refused: {} present but unusable",
            self.key
        )
    }
}

impl std::error::Error for ContainmentTuningRefusal {}

impl ContainmentSettings {
    /// Live reader: absent/empty keys retain defaults; malformed values refuse
    /// the whole read. No input value is retained in the diagnostic.
    pub fn try_from_vars(vars: &HashMap<String, String>) -> Result<Self, ContainmentTuningRefusal> {
        let seconds = |key: &str, default_ms: i64| match vars.get(key).map(|raw| raw.trim()) {
            None | Some("") => Ok(default_ms),
            Some(raw) => raw
                .parse::<f64>()
                .ok()
                .and_then(seconds_to_ms)
                .ok_or_else(|| ContainmentTuningRefusal::new(key)),
        };
        let window_ms = seconds(WINDOW_KEY, DEFAULT_CONTAINMENT_WINDOW_MS)?;
        let max_age_ms = seconds(MAX_AGE_KEY, DEFAULT_CONTAINMENT_MAX_AGE_MS)?;
        let heat_threshold = match vars.get(HEAT_KEY).map(|raw| raw.trim()) {
            None | Some("") => DEFAULT_CONTAINMENT_HEAT_THRESHOLD,
            Some(raw) => match raw.parse::<u64>() {
                Ok(value) if value > 0 => value,
                _ => return Err(ContainmentTuningRefusal::new(HEAT_KEY)),
            },
        };
        let staff_channel = match vars.get(CHANNEL_KEY).map(|raw| raw.trim()) {
            None | Some("") => None,
            Some(raw) if is_snowflake(raw) => Some(raw.to_owned()),
            Some(_) => return Err(ContainmentTuningRefusal::new(CHANNEL_KEY)),
        };
        Ok(Self {
            window_ms,
            max_age_ms,
            heat_threshold,
            staff_channel,
        })
    }

    /// Legacy compatibility reader, only for tests. Live callers must refuse
    /// malformed values rather than substitute defaults or log-only delivery.
    #[cfg(test)]
    pub fn from_vars(vars: &HashMap<String, String>) -> Self {
        let seconds = |key: &str, default_ms: i64| match vars.get(key).map(|raw| raw.trim()) {
            None | Some("") => default_ms,
            Some(raw) => match raw.parse::<f64>().ok().and_then(seconds_to_ms) {
                Some(ms) => ms,
                None => {
                    warn!(key, "containment setting unusable; using the default");
                    default_ms
                }
            },
        };
        let heat_threshold = match vars.get(HEAT_KEY).map(|raw| raw.trim()) {
            None | Some("") => DEFAULT_CONTAINMENT_HEAT_THRESHOLD,
            Some(raw) => match raw.parse::<u64>() {
                Ok(value) if value > 0 => value,
                _ => {
                    warn!(key = HEAT_KEY, "heat threshold unusable; using the default");
                    DEFAULT_CONTAINMENT_HEAT_THRESHOLD
                }
            },
        };
        let staff_channel = match vars.get(CHANNEL_KEY).map(|raw| raw.trim()) {
            None | Some("") => None,
            Some(raw) if is_snowflake(raw) => Some(raw.to_owned()),
            Some(_) => {
                warn!(key = CHANNEL_KEY, "staff alert channel unusable; log only");
                None
            }
        };
        Self {
            window_ms: seconds(WINDOW_KEY, DEFAULT_CONTAINMENT_WINDOW_MS),
            max_age_ms: seconds(MAX_AGE_KEY, DEFAULT_CONTAINMENT_MAX_AGE_MS),
            heat_threshold,
            staff_channel,
        }
    }
}

/// Whole positive i64 milliseconds; sub-millisecond, overflow and non-finite input give `None`.
fn seconds_to_ms(seconds: f64) -> Option<i64> {
    // Upper bound is 2^63: the first f64 that no longer fits in an i64.
    let ms = seconds * 1000.0;
    let upper = (1_u64 << 63) as f64;
    (1.0..upper).contains(&ms).then_some(ms as i64)
}

fn is_snowflake(value: &str) -> bool {
    (17..=20).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_digit())
}

/// Cold user lists: comma-separated snowflakes, read once at boot. One
/// malformed nonempty entry refuses the entire list; never return a partial set.
pub fn id_list(
    vars: &HashMap<String, String>,
    key: &str,
) -> Result<HashSet<String>, ContainmentTuningRefusal> {
    vars.get(key)
        .into_iter()
        .flat_map(|raw| raw.split(','))
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(|id| {
            if is_snowflake(id) {
                Ok(id.to_owned())
            } else {
                Err(ContainmentTuningRefusal::new(key))
            }
        })
        .collect()
}

/// Compatibility seam for callers supplying already-constructed settings.
/// Raw live configuration uses [`CheckedSettingsSource`] instead. `Sync` is
/// required because the worker is shared by reference across awaits.
pub trait SettingsSource: Send + Sync + 'static {
    fn current(&mut self) -> impl Future<Output = ContainmentSettings> + Send;
}

/// Live settings reads can refuse an observation before any durable claim.
pub trait CheckedSettingsSource: Send + Sync + 'static {
    fn try_current(
        &mut self,
    ) -> impl Future<Output = Result<ContainmentSettings, ContainmentTuningRefusal>> + Send;
}

impl<S: SettingsSource> CheckedSettingsSource for S {
    async fn try_current(&mut self) -> Result<ContainmentSettings, ContainmentTuningRefusal> {
        Ok(self.current().await)
    }
}

/// Tuning and the staff channel are store-first (hot rows win over the
/// deployment environment) and re-read at most every [`SETTINGS_MAX_AGE`]; a
/// failed refresh keeps the last outcome, including a refusal.
pub(crate) struct StoreSettings {
    pool: PgPool,
    guild_id: String,
    deployment: HashMap<String, String>,
    max_age: Duration,
    cache: Option<SettingsCache>,
    checked: Option<Instant>,
    last: Option<Result<ContainmentSettings, ContainmentTuningRefusal>>,
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

    async fn refresh(&mut self) -> Result<ContainmentSettings, ContainmentTuningRefusal> {
        let store = SettingsStore::new(&self.pool);
        let stale = self.cache.as_ref();
        let loaded = tokio::time::timeout(SETTINGS_READ_MAX, async {
            let (revision, rows) = store.poll_guild_marks(&self.guild_id).await?;
            if stale.is_some_and(|cache| !cache.needs_refresh(revision, rows)) {
                return Ok::<_, sqlx::Error>(None);
            }
            let snapshot = store.load_guild_snapshot(&self.guild_id).await?;
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
    ) -> Result<ContainmentSettings, ContainmentTuningRefusal> {
        match loaded {
            Ok(Some(cache)) => self.cache = Some(cache),
            Ok(None) => {}
            Err(()) => {
                warn!("containment settings refresh failed; keeping the last outcome");
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
                    .filter(|(key, _)| HOT_KEYS.contains(&key.as_str())),
            );
        }
        let settings = match ContainmentSettings::try_from_vars(&vars) {
            Ok(settings) => settings,
            Err(refusal) => {
                if !matches!(&self.last, Some(Err(previous)) if previous.key() == refusal.key()) {
                    error!(
                        key = refusal.key(),
                        "containment_tuning_refused: observations disabled"
                    );
                }
                self.last = Some(Err(refusal.clone()));
                return Err(refusal);
            }
        };
        if !matches!(&self.last, Some(Ok(previous)) if previous == &settings) {
            info!(
                window_ms = settings.window_ms,
                max_age_ms = settings.max_age_ms,
                heat_threshold = settings.heat_threshold,
                alert_target = settings
                    .staff_channel
                    .as_deref()
                    .unwrap_or("log only (DISCORD_STAFF_ALERT_CHANNEL_ID unset)"),
                "containment_tuning"
            );
        }
        self.last = Some(Ok(settings.clone()));
        Ok(settings)
    }
}

impl CheckedSettingsSource for StoreSettings {
    async fn try_current(&mut self) -> Result<ContainmentSettings, ContainmentTuningRefusal> {
        if let (Some(at), Some(last)) = (self.checked, &self.last) {
            if at.elapsed() < self.max_age {
                return last.clone();
            }
        }
        self.checked = Some(Instant::now());
        self.refresh().await
    }
}

fn processing_now_ms(fallback_ms: i64) -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|age| age.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(fallback_ms)
}

/// Fail closed on malformed fields rather than dropping individual roles.
/// The core planner additionally validates identities, hierarchy positions,
/// baseline presence and coverage of both fetched held-role sets before it
/// can produce any removals or an empty-plan success.
fn role_snapshot(body: &serde_json::Value) -> Option<Vec<ContainmentRole>> {
    let roles = body.as_array()?;
    roles
        .iter()
        .map(|role| {
            Some(ContainmentRole {
                id: role.get("id")?.as_str()?.to_owned(),
                position: role.get("position")?.as_i64()?,
                permissions: role.get("permissions")?.as_str()?.parse::<u64>().ok()?,
                managed: role.get("managed")?.as_bool()?,
            })
        })
        .collect()
}

struct ContainmentWorker<S> {
    guild_id: u64,
    settings: S,
    store: ContainmentStore,
    delivery: RaidDelivery,
    executor: ActionExecutor,
    trusted_user_ids: HashSet<String>,
    protected_user_ids: HashSet<String>,
    bot_user_id: String,
    armed: bool,
    entries: mpsc::Receiver<AuditLogObservation>,
    shutdown: Arc<crate::raid_runtime::WorkerShutdown>,
}

impl<S: CheckedSettingsSource> ContainmentWorker<S> {
    async fn run(mut self) {
        // Verified identity before any entry: the staging application check
        // fails closed, and the bot ID seeds the protected-executor fence.
        let verified = match self.executor.current_application_id().await {
            Ok(id) if id.to_string() == STAGING_BOT_APPLICATION_ID => true,
            Ok(id) => {
                error!(
                    application_id = id,
                    "containment_identity_refused: not staging"
                );
                false
            }
            Err(_) => {
                error!("containment_identity_refused: application unreadable");
                false
            }
        };
        let bot_id = match self.executor.current_bot_user_id().await {
            Ok(id) => Some(id.to_string()),
            Err(_) => {
                error!("containment_identity_refused: bot user unreadable");
                None
            }
        };
        let Some(bot) = bot_id else {
            return;
        };
        if !verified {
            return;
        }
        self.bot_user_id = bot;
        loop {
            tokio::select! {
                biased;
                () = self.shutdown.notified() => {
                    let dropped = crate::raid_runtime::drain_queued(&mut self.entries);
                    self.shutdown.note_dropped(dropped);
                    crate::raid_runtime::worker_shutdown_drain(self.shutdown.finished(), dropped);
                    return;
                }
                msg = self.entries.recv() => {
                    let Some(entry) = msg else {
                        // Unlatched sender-drop: every buffered entry already
                        // yielded above, so the legacy full drain exits here.
                        return;
                    };
                    // An internal failure must never end containment: log and carry on.
                    if AssertUnwindSafe(self.handle(entry))
                        .catch_unwind()
                        .await
                        .is_err()
                    {
                        error!("containment_failed: observation panicked");
                    }
                    self.shutdown.note_finished();
                }
            }
        }
    }

    async fn handle(&mut self, entry: AuditLogObservation) {
        // The shard serves one configured guild; other guilds are not watched.
        if entry.guild_id != self.guild_id {
            return;
        }
        let Some(action) = map_action(&entry.action) else {
            debug!("containment_skipped: unsupported audit action");
            return;
        };
        let Some(occurred_at_ms) =
            snowflake_to_date_ms(&entry.entry_id.to_string()).map(|ms| ms as i64)
        else {
            error!(
                entry_id = entry.entry_id,
                "containment_stale: no entry time"
            );
            return;
        };
        // State changes are logged at the source, not once per entry. Refuse
        // before building policy, claiming heat or attempting any Discord IO.
        let Ok(settings) = self.settings.try_current().await else {
            return;
        };
        let mut policy = match ContainmentPolicy::new(
            settings.window_ms,
            settings.max_age_ms,
            settings.heat_threshold,
        ) {
            Ok(policy) => policy,
            Err(_) => {
                error!("containment_policy_rejected: live tuning unusable");
                return;
            }
        };
        policy.bot_user_id = Some(self.bot_user_id.clone());
        policy.trusted_user_ids = self.trusted_user_ids.clone();
        policy.protected_user_ids = self.protected_user_ids.clone();
        let event = DestructiveAuditEvent {
            audit_entry_id: entry.entry_id.to_string(),
            guild_id: entry.guild_id.to_string(),
            executor_id: entry.executor_id.map(|id| id.to_string()),
            action,
            target_id: entry.target_id.map(|id| id.to_string()),
            occurred_at_ms: Some(occurred_at_ms),
        };
        let now_ms = processing_now_ms(occurred_at_ms);
        let claimed = match self.store.claim_event(&policy, &event, now_ms).await {
            Ok(EventClaim::Claimed {
                event,
                disposition,
                heat,
            }) => {
                log_disposition(&disposition);
                (event, heat)
            }
            Ok(EventClaim::Duplicate(_)) => {
                // Already claimed: a replay must not recount or re-alert.
                debug!(entry_id = entry.entry_id, "containment_duplicate");
                return;
            }
            Err(_) => {
                error!("containment_store_failed: claim not recorded");
                return;
            }
        };
        let (trigger, heat) = claimed;
        if heat < policy.heat_threshold() {
            return;
        }
        let incident = match self
            .store
            .begin_incident(&policy, &trigger, heat, now_ms)
            .await
        {
            Ok(IncidentClaim::Started(incident)) => incident,
            Ok(IncidentClaim::Blocked { incident_id, state }) => {
                // A same-guild/executor incident still blocks: log the
                // suppression proof, never a second alert.
                error!(
                    incident_id = %incident_id,
                    state = ?state,
                    heat,
                    threshold = policy.heat_threshold(),
                    "containment_alert_suppressed"
                );
                return;
            }
            Ok(IncidentClaim::NotEligible) => return,
            Err(_) => {
                error!("containment_store_failed: incident not opened");
                return;
            }
        };
        self.contain(&settings, &event, &incident, heat, now_ms)
            .await;
    }

    /// Plan, execute (armed only) and report one started incident. The state
    /// follows the `docs/containment.md` decision contract via
    /// [`quarantine_outcome`]: partial success then failure is uncertain, an
    /// empty successful plan is contained, a first-removal timeout/429/5xx is
    /// uncertain, and definitive rejection with nothing removed is refused. A
    /// latched shutdown between steps interrupts the plan the same way a
    /// timeout does: the confirmed removals persist and the incident is
    /// uncertain (held, never released at cooldown expiry) rather than left
    /// stranded in `Containing` by a supervisor abort.
    async fn contain(
        &self,
        settings: &ContainmentSettings,
        event: &DestructiveAuditEvent,
        incident: &ContainmentIncident,
        heat: u64,
        now_ms: i64,
    ) {
        let dry_run = !self.armed;
        let threshold = settings.heat_threshold;
        let completion = match self
            .plan(&incident.guild_id, &incident.executor_id, dry_run)
            .await
        {
            PlanOutcome::DryRun => Completion {
                heat,
                threshold,
                outcome: ContainmentIncidentState::DryRun,
                removed: Vec::new(),
                now_ms,
            },
            PlanOutcome::Refused => Completion {
                heat,
                threshold,
                outcome: ContainmentIncidentState::Refused,
                removed: Vec::new(),
                now_ms,
            },
            PlanOutcome::Removed { removed, failure } => {
                let outcome = quarantine_outcome(&removed, failure);
                Completion {
                    heat,
                    threshold,
                    outcome,
                    removed,
                    now_ms,
                }
            }
            PlanOutcome::Failed => Completion {
                heat,
                threshold,
                outcome: ContainmentIncidentState::Failed,
                removed: Vec::new(),
                now_ms,
            },
        };
        self.finish(settings, event, incident, completion).await;
    }

    /// The plan's shutdown interrupt, checked before each Discord step. A
    /// disarmed plan removes nothing, so its end state is known and it still
    /// completes as a dry run (released at cooldown); an armed plan stopped
    /// before any removal is uncertain and held, never released.
    fn latched_plan(&self, dry_run: bool) -> Option<PlanOutcome> {
        self.shutdown.is_latched().then(|| {
            if dry_run {
                PlanOutcome::DryRun
            } else {
                PlanOutcome::Removed {
                    removed: Vec::new(),
                    failure: Some(QuarantineFailure::Shutdown),
                }
            }
        })
    }

    /// Snapshot roles and plan. Armed execution of removals happens in
    /// [`Self::contain`]; this only reads.
    async fn plan(&self, guild_id: &str, executor_id: &str, dry_run: bool) -> PlanOutcome {
        let guild: u64 = match guild_id.parse() {
            Ok(id) => id,
            Err(_) => {
                error!("containment_snapshot_failed: guild not a snowflake");
                return PlanOutcome::Failed;
            }
        };
        let executor: u64 = match executor_id.parse() {
            Ok(id) => id,
            Err(_) => {
                error!("containment_snapshot_failed: executor not a snowflake");
                return PlanOutcome::Failed;
            }
        };
        let bot: u64 = match self.bot_user_id.parse() {
            Ok(id) => id,
            Err(_) => {
                error!("containment_snapshot_failed: bot not a snowflake");
                return PlanOutcome::Failed;
            }
        };
        // A latched shutdown starts no new Discord step (checked before every
        // read and between removals), so the drain after the latch is one
        // in-flight call plus the outcome persist. Without it a slow-but-
        // healthy plan outlives the drain budget and the abort strands the
        // incident in `Containing`.
        if let Some(outcome) = self.latched_plan(dry_run) {
            return outcome;
        }
        let executor_roles = match self.executor.member_role_ids(guild, executor).await {
            Ok(roles) => roles,
            Err(_) => {
                error!("containment_snapshot_failed: executor roles unreadable");
                return PlanOutcome::Failed;
            }
        };
        if let Some(outcome) = self.latched_plan(dry_run) {
            return outcome;
        }
        let bot_roles = match self.executor.member_role_ids(guild, bot).await {
            Ok(roles) => roles,
            Err(_) => {
                error!("containment_snapshot_failed: bot roles unreadable");
                return PlanOutcome::Failed;
            }
        };
        if let Some(outcome) = self.latched_plan(dry_run) {
            return outcome;
        }
        let snapshot = match self
            .executor
            .get_json(&format!("/guilds/{guild}/roles"))
            .await
        {
            Ok(Some(body)) => match role_snapshot(&body) {
                Some(snapshot) => snapshot,
                None => {
                    error!("containment_snapshot_failed: guild roles malformed");
                    return PlanOutcome::Failed;
                }
            },
            Ok(None) | Err(_) => {
                error!("containment_snapshot_failed: guild roles unreadable");
                return PlanOutcome::Failed;
            }
        };
        match plan_quarantine(guild_id, &executor_roles, &bot_roles, &snapshot, dry_run) {
            QuarantinePlan::DryRun => PlanOutcome::DryRun,
            QuarantinePlan::Refused {
                reason: QuarantineRefusal::InvalidEvidence,
                ..
            } => {
                error!("containment_refused: role evidence incomplete or contradictory");
                PlanOutcome::Refused
            }
            QuarantinePlan::Refused {
                reason: QuarantineRefusal::ManagedRole | QuarantineRefusal::Hierarchy,
                ..
            } => {
                error!("containment_refused: hierarchy or managed role blocks removal");
                PlanOutcome::Refused
            }
            QuarantinePlan::RemoveRoles { role_ids } => {
                if !self.armed {
                    // Unreachable: `dry_run` is `!armed`, and a dry run never
                    // plans removals. Refuse loudly rather than execute.
                    error!("containment_refused: removals planned while disarmed");
                    return PlanOutcome::Failed;
                }
                let mut removed = Vec::new();
                let mut failure: Option<QuarantineFailure> = None;
                for role_id in role_ids {
                    // Stop between removals once latched: the confirmed
                    // removals persist and the incident is uncertain, instead
                    // of timing out mid-loop and stranding it in `Containing`.
                    if self.shutdown.is_latched() {
                        failure = Some(QuarantineFailure::Shutdown);
                        break;
                    }
                    match self
                        .executor
                        .set_member_role(
                            guild_id,
                            executor_id,
                            &role_id,
                            false,
                            "anti-nuke quarantine",
                        )
                        .await
                    {
                        Ok(()) => removed.push(role_id),
                        Err(error) if is_already_gone(&error) => removed.push(role_id),
                        Err(error) => {
                            error!(role_id = %role_id, "containment_removal_failed: stopping");
                            failure = Some(quarantine_failure(&error));
                            break;
                        }
                    }
                }
                PlanOutcome::Removed { removed, failure }
            }
        }
    }

    /// Persist the outcome and post the flag-only staff alert. A dropped
    /// completion surfaces as outcome-unknown (plus a metered store error),
    /// never as the unrecorded outcome posted as fact.
    async fn finish(
        &self,
        settings: &ContainmentSettings,
        event: &DestructiveAuditEvent,
        incident: &ContainmentIncident,
        mut completion: Completion,
    ) {
        if self
            .store
            .complete_incident(
                &incident.id,
                completion.outcome,
                &serde_json::json!({
                    "outcome": completion.outcome.outcome_label(),
                    "removed_role_ids": completion.removed,
                }),
                completion.now_ms,
            )
            .await
            .is_err()
        {
            error!(incident_id = %incident.id, "containment_store_failed: incident not completed");
            two_bot_core::metrics::global().containment_store_error("complete_incident");
            completion.outcome = ContainmentIncidentState::Uncertain;
        }
        self.alert(settings, event, incident, &completion).await;
    }

    /// Log first, then post the staff alert with empty allowed mentions.
    /// Never retried.
    async fn alert(
        &self,
        settings: &ContainmentSettings,
        event: &DestructiveAuditEvent,
        incident: &ContainmentIncident,
        completion: &Completion,
    ) {
        // Always log first. If the post fails the evidence still exists.
        error!(
            incident_id = %incident.id,
            executor_id = %incident.executor_id,
            action = event.action.as_str(),
            heat = completion.heat,
            threshold = completion.threshold,
            outcome = completion.outcome.outcome_label(),
            "containment_alert"
        );
        let Some(channel) = settings.staff_channel.as_deref() else {
            return;
        };
        let message = ContainmentAlert::from_trigger(
            event,
            completion.heat,
            completion.threshold,
            completion.outcome,
            completion.removed.clone(),
        )
        .staff_message();
        // The shared executor always sends empty allowed mentions; refuse a
        // proposal that asks for anything else rather than widen that boundary.
        if message.mentions != MentionPolicy::None {
            error!(channel, "containment_refused: mention policy is not empty");
            return;
        }
        match self
            .delivery
            .post_staff_message(channel, &message.content)
            .await
        {
            Delivery::Posted => info!(channel, incident_id = %incident.id, "containment_posted"),
            Delivery::Undeliverable => error!(
                channel,
                "containment_undeliverable: channel missing, not text, or bot lacks View/Send"
            ),
            Delivery::Failed => error!(channel, "containment_post_failed"),
        }
    }
}

enum PlanOutcome {
    DryRun,
    Refused,
    Removed {
        removed: Vec<String>,
        failure: Option<QuarantineFailure>,
    },
    Failed,
}

/// Resolved incident outcome with confirmed removals. One struct keeps
/// `finish`/`alert` under the `too_many_arguments` limit.
struct Completion {
    heat: u64,
    threshold: u64,
    outcome: ContainmentIncidentState,
    removed: Vec<String>,
    now_ms: i64,
}

/// The decision contract treats HTTP 404 on role removal as success: the role
/// is already gone. `set_member_role` only accepts 200/204, so a 404 surfaces
/// here as a rejection mentioning 404 and is reclaimed on this path.
fn is_already_gone(error: &DiscordError) -> bool {
    matches!(error, DiscordError::Rejected(detail) if detail.contains("404"))
}

/// Map the transport outcome onto the quarantine decision contract: timeouts,
/// unavailability and rate limits are uncertain; definitive rejections and
/// local guard refusals (which never reached the wire) are rejected.
fn quarantine_failure(error: &DiscordError) -> QuarantineFailure {
    match error {
        DiscordError::Timeout => QuarantineFailure::Timeout,
        DiscordError::Unavailable(_) => QuarantineFailure::Unavailable,
        DiscordError::RateLimited => QuarantineFailure::RateLimited,
        DiscordError::Rejected(_) | DiscordError::Guard(_) => QuarantineFailure::Rejected,
    }
}

fn log_disposition(disposition: &ContainmentDisposition) {
    match disposition.reason {
        ContainmentReason::Counted => {}
        _ => {
            debug!(
                state = ?disposition.state,
                reason = ?disposition.reason,
                "containment_rejected"
            );
        }
    }
}

/// The pipeline-facing half: queue only, never block.
pub struct ContainmentRuntime {
    entries: mpsc::Sender<AuditLogObservation>,
    shutdown: Arc<crate::raid_runtime::WorkerShutdown>,
}

impl ContainmentRuntime {
    /// Latch the bounded shutdown drain: the worker finishes its in-flight
    /// entry, drops the queued inbox with a count, logs
    /// `worker_shutdown_drain`, and exits so its handle joins.
    pub fn shutdown(&self) {
        self.shutdown.latch();
    }

    /// Dequeued entries handled to completion, caught panics included.
    #[cfg(test)]
    pub fn finished(&self) -> u64 {
        self.shutdown.finished()
    }

    /// Queued entries dropped without starting after a latched shutdown.
    #[cfg(test)]
    pub fn dropped_shutdown(&self) -> u64 {
        self.shutdown.dropped()
    }
}

impl AuditEntryObserver for ContainmentRuntime {
    fn observe_audit_entry(&self, entry: AuditLogObservation) {
        match self.entries.try_send(entry) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(entry)) => warn!(
                entry_id = entry.entry_id,
                "containment queue full; entry not claimed"
            ),
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }
}

/// Spawn the worker. [`ContainmentRuntime::shutdown`] latches the bounded
/// drain (finish the in-flight entry, drop the queue with a count); an
/// unlatched worker ends when the last [`ContainmentRuntime`] reference
/// drops, draining fully as before. Identity is verified inside `run`,
/// before any entry is claimed.
pub fn start<S: CheckedSettingsSource>(
    guild_id: u64,
    settings: S,
    store: ContainmentStore,
    executor: ActionExecutor,
    trusted_user_ids: HashSet<String>,
    protected_user_ids: HashSet<String>,
    armed: bool,
) -> (Arc<ContainmentRuntime>, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
    let shutdown = Arc::new(crate::raid_runtime::WorkerShutdown::default());
    let worker = ContainmentWorker {
        guild_id,
        settings,
        store,
        delivery: RaidDelivery::new(executor.clone(), guild_id),
        executor,
        trusted_user_ids,
        protected_user_ids,
        bot_user_id: String::new(),
        armed,
        entries: receiver,
        shutdown: Arc::clone(&shutdown),
    };
    (
        Arc::new(ContainmentRuntime {
            entries: sender,
            shutdown,
        }),
        tokio::spawn(worker.run()),
    )
}

/// Production wiring: the containment runtime only inside the anti-nuke
/// fences, else no audit-entry observer at all. Cold reads (`TWO_ANTI_NUKE`,
/// `TWO_ANTI_NUKE_DRY_RUN`, `TWO_ONBOARDING_MODE`, trusted/protected lists)
/// come from the deployment environment; Hot tuning comes from the settings
/// store. `armed` is false unless dry-run is explicitly `0` outside session
/// mode, and only an armed worker executes removals. The service shutdown
/// receiver latches the worker drain through a bounded supervisor that holds
/// the worker handle and joins-or-aborts it; the supervisor handle is
/// returned for the `main` shutdown join and must not be detached.
pub fn start_from_env(
    pool: PgPool,
    executor: ActionExecutor,
    guild_id: u64,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> Option<(Arc<dyn AuditEntryObserver>, JoinHandle<()>)> {
    start_from_vars(
        pool,
        executor,
        guild_id,
        &std::env::vars().collect(),
        shutdown,
    )
}

fn start_from_vars(
    pool: PgPool,
    executor: ActionExecutor,
    guild_id: u64,
    vars: &HashMap<String, String>,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> Option<(Arc<dyn AuditEntryObserver>, JoinHandle<()>)> {
    let fences = AntiNukeFences::from_vars(vars, guild_id);
    if !fences.enabled {
        info!("containment_disabled: no audit-entry observer without the fences");
        return None;
    }
    let lists = id_list(vars, TRUSTED_KEY)
        .and_then(|trusted| id_list(vars, PROTECTED_KEY).map(|protected| (trusted, protected)));
    let (trusted, protected) = match lists {
        Ok(lists) => lists,
        Err(refusal) => {
            error!(
                key = refusal.key(),
                "containment_tuning_refused: no audit-entry observer"
            );
            return None;
        }
    };
    info!(
        armed = fences.armed,
        "containment_fences: dry-run default; removals execute only when armed"
    );
    let deployment: HashMap<String, String> = HOT_KEYS
        .iter()
        .filter_map(|key| {
            vars.get(*key)
                .map(|value| ((*key).to_owned(), value.clone()))
        })
        .collect();
    let (runtime, worker) = start(
        guild_id,
        StoreSettings::new(pool.clone(), guild_id, deployment),
        ContainmentStore::from_pool(pool),
        executor,
        trusted,
        protected,
        fences.armed,
    );
    let supervisor = crate::raid_runtime::join_on_shutdown(
        worker,
        shutdown,
        crate::raid_runtime::WORKER_JOIN_BUDGET,
        {
            let runtime = Arc::clone(&runtime);
            move || runtime.shutdown()
        },
    );
    let observer: Arc<dyn AuditEntryObserver> = runtime;
    Some((observer, supervisor))
}

#[cfg(test)]
#[path = "containment_tuning_runtime_tests.rs"]
mod tuning_runtime_tests;

#[cfg(test)]
#[path = "containment_latch_tests.rs"]
mod latch_tests;
