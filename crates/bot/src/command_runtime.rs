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
//! - feed relays (`/feed-add`, `/feed-remove`, `/feed-list`; TOG-10085 domain
//!   and store): plan → guild-scoped CRUD → `announcements_audit_log` row → ephemeral
//!   completion. The generated relay/audit ids replace legacy `randomUUID()`.
//! - schedules (`/schedule`, `/schedule-remove`, `/schedule-list`; TOG-12237
//!   over the TOG-10081 domain and store): validate → guild-scoped CRUD →
//!   `automation_audit_log` row → ephemeral completion. The handlers live in
//!   [`crate::schedule_runtime`]; the runtime only registers and dispatches.
//! - leveling (`/rank [member]`, `/leaderboard`): one immediate callback,
//!   ephemeral rank and public mention-suppressed top ten. The ordered gateway
//!   award path shares this runtime's pool, executor and onboarding gates.
//!
//! Registry publication runs here too: every `Event::Ready` publishes the
//! router's ONE merged publish set (`set_guild_commands` is idempotent, so a
//! duplicate READY is a harmless repeat). A resumed process also synchronizes
//! once: a persisted gateway session does not preserve this process's gates
//! or command definitions. Until the custom-command store supplies authoritative
//! rows, publication is deferred when automations enable dynamic commands.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use crate::self_role_handlers::SelfRoleService;
use sqlx::{Pool, Postgres};
use tracing::warn;
use twilight_gateway::Event;
use twilight_model::{
    application::interaction::{
        application_command::CommandOptionValue, Interaction, InteractionData,
    },
    channel::message::{Message, MessageFlags},
    gateway::GatewayReaction,
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
};
use two_bot_core::{
    commands::PERM_MANAGE_GUILD,
    feeds::{
        feed_list_text, feed_removed_text, plan_command, FeedCommand, FeedCommandContext,
        FeedCommandPlan, FeedError, FeedKind,
    },
    feeds_store::{add_feed, list_feeds, remove_feed, write_audit, FeedAudit},
    funnel::now_millis_for_test,
    sticky::{
        activity_eligible, decide_activity, normalize_debounce, sticky_removed_reply,
        sticky_set_reply, store, validate_body, ActivityDecision, ActivityOutcome, PutSticky,
        RemoveOutcome, StickyAudit, StickyAuditAction, StickyAuditOutcome,
    },
    tickets::TicketAction,
    ComponentHandler, ComponentOutcome, FeatureGates, HandlerId, InteractionHandler,
    InteractionRouter, ModerationGates, RouterGates, SlashOutcome, SurfaceFlags,
};
use two_bot_discord::{
    publish_commands, response_for_slash, route_interaction, ActionExecutor, LevelingRuntime,
    RoutedInteraction,
};

/// Audit-log reason for retiring the previous sticky (legacy audits carry a
/// free-text reason; kept short — `audit_reason` caps at 512 chars).
const RETIRE_REASON: &str = "sticky re-post";
/// Reason for deleting a replacement post whose claim moved on.
const ORPHAN_REASON: &str = "sticky re-post rolled back";
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

/// The shared command runtime: one router + one REST executor + the sqlx
/// stores, driven by detached gateway tasks. Cheap to clone behind `Arc`; no
/// REST or feature-store work is awaited while polling the shard. Dispatch
/// itself is unbounded; LFG caps its own pool use (`LFG_MAX_IN_FLIGHT`).
pub struct CommandRuntime {
    pool: Pool<Postgres>,
    executor: ActionExecutor,
    interactions: two_bot_discord::interactions::InteractionRuntime,
    custom_commands: Option<Vec<two_bot_core::CustomCommand>>,
    application_id: AtomicU64,
    /// Shared self-role surface; production boot stays parked pending acceptance.
    self_roles: Option<Arc<SelfRoleService>>,
    leveling: LevelingRuntime,
    /// Configured guild (`GUILD_ID`); also the router's guild fence.
    guild_id: u64,
    tickets: Option<Arc<crate::ticket_runtime::TicketRuntime>>,
    /// `TWO_AUTOMATIONS=1`: fast-path gate for the message hook (the router
    /// still answers `/sticky*` refusals when it is off).
    automations: bool,
    /// Serialize publication and remember a successful sync for this process.
    /// Repeated RESUMED events need no work; READY still replaces the full set.
    registry_synced: tokio::sync::Mutex<bool>,
    /// Monotonic attempt ids: one value mints both the DB claim token
    /// (`s{n:x}`, ≤25 chars) and the numeric post nonce for dedupe.
    attempts: AtomicU64,
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
    ) -> Arc<Self> {
        let automations = router.gates().automations;
        let interactions = two_bot_discord::interactions::InteractionRuntime::with_router(
            router,
            pool.clone(),
            executor.clone(),
            0,
            two_bot_core::ClassifierConfig::from_env(),
        );
        Arc::new(Self {
            pool,
            executor,
            interactions,
            custom_commands,
            application_id: AtomicU64::new(0),
            self_roles,
            leveling,
            guild_id,
            tickets,
            automations,
            registry_synced: tokio::sync::Mutex::new(false),
            attempts: AtomicU64::new(now_millis_for_test().max(0) as u64),
        })
    }

    /// Test-only access to the shared interaction surface for identity assertions.
    #[cfg(test)]
    pub(crate) fn test_interactions(&self) -> &two_bot_discord::interactions::InteractionRuntime {
        &self.interactions
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
    /// opens the self-role router surface.
    #[must_use]
    pub fn from_env(
        pool: Pool<Postgres>,
        token: &str,
        guild_id: u64,
        self_roles: Option<Arc<SelfRoleService>>,
        onboarding: two_bot_core::OnboardingGates,
    ) -> Option<Arc<Self>> {
        let features = match FeatureGates::from_env() {
            Ok(features) => features,
            Err(err) => {
                warn!(error = %err, "feature gates invalid; command runtime disabled");
                return None;
            }
        };
        let moderation = match ModerationGates::from_env() {
            Ok(moderation) => moderation,
            Err(err) => {
                warn!(error = %err, "moderation gates invalid; command runtime disabled");
                return None;
            }
        };
        let ticket_config = crate::ticket_runtime::TicketConfig::from_env(guild_id);
        let gates = RouterGates::from_slices(
            Some(guild_id),
            &features,
            &moderation,
            SurfaceFlags {
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
                    warn!(error = %err, "send admission invalid; command runtime disabled");
                    return None;
                }
            };
        let executor = match ActionExecutor::with_admission(token.to_owned(), proxy, admission) {
            Ok(executor) => executor,
            Err(err) => {
                warn!(error = %err, "REST executor failed to build; command runtime disabled");
                return None;
            }
        };
        let leveling = LevelingRuntime::new(
            pool.clone(),
            Arc::new(executor.clone()),
            guild_id,
            onboarding,
        );
        let tickets = match ticket_config {
            Some(config) => match crate::ticket_runtime::TicketRuntime::new(
                pool.clone(),
                executor.clone(),
                config,
            ) {
                Ok(runtime) => Some(Arc::new(runtime)),
                Err(_) => return None,
            },
            None => None,
        };
        // No custom-command store exists on main yet: an empty slice is the
        // authoritative baseline (same as before LFG).
        Some(Self::build(
            pool,
            executor,
            router_with_commands(gates),
            self_roles,
            leveling,
            guild_id,
            Some(Vec::new()),
            tickets,
        ))
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
        Self::build(
            pool,
            executor,
            router,
            None,
            leveling,
            guild_id,
            Some(Vec::new()),
            None,
        )
    }

    /// Dispatch alongside ordered RSVP execution. That path owns RSVP replies
    /// (including refusals) and publishes the full registry before polling.
    /// Keep the standalone dispatch fallback for gateway callers without it.
    pub(crate) fn dispatch_remaining(self: &Arc<Self>, event: &Event) {
        match event {
            Event::Ready(ready) => {
                // The ordered surface owns registry publication; identity still
                // initializes here so LFG keeps READY's ids without a REST read.
                self.set_identity(ready.user.id.get(), ready.application.id.get());
                self.dispatch_ticket_connection(event);
                return;
            }
            Event::Resumed => {
                self.dispatch_ticket_connection(event);
                return;
            }
            Event::InteractionCreate(interaction) => {
                if let Some(InteractionData::ApplicationCommand(data)) = interaction.data.as_ref() {
                    if matches!(
                        data.name.as_str(),
                        "rsvp" | "rsvp-attendance" | "attendance"
                    ) {
                        return;
                    }
                }
            }
            _ => {}
        }
        self.dispatch(event);
    }

    /// The one process executor (admission lane and pacing included); other
    /// runtimes (onboarding, automod) and the ordered interaction surface
    /// render through a clone, never a private client. One token key, one
    /// pacing lane, one governed admission for both surfaces.
    pub fn executor(&self) -> ActionExecutor {
        self.executor.clone()
    }

    /// Shares this runtime's pool, executor/pacing and onboarding gates with
    /// the ordered award path; only this runtime dispatches interactions.
    pub fn leveling(&self) -> LevelingRuntime {
        self.leveling.clone()
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
            self_roles: false,
            onboarding_picker: false,
            session_picker: false,
        });
        let mut runtime = Self::new(pool, executor, router, 100, false);
        Arc::get_mut(&mut runtime)
            .expect("unshared test runtime")
            .tickets = Some(tickets);
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

    /// Detached dispatch for one gateway event. Clones the payload and spawns
    /// so the shard loop never awaits runtime work; the DB claims tolerate
    /// the reorder/crash windows spawning opens.
    ///
    /// READY replaces the full merged set; a first RESUMED also synchronizes
    /// this process's definitions/gates, even without a preceding READY.
    pub fn dispatch(self: &Arc<Self>, event: &Event) {
        self.dispatch_ticket_connection(event);
        match event {
            Event::MessageCreate(message) => {
                let runtime = Arc::clone(self);
                let message = message.0.clone();
                // Dropping the JoinHandle detaches the task — exactly what the
                // shard loop needs (never await runtime work while polling).
                drop(tokio::spawn(async move {
                    runtime.on_message(&message).await;
                }));
            }
            Event::InteractionCreate(interaction) => {
                let runtime = Arc::clone(self);
                let interaction = interaction.0.clone();
                let is_ticket = matches!(
                    interaction.data.as_ref(),
                    Some(twilight_model::application::interaction::InteractionData::MessageComponent(component))
                        if TicketAction::from_custom_id(&component.custom_id).is_some()
                );
                if let Some(tickets) = self.tickets.as_ref().filter(|_| is_ticket) {
                    tickets.spawn(async move {
                        runtime.on_interaction(&interaction).await;
                    });
                } else {
                    drop(tokio::spawn(async move {
                        runtime.on_interaction(&interaction).await;
                    }));
                }
            }
            Event::ReactionAdd(reaction) => self.dispatch_self_role_reaction(&reaction.0, false),
            Event::ReactionRemove(reaction) => self.dispatch_self_role_reaction(&reaction.0, true),
            Event::Ready(ready) => {
                self.set_identity(ready.user.id.get(), ready.application.id.get());
                if let Some(tickets) = &self.tickets {
                    tickets.on_ready(ready.user.id.get());
                }
                let runtime = Arc::clone(self);
                let application_id = ready.application.id.get();
                drop(tokio::spawn(async move {
                    runtime.publish_registry(Some(application_id)).await;
                }));
            }
            Event::Resumed => {
                let runtime = Arc::clone(self);
                drop(tokio::spawn(async move {
                    runtime.publish_registry(None).await;
                }));
            }
            _ => {}
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

    fn dispatch_self_role_reaction(&self, reaction: &GatewayReaction, remove: bool) {
        if !self.interactions.router.gates().self_roles {
            return;
        }
        let Some(service) = self.self_roles.as_ref().cloned() else {
            return;
        };
        let Some(input) = service.reaction_input(reaction, remove) else {
            return;
        };
        drop(tokio::spawn(async move {
            let _ = service.handle(&input).await;
        }));
    }

    async fn self_role_component(&self, interaction: &Interaction) {
        let Some(service) = &self.self_roles else {
            return;
        };
        let Some(input) = service.component_input(interaction) else {
            return;
        };
        if !self.defer(interaction, "self-role").await {
            return;
        }
        let result = service.handle(&input).await;
        self.finish(interaction, result.reply(input.panel.color))
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
        let Some(defs) = publication_definitions(
            &self.interactions.router,
            self.interactions.router.gates(),
            self.custom_commands.as_deref(),
        )?
        else {
            return Ok(());
        };
        let application_id = match application_id {
            Some(id) => id,
            None => {
                let id = self
                    .executor
                    .current_application_id()
                    .await
                    .map_err(|_| RegistrySyncError::ApplicationLookup)?;
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
        *synced = true;
        Ok(())
    }

    /// Route slash commands and LFG selects once through the shared router. An
    /// injected self-role service handles only its owned component surface.
    /// Refusals get the existing ephemeral text; LFG/sticky/feed/schedule defer
    /// before I/O; leveling sends its own immediate callback. Unwired builtins
    /// get an unavailable reply; router Ignore (unknown/guild) stays silent.
    pub(crate) async fn on_interaction(&self, interaction: &Interaction) {
        let application_id = self.application_id.load(Ordering::Relaxed);
        if application_id != 0 && interaction.application_id.get() != application_id {
            return;
        }
        let routed = route_interaction(&self.interactions.router, interaction, None);
        if matches!(
            &routed,
            RoutedInteraction::Slash {
                outcome: SlashOutcome::Handled {
                    handler: HandlerId::Lfg | HandlerId::LfgClose,
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
                    interaction_id = interaction.id.get(),
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
        let RoutedInteraction::Slash { name, outcome } = routed else {
            return;
        };
        if let Some(response) = response_for_slash(&outcome) {
            self.answer(interaction, response).await;
            return;
        }
        let SlashOutcome::Handled { handler } = outcome else {
            return;
        };
        if matches!(handler, HandlerId::Rank | HandlerId::Leaderboard) {
            // Leveling owns its immediate rank-ephemeral/leaderboard-public
            // callback. Never send the generic ephemeral defer as well.
            if let Err(error) = self.leveling.handle_interaction(interaction, handler).await {
                warn!(interaction_id = %interaction.id.get(), command = %name, error = %error, "leveling interaction failed");
            }
            return;
        }
        let owner = match name.as_str() {
            "sticky" | "sticky-remove" | "schedule" | "schedule-remove" | "schedule-list" => {
                Some(HandlerId::AutomationAdmin)
            }
            "feed-add" => Some(HandlerId::FeedAdd),
            "feed-remove" => Some(HandlerId::FeedRemove),
            "feed-list" => Some(HandlerId::FeedList),
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
            "sticky" => self.sticky_set(interaction).await,
            "sticky-remove" => self.sticky_remove(interaction).await,
            "feed-add" => self.feed_add(interaction).await,
            "feed-remove" => self.feed_remove(interaction).await,
            "feed-list" => self.feed_list(interaction).await,
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
            _ => {}
        }
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
            .answer_interaction(
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
                warn!(error_class = ?error.class(), "ticket action incomplete");
                error.reply()
            }
        };
        if self
            .executor
            .edit_interaction_response(interaction.application_id.get(), &interaction.token, &reply)
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
                warn!(error = %err, "sticky lookup failed; skipping activity");
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
                    warn!(error = %err, "sticky claim failed; skipping activity");
                    return ActivityOutcome::None;
                }
            })
        else {
            // Debounced, disabled, or another attempt holds the claim.
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
                        // deletes exactly this, then reports held).
                        self.delete_quiet(&channel_id, &message_id, ORPHAN_REASON)
                            .await;
                        ActivityOutcome::Held
                    }
                    Err(err) => {
                        // Record failed — the post may be live but unrecorded,
                        // so roll back what we can: release our claim and
                        // delete the orphan, then audit post_failed.
                        warn!(error = %err, "sticky record failed; rolling back");
                        self.release(&guild_id, &channel_id, &claim_token).await;
                        self.delete_quiet(&channel_id, &message_id, ORPHAN_REASON)
                            .await;
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
                }
            }
            Ok(_) => {
                // A 2xx without a usable id is not a confirmed replacement.
                // Keep the previous message; the unknown post cannot safely
                // be recorded or cleaned up by id.
                self.release(&guild_id, &channel_id, &claim_token).await;
                self.audit(
                    &guild_id,
                    None,
                    StickyAuditAction::Run,
                    Some(&channel_id),
                    StickyAuditOutcome::PostFailed,
                    Some("replacement message id missing or invalid"),
                )
                .await;
                ActivityOutcome::Held
            }
            Err(err) => {
                // Post did not land (or its id was lost): release the claim so
                // the next message can retry.
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
                ActivityOutcome::Held
            }
        }
    }

    /// `/sticky`: validate → upsert → audit → ephemeral confirmation.
    /// Rejected input still audits (`rejected`, Create vs Update by whether a
    /// row already exists — the read only names the audit action).
    async fn sticky_set(&self, interaction: &Interaction) {
        let guild_id = self.guild_id.to_string();
        let Some(channel_id) = interaction_channel(interaction) else {
            self.finish(interaction, NO_CHANNEL_REPLY).await;
            return;
        };
        let actor_id = actor_id(interaction);
        let (body, debounce_option) = sticky_options(interaction);
        let body = body.unwrap_or_default();

        let debounce = match validate_body(&body).and_then(|()| normalize_debounce(debounce_option))
        {
            Ok(debounce) => debounce,
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
                body: &body,
                debounce_seconds: debounce,
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
                let reply = sticky_set_reply(&channel_id, debounce_option.map(|_| debounce));
                self.finish(interaction, reply).await;
            }
            Err(err) => {
                warn!(error = %err, "sticky put failed");
                self.finish(interaction, STORE_FAILURE_REPLY).await;
            }
        }
    }

    /// `/sticky-remove`: delete the row → best-effort delete the previous
    /// Discord message → audit → ephemeral confirmation.
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
                warn!(error = %err, "sticky delete failed");
                self.finish(interaction, STORE_FAILURE_REPLY).await;
            }
        }
    }

    /// `/feed-add`: decode options → plan → insert-only write → `feed.create`
    /// audit → ephemeral confirmation. The relay id is generated here (legacy
    /// `randomUUID()`); plan errors reply with the domain text and write
    /// neither a row nor an audit (legacy parity). The relay binds to the
    /// invoking channel.
    async fn feed_add(&self, interaction: &Interaction) {
        let Some(channel_id) = interaction_channel(interaction) else {
            self.finish(interaction, NO_CHANNEL_REPLY).await;
            return;
        };
        let inputs = self.feed_inputs(interaction);
        let (kind, source) = feed_add_options(interaction);
        let kind = match kind
            .as_deref()
            .map_or(Err(FeedError::InvalidKind), FeedKind::parse)
        {
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
            source: source.unwrap_or_default(),
        };
        match plan_command(&context, command) {
            Ok(FeedCommandPlan::Add(relay)) => {
                if let Err(err) = add_feed(&self.pool, &relay).await {
                    warn!(error = %err, "feed insert failed");
                    self.finish(interaction, FEED_STORE_FAILURE_REPLY).await;
                    return;
                }
                self.feed_audit(&inputs, "feed.create", &relay.id, relay.kind.as_str())
                    .await;
                self.finish(interaction, format!("Feed relay created: `{}`.", relay.id))
                    .await;
            }
            // `FeedCommand::Add` can only plan to `Add`; the other arms are
            // unreachable, not silently wrong.
            Ok(plan) => {
                warn!(?plan, "feed-add planned to a non-add variant");
            }
            Err(err) => self.finish(interaction, err.to_string()).await,
        }
    }

    /// `/feed-remove`: decode `id` → plan → guild-scoped delete → `feed.remove`
    /// audit (`removed`/`missing` outcome) → ephemeral confirmation.
    async fn feed_remove(&self, interaction: &Interaction) {
        // Remove does not bind to the invoking channel; `channel_id` is only
        // context the planner ignores for this command.
        let channel_id = interaction_channel(interaction).unwrap_or_default();
        let inputs = self.feed_inputs(interaction);
        let context = inputs.context(&channel_id);
        let command = FeedCommand::Remove {
            id: feed_remove_option(interaction).unwrap_or_default(),
        };
        match plan_command(&context, command) {
            Ok(FeedCommandPlan::Remove { guild_id, id }) => {
                match remove_feed(&self.pool, &guild_id, &id).await {
                    Ok(removed) => {
                        self.feed_audit(
                            &inputs,
                            "feed.remove",
                            &id,
                            if removed { "removed" } else { "missing" },
                        )
                        .await;
                        self.finish(interaction, feed_removed_text(removed)).await;
                    }
                    Err(err) => {
                        warn!(error = %err, "feed remove failed");
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

    /// `/feed-list`: plan → guild-scoped list → ephemeral text. Legacy
    /// `listFeeds` lists every relay for the guild (not `enabled` only) and
    /// writes no audit row.
    async fn feed_list(&self, interaction: &Interaction) {
        let channel_id = interaction_channel(interaction).unwrap_or_default();
        let inputs = self.feed_inputs(interaction);
        let context = inputs.context(&channel_id);
        match plan_command(&context, FeedCommand::List) {
            Ok(FeedCommandPlan::List { guild_id }) => {
                match list_feeds(&self.pool, &guild_id, false).await {
                    Ok(feeds) => self.finish(interaction, feed_list_text(&feeds)).await,
                    Err(err) => {
                        warn!(error = %err, "feed list failed");
                        self.finish(interaction, FEED_STORE_FAILURE_REPLY).await;
                    }
                }
            }
            Ok(plan) => {
                warn!(?plan, "feed-list planned to a non-list variant");
            }
            Err(err) => self.finish(interaction, err.to_string()).await,
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
    async fn feed_audit(&self, inputs: &FeedInputs, action: &str, target_key: &str, outcome: &str) {
        let audit_id = new_id();
        let row = FeedAudit {
            id: &audit_id,
            guild_id: &inputs.guild_id,
            actor_id: Some(inputs.actor_id.as_str()).filter(|actor| !actor.is_empty()),
            action,
            target_key,
            outcome,
            reason: None,
            now_ms: now_millis_for_test(),
        };
        if let Err(err) = write_audit(&self.pool, &row).await {
            warn!(action, outcome, error = %err, "feed audit write failed");
        }
    }

    /// Answer an interaction through the shared executor. The token never
    /// appears in logs — only the interaction id and error.
    async fn defer(&self, interaction: &Interaction, command: &str) -> bool {
        match self
            .executor
            .answer_interaction(
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
                warn!(interaction_id = %interaction.id.get(), command, error = %err, "command defer failed");
                false
            }
        }
    }

    async fn answer(&self, interaction: &Interaction, response: InteractionResponse) {
        if let Err(err) = self
            .executor
            .answer_interaction(interaction.id.get(), &interaction.token, &response)
            .await
        {
            warn!(interaction_id = %interaction.id.get(), error = %err, "sticky reply failed");
        }
    }

    /// Complete the original ephemeral defer, never issue a second callback.
    async fn finish(&self, interaction: &Interaction, content: impl AsRef<str>) {
        if let Err(err) = self
            .executor
            .edit_interaction_response(
                interaction.application_id.get(),
                &interaction.token,
                content.as_ref(),
            )
            .await
        {
            warn!(interaction_id = %interaction.id.get(), error = %err, "sticky reply edit failed");
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
            warn!(error = %err, "sticky cleanup delete failed; continuing");
        }
    }

    /// Release this attempt's claim; failure means expiry clears it in 60s.
    async fn release(&self, guild_id: &str, channel_id: &str, claim_token: &str) {
        if let Err(err) =
            store::release_sticky_post(&self.pool, guild_id, channel_id, claim_token).await
        {
            warn!(error = %err, "sticky claim release failed; expiry will clear it");
        }
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
            warn!(action = action.as_str(), outcome = outcome.as_str(), error = %err, "sticky audit write failed");
        }
    }
}

/// Invoking channel as a snowflake string. `channel_id` is deprecated on the
/// wire but still the field Discord populates for guild slash commands; the
/// replacement `channel` object carries the id plus a payload we never read.
#[allow(deprecated)]
fn interaction_channel(interaction: &Interaction) -> Option<String> {
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

/// `/feed-add`'s options: `kind` (choice string) + `source` (string). A
/// missing `kind` replies `InvalidKind`; a missing `source` plans against an
/// empty string, which the SSRF guard rejects.
pub(crate) fn feed_add_options(interaction: &Interaction) -> (Option<String>, Option<String>) {
    let mut kind = None;
    let mut source = None;
    let Some(twilight_model::application::interaction::InteractionData::ApplicationCommand(data)) =
        &interaction.data
    else {
        return (kind, source);
    };
    for option in &data.options {
        match (option.name.as_str(), &option.value) {
            ("kind", CommandOptionValue::String(value)) => kind = Some(value.clone()),
            ("source", CommandOptionValue::String(value)) => source = Some(value.clone()),
            _ => {}
        }
    }
    (kind, source)
}

/// `/feed-remove`'s `id` option; `None` plans against an empty id, which
/// `validate_id` rejects with `InvalidId`.
pub(crate) fn feed_remove_option(interaction: &Interaction) -> Option<String> {
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

/// Shared sticky/feed/leveling registrations; LFG is composed by
/// `InteractionRuntime`.
pub(crate) fn router_with_commands(gates: RouterGates) -> InteractionRouter {
    let mut router = InteractionRouter::new(gates);
    router.register(Box::new(StickyHandler));
    for id in [
        HandlerId::FeedAdd,
        HandlerId::FeedRemove,
        HandlerId::FeedList,
        HandlerId::Rank,
        HandlerId::Leaderboard,
    ] {
        router.register(Box::new(SliceHandler(id)));
    }
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

/// Extract `/sticky`'s options: `body` (required string) + `debounce`
/// (optional integer). Missing `body` becomes `None` → validation rejects it
/// with the legacy body-length text.
pub(crate) fn sticky_options(interaction: &Interaction) -> (Option<String>, Option<i64>) {
    let mut body = None;
    let mut debounce = None;
    let Some(twilight_model::application::interaction::InteractionData::ApplicationCommand(data)) =
        &interaction.data
    else {
        return (body, debounce);
    };
    for option in &data.options {
        match (option.name.as_str(), &option.value) {
            ("body", CommandOptionValue::String(value)) => body = Some(value.clone()),
            ("debounce", CommandOptionValue::Integer(value)) => debounce = Some(*value),
            _ => {}
        }
    }
    (body, debounce)
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
