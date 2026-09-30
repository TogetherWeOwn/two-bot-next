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
    let _span = tracing::info_span!(
        "interaction",
        interaction_id = %interaction.id,
        guild_id = interaction.guild_id.map(|id| id.to_string()),
    )
    .entered();
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

/// Refusal reply: ephemeral, legacy text, content capped the way legacy
/// `ephemeralReply` caps (2000 chars; refusal texts are far shorter).
#[must_use]
pub fn refusal_response(refusal: RouterRefusal) -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(InteractionResponseData {
            content: Some(refusal.message()),
            flags: Some(MessageFlags::EPHEMERAL),
            ..Default::default()
        }),
    }
}

/// Reply the router itself owes for a slash outcome: refusals get the legacy
/// text, everything else is `None` — a handled command is answered by its
/// feature slice through the executor, and an ignored one is silence (some
/// other application's command, legacy fall-through).
#[must_use]
pub fn response_for_slash(outcome: &SlashOutcome) -> Option<InteractionResponse> {
    match outcome {
        SlashOutcome::Refuse { refusal } => Some(refusal_response(*refusal)),
        SlashOutcome::Handled { .. } | SlashOutcome::Ignore => None,
    }
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
        4 => CommandOptionType::Integer,
        6 => CommandOptionType::User,
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
        options: None,
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
