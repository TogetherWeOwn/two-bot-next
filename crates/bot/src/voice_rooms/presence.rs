//! Gateway presence → room-name facts (`TWO_VOICE_PRESENCE=1`).
//!
//! Maps twilight presences to [`MemberPresence`] and lets a game or stream
//! change re-render the occupant's room on the next template-name tick.

use twilight_model::gateway::presence::{ActivityType, Presence, UserOrId};
use two_bot_core::voice_presence::{ActivityFacts, ActivityKind, MemberPresence};

use super::{GuildRoomWorker, RoomPersistence, RoomWrites, Snowflake};

pub(super) fn member_id(presence: &Presence) -> Snowflake {
    match &presence.user {
        UserOrId::User(user) => user.id.get(),
        UserOrId::UserId { id } => id.get(),
    }
}

pub(super) fn facts(presence: &Presence) -> MemberPresence {
    let activities: Vec<ActivityFacts<'_>> = presence
        .activities
        .iter()
        .map(|activity| ActivityFacts {
            kind: match activity.kind {
                ActivityType::Playing => ActivityKind::Playing,
                ActivityType::Streaming => ActivityKind::Streaming,
                _ => ActivityKind::Other,
            },
            name: &activity.name,
            details: activity.details.as_deref(),
            state: activity.state.as_deref(),
            party_size: activity.party.as_ref().and_then(|party| party.size),
        })
        .collect();
    MemberPresence::from_activities(&activities)
}

impl<S: RoomPersistence, H: RoomWrites> GuildRoomWorker<S, H> {
    /// An occupant's game or stream changed. Forget the room's last rendered
    /// signature so the next template-name tick re-renders it (through the
    /// rename coalescer, within Discord's rename budget).
    pub(super) fn room_facts_changed(&mut self, room: Snowflake, _now_ms: u64) {
        if self.rooms.contains_key(&room) {
            self.name_signatures.remove(&room);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn presence(activities: serde_json::Value) -> Presence {
        serde_json::from_value(json!({
            "user": {"id": "42"},
            "guild_id": "7",
            "status": "online",
            "activities": activities,
            "client_status": {"desktop": "online"}
        }))
        .expect("presence")
    }

    #[test]
    fn wire_presence_maps_game_party_and_stream() {
        let presence = presence(json!([
            {"type": 2, "name": "Spotify"},
            {"type": 0, "name": "Apex Legends", "state": "In a party", "details": "Ranked",
             "party": {"id": "p", "size": [2, 3]}},
            {"type": 1, "name": "Twitch", "details": "Grinding", "url": "https://twitch.tv/x"}
        ]));
        assert_eq!(member_id(&presence), 42);
        let facts = facts(&presence);
        assert_eq!(facts.game.as_deref(), Some("Apex Legends"));
        let party = facts.party.expect("party");
        assert_eq!((party.size, party.max), (2, Some(3)));
        assert!(facts.live_external);
        assert_eq!(facts.stream_title.as_deref(), Some("Grinding"));
    }

    #[test]
    fn live_facts_report_the_room_only_on_change() {
        let live = super::super::LiveGuild::new(7);
        let game = facts(&presence(json!([{"type": 0, "name": "Apex Legends"}])));
        // Not in voice: remembered, but no room to re-render.
        assert_eq!(live.set_presence(42, game.clone()), None);
        live.voice_update(42, Some(900), Some(false));
        // Same facts again: no change.
        assert_eq!(live.set_presence(42, game.clone()), None);
        let other = facts(&presence(json!([{"type": 0, "name": "Minecraft"}])));
        assert_eq!(live.set_presence(42, other), Some(900));
        assert_eq!(live.set_presence(42, MemberPresence::default()), Some(900));
        assert_eq!(live.set_presence(42, MemberPresence::default()), None);
        assert_eq!(live.set_self_stream(42, true), Some(900));
        assert_eq!(live.set_self_stream(42, true), None);
        assert_eq!(live.set_self_stream(42, false), Some(900));
        let state = live.read_state();
        assert!(state.presences.is_empty(), "empty facts are not kept");
        assert!(state.self_streaming.is_empty());
    }

    #[test]
    fn occupant_presences_flag_owner_and_streamers() {
        let live = super::super::LiveGuild::new(7);
        live.voice_update(1, Some(900), Some(false));
        live.voice_update(2, Some(900), Some(false));
        live.voice_update(3, Some(900), Some(true));
        live.set_presence(1, facts(&presence(json!([{"type": 0, "name": "Apex"}]))));
        live.set_self_stream(2, true);
        let state = live.read_state();
        let occupants = state.occupant_presences(900, 1);
        assert_eq!(occupants.len(), 2, "bots are not occupants");
        let owner = occupants
            .iter()
            .find(|(_, _, owner)| *owner)
            .expect("owner");
        assert_eq!(owner.0.and_then(|p| p.game.as_deref()), Some("Apex"));
        assert!(occupants
            .iter()
            .any(|(_, streaming, owner)| *streaming && !owner));
    }

    #[test]
    fn idle_presence_is_empty() {
        let presence = presence(json!([{"type": 4, "name": "Custom Status", "state": "afk"}]));
        assert!(facts(&presence).is_empty());
    }
}
