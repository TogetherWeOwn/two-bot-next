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
        accepted_text_trigger, builtin_command_names, format_command_list, render_template,
        AuditRecord, AutomationMessageAcceptance, PutCommandInput, StoredCommand, TemplateContext,
    },
    router::InteractionHandler,
    HandlerId, InteractionRouter, SlashOutcome,
};

use crate::{
    automation_admission::ActorCooldowns,
    interactions::{publish_commands, refusal_response, route_interaction, RoutedInteraction},
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
            "command" | "command-remove" | "command-list"
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
            tokio::time::timeout(
                Duration::from_millis(750),
                store::get_command(&self.pool, &guild, &data.name),
            )
            .await
            .map_err(|_| CustomCommandError::Storage)?
            .map_err(|_| CustomCommandError::Storage)?
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
                let reply = self.manage(interaction, data, &guild).await;
                self.complete(
                    interaction,
                    reply
                        .as_deref()
                        .unwrap_or("Custom-command operation failed. Please try again."),
                )
                .await?;
                reply?;
                Ok(true)
            }
            SlashOutcome::Handled {
                handler: HandlerId::AutomationCustom,
            } => {
                self.defer(interaction, false, received).await?;
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
                self.audit(&record, &format!("custom:run:{}", interaction.id))
                    .await?;
                result?;
                Ok(true)
            }
            _ => Ok(false),
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
        // Verdict refusal is distinct telemetry from unmatched content: the
        // automod verdict contained the create before any trigger lookup.
        // Guild/webhook scope mismatches stay `Ignored` and never count here.
        if !acceptance.permits_automations() {
            two_bot_core::metrics::global().prefix_trigger_refused("verdict");
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
        tokio::time::timeout_at(
            received + Duration::from_millis(crate::executor::RECEIPT_ADMISSION_BUDGET_MS),
            self.executor.answer_interaction_with_blocked_retry(
                interaction.id.get(),
                &interaction.token,
                &response,
            ),
        )
        .await
        .map_err(|_| CustomCommandError::Delivery)?
        .map_err(|_| CustomCommandError::Delivery)
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
        self.executor
            .edit_interaction_response_with_blocked_retry(
                self.application_id,
                &interaction.token,
                content,
            )
            .await
            .map_err(|_| CustomCommandError::Delivery)
    }

    async fn audit(&self, record: &AuditRecord, id: &str) -> Result<(), CustomCommandError> {
        store::audit(&self.pool, record, id, &two_bot_core::now_iso())
            .await
            .map_err(|_| CustomCommandError::Storage)
    }

    async fn manage(
        &self,
        interaction: &Interaction,
        data: &CommandData,
        guild: &str,
    ) -> Result<String, CustomCommandError> {
        let actor = interaction
            .author_id()
            .ok_or(CustomCommandError::Context)?
            .to_string();
        if data.name == "command-list" {
            let rows = store::list_commands(&self.pool, guild)
                .await
                .map_err(|_| CustomCommandError::Storage)?;
            return Ok(format_command_list(&rows));
        }
        let Some(name) = string_option(data, "name") else {
            return Ok("A command name is required.".to_owned());
        };
        let name = name.to_lowercase();
        let audit_id = format!("custom:manage:{}", interaction.id);
        let at = two_bot_core::now_iso();
        let _guard = self.registry.lock().await;
        let (reply, resync) = if data.name == "command" {
            let Some(template) = string_option(data, "template") else {
                return Ok("A template is required.".to_owned());
            };
            let input = PutCommandInput {
                name: name.clone(),
                description: string_option(data, "description")
                    .unwrap_or("Custom command")
                    .to_owned(),
                template: template.to_owned(),
                text_trigger: string_option(data, "text-trigger").map(str::to_lowercase),
            };
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
}

fn string_option<'a>(data: &'a CommandData, name: &str) -> Option<&'a str> {
    data.options.iter().find_map(|option| match &option.value {
        CommandOptionValue::String(value) if option.name == name => Some(value.as_str()),
        _ => None,
    })
}
