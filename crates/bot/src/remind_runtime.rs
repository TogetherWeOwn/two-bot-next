//! Personal reminders: strict slash decoding, owner-scoped storage and ephemeral replies.

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use twilight_model::application::interaction::{
    application_command::CommandOptionValue, Interaction, InteractionData,
};
use twilight_model::{channel::ChannelType, guild::Permissions};
use two_bot_core::{
    funnel::now_millis_for_test,
    remind::{self, delivery_body, TEXT_LIMIT},
    remind_store::{self, Admission},
    scheduled::{format_iso_ms, next_run_at_ms},
    scheduled_store::ScheduledWrite,
};
use two_bot_discord::ActionExecutor;

use crate::command_runtime::actor_id;

const INVALID: &str =
    "Use /remind me with in (minutes) and text, /remind list, or /remind cancel with a target.";
const STORE_FAILURE: &str = "Reminder command failed; try again.";

enum Request<'a> {
    Create { minutes: i64, text: &'a str },
    List,
    Cancel(&'a str),
}

fn decode(interaction: &Interaction) -> Option<Request<'_>> {
    let Some(InteractionData::ApplicationCommand(data)) = &interaction.data else {
        return None;
    };
    let [sub] = data.options.as_slice() else {
        return None;
    };
    let CommandOptionValue::SubCommand(options) = &sub.value else {
        return None;
    };
    match sub.name.as_str() {
        "me" if options.len() == 2 => {
            let mut minutes = None;
            let mut text = None;
            for option in options {
                match (option.name.as_str(), &option.value) {
                    ("in", CommandOptionValue::Integer(value)) if minutes.is_none() => {
                        minutes = Some(*value)
                    }
                    ("text", CommandOptionValue::String(value)) if text.is_none() => {
                        text = Some(value.as_str())
                    }
                    _ => return None,
                }
            }
            Some(Request::Create {
                minutes: minutes?,
                text: text?,
            })
        }
        "list" if options.is_empty() => Some(Request::List),
        "cancel" => match options.as_slice() {
            [option] if option.name == "target" => match &option.value {
                CommandOptionValue::String(target)
                    if !target.trim().is_empty()
                        && two_bot_core::message_safety::text_len(target) <= TEXT_LIMIT =>
                {
                    Some(Request::Cancel(target))
                }
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

/// Hash the canonical immutable invocation, not wall time: replay after a
/// restart cannot create or re-enable a second occurrence. Length prefixes
/// avoid ambiguous concatenation; content never appears in the storage id.
fn reminder_id(
    interaction: &Interaction,
    guild: &str,
    owner: &str,
    channel: &str,
    minutes: i64,
    text: &str,
) -> String {
    let mut hash = Sha256::new();
    for part in [
        interaction.id.to_string(),
        guild.to_owned(),
        owner.to_owned(),
        channel.to_owned(),
        minutes.to_string(),
        text.to_owned(),
    ] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    format!("{}{}", remind::ID_PREFIX, hex::encode(hash.finalize()))
}

/// Discord supplies the invoker's resolved channel permissions, including
/// overwrites, in `member.permissions`. Slash-command access alone is not
/// authority to have the bot publish member-authored text. Require channel
/// metadata so missing/unknown types cannot accidentally use regular send bits.
fn posting_channel(interaction: &Interaction) -> Option<String> {
    let channel = interaction.channel.as_ref()?;
    if channel
        .guild_id
        .is_some_and(|guild| Some(guild) != interaction.guild_id)
    {
        return None;
    }
    let send = match channel.kind {
        ChannelType::AnnouncementThread
        | ChannelType::PublicThread
        | ChannelType::PrivateThread => Permissions::SEND_MESSAGES_IN_THREADS,
        ChannelType::GuildText
        | ChannelType::GuildAnnouncement
        | ChannelType::GuildVoice
        | ChannelType::GuildStageVoice => Permissions::SEND_MESSAGES,
        _ => return None,
    };
    let permissions = interaction.member.as_ref()?.permissions?;
    permissions
        .contains(Permissions::VIEW_CHANNEL | send)
        .then(|| channel.id.to_string())
}

pub(crate) async fn handle(
    pool: &PgPool,
    executor: &ActionExecutor,
    guild: &str,
    interaction: &Interaction,
) {
    let owner = actor_id(interaction);
    // Guild and feature gates run in the shared router; this is an additional
    // subject/channel fence for malformed authenticated interactions.
    if owner.is_empty() || interaction.guild_id.map(|id| id.to_string()).as_deref() != Some(guild) {
        finish(executor, interaction, INVALID).await;
        return;
    }
    let Some(request) = decode(interaction) else {
        finish(executor, interaction, INVALID).await;
        return;
    };
    let reply = match request {
        Request::Create { minutes, text } => {
            let safe = match remind::validate(minutes, text) {
                Ok(safe) => safe,
                Err(reason) => {
                    finish(executor, interaction, reason).await;
                    return;
                }
            };
            let Some(channel) = posting_channel(interaction) else {
                finish(
                    executor,
                    interaction,
                    "You need permission to send messages in this channel to create a reminder.",
                )
                .await;
                return;
            };
            let now = now_millis_for_test().max(0) as u64;
            let id = reminder_id(interaction, guild, &owner, &channel, minutes, &safe);
            let row = ScheduledWrite {
                id: id.clone(),
                guild_id: guild.to_owned(),
                channel_id: channel,
                body: delivery_body(&owner, &safe),
                next_run_at: format_iso_ms(next_run_at_ms(now, minutes)),
                interval_seconds: None,
                enabled: true,
                created_by: owner.clone(),
                created_at: format_iso_ms(now),
                updated_by: owner.clone(),
                updated_at: format_iso_ms(now),
            };
            match remind_store::create(pool, &row).await {
                Ok(Admission::Created) => format!("Reminder `{id}` set for {} in this channel (no ping).", row.next_run_at),
                Ok(Admission::Existing) => "This reminder request has already been recorded. Use /remind list to see pending reminders.".to_owned(),
                Ok(Admission::Capacity) => format!("You have {} pending reminders in this server. Cancel one with /remind cancel before creating another.", remind::PENDING_LIMIT),
                Err(_) => STORE_FAILURE.to_owned(),
            }
        }
        Request::List => match remind_store::list(pool, guild, &owner).await {
            Ok(rows) if rows.is_empty() => {
                "You have no pending reminders in this server.".to_owned()
            }
            Ok(rows) => rows
                .into_iter()
                .map(|row| {
                    let state = if row.delivery_uncertain {
                        "delivery held; inspect channel before cancelling"
                    } else {
                        row.next_run_at.as_str()
                    };
                    let prefix = format!("Reminder for {owner}: ");
                    let text = row.body.strip_prefix(&prefix).unwrap_or(&row.body);
                    format!(
                        "`{}` in <#{}> — {} — {}",
                        row.id, row.channel_id, state, text
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Err(_) => STORE_FAILURE.to_owned(),
        },
        Request::Cancel(target) => match remind_store::cancel(
            pool,
            guild,
            &owner,
            target,
            &format_iso_ms(now_millis_for_test().max(0) as u64),
        )
        .await
        {
            Ok(true) => "Reminder cancelled.".to_owned(),
            Ok(false) => {
                "No unique pending reminder matches. Use a full id from /remind list.".to_owned()
            }
            Err(_) => STORE_FAILURE.to_owned(),
        },
    };
    finish(executor, interaction, reply).await;
}

async fn finish(executor: &ActionExecutor, interaction: &Interaction, text: impl Into<String>) {
    // The runtime deferred ephemerally before any store work. The executor
    // applies closed mentions and shared outbound safety to this one edit.
    if let Err(error) = executor
        .edit_interaction_response_with_blocked_retry(
            interaction.application_id.get(),
            &interaction.token,
            &text.into(),
        )
        .await
    {
        tracing::warn!(interaction_id = %interaction.id.get(), reason = crate::db_log::discord_class(&error), "schedule reply edit failed");
    }
}
