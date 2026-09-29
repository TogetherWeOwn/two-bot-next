//! Twilight → core event translation.
//!
//! S1 covers the member/voice/message spine; the remaining subscribed events
//! (reactions, invites, interactions, audit-log entries) arrive as
//! [`CoreEvent::Unmodelled`] with a stable `kind` tag so S3+ can promote them
//! one slice at a time without changing the adapter signature.

use twilight_model::gateway::event::Event;
use two_bot_core::{CoreEvent, VoiceSessionDelta};

/// Translate one twilight gateway dispatch into a framework-free core event.
///
/// Voice deltas are derived from the *current* channel in the payload: when
/// `channel_id` is `None` the member left voice; otherwise they joined (S1
/// has no previous-state store — S3 adds join/leave/move resolution against
/// the in-memory session map).
#[must_use]
pub fn event_to_core(event: &Event) -> Option<CoreEvent> {
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
        // Modelled in later slices; tagged so coverage is auditable.
        Event::ReactionAdd(_) => Some(CoreEvent::Unmodelled {
            kind: "reaction_add".to_owned(),
        }),
        Event::ReactionRemove(_) => Some(CoreEvent::Unmodelled {
            kind: "reaction_remove".to_owned(),
        }),
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
