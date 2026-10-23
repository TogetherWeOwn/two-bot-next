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
        gateway::payload::incoming::{
            MemberRemove, MessageDelete, ReactionAdd, ReactionRemove, VoiceStateUpdate,
        },
        guild::{Member, MemberFlags},
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

    fn reaction_like() -> GatewayReaction {
        GatewayReaction {
            burst: false,
            burst_colors: Vec::new(),
            channel_id: Id::new(7),
            emoji: EmojiReactionType::Unicode {
                name: "⭐".to_owned(),
            },
            guild_id: Some(Id::new(1)),
            member: None,
            message_author_id: None,
            message_id: Id::new(8),
            user_id: Id::new(2),
        }
    }

    #[test]
    fn reaction_delivery_is_replay_stable_and_session_scoped() {
        let first = reaction_delivery("session-a", 9);
        let replay = reaction_delivery("session-a", 9);
        assert_eq!(first, replay, "gateway replay must mint the same identity");
        assert!(!first.event_id.is_empty() && !first.event_order.is_empty());
        assert_ne!(
            first,
            reaction_delivery("session-b", 9),
            "order is comparable within one session only"
        );
        assert!(
            reaction_delivery("session-a", 9).event_order
                < reaction_delivery("session-a", 10).event_order,
            "fixed-width sequence orders lexicographically"
        );
    }

    #[test]
    fn with_delivery_carries_supplied_identity_without_delivery_mints_local() {
        let event = Event::ReactionAdd(Box::new(
            twilight_model::gateway::payload::incoming::ReactionAdd(reaction_like()),
        ));
        let supplied = reaction_delivery("session-a", 9);
        let carried = event_to_core_with_delivery(&event, &supplied);
        assert_eq!(
            carried.as_ref().and_then(CoreEvent::starboard_delivery_key),
            Some((supplied.event_id.clone(), supplied.event_order.clone())),
            "ingress-minted identity survives translation"
        );
        let local_a = event_to_core(&event).expect("reaction models");
        let local_b = event_to_core(&event).expect("reaction models");
        for local in [&local_a, &local_b] {
            let (id, order) = local
                .starboard_delivery_key()
                .expect("reactions carry a delivery key");
            assert!(id.starts_with("reaction:local:") && !order.is_empty());
        }
        assert_ne!(
            local_a.starboard_delivery_key(),
            local_b.starboard_delivery_key(),
            "local fallback never reuses an identity"
        );
    }

    fn member(user_id: u64, bot: bool) -> Member {
        let mut member_user = user(user_id);
        member_user.bot = bot;
        Member {
            avatar: None,
            avatar_decoration_data: None,
            banner: None,
            communication_disabled_until: None,
            deaf: false,
            flags: MemberFlags::empty(),
            joined_at: None,
            mute: false,
            nick: None,
            pending: false,
            premium_since: None,
            roles: vec![],
            user: member_user,
        }
    }

    fn custom_burst_reaction() -> GatewayReaction {
        // Every id is distinct, so any field swap fails the exact match.
        GatewayReaction {
            burst: true,
            burst_colors: Vec::new(),
            channel_id: Id::new(12),
            emoji: EmojiReactionType::Custom {
                animated: true,
                id: Id::new(16),
                name: Some("star".to_owned()),
            },
            guild_id: Some(Id::new(11)),
            member: Some(member(14, true)),
            message_author_id: Some(Id::new(15)),
            message_id: Id::new(13),
            user_id: Id::new(14),
        }
    }

    fn unicode_reaction() -> GatewayReaction {
        GatewayReaction {
            burst: false,
            burst_colors: Vec::new(),
            channel_id: Id::new(22),
            emoji: EmojiReactionType::Unicode {
                name: "⭐".to_owned(),
            },
            guild_id: Some(Id::new(21)),
            member: Some(member(24, false)),
            message_author_id: Some(Id::new(25)),
            message_id: Id::new(23),
            user_id: Id::new(24),
        }
    }

    fn bare_reaction() -> GatewayReaction {
        // No member and no author: pins the None arms of both mappings.
        GatewayReaction {
            burst: false,
            burst_colors: Vec::new(),
            channel_id: Id::new(32),
            emoji: EmojiReactionType::Unicode {
                name: "⭐".to_owned(),
            },
            guild_id: Some(Id::new(31)),
            member: None,
            message_author_id: None,
            message_id: Id::new(33),
            user_id: Id::new(34),
        }
    }

    #[test]
    fn reaction_add_and_remove_pin_exact_variant_and_fields() {
        // Builder pin (the acceptance target only asserts modelled + guild +
        // non-empty delivery key): exact CoreEvent equality, so an add/remove
        // arm swap, a channel/message swap, or a custom-emoji off-by-one turns
        // this suite red. The runtime card counts stars from these fields.
        let delivery = reaction_delivery("session-a", 9);
        for (make, emoji, burst, member_is_bot, author, ids) in [
            (
                custom_burst_reaction as fn() -> GatewayReaction,
                ReactionEmoji::Custom(16),
                true,
                Some(true),
                Some(15),
                (11, 12, 13, 14),
            ),
            (
                unicode_reaction as fn() -> GatewayReaction,
                ReactionEmoji::Unicode("⭐".to_owned()),
                false,
                Some(false),
                Some(25),
                (21, 22, 23, 24),
            ),
            (
                bare_reaction as fn() -> GatewayReaction,
                ReactionEmoji::Unicode("⭐".to_owned()),
                false,
                None,
                None,
                (31, 32, 33, 34),
            ),
        ] {
            let (guild, channel, message, user) = ids;
            let expected = ReactionEvent {
                guild_id: Some(guild),
                channel_id: channel,
                message_id: message,
                user_id: user,
                message_author_id: author,
                member_is_bot,
                emoji,
                burst,
                delivery: delivery.clone(),
            };
            let add = Event::ReactionAdd(Box::new(ReactionAdd(make())));
            assert_eq!(
                event_to_core_with_delivery(&add, &delivery),
                Some(CoreEvent::ReactionAdded(expected.clone())),
                "ReactionAdd maps every field exactly"
            );
            let remove = Event::ReactionRemove(Box::new(ReactionRemove(make())));
            assert_eq!(
                event_to_core_with_delivery(&remove, &delivery),
                Some(CoreEvent::ReactionRemoved(expected)),
                "ReactionRemove maps every field exactly"
            );
        }
    }

    #[test]
    fn reaction_delivery_hashes_session_id() {
        // The D1 claim is that the session is hashed, not copied: the raw
        // gateway session id must appear in neither delivery part.
        let delivery = reaction_delivery("session-a", 9);
        assert!(
            !delivery.event_id.contains("session-a"),
            "event_id carries the hash, not the raw session"
        );
        assert!(
            !delivery.event_order.contains("session-a"),
            "event_order carries the hash, not the raw session"
        );
    }

    #[test]
    fn message_delete_pins_exact_ids() {
        // Exact match with distinct ids, so a channel/message swap turns red.
        let event = Event::MessageDelete(MessageDelete {
            channel_id: Id::new(42),
            guild_id: Some(Id::new(41)),
            id: Id::new(43),
        });
        assert_eq!(
            event_to_core(&event),
            Some(CoreEvent::MessageDeleted {
                guild_id: Some(41),
                channel_id: 42,
                message_id: 43,
            })
        );
    }
}
