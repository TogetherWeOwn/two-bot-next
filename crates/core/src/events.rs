//! Framework-free domain events.
//!
//! These mirror the two-bot workload from ADR 0001 (member join/update/remove,
//! voice presence deltas, messages, reactions, invites) without importing any
//! Discord types, so S3+ handlers can be unit-tested against fixtures.

use serde::{Deserialize, Serialize};

/// Discord snowflake IDs, kept as raw u64 (no framework newtype).
pub type Snowflake = u64;

/// Voice session delta: join, leave, or move between voice channels.
/// Open sessions are held in memory and dropped on reconnect (ADR 0001 §State).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VoiceSessionDelta {
    /// Member joined `channel_id` (None = stage/unknown channel).
    Join { channel_id: Option<Snowflake> },
    /// Member left voice entirely.
    Leave,
    /// Member moved from one channel to another.
    Move {
        from_channel_id: Option<Snowflake>,
        to_channel_id: Option<Snowflake>,
    },
}

/// Identity minted once at ingress and retained when a delivery is retried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionDelivery {
    pub event_id: String,
    pub event_order: String,
}

/// Emoji identity, independent of a custom emoji's mutable display name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReactionEmoji {
    Unicode(String),
    Custom(Snowflake),
}

/// IDs and filter inputs only; no message content is retained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionEvent {
    pub guild_id: Option<Snowflake>,
    pub channel_id: Snowflake,
    pub message_id: Snowflake,
    pub user_id: Snowflake,
    pub message_author_id: Option<Snowflake>,
    pub member_is_bot: Option<bool>,
    pub emoji: ReactionEmoji,
    pub burst: bool,
    pub delivery: ReactionDelivery,
}

/// Domain event: the framework-free unit the bot reasons about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CoreEvent {
    MemberJoined {
        guild_id: Snowflake,
        user_id: Snowflake,
    },
    MemberLeft {
        guild_id: Snowflake,
        user_id: Snowflake,
    },
    VoiceStateChanged {
        guild_id: Snowflake,
        user_id: Snowflake,
        delta: VoiceSessionDelta,
    },
    MessageCreated {
        guild_id: Option<Snowflake>,
        channel_id: Snowflake,
        message_id: Snowflake,
        author_id: Snowflake,
        author_is_bot: bool,
    },
    ReactionAdded(ReactionEvent),
    ReactionRemoved(ReactionEvent),
    MessageDeleted {
        guild_id: Option<Snowflake>,
        channel_id: Snowflake,
        message_id: Snowflake,
    },
    /// Catch-all for subscribed events not yet modelled (keeps the adapter
    /// total while S3+ grows coverage slice by slice). Owned so events can
    /// round-trip through serde (DB-backed session resume, S3+).
    Unmodelled {
        kind: String,
    },
}

impl CoreEvent {
    /// Guild this event belongs to, if any (DM events carry None).
    #[must_use]
    pub fn guild_id(&self) -> Option<Snowflake> {
        match self {
            Self::MemberJoined { guild_id, .. }
            | Self::MemberLeft { guild_id, .. }
            | Self::VoiceStateChanged { guild_id, .. } => Some(*guild_id),
            Self::MessageCreated { guild_id, .. } | Self::MessageDeleted { guild_id, .. } => {
                *guild_id
            }
            Self::ReactionAdded(reaction) | Self::ReactionRemoved(reaction) => reaction.guild_id,
            Self::Unmodelled { .. } => None,
        }
    }

    /// Delivery key for reaction consumers. Retry the same event with this
    /// original pair, never mint another identity for detached work.
    #[must_use]
    pub fn starboard_delivery_key(&self) -> Option<(String, String)> {
        match self {
            Self::ReactionAdded(reaction) | Self::ReactionRemoved(reaction) => Some((
                reaction.delivery.event_id.clone(),
                reaction.delivery.event_order.clone(),
            )),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_move_round_trips_through_json() {
        let event = CoreEvent::VoiceStateChanged {
            guild_id: 1,
            user_id: 2,
            delta: VoiceSessionDelta::Move {
                from_channel_id: Some(10),
                to_channel_id: Some(11),
            },
        };
        let json = serde_json::to_string(&event).expect("serializes");
        let back: CoreEvent = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(event, back);
        assert_eq!(event.guild_id(), Some(1));
    }
}
