//! Custom-command execution through the shared router and REST executor.
//! Database mutations commit before the ONE full registry is republished.
//! No gateway listener here: the runtime injects this service into its dispatch.

use std::{sync::Arc, time::Duration};

use sqlx::PgPool;
use tokio::sync::Mutex;
use twilight_model::{
    application::interaction::{
        application_command::{CommandData, CommandOptionValue},
        Interaction, InteractionData,
    },
    channel::message::{AllowedMentions, MessageFlags},
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
};
use two_bot_core::{
    custom_command_service as service, custom_command_store as store,
    custom_commands::{
        builtin_command_names, format_command_list, render_template, AuditRecord, PutCommandInput,
        StoredCommand, TemplateContext,
    },
    router::InteractionHandler,
    HandlerId, InteractionRouter, SlashOutcome,
};

use crate::{
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
    #[error("Custom-command registry publication failed; the saved definition is unchanged.")]
    Publication,
    #[error("Custom-command context is unavailable.")]
    Context,
}

#[derive(Debug)]
struct Registration(HandlerId);

impl InteractionHandler for Registration {
    fn id(&self) -> HandlerId {
        self.0
    }
}

#[derive(Clone)]
pub struct CustomCommandRuntime {
    pool: PgPool,
    router: Arc<InteractionRouter>,
    executor: ActionExecutor,
    application_id: u64,
    // All clones serialize mutation + read + publish, not just the HTTP PUT.
    // Otherwise an older full-set snapshot can overwrite a newer addition.
    registry: Arc<Mutex<()>>,
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
        }
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
            .map_err(|_| CustomCommandError::Publication)
    }

    /// Returns false for another feature's interaction. Guild/application fences
    /// precede every lookup, and builtin names never fall through to DB rows.
    /// The guild name comes from the shared cache, never from a synthetic ID.
    pub async fn handle_interaction(
        &self,
        interaction: &Interaction,
        guild_name: Option<&str>,
    ) -> Result<bool, CustomCommandError> {
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
                self.executor
                    .answer_interaction(
                        interaction.id.get(),
                        &interaction.token,
                        &refusal_response(refusal),
                    )
                    .await
                    .map_err(|_| CustomCommandError::Delivery)?;
                Ok(true)
            }
            SlashOutcome::Handled {
                handler: HandlerId::AutomationAdmin,
            } if management => {
                self.defer(interaction, true).await?;
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
                self.defer(interaction, false).await?;
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

    async fn defer(
        &self,
        interaction: &Interaction,
        ephemeral: bool,
    ) -> Result<(), CustomCommandError> {
        let response = InteractionResponse {
            kind: InteractionResponseType::DeferredChannelMessageWithSource,
            data: Some(InteractionResponseData {
                flags: ephemeral.then_some(MessageFlags::EPHEMERAL),
                allowed_mentions: Some(AllowedMentions::default()),
                ..Default::default()
            }),
        };
        self.executor
            .answer_interaction(interaction.id.get(), &interaction.token, &response)
            .await
            .map_err(|_| CustomCommandError::Delivery)
    }

    async fn complete(
        &self,
        interaction: &Interaction,
        content: &str,
    ) -> Result<(), CustomCommandError> {
        self.executor
            .edit_interaction_response(self.application_id, &interaction.token, content)
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
