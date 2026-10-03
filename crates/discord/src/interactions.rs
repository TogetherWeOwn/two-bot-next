//! Twilight translation for the interaction router (TOG-10075).
//!
//! Thin adapter over [`two_bot_core::InteractionRouter`]: twilight
//! `Interaction`s in, framework-free outcomes out. Routing decisions live in
//! the core router (fence → gate → permission); this module only extracts the
//! wire fields, builds refusal replies, and converts the publish set to
//! twilight `Command`s for the S4 REST executor ([TOG-10076]) to send.
//!
//! No dispatch loop and no HTTP client here: the feature slice answers a
//! routed interaction through the executor, and the publish-on-ready
//! `set_guild_commands` call belongs to the executor too. What this module
//! produces is data (outcomes, `InteractionResponse`s, `Vec<Command>`) so the
//! whole surface stays testable against in-memory twilight values.

use twilight_model::{
    application::{
        command::{
            Command, CommandOption, CommandOptionChoice, CommandOptionChoiceValue,
            CommandOptionType, CommandOptionValue, CommandType,
        },
        interaction::{Interaction, InteractionData, InteractionType},
    },
    channel::message::MessageFlags,
    guild::Permissions,
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
};
use two_bot_core::{
    CommandDefinition, ComponentOutcome, InteractionRouter, RouterRefusal, SlashContext,
    SlashOutcome,
};

/// A routed interaction: what the core router decided, with the wire
/// identity the executor needs to answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutedInteraction {
    Slash {
        name: String,
        outcome: SlashOutcome,
    },
    Component {
        custom_id: String,
        values: Vec<String>,
        outcome: ComponentOutcome,
    },
    Modal {
        custom_id: String,
        outcome: ComponentOutcome,
    },
    /// Ping, autocomplete, or a payload with no routable data — not ours.
    Ignore,
}

/// Route one twilight interaction through the core router.
///
/// `custom_row` is the DB lookup for dynamic custom commands (`Some(true)`
/// enabled row, `Some(false)` disabled row, `None` no row); the caller reads
/// it from the store, the router only adjudicates.
#[must_use]
pub fn route_interaction(
    router: &InteractionRouter,
    interaction: &Interaction,
    custom_row: Option<bool>,
) -> RoutedInteraction {
    let guild_id = interaction.guild_id.map(|id| id.get());
    let actor_permissions = interaction
        .member
        .as_ref()
        .and_then(|m| m.permissions)
        .map(|p| p.bits());
    match (&interaction.kind, interaction.data.as_ref()) {
        (InteractionType::ApplicationCommand, Some(InteractionData::ApplicationCommand(data))) => {
            let ctx = SlashContext {
                name: &data.name,
                guild_id,
                actor_permissions,
                custom_row,
            };
            RoutedInteraction::Slash {
                name: data.name.clone(),
                outcome: router.route_slash(&ctx),
            }
        }
        (InteractionType::MessageComponent, Some(InteractionData::MessageComponent(data))) => {
            RoutedInteraction::Component {
                custom_id: data.custom_id.clone(),
                values: data.values.clone(),
                outcome: router.route_component(&data.custom_id, guild_id),
            }
        }
        (InteractionType::ModalSubmit, Some(InteractionData::ModalSubmit(data))) => {
            RoutedInteraction::Modal {
                custom_id: data.custom_id.clone(),
                outcome: router.route_modal(&data.custom_id, guild_id),
            }
        }
        _ => RoutedInteraction::Ignore,
    }
}

use crate::{ActionExecutor, DiscordError};
use std::{fmt::Debug, future::Future};
use two_bot_core::router::replies::{
    run_handler, InteractionReply, ReplyError, ReplyOperation, ReplyPolicy, ReplySession,
    ReplyTransport, UNKNOWN_INTERACTION_REPLY,
};

/// Text replies suppress all mentions and respect Discord's content ceiling.
#[must_use]
pub fn text_response(reply: InteractionReply) -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(InteractionResponseData {
            allowed_mentions: Some(Default::default()),
            content: Some(reply.content.chars().take(2000).collect()),
            flags: Some(if reply.ephemeral {
                MessageFlags::EPHEMERAL
            } else {
                MessageFlags::empty()
            }),
            ..Default::default()
        }),
    }
}

#[must_use]
pub fn deferred_response(ephemeral: bool) -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::DeferredChannelMessageWithSource,
        data: Some(InteractionResponseData {
            flags: Some(if ephemeral {
                MessageFlags::EPHEMERAL
            } else {
                MessageFlags::empty()
            }),
            ..Default::default()
        }),
    }
}

/// Refusal reply: ephemeral, legacy text, capped at 2000 chars.
#[must_use]
pub fn refusal_response(refusal: RouterRefusal) -> InteractionResponse {
    text_response(InteractionReply::new(refusal.message(), true))
}

#[must_use]
pub fn response_for_slash(outcome: &SlashOutcome) -> Option<InteractionResponse> {
    match outcome {
        SlashOutcome::Refuse { refusal } => Some(refusal_response(*refusal)),
        SlashOutcome::Unknown => Some(text_response(InteractionReply::new(
            UNKNOWN_INTERACTION_REPLY,
            true,
        ))),
        SlashOutcome::Handled { .. } | SlashOutcome::Ignore => None,
    }
}

/// Shared execution runtime: route once, acknowledge promptly, then run the
/// registered feature through the shared REST executor. Unsupported features
/// remain owned by their integration slices, not by a second dispatcher.
#[cfg(feature = "db")]
#[derive(Debug)]
pub struct InteractionRuntime {
    /// Shared with integration seams that read the same registrations (for
    /// example the custom-command execution seam): registrations are complete
    /// once `with_router` returns, so every `Arc` clone reads the same set.
    pub router: std::sync::Arc<InteractionRouter>,
    executor: crate::ActionExecutor,
    lfg: crate::lfg_interactions::LfgInteractions,
    bot_user_id: std::sync::atomic::AtomicU64,
    application_id: std::sync::atomic::AtomicU64,
}

#[cfg(feature = "db")]
impl InteractionRuntime {
    pub fn new(
        gates: two_bot_core::RouterGates,
        pool: sqlx::PgPool,
        executor: crate::ActionExecutor,
        bot_user_id: u64,
    ) -> Self {
        Self::with_router(InteractionRouter::new(gates), pool, executor, bot_user_id)
    }

    /// Compose LFG with the bot's existing feature registrations in ONE router.
    pub fn with_router(
        mut router: InteractionRouter,
        pool: sqlx::PgPool,
        executor: crate::ActionExecutor,
        bot_user_id: u64,
    ) -> Self {
        #[derive(Debug)]
        struct LfgRegistration(two_bot_core::HandlerId);
        impl two_bot_core::InteractionHandler for LfgRegistration {
            fn id(&self) -> two_bot_core::HandlerId {
                self.0
            }
        }
        router.register(Box::new(LfgRegistration(two_bot_core::HandlerId::Lfg)));
        router.register(Box::new(LfgRegistration(two_bot_core::HandlerId::LfgClose)));
        Self {
            router: std::sync::Arc::new(router),
            executor,
            lfg: crate::lfg_interactions::LfgInteractions::new(pool),
            bot_user_id: std::sync::atomic::AtomicU64::new(bot_user_id),
            application_id: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn set_bot_user_id(&self, id: u64) {
        self.bot_user_id
            .store(id, std::sync::atomic::Ordering::Relaxed);
    }

    /// RESUMED boots without tickets never learn the bot user: resolve it once
    /// through the shared executor. Unknown stays 0, which nonce recovery refuses.
    async fn bot_user_id(&self) -> u64 {
        let id = self.bot_user_id.load(std::sync::atomic::Ordering::Relaxed);
        if id != 0 {
            return id;
        }
        match self.executor.current_bot_user_id().await {
            Ok(id) => {
                self.set_bot_user_id(id);
                id
            }
            Err(_) => 0,
        }
    }

    pub fn set_application_id(&self, id: u64) {
        self.application_id
            .store(id, std::sync::atomic::Ordering::Relaxed);
    }

    /// Returns false for interactions owned by another feature or guild.
    pub async fn handle(&self, interaction: &Interaction) -> Result<bool, crate::DiscordError> {
        let application_id = self
            .application_id
            .load(std::sync::atomic::Ordering::Relaxed);
        if application_id != 0 && interaction.application_id.get() != application_id {
            return Ok(false);
        }
        let routed = route_interaction(&self.router, interaction, None);
        self.handle_routed(interaction, routed).await
    }

    /// Execute an outcome from this runtime's shared router without routing twice.
    pub async fn handle_routed(
        &self,
        interaction: &Interaction,
        routed: RoutedInteraction,
    ) -> Result<bool, crate::DiscordError> {
        let application_id = self
            .application_id
            .load(std::sync::atomic::Ordering::Relaxed);
        if application_id != 0 && interaction.application_id.get() != application_id {
            return Ok(false);
        }
        use crate::lfg_interactions::{LfgError, LfgRequest};
        use twilight_model::application::interaction::application_command::CommandOptionValue;
        use twilight_model::channel::message::{component::ComponentType, AllowedMentions};
        use two_bot_core::{ComponentHandler, HandlerId};
        let request = match routed {
            RoutedInteraction::Slash {
                outcome: SlashOutcome::Refuse { refusal },
                ..
            } => {
                self.executor
                    .answer_interaction(
                        interaction.id.get(),
                        &interaction.token,
                        &refusal_response(refusal),
                    )
                    .await?;
                return Ok(true);
            }
            RoutedInteraction::Slash {
                outcome:
                    SlashOutcome::Handled {
                        handler: HandlerId::Lfg | HandlerId::LfgClose,
                    },
                ..
            } => {
                let Some(InteractionData::ApplicationCommand(data)) = interaction.data.as_ref()
                else {
                    return Ok(false);
                };
                let option = |name: &str| -> Result<String, LfgError> {
                    data.options
                        .iter()
                        .find(|o| o.name == name)
                        .and_then(|o| match &o.value {
                            CommandOptionValue::String(value) => Some(value.clone()),
                            _ => None,
                        })
                        .ok_or_else(|| LfgError::Invalid(format!("Missing {name} option.")))
                };
                if data.name == "lfg" {
                    option("title").and_then(|title| {
                        Ok(LfgRequest::Create {
                            title,
                            starts_at: option("starts-at")?,
                            roles: option("roles")?,
                            channel_id: interaction
                                .channel
                                .as_ref()
                                .map(|c| c.id.to_string())
                                .ok_or_else(|| LfgError::Invalid("Missing channel.".into()))?,
                        })
                    })
                } else {
                    option("id").map(|post_id| LfgRequest::Close { post_id })
                }
            }
            RoutedInteraction::Component {
                custom_id,
                values,
                outcome:
                    ComponentOutcome::Handled {
                        handler: ComponentHandler::LfgSignup,
                    },
            } => {
                let Some(InteractionData::MessageComponent(data)) = interaction.data.as_ref()
                else {
                    return Ok(false);
                };
                if data.component_type != ComponentType::TextSelectMenu || values.len() != 1 {
                    Err(LfgError::Invalid("Choose one LFG role.".into()))
                } else {
                    two_bot_core::lfg::parse_lfg_select(&custom_id, &values[0])
                        .map(LfgRequest::Select)
                        .ok_or_else(|| LfgError::Invalid("Invalid LFG select.".into()))
                }
            }
            _ => return Ok(false),
        };
        let Some(guild) = interaction.guild_id else {
            return Ok(false);
        };
        let Some(actor) = interaction.author_id() else {
            return Ok(false);
        };
        let deferred = InteractionResponse {
            kind: InteractionResponseType::DeferredChannelMessageWithSource,
            data: Some(InteractionResponseData {
                flags: Some(MessageFlags::EPHEMERAL),
                allowed_mentions: Some(AllowedMentions {
                    parse: vec![],
                    replied_user: false,
                    roles: vec![],
                    users: vec![],
                }),
                ..Default::default()
            }),
        };
        // Acknowledge before locks, SQL or paced REST can exceed Discord's 3 s window.
        // https://docs.discord.com/developers/interactions/receiving-and-responding#interaction-response
        self.executor
            .answer_interaction(interaction.id.get(), &interaction.token, &deferred)
            .await?;
        let result = match request {
            Ok(request) => {
                self.lfg
                    .execute(
                        &self.executor,
                        request,
                        &guild.to_string(),
                        &actor.to_string(),
                        interaction.id.get(),
                        self.bot_user_id().await,
                    )
                    .await
            }
            Err(error) => Err(error),
        };
        let reply = match result {
            Ok(reply) => reply,
            Err(LfgError::Invalid(reply)) => reply,
            Err(LfgError::Uncertain) => {
                "LFG post acceptance is uncertain; saved state retained for nonce recovery.".into()
            }
            Err(_) => {
                tracing::warn!(
                    interaction_id = interaction.id.get(),
                    "LFG operation failed; details withheld"
                );
                "LFG operation failed; check the saved state before retrying.".into()
            }
        };
        self.executor
            .finish_interaction(interaction.application_id.get(), &interaction.token, &reply)
            .await?;
        Ok(true)
    }
}

/// Token-bearing transport, deliberately not Debug. Uses the existing executor
/// and its injectable REST transport rather than a second HTTP client.
pub struct InteractionReplyTransport<'a> {
    executor: &'a ActionExecutor,
    interaction: &'a Interaction,
}

impl<'a> InteractionReplyTransport<'a> {
    #[must_use]
    pub fn new(executor: &'a ActionExecutor, interaction: &'a Interaction) -> Self {
        Self {
            executor,
            interaction,
        }
    }
}

impl ReplyTransport for InteractionReplyTransport<'_> {
    type Error = DiscordError;

    async fn execute(&self, operation: ReplyOperation) -> Result<Option<u64>, Self::Error> {
        self.executor
            .execute_reply_operation(
                self.interaction.application_id.get(),
                self.interaction.id.get(),
                &self.interaction.token,
                operation,
            )
            .await
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DispatchOptions {
    pub custom_row: Option<bool>,
    pub reply_policy: ReplyPolicy,
    /// Set from the feature's visibility policy, not from user-supplied options.
    pub ephemeral: bool,
}

/// Route and execute through one shared reply lifecycle. Existing feature
/// functions are adapted by the closure; registration traits are unchanged.
/// Returns false only for fenced/disabled/unmodelled events (no handler/I/O).
pub async fn dispatch_interaction<'a, T, H, F, E>(
    router: &InteractionRouter,
    interaction: &Interaction,
    transport: &'a T,
    options: DispatchOptions,
    handler: H,
) -> Result<bool, ReplyError<T::Error>>
where
    T: ReplyTransport,
    H: FnOnce(RoutedInteraction, ReplySession<'a, T>) -> F,
    F: Future<Output = Result<InteractionReply, E>>,
    E: Debug,
{
    let routed = route_interaction(router, interaction, options.custom_row);
    let immediate = match &routed {
        RoutedInteraction::Slash {
            outcome: SlashOutcome::Refuse { refusal },
            ..
        } => Some(InteractionReply::new(refusal.message(), true)),
        RoutedInteraction::Slash {
            outcome: SlashOutcome::Unknown,
            ..
        }
        | RoutedInteraction::Component {
            outcome: ComponentOutcome::Unknown,
            ..
        }
        | RoutedInteraction::Modal {
            outcome: ComponentOutcome::Unknown,
            ..
        } => Some(InteractionReply::new(UNKNOWN_INTERACTION_REPLY, true)),
        RoutedInteraction::Slash {
            outcome: SlashOutcome::Ignore,
            ..
        }
        | RoutedInteraction::Component {
            outcome: ComponentOutcome::Ignore,
            ..
        }
        | RoutedInteraction::Modal {
            outcome: ComponentOutcome::Ignore,
            ..
        }
        | RoutedInteraction::Ignore => return Ok(false),
        _ => None,
    };
    if let Some(reply) = immediate {
        transport
            .execute(ReplyOperation::Respond(reply))
            .await
            .map_err(ReplyError::Transport)?;
    } else {
        run_handler(
            transport,
            options.reply_policy,
            options.ephemeral,
            |session| handler(routed, session),
        )
        .await?;
    }
    Ok(true)
}

/// Convert one registry definition to the twilight publish shape.
///
/// `version`/`id` are server-assigned on bulk set — `Id::new(1)` is a
/// placeholder the endpoint ignores. `dm_permission` stays unset: guild
/// commands are guild-scoped by the endpoint, and the field is deprecated in
/// favour of contexts.
#[allow(deprecated)]
#[must_use]
pub fn command_to_twilight(def: &CommandDefinition) -> Command {
    Command {
        application_id: None,
        contexts: None,
        default_member_permissions: def.default_member_permissions.as_deref().map(|bits| {
            bits.parse::<u64>()
                .map(Permissions::from_bits_truncate)
                .unwrap_or(Permissions::empty())
        }),
        dm_permission: None,
        description: def.description.clone(),
        description_localizations: None,
        guild_id: None,
        id: None,
        integration_types: None,
        kind: CommandType::ChatInput,
        name: def.name.clone(),
        name_localizations: None,
        nsfw: None,
        options: def.options.iter().map(option_to_twilight).collect(),
        version: twilight_model::id::Id::new(1),
    }
}

fn option_to_twilight(opt: &two_bot_core::commands::CommandOption) -> CommandOption {
    let kind = match opt.kind {
        1 => CommandOptionType::SubCommand,
        4 => CommandOptionType::Integer,
        5 => CommandOptionType::Boolean,
        6 => CommandOptionType::User,
        7 => CommandOptionType::Channel,
        8 => CommandOptionType::Role,
        _ => CommandOptionType::String,
    };
    CommandOption {
        autocomplete: None,
        channel_types: None,
        choices: if opt.choices.is_empty() {
            None
        } else {
            Some(
                opt.choices
                    .iter()
                    .map(|c| CommandOptionChoice {
                        name: c.name.clone(),
                        name_localizations: None,
                        value: CommandOptionChoiceValue::String(c.value.clone()),
                    })
                    .collect(),
            )
        },
        description: opt.description.clone(),
        description_localizations: None,
        kind,
        max_length: opt.max_length.and_then(|m| u16::try_from(m).ok()),
        max_value: opt.max_value.map(CommandOptionValue::Integer),
        min_length: None,
        min_value: opt.min_value.map(CommandOptionValue::Integer),
        name: opt.name.clone(),
        name_localizations: None,
        options: if opt.options.is_empty() {
            None
        } else {
            Some(opt.options.iter().map(option_to_twilight).collect())
        },
        required: opt.required,
    }
}

/// Convert the router's complete publish set to twilight commands in one
/// call — one set, one send, no partial view (the executor sends the whole
/// `Vec` to `set_guild_commands`).
#[must_use]
pub fn publish_commands(defs: &[CommandDefinition]) -> Vec<Command> {
    defs.iter().map(command_to_twilight).collect()
}
