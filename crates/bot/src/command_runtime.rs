//! Shared command runtime (TOG-11020; grew out of the S4 sticky runtime,
//! TOG-10309 — the slice of TOG-9809).
//!
//! This module is the ONE bot composition/dispatch owner: the shared
//! interaction router (TOG-10075) decides route/refusal for every slash
//! command the runtime serves, the shared REST executor (TOG-10076) performs
//! every Discord side effect, and each feature's domain + sqlx store owns
//! its validation and mutation. No competing interaction listener, private
//! dispatcher, or Discord client exists: gateway events arrive via
//! [`CommandRuntime::dispatch`] and are handled entirely through those interfaces.
//!
//! Served slices:
//! - LFG (`/lfg`, `/lfg-close`, `two:lfg:` selects; TOG-10260), composed
//!   through `InteractionRuntime` over this same router and executor.
//! - sticky (`/sticky`, `/sticky-remove` + the accepted-message re-post hook;
//!   legacy order pinned below: claim → post → record → delete previous →
//!   audit, with `post_failed`/orphan cleanup on the failure edges).
//! - feed relays (`/feed-add`, `/feed-remove`, `/feed-pause`, `/feed-resume`,
//!   `/feed-list`, `/feed-inspect`; TOG-10085 domain and store): plan →
//!   guild-scoped CRUD → `announcements_audit_log` row → ephemeral completion.
//!   The generated relay/audit ids replace legacy `randomUUID()`. List and
//!   inspect are pure reads and write no audit row.
//! - channel moderation (`/purge`, `/slowmode`, `/lockdown`, `/unlock`): shared
//!   router authorization → ephemeral defer → durable claim/lane → REST effect →
//!   atomic result/audit → original-response edit. Ambiguity retains the lane.
//! - schedules (`/schedule`, `/schedule-remove`, `/schedule-list`; TOG-12237
//!   over the TOG-10081 domain and store): validate → guild-scoped CRUD →
//!   `automation_audit_log` row → ephemeral completion. The handlers live in
//!   [`crate::schedule_runtime`]; the runtime only registers and dispatches.
//! - leveling (`/rank [member]`, `/leaderboard`): defer first (ephemeral rank,
//!   public mention-suppressed top ten), then one completion edit; an
//!   acknowledged store failure completes with a sanitized unavailable reply.
//!   The ordered gateway award path shares this runtime's pool, executor and
//!   onboarding gates.
//! - discovery (`/help`): one immediate ephemeral callback rendered from the
//!   live publish set, grouped with permission hints; never deferred, never
//!   stored.
//!
//! Registry publication runs here too: every `Event::Ready` publishes the
//! router's ONE merged publish set (`set_guild_commands` is idempotent, so a
//! duplicate READY is a harmless repeat). A resumed process also synchronizes
//! once: a persisted gateway session does not preserve this process's gates
//! or command definitions. Custom-command publication loads persisted rows
//! through the same router and executor with one serialized full-set
//! publisher. Until the custom-command store is bootstrapped, publication
//! is deferred when automations enable dynamic commands.

use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, PoisonError, RwLock,
    },
};

use crate::member_runtime::MemberRuntime;
use crate::self_role_handlers::SelfRoleService;
use sqlx::{Pool, Postgres};
use tracing::warn;
use twilight_gateway::Event;
use twilight_model::{
    application::interaction::{application_command::CommandOptionValue, Interaction},
    channel::message::{Message, MessageFlags},
    gateway::GatewayReaction,
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
};
use two_bot::voice_rooms::voice_claims_custom_id;
use two_bot_core::{
    automod_runtime::FunnelDisposition,
    commands::PERM_MANAGE_GUILD,
    feeds::{
        feed_already_paused_text, feed_already_resumed_text, feed_existing_text, feed_list_page,
        feed_list_page_invalid_text, feed_list_page_underflow_text, feed_paused_text,
        feed_pending_evidence_refusal_text, feed_removed_text, feed_resumed_text, plan_command,
        FeedCommand, FeedCommandContext, FeedCommandPlan, FeedKind, FeedPendingEvidenceOperation,
    },
    feeds_store::{
        add_feed, list_feeds, pause_feed_with_evidence, remove_feed_with_evidence, resume_feed,
        write_audit, FeedAddOutcome, FeedAudit, FeedPauseEvidence, FeedRemovalEvidence,
        FeedResumeOutcome,
    },
    funnel::now_millis_for_test,
    help::{canonical_help_selector, unknown_help_branch_correction, unknown_help_correction},
    moderation::ModerationAction,
    router::replies::EXPIRED_COMPONENT_REPLY,
    sticky::{
        activity_eligible, decide_activity, sticky_removed_reply, sticky_set_reply, store,
        ActivityDecision, ActivityOutcome, PutSticky, RemoveOutcome, StickyAudit,
        StickyAuditAction, StickyAuditOutcome, StickySetup,
    },
    tickets::TicketAction,
    ChannelModerationStore, ComponentHandler, ComponentOutcome, FeatureGates, HandlerId,
    InteractionHandler, InteractionRouter, ModerationGates, RouterGates, RouterRefusal,
    SlashOutcome, SurfaceFlags,
};
use two_bot_discord::{
    help_detail_response, help_response, publish_commands, register_channel_handlers,
    response_for_slash, route_interaction, ActionExecutor, ChannelModerationRuntime, DiscordError,
    LevelingRuntime, RoutedInteraction,
};

use crate::activation::BootActivation;
use crate::db_log::{
    channel_response_class, db_class, discord_class, feed_store_class, leveling_class,
};
use crate::interaction_admission::{
    accepts_busy_reply, is_privileged, UserSlots, BUSY_REPLY, PER_USER_IN_FLIGHT,
};

/// Audit-log reason for retiring the previous sticky (legacy audits carry a
/// free-text reason; kept short — `audit_reason` caps at 512 chars).
const RETIRE_REASON: &str = "sticky re-post";
/// Reason for deleting a replacement post whose claim moved on.
const ORPHAN_REASON: &str = "sticky re-post rolled back";
/// Bounded diagnostic code logged when a re-post attempt leaves delivery
/// uncertainty behind (the store fence stays armed — no retry until an
/// administrator reconfigures). Fixed vocabulary; the log line carries ids
/// and cause tokens only, never the sticky body, tokens or member content.
const UNCERTAIN_DELIVERY_CODE: &str = "sticky_delivery_uncertain";
/// Recovery next action for [`UNCERTAIN_DELIVERY_CODE`]: inspect the channel
/// and resolve any possible orphan before explicitly resetting the sticky.
/// Never wait out the lease or blindly resend — the fence outlives both.
const UNCERTAIN_DELIVERY_NEXT_ACTION: &str = "inspect_channel_before_sticky_reset";
/// Reason for the `/sticky-remove` Discord cleanup delete.
const REMOVE_REASON: &str = "sticky-remove";
/// Safe reply when the store fails — never leak sqlx internals to Discord.
const STORE_FAILURE_REPLY: &str = "Sticky command failed; try again.";
/// Same shape for the feed slice's store failures.
const FEED_STORE_FAILURE_REPLY: &str = "Feed command failed; try again.";
/// Reply when the interaction arrives without a channel (pathological —
/// Discord always sends `channel_id` for guild slash commands).
const NO_CHANNEL_REPLY: &str = "This command only works in a channel.";
/// The merged registry includes builtins whose runtime slices have not landed.
const UNAVAILABLE_REPLY: &str = "This command is not available in this build yet.";

/// Router handler marker: this runtime is the `AutomationAdmin` executor for
/// the sticky commands. Registration documents the ownership the router
/// outcome names; execution happens in [`CommandRuntime::on_interaction`].
#[derive(Debug)]
struct StickyHandler;

impl InteractionHandler for StickyHandler {
    fn id(&self) -> HandlerId {
        HandlerId::AutomationAdmin
    }
}

/// Router handler marker for the feed and leveling slices; one per id.
#[derive(Debug)]
struct SliceHandler(HandlerId);

impl InteractionHandler for SliceHandler {
    fn id(&self) -> HandlerId {
        self.0
    }
}

/// Voice vote-kick delegate (V4 `kick` collision). After the moderation path
/// refuses a `/kick`, the router hands the interaction to the voice runtime,
/// which starts a room vote when the invoker shares a tracked room with the
/// target. `true` means voice answered the interaction itself, so the router
/// must not; `false` means voice sent no callback and the router answers.
/// Wired from the voice sink in `main`; `None` until then, which keeps every
/// `/kick` answered by the router alone.
pub type VoiceKickVote =
    Arc<dyn Fn(Interaction) -> Pin<Box<dyn Future<Output = bool> + Send + 'static>> + Send + Sync>;

/// Separate admission budgets prevent message bursts or registry pacing from
/// consuming interaction acknowledgement capacity. No queued/spawned waiters.
/// `PRIVILEGED` is reserved for permission-gated slash commands from members who
/// hold the permission; open interactions never enter it, so a burst of them
/// cannot starve moderation. `BUSY` carries the one-call "busy" replies.
/// `REACTIONS` keeps self-role reaction bursts off the message lane, so neither
/// burst can discard the other's automations.
pub(crate) const DISPATCH_LIMITS: [usize; 6] = [16, 16, 1, 8, 8, 8];
pub(crate) const LANE_MESSAGES: usize = 0;
const LANE_INTERACTIONS: usize = 1;
const LANE_REGISTRY: usize = 2;
const LANE_PRIVILEGED: usize = 3;
pub(crate) const LANE_BUSY: usize = 4;
pub(crate) const LANE_REACTIONS: usize = 5;

// A single member's reaction burst must leave room for other members: the
// per-member budget stays strictly below the lane cap, so fairness holds even
// before the saturation drop is reached.
const _: () = assert!(
    PER_USER_IN_FLIGHT < DISPATCH_LIMITS[LANE_REACTIONS],
    "per-member reaction budget must leave room for other members"
);

/// Seconds between saturation `warn!` lines per runtime (TOG-19878). The
/// drop counter increments on every event; the log samples the first drop in
/// each window so a burst is O(1) lines.
const SATURATION_LOG_WINDOW_SECS: u64 = 60;

#[derive(Default)]
struct DispatchTasks {
    stopped: bool,
    lanes: [Vec<tokio::task::AbortHandle>; DISPATCH_LIMITS.len()],
    /// Per-lane drops (test-visible mirror of the global
    /// `two_bot_dispatch_drops_total{lane}` counter, incremented alongside by
    /// `DispatchTasks::record_drop`; fairness drops share their lane's
    /// counter).
    drops: [u64; DISPATCH_LIMITS.len()],
    last_drop_log: Option<std::time::Instant>,
}

impl DispatchTasks {
    /// Record a refused event on `lanes`: increment each lane's drop counter
    /// (test-visible mirror of the global `two_bot_dispatch_drops_total{lane}`
    /// counter, incremented alongside) and emit at most one `warn!` per 60 s
    /// per runtime. Fairness drops share their lane's counter so every refused
    /// event stays observable through one path; each caller keeps its own
    /// sampled message so existing log filters keep matching.
    fn record_drop(&mut self, lanes: &[usize], reason: &'static str, message: &'static str) {
        for &lane in lanes {
            self.drops[lane] = self.drops[lane].saturating_add(1);
            two_bot_core::metrics::global()
                .dispatch_drop(two_bot_core::metrics::DISPATCH_LANES[lane]);
        }
        let now = std::time::Instant::now();
        let due = self
            .last_drop_log
            .is_none_or(|at| now.duration_since(at).as_secs() >= SATURATION_LOG_WINDOW_SECS);
        if due {
            self.last_drop_log = Some(now);
            warn!(?lanes, reason, "{message}");
        }
    }
}

// Lane labels stay in `DISPATCH_LIMITS` order with the global metric allowlist.
const _: () = assert!(two_bot_core::metrics::DISPATCH_LANES.len() == DISPATCH_LIMITS.len());

/// Cancels admitted work on gateway exit, including supervisor cancellation.
/// Interrupted channel effects retain their durable claims for reconciliation.
pub(crate) struct CommandDispatchGuard(Arc<CommandRuntime>);

impl Drop for CommandDispatchGuard {
    fn drop(&mut self) {
        let mut tasks = two_bot_core::metrics::recover_drop_poison(
            self.0.tasks.lock(),
            "command_dispatch_guard",
        );
        tasks.stopped = true;
        for lane in &mut tasks.lanes {
            for handle in lane.drain(..) {
                handle.abort();
            }
        }
    }
}

/// Callback target for a busy reply; token-bearing, so deliberately not Debug.
struct BusyTarget {
    interaction_id: u64,
    token: String,
}

/// The shared command runtime: one router + one REST executor + the sqlx
/// stores, driven by gateway dispatches. Bounded asynchronous work leaves
/// twilight free to poll heartbeats, and is cancelled when its shard exits.
/// LFG caps its own pool use (`LFG_MAX_IN_FLIGHT`).
pub struct CommandRuntime {
    pool: Pool<Postgres>,
    executor: ActionExecutor,
    interactions: two_bot_discord::interactions::InteractionRuntime,
    custom_commands: Option<Vec<two_bot_core::CustomCommand>>,
    /// Confirmed publication for the non-DB/fallback publisher. Production
    /// discovery reads the custom-command publisher's shared snapshot instead.
    published_commands: RwLock<Option<Arc<[two_bot_core::CommandDefinition]>>>,
    application_id: AtomicU64,
    /// Bootstrapped custom-command execution seam (dynamic dispatch, prefix
    /// triggers, serialized republication). Shares the interaction runtime's
    /// router, whose registrations are complete once built.
    gateway_commands: tokio::sync::OnceCell<crate::gateway_commands::GatewayCommands>,
    channel: ChannelModerationRuntime,
    tasks: Mutex<DispatchTasks>,
    giveaways: crate::giveaway_runtime::GiveawayRuntime,
    /// Interactions admitted per member; see [`crate::interaction_admission`].
    user_slots: UserSlots,
    /// Shared self-role surface; production boot stays parked pending acceptance.
    self_roles: Option<Arc<SelfRoleService>>,
    leveling: LevelingRuntime,
    /// Configured guild (`GUILD_ID`); also the router's guild fence.
    guild_id: u64,
    tickets: Option<Arc<crate::ticket_runtime::TicketRuntime>>,
    /// Shared member-moderation consumer; its presence opens the five member
    /// verbs. Absent while moderation is off or the guild is non-staging.
    member: Option<Arc<MemberRuntime>>,
    /// `TWO_AUTOMATIONS=1`: fast-path gate for the message hook (the router
    /// still answers `/sticky*` refusals when it is off).
    automations: bool,
    /// Serialize publication and remember a successful sync for this process.
    /// Repeated RESUMED events need no work; READY still replaces the full set.
    registry_synced: tokio::sync::Mutex<bool>,
    /// Monotonic attempt ids: one value mints both the DB claim token
    /// (`s{n:x}`, ≤25 chars) and the numeric post nonce for dedupe.
    attempts: AtomicU64,
    /// Voice vote-kick delegate (V4 `kick` collision). The router is the only
    /// answerer of `/kick`: moderation first, then this delegate for room
    /// occupants the moderation path refused.
    voice_kick_vote: Mutex<Option<VoiceKickVote>>,
}

// Each in-flight LFG execution holds up to two connections of the shared pool;
// the gateway checkpoint writer must still find one free.
const _: () = assert!(
    two_bot_discord::lfg_interactions::LFG_MAX_IN_FLIGHT * 2 < two_bot_store::DB_POOL_MAX as usize
);

/// Why an attempted registry sync did not publish. Kept detail-free: the
/// underlying REST errors are not logged.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RegistrySyncError {
    /// The merged command set failed to assemble.
    Invalid,
    /// RESUMED needed the application id and the lookup failed.
    ApplicationLookup,
    /// Discord refused or did not answer the bulk overwrite.
    Publish,
}

impl CommandRuntime {
    /// Compose all feature slices over the same router, executor and pool.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn build(
        pool: Pool<Postgres>,
        executor: ActionExecutor,
        router: InteractionRouter,
        self_roles: Option<Arc<SelfRoleService>>,
        leveling: LevelingRuntime,
        guild_id: u64,
        custom_commands: Option<Vec<two_bot_core::CustomCommand>>,
        tickets: Option<Arc<crate::ticket_runtime::TicketRuntime>>,
        member: Option<Arc<MemberRuntime>>,
    ) -> Arc<Self> {
        let automations = router.gates().automations;
        let mut router = router;
        // Registration documents the ownership the router outcome names; the
        // shared router routes member verbs by name table regardless.
        if member.is_some() {
            for action in [
                ModerationAction::Ban,
                ModerationAction::TempBan,
                ModerationAction::Kick,
                ModerationAction::Timeout,
                ModerationAction::Warn,
            ] {
                router.register(Box::new(SliceHandler(HandlerId::Moderation(action))));
            }
        }
        let interactions = two_bot_discord::interactions::InteractionRuntime::with_router(
            router,
            pool.clone(),
            executor.clone(),
            0,
            two_bot_core::ClassifierConfig::from_env(),
        )
        .with_classifier_provider(Arc::new(crate::community_classifier::LiveClassifier));
        let channel = ChannelModerationRuntime::new(
            ChannelModerationStore::from_pool(pool.clone()),
            executor.clone(),
        );
        let giveaways =
            crate::giveaway_runtime::GiveawayRuntime::new(pool.clone(), executor.clone());
        Arc::new(Self {
            giveaways,
            pool,
            executor,
            interactions,
            custom_commands,
            published_commands: RwLock::new(None),
            application_id: AtomicU64::new(0),
            gateway_commands: tokio::sync::OnceCell::new(),
            channel,
            tasks: Mutex::new(DispatchTasks::default()),
            user_slots: UserSlots::default(),
            self_roles,
            leveling,
            guild_id,
            tickets,
            member,
            automations,
            registry_synced: tokio::sync::Mutex::new(false),
            attempts: AtomicU64::new(now_millis_for_test().max(0) as u64),
            voice_kick_vote: Mutex::new(None),
        })
    }

    /// Test-only access to the shared interaction surface for identity assertions.
    #[cfg(test)]
    pub(crate) fn test_interactions(&self) -> &two_bot_discord::interactions::InteractionRuntime {
        &self.interactions
    }

    pub(crate) fn published_commands(&self) -> Option<Arc<[two_bot_core::CommandDefinition]>> {
        if let Some(custom) = self.gateway_commands.get() {
            return custom.published_commands();
        }
        self.published_commands
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn set_identity(&self, bot_user_id: u64, application_id: u64) {
        self.interactions.set_bot_user_id(bot_user_id);
        self.interactions.set_application_id(application_id);
        self.application_id.store(application_id, Ordering::Relaxed);
    }

    /// Build the runtime from process env gates + the optional
    /// `DISCORD_API_BASE` proxy override. Returns `None` (gateway still boots)
    /// when gate parsing or executor construction fails. `self_roles` is the
    /// boot-composed service shared with the recovery job; only its presence
    /// opens the self-role router surface. `member` is the boot-composed
    /// member-moderation consumer shared with the unban sweep; only its
    /// presence opens the five member verbs. `voice_live` is whether the voice
    /// sink actually built (not just `TWO_VOICE=1`): the voice commands publish
    /// only while something is there to answer them.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn from_env(
        pool: Pool<Postgres>,
        token: &str,
        guild_id: u64,
        self_roles: Option<Arc<SelfRoleService>>,
        onboarding: two_bot_core::OnboardingGates,
        member: Option<Arc<MemberRuntime>>,
        activation: &BootActivation,
        voice_live: bool,
    ) -> Option<Arc<Self>> {
        let features = match FeatureGates::from_env() {
            Ok(features) => features,
            Err(err) => {
                warn!(reason = %err, "feature gates invalid; command runtime disabled");
                return None;
            }
        };
        // A denied capability never reaches its feature-specific validation:
        // live/unknown identities must retain unrelated commands even with
        // stale moderation env. Permitted staging still validates every gate.
        let moderation =
            if activation.permitted(two_bot_core::activation::LiveCapability::Moderation) {
                match ModerationGates::from_env() {
                    Ok(moderation) => moderation,
                    Err(err) => {
                        warn!(reason = %err, "moderation gates invalid; command runtime disabled");
                        return None;
                    }
                }
            } else {
                ModerationGates {
                    enabled: false,
                    owen_user_id: String::new(),
                    protected_role_ids: Default::default(),
                }
            };
        let ticket_config = crate::ticket_runtime::TicketConfig::from_env(guild_id);
        let gates = RouterGates::from_slices(
            Some(guild_id),
            &features,
            &moderation,
            SurfaceFlags {
                scorecard: std::env::var("TWO_COMMUNITY_SCORECARD").is_ok_and(|value| value == "1"),
                voice: voice_live,
                voice_assistant: two_bot_core::voice_assistant::AssistantConfig::from_env()
                    .is_some(),
                session_picker: onboarding.mode == two_bot_core::OnboardingMode::Session,
                tickets: ticket_config.is_some(),
                self_roles: self_roles.is_some(),
                ..SurfaceFlags::default()
            },
        );
        let proxy = std::env::var("DISCORD_API_BASE")
            .ok()
            .filter(|value| !value.is_empty());
        let admission =
            match two_bot_core::send_admission::PgSendAdmission::new(pool.clone(), token) {
                Ok(admission) => Arc::new(admission),
                Err(err) => {
                    warn!(reason = %err, "send admission invalid; command runtime disabled");
                    return None;
                }
            };
        let executor = match ActionExecutor::with_admission(token.to_owned(), proxy, admission) {
            Ok(executor) => executor,
            Err(err) => {
                warn!(reason = %err, "REST executor failed to build; command runtime disabled");
                return None;
            }
        };
        // A refused tickets capability never constructs its runtime, so a
        // live identity gets no supervisor, no component execution, and a
        // narrowed router gate; unrelated commands keep working.
        let tickets = if activation.permitted(two_bot_core::activation::LiveCapability::Tickets) {
            match ticket_config {
                Some(config) => match crate::ticket_runtime::TicketRuntime::new(
                    pool.clone(),
                    executor.clone(),
                    config,
                    std::env::vars()
                        .filter(|(key, _)| crate::ticket_runtime::LIVE_KEYS.contains(&key.as_str()))
                        .collect(),
                ) {
                    Ok(runtime) => Some(Arc::new(runtime)),
                    Err(_) => return None,
                },
                None => None,
            }
        } else {
            None
        };
        Some(Self::from_gates(
            pool, executor, gates, self_roles, tickets, member, onboarding, activation,
        ))
    }

    /// Composition seam shared by boot and mock-Discord tests. Only narrowed
    /// gates reach registration, publication and the accepted-message hook.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_gates(
        pool: Pool<Postgres>,
        executor: ActionExecutor,
        gates: RouterGates,
        self_roles: Option<Arc<SelfRoleService>>,
        tickets: Option<Arc<crate::ticket_runtime::TicketRuntime>>,
        member: Option<Arc<MemberRuntime>>,
        onboarding: two_bot_core::OnboardingGates,
        activation: &BootActivation,
    ) -> Arc<Self> {
        let gates = activation.constrain_router(gates);
        // The service only serves an open, permitted surface.
        let self_roles = self_roles.filter(|_| gates.self_roles);
        // A refused tickets capability keeps no runtime even when the caller
        // composed one: no supervisor, no component execution, narrowed gate.
        let tickets = tickets.filter(|_| gates.tickets);
        let guild_id = gates
            .configured_guild
            .expect("boot supplies configured guild");
        // The ordered leveling path shares this runtime's executor/pacing.
        let leveling = LevelingRuntime::new(
            pool.clone(),
            Arc::new(executor.clone()),
            guild_id,
            onboarding,
        );
        // No custom-command store exists on main yet: an empty slice is the
        // authoritative baseline (same as before LFG).
        let runtime = Self::build(
            pool,
            executor,
            router_with_commands(gates),
            self_roles,
            leveling,
            guild_id,
            Some(Vec::new()),
            tickets,
            member,
        );
        // Pin the token-derived application identity before any READY arming:
        // a READY-supplied id stays untrusted until this boot pin confirms it
        // in `publish_registry_checked`. Zero means unpinned (unparseable
        // token); such a runtime never publishes on faith.
        if let Some(application_id) = activation.application_id() {
            runtime.interactions.set_application_id(application_id);
            runtime
                .application_id
                .store(application_id, Ordering::Relaxed);
        }
        runtime
    }

    #[cfg(test)]
    pub(crate) fn router(&self) -> &InteractionRouter {
        &self.interactions.router
    }

    /// Bootstrap before constructing the shard, reusing this runtime's router,
    /// pool and executor (including its proxy and shared pacing state).
    pub(crate) async fn initialize_custom_commands(
        &self,
        config: crate::gateway_commands::GatewayCommandConfig,
    ) -> Result<(), sqlx::Error> {
        self.gateway_commands
            .get_or_try_init(|| {
                crate::gateway_commands::GatewayCommands::bootstrap_with_router(
                    self.pool.clone(),
                    self.executor.clone(),
                    config,
                    Arc::clone(&self.interactions.router),
                )
            })
            .await
            .map(|_| ())
    }

    #[cfg(test)]
    pub(crate) fn initialize_custom_commands_for_test(
        &self,
        config: crate::gateway_commands::GatewayCommandConfig,
    ) {
        let commands = crate::gateway_commands::GatewayCommands::for_test(
            self.pool.clone(),
            self.executor.clone(),
            config,
            Arc::clone(&self.interactions.router),
        );
        assert!(self.gateway_commands.set(commands).is_ok());
    }

    /// Test constructor: skips env gate reads so tests inject their own
    /// router, executor (mock REST), and automations flag directly.
    #[cfg(test)]
    pub(crate) fn new(
        pool: Pool<Postgres>,
        executor: ActionExecutor,
        router: InteractionRouter,
        guild_id: u64,
        automations: bool,
    ) -> Arc<Self> {
        Self::new_with_optional_member(pool, executor, router, guild_id, automations, None)
    }

    /// Test constructor with the member-moderation consumer attached.
    #[cfg(test)]
    pub(crate) fn new_with_member(
        pool: Pool<Postgres>,
        executor: ActionExecutor,
        router: InteractionRouter,
        guild_id: u64,
        automations: bool,
        member: Arc<MemberRuntime>,
    ) -> Arc<Self> {
        Self::new_with_optional_member(pool, executor, router, guild_id, automations, Some(member))
    }

    #[cfg(test)]
    fn new_with_optional_member(
        pool: Pool<Postgres>,
        executor: ActionExecutor,
        router: InteractionRouter,
        guild_id: u64,
        automations: bool,
        member: Option<Arc<MemberRuntime>>,
    ) -> Arc<Self> {
        assert_eq!(automations, router.gates().automations);
        let leveling = LevelingRuntime::new(
            pool.clone(),
            Arc::new(executor.clone()),
            guild_id,
            two_bot_core::OnboardingGates {
                mode: two_bot_core::OnboardingMode::Legacy,
                dry_run: false,
            },
        );
        // Tests provide an authoritative empty custom-command fixture.
        let runtime = Self::build(
            pool,
            executor,
            router,
            None,
            leveling,
            guild_id,
            Some(Vec::new()),
            None,
            member,
        );
        // Tests pin the mock-Discord application identity (1111), like a boot
        // whose token parses to it, so READY/RESUMED trust flows exercise
        // the pin. The bot user stays unknown (0), so nonce recovery still
        // resolves it through the executor exactly as on main.
        runtime.interactions.set_application_id(1111);
        runtime.application_id.store(1111, Ordering::Relaxed);
        runtime
    }

    /// Dispatch alongside ordered RSVP execution. That path owns RSVP replies
    /// (including refusals) and publishes the full registry before polling.
    /// Keep the standalone dispatch fallback for gateway callers without it.
    pub(crate) fn dispatch_remaining(self: &Arc<Self>, event: &Event) {
        match event {
            Event::Ready(ready) => {
                // The ordered surface owns registry publication; identity still
                // initializes here so LFG keeps READY's ids without a REST read.
                // A READY-supplied id never overwrites the boot pin on faith.
                let ready_application = ready.application.id.get();
                let pinned = self.application_id.load(Ordering::Relaxed);
                if pinned == 0 || pinned == ready_application {
                    self.set_identity(ready.user.id.get(), ready_application);
                } else {
                    warn!(
                        application_id = ready_application,
                        "READY identity differs from boot token; identity not armed"
                    );
                }
                self.dispatch_ticket_connection(event);
                return;
            }
            Event::Resumed => {
                self.dispatch_ticket_connection(event);
                return;
            }
            Event::InteractionCreate(interaction)
                if self
                    .executor
                    .command_identities()
                    .slash_name(&interaction.0)
                    .is_some_and(|name| {
                        matches!(name.as_str(), "rsvp" | "rsvp-attendance" | "attendance")
                    }) =>
            {
                return;
            }
            _ => {}
        }
        self.dispatch(event);
    }

    pub(crate) fn dispatch_guard(self: &Arc<Self>) -> CommandDispatchGuard {
        CommandDispatchGuard(Arc::clone(self))
    }

    /// The one process executor (admission lane and pacing included); other
    /// runtimes (onboarding, automod) and the ordered interaction surface
    /// render through a clone, never a private client. One token key, one
    /// pacing lane, one governed admission for both surfaces.
    pub fn executor(&self) -> ActionExecutor {
        self.executor.clone()
    }

    fn spawn(
        &self,
        lane: usize,
        work: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> bool {
        self.spawn_first(&[lane], work)
    }

    /// Admit `work` into the first of `lanes` that has room. A drop with
    /// every attempted lane full increments each attempted lane's saturation
    /// counter (single-lane calls stay 1:1; the privileged spill pair counts
    /// both lanes) and emits at most one `warn!` per 60 s per runtime.
    fn spawn_first(
        &self,
        lanes: &[usize],
        work: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> bool {
        let mut tasks = self.tasks.lock().expect("command task scope");
        if tasks.stopped {
            return false;
        }
        for &lane in lanes {
            let handles = &mut tasks.lanes[lane];
            handles.retain(|handle| !handle.is_finished());
            if handles.len() < DISPATCH_LIMITS[lane] {
                let task = tokio::spawn(work);
                handles.push(task.abort_handle());
                return true;
            }
        }
        tasks.record_drop(
            lanes,
            "lane saturated",
            "command dispatch saturated; event not admitted (sampled; see two_bot_dispatch_drops_total)",
        );
        false
    }

    /// Admit one non-ticket interaction. A member at their cap, or a full lane,
    /// gets an ephemeral busy reply rather than silence. Privileged commands try
    /// the reserved lane first and may spill into the shared one.
    fn admit_interaction(self: &Arc<Self>, interaction: Interaction) -> bool {
        let busy = self.busy_target(&interaction);
        let slot = match interaction.author_id() {
            Some(user) => match self.user_slots.acquire(user.get()) {
                Some(slot) => Some(slot),
                None => {
                    self.reply_busy(busy);
                    return false;
                }
            },
            None => None,
        };
        let lanes: &[usize] = if is_privileged(&interaction) {
            &[LANE_PRIVILEGED, LANE_INTERACTIONS]
        } else {
            &[LANE_INTERACTIONS]
        };
        let runtime = Arc::clone(self);
        let admitted = self.spawn_first(lanes, async move {
            let _slot = slot;
            let handled = match runtime.gateway_commands.get() {
                Some(custom) => custom.handle_interaction(&interaction).await,
                None => false,
            };
            if !handled {
                runtime.on_interaction(&interaction).await;
            }
        });
        if !admitted {
            self.reply_busy(busy);
        }
        admitted
    }

    /// Where to send the busy reply, for interactions that accept one.
    fn busy_target(&self, interaction: &Interaction) -> Option<BusyTarget> {
        let known = self.application_id.load(Ordering::Relaxed);
        if !accepts_busy_reply(interaction)
            || (known != 0 && interaction.application_id.get() != known)
        {
            return None;
        }
        Some(BusyTarget {
            interaction_id: interaction.id.get(),
            token: interaction.token.clone(),
        })
    }

    /// Tell the invoker to retry: one callback on a small dedicated lane. When
    /// that lane is saturated too, the event is dropped with only a log line.
    fn reply_busy(self: &Arc<Self>, target: Option<BusyTarget>) {
        let Some(target) = target else {
            return;
        };
        let runtime = Arc::clone(self);
        self.spawn(LANE_BUSY, async move {
            if let Err(error) = runtime
                .executor
                .answer_interaction(target.interaction_id, &target.token, &ephemeral(BUSY_REPLY))
                .await
            {
                warn!(
                    interaction_id = target.interaction_id.to_string(),
                    reason = discord_class(&error),
                    "busy reply failed"
                );
            }
        });
    }

    /// Shares this runtime's pool, executor/pacing and onboarding gates with
    /// the ordered award path; only this runtime dispatches interactions.
    pub fn leveling(&self) -> LevelingRuntime {
        self.leveling.clone()
    }

    /// Wire the voice vote-kick delegate after boot composes both runtimes.
    /// Called once from `main`; the `None` default keeps router behavior.
    pub fn set_voice_kick_vote(&self, vote: VoiceKickVote) {
        *self.voice_kick_vote.lock().expect("voice vote lock") = Some(vote);
    }

    #[cfg(test)]
    pub(crate) fn new_with_self_roles(
        pool: Pool<Postgres>,
        executor: ActionExecutor,
        router: InteractionRouter,
        guild_id: u64,
        automations: bool,
        service: Arc<SelfRoleService>,
    ) -> Arc<Self> {
        let mut runtime = Self::new(pool, executor, router, guild_id, automations);
        Arc::get_mut(&mut runtime)
            .expect("new runtime is unshared")
            .self_roles = Some(service);
        runtime
    }

    /// Test-only lane depth: prunes finished tasks, then reports the live
    /// count for `lane`. Reactions run on `LANE_REACTIONS`; the bound test
    /// asserts this never exceeds `DISPATCH_LIMITS[LANE_REACTIONS]`.
    #[cfg(test)]
    pub(crate) fn lane_in_flight(&self, lane: usize) -> usize {
        let mut tasks = self.tasks.lock().expect("command task scope");
        tasks.lanes[lane].retain(|handle| !handle.is_finished());
        tasks.lanes[lane].len()
    }

    /// Test-only lane occupant: holds one slot for `duration` so the bound
    /// test can saturate the lane without database or REST work.
    #[cfg(test)]
    pub(crate) fn hold_lane_for_test(&self, lane: usize, duration: std::time::Duration) -> bool {
        self.spawn(lane, async move {
            tokio::time::sleep(duration).await;
        })
    }

    /// Test-only per-member occupancy: claims one [`PER_USER_IN_FLIGHT`] slot
    /// for `user` without touching any lane, so the fairness test can pin a
    /// single-member burst deterministically instead of racing task settle.
    #[cfg(test)]
    pub(crate) fn acquire_user_slot_for_test(
        &self,
        user: u64,
    ) -> Option<crate::interaction_admission::UserSlot> {
        self.user_slots.acquire(user)
    }

    /// Test-only saturation count for `lane`: every `spawn_first` drop
    /// increments it alongside the global metric, so the pin stays
    /// deterministic under parallel suites that share the global registry.
    #[cfg(test)]
    pub(crate) fn dispatch_drops_total(&self, lane: usize) -> u64 {
        self.tasks.lock().expect("command task scope").drops[lane]
    }

    #[cfg(test)]
    pub(crate) fn with_tickets(
        pool: Pool<Postgres>,
        executor: ActionExecutor,
        tickets: Arc<crate::ticket_runtime::TicketRuntime>,
    ) -> Arc<Self> {
        let router = InteractionRouter::new(RouterGates {
            configured_guild: Some(100),
            tickets: true,
            scorecard: false,
            automations: false,
            announcements: false,
            moderation: false,
            voice: false,
            voice_assistant: false,
            self_roles: false,
            onboarding_picker: false,
            session_picker: false,
        });
        let mut runtime = Self::new(pool, executor, router, 100, false);
        {
            let unshared = Arc::get_mut(&mut runtime).expect("unshared test runtime");
            unshared.tickets = Some(tickets);
            // No boot token pins this constructor: registry publication may
            // resolve the application id through the token-authenticated
            // lookup, exactly like a boot whose token never parses.
            unshared.application_id.store(0, Ordering::Relaxed);
        }
        runtime
    }

    /// Isolate ticket gateway acceptance from unrelated command publication.
    #[cfg(test)]
    pub(crate) async fn suppress_registry_for_test(&self) {
        *self.registry_synced.lock().await = true;
    }

    pub(crate) fn start_tickets(&self) -> Option<crate::ticket_runtime::TicketSupervisor> {
        self.tickets.as_ref().and_then(|tickets| tickets.start())
    }

    fn voice_owns_interaction(&self, interaction: &Interaction) -> bool {
        let application_id = self.application_id.load(Ordering::Relaxed);
        if application_id != 0 && interaction.application_id.get() != application_id {
            return false;
        }
        self.executor
            .command_identities()
            .slash_name(interaction)
            .is_some_and(|name| self.interactions.router.voice_owns_command(&name))
    }

    /// Admit before spawning, without waiting on SQL/REST in the shard loop.
    /// Saturated lanes drop events without effects, queued waiters or tokens in
    /// logs. READY/RESUMED publication has independent capacity; overlapping
    /// connection events coalesce while the full registry is being synchronized.
    /// Custom commands answer first and report ownership, so sticky/feed
    /// routing never sends a second response after an acknowledgement.
    pub fn dispatch(self: &Arc<Self>, event: &Event) -> bool {
        self.dispatch_with_verdict(event, None)
    }

    /// Record a refused prefix candidate without dispatching the message.
    /// Called by the ordered gateway worker when its capture-only gate stops a
    /// create before the detached handler can account for the refusal.
    pub(crate) fn record_refused_prefix_trigger(&self, event: &Event) {
        if let Event::MessageCreate(message) = event {
            if let Some(custom) = self.gateway_commands.get() {
                custom.record_refused_prefix_trigger(&message.0);
            }
        }
    }

    /// Message dispatch with the automod verdict for this create. `None` keeps
    /// the configured acceptance (the `TWO_AUTOMOD=0` fast path); the gateway
    /// worker passes its verdict once automod has decided, and the trigger
    /// handler fails a missing verdict closed.
    pub fn dispatch_with_verdict(
        self: &Arc<Self>,
        event: &Event,
        verdict: Option<FunnelDisposition>,
    ) -> bool {
        if let Some(custom) = self.gateway_commands.get() {
            custom.observe(event);
        }
        match event {
            Event::MessageCreate(message) => {
                if !self.automations {
                    return false;
                }
                let runtime = Arc::clone(self);
                let message = message.0.clone();
                self.spawn(LANE_MESSAGES, async move {
                    if let Some(custom) = runtime.gateway_commands.get() {
                        custom.handle_message(&message, verdict).await;
                    }
                    runtime.on_message(&message).await;
                })
            }
            Event::InteractionCreate(interaction) => {
                let Some(interaction) = self
                    .executor
                    .command_identities()
                    .resolve_interaction(&interaction.0)
                else {
                    return true;
                };
                // The gateway also delivers these to their feature owners. Yield
                // before user-slot/lane admission so overload cannot race the
                // voice, onboarding preview or reopen defer with the shared
                // busy callback. Preview picker clicks yield the same way:
                // onboarding owns their defer and guidance, so a saturated
                // shared lane must not race it with a competing busy callback.
                if self.voice_owns_interaction(&interaction)
                    || crate::onboarding::owns_welcome_preview(&interaction)
                    || crate::onboarding::is_preview_control(&interaction)
                    || matches!(
                        route_interaction(&self.interactions.router, &interaction, None),
                        RoutedInteraction::Slash {
                            outcome: SlashOutcome::Handled {
                                handler: HandlerId::Reopen
                            },
                            ..
                        }
                    )
                {
                    return true;
                }
                let runtime = Arc::clone(self);
                let interaction = interaction.into_owned();
                let is_ticket = matches!(
                    interaction.data.as_ref(),
                    Some(twilight_model::application::interaction::InteractionData::MessageComponent(component))
                        if TicketAction::from_custom_id(&component.custom_id).is_some()
                );
                if let Some(tickets) = self.tickets.as_ref().filter(|_| is_ticket) {
                    // Ticket components never entered the shared
                    // user-slot/lane admission: the ticket runtime admits
                    // synchronously before spawning, and overload gets the
                    // same one-call ephemeral busy reply as the shared lanes
                    // instead of silence. The busy target is captured before
                    // the handler moves the interaction.
                    let busy = self.busy_target(&interaction);
                    let interaction_id = interaction.id.get();
                    let user = interaction.author_id().map(|id| id.get());
                    let admitted = tickets.try_spawn_component(user, |permit| async move {
                        let _permit = permit;
                        runtime.on_interaction(&interaction).await;
                    });
                    if !admitted {
                        tickets.record_component_saturation(interaction_id);
                        self.reply_busy(busy);
                    }
                    admitted
                } else {
                    self.admit_interaction(interaction)
                }
            }
            Event::ReactionAdd(reaction) => self.dispatch_self_role_reaction(&reaction.0, false),
            Event::ReactionRemove(reaction) => self.dispatch_self_role_reaction(&reaction.0, true),
            Event::Ready(ready) => {
                // A READY-supplied application id never overwrites the boot
                // pin on faith: arm identity only when the token-derived pin
                // confirms it (or no pin exists). Publication re-checks this
                // in `publish_registry_checked`.
                let ready_application = ready.application.id.get();
                let pinned = self.application_id.load(Ordering::Relaxed);
                if pinned == 0 || pinned == ready_application {
                    self.set_identity(ready.user.id.get(), ready_application);
                } else {
                    warn!(
                        application_id = ready_application,
                        "READY identity differs from boot token; identity not armed"
                    );
                }
                if let Some(tickets) = &self.tickets {
                    tickets.on_ready(ready.user.id.get());
                }
                let runtime = Arc::clone(self);
                let application_id = ready.application.id.get();
                self.spawn(LANE_REGISTRY, async move {
                    runtime.publish_registry(Some(application_id)).await;
                })
            }
            Event::Resumed => {
                // Saved sessions emit RESUMED without READY: wake ticket
                // maintenance through the shared ticket scope before the
                // registry sync, exactly as the ordered surface does.
                self.dispatch_ticket_connection(event);
                let runtime = Arc::clone(self);
                self.spawn(LANE_REGISTRY, async move {
                    runtime.publish_registry(None).await;
                })
            }
            _ => false,
        }
    }

    /// Ticket identity is independent of which runtime owns registry publication.
    fn dispatch_ticket_connection(self: &Arc<Self>, event: &Event) {
        let Some(tickets) = &self.tickets else {
            return;
        };
        match event {
            Event::Ready(ready) => tickets.on_ready(ready.user.id.get()),
            Event::Resumed => {
                let runtime = Arc::clone(self);
                // Application ids are not author ids. Keep this lookup within
                // the ticket shutdown scope, independent of registry sync.
                tickets.spawn(async move {
                    runtime.ready_tickets_after_resume().await;
                });
            }
            _ => {}
        }
    }

    /// Reactions run on their own lane: a reaction burst must consume neither
    /// message automation capacity nor interaction acknowledgement capacity.
    /// Per-member fairness mirrors the interaction lane: one member holds at
    /// most [`PER_USER_IN_FLIGHT`] reaction slots, so a single-member burst
    /// cannot fill `LANE_REACTIONS` and starve other members. Excess reactions
    /// drop (`false`) through the same counted, log-sampled path as saturated
    /// lanes; no busy reply exists on the reaction path, which carries no
    /// interaction token.
    fn dispatch_self_role_reaction(&self, reaction: &GatewayReaction, remove: bool) -> bool {
        if !self.interactions.router.gates().self_roles {
            return true;
        }
        let Some(service) = self.self_roles.as_ref().cloned() else {
            return true;
        };
        let Some(input) = service.reaction_input(reaction, remove) else {
            return true;
        };
        let Some(slot) = self.user_slots.acquire(reaction.user_id.get()) else {
            self.tasks.lock().expect("command task scope").record_drop(
                &[LANE_REACTIONS],
                "reaction per-member cap reached",
                "command dispatch fairness drop; event not admitted (sampled; see two_bot_dispatch_drops_total)",
            );
            return false;
        };
        self.spawn(LANE_REACTIONS, async move {
            let _slot = slot;
            let _ = service.handle(&input).await;
        })
    }

    async fn self_role_component(&self, interaction: &Interaction) {
        let Some(service) = &self.self_roles else {
            return;
        };
        let Some(input) = service.component_input(interaction) else {
            // Stale reference (removed panel, replaced source message, or
            // removed option): one expired-control reply, zero mutation.
            // Malformed input stays silent.
            if service.stale_component(interaction) {
                self.answer(interaction, ephemeral(EXPIRED_COMPONENT_REPLY))
                    .await;
            }
            return;
        };
        if !self.defer(interaction, "self-role").await {
            return;
        }
        let result = service.handle(&input).await;
        self.finish(interaction, result.reply(input.panel.color))
            .await;
    }

    /// One expired-control reply for genuinely unowned, in-scope components
    /// and modal submits. The router has already fenced the guild (foreign
    /// and missing guilds route `Ignore`) and the surface gates (known ids on
    /// disabled surfaces route `Ignore`), so `Unknown` here means unowned,
    /// not merely unserved. Voice-owned controls belong to the voice sink,
    /// which answers each exactly once; bot authors stay silent.
    async fn answer_stale_control(&self, interaction: &Interaction, routed: &RoutedInteraction) {
        let custom_id = match routed {
            RoutedInteraction::Component {
                custom_id,
                outcome: ComponentOutcome::Unknown,
                ..
            }
            | RoutedInteraction::Modal {
                custom_id,
                outcome: ComponentOutcome::Unknown,
            } => custom_id,
            _ => return,
        };
        // Ownership is the sink's own predicate: bare `two:voice:` ids,
        // epoch-suffixed runtime join buttons, and `votekick:` ballots. A bare
        // prefix never claims ownership on its own.
        if voice_claims_custom_id(custom_id) {
            return;
        }
        if author_is_bot(interaction) {
            return;
        }
        self.answer(interaction, ephemeral(EXPIRED_COMPONENT_REPLY))
            .await;
    }

    async fn ready_tickets_after_resume(&self) {
        let Some(tickets) = &self.tickets else {
            return;
        };
        match self.executor.current_bot_user_id().await {
            Ok(bot_id) => {
                // Reuse this lookup for LFG nonce recovery; no extra REST read.
                self.interactions.set_bot_user_id(bot_id);
                tickets.on_ready(bot_id);
            }
            Err(_) => {
                warn!("bot user lookup failed; ticket readiness skipped");
            }
        }
    }

    /// Publish the ONE complete merged registry (legacy `CommandRegistry::sync`
    /// on `ready`). `publish_set` assembles every gated builtin plus DB custom
    /// rows. Unknown custom rows defer publication rather than deleting commands.
    /// `set_guild_commands` is a full replace, making duplicate READY idempotent.
    /// RESUMED supplies no application id: resolve it through the same executor
    /// and synchronize once per process. Failed syncs remain eligible to retry
    /// on a later gateway connection event, never a polling timer.
    pub(crate) async fn publish_registry(&self, application_id: Option<u64>) {
        if self.publish_registry_checked(application_id).await.is_err() {
            warn!("command registry sync failed; details withheld");
        }
    }

    /// [`Self::publish_registry`] with the outcome returned. `Ok(())` covers a
    /// completed sync and a deferred one (custom-command rows unknown, nothing
    /// sent); `Err` means an attempted sync failed and a later READY/RESUMED retries it.
    pub(crate) async fn publish_registry_checked(
        &self,
        application_id: Option<u64>,
    ) -> Result<(), RegistrySyncError> {
        let mut synced = self.registry_synced.lock().await;
        if application_id.is_none() && *synced {
            return Ok(());
        }
        // Once bootstrapped, custom commands own the full-set publisher: it
        // loads persisted rows and serializes add/remove republication.
        if let Some(custom) = self.gateway_commands.get() {
            let application_id = match application_id {
                Some(id) => id,
                None => match self.executor.current_application_id().await {
                    Ok(id) => {
                        // RESUMED carries no application: arm the interaction
                        // fence from this registry lookup, as below.
                        self.interactions.set_application_id(id);
                        self.application_id.store(id, Ordering::Relaxed);
                        id
                    }
                    Err(_) => return Err(RegistrySyncError::ApplicationLookup),
                },
            };
            custom
                .sync_registry(application_id)
                .await
                .map_err(|_| RegistrySyncError::Publish)?;
            *synced = true;
            return Ok(());
        }
        let Some(defs) = publication_definitions(
            &self.interactions.router,
            self.interactions.router.gates(),
            self.custom_commands.as_deref(),
        )?
        else {
            return Ok(());
        };
        let application_id = match application_id {
            // A READY-supplied id is untrusted: the boot token's own identity
            // must confirm it before anything is published. A runtime with no
            // boot pin (zero) never publishes on faith.
            Some(id) => {
                let pinned = self.application_id.load(Ordering::Relaxed);
                if pinned == 0 || pinned != id {
                    warn!(
                        application_id = id,
                        "application identity differs from boot token; publish skipped"
                    );
                    return Ok(());
                }
                id
            }
            None => {
                let id = self
                    .executor
                    .current_application_id()
                    .await
                    .map_err(|_| RegistrySyncError::ApplicationLookup)?;
                // The token-authenticated lookup answers for this very token;
                // a boot pin (always present for a parseable token) must still
                // agree before anything is published.
                let pinned = self.application_id.load(Ordering::Relaxed);
                if pinned != 0 && pinned != id {
                    warn!(
                        application_id = id,
                        "application lookup differs from boot token; publish skipped"
                    );
                    return Ok(());
                }
                // RESUMED carries no application: arm the interaction fence from
                // this registry lookup instead of a separate identity read.
                self.interactions.set_application_id(id);
                self.application_id.store(id, Ordering::Relaxed);
                id
            }
        };
        let commands = publish_commands(&defs);
        self.executor
            .publish_guild_commands(application_id, self.guild_id, &commands)
            .await
            .map_err(|_| RegistrySyncError::Publish)?;
        let snapshot: Arc<[two_bot_core::CommandDefinition]> = defs.into();
        *self
            .published_commands
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(snapshot);
        *synced = true;
        Ok(())
    }

    /// Route slash commands and LFG selects once through the shared router. An
    /// injected self-role service handles only its owned component surface.
    /// Refusals get the existing ephemeral text; LFG/sticky/feed/schedule defer
    /// before I/O; leveling defers, then completes with one edit. Channel
    /// commands own their defer/audit lifecycle. Unwired builtins
    /// get an unavailable reply; router Ignore (unknown/guild) stays silent.
    pub(crate) async fn on_interaction(&self, interaction: &Interaction) {
        let application_id = self.application_id.load(Ordering::Relaxed);
        if application_id != 0 && interaction.application_id.get() != application_id {
            return;
        }
        let Some(interaction) = self
            .executor
            .command_identities()
            .resolve_interaction(interaction)
        else {
            return;
        };
        let interaction = interaction.as_ref();
        // The onboarding ingress already owns the preview's receive-relative
        // ACK and private refusal. Never double-answer it here.
        if crate::onboarding::owns_welcome_preview(interaction) {
            return;
        }
        if ChannelModerationRuntime::accepts(interaction) {
            if let Err(error) = self
                .channel
                .respond(&self.interactions.router, interaction)
                .await
            {
                warn!(interaction_id = %interaction.id.get(), reason = channel_response_class(&error), "channel response failed; no effect retry");
            }
            return;
        }
        let routed = route_interaction(&self.interactions.router, interaction, None);
        if matches!(
            &routed,
            RoutedInteraction::Slash {
                outcome: SlashOutcome::Handled {
                    handler: HandlerId::Lfg
                        | HandlerId::LfgClose
                        | HandlerId::LfgEdit
                        | HandlerId::LfgList,
                },
                ..
            } | RoutedInteraction::Component {
                outcome: two_bot_core::ComponentOutcome::Handled {
                    handler: two_bot_core::ComponentHandler::LfgSignup,
                },
                ..
            }
        ) {
            if self
                .interactions
                .handle_routed(interaction, routed)
                .await
                .is_err()
            {
                warn!(
                    interaction_id = interaction.id.get().to_string(),
                    "LFG execution failed; details withheld"
                );
            }
            return;
        }
        if let RoutedInteraction::Component {
            custom_id,
            outcome: ComponentOutcome::Handled { handler },
            ..
        } = &routed
        {
            match handler {
                ComponentHandler::GiveawayEntry => {
                    if self.defer(interaction, "giveaway").await {
                        self.giveaways.enter(interaction, custom_id).await;
                    }
                }
                ComponentHandler::SelfRole => {
                    self.self_role_component(interaction).await;
                }
                ComponentHandler::Tickets => {
                    if let (Some(tickets), Some(action)) =
                        (&self.tickets, TicketAction::from_custom_id(custom_id))
                    {
                        self.on_ticket_interaction(tickets, interaction, action)
                            .await;
                    }
                }
                _ => {}
            }
            return;
        }
        let (name, outcome) = match routed {
            RoutedInteraction::Slash { name, outcome } => (name, outcome),
            other => {
                self.answer_stale_control(interaction, &other).await;
                return;
            }
        };
        // V4 `kick` collision: this router is the only answerer. The guild
        // fence, the moderation gate and the Kick Members check run first, and
        // a moderator who passes them is never turned into a voter (a target
        // sitting in a room is no shield). Only a refusal that the V4 vote
        // contract can still serve reaches the voice runtime: moderation off,
        // or the invoker lacking Kick Members. The guild fence never does.
        if name == "kick"
            && matches!(
                outcome,
                SlashOutcome::Refuse {
                    refusal: RouterRefusal::ModerationDisabled
                        | RouterRefusal::ModerationPermission(_),
                }
            )
        {
            let vote = self
                .voice_kick_vote
                .lock()
                .expect("voice vote lock")
                .clone();
            if let Some(vote) = vote {
                if vote(interaction.clone()).await {
                    return;
                }
            }
        }
        if let Some(response) = response_for_slash(&outcome) {
            self.answer(interaction, response).await;
            return;
        }
        let SlashOutcome::Handled { handler } = outcome else {
            return;
        };
        if handler == HandlerId::Reopen {
            // The onboarding worker owns `/reopen` end to end (ingress defer
            // at reception plus the durable menu job through the same
            // pipeline as the picker components). Answering here — even the
            // unavailable terminal below — would race that owner for the
            // single initial callback, so this runtime stays silent exactly
            // like it does for the `GamePicker`/`SessionPicker` components.
            return;
        }
        if matches!(handler, HandlerId::Rank | HandlerId::Leaderboard) {
            // Leveling owns its defer-then-edit lifecycle: an ephemeral defer
            // for rank, a public defer for leaderboard, each completed by one
            // original-response edit (a sanitized unavailable reply when the
            // acknowledged store read fails). Never send the generic ephemeral
            // defer as well, and never answer here: a failed or uncertain
            // defer already returned with zero reads and nothing further to
            // send, so this branch only logs the sanitized cause.
            if let Err(error) = self.leveling.handle_interaction(interaction, handler).await {
                warn!(interaction_id = %interaction.id.get(), command = %name, reason = leveling_class(&error), "leveling interaction failed");
            }
            return;
        }
        if handler == HandlerId::Help {
            // Read the actual confirmed publication, including DB-backed
            // custom rows and successful add/remove refreshes. No store wait.
            // An omitted selector keeps the grouped index; a selector reads
            // one confirmed definition, never an unconfirmed refresh.
            let response = match self.published_commands() {
                Some(defs) => match (
                    help_command_selector(interaction),
                    help_subcommand_selector(interaction),
                ) {
                    (None, HelpSubcommand::Absent) => help_response(&defs),
                    // A focused path without a root is malformed, not an index.
                    (None, _) => help_detail_response(unknown_help_branch_correction()),
                    (Some(raw), focus) => {
                        let def = canonical_help_selector(&raw).and_then(|name| {
                            defs.iter().find(|def| def.name.eq_ignore_ascii_case(&name))
                        });
                        match (def, focus) {
                            (Some(def), HelpSubcommand::Absent) => {
                                help_detail_response(def.help_detail_text())
                            }
                            (Some(def), HelpSubcommand::Path(path)) => {
                                def.help_branch_text(&path).map_or_else(
                                    || help_detail_response(unknown_help_branch_correction()),
                                    help_detail_response,
                                )
                            }
                            (None, HelpSubcommand::Absent) => {
                                help_detail_response(unknown_help_correction())
                            }
                            (_, _) => help_detail_response(unknown_help_branch_correction()),
                        }
                    }
                },
                None => ephemeral("The command list is still refreshing. Try /help again shortly."),
            };
            self.answer(interaction, response).await;
            return;
        }
        // No `"help"` entry: the help arm above owns `/help` and returns
        // first, so an owner-map entry could never execute.
        let owner = match name.as_str() {
            "privacy" => Some(HandlerId::Privacy),
            "game-mix" => Some(HandlerId::GameMix),
            "sticky" | "sticky-remove" | "schedule" | "schedule-remove" | "schedule-list" => {
                Some(HandlerId::AutomationAdmin)
            }
            "giveaway" => Some(HandlerId::Giveaway),
            "feed-add" => Some(HandlerId::FeedAdd),
            "feed-remove" => Some(HandlerId::FeedRemove),
            "feed-pause" => Some(HandlerId::FeedPause),
            "feed-resume" => Some(HandlerId::FeedResume),
            "feed-list" => Some(HandlerId::FeedList),
            "feed-inspect" => Some(HandlerId::FeedInspect),
            // Member verbs are owned only while the consumer exists; without
            // it they keep the immediate, non-deferred unavailable reply.
            "ban" if self.member.is_some() => Some(HandlerId::Moderation(ModerationAction::Ban)),
            "tempban" if self.member.is_some() => {
                Some(HandlerId::Moderation(ModerationAction::TempBan))
            }
            "kick" if self.member.is_some() => Some(HandlerId::Moderation(ModerationAction::Kick)),
            "timeout" if self.member.is_some() => {
                Some(HandlerId::Moderation(ModerationAction::Timeout))
            }
            "warn" if self.member.is_some() => Some(HandlerId::Moderation(ModerationAction::Warn)),
            _ => None,
        };
        if owner != Some(handler) {
            self.answer(interaction, ephemeral(UNAVAILABLE_REPLY)).await;
            return;
        }
        // Acknowledge before any database wait or Discord cleanup. If the
        // acknowledgement fails, do not mutate state without a reply path.
        if !self.defer(interaction, &name).await {
            return;
        }
        match name.as_str() {
            "privacy" => {
                crate::privacy_command::privacy_command(&self.pool, &self.executor, interaction)
                    .await;
            }
            "game-mix" => {
                let guild_id = self.guild_id.to_string();
                // The CLI requires an explicit live-guild opt-in. This
                // Discord read has no rollout approval mechanism, and the
                // browse selectors never become one.
                if guild_id == two_bot_core::backup::guild_config::LIVE_GUILD_ID {
                    self.finish(interaction, "Game activity reports are not enabled for the live guild. This command does not authorize production reporting.").await;
                    return;
                }
                // Strict selector decode before any storage read: an
                // invalid, duplicated, unknown or type-confused option is
                // one private correction, never a silently served report.
                let browse = match game_mix_browse_request(interaction) {
                    Ok(browse) => browse,
                    Err(correction) => {
                        self.finish(interaction, correction).await;
                        return;
                    }
                };
                match two_bot_core::game_activity_store::read_guild_aggregates(
                    &self.pool, &guild_id,
                )
                .await
                {
                    Ok(rows) => {
                        let reply = match browse {
                            // The optionless default render is byte-stable:
                            // browse rendering applies only once a selector
                            // was explicitly chosen.
                            None => {
                                two_bot_core::game_activity::render_game_activity_discord_report(
                                    self.guild_id,
                                    &rows,
                                )
                            }
                            Some((page, sort, format)) => {
                                match two_bot_core::game_activity::render_game_activity_discord_browse(
                                    self.guild_id, &rows, page, sort, format,
                                ) {
                                    two_bot_core::game_activity::GameMixBrowseView::Page(text) => {
                                        text
                                    }
                                    two_bot_core::game_activity::GameMixBrowseView::OutOfRange {
                                        ..
                                    } => game_mix_page_range_correction(page),
                                }
                            }
                        };
                        self.finish(interaction, reply).await;
                    }
                    Err(two_bot_core::game_activity_store::GameActivityStoreError::Db(err)) => {
                        warn!(
                            msg = "game_mix_aggregate_read_failed",
                            reason = db_class(&err),
                            "game-mix aggregate read failed",
                        );
                        self.finish(
                            interaction,
                            "Game activity is unavailable right now. Please try again shortly.",
                        )
                        .await;
                    }
                }
            }
            "giveaway" => self.giveaways.command(interaction).await,
            "sticky" => self.sticky_set(interaction).await,
            "sticky-remove" => self.sticky_remove(interaction).await,
            "feed-add" => self.feed_add(interaction).await,
            "feed-remove" => self.feed_remove(interaction).await,
            "feed-pause" => self.feed_pause(interaction).await,
            "feed-resume" => self.feed_resume(interaction).await,
            "feed-list" => self.feed_list(interaction).await,
            "feed-inspect" => self.feed_inspect(interaction).await,
            "schedule" => {
                crate::schedule_runtime::schedule_create(
                    &self.pool,
                    &self.executor,
                    &self.guild_id.to_string(),
                    interaction,
                )
                .await;
            }
            "schedule-remove" => {
                crate::schedule_runtime::schedule_remove(
                    &self.pool,
                    &self.executor,
                    &self.guild_id.to_string(),
                    interaction,
                )
                .await;
            }
            "schedule-list" => {
                crate::schedule_runtime::schedule_list(
                    &self.pool,
                    &self.executor,
                    &self.guild_id.to_string(),
                    interaction,
                )
                .await;
            }
            "ban" => {
                self.member_command(interaction, ModerationAction::Ban)
                    .await;
            }
            "tempban" => {
                self.member_command(interaction, ModerationAction::TempBan)
                    .await;
            }
            "kick" => {
                self.member_command(interaction, ModerationAction::Kick)
                    .await;
            }
            "timeout" => {
                self.member_command(interaction, ModerationAction::Timeout)
                    .await;
            }
            "warn" => {
                self.member_command(interaction, ModerationAction::Warn)
                    .await;
            }
            _ => {}
        }
    }

    /// One member verb through the shared consumer. The router has already
    /// fenced the guild, the feature gate and the handler permission, and the
    /// owner mapping in `on_interaction` only admits these verbs while the
    /// consumer exists (moderation off or non-staging answers the unavailable
    /// reply before any defer). The interaction is already deferred by the
    /// caller.
    async fn member_command(&self, interaction: &Interaction, action: ModerationAction) {
        let Some(member) = &self.member else {
            return;
        };
        crate::member_runtime::handle_command(member, &self.executor, interaction, action).await;
    }

    async fn on_ticket_interaction(
        &self,
        tickets: &crate::ticket_runtime::TicketRuntime,
        interaction: &Interaction,
        action: TicketAction,
    ) {
        if let Err(error) = tickets.authorize(interaction, action) {
            self.answer(interaction, ephemeral(error.to_string())).await;
            return;
        }
        if self
            .executor
            .answer_interaction_with_blocked_retry(
                interaction.id.get(),
                &interaction.token,
                &InteractionResponse {
                    kind: InteractionResponseType::DeferredChannelMessageWithSource,
                    data: Some(InteractionResponseData {
                        flags: Some(MessageFlags::EPHEMERAL),
                        ..Default::default()
                    }),
                },
            )
            .await
            .is_err()
        {
            warn!("ticket defer failed; no state mutated");
            return;
        }
        let reply = match tickets.execute(interaction, action).await {
            Ok(reply) => reply,
            Err(error) => {
                let action_token = ticket_action_token(action);
                // Best-effort enrichment: when the failure is itself a SQL
                // storage failure, skip the decorating lookup instead of
                // spending a second pool acquisition on a sick store. The
                // lookup is also time-boxed so a slow store cannot delay the
                // user-visible error reply; expiry degrades to no context.
                let context = if error.is_storage_failure() {
                    None
                } else {
                    tokio::time::timeout(
                        std::time::Duration::from_secs(2),
                        tickets.warn_ticket_context(interaction, action),
                    )
                    .await
                    .unwrap_or(None)
                };
                match context {
                    Some((ticket_id, status)) => {
                        warn!(error_class = ?error.class(), action = %action_token, ticket_id = %ticket_id, status = %status, "ticket action incomplete")
                    }
                    None => {
                        warn!(error_class = ?error.class(), action = %action_token, "ticket action incomplete")
                    }
                }
                error.reply()
            }
        };
        if self
            .executor
            .edit_interaction_response_with_blocked_retry(
                interaction.application_id.get(),
                &interaction.token,
                &reply,
            )
            .await
            .is_err()
        {
            warn!("ticket response edit failed; durable state retained");
        }
    }

    /// Gateway accepted-message path (legacy `automationMessageAccepted`
    /// sticky half). The pure decision is only a precheck — the store's
    /// atomic claim is what coalesces a burst to one re-post.
    pub(crate) async fn on_message(&self, message: &Message) -> ActivityOutcome {
        let guild_matches = message.guild_id.map(|id| id.get()) == Some(self.guild_id);
        if !self.automations || !activity_eligible(message.author.bot, guild_matches, true) {
            return ActivityOutcome::None;
        }
        let guild_id = self.guild_id.to_string();
        let channel_id = message.channel_id.get().to_string();
        let now = now_millis_for_test();
        let state = match store::get_sticky(&self.pool, &guild_id, &channel_id).await {
            Ok(state) => state,
            Err(err) => {
                warn!(
                    reason = db_class(&err),
                    "sticky lookup failed; skipping activity"
                );
                return ActivityOutcome::None;
            }
        };
        match decide_activity(state.as_ref(), message.author.bot, guild_matches, true, now) {
            ActivityDecision::Repost { .. } => {}
            ActivityDecision::Hold => return ActivityOutcome::Held,
            ActivityDecision::Ignore | ActivityDecision::NoSticky => {
                return ActivityOutcome::None;
            }
        }

        let attempt = self.attempts.fetch_add(1, Ordering::Relaxed);
        let claim_token = format!("s{attempt:x}");
        let Some(grant) =
            (match store::claim_sticky_post(&self.pool, &guild_id, &channel_id, &claim_token, now)
                .await
            {
                Ok(grant) => grant,
                Err(err) => {
                    warn!(
                        reason = db_class(&err),
                        "sticky claim failed; skipping activity"
                    );
                    return ActivityOutcome::None;
                }
            })
        else {
            // Debounced, disabled, claimed, or fenced by an uncertain delivery.
            return ActivityOutcome::Held;
        };

        match self
            .executor
            .post_message(&channel_id, &grant.body, Some(attempt))
            .await
        {
            Ok(message_id)
                if message_id.parse::<u64>().is_ok_and(|id| id != 0)
                    && message_id.bytes().all(|b| b.is_ascii_digit()) =>
            {
                match store::record_sticky_post(
                    &self.pool,
                    &guild_id,
                    &channel_id,
                    &message_id,
                    now_millis_for_test(),
                    &claim_token,
                )
                .await
                {
                    Ok(true) => {
                        if let Some(previous) = grant.previous_message_id {
                            self.delete_quiet(&channel_id, &previous, RETIRE_REASON)
                                .await;
                        }
                        self.audit(
                            &guild_id,
                            None,
                            StickyAuditAction::Run,
                            Some(&channel_id),
                            StickyAuditOutcome::Ok,
                            None,
                        )
                        .await;
                        ActivityOutcome::Reposted
                    }
                    Ok(false) => {
                        // The claim moved on between post and record: the
                        // replacement we just posted is an orphan (legacy
                        // deletes exactly this, then reports held). Proven
                        // absence keeps that shape; an unconfirmed cleanup
                        // re-arms the fence instead of authorizing a
                        // duplicate, and a removed row stays absent.
                        match self.delete_orphan(&channel_id, &message_id).await {
                            Ok(_) => ActivityOutcome::Held,
                            Err(err) => {
                                let cause = discord_class(&err);
                                self.fence_uncertain(&guild_id, &channel_id).await;
                                self.diagnose_uncertain(&guild_id, &channel_id, cause).await;
                                ActivityOutcome::Held
                            }
                        }
                    }
                    Err(err) => {
                        // Record failed — the post may be live but unrecorded.
                        // Only proven orphan absence reopens the window;
                        // anything else re-arms the fence claim-blind: an
                        // administrator reconfigure during the POST may have
                        // disarmed the fence the claim armed.
                        warn!(
                            reason = db_class(&err),
                            "sticky record failed; confirming orphan absence"
                        );
                        match self.delete_orphan(&channel_id, &message_id).await {
                            Ok(_) => {
                                self.release(&guild_id, &channel_id, &claim_token).await;
                                self.audit(
                                    &guild_id,
                                    None,
                                    StickyAuditAction::Run,
                                    Some(&channel_id),
                                    StickyAuditOutcome::PostFailed,
                                    Some("record failed"),
                                )
                                .await;
                                ActivityOutcome::Held
                            }
                            Err(cleanup_err) => {
                                // An administrator reconfigure during the POST
                                // may have disarmed the claim's fence; re-arm
                                // claim-blind before diagnosing.
                                self.fence_uncertain(&guild_id, &channel_id).await;
                                self.diagnose_uncertain(
                                    &guild_id,
                                    &channel_id,
                                    discord_class(&cleanup_err),
                                )
                                .await;
                                ActivityOutcome::Held
                            }
                        }
                    }
                }
            }
            Ok(_) => {
                // A 2xx without a usable id is not a confirmed replacement
                // (the executor normally rejects these before they arrive).
                // Discord may have accepted the post, so re-arm the fence
                // claim-blind — an administrator reconfigure during the POST
                // may have disarmed the claim's fence — then diagnose.
                self.fence_uncertain(&guild_id, &channel_id).await;
                self.diagnose_uncertain(&guild_id, &channel_id, "receipt_id_unusable")
                    .await;
                ActivityOutcome::Held
            }
            Err(err) => {
                // Only a proven no-send reopens the window (explicit
                // rejection, rate limit, local guard, or a pre-wire
                // admission refusal — none of them reached Discord as a
                // mutation). Timeouts, transport losses and ambiguous
                // statuses re-arm the fence: the post may be live.
                let admission_refused = err.is_admission_blocked()
                    || matches!(
                        discord_class(&err),
                        "admission_storage_unavailable"
                            | "admission_configuration_invalid"
                            | "admission_stale_claim"
                    );
                if admission_refused || err.proves_no_effect() {
                    self.release(&guild_id, &channel_id, &claim_token).await;
                    self.audit(
                        &guild_id,
                        None,
                        StickyAuditAction::Run,
                        Some(&channel_id),
                        StickyAuditOutcome::PostFailed,
                        Some(&err.to_string()),
                    )
                    .await;
                } else {
                    // Ambiguous outcome, possibly live: re-arm claim-blind
                    // (an administrator reconfigure during the POST may have
                    // disarmed the fence) before diagnosing.
                    self.fence_uncertain(&guild_id, &channel_id).await;
                    self.diagnose_uncertain(&guild_id, &channel_id, discord_class(&err))
                        .await;
                }
                ActivityOutcome::Held
            }
        }
    }

    /// `/sticky`: validate → upsert → audit → ephemeral confirmation.
    /// Rejected input still audits (`rejected`, Create vs Update by whether a
    /// row already exists — the read only names the audit action). A store
    /// failure audits that same refusal shape with the `db_class` token.
    /// Malformed options refuse before any store read or audit via
    /// [`sticky_preview_option`] and [`sticky_options_strict`]: no row, no audit,
    /// one private correction. Preview validates and renders before all store
    /// access, including rejected-input auditing; it never enters delivery.
    async fn sticky_set(&self, interaction: &Interaction) {
        let guild_id = self.guild_id.to_string();
        let Some(channel_id) = interaction_channel(interaction) else {
            self.finish(interaction, NO_CHANNEL_REPLY).await;
            return;
        };
        let actor_id = actor_id(interaction);
        let preview = match sticky_preview_option(interaction) {
            Ok(preview) => preview,
            Err(refusal) => {
                self.finish(interaction, refusal).await;
                return;
            }
        };
        let (body, debounce_option) = match sticky_options_strict(interaction) {
            Ok(decoded) => decoded,
            Err(refusal) => {
                self.finish(interaction, refusal).await;
                return;
            }
        };

        let setup = StickySetup::validate(&body, debounce_option);
        if preview {
            let reply = match setup {
                Ok(setup) => setup.preview_reply(),
                Err(err) => err.to_string(),
            };
            self.finish(interaction, reply).await;
            return;
        }
        let setup = match setup {
            Ok(setup) => setup,
            Err(err) => {
                let existed = store::get_sticky(&self.pool, &guild_id, &channel_id)
                    .await
                    .ok()
                    .flatten()
                    .is_some();
                self.audit(
                    &guild_id,
                    Some(&actor_id),
                    if existed {
                        StickyAuditAction::Update
                    } else {
                        StickyAuditAction::Create
                    },
                    Some(&channel_id),
                    StickyAuditOutcome::Rejected,
                    Some(&err.to_string()),
                )
                .await;
                self.finish(interaction, err.to_string()).await;
                return;
            }
        };

        match store::put_sticky(
            &self.pool,
            &PutSticky {
                guild_id: &guild_id,
                channel_id: &channel_id,
                body: setup.body,
                debounce_seconds: setup.debounce_seconds,
                enabled: true,
                actor_id: &actor_id,
                now_ms: now_millis_for_test(),
            },
        )
        .await
        {
            Ok(created) => {
                self.audit(
                    &guild_id,
                    Some(&actor_id),
                    if created {
                        StickyAuditAction::Create
                    } else {
                        StickyAuditAction::Update
                    },
                    Some(&channel_id),
                    StickyAuditOutcome::Ok,
                    None,
                )
                .await;
                let reply =
                    sticky_set_reply(&channel_id, debounce_option.map(|_| setup.debounce_seconds));
                self.finish(interaction, reply).await;
            }
            Err(err) => {
                warn!(reason = db_class(&err), "sticky put failed");
                let existed = store::get_sticky(&self.pool, &guild_id, &channel_id)
                    .await
                    .ok()
                    .flatten()
                    .is_some();
                self.audit(
                    &guild_id,
                    Some(&actor_id),
                    if existed {
                        StickyAuditAction::Update
                    } else {
                        StickyAuditAction::Create
                    },
                    Some(&channel_id),
                    StickyAuditOutcome::Rejected,
                    Some(db_class(&err)),
                )
                .await;
                self.finish(interaction, STORE_FAILURE_REPLY).await;
            }
        }
    }

    /// `/sticky-remove`: delete the row → best-effort delete the previous
    /// Discord message → audit → ephemeral confirmation. A store failure
    /// audits a `rejected` row with the `db_class` token instead.
    async fn sticky_remove(&self, interaction: &Interaction) {
        let guild_id = self.guild_id.to_string();
        let Some(channel_id) = interaction_channel(interaction) else {
            self.finish(interaction, NO_CHANNEL_REPLY).await;
            return;
        };
        let actor_id = actor_id(interaction);

        match store::delete_sticky(&self.pool, &guild_id, &channel_id).await {
            Ok(RemoveOutcome::Removed {
                previous_message_id,
            }) => {
                if let Some(previous) = previous_message_id {
                    self.delete_quiet(&channel_id, &previous, REMOVE_REASON)
                        .await;
                }
                self.audit(
                    &guild_id,
                    Some(&actor_id),
                    StickyAuditAction::Delete,
                    Some(&channel_id),
                    StickyAuditOutcome::Ok,
                    None,
                )
                .await;
                self.finish(interaction, sticky_removed_reply(true)).await;
            }
            Ok(RemoveOutcome::Absent) => {
                self.audit(
                    &guild_id,
                    Some(&actor_id),
                    StickyAuditAction::Delete,
                    Some(&channel_id),
                    StickyAuditOutcome::Absent,
                    None,
                )
                .await;
                self.finish(interaction, sticky_removed_reply(false)).await;
            }
            Err(err) => {
                warn!(reason = db_class(&err), "sticky delete failed");
                self.audit(
                    &guild_id,
                    Some(&actor_id),
                    StickyAuditAction::Delete,
                    Some(&channel_id),
                    StickyAuditOutcome::Rejected,
                    Some(db_class(&err)),
                )
                .await;
                self.finish(interaction, STORE_FAILURE_REPLY).await;
            }
        }
    }

    /// `/feed-add`: decode options → plan → insert-only write → `feed.create`
    /// audit → ephemeral confirmation. The relay id is generated here (legacy
    /// `randomUUID()`); plan errors reply with the domain text and write
    /// neither a row nor an audit (legacy parity). An exact saved tuple replies
    /// with its original id/state, without a write or creation/rejection audit.
    /// A store failure audits one `feed.create`/`rejected` refusal row, then
    /// replies safe. The relay
    /// binds to the invoking channel. Malformed options refuse before any
    /// store read or audit via [`feed_add_options_strict`].
    async fn feed_add(&self, interaction: &Interaction) {
        let Some(channel_id) = interaction_channel(interaction) else {
            self.finish(interaction, NO_CHANNEL_REPLY).await;
            return;
        };
        let inputs = self.feed_inputs(interaction);
        let (kind, source) = match feed_add_options_strict(interaction) {
            Ok(decoded) => decoded,
            Err(refusal) => {
                self.finish(interaction, refusal).await;
                return;
            }
        };
        let kind = match FeedKind::parse(&kind) {
            Ok(kind) => kind,
            Err(err) => {
                self.finish(interaction, err.to_string()).await;
                return;
            }
        };
        let context = inputs.context(&channel_id);
        let command = FeedCommand::Add {
            id: new_id(),
            kind,
            source,
        };
        match plan_command(&context, command) {
            Ok(FeedCommandPlan::Add(relay)) => match add_feed(&self.pool, &relay).await {
                Ok(FeedAddOutcome::Created) => {
                    self.feed_audit(&inputs, "feed.create", &relay.id, relay.kind.as_str(), None)
                        .await;
                    self.finish(interaction, format!("Feed relay created: `{}`.", relay.id))
                        .await;
                }
                Ok(FeedAddOutcome::Existing { id, enabled }) => {
                    self.finish(interaction, feed_existing_text(&id, enabled))
                        .await;
                }
                Err(two_bot_core::feeds_store::FeedStoreError::Capacity(quota)) => {
                    self.finish(interaction, quota.to_string()).await;
                }
                Err(err) => {
                    let class = feed_store_class(&err);
                    warn!(reason = class, "feed insert failed");
                    self.feed_audit(&inputs, "feed.create", &relay.id, "rejected", Some(class))
                        .await;
                    self.finish(interaction, FEED_STORE_FAILURE_REPLY).await;
                }
            },
            // `FeedCommand::Add` can only plan to `Add`; the other arms are
            // unreachable, not silently wrong.
            Ok(plan) => {
                warn!(?plan, "feed-add planned to a non-add variant");
            }
            Err(err) => self.finish(interaction, err.to_string()).await,
        }
    }

    /// `/feed-remove`: decode `id` → plan → guild-scoped delete → `feed.remove`
    /// audit (`removed`/`missing`/`recovery_required` outcome) → ephemeral
    /// confirmation. A relay carrying unresolved pending delivery evidence is
    /// retained with a snapshot reply carrying the fenced pending count,
    /// never claimed removed. A store failure audits one
    /// `feed.remove`/`rejected` refusal row, then replies safe. Malformed `id`
    /// options refuse before any store read or audit via
    /// [`feed_remove_option_strict`].
    async fn feed_remove(&self, interaction: &Interaction) {
        // Remove does not bind to the invoking channel; `channel_id` is only
        // context the planner ignores for this command.
        let channel_id = interaction_channel(interaction).unwrap_or_default();
        let inputs = self.feed_inputs(interaction);
        let context = inputs.context(&channel_id);
        let id = match feed_remove_option_strict(interaction) {
            Ok(id) => id,
            Err(refusal) => {
                self.finish(interaction, refusal).await;
                return;
            }
        };
        let command = FeedCommand::Remove { id };
        match plan_command(&context, command) {
            Ok(FeedCommandPlan::Remove { guild_id, id }) => {
                match remove_feed_with_evidence(&self.pool, &guild_id, &id).await {
                    Ok(outcome) => {
                        let (audit_outcome, reply) = match outcome {
                            FeedRemovalEvidence::Removed => {
                                ("removed", feed_removed_text(true).to_owned())
                            }
                            FeedRemovalEvidence::Missing => {
                                ("missing", feed_removed_text(false).to_owned())
                            }
                            FeedRemovalEvidence::RecoveryRequired { pending } => (
                                "recovery_required",
                                feed_pending_evidence_refusal_text(
                                    FeedPendingEvidenceOperation::Remove,
                                    &id,
                                    pending,
                                ),
                            ),
                        };
                        self.feed_audit(&inputs, "feed.remove", &id, audit_outcome, None)
                            .await;
                        self.finish(interaction, reply).await;
                    }
                    Err(err) => {
                        let class = feed_store_class(&err);
                        warn!(reason = class, "feed remove failed");
                        self.feed_audit(&inputs, "feed.remove", &id, "rejected", Some(class))
                            .await;
                        self.finish(interaction, FEED_STORE_FAILURE_REPLY).await;
                    }
                }
            }
            Ok(plan) => {
                warn!(?plan, "feed-remove planned to a non-remove variant");
            }
            Err(err) => self.finish(interaction, err.to_string()).await,
        }
    }

    /// `/feed-pause`: decode `id` → plan → guild-scoped disable →
    /// `feed.pause` audit (`paused`/`already_paused`/`missing`/`recovery_required`
    /// outcome) → ephemeral confirmation. URL, destination, seen-item and
    /// delivery history are preserved; only lifecycle state changes. An enabled
    /// relay carrying unresolved pending delivery evidence stays enabled with a
    /// snapshot reply carrying the fenced pending count, so recovery is never
    /// stranded behind disabled discovery. Already-admitted work is not
    /// cancelled, and no fresh post claim may begin after a successful pause.
    /// A store failure audits one `feed.pause`/`rejected` refusal row, then
    /// replies safe.
    async fn feed_pause(&self, interaction: &Interaction) {
        // Pause does not bind to the invoking channel; `channel_id` is only
        // context the planner ignores for this command.
        let channel_id = interaction_channel(interaction).unwrap_or_default();
        let inputs = self.feed_inputs(interaction);
        let context = inputs.context(&channel_id);
        let id = match feed_id_option_strict(
            interaction,
            FEED_PAUSE_INVALID_ID,
            FEED_PAUSE_DUPLICATE_ID,
            FEED_PAUSE_MISSING_ID,
        ) {
            Ok(id) => id,
            Err(refusal) => {
                self.finish(interaction, refusal).await;
                return;
            }
        };
        let command = FeedCommand::Pause { id };
        match plan_command(&context, command) {
            Ok(FeedCommandPlan::Pause { guild_id, id }) => {
                match pause_feed_with_evidence(&self.pool, &guild_id, &id, inputs.now_ms).await {
                    Ok(outcome) => {
                        let (audit_outcome, reply) = match outcome {
                            FeedPauseEvidence::Paused => ("paused", feed_paused_text(&id)),
                            FeedPauseEvidence::AlreadyPaused => {
                                ("already_paused", feed_already_paused_text(&id))
                            }
                            FeedPauseEvidence::Missing => {
                                ("missing", feed_removed_text(false).to_owned())
                            }
                            FeedPauseEvidence::RecoveryRequired { pending } => (
                                "recovery_required",
                                feed_pending_evidence_refusal_text(
                                    FeedPendingEvidenceOperation::Pause,
                                    &id,
                                    pending,
                                ),
                            ),
                        };
                        self.feed_audit(&inputs, "feed.pause", &id, audit_outcome, None)
                            .await;
                        self.finish(interaction, reply).await;
                    }
                    Err(err) => {
                        let class = feed_store_class(&err);
                        warn!(reason = class, "feed pause failed");
                        self.feed_audit(&inputs, "feed.pause", &id, "rejected", Some(class))
                            .await;
                        self.finish(interaction, FEED_STORE_FAILURE_REPLY).await;
                    }
                }
            }
            Ok(plan) => {
                warn!(?plan, "feed-pause planned to a non-pause variant");
            }
            Err(err) => self.finish(interaction, err.to_string()).await,
        }
    }

    /// `/feed-resume`: decode `id` → plan → guild-scoped enable →
    /// `feed.resume` audit (`resumed`/`already_resumed`/`missing` outcome) →
    /// ephemeral confirmation. Seen-item/high-water and nonce/dedup history
    /// are preserved; resume never reposts an accepted item and keeps the
    /// existing per-poll post budget rather than draining a backlog. A store
    /// failure audits one `feed.resume`/`rejected` refusal row, then replies
    /// safe.
    async fn feed_resume(&self, interaction: &Interaction) {
        // Resume does not bind to the invoking channel; `channel_id` is only
        // context the planner ignores for this command.
        let channel_id = interaction_channel(interaction).unwrap_or_default();
        let inputs = self.feed_inputs(interaction);
        let context = inputs.context(&channel_id);
        let id = match feed_id_option_strict(
            interaction,
            FEED_RESUME_INVALID_ID,
            FEED_RESUME_DUPLICATE_ID,
            FEED_RESUME_MISSING_ID,
        ) {
            Ok(id) => id,
            Err(refusal) => {
                self.finish(interaction, refusal).await;
                return;
            }
        };
        let command = FeedCommand::Resume { id };
        match plan_command(&context, command) {
            Ok(FeedCommandPlan::Resume { guild_id, id }) => {
                match resume_feed(&self.pool, &guild_id, &id, inputs.now_ms).await {
                    Ok(outcome) => {
                        let (audit_outcome, reply) = match outcome {
                            FeedResumeOutcome::Resumed => ("resumed", feed_resumed_text(&id)),
                            FeedResumeOutcome::AlreadyResumed => {
                                ("already_resumed", feed_already_resumed_text(&id))
                            }
                            FeedResumeOutcome::Missing => {
                                ("missing", feed_removed_text(false).to_owned())
                            }
                        };
                        self.feed_audit(&inputs, "feed.resume", &id, audit_outcome, None)
                            .await;
                        self.finish(interaction, reply).await;
                    }
                    Err(err) => {
                        let class = feed_store_class(&err);
                        warn!(reason = class, "feed resume failed");
                        self.feed_audit(&inputs, "feed.resume", &id, "rejected", Some(class))
                            .await;
                        self.finish(interaction, FEED_STORE_FAILURE_REPLY).await;
                    }
                }
            }
            Ok(plan) => {
                warn!(?plan, "feed-resume planned to a non-resume variant");
            }
            Err(err) => self.finish(interaction, err.to_string()).await,
        }
    }

    /// `/feed-list`: plan → page decode → guild-scoped list → ephemeral
    /// text. Legacy `listFeeds` lists every relay for the guild (not `enabled`
    /// only) and writes no audit row. An omitted `page` means page 1; a
    /// present-but-unusable option refuses without a store read, and
    /// out-of-range refuses after the same bounded read every list uses.
    async fn feed_list(&self, interaction: &Interaction) {
        let channel_id = interaction_channel(interaction).unwrap_or_default();
        let inputs = self.feed_inputs(interaction);
        let context = inputs.context(&channel_id);
        match plan_command(&context, FeedCommand::List) {
            Ok(FeedCommandPlan::List { guild_id }) => {
                let reply = match feed_list_page_request(interaction) {
                    FeedListPage::Invalid => feed_list_page_invalid_text().to_owned(),
                    FeedListPage::Default => self.list_feed_page(&guild_id, 1).await,
                    FeedListPage::Page(page) if page < 1 => {
                        feed_list_page_underflow_text().to_owned()
                    }
                    FeedListPage::Page(page) => self.list_feed_page(&guild_id, page).await,
                };
                self.finish(interaction, reply).await;
            }
            Ok(plan) => {
                warn!(?plan, "feed-list planned to a non-list variant");
            }
            Err(err) => self.finish(interaction, err.to_string()).await,
        }
    }

    /// `/feed-inspect`: decode `id` → plan → one bounded guild-scoped read →
    /// ephemeral detail. A pure read: no fetch, schedule change, relay
    /// mutation, audit write, claim renewal or delivery reset on any path,
    /// including store failure (legacy list parity). Absent, invalid-id and
    /// store-failure replies stay distinct; the raw source never reaches logs.
    async fn feed_inspect(&self, interaction: &Interaction) {
        // Inspect does not bind to the invoking channel; `channel_id` is only
        // context the planner ignores for this command.
        let channel_id = interaction_channel(interaction).unwrap_or_default();
        let inputs = self.feed_inputs(interaction);
        let context = inputs.context(&channel_id);
        let command = FeedCommand::Inspect {
            id: feed_id_option(interaction).unwrap_or_default(),
        };
        match plan_command(&context, command) {
            Ok(FeedCommandPlan::Inspect { guild_id, id }) => {
                let reply = match list_feeds(&self.pool, &guild_id, false).await {
                    Ok(feeds) => feeds
                        .iter()
                        .find(|feed| feed.id == id)
                        .map(|feed| feed.inspect_text())
                        .unwrap_or_else(|| feed_removed_text(false).to_owned()),
                    Err(err) => {
                        warn!(reason = feed_store_class(&err), "feed inspect failed");
                        FEED_STORE_FAILURE_REPLY.to_owned()
                    }
                };
                self.finish(interaction, reply).await;
            }
            Ok(plan) => {
                warn!(?plan, "feed-inspect planned to a non-inspect variant");
            }
            Err(err) => self.finish(interaction, err.to_string()).await,
        }
    }

    /// One bounded guild-scoped relay read rendered as the requested page.
    /// A failed read keeps the fixed safe reply and writes no audit row.
    async fn list_feed_page(&self, guild_id: &str, page: i64) -> String {
        match list_feeds(&self.pool, guild_id, false).await {
            Ok(feeds) => feed_list_page(&feeds, page),
            Err(err) => {
                warn!(reason = feed_store_class(&err), "feed list failed");
                FEED_STORE_FAILURE_REPLY.to_owned()
            }
        }
    }

    /// Owned pieces of the feed plan context for one interaction: the
    /// router's `announcements` gate is the enable switch (the same value the
    /// router refused on), the configured `GUILD_ID` is the fence, and the
    /// invoker's member permission bits decide `can_manage_guild`.
    fn feed_inputs(&self, interaction: &Interaction) -> FeedInputs {
        FeedInputs {
            enabled: self.interactions.router.gates().announcements,
            configured_guild_id: self.guild_id.to_string(),
            guild_id: interaction
                .guild_id
                .map(|id| id.get().to_string())
                .unwrap_or_default(),
            actor_id: actor_id(interaction),
            can_manage_guild: can_manage_guild(interaction),
            now_ms: now_millis_for_test(),
        }
    }

    /// Append one `feed.*` audit row; an audit write failure is logged but
    /// never fails the operation it records (same posture as [`Self::audit`]).
    /// Refusals carry the shared error-class token as `reason`; healthy
    /// outcomes pass `None`.
    async fn feed_audit(
        &self,
        inputs: &FeedInputs,
        action: &str,
        target_key: &str,
        outcome: &str,
        reason: Option<&str>,
    ) {
        let audit_id = new_id();
        let row = FeedAudit {
            id: &audit_id,
            guild_id: &inputs.guild_id,
            actor_id: Some(inputs.actor_id.as_str()).filter(|actor| !actor.is_empty()),
            action,
            target_key,
            outcome,
            reason,
            now_ms: now_millis_for_test(),
        };
        if let Err(err) = write_audit(&self.pool, &row).await {
            warn!(
                action,
                outcome,
                reason = feed_store_class(&err),
                "feed audit write failed"
            );
        }
    }

    /// Answer an interaction through the shared executor. The token never
    /// appears in logs — only the interaction id and error.
    async fn defer(&self, interaction: &Interaction, command: &str) -> bool {
        match self
            .executor
            .answer_interaction_with_blocked_retry(
                interaction.id.get(),
                &interaction.token,
                &InteractionResponse {
                    kind: InteractionResponseType::DeferredChannelMessageWithSource,
                    data: Some(InteractionResponseData {
                        flags: Some(MessageFlags::EPHEMERAL),
                        ..Default::default()
                    }),
                },
            )
            .await
        {
            Ok(()) => true,
            Err(err) => {
                warn!(interaction_id = %interaction.id.get(), command, reason = discord_class(&err), "command defer failed");
                false
            }
        }
    }

    async fn answer(&self, interaction: &Interaction, response: InteractionResponse) {
        if let Err(err) = self
            .executor
            .answer_interaction_with_blocked_retry(
                interaction.id.get(),
                &interaction.token,
                &response,
            )
            .await
        {
            warn!(interaction_id = %interaction.id.get(), reason = discord_class(&err), "sticky reply failed");
        }
    }

    /// Complete the original ephemeral defer, never issue a second callback.
    async fn finish(&self, interaction: &Interaction, content: impl AsRef<str>) {
        if let Err(err) = self
            .executor
            .edit_interaction_response_with_blocked_retry(
                interaction.application_id.get(),
                &interaction.token,
                content.as_ref(),
            )
            .await
        {
            warn!(interaction_id = %interaction.id.get(), reason = discord_class(&err), "sticky reply edit failed");
        }
    }

    /// Best-effort message delete: cleanup failures are logged, never fatal
    /// (legacy swallows them the same way).
    async fn delete_quiet(&self, channel_id: &str, message_id: &str, reason: &str) {
        if let Err(err) = self
            .executor
            .delete_message(channel_id, message_id, reason)
            .await
        {
            warn!(
                reason = discord_class(&err),
                "sticky cleanup delete failed; continuing"
            );
        }
    }

    /// Release this attempt's claim after a definitive outcome; failure
    /// leaves the delivery fence armed for an administrator reset (the claim
    /// itself still expires in 60s, but expiry no longer authorizes a post).
    async fn release(&self, guild_id: &str, channel_id: &str, claim_token: &str) {
        if let Err(err) =
            store::release_sticky_post(&self.pool, guild_id, channel_id, claim_token).await
        {
            warn!(
                reason = db_class(&err),
                "sticky claim release failed; uncertainty fence retained"
            );
        }
    }

    /// Confirming orphan delete: `Ok(true)` deleted, `Ok(false)` already
    /// absent (both proven — a retry is safe), `Err` the orphan may still be
    /// live. An unconfirmed cleanup is never reported as success; the caller
    /// keeps or re-arms the delivery fence.
    async fn delete_orphan(
        &self,
        channel_id: &str,
        message_id: &str,
    ) -> Result<bool, DiscordError> {
        let outcome = self
            .executor
            .delete_message_with_absence_receipt(channel_id, message_id, ORPHAN_REASON)
            .await;
        if let Err(err) = &outcome {
            warn!(
                reason = discord_class(err),
                "sticky orphan cleanup unconfirmed; fencing delivery"
            );
        }
        outcome
    }

    /// Re-arm the delivery fence on any uncertain outcome — the claim's
    /// own fence may have been disarmed by an administrator reconfigure
    /// mid-flight; a no-op when the administrator removed the row. A
    /// failed write is logged — without a claim no further post is
    /// possible until the store recovers, and the next attempt re-fences
    /// if needed.
    async fn fence_uncertain(&self, guild_id: &str, channel_id: &str) {
        if let Err(err) =
            store::mark_sticky_delivery_uncertain(&self.pool, guild_id, channel_id).await
        {
            warn!(
                reason = db_class(&err),
                "sticky uncertainty fence write failed; orphan may be live"
            );
        }
    }

    /// Log the bounded uncertainty diagnostic and audit the attempt as
    /// failed — never as success. `cause` is a fixed cause token, never
    /// payload or member content.
    async fn diagnose_uncertain(&self, guild_id: &str, channel_id: &str, cause: &str) {
        warn!(
            code = UNCERTAIN_DELIVERY_CODE,
            retryable = false,
            next_action = UNCERTAIN_DELIVERY_NEXT_ACTION,
            reason = cause,
            guild_id = %guild_id,
            channel_id = %channel_id,
            "sticky re-post outcome uncertain; inspect the channel before resetting the sticky"
        );
        self.audit(
            guild_id,
            None,
            StickyAuditAction::Run,
            Some(channel_id),
            StickyAuditOutcome::PostFailed,
            Some(cause),
        )
        .await;
    }

    /// Append one `sticky.*` audit row; an audit write failure is logged but
    /// never fails the operation it records.
    #[allow(clippy::too_many_arguments)]
    async fn audit(
        &self,
        guild_id: &str,
        actor_id: Option<&str>,
        action: StickyAuditAction,
        target_key: Option<&str>,
        outcome: StickyAuditOutcome,
        reason: Option<&str>,
    ) {
        let row = StickyAudit {
            guild_id,
            actor_id,
            action,
            target_key,
            outcome,
            reason,
            at_ms: now_millis_for_test(),
        };
        if let Err(err) = store::audit_sticky(&self.pool, &row).await {
            warn!(
                action = action.as_str(),
                outcome = outcome.as_str(),
                reason = db_class(&err),
                "sticky audit write failed"
            );
        }
    }
}

/// Fixed-vocabulary token for the `ticket action incomplete` warn's `action`
/// field.
fn ticket_action_token(action: TicketAction) -> &'static str {
    match action {
        TicketAction::Open => "open",
        TicketAction::Claim => "claim",
        TicketAction::Close => "close",
    }
}

/// Invoking channel as a snowflake string. `channel_id` is deprecated on the
/// wire but still the field Discord populates for guild slash commands; the
/// replacement `channel` object carries the id plus a payload we never read.
#[allow(deprecated)]
pub(crate) fn interaction_channel(interaction: &Interaction) -> Option<String> {
    interaction
        .channel
        .as_ref()
        .map(|channel| channel.id.get().to_string())
        .or_else(|| interaction.channel_id.map(|id| id.get().to_string()))
}

/// Owned inputs for one feed command's plan context and audit rows. The
/// borrowed [`FeedCommandContext`] cannot own its strings, so this holds them
/// and lends them through [`FeedInputs::context`].
struct FeedInputs {
    enabled: bool,
    configured_guild_id: String,
    guild_id: String,
    actor_id: String,
    can_manage_guild: bool,
    now_ms: i64,
}

impl FeedInputs {
    fn context<'a>(&'a self, channel_id: &'a str) -> FeedCommandContext<'a> {
        FeedCommandContext {
            enabled: self.enabled,
            configured_guild_id: &self.configured_guild_id,
            guild_id: &self.guild_id,
            channel_id,
            actor_id: &self.actor_id,
            can_manage_guild: self.can_manage_guild,
            now_ms: self.now_ms,
        }
    }
}

/// Invoker's `member.permissions` bits → Manage Guild check (the same bits
/// the router already adjudicated; the planner re-checks them).
fn can_manage_guild(interaction: &Interaction) -> bool {
    interaction
        .member
        .as_ref()
        .and_then(|member| member.permissions)
        .is_some_and(|permissions| permissions.bits() & PERM_MANAGE_GUILD == PERM_MANAGE_GUILD)
}

/// Relay and audit row ids: 16 CSPRNG bytes, hex-encoded to the 32-hex shape
/// `internal_actions` already uses. `validate_id` accepts it (alphanumeric,
/// ≤128) and it fills legacy `randomUUID()`'s role — opaque, unique.
pub(crate) fn new_id() -> String {
    hex::encode(rand::random::<[u8; 16]>())
}

/// Refusal literals for malformed `/feed-add` and `/feed-remove` options.
/// Each names the offending option and carries a copyable corrected
/// invocation, echoes no user input (mention-safe), and stays far under
/// Discord's 2,000-unit cap (bounded). Handlers reply with these before any
/// store read or audit write.
const FEED_ADD_INVALID_KIND: &str = "Invalid kind. Use /feed-add kind:rss source:<https-url>.";
const FEED_ADD_DUPLICATE_KIND: &str = "Duplicate kind. Use /feed-add kind:rss source:<https-url>.";
const FEED_ADD_MISSING_KIND: &str = "Missing kind. Use /feed-add kind:rss source:<https-url>.";
const FEED_ADD_INVALID_SOURCE: &str = "Invalid source. Use /feed-add kind:rss source:<https-url>.";
const FEED_ADD_DUPLICATE_SOURCE: &str =
    "Duplicate source. Use /feed-add kind:rss source:<https-url>.";
const FEED_ADD_MISSING_SOURCE: &str = "Missing source. Use /feed-add kind:rss source:<https-url>.";
const FEED_REMOVE_INVALID_ID: &str = "Invalid id. Use /feed-remove id:<relay-id>.";
const FEED_REMOVE_DUPLICATE_ID: &str = "Duplicate id. Use /feed-remove id:<relay-id>.";
const FEED_REMOVE_MISSING_ID: &str = "Missing id. Use /feed-remove id:<relay-id>.";
const FEED_PAUSE_INVALID_ID: &str = "Invalid id. Use /feed-pause id:<relay-id>.";
const FEED_PAUSE_DUPLICATE_ID: &str = "Duplicate id. Use /feed-pause id:<relay-id>.";
const FEED_PAUSE_MISSING_ID: &str = "Missing id. Use /feed-pause id:<relay-id>.";
const FEED_RESUME_INVALID_ID: &str = "Invalid id. Use /feed-resume id:<relay-id>.";
const FEED_RESUME_DUPLICATE_ID: &str = "Duplicate id. Use /feed-resume id:<relay-id>.";
const FEED_RESUME_MISSING_ID: &str = "Missing id. Use /feed-resume id:<relay-id>.";

/// Strictly decode `/feed-add`'s options: `kind` and `source` must each
/// appear exactly once with a string value. Duplicates win over wrong types
/// win over missing options, and `kind` wins over `source` within a phase;
/// unknown names stay ignored. A non-command interaction carries no options,
/// so it refuses as a missing `kind`. Valid invocations decode to the same
/// `kind`/`source` values the previous lenient decoder extracted.
pub(crate) fn feed_add_options_strict(
    interaction: &Interaction,
) -> Result<(String, String), &'static str> {
    let Some(twilight_model::application::interaction::InteractionData::ApplicationCommand(data)) =
        &interaction.data
    else {
        return Err(FEED_ADD_MISSING_KIND);
    };
    let mut kinds: Vec<&CommandOptionValue> = Vec::new();
    let mut sources: Vec<&CommandOptionValue> = Vec::new();
    for option in &data.options {
        match option.name.as_str() {
            "kind" => kinds.push(&option.value),
            "source" => sources.push(&option.value),
            _ => {}
        }
    }
    if kinds.len() > 1 {
        return Err(FEED_ADD_DUPLICATE_KIND);
    }
    if sources.len() > 1 {
        return Err(FEED_ADD_DUPLICATE_SOURCE);
    }
    // Wrong-type phase: a present-but-unusable option refuses before any
    // missing option is named, in declared order.
    if !matches!(kinds.first(), None | Some(CommandOptionValue::String(_))) {
        return Err(FEED_ADD_INVALID_KIND);
    }
    if !matches!(sources.first(), None | Some(CommandOptionValue::String(_))) {
        return Err(FEED_ADD_INVALID_SOURCE);
    }
    let kind = match kinds.first() {
        None => return Err(FEED_ADD_MISSING_KIND),
        Some(CommandOptionValue::String(value)) => (*value).clone(),
        Some(_) => return Err(FEED_ADD_INVALID_KIND),
    };
    let source = match sources.first() {
        None => return Err(FEED_ADD_MISSING_SOURCE),
        Some(CommandOptionValue::String(value)) => (*value).clone(),
        Some(_) => return Err(FEED_ADD_INVALID_SOURCE),
    };
    Ok((kind, source))
}

/// Strictly decode `/feed-remove`'s `id` option: it must appear exactly once
/// with a string value. A duplicated name wins over a wrong wire type, which
/// wins over a missing option; unknown names stay ignored. A non-command
/// interaction carries no options, so it refuses as a missing `id`.
/// Present-but-empty still decodes (the planner's `validate_id` owns that
/// refusal), so valid invocations plan byte-identically to before.
pub(crate) fn feed_remove_option_strict(interaction: &Interaction) -> Result<String, &'static str> {
    feed_id_option_strict(
        interaction,
        FEED_REMOVE_INVALID_ID,
        FEED_REMOVE_DUPLICATE_ID,
        FEED_REMOVE_MISSING_ID,
    )
}

/// Share strict relay-id decoding without sharing command-worded refusals.
/// Malformed options refuse before planning, store access or audit writes;
/// typed-empty values still reach the planner's `validate_id` check.
fn feed_id_option_strict(
    interaction: &Interaction,
    invalid_id: &'static str,
    duplicate_id: &'static str,
    missing_id: &'static str,
) -> Result<String, &'static str> {
    let Some(twilight_model::application::interaction::InteractionData::ApplicationCommand(data)) =
        &interaction.data
    else {
        return Err(missing_id);
    };
    let mut ids: Vec<&CommandOptionValue> = Vec::new();
    for option in &data.options {
        if option.name.as_str() == "id" {
            ids.push(&option.value);
        }
    }
    if ids.len() > 1 {
        return Err(duplicate_id);
    }
    match ids.first() {
        None => Err(missing_id),
        Some(CommandOptionValue::String(value)) => Ok((*value).clone()),
        Some(_) => Err(invalid_id),
    }
}

/// `/feed-inspect`'s `id` option; `None` plans against an empty id, which
/// `validate_id` rejects with `InvalidId` and inspect's distinct recovery
/// copy (`/feed-remove` decodes strictly via [`feed_remove_option_strict`]).
pub(crate) fn feed_id_option(interaction: &Interaction) -> Option<String> {
    let Some(twilight_model::application::interaction::InteractionData::ApplicationCommand(data)) =
        &interaction.data
    else {
        return None;
    };
    data.options
        .iter()
        .find_map(|option| match (option.name.as_str(), &option.value) {
            ("id", CommandOptionValue::String(value)) => Some(value.clone()),
            _ => None,
        })
}

/// `/help`'s optional `command` selector. Omitted means the grouped index.
/// A present option with a non-string wire value is malformed and decodes
/// to an empty selector, which answers the unknown-name correction rather
/// than silently listing the index.
pub(crate) fn help_command_selector(interaction: &Interaction) -> Option<String> {
    let Some(twilight_model::application::interaction::InteractionData::ApplicationCommand(data)) =
        &interaction.data
    else {
        return None;
    };
    let option = data
        .options
        .iter()
        .find(|option| option.name == "command")?;
    match &option.value {
        CommandOptionValue::String(value) => Some(value.clone()),
        _ => Some(String::new()),
    }
}

/// The optional focused `/help subcommand:` path. A present option with a
/// non-string wire value, or a duplicated option, is malformed and answers
/// the branch correction rather than silently showing the root detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HelpSubcommand {
    Absent,
    Path(String),
    Malformed,
}

pub(crate) fn help_subcommand_selector(interaction: &Interaction) -> HelpSubcommand {
    let Some(twilight_model::application::interaction::InteractionData::ApplicationCommand(data)) =
        &interaction.data
    else {
        return HelpSubcommand::Absent;
    };
    let mut selected = data
        .options
        .iter()
        .filter(|option| option.name == "subcommand");
    let (Some(option), None) = (selected.next(), selected.next()) else {
        return if data
            .options
            .iter()
            .any(|option| option.name == "subcommand")
        {
            HelpSubcommand::Malformed
        } else {
            HelpSubcommand::Absent
        };
    };
    match &option.value {
        CommandOptionValue::String(value) => HelpSubcommand::Path(value.clone()),
        _ => HelpSubcommand::Malformed,
    }
}

/// `/feed-list`'s optional 1-based `page` option, decoded by name first: an
/// omitted option means page 1, while a present option with a non-integer
/// wire value is unusable and must refuse rather than silently list page 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FeedListPage {
    Default,
    Page(i64),
    Invalid,
}

pub(crate) fn feed_list_page_request(interaction: &Interaction) -> FeedListPage {
    let Some(twilight_model::application::interaction::InteractionData::ApplicationCommand(data)) =
        &interaction.data
    else {
        return FeedListPage::Default;
    };
    let Some(page) = data.options.iter().find(|option| option.name == "page") else {
        return FeedListPage::Default;
    };
    match &page.value {
        CommandOptionValue::Integer(value) => FeedListPage::Page(*value),
        _ => FeedListPage::Invalid,
    }
}

/// Strict `/game-mix` browse selectors, decoded before any storage read.
///
/// Every option must be one of the published selectors, appear at most
/// once, and carry the published wire type: `page` is an integer within
/// the published bounds, `sort` is `play` or `voice`, `format` is `json`
/// or `summary`. Anything else — an unknown name such as a guild, member
/// or live-guild override, a duplicate, a wrong-typed value — returns one
/// bounded correction naming the offending selector. `Ok(None)` is the
/// optionless legacy call, which keeps the default JSON render.
pub(crate) fn game_mix_browse_request(
    interaction: &Interaction,
) -> Result<
    Option<(
        usize,
        two_bot_core::game_activity::GameMixBrowseSort,
        two_bot_core::game_activity::GameMixBrowseFormat,
    )>,
    String,
> {
    use two_bot_core::game_activity::{GameMixBrowseFormat, GameMixBrowseSort};
    let Some(twilight_model::application::interaction::InteractionData::ApplicationCommand(data)) =
        &interaction.data
    else {
        return Ok(None);
    };
    if data.options.is_empty() {
        return Ok(None);
    }
    let correction = |selector: &'static str| {
        format!(
            "That {selector} value is not valid for /game-mix. Use page 1 to {}, \
             sort play or voice, and format json or summary — and no other options.",
            two_bot_core::game_activity::GAME_MIX_MAX_PAGE
        )
    };
    let mut page = 1usize;
    let mut sort = GameMixBrowseSort::Play;
    let mut format = GameMixBrowseFormat::Json;
    let mut seen: Vec<&str> = Vec::new();
    for option in &data.options {
        if seen.contains(&option.name.as_str()) {
            return Err(correction(match option.name.as_str() {
                "page" => "page",
                "sort" => "sort",
                "format" => "format",
                _ => "game-mix",
            }));
        }
        seen.push(option.name.as_str());
        match (option.name.as_str(), &option.value) {
            ("page", CommandOptionValue::Integer(value))
                if (1..=two_bot_core::game_activity::GAME_MIX_MAX_PAGE).contains(value) =>
            {
                page = usize::try_from(*value).map_err(|_| correction("page"))?;
            }
            ("page", _) => return Err(correction("page")),
            ("sort", CommandOptionValue::String(value)) => {
                sort = match value.as_str() {
                    "play" => GameMixBrowseSort::Play,
                    "voice" => GameMixBrowseSort::Voice,
                    _ => return Err(correction("sort")),
                };
            }
            ("sort", _) => return Err(correction("sort")),
            ("format", CommandOptionValue::String(value)) => {
                format = match value.as_str() {
                    "json" => GameMixBrowseFormat::Json,
                    "summary" => GameMixBrowseFormat::Summary,
                    _ => return Err(correction("format")),
                };
            }
            ("format", _) => return Err(correction("format")),
            _ => return Err(correction("game-mix")),
        }
    }
    Ok(Some((page, sort, format)))
}

/// Bounded correction for a well-formed page past the saved games. The
/// saved totals stay out of the reply: page bounds follow from the
/// request, never from data a refused-at-decode call could not see.
fn game_mix_page_range_correction(page: usize) -> String {
    format!(
        "Page {page} is past the saved games for this server. \
         Run /game-mix with page 1 to start from the first saved games."
    )
}

/// Shared sticky/feed/channel/leveling registrations over the narrowed boot
/// gates; LFG is composed by `InteractionRuntime`. Tests that bypass
/// `from_env`'s env reads use this same router, matching `from_env`'s
/// registrations.
pub(crate) fn router_with_commands(gates: RouterGates) -> InteractionRouter {
    let mut router = InteractionRouter::new(gates);
    if gates.moderation {
        register_channel_handlers(&mut router);
    }
    if gates.automations {
        router.register(Box::new(StickyHandler));
        router.register(Box::new(SliceHandler(HandlerId::Giveaway)));
        // Custom commands are an automations capability: a refused identity
        // must not even resolve their handlers (4014 isolation depends on the
        // narrowed intent too). Dispatch refuses and publish withholds when
        // off, but registration alone would advertise the surface.
        two_bot_discord::custom_commands::CustomCommandRuntime::register(&mut router);
    }
    if gates.announcements {
        for id in [
            HandlerId::FeedAdd,
            HandlerId::FeedRemove,
            HandlerId::FeedPause,
            HandlerId::FeedResume,
            HandlerId::FeedList,
            HandlerId::FeedInspect,
            // The ordered RSVP surface executes these ids; registration
            // documents the ownership the router outcome names.
            HandlerId::Rsvp,
            HandlerId::RsvpAttendance,
        ] {
            router.register(Box::new(SliceHandler(id)));
        }
    }
    if gates.scorecard {
        router.register(Box::new(SliceHandler(HandlerId::ScorecardAttendance)));
    }
    // Leveling is not an activation-fenced capability: the core rank and
    // leaderboard commands stay available to every permitted identity.
    for id in [HandlerId::Rank, HandlerId::Leaderboard] {
        router.register(Box::new(SliceHandler(id)));
    }
    // `/help` discovery is never activation-fenced either: it renders from
    // the live publish set, so gated-off features simply do not appear.
    router.register(Box::new(SliceHandler(HandlerId::Help)));
    // `/privacy` member self-serve is never activation-fenced either: every
    // member may opt out of analytics in any guild.
    router.register(Box::new(SliceHandler(HandlerId::Privacy)));
    // `/reopen` picker recovery is never activation-fenced either: every
    // member may reopen their own pickers in any gate state. Execution
    // lives in the onboarding worker (`on_interaction` below stays silent
    // for it, exactly like the picker components); this registration only
    // advertises the ownership the router outcome names.
    router.register(Box::new(SliceHandler(HandlerId::Reopen)));
    // Onboarding ingress owns the preview, including its private refusal.
    router.register(Box::new(SliceHandler(HandlerId::WelcomePreview)));
    router.register(Box::new(SliceHandler(HandlerId::GameMix)));
    router
}

/// Ephemeral channel-message reply (same shape the router's refusal builder
/// produces).
pub(crate) fn ephemeral(text: impl Into<String>) -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(InteractionResponseData {
            content: Some(text.into()),
            flags: Some(MessageFlags::EPHEMERAL),
            ..Default::default()
        }),
    }
}

/// Bot-authored interactions stay silent: guild presses carry the author on
/// the member, DMs on the top-level user.
fn author_is_bot(interaction: &Interaction) -> bool {
    interaction.user.as_ref().is_some_and(|user| user.bot)
        || interaction
            .member
            .as_ref()
            .and_then(|member| member.user.as_ref())
            .is_some_and(|user| user.bot)
}

/// Invoker id: guild member's user, else the DM user (legacy reads
/// `interaction.user.id` first either way). `""` when both are absent —
/// `PutSticky.actor_id` is NOT NULL, and the audit column tolerates NULL via
/// `Option`.
pub(crate) fn actor_id(interaction: &Interaction) -> String {
    interaction
        .member
        .as_ref()
        .and_then(|member| member.user.as_ref())
        .or(interaction.user.as_ref())
        .map(|user| user.id.get().to_string())
        .unwrap_or_default()
}

/// Refusal literals for malformed `/sticky` options. Each names the
/// offending option and carries a copyable corrected invocation, echoes no
/// user input (mention-safe), and stays far under Discord's 2,000-unit cap
/// (bounded). Handlers reply with these before any store read or audit write.
const STICKY_INVALID_BODY: &str = "Invalid body. Use /sticky body:<text> debounce:<seconds>.";
const STICKY_DUPLICATE_BODY: &str = "Duplicate body. Use /sticky body:<text> debounce:<seconds>.";
const STICKY_MISSING_BODY: &str = "Missing body. Use /sticky body:<text> debounce:<seconds>.";
const STICKY_INVALID_DEBOUNCE: &str =
    "Invalid debounce. Use /sticky body:<text> debounce:<seconds>.";
const STICKY_DUPLICATE_DEBOUNCE: &str =
    "Duplicate debounce. Use /sticky body:<text> debounce:<seconds>.";

/// Guard the complete setup option namespace before decoding body/debounce.
/// Unknown names (including misspelled preview flags) must never silently save.
/// Keep the existing body/debounce decoder and its refusal precedence unchanged.
fn sticky_preview_option(interaction: &Interaction) -> Result<bool, &'static str> {
    let Some(twilight_model::application::interaction::InteractionData::ApplicationCommand(data)) =
        &interaction.data
    else {
        return Err(STICKY_MISSING_BODY);
    };
    let mut preview = None;
    for option in &data.options {
        match option.name.as_str() {
            "body" | "debounce" => {}
            "preview" => match (&option.value, preview) {
                (CommandOptionValue::Boolean(value), None) => preview = Some(*value),
                _ => return Err("Invalid preview. Use /sticky body:<text> [debounce:<seconds>] preview:true or preview:false; supply preview only once."),
            },
            _ => return Err("Unknown option. Use /sticky body:<text> [debounce:<seconds>] [preview:true|false]."),
        }
    }
    Ok(preview.unwrap_or(false))
}

/// Strictly decode `/sticky`'s options: `body` must appear exactly once with
/// a string value, while `debounce` stays optional (an omitted option means
/// the default quiet window). Duplicates win over wrong types win over a
/// missing `body`, and `body` wins over `debounce` within a phase. This helper
/// decodes only these two fields; [`sticky_preview_option`] must run first to
/// reject unknown names and malformed preview flags. A non-command interaction
/// refuses as a missing `body`. Present-but-empty and out-of-range values
/// still decode (validation owns those refusals), so valid invocations
/// decode byte-identically to before.
pub(crate) fn sticky_options_strict(
    interaction: &Interaction,
) -> Result<(String, Option<i64>), &'static str> {
    let Some(twilight_model::application::interaction::InteractionData::ApplicationCommand(data)) =
        &interaction.data
    else {
        return Err(STICKY_MISSING_BODY);
    };
    let mut bodies: Vec<&CommandOptionValue> = Vec::new();
    let mut debounces: Vec<&CommandOptionValue> = Vec::new();
    for option in &data.options {
        match option.name.as_str() {
            "body" => bodies.push(&option.value),
            "debounce" => debounces.push(&option.value),
            _ => {}
        }
    }
    if bodies.len() > 1 {
        return Err(STICKY_DUPLICATE_BODY);
    }
    if debounces.len() > 1 {
        return Err(STICKY_DUPLICATE_DEBOUNCE);
    }
    // Wrong-type phase: a present-but-unusable option refuses before a
    // missing `body` is named, in declared order.
    if !matches!(bodies.first(), None | Some(CommandOptionValue::String(_))) {
        return Err(STICKY_INVALID_BODY);
    }
    if !matches!(
        debounces.first(),
        None | Some(CommandOptionValue::Integer(_))
    ) {
        return Err(STICKY_INVALID_DEBOUNCE);
    }
    let body = match bodies.first() {
        None => return Err(STICKY_MISSING_BODY),
        Some(CommandOptionValue::String(value)) => (*value).clone(),
        Some(_) => return Err(STICKY_INVALID_BODY),
    };
    let debounce = match debounces.first() {
        None => None,
        Some(CommandOptionValue::Integer(value)) => Some(*value),
        Some(_) => return Err(STICKY_INVALID_DEBOUNCE),
    };
    Ok((body, debounce))
}

#[cfg(test)]
#[path = "command_runtime_resumed_tests.rs"]
mod resumed_tests;

/// Full-set publication: a bulk replace needs an authoritative custom-command
/// row load, so an unknown store defers instead of deleting Discord commands.
pub(crate) fn publication_definitions(
    router: &InteractionRouter,
    gates: RouterGates,
    custom: Option<&[two_bot_core::CustomCommand]>,
) -> Result<Option<Vec<two_bot_core::CommandDefinition>>, RegistrySyncError> {
    if gates.automations && custom.is_none() {
        warn!("registry publication deferred: custom-command store unavailable");
        return Ok(None);
    }
    router
        .publish_set(custom.unwrap_or_default())
        .map(Some)
        .map_err(|_| RegistrySyncError::Invalid)
}

#[cfg(test)]
#[path = "command_runtime_poison_acceptance.rs"]
mod dispatch_guard_poison_acceptance;
