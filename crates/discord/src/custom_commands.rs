//! Custom-command execution through the shared router and REST executor.
//! Database mutations commit before the ONE full registry is republished.
//! No gateway listener here: the runtime injects this service into its dispatch.

use std::{
    sync::{Arc, PoisonError, RwLock},
    time::Duration,
};

use sqlx::PgPool;
use tokio::sync::Mutex;
use twilight_model::{
    application::interaction::{
        application_command::{CommandData, CommandOptionValue},
        Interaction, InteractionData,
    },
    channel::{
        message::{AllowedMentions, MessageFlags},
        Message,
    },
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
};
use two_bot_core::{
    custom_command_service as service, custom_command_store as store,
    custom_commands::{
        accepted_text_trigger, builtin_command_names, format_command_list_page,
        format_draft_preview, format_inspect_miss, format_inspect_preview, format_inspect_summary,
        format_inspect_template, inspect_preview_context_unavailable, invalid_inspect_option_reply,
        invalid_inspect_view_reply, render_template, validate_inspect_name, AuditRecord,
        AutomationMessageAcceptance, CloneCommandInput, CommandError, InspectView, PutCommandInput,
        StoredCommand, TemplateContext, ToggleOutcome, INVALID_COMMAND_LIST_PAGE,
        MAX_INSPECT_ARGUMENT_UNITS,
    },
    router::{replies::InteractionReply, InteractionHandler},
    HandlerId, InteractionRouter, SlashOutcome,
};

use crate::{
    automation_admission::ActorCooldowns,
    interactions::{
        publish_commands, refusal_response, route_interaction, text_response, RoutedInteraction,
    },
    ActionExecutor,
};

/// Sanitized errors: never log SQL values, interaction tokens or templates.
#[derive(Debug, thiserror::Error)]
pub enum CustomCommandError {
    #[error("Custom-command storage failed.")]
    Storage,
    #[error("Custom-command Discord delivery failed.")]
    Delivery,
    #[error("Custom-command Discord delivery outcome is unknown.")]
    DeliveryUnknown,
    #[error("Custom-command registry publication failed; the saved definition is unchanged.")]
    Publication,
    #[error("Custom-command context is unavailable.")]
    Context,
    #[error("Custom-command template could not be rendered.")]
    Render,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextCommandOutcome {
    Ignored,
    /// The automod verdict refused the create before any trigger lookup.
    /// Telemetry counts this separately from [`Self::Ignored`]; dispatch
    /// still sends nothing, like ignored.
    Refused,
    /// A distinct candidate from this actor is within the local window.
    CoolingDown,
    /// A prior invocation may have sent a reply. Never resend automatically.
    AlreadyAttempted,
    Delivered,
}

#[derive(Debug)]
struct Registration(HandlerId);

impl InteractionHandler for Registration {
    fn id(&self) -> HandlerId {
        self.0
    }
}

type PublishedCommands = Option<Arc<[two_bot_core::CommandDefinition]>>;

/// Fixed availability copy for a failed definition lookup: generic fixed
/// text, no command name, error, or transport detail. Pinned by the lookup
/// outage acceptance suites in both crates and quoted in
/// `docs/custom-commands.md`; keep every copy identical — a different copy
/// needs a Test Author correction.
const LOOKUP_OUTAGE_REPLY: &str =
    "Custom commands are temporarily unavailable. Please try again shortly.";

#[derive(Clone)]
pub struct CustomCommandRuntime {
    pool: PgPool,
    router: Arc<InteractionRouter>,
    executor: ActionExecutor,
    application_id: u64,
    // All clones serialize mutation + read + publish, not just the HTTP PUT.
    // Otherwise an older full-set snapshot can overwrite a newer addition.
    registry: Arc<Mutex<()>>,
    text_cooldowns: Arc<Mutex<ActorCooldowns>>,
    /// Exact merged set from the last confirmed full-registry PUT. Clones share
    /// this snapshot; committed DB changes alone must not advance discovery.
    published_commands: Arc<RwLock<PublishedCommands>>,
}

impl CustomCommandRuntime {
    pub fn register(router: &mut InteractionRouter) {
        router.register(Box::new(Registration(HandlerId::AutomationAdmin)));
        router.register(Box::new(Registration(HandlerId::AutomationCustom)));
    }

    pub fn new(
        pool: PgPool,
        router: Arc<InteractionRouter>,
        executor: ActionExecutor,
        application_id: u64,
    ) -> Self {
        Self {
            pool,
            router,
            executor,
            application_id,
            registry: Arc::new(Mutex::new(())),
            text_cooldowns: Arc::new(Mutex::new(ActorCooldowns::default())),
            published_commands: Arc::new(RwLock::new(None)),
        }
    }

    /// Clone the last confirmed publication without a DB read or a lock held
    /// across the reply's I/O. `None` means publication is not yet confirmed.
    pub fn published_commands(&self) -> Option<Arc<[two_bot_core::CommandDefinition]>> {
        self.published_commands
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Used on READY and after successful add/remove. Never publish just the
    /// custom subset: the endpoint replaces the entire guild registry.
    pub async fn sync_registry(&self) -> Result<(), CustomCommandError> {
        let _guard = self.registry.lock().await;
        self.publish_locked().await
    }

    async fn publish_locked(&self) -> Result<(), CustomCommandError> {
        let gates = self.router.gates();
        let guild = gates.configured_guild.ok_or(CustomCommandError::Context)?;
        let rows = if gates.automations {
            store::list_commands(&self.pool, &guild.to_string())
                .await
                .map_err(|_| CustomCommandError::Storage)?
        } else {
            Vec::new()
        };
        let custom = rows
            .iter()
            .map(StoredCommand::registry_entry)
            .collect::<Vec<_>>();
        let defs = self
            .router
            .publish_set(&custom)
            .map_err(|_| CustomCommandError::Publication)?;
        self.executor
            .publish_guild_commands(self.application_id, guild, &publish_commands(&defs))
            .await
            .map_err(|_| CustomCommandError::Publication)?;
        let snapshot: Arc<[two_bot_core::CommandDefinition]> = defs.into();
        *self
            .published_commands
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(snapshot);
        Ok(())
    }

    /// Returns false for another feature's interaction. Guild/application fences
    /// precede every lookup, and builtin names never fall through to DB rows.
    /// The guild name comes from the shared cache, never from a synthetic ID.
    /// A failed definition lookup is not a definitive miss: when the surface
    /// is in scope the runtime owns the interaction with one fixed ephemeral
    /// availability reply instead of executing a cached or guessed definition.
    pub async fn handle_interaction(
        &self,
        interaction: &Interaction,
        guild_name: Option<&str>,
    ) -> Result<bool, CustomCommandError> {
        // Receipt-relative acknowledgement budget: the bounded pre-wire retry
        // below must cover this lookup plus admission/transport, leaving margin
        // below Discord's three-second initial deadline.
        let received = tokio::time::Instant::now();
        if interaction.application_id.get() != self.application_id
            || interaction.guild_id.map(|id| id.get()) != self.router.gates().configured_guild
            || interaction.guild_id.is_none()
        {
            return Ok(false);
        }
        let Some(InteractionData::ApplicationCommand(data)) = interaction.data.as_ref() else {
            return Ok(false);
        };
        let management = matches!(
            data.name.as_str(),
            "command"
                | "command-remove"
                | "command-list"
                | "command-inspect"
                | "command-clone"
                | "command-disable"
                | "command-enable"
        );
        let builtin = builtin_command_names().contains(&data.name);
        if builtin && !management {
            return Ok(false);
        }
        let guild = interaction.guild_id.expect("fenced guild").to_string();
        // Unknown commands must remain silent, so look up before deferring.
        // Bound that lookup well inside Discord's three-second initial deadline.
        let row = if management {
            None
        } else {
            match tokio::time::timeout(
                Duration::from_millis(750),
                store::get_command(&self.pool, &guild, &data.name),
            )
            .await
            {
                Ok(Ok(row)) => row,
                // A lookup outage is not a definitive miss: unknown commands
                // must stay silent, but a failed lookup cannot prove absence.
                // Answer once with the fixed availability copy when the
                // surface is in scope, instead of executing a cached or
                // guessed definition.
                Ok(Err(_)) | Err(_) => {
                    return self.answer_lookup_outage(interaction, received).await;
                }
            }
        };
        let RoutedInteraction::Slash { outcome, .. } = route_interaction(
            &self.router,
            interaction,
            row.as_ref().map(|row| row.enabled),
        ) else {
            return Ok(false);
        };
        match outcome {
            SlashOutcome::Refuse { refusal } => {
                // Bounded retry for provably pre-wire Blocked contention only;
                // the receipt-relative deadline covers the pre-defer lookup
                // above. Uncertain sends are never retried.
                tokio::time::timeout_at(
                    received + Duration::from_millis(crate::executor::RECEIPT_ADMISSION_BUDGET_MS),
                    self.executor.answer_interaction_with_blocked_retry(
                        interaction.id.get(),
                        &interaction.token,
                        &refusal_response(refusal),
                    ),
                )
                .await
                .map_err(|_| CustomCommandError::Delivery)?
                .map_err(|_| CustomCommandError::Delivery)?;
                Ok(true)
            }
            SlashOutcome::Handled {
                handler: HandlerId::AutomationAdmin,
            } if management => {
                self.defer(interaction, true, received).await?;
                let reply = self.manage(interaction, data, &guild, guild_name).await;
                let body = reply
                    .as_deref()
                    .unwrap_or("Custom-command operation failed. Please try again.");
                // Inspect views show saved text exactly as stored: mention
                // safety comes from the reply's explicit empty
                // allowed_mentions, never from rewriting `@everyone`/`@here`.
                // Every other completion keeps the neutralizing path.
                if data.name == "command-inspect" {
                    self.complete_without_mention_neutralization(interaction, body)
                        .await?;
                } else {
                    self.complete(interaction, body).await?;
                }
                reply?;
                Ok(true)
            }
            SlashOutcome::Handled {
                handler: HandlerId::AutomationCustom,
            } => {
                if let Err(defer_error) = self.defer(interaction, false, received).await {
                    // A refused defer proves no Discord-side effect, so audit
                    // the failed run: operators must distinguish "attempted
                    // and refused" from "never attempted". An ambiguous defer
                    // (uncertain wire outcome) stays unaudited so a later
                    // retry of the same interaction cannot double-record.
                    if !matches!(defer_error, CustomCommandError::Delivery) {
                        return Err(defer_error);
                    }
                    let row = row.ok_or(CustomCommandError::Context)?;
                    let actor = interaction.author().ok_or(CustomCommandError::Context)?;
                    let record = AuditRecord::run(
                        &guild,
                        &actor.id.to_string(),
                        &row.name,
                        false,
                        Some("delivery_failed"),
                    );
                    self.audit_slash_run(&record, interaction.id.get()).await?;
                    return Err(CustomCommandError::Delivery);
                }
                let row = row.ok_or(CustomCommandError::Context)?;
                let actor = interaction.author().ok_or(CustomCommandError::Context)?;
                let context =
                    guild_name
                        .zip(interaction.channel.as_ref())
                        .map(|(server, channel)| TemplateContext {
                            user: format!("<@{}>", actor.id),
                            username: actor.name.clone(),
                            server: server.to_owned(),
                            channel: format!("<#{}>", channel.id),
                        });
                let rendered = context
                    .as_ref()
                    .and_then(|context| render_template(&row.template, context).ok());
                let (result, reason) = match rendered {
                    Some(content) => (self.complete(interaction, &content).await, None),
                    None => (
                        self.complete(
                            interaction,
                            "Custom-command context or template is unavailable.",
                        )
                        .await,
                        Some("render_failed"),
                    ),
                };
                // An uncertain completion proves nothing about the run, so it
                // stays unaudited (like an ambiguous defer): no definitive
                // `delivery_failed` fact, and a later explicit retry of the
                // same interaction records only its own outcome. A render or
                // context failure is definitive and local, so it still audits.
                if matches!(result, Err(CustomCommandError::DeliveryUnknown)) && reason.is_none() {
                    return Err(CustomCommandError::DeliveryUnknown);
                }
                let ok = result.is_ok() && reason.is_none();
                let record = AuditRecord::run(
                    &guild,
                    &actor.id.to_string(),
                    &row.name,
                    ok,
                    reason.or(if result.is_err() {
                        Some("delivery_failed")
                    } else {
                        None
                    }),
                );
                self.audit_slash_run(&record, interaction.id.get()).await?;
                result?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Record a refused prefix candidate when the gateway worker rejects a
    /// message before detached command dispatch. This is telemetry only: it
    /// never performs a lookup or starts automation work.
    pub fn record_refused_prefix_trigger(&self, message: &Message, text_commands_enabled: bool) {
        let in_scope = message.guild_id.is_some()
            && message.guild_id.map(|id| id.get()) == self.router.gates().configured_guild
            && message.webhook_id.is_none();
        if in_scope
            && accepted_text_trigger(
                self.router.gates().automations,
                text_commands_enabled,
                message.author.bot,
                &message.content,
                &builtin_command_names(),
            )
            .is_some()
        {
            two_bot_core::metrics::global().prefix_trigger_refused("verdict");
        }
    }

    /// Called with an explicit moderation acceptance, never a default. Never
    /// derive acceptance from MessageCreate, funnel capture, or whether
    /// deletion succeeded. Unknown errors fail closed (unlike the legacy
    /// emitter's fail-open null result).
    pub async fn handle_message(
        &self,
        message: &Message,
        acceptance: AutomationMessageAcceptance,
        text_commands_enabled: bool,
        guild_name: Option<&str>,
    ) -> Result<TextCommandOutcome, CustomCommandError> {
        if !acceptance.permits_automations() {
            self.record_refused_prefix_trigger(message, text_commands_enabled);
            return Ok(TextCommandOutcome::Refused);
        }
        if message.guild_id.is_none()
            || message.guild_id.map(|id| id.get()) != self.router.gates().configured_guild
            || message.webhook_id.is_some()
        {
            return Ok(TextCommandOutcome::Ignored);
        }
        let Some(trigger) = accepted_text_trigger(
            self.router.gates().automations,
            text_commands_enabled,
            message.author.bot,
            &message.content,
            &builtin_command_names(),
        ) else {
            return Ok(TextCommandOutcome::Ignored);
        };
        let guild_id = message.guild_id.expect("fenced guild").get();
        // Bound even unknown prefix candidates before their SQL lookup. Clones
        // share the window; the permanent attempt claim below is unchanged.
        if !self.text_cooldowns.lock().await.admit(
            guild_id,
            message.author.id.get(),
            message.id.get(),
            tokio::time::Instant::now(),
        ) {
            return Ok(TextCommandOutcome::CoolingDown);
        }
        let guild = guild_id.to_string();
        let Some(row) = store::find_text_trigger(&self.pool, &guild, &trigger)
            .await
            .map_err(|_| CustomCommandError::Storage)?
        else {
            return Ok(TextCommandOutcome::Ignored);
        };
        // Imported/custom rows must not execute a reserved slash name either.
        if builtin_command_names().contains(&row.name) {
            return Ok(TextCommandOutcome::Ignored);
        }
        let actor = message.author.id.to_string();
        if !store::claim_text_attempt(
            &self.pool,
            &guild,
            &actor,
            &row.name,
            message.id.get(),
            &two_bot_core::now_iso(),
        )
        .await
        .map_err(|_| CustomCommandError::Storage)?
        {
            return Ok(TextCommandOutcome::AlreadyAttempted);
        }
        // The committed attempt is never cleared, including on cancellation,
        // rendering/audit failure, or an ambiguous network response. Nonce
        // enforcement is additional protection, not our durable replay guard.
        let rendered = guild_name
            .ok_or(CustomCommandError::Context)
            .and_then(|server| {
                render_template(
                    &row.template,
                    &TemplateContext {
                        user: format!("<@{}>", message.author.id),
                        username: message.author.name.clone(),
                        server: server.to_owned(),
                        channel: format!("<#{}>", message.channel_id),
                    },
                )
                .map_err(|_| CustomCommandError::Render)
            });
        let (result, reason) = match rendered {
            Ok(content) => {
                let result = match self
                    .executor
                    .post_message(
                        &message.channel_id.to_string(),
                        &content,
                        Some(message.id.get()),
                    )
                    .await
                {
                    Ok(_) => Ok(()),
                    Err(error) if error.is_safe_pre_mutation() => Err(CustomCommandError::Delivery),
                    // Timeout/transport/5xx/429 cannot prove that no POST took
                    // effect. Keep the attempt unresolved; never append a
                    // definitive failed result or automatically resend it.
                    Err(_) => return Err(CustomCommandError::DeliveryUnknown),
                };
                let reason = result.as_ref().err().map(|_| "delivery_failed");
                (result, reason)
            }
            Err(CustomCommandError::Context) => (
                Err(CustomCommandError::Context),
                Some("context_unavailable"),
            ),
            Err(error) => (Err(error), Some("render_failed")),
        };
        self.audit(
            &AuditRecord::run(&guild, &actor, &row.name, result.is_ok(), reason),
            &format!("custom:text:result:{}", message.id),
        )
        .await?;
        result?;
        Ok(TextCommandOutcome::Delivered)
    }

    /// One bounded fixed-text ephemeral reply for a failed definition lookup.
    /// Builtin names (which carry refusals) and foreign/gated inputs never
    /// reach the lookup, so the error path only sees `Unknown` or silence: a
    /// disabled surface, a voice-owned name, or anything else stays unowned
    /// with no I/O. The reply lands inside the existing receipt budget,
    /// leaving margin below Discord's three-second acknowledgement deadline.
    /// The interaction stays owned after the single attempt whatever the wire
    /// outcome: the lookup already failed, so no fallback handler may execute
    /// a cached or guessed definition or send a second reply.
    async fn answer_lookup_outage(
        &self,
        interaction: &Interaction,
        received: tokio::time::Instant,
    ) -> Result<bool, CustomCommandError> {
        let RoutedInteraction::Slash { outcome, .. } =
            route_interaction(&self.router, interaction, None)
        else {
            return Ok(false);
        };
        match outcome {
            SlashOutcome::Unknown if self.router.gates().automations => {
                let response = text_response(InteractionReply::new(LOOKUP_OUTAGE_REPLY, true));
                // One attempt only, inside the receipt budget. The attempt's
                // wire outcome (provably pre-wire or uncertain alike) is
                // spent, never retried, and never propagated: ownership is
                // decided by the failed lookup, not by the reply landing.
                let _attempted = tokio::time::timeout_at(
                    received + Duration::from_millis(crate::executor::RECEIPT_ADMISSION_BUDGET_MS),
                    self.executor.answer_interaction_with_blocked_retry(
                        interaction.id.get(),
                        &interaction.token,
                        &response,
                    ),
                )
                .await;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn defer(
        &self,
        interaction: &Interaction,
        ephemeral: bool,
        received: tokio::time::Instant,
    ) -> Result<(), CustomCommandError> {
        let response = InteractionResponse {
            kind: InteractionResponseType::DeferredChannelMessageWithSource,
            data: Some(InteractionResponseData {
                flags: ephemeral.then_some(MessageFlags::EPHEMERAL),
                allowed_mentions: Some(AllowedMentions::default()),
                ..Default::default()
            }),
        };
        // Same receipt-relative bounded pre-wire retry as the refusal path:
        // a Blocked attempt never reached the wire, so retrying it cannot
        // double-acknowledge. Uncertain sends are never retried.
        //
        // Receipt-deadline expiry is ambiguous: the defer may have landed.
        // Only provably pre-wire failures map to `Delivery`; everything
        // else is `DeliveryUnknown` and must stay unaudited so a later
        // retry cannot double-record.
        tokio::time::timeout_at(
            received + Duration::from_millis(crate::executor::RECEIPT_ADMISSION_BUDGET_MS),
            self.executor.answer_interaction_with_blocked_retry(
                interaction.id.get(),
                &interaction.token,
                &response,
            ),
        )
        .await
        .map_err(|_| CustomCommandError::DeliveryUnknown)?
        .map_err(delivery_error)
    }

    /// Inspect-view completion: same retry/error contract as [`Self::complete`],
    /// without mention neutralization — the ephemeral reply's explicit empty
    /// `allowed_mentions` is the mention safety, and the admin must see the
    /// saved text exactly as stored.
    async fn complete_without_mention_neutralization(
        &self,
        interaction: &Interaction,
        content: &str,
    ) -> Result<(), CustomCommandError> {
        self.executor
            .edit_interaction_response_without_mention_neutralization_with_blocked_retry(
                self.application_id,
                &interaction.token,
                content,
            )
            .await
            .map_err(delivery_error)
    }

    async fn complete(
        &self,
        interaction: &Interaction,
        content: &str,
    ) -> Result<(), CustomCommandError> {
        // Bounded retry for provably pre-wire Blocked contention only. The
        // deferred completion carries no three-second deadline, but a Blocked
        // attempt never reached the wire, so retrying it cannot repeat an
        // accepted edit. Uncertain sends are never retried.
        //
        // A proven rejection maps to `Delivery`; an ambiguous transport
        // outcome (timeout, transport failure, 5xx, 429 — including an
        // accepted edit whose response is lost) maps to `DeliveryUnknown`,
        // like the defer and prefix paths.
        self.executor
            .edit_interaction_response_with_blocked_retry(
                self.application_id,
                &interaction.token,
                content,
            )
            .await
            .map_err(delivery_error)
    }

    // Explicit re-entry still attempts Discord delivery and returns that
    // attempt's outcome. The immutable slash fact is first-write-wins across
    // both refusal and completion, never an execution claim or a resend loop.
    async fn audit_slash_run(
        &self,
        record: &AuditRecord,
        interaction_id: u64,
    ) -> Result<(), CustomCommandError> {
        store::audit_once(
            &self.pool,
            record,
            &format!("custom:run:{interaction_id}"),
            &two_bot_core::now_iso(),
        )
        .await
        .map_err(|_| CustomCommandError::Storage)
    }

    async fn audit(&self, record: &AuditRecord, id: &str) -> Result<(), CustomCommandError> {
        store::audit(&self.pool, record, id, &two_bot_core::now_iso())
            .await
            .map_err(|_| CustomCommandError::Storage)
    }

    /// Private saved-response inspection or unsaved draft preview: one
    /// ephemeral content-only view, never execution or mutation. Drafts never
    /// access storage; saved mode reads live only after validation, so deleted
    /// definitions immediately miss.
    async fn inspect(
        &self,
        interaction: &Interaction,
        data: &CommandData,
        guild: &str,
        guild_name: Option<&str>,
    ) -> String {
        if data.options.iter().any(|option| {
            !matches!(
                option.name.as_str(),
                "name" | "draft" | "view" | "arguments"
            )
        }) {
            return invalid_inspect_option_reply(
                "options",
                "use only name or draft, with optional view and arguments.",
            );
        }
        let (name, draft) = match (
            single_string_option(data, "name"),
            single_string_option(data, "draft"),
        ) {
            (SingleOption::Value(name), SingleOption::Missing) => (Some(name), None),
            (SingleOption::Missing, SingleOption::Value(draft)) => (None, Some(draft)),
            _ => {
                return "Invalid input: pass exactly one text name or draft, never both or duplicates. Examples: /command-inspect name:faq or /command-inspect draft:Hello".to_owned();
            }
        };
        let view = match single_string_option(data, "view") {
            SingleOption::Missing if draft.is_some() => InspectView::Preview,
            SingleOption::Missing => InspectView::Summary,
            SingleOption::Malformed => return invalid_inspect_view_reply(None),
            SingleOption::Value(raw) => match InspectView::parse(raw) {
                Some(view) => view,
                None => return invalid_inspect_view_reply(Some(raw)),
            },
        };
        match single_string_option(data, "arguments") {
            SingleOption::Missing => {}
            SingleOption::Malformed => {
                return invalid_inspect_option_reply(
                    "arguments",
                    "expected a single text sample of invocation arguments.",
                );
            }
            SingleOption::Value(sample) => {
                if sample.encode_utf16().count() > MAX_INSPECT_ARGUMENT_UNITS {
                    return invalid_inspect_option_reply(
                        "arguments",
                        "sample arguments must be at most 500 characters.",
                    );
                }
            }
        }
        // Both preview modes use invocation context, never sample arguments.
        let context = || {
            interaction
                .author()
                .zip(interaction.channel.as_ref())
                .zip(guild_name)
                .map(|((actor, channel), server)| TemplateContext {
                    user: format!("<@{}>", actor.id),
                    username: actor.name.clone(),
                    server: server.to_owned(),
                    channel: format!("<#{}>", channel.id),
                })
        };
        if let Some(draft) = draft {
            if view != InspectView::Preview {
                return "Invalid draft view: only preview is available for unsaved text. Use /command-inspect draft:Hello view:preview".to_owned();
            }
            return match context() {
                Some(context) => format_draft_preview(draft, &context),
                None => "Draft preview is unavailable: command context is missing. Please try /command-inspect draft:Hello again.".to_owned(),
            };
        }
        let name = match validate_inspect_name(name.unwrap_or_default(), &builtin_command_names()) {
            Ok(name) => name,
            Err(reason) => {
                return invalid_inspect_option_reply("command name", &reason.to_string());
            }
        };
        let row = match store::get_command(&self.pool, guild, &name).await {
            Ok(row) => row,
            Err(_) => return LOOKUP_OUTAGE_REPLY.to_owned(),
        };
        let Some(row) = row else {
            return format_inspect_miss(&name);
        };
        match view {
            InspectView::Summary => format_inspect_summary(&row),
            InspectView::Template => format_inspect_template(&row),
            InspectView::Preview => match context() {
                Some(context) => format_inspect_preview(&row, &context),
                None => inspect_preview_context_unavailable(&row.name),
            },
        }
    }

    async fn clone_saved(
        &self,
        interaction: &Interaction,
        data: &CommandData,
        guild: &str,
        actor: &str,
    ) -> Result<String, CustomCommandError> {
        // Forged options must not silently select a partial value or add a
        // template, state or trigger override to this deliberately narrow API.
        if data
            .options
            .iter()
            .any(|option| !matches!(option.name.as_str(), "source" | "destination"))
        {
            return Ok("Only source and destination name options are supported.".to_owned());
        }
        let (SingleOption::Value(source), SingleOption::Value(destination)) = (
            single_string_option(data, "source"),
            single_string_option(data, "destination"),
        ) else {
            return Ok("Provide one source name and one new destination name as text.".to_owned());
        };
        let _guard = self.registry.lock().await;
        let saved = match service::clone_command(
            &self.pool,
            self.router.gates().automations,
            guild,
            actor,
            &CloneCommandInput {
                source: source.to_owned(),
                destination: destination.to_owned(),
            },
            &format!("custom:manage:{}", interaction.id),
            &two_bot_core::now_iso(),
        )
        .await
        {
            Ok(saved) => saved,
            Err(service::ServiceError::Storage(_)) => return Err(CustomCommandError::Storage),
            Err(service::ServiceError::Validation(CommandError::InvalidTemplate(_))) => {
                return Ok(
                    "Saved source template is invalid. Inspect and correct it before cloning."
                        .to_owned(),
                );
            }
            Err(error) => return Ok(error.to_string()),
        };
        let state = if saved.enabled { "enabled" } else { "disabled" };
        let reply = format!(
            "Saved /{} as {state}. The source text trigger was not copied; the source is unchanged.",
            saved.name
        );
        if self.publish_locked().await.is_err() {
            tracing::error!("custom-command clone committed but registry publication failed");
            return Ok(format!(
                "{reply} The command picker is not synchronized. Ask a bot administrator to resynchronize the registry; do not clone again."
            ));
        }
        Ok(reply)
    }

    async fn manage(
        &self,
        interaction: &Interaction,
        data: &CommandData,
        guild: &str,
        guild_name: Option<&str>,
    ) -> Result<String, CustomCommandError> {
        if data.name == "command-inspect" {
            // Routing already enforced guild/application, ManageGuild and the
            // feature gate. Reject forged types/duplicates before a store
            // read; validate name/view/arguments before any lookup.
            return Ok(self.inspect(interaction, data, guild, guild_name).await);
        }
        let actor = interaction
            .author_id()
            .ok_or(CustomCommandError::Context)?
            .to_string();
        if data.name == "command-clone" {
            return self.clone_saved(interaction, data, guild, &actor).await;
        }
        if data.name == "command-list" {
            // Routing already enforced guild/application, ManageGuild and the
            // feature gate. Reject forged types/duplicates before a store read.
            let mut options = data.options.iter().filter(|option| option.name == "page");
            let page = match options.next().map(|option| &option.value) {
                None => 1,
                Some(CommandOptionValue::Integer(value)) if *value > 0 => {
                    let Ok(page) = usize::try_from(*value) else {
                        return Ok(INVALID_COMMAND_LIST_PAGE.to_owned());
                    };
                    page
                }
                Some(_) => return Ok(INVALID_COMMAND_LIST_PAGE.to_owned()),
            };
            if options.next().is_some() {
                return Ok(INVALID_COMMAND_LIST_PAGE.to_owned());
            }
            let rows = store::list_commands(&self.pool, guild)
                .await
                .map_err(|_| CustomCommandError::Storage)?;
            return Ok(format_command_list_page(&rows, page).unwrap_or_else(str::to_owned));
        }
        if data.name == "command-disable" || data.name == "command-enable" {
            // Routing already enforced guild/application, ManageGuild and the
            // feature gate. The lifecycle takes exactly one text `name`:
            // forged types, duplicates and extra options refuse before any
            // store read or publication.
            return self.toggle(interaction, data, guild).await;
        }
        // The remaining management names are the two writers. Routing has
        // already enforced guild/application, ManageGuild and the feature
        // gate. Strictly decode their complete option shapes before the
        // registry lock, any store access, audit or publication: a forged
        // duplicate, wrong type, nested or unknown option must never let a
        // first-matching value silently win.
        let is_define = data.name == "command";
        let allowed: &[&str] = if is_define {
            &["name", "template", "description", "text-trigger"]
        } else {
            &["name"]
        };
        if data
            .options
            .iter()
            .any(|option| !allowed.contains(&option.name.as_str()))
        {
            return Ok(invalid_writer_option("option"));
        }
        let name = match single_string_option(data, "name") {
            SingleOption::Missing => return Ok("A command name is required.".to_owned()),
            SingleOption::Malformed => return Ok(invalid_writer_option("name")),
            SingleOption::Value(name) => name.to_lowercase(),
        };
        let define = if is_define {
            let template = match single_string_option(data, "template") {
                SingleOption::Missing => return Ok("A template is required.".to_owned()),
                SingleOption::Malformed => return Ok(invalid_writer_option("template")),
                SingleOption::Value(template) => template.to_owned(),
            };
            let description = match single_string_option(data, "description") {
                SingleOption::Missing => "Custom command".to_owned(),
                SingleOption::Malformed => return Ok(invalid_writer_option("description")),
                SingleOption::Value(description) => description.to_owned(),
            };
            let text_trigger = match single_string_option(data, "text-trigger") {
                SingleOption::Missing => None,
                SingleOption::Malformed => return Ok(invalid_writer_option("text-trigger")),
                SingleOption::Value(trigger) => Some(trigger.to_lowercase()),
            };
            Some(PutCommandInput {
                name: name.clone(),
                description,
                template,
                text_trigger,
            })
        } else {
            None
        };
        let audit_id = format!("custom:manage:{}", interaction.id);
        let at = two_bot_core::now_iso();
        let _guard = self.registry.lock().await;
        let (reply, resync) = if let Some(input) = define {
            match service::put(
                &self.pool,
                self.router.gates().automations,
                guild,
                &actor,
                &input,
                &audit_id,
                &at,
            )
            .await
            {
                Ok(decision) => (format!("Saved /{name}."), decision.resync_registry),
                Err(service::ServiceError::Storage(_)) => return Err(CustomCommandError::Storage),
                Err(error) => return Ok(error.to_string()),
            }
        } else {
            match service::delete(
                &self.pool,
                self.router.gates().automations,
                guild,
                &actor,
                &name,
                &audit_id,
                &at,
            )
            .await
            {
                Ok(decision) => (
                    if decision.deleted {
                        format!("Removed /{name}.")
                    } else {
                        format!("No command /{name} found.")
                    },
                    decision.resync_registry,
                ),
                Err(service::ServiceError::Storage(_)) => return Err(CustomCommandError::Storage),
                Err(error) => return Ok(error.to_string()),
            }
        };
        if resync && self.publish_locked().await.is_err() {
            // The write is committed. Report the partial outcome, never tell the
            // admin to retry a destructive mutation as if nothing happened.
            tracing::error!("custom-command definition committed but registry publication failed");
            return Ok(format!(
                "{reply} Registry publication failed; the command picker is not yet synchronized."
            ));
        }
        Ok(reply)
    }

    /// Pause or resume one saved definition without deleting it. Only the
    /// enabled flag and update metadata change, and only on a real flip;
    /// missing rows, current-state requests and validation refusals never
    /// publish. The flip commits before the one full-set publication, so an
    /// invocation racing a stale picker reads the saved state.
    async fn toggle(
        &self,
        interaction: &Interaction,
        data: &CommandData,
        guild: &str,
    ) -> Result<String, CustomCommandError> {
        let target = data.name == "command-enable";
        let raw = match single_string_option(data, "name") {
            SingleOption::Missing => {
                return Ok("A command name is required.".to_owned());
            }
            SingleOption::Malformed => return Ok(invalid_toggle_option(&data.name)),
            SingleOption::Value(name) => name,
        };
        if data.options.iter().any(|option| option.name != "name") {
            return Ok(invalid_toggle_option(&data.name));
        }
        let actor = interaction
            .author_id()
            .ok_or(CustomCommandError::Context)?
            .to_string();
        let name = raw.to_lowercase();
        let audit_id = format!("custom:manage:{}", interaction.id);
        let at = two_bot_core::now_iso();
        let _guard = self.registry.lock().await;
        let decision = service::set_enabled(
            &self.pool,
            self.router.gates().automations,
            &service::ToggleRequest {
                guild,
                actor: &actor,
                name: &name,
                enabled: target,
                audit_id: &audit_id,
                at: &at,
            },
        )
        .await;
        let (reply, resync) = match decision {
            Ok(decision) => {
                let reply = match decision.outcome {
                    ToggleOutcome::Missing => format!("No command /{name} found."),
                    ToggleOutcome::Unchanged if target => {
                        format!("/{name} is already enabled.")
                    }
                    ToggleOutcome::Unchanged => format!("/{name} is already disabled."),
                    ToggleOutcome::Changed if target => format!("Enabled /{name}."),
                    ToggleOutcome::Changed => format!("Disabled /{name}."),
                };
                (reply, decision.resync_registry)
            }
            Err(service::ServiceError::Storage(_)) => return Err(CustomCommandError::Storage),
            Err(error) => return Ok(error.to_string()),
        };
        if resync && self.publish_locked().await.is_err() {
            // The flip is committed. Report the saved-but-not-synchronized
            // state with a safe next action: never a delete recommendation,
            // and never a claim that a later no-op repaired the picker.
            tracing::error!("custom-command toggle committed but registry publication failed");
            return Ok(format!(
                "{reply} The definition is saved but the command picker is not yet synchronized because registry publication failed. Repeating this command will not resync it; restart the bot after Discord recovers to resync."
            ));
        }
        Ok(reply)
    }
}

/// Proven pre-wire refusals and guard failures map to `Delivery`; anything
/// else (timeout, transport failure, 5xx, 429) is `DeliveryUnknown`: the send
/// may have landed, so it must never be resent or audited as definitively
/// failed.
fn delivery_error(error: crate::DiscordError) -> CustomCommandError {
    if error.is_safe_pre_mutation() {
        CustomCommandError::Delivery
    } else {
        CustomCommandError::DeliveryUnknown
    }
}

/// Toggle-option refusal: the lifecycle commands take exactly one text
/// `name`, never a replacement/editor option set.
fn invalid_toggle_option(command: &str) -> String {
    format!("Invalid name option: pass exactly one text name, e.g. /{command} name:faq.")
}

/// Writer-shape refusal: names only the trusted option at fault (or a
/// generic `option` for an unknown key), never any submitted value.
fn invalid_writer_option(field: &str) -> String {
    if field == "option" {
        "Invalid option: use only the options listed for this command.".to_owned()
    } else if field == "name" || field == "template" {
        format!("Invalid {field} option: pass exactly one text {field}.")
    } else {
        format!("Invalid {field} option: pass at most one text {field}.")
    }
}

/// One named option read strictly: a forged type or a duplicate refuses
/// instead of silently taking the first value.
enum SingleOption<'a> {
    Missing,
    Malformed,
    Value(&'a str),
}

fn single_string_option<'a>(data: &'a CommandData, name: &str) -> SingleOption<'a> {
    let mut matches = data.options.iter().filter(|option| option.name == name);
    let Some(first) = matches.next() else {
        return SingleOption::Missing;
    };
    if matches.next().is_some() {
        return SingleOption::Malformed;
    }
    match &first.value {
        CommandOptionValue::String(value) => SingleOption::Value(value.as_str()),
        _ => SingleOption::Malformed,
    }
}

#[cfg(test)]
mod inspect_regressions {
    use super::*;
    use crate::test_wire::{MockRest, ScriptedResponse};
    use serde_json::{json, Value};
    use two_bot_core::RouterGates;
    use two_bot_testsupport::TestDatabase;

    const TOKEN: &str = "inspect-regression-fixture";

    struct Fixture {
        db: TestDatabase,
        http: MockRest,
        runtime: CustomCommandRuntime,
    }

    impl Fixture {
        async fn new(name: &str, template: &str) -> Self {
            let url =
                std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required");
            let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
                .await
                .expect("isolated migrated fixture");
            store::put_command(
                db.pool(),
                &StoredCommand {
                    guild_id: "2222".to_owned(),
                    name: name.to_owned(),
                    description: "Saved response".to_owned(),
                    template: template.to_owned(),
                    text_trigger: Some(format!("!{name}")),
                    enabled: true,
                },
                "3333",
                "3333",
                "2026-10-01T00:00:00Z",
                "2026-10-02T00:00:00Z",
            )
            .await
            .expect("seed existing row without write-time validation");
            let http = MockRest::with_responder(|request| match request.method.as_str() {
                "POST" if request.path.ends_with("/callback") => ScriptedResponse::status(204),
                "PATCH" if request.path.ends_with("/messages/@original") => {
                    ScriptedResponse::json(200, json!({"id": "9000"}))
                }
                _ => ScriptedResponse::status(403),
            })
            .await;
            let mut router = InteractionRouter::new(RouterGates {
                configured_guild: Some(2222),
                automations: true,
                moderation: false,
                voice: false,
                voice_assistant: false,
                scorecard: false,
                announcements: false,
                tickets: false,
                self_roles: false,
                onboarding_picker: false,
                session_picker: false,
            });
            CustomCommandRuntime::register(&mut router);
            let runtime = CustomCommandRuntime::new(
                db.pool().clone(),
                Arc::new(router),
                ActionExecutor::with_proxy(TOKEN.to_owned(), Some(http.origin()))
                    .expect("loopback executor"),
                1111,
            );
            Self { db, http, runtime }
        }

        async fn definitions(&self) -> Vec<Value> {
            sqlx::query_scalar(
                "SELECT to_jsonb(c) FROM automation_commands c ORDER BY guild_id, name",
            )
            .fetch_all(self.db.pool())
            .await
            .expect("full definition snapshot")
        }

        async fn assert_read_only(&self, before: Vec<Value>) {
            assert_eq!(self.definitions().await, before);
            let facts: i64 = sqlx::query_scalar("SELECT count(*) FROM automation_audit_log")
                .fetch_one(self.db.pool())
                .await
                .expect("execution and management facts");
            assert_eq!(facts, 0);
            assert!(self.runtime.published_commands().is_none());
        }

        async fn reply(&self, input: &Interaction, guild_name: Option<&str>) -> String {
            let start = self.http.requests().len();
            assert!(matches!(
                self.runtime.handle_interaction(input, guild_name).await,
                Ok(true)
            ));
            let requests = self.http.requests();
            let requests = &requests[start..];
            assert_eq!(
                requests.len(),
                2,
                "one private defer and completion, no execution/registration"
            );
            assert_eq!(requests[0].method, "POST");
            assert!(requests[0]
                .path
                .ends_with(&format!("/interactions/{}/{TOKEN}/callback", input.id)));
            let defer: Value = serde_json::from_slice(&requests[0].body).expect("defer JSON");
            assert_eq!(defer["type"], 5);
            assert_eq!(defer["data"]["flags"].as_u64().expect("flags") & 64, 64);
            assert_eq!(requests[1].method, "PATCH");
            assert!(requests[1]
                .path
                .ends_with(&format!("/webhooks/1111/{TOKEN}/messages/@original")));
            let edit: Value = serde_json::from_slice(&requests[1].body).expect("edit JSON");
            for body in [&defer["data"], &edit] {
                assert_eq!(body["allowed_mentions"]["parse"], json!([]));
                for key in ["users", "roles"] {
                    assert!(
                        body["allowed_mentions"][key].is_null()
                            || body["allowed_mentions"][key] == json!([])
                    );
                }
                assert_ne!(body["allowed_mentions"]["replied_user"], true);
                assert!(body["embeds"].is_null() || body["embeds"] == json!([]));
            }
            let content = edit["content"].as_str().expect("private content");
            assert!(!content.is_empty());
            assert!(content.encode_utf16().count() <= 2000);
            content.to_owned()
        }

        async fn close(self) {
            self.http.shutdown().await;
            self.db.close().await.expect("drop isolated fixture");
        }
    }

    fn inspect(id: u64, name: &str, view: &str) -> Interaction {
        serde_json::from_value(json!({
            "id": id.to_string(), "application_id": "1111", "type": 2,
            "token": TOKEN, "version": 1,
            "guild_id": "2222", "channel": {"id": "4444", "type": 0},
            "authorizing_integration_owners": {"0": "2222"}, "entitlements": [],
            "member": {
                "permissions": "32", "roles": [], "joined_at": null,
                "deaf": false, "mute": false, "flags": 0,
                "user": {"id": "3333", "username": "tester", "discriminator": "0000", "avatar": null}
            },
            "data": {"id": "5555", "name": "command-inspect", "type": 1, "options": [
                {"name": "name", "type": 3, "value": name},
                {"name": "view", "type": 3, "value": view}
            ]}
        })).expect("Twilight interaction fixture")
    }

    #[tokio::test]
    async fn grandfathered_voice_name_remains_privately_inspectable_with_voice_off() {
        let fixture = Fixture::new("setup", "Legacy {username}").await;
        let before = fixture.definitions().await;
        let summary = fixture
            .reply(&inspect(6001, "SETUP", "summary"), Some("TWO"))
            .await;
        assert_eq!(summary, "Summary /setup\nDescription: Saved response\nStatus: Enabled\nText trigger: !setup\nSaved template: /command-inspect name:setup view:template\nPrivate preview: /command-inspect name:setup view:preview\nReplace the definition: /command name:setup template:...");
        assert_eq!(
            fixture
                .reply(&inspect(6002, "setup", "template"), Some("TWO"))
                .await,
            "Saved template /setup\nLegacy {username}"
        );
        assert_eq!(
            fixture
                .reply(&inspect(6003, "setup", "preview"), Some("TWO"))
                .await,
            "Preview /setup\nLegacy tester"
        );
        fixture.assert_read_only(before).await;
        fixture.close().await;
    }

    #[tokio::test]
    async fn missing_preview_context_refuses_privately_without_execution_effects() {
        let fixture = Fixture::new("faq", "Hi {username}").await;
        let before = fixture.definitions().await;
        for missing in 0..3 {
            let mut input = inspect(6100 + missing, "faq", "preview");
            let mut guild_name = Some("TWO");
            match missing {
                0 => guild_name = None,
                1 => input.channel = None,
                _ => {
                    input
                        .member
                        .as_mut()
                        .expect("member permissions retained")
                        .user = None;
                    input.user = None;
                }
            }
            assert_eq!(
                fixture.reply(&input, guild_name).await,
                "Preview for /faq is unavailable: command context is missing. Please try again."
            );
        }
        fixture.assert_read_only(before).await;
        fixture.close().await;
    }

    #[tokio::test]
    async fn long_invalid_template_keeps_correction_on_the_private_wire() {
        let fixture = Fixture::new("faq", &format!("{{{}}}", "a".repeat(1998))).await;
        let before = fixture.definitions().await;
        let reply = fixture
            .reply(&inspect(6200, "faq", "preview"), Some("TWO"))
            .await;
        assert_eq!(reply.encode_utf16().count(), 2000);
        assert!(reply.starts_with("Template for /faq is invalid: Unknown placeholder"));
        assert!(reply.contains("…[truncated]"));
        assert!(reply.ends_with(" Replace it with /command name:faq template:..."));
        fixture.assert_read_only(before).await;
        fixture.close().await;
    }
}
