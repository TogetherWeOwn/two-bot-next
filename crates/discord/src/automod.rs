//! Twilight message translation for the shared gateway/runtime seam.
//!
//! No shard runner, cache, REST client or effect executor is created here.
//! Twilight 0.17 models MessageUpdate as a Message; it is still a partial
//! gateway dispatch, so fetch/enrich it rather than interpreting absent facts
//! as empty roles, content, mentions or attachments.

use twilight_model::{channel::Message, gateway::event::Event, util::Timestamp};
use two_bot_core::automod_runtime::{MessageDelivery, MessageDeliveryKind};
use two_bot_core::AutomodMessage;

#[must_use]
pub fn event_to_automod(event: &Event, receipt_ms: u64) -> Option<MessageDelivery> {
    match event {
        Event::MessageCreate(message) => {
            let roles = message.member.as_ref().map(|member| {
                member
                    .roles
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            });
            let snapshot = if let Some(roles) = roles {
                Some(snapshot(
                    message,
                    message.guild_id?.to_string(),
                    &roles,
                    millis(message.timestamp)?,
                ))
            } else if message.author.bot {
                Some(snapshot(
                    message,
                    message.guild_id?.to_string(),
                    &[],
                    millis(message.timestamp)?,
                ))
            } else {
                None
            };
            Some(MessageDelivery {
                kind: MessageDeliveryKind::Create,
                guild_id: message.guild_id.map(|id| id.to_string()),
                channel_id: message.channel_id.to_string(),
                message_id: message.id.to_string(),
                snapshot,
                edited_timestamp_ms: message.edited_timestamp.and_then(millis),
                observed_timestamp_ms: receipt_ms,
            })
        }
        Event::MessageUpdate(message) => Some(MessageDelivery {
            kind: MessageDeliveryKind::Update,
            guild_id: message.guild_id.map(|id| id.to_string()),
            channel_id: message.channel_id.to_string(),
            message_id: message.id.to_string(),
            snapshot: None,
            edited_timestamp_ms: message.edited_timestamp.and_then(millis),
            observed_timestamp_ms: receipt_ms,
        }),
        _ => None,
    }
}

/// Complete a fetch request using a REST message and authoritative role IDs for
/// its author in the requested guild. REST messages need not include guild_id
/// or member. The shared adapter must resolve the member, not pass an empty
/// list after a lookup failure. An identity mismatch is never inspected.
#[must_use]
pub fn with_fetched_message(
    delivery: &MessageDelivery,
    message: &Message,
    author_role_ids: &[String],
) -> Option<MessageDelivery> {
    let guild_id = delivery.guild_id.as_ref()?;
    if message.id.to_string() != delivery.message_id
        || message.channel_id.to_string() != delivery.channel_id
        || message
            .guild_id
            .is_some_and(|id| id.to_string() != *guild_id)
    {
        return None;
    }
    let observed_ms = match delivery.kind {
        MessageDeliveryKind::Create => millis(message.timestamp)?,
        MessageDeliveryKind::Update => delivery.observed_timestamp_ms,
    };
    Some(MessageDelivery {
        snapshot: Some(snapshot(
            message,
            guild_id.clone(),
            author_role_ids,
            observed_ms,
        )),
        edited_timestamp_ms: message.edited_timestamp.and_then(millis),
        ..delivery.clone()
    })
}

fn snapshot(
    message: &Message,
    guild_id: String,
    roles: &[String],
    observed_ms: u64,
) -> AutomodMessage {
    AutomodMessage {
        guild_id,
        channel_id: message.channel_id.to_string(),
        message_id: message.id.to_string(),
        author_id: message.author.id.to_string(),
        author_is_bot: message.author.bot,
        role_ids: roles.to_vec(),
        content: message.content.clone(),
        // A reply reference alone is not an explicit ping.
        mentioned_user_ids: message
            .mentions
            .iter()
            .map(|mention| mention.id.to_string())
            .collect(),
        attachment_names: message
            .attachments
            .iter()
            .map(|attachment| attachment.filename.clone())
            .collect(),
        observed_timestamp_ms: observed_ms,
    }
}

fn millis(timestamp: Timestamp) -> Option<u64> {
    u64::try_from(timestamp.as_micros() / 1_000).ok()
}
