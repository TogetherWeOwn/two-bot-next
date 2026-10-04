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
    DEFAULT_CONTAINMENT_HEAT_THRESHOLD, DEFAULT_CONTAINMENT_MAX_AGE_MS,
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
const SETTINGS_READ_MAX: Duration = Duration::from_millis(1500);

const WINDOW_KEY: &str = "TWO_ANTI_NUKE_WINDOW_SECONDS";
const MAX_AGE_KEY: &str = "TWO_ANTI_NUKE_EVENT_MAX_AGE_SECONDS";
const HEAT_KEY: &str = "TWO_ANTI_NUKE_HEAT_THRESHOLD";
const TRUSTED_KEY: &str = "TWO_ANTI_NUKE_TRUSTED_USER_IDS";
const PROTECTED_KEY: &str = "TWO_ANTI_NUKE_PROTECTED_USER_IDS";
const CHANNEL_KEY: &str = "DISCORD_STAFF_ALERT_CHANNEL_ID";
const HOT_KEYS: [&str; 3] = [WINDOW_KEY, MAX_AGE_KEY, HEAT_KEY];

/// Discord audit-log action to legacy destructive weight. Unsupported actions
/// never acquire a weight and are skipped before any claim.
pub(crate) fn map_action(action: &AuditLogEventType) -> Option<DestructiveAction> {
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
pub(crate) struct ContainmentSettings {
    pub(crate) window_ms: i64,
    pub(crate) max_age_ms: i64,
    pub(crate) heat_threshold: u64,
    /// `None` means log only: no channel is guessed on a live server.
    pub(crate) staff_channel: Option<String>,
}

impl ContainmentSettings {
    /// A missing or unusable number falls back to the shipped default rather
    /// than disabling containment. Present-but-bad values warn.
    pub(crate) fn from_vars(vars: &HashMap<String, String>) -> Self {
        let seconds = |key: &str, default_ms: i64| match vars.get(key).map(|raw| raw.trim()) {
            None | Some("") => default_ms,
            Some(raw) => match raw.parse::<f64>() {
                Ok(value) if value.is_finite() && value > 0.0 => (value * 1000.0) as i64,
                _ => {
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

fn is_snowflake(value: &str) -> bool {
    (17..=20).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_digit())
}

/// Cold user lists: comma-separated snowflakes, read once at boot.
pub(crate) fn id_list(vars: &HashMap<String, String>, key: &str) -> HashSet<String> {
    vars.get(key)
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Where the live [`ContainmentSettings`] come from. `Sync` is required: the
/// worker is shared by reference across awaits (`contain`, `plan`) and the
/// spawned [`run`](ContainmentWorker::run) future must stay `Send`.
pub(crate) trait SettingsSource: Send + Sync + 'static {
    fn current(&mut self) -> impl Future<Output = ContainmentSettings> + Send;
}

/// Tuning is store-first (hot rows win over the deployment environment) and
/// re-read at most every [`SETTINGS_MAX_AGE`]; a failed refresh keeps the last
/// good values. The staff channel is the deployment value read once at boot.
pub(crate) struct StoreSettings {
    pool: PgPool,
    guild_id: String,
    deployment: HashMap<String, String>,
    max_age: Duration,
    cache: Option<SettingsCache>,
    checked: Option<Instant>,
    last: Option<ContainmentSettings>,
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
                warn!("containment settings refresh failed; keeping the last good values");
            }
        }
    }
}

impl SettingsSource for StoreSettings {
    async fn current(&mut self) -> ContainmentSettings {
        if let (Some(at), Some(last)) = (self.checked, &self.last) {
            if at.elapsed() < self.max_age {
                return last.clone();
            }
        }
        self.checked = Some(Instant::now());
        self.refresh().await;
        let mut vars = self.deployment.clone();
        if let Some(cache) = &self.cache {
            // Only the three containment keys are live (`HOT_WIRED`); the
            // staff channel stays the boot-time deployment value.
            vars.extend(
                cache
                    .env_snapshot(Some(self.guild_id.as_str()))
                    .into_iter()
                    .filter(|(key, _)| HOT_KEYS.contains(&key.as_str())),
            );
        }
        let settings = ContainmentSettings::from_vars(&vars);
        if self.last.as_ref() != Some(&settings) {
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
        self.last = Some(settings.clone());
        settings
    }
}

fn processing_now_ms(fallback_ms: i64) -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|age| age.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(fallback_ms)
}

/// Fail closed: any malformed role (or a non-array body) refuses the whole
/// snapshot, so a dropped managed or higher-than-bot role can never let the
/// plan remove the others against the whole-plan preflight rule.
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
}

impl<S: SettingsSource> ContainmentWorker<S> {
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
        while let Some(entry) = self.entries.recv().await {
            // An internal failure must never end containment: log and carry on.
            if AssertUnwindSafe(self.handle(entry))
                .catch_unwind()
                .await
                .is_err()
            {
                error!("containment_failed: observation panicked");
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
        let settings = self.settings.current().await;
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
    /// uncertain, and definitive rejection with nothing removed is refused.
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
        let executor_roles = match self.executor.member_role_ids(guild, executor).await {
            Ok(roles) => roles,
            Err(_) => {
                error!("containment_snapshot_failed: executor roles unreadable");
                return PlanOutcome::Failed;
            }
        };
        let bot_roles = match self.executor.member_role_ids(guild, bot).await {
            Ok(roles) => roles,
            Err(_) => {
                error!("containment_snapshot_failed: bot roles unreadable");
                return PlanOutcome::Failed;
            }
        };
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
            QuarantinePlan::Refused { .. } => {
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

    /// Persist the outcome and post the flag-only staff alert.
    async fn finish(
        &self,
        settings: &ContainmentSettings,
        event: &DestructiveAuditEvent,
        incident: &ContainmentIncident,
        completion: Completion,
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
pub(crate) struct ContainmentRuntime {
    entries: mpsc::Sender<AuditLogObservation>,
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

/// Spawn the worker. It ends when the last [`ContainmentRuntime`] reference
/// drops. Identity is verified inside [`run`], before any entry is claimed.
pub(crate) fn start<S: SettingsSource>(
    guild_id: u64,
    settings: S,
    store: ContainmentStore,
    executor: ActionExecutor,
    trusted_user_ids: HashSet<String>,
    protected_user_ids: HashSet<String>,
    armed: bool,
) -> (Arc<ContainmentRuntime>, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
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
    };
    (
        Arc::new(ContainmentRuntime { entries: sender }),
        tokio::spawn(worker.run()),
    )
}

/// Production wiring: the containment runtime only inside the anti-nuke
/// fences, else no audit-entry observer at all. Cold reads (`TWO_ANTI_NUKE`,
/// `TWO_ANTI_NUKE_DRY_RUN`, `TWO_ONBOARDING_MODE`, trusted/protected lists)
/// come from the deployment environment; Hot tuning comes from the settings
/// store. `armed` is false unless dry-run is explicitly `0` outside session
/// mode, and only an armed worker executes removals.
pub(crate) fn start_from_env(
    pool: PgPool,
    executor: ActionExecutor,
    guild_id: u64,
) -> Option<Arc<dyn AuditEntryObserver>> {
    let vars: HashMap<String, String> = std::env::vars().collect();
    let fences = AntiNukeFences::from_vars(&vars, guild_id);
    if !fences.enabled {
        info!("containment_disabled: no audit-entry observer without the fences");
        return None;
    }
    info!(
        armed = fences.armed,
        "containment_fences: dry-run default; removals execute only when armed"
    );
    let mut deployment: HashMap<String, String> = HOT_KEYS
        .iter()
        .filter_map(|key| {
            vars.get(*key)
                .map(|value| ((*key).to_owned(), value.clone()))
        })
        .collect();
    if let Some(channel) = vars.get(CHANNEL_KEY) {
        deployment.insert(CHANNEL_KEY.to_owned(), channel.clone());
    }
    let (runtime, _worker) = start(
        guild_id,
        StoreSettings::new(pool.clone(), guild_id, deployment),
        ContainmentStore::from_pool(pool),
        executor,
        id_list(&vars, TRUSTED_KEY),
        id_list(&vars, PROTECTED_KEY),
        fences.armed,
    );
    Some(runtime)
}
