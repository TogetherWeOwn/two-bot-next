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
//! - sticky (`/sticky`, `/sticky-remove` + the accepted-message re-post hook;
//!   legacy order pinned below: claim → post → record → delete previous →
//!   audit, with `post_failed`/orphan cleanup on the failure edges).
//! - feed relays (`/feed-add`, `/feed-remove`, `/feed-list`; TOG-10085 domain
//!   and store): plan → guild-scoped CRUD → `announcements_audit_log` row → ephemeral
//!   completion. The generated relay/audit ids replace legacy `randomUUID()`.
//! - leveling (`/rank [member]`, `/leaderboard`): one immediate callback,
//!   ephemeral rank and public mention-suppressed top ten. The ordered gateway
//!   award path shares this runtime's pool, executor and onboarding gates.
//!
//! Registry publication runs here too: every `Event::Ready` publishes the
//! router's ONE merged publish set (`set_guild_commands` is idempotent, so a
//! duplicate READY is a harmless repeat). A resumed process also synchronizes
//! once: a persisted gateway session does not preserve this process's gates
//! or command definitions. No custom-command store exists on `main` yet —
//! `publish_set` gets an empty custom slice until that slice lands its own reader.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use sqlx::{Pool, Postgres};
use tracing::warn;
use twilight_gateway::Event;
use twilight_model::{
    application::interaction::{application_command::CommandOptionValue, Interaction},
    channel::message::{Message, MessageFlags},
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
    FeatureGates, HandlerId, InteractionHandler, InteractionRouter, ModerationGates, RouterGates,
    SlashOutcome, SurfaceFlags,
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
/// stores, driven by gateway dispatches. Cheap to clone behind `Arc`; every
/// `dispatch` spawns detached work because twilight only drives heartbeats
/// while the shard is polled.
pub struct CommandRuntime {
    pool: Pool<Postgres>,
    executor: ActionExecutor,
    router: InteractionRouter,
    leveling: LevelingRuntime,
    /// Configured guild (`GUILD_ID`); also the router's guild fence.
    guild_id: u64,
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

impl CommandRuntime {
    /// Build the runtime from process env gates + the optional
    /// `DISCORD_API_BASE` proxy override. Returns `None` — gateway still
    /// boots — when gate parsing or executor construction fails, so bad
    /// env cannot take the shard down.
    #[must_use]
    pub fn from_env(
        pool: Pool<Postgres>,
        token: &str,
        guild_id: u64,
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
        let gates = RouterGates::from_slices(
            Some(guild_id),
            &features,
            &moderation,
            SurfaceFlags {
                session_picker: onboarding.mode == two_bot_core::OnboardingMode::Session,
                ..SurfaceFlags::default()
            },
        );
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
        let proxy = std::env::var("DISCORD_API_BASE")
            .ok()
            .filter(|value| !value.is_empty());
        let executor = match ActionExecutor::with_proxy(token.to_owned(), proxy) {
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
        Some(Arc::new(Self {
            pool,
            executor,
            router,
            leveling,
            guild_id,
            automations: features.automations,
            registry_synced: tokio::sync::Mutex::new(false),
            attempts: AtomicU64::new(now_millis_for_test().max(0) as u64),
        }))
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
        let leveling = LevelingRuntime::new(
            pool.clone(),
            Arc::new(executor.clone()),
            guild_id,
            two_bot_core::OnboardingGates {
                mode: two_bot_core::OnboardingMode::Legacy,
                dry_run: false,
            },
        );
        Arc::new(Self {
            pool,
            executor,
            router,
            leveling,
            guild_id,
            automations,
            registry_synced: tokio::sync::Mutex::new(false),
            attempts: AtomicU64::new(now_millis_for_test().max(0) as u64),
        })
    }

    /// Shares this runtime's pool, executor/pacing and onboarding gates with
    /// the ordered award path; only this runtime dispatches interactions.
    pub fn leveling(&self) -> LevelingRuntime {
        self.leveling.clone()
    }

    /// Detached dispatch for one gateway event. Clones the payload and spawns
    /// so the shard loop never awaits runtime work; the DB claims tolerate
    /// the reorder/crash windows spawning opens.
    ///
    /// READY replaces the full merged set; a first RESUMED also synchronizes
    /// this process's definitions/gates, even without a preceding READY.
    pub fn dispatch(self: &Arc<Self>, event: &Event) {
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
                drop(tokio::spawn(async move {
                    runtime.on_interaction(&interaction).await;
                }));
            }
            Event::Ready(ready) => {
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

    /// Publish the ONE complete merged registry (legacy `CommandRegistry::sync`
    /// on `ready`). `publish_set` assembles every gated builtin plus DB custom
    /// rows — none on `main` yet, so `&[]` — and `set_guild_commands` is a
    /// full replace, making a duplicate READY idempotent rather than stale.
    /// RESUMED supplies no application id: resolve it through the same executor
    /// and synchronize once per process. Failed syncs remain eligible to retry
    /// on a later gateway connection event, never a polling timer.
    pub(crate) async fn publish_registry(&self, application_id: Option<u64>) {
        let mut synced = self.registry_synced.lock().await;
        if application_id.is_none() && *synced {
            return;
        }
        let application_id = match application_id {
            Some(id) => id,
            None => match self.executor.current_application_id().await {
                Ok(id) => id,
                Err(err) => {
                    warn!(error = %err, "application lookup failed; publish skipped");
                    return;
                }
            },
        };
        let defs = match self.router.publish_set(&[]) {
            Ok(defs) => defs,
            Err(err) => {
                warn!(error = %err, "command registry failed to assemble; publish skipped");
                return;
            }
        };
        let commands = publish_commands(&defs);
        if let Err(err) = self
            .executor
            .publish_guild_commands(application_id, self.guild_id, &commands)
            .await
        {
            warn!(error = %err, "command registry publish failed");
        } else {
            *synced = true;
        }
    }

    /// Route all slash commands through the shared router. Refusals get the
    /// existing ephemeral text; accepted builtins without a wired slice get
    /// an unavailable reply. Sticky/feed defer ephemerally; leveling sends
    /// its own immediate callback. Router Ignore (unknown/guild) stays silent.
    pub(crate) async fn on_interaction(&self, interaction: &Interaction) {
        let RoutedInteraction::Slash { name, outcome } =
            route_interaction(&self.router, interaction, None)
        else {
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
            "sticky" | "sticky-remove" => Some(HandlerId::AutomationAdmin),
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
        if let Err(err) = self
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
            warn!(interaction_id = %interaction.id.get(), command = %name, error = %err, "command defer failed");
            return;
        }
        match name.as_str() {
            "sticky" => self.sticky_set(interaction).await,
            "sticky-remove" => self.sticky_remove(interaction).await,
            "feed-add" => self.feed_add(interaction).await,
            "feed-remove" => self.feed_remove(interaction).await,
            "feed-list" => self.feed_list(interaction).await,
            _ => {}
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
            enabled: self.router.gates().announcements,
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

/// The runtime's router for tests that bypass `from_env`'s env reads:
/// sticky + feed + leveling markers, matching `from_env`'s registrations.
#[cfg(test)]
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
