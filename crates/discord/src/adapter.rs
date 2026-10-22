//! Twilight → core event translation.
//!
//! Member, voice, message and reaction events are modelled. Remaining
//! subscribed kinds arrive as [`CoreEvent::Unmodelled`] with a stable tag.

use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};
use twilight_model::{
    channel::message::EmojiReactionType,
    gateway::{event::Event, GatewayReaction},
};
use two_bot_core::{
    events::{ReactionDelivery, ReactionEmoji, ReactionEvent},
    CoreEvent, VoiceSessionDelta,
};

/// Replay-stable identity from the gateway session and dispatch sequence.
/// The session is hashed, not copied into diagnostic or persisted event IDs.
/// Order is fixed-width and comparable within the same session only.
#[must_use]
pub fn reaction_delivery(session_id: &str, sequence: u64) -> ReactionDelivery {
    let session = hex::encode(Sha256::digest(session_id.as_bytes()));
    ReactionDelivery {
        event_id: format!("reaction:{session}:{sequence}"),
        event_order: format!("{session}:{sequence:020}"),
    }
}

fn local_delivery() -> ReactionDelivery {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let ordinal = NEXT.fetch_add(1, Ordering::Relaxed);
    let now = two_bot_core::funnel::now_millis_for_test();
    ReactionDelivery {
        event_id: format!("reaction:local:{}:{now}:{ordinal}", std::process::id()),
        event_order: format!("{now:020}:{ordinal:020}"),
    }
}

fn reaction_event(
    reaction: &GatewayReaction,
    delivery: Option<&ReactionDelivery>,
) -> ReactionEvent {
    ReactionEvent {
        guild_id: reaction.guild_id.map(|id| id.get()),
        channel_id: reaction.channel_id.get(),
        message_id: reaction.message_id.get(),
        user_id: reaction.user_id.get(),
        message_author_id: reaction.message_author_id.map(|id| id.get()),
        member_is_bot: reaction.member.as_ref().map(|member| member.user.bot),
        emoji: match &reaction.emoji {
            EmojiReactionType::Unicode { name } => ReactionEmoji::Unicode(name.clone()),
            EmojiReactionType::Custom { id, .. } => ReactionEmoji::Custom(id.get()),
        },
        burst: reaction.burst,
        delivery: delivery.cloned().unwrap_or_else(local_delivery),
    }
}

/// Translate one twilight gateway dispatch into a framework-free core event.
///
/// Voice deltas are derived from the *current* channel in the payload: when
/// `channel_id` is `None` the member left voice; otherwise they joined (S1
/// has no previous-state store — S3 adds join/leave/move resolution against
/// the in-memory session map).
#[must_use]
pub fn event_to_core(event: &Event) -> Option<CoreEvent> {
    event_to_core_inner(event, None)
}

/// Translate with an ingress-minted identity. Gateway consumers must use this
/// entry point for replay handling; `event_to_core` can only mint a local
/// identity because a parsed payload has no session or sequence metadata.
#[must_use]
pub fn event_to_core_with_delivery(
    event: &Event,
    delivery: &ReactionDelivery,
) -> Option<CoreEvent> {
    event_to_core_inner(event, Some(delivery))
}

fn event_to_core_inner(event: &Event, delivery: Option<&ReactionDelivery>) -> Option<CoreEvent> {
    match event {
        Event::MemberAdd(add) => Some(CoreEvent::MemberJoined {
            guild_id: add.guild_id.get(),
            user_id: add.user.id.get(),
        }),
        Event::MemberRemove(remove) => Some(CoreEvent::MemberLeft {
            guild_id: remove.guild_id.get(),
            user_id: remove.user.id.get(),
        }),
        Event::VoiceStateUpdate(update) => {
            let guild_id = update.guild_id?;
            let delta = match update.channel_id {
                Some(channel) => VoiceSessionDelta::Join {
                    channel_id: Some(channel.get()),
                },
                None => VoiceSessionDelta::Leave,
            };
            Some(CoreEvent::VoiceStateChanged {
                guild_id: guild_id.get(),
                user_id: update.user_id.get(),
                delta,
            })
        }
        Event::MessageCreate(message) => Some(CoreEvent::MessageCreated {
            guild_id: message.guild_id.map(|id| id.get()),
            channel_id: message.channel_id.get(),
            message_id: message.id.get(),
            author_id: message.author.id.get(),
            author_is_bot: message.author.bot,
        }),
        Event::ReactionAdd(reaction) => {
            Some(CoreEvent::ReactionAdded(reaction_event(reaction, delivery)))
        }
        Event::ReactionRemove(reaction) => Some(CoreEvent::ReactionRemoved(reaction_event(
            reaction, delivery,
        ))),
        Event::MessageDelete(message) => Some(CoreEvent::MessageDeleted {
            guild_id: message.guild_id.map(|id| id.get()),
            channel_id: message.channel_id.get(),
            message_id: message.id.get(),
        }),
        // Modelled in later slices; tagged so coverage is auditable.
        Event::InviteCreate(_) => Some(CoreEvent::Unmodelled {
            kind: "invite_create".to_owned(),
        }),
        Event::InteractionCreate(_) => Some(CoreEvent::Unmodelled {
            kind: "interaction_create".to_owned(),
        }),
        Event::GuildAuditLogEntryCreate(_) => Some(CoreEvent::Unmodelled {
            kind: "audit_log_entry_create".to_owned(),
        }),
        // Connection lifecycle and unsubscribed events: no core event.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use twilight_model::{
        gateway::payload::incoming::{MemberRemove, VoiceStateUpdate},
        guild::Member,
        id::Id,
        user::User,
        voice::VoiceState,
    };

    fn user(id: u64) -> User {
        User {
            accent_color: None,
            avatar: None,
            avatar_decoration: None,
            avatar_decoration_data: None,
            banner: None,
            bot: false,
            discriminator: 0,
            email: None,
            flags: None,
            global_name: None,
            id: Id::new(id),
            locale: None,
            mfa_enabled: None,
            name: "test".to_owned(),
            premium_type: None,
            primary_guild: None,
            public_flags: None,
            system: None,
            verified: None,
        }
    }

    #[test]
    fn member_remove_maps_to_member_left() {
        let event = Event::MemberRemove(MemberRemove {
            guild_id: Id::new(10),
            user: user(20),
        });
        assert_eq!(
            event_to_core(&event),
            Some(CoreEvent::MemberLeft {
                guild_id: 10,
                user_id: 20,
            })
        );
    }

    #[test]
    fn voice_update_without_channel_is_leave() {
        let state = VoiceState {
            channel_id: None,
            deaf: false,
            guild_id: Some(Id::new(10)),
            member: None::<Member>,
            mute: false,
            self_deaf: false,
            self_mute: false,
            self_stream: false,
            self_video: false,
            session_id: "s".to_owned(),
            suppress: false,
            user_id: Id::new(20),
            request_to_speak_timestamp: None,
        };
        let event = Event::VoiceStateUpdate(Box::new(VoiceStateUpdate(state)));
        assert_eq!(
            event_to_core(&event),
            Some(CoreEvent::VoiceStateChanged {
                guild_id: 10,
                user_id: 20,
                delta: VoiceSessionDelta::Leave,
            })
        );
    }

    #[test]
    fn lifecycle_events_yield_nothing() {
        assert_eq!(event_to_core(&Event::GatewayClose(None)), None);
    }
}
