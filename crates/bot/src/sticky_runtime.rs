//! Sticky runtime wiring (TOG-10309, S4 slice of TOG-9809).
//!
//! Composes the three merged halves: the shared interaction router
//! (TOG-10075) decides route/refusal for `/sticky` and `/sticky-remove`,
//! the shared REST executor (TOG-10076) performs every Discord side effect,
//! and the sticky domain + sqlx store (TOG-10082) owns validation, the
//! debounce decision, and the atomic claim. This module builds no private
//! dispatcher or HTTP client: gateway events arrive via [`Self::dispatch`]
//! and are handled entirely through those interfaces.
//!
//! Pinned legacy order (`sticky.rs` header): claim → post replacement →
//! record → best-effort delete previous → audit. A failed post releases the
//! claim and writes `post_failed`; `record` losing the claim means the just-
//! posted message is an orphan and is deleted with no audit row; a hold
//! (debounce or lost claim) writes nothing.

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
    funnel::now_millis_for_test,
    sticky::{
        activity_eligible, decide_activity, normalize_debounce, sticky_removed_reply,
        sticky_set_reply, store, validate_body, ActivityDecision, ActivityOutcome, PutSticky,
        RemoveOutcome, StickyAudit, StickyAuditAction, StickyAuditOutcome,
    },
    FeatureGates, HandlerId, InteractionHandler, InteractionRouter, ModerationGates, RouterGates,
    SlashOutcome, SurfaceFlags,
};
use two_bot_discord::{response_for_slash, route_interaction, ActionExecutor, RoutedInteraction};

/// Audit-log reason for retiring the previous sticky (legacy audits carry a
/// free-text reason; kept short — `audit_reason` caps at 512 chars).
const RETIRE_REASON: &str = "sticky re-post";
/// Reason for deleting a replacement post whose claim moved on.
const ORPHAN_REASON: &str = "sticky re-post rolled back";
/// Reason for the `/sticky-remove` Discord cleanup delete.
const REMOVE_REASON: &str = "sticky-remove";
/// Safe reply when the store fails — never leak sqlx internals to Discord.
const STORE_FAILURE_REPLY: &str = "Sticky command failed; try again.";
/// Reply when the interaction arrives without a channel (pathological —
/// Discord always sends `channel_id` for guild slash commands).
const NO_CHANNEL_REPLY: &str = "This command only works in a channel.";

/// Router handler marker: this runtime is the `AutomationAdmin` executor for
/// the sticky commands. Registration documents the ownership the router
/// outcome names; execution happens in [`StickyRuntime::on_interaction`].
#[derive(Debug)]
struct StickyHandler;

impl InteractionHandler for StickyHandler {
    fn id(&self) -> HandlerId {
        HandlerId::AutomationAdmin
    }
}

/// The sticky slice's runtime: shared router + shared executor + the sqlx
/// store, driven by gateway dispatches. Cheap to clone behind `Arc`; every
/// `dispatch` spawns detached work because twilight only drives heartbeats
/// while the shard is polled.
pub struct StickyRuntime {
    pool: Pool<Postgres>,
    executor: ActionExecutor,
    router: InteractionRouter,
    /// Configured guild (`GUILD_ID`); also the router's guild fence.
    guild_id: u64,
    /// `TWO_AUTOMATIONS=1`: fast-path gate for the message hook (the router
    /// still answers `/sticky*` refusals when it is off).
    automations: bool,
    /// Monotonic attempt ids: one value mints both the DB claim token
    /// (`s{n:x}`, ≤25 chars) and the numeric post nonce for dedupe.
    attempts: AtomicU64,
}

impl StickyRuntime {
    /// Build the runtime from process env gates + the optional
    /// `DISCORD_API_BASE` proxy override. Returns `None` — gateway still
    /// boots — when gate parsing or executor construction fails, so bad
    /// env cannot take the shard down.
    #[must_use]
    pub fn from_env(pool: Pool<Postgres>, token: &str, guild_id: u64) -> Option<Arc<Self>> {
        let features = match FeatureGates::from_env() {
            Ok(features) => features,
            Err(err) => {
                warn!(error = %err, "feature gates invalid; sticky runtime disabled");
                return None;
            }
        };
        let moderation = match ModerationGates::from_env() {
            Ok(moderation) => moderation,
            Err(err) => {
                warn!(error = %err, "moderation gates invalid; sticky runtime disabled");
                return None;
            }
        };
        let gates = RouterGates::from_slices(
            Some(guild_id),
            &features,
            &moderation,
            SurfaceFlags::default(),
        );
        let mut router = InteractionRouter::new(gates);
        router.register(Box::new(StickyHandler));
        let proxy = std::env::var("DISCORD_API_BASE")
            .ok()
            .filter(|value| !value.is_empty());
        let admission = match two_bot_core::send_admission::PgSendAdmission::new(pool.clone(), token) {
            Ok(admission) => Arc::new(admission),
            Err(err) => {
                warn!(error = %err, "send admission invalid; sticky runtime disabled");
                return None;
            }
        };
        let executor = match ActionExecutor::with_admission(token.to_owned(), proxy, admission) {
            Ok(executor) => executor,
            Err(err) => {
                warn!(error = %err, "REST executor failed to build; sticky runtime disabled");
                return None;
            }
        };
        Some(Arc::new(Self {
            pool,
            executor,
            router,
            guild_id,
            automations: features.automations,
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
        Arc::new(Self {
            pool,
            executor,
            router,
            guild_id,
            automations,
            attempts: AtomicU64::new(now_millis_for_test().max(0) as u64),
        })
    }

    /// Detached dispatch for one gateway event. Clones the payload and spawns
    /// so the shard loop never awaits sticky work; the DB claim tolerates the
    /// reorder/crash windows spawning opens.
    pub fn dispatch(self: &Arc<Self>, event: &Event) {
        match event {
            Event::MessageCreate(message) => {
                let runtime = Arc::clone(self);
                let message = message.0.clone();
                // Dropping the JoinHandle detaches the task — exactly what the
                // shard loop needs (never await sticky work while polling).
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
            _ => {}
        }
    }

    /// Route one interaction through the shared router: refusals get the
    /// ephemeral legacy text, `Handled` sticky commands run their slice, and
    /// everything else (including `command`/`command-remove`, owned by the
    /// custom-commands slice) is ignored.
    pub(crate) async fn on_interaction(&self, interaction: &Interaction) {
        let RoutedInteraction::Slash { name, outcome } =
            route_interaction(&self.router, interaction, None)
        else {
            return;
        };
        if !matches!(name.as_str(), "sticky" | "sticky-remove") {
            return;
        }
        if let Some(response) = response_for_slash(&outcome) {
            self.answer(interaction, response).await;
            return;
        }
        if outcome
            != (SlashOutcome::Handled {
                handler: HandlerId::AutomationAdmin,
            })
        {
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
            warn!(interaction_id = %interaction.id.get(), error = %err, "sticky defer failed");
            return;
        }
        match name.as_str() {
            "sticky" => self.sticky_set(interaction).await,
            "sticky-remove" => self.sticky_remove(interaction).await,
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

/// The runtime's router for tests that bypass `from_env`'s env reads.
#[cfg(test)]
pub(crate) fn router_with_sticky(gates: RouterGates) -> InteractionRouter {
    let mut router = InteractionRouter::new(gates);
    router.register(Box::new(StickyHandler));
    router
}

/// Ephemeral channel-message reply (same shape the router's refusal builder
/// produces).
#[cfg(test)]
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
