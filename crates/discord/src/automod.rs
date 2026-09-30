//! Twilight message translation for the shared gateway/runtime seam.
//!
//! No shard runner, cache, REST client or effect executor is created here.
//! Twilight 0.17 models MessageUpdate as a Message; it is still a partial
//! gateway dispatch, so fetch/enrich it rather than interpreting absent facts
//! as empty roles, content, mentions or attachments.

use twilight_model::{channel::Message, gateway::event::Event, util::Timestamp};
use two_bot_core::automod_runtime::{MessageDelivery, MessageDeliveryKind};
use two_bot_core::AutomodMessage;

/// A MESSAGE_UPDATE dispatch as Discord actually sends it: only IDs are
/// guaranteed. Author, attachments, timestamps and content may be absent, so
/// this decodes the raw dispatch object — never a full Twilight `Message`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartialEdit {
    pub guild_id: Option<String>,
    pub channel_id: String,
    pub message_id: String,
    pub edited_timestamp: Option<String>,
}

impl PartialEdit {
    /// Decode one raw `d` object. Missing IDs are not defaulted to empty
    /// strings; a missing `id` or `channel_id` rejects the dispatch.
    #[must_use]
    pub fn from_dispatch(value: &serde_json::Value) -> Option<Self> {
        let object = value.as_object()?;
        let id = |key: &str| object.get(key)?.as_str().map(str::to_owned);
        Some(Self {
            guild_id: id("guild_id"),
            channel_id: id("channel_id")?,
            message_id: id("id")?,
            edited_timestamp: id("edited_timestamp"),
        })
    }
}

/// Accept a partial MESSAGE_UPDATE dispatch before full-message decoding.
/// Twilight 0.17 models `MessageUpdate` as a complete `Message`, so a minimal
/// edit (IDs + changed content, no author/attachments/timestamps) fails
/// `twilight_gateway::parse` before `event_to_automod` can request a fetch.
/// Route the raw `d` object here first: the returned delivery carries no
/// snapshot, and `inspect` maps it to `FetchMessage` for authoritative
/// enrichment via [`with_fetched_message`]. Never fills absent content/roles.
/// An unparseable `edited_timestamp` keeps the delivery and drops the stamp.
#[must_use]
pub fn partial_edit_delivery(edit: &PartialEdit, receipt_ms: u64) -> MessageDelivery {
    MessageDelivery {
        kind: MessageDeliveryKind::Update,
        guild_id: edit.guild_id.clone(),
        channel_id: edit.channel_id.clone(),
        message_id: edit.message_id.clone(),
        snapshot: None,
        edited_timestamp_ms: edit
            .edited_timestamp
            .as_deref()
            .and_then(|stamp| Timestamp::parse(stamp).ok())
            .and_then(millis),
        observed_timestamp_ms: receipt_ms,
    }
}

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
