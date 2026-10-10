//! Member presence facts for voice-room names (`docs/voice-rooms.md` §V5/§V6).
//!
//! The gateway reports each member's activities (`PRESENCE_UPDATE`, and the
//! presences in `GUILD_CREATE`) and whether they are streaming through
//! Discord (`self_stream` on their voice state). This module turns those
//! facts for the members of one room into the inputs the template engine
//! reads: `@@game_name@@`, `@@num_playing@@`, `@@num_live@@`,
//! `@@stream_name@@`, the party tokens, and the `PLAYING`, `LIVE*`,
//! `ANY_LIVE`, `GAME`, `PLAYERS`, `RICH` and `MAX` conditions.
//!
//! Pure: no Discord wire types, store or clock. The runtime maps wire
//! activities to [`MemberPresence`] with [`MemberPresence::from_activities`]
//! and calls [`apply_room_presence`] after it has filled the rest of the
//! room's [`RoomContext`] and [`ConditionFacts`].

use crate::voice_alias::{resolve_game, AliasTable};
use crate::voice_conditions::ConditionFacts;
use crate::voice_naming::{
    majority_games, resolve_majority_game, GameOptions, PartyInfo, RoomContext,
};

/// Longest activity title or party text kept, in characters. Rendered names
/// are cut at 100 characters, so nothing longer can ever show.
pub const MAX_PRESENCE_TEXT_CHARS: usize = 128;

/// Discord activity kinds this module reads (the gateway `type` field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    /// `0`: playing a game.
    Playing,
    /// `1`: live on an external platform (Twitch, YouTube).
    Streaming,
    /// Listening, watching, custom status, competing: not a game.
    Other,
}

/// One wire activity, reduced to what room names use.
#[derive(Debug, Clone, Copy)]
pub struct ActivityFacts<'a> {
    pub kind: ActivityKind,
    pub name: &'a str,
    pub details: Option<&'a str>,
    pub state: Option<&'a str>,
    /// `party.size` as `[current, max]`.
    pub party_size: Option<[u64; 2]>,
}

/// What one member's presence contributes to a room name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemberPresence {
    /// Title of the first game activity, trimmed and bounded.
    pub game: Option<String>,
    /// The party that game advertises.
    pub party: Option<PartyInfo>,
    /// Live on an external platform.
    pub live_external: bool,
    /// Title of that stream (details, else state, else the platform name).
    pub stream_title: Option<String>,
}

fn bounded(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(MAX_PRESENCE_TEXT_CHARS).collect())
}

impl MemberPresence {
    /// Reduce a member's activities. The first game and the first stream
    /// win, matching how Discord lists the member's main activity first.
    #[must_use]
    pub fn from_activities(activities: &[ActivityFacts<'_>]) -> Self {
        let mut presence = Self::default();
        for activity in activities {
            match activity.kind {
                ActivityKind::Playing if presence.game.is_none() => {
                    let Some(game) = bounded(activity.name) else {
                        continue;
                    };
                    presence.game = Some(game);
                    presence.party = activity.party_size.map(|[current, max]| PartyInfo {
                        size: u32::try_from(current).unwrap_or(u32::MAX),
                        max: (max > 0).then(|| u32::try_from(max).unwrap_or(u32::MAX)),
                        state: activity.state.and_then(bounded).unwrap_or_default(),
                        details: activity.details.and_then(bounded).unwrap_or_default(),
                    });
                }
                ActivityKind::Streaming if !presence.live_external => {
                    presence.live_external = true;
                    presence.stream_title = activity
                        .details
                        .and_then(bounded)
                        .or_else(|| activity.state.and_then(bounded))
                        .or_else(|| bounded(activity.name));
                }
                _ => {}
            }
        }
        presence
    }

    /// Nothing worth keeping: the runtime drops empty entries to bound memory.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.game.is_none() && !self.live_external
    }
}

/// One human occupant of the room.
#[derive(Debug, Clone, Copy)]
pub struct OccupantPresence<'a> {
    pub presence: Option<&'a MemberPresence>,
    /// Streaming through Discord (voice state `self_stream`).
    pub live_discord: bool,
    pub is_owner: bool,
}

/// Fill the presence-derived fields of a room's naming context and
/// conditions. Everything else in `context` and `conditions` is left as the
/// caller set it. `options.aliases` is ignored in favour of `aliases`, which
/// matches activity names the way `/alias` stores them (folded, whole key).
pub fn apply_room_presence(
    context: &mut RoomContext,
    conditions: &mut ConditionFacts,
    occupants: &[OccupantPresence<'_>],
    aliases: &AliasTable,
    options: &GameOptions,
) {
    let resolved = |presence: Option<&MemberPresence>| -> Option<String> {
        presence
            .and_then(|presence| presence.game.as_deref())
            .map(|game| resolve_game(game, aliases).to_owned())
    };
    let activities: Vec<Option<String>> = occupants
        .iter()
        .map(|occupant| resolved(occupant.presence))
        .collect();
    let owner = occupants.iter().find(|occupant| occupant.is_owner);
    let owner_game = owner.and_then(|owner| resolved(owner.presence));
    let options = GameOptions {
        aliases: Default::default(),
        ..options.clone()
    };

    context.game_name = resolve_majority_game(&activities, owner_game.as_deref(), &options);
    conditions.games = majority_games(&activities, owner_game.as_deref(), &options);
    context.members_playing = count(activities.iter().filter(|game| game.is_some()));

    let live_discord = count(occupants.iter().filter(|occupant| occupant.live_discord));
    let live_external = count(occupants.iter().filter(|occupant| {
        occupant
            .presence
            .is_some_and(|presence| presence.live_external)
    }));
    context.live_count = count(occupants.iter().filter(|occupant| {
        occupant.live_discord
            || occupant
                .presence
                .is_some_and(|presence| presence.live_external)
    }));
    conditions.live_discord_count = live_discord;
    conditions.live_external_count = live_external;

    conditions.owner_playing = owner_game.is_some();
    conditions.owner_live_discord = owner.is_some_and(|owner| owner.live_discord);
    conditions.owner_live_external = owner.is_some_and(|owner| {
        owner
            .presence
            .is_some_and(|presence| presence.live_external)
    });
    context.stream_title = owner
        .and_then(|owner| owner.presence)
        .filter(|presence| presence.live_external)
        .and_then(|presence| presence.stream_title.clone())
        .unwrap_or_default();

    let mut parties: Vec<PartyInfo> = Vec::new();
    for party in occupants.iter().filter_map(|occupant| {
        occupant
            .presence
            .and_then(|presence| presence.party.as_ref())
    }) {
        if !parties.contains(party) {
            parties.push(party.clone());
        }
    }
    context.parties = parties;
}

fn count<T>(items: impl Iterator<Item = T>) -> u32 {
    u32::try_from(items.count()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playing<'a>(name: &'a str, party: Option<[u64; 2]>) -> ActivityFacts<'a> {
        ActivityFacts {
            kind: ActivityKind::Playing,
            name,
            details: Some("Ranked"),
            state: Some("In queue"),
            party_size: party,
        }
    }

    fn streaming<'a>(details: Option<&'a str>) -> ActivityFacts<'a> {
        ActivityFacts {
            kind: ActivityKind::Streaming,
            name: "Twitch",
            details,
            state: None,
            party_size: None,
        }
    }

    fn options() -> GameOptions {
        GameOptions {
            no_game_label: "Hangout".to_owned(),
            ..GameOptions::default()
        }
    }

    #[test]
    fn first_game_and_first_stream_win() {
        let presence = MemberPresence::from_activities(&[
            ActivityFacts {
                kind: ActivityKind::Other,
                name: "Spotify",
                details: None,
                state: None,
                party_size: None,
            },
            playing("Apex Legends", Some([2, 3])),
            playing("Minecraft", None),
            streaming(Some("Ranked grind")),
        ]);
        assert_eq!(presence.game.as_deref(), Some("Apex Legends"));
        let party = presence.party.clone().expect("party");
        assert_eq!((party.size, party.max), (2, Some(3)));
        assert!(presence.live_external);
        assert_eq!(presence.stream_title.as_deref(), Some("Ranked grind"));
        assert!(!presence.is_empty());
    }

    #[test]
    fn blank_titles_and_other_activities_are_empty() {
        let presence = MemberPresence::from_activities(&[
            playing("   ", None),
            ActivityFacts {
                kind: ActivityKind::Other,
                name: "Custom Status",
                details: None,
                state: None,
                party_size: None,
            },
        ]);
        assert!(presence.is_empty());
        assert_eq!(presence, MemberPresence::default());
    }

    #[test]
    fn titles_are_bounded() {
        let long = "x".repeat(500);
        let presence = MemberPresence::from_activities(&[playing(&long, None)]);
        assert_eq!(
            presence.game.as_deref().map(|game| game.chars().count()),
            Some(MAX_PRESENCE_TEXT_CHARS)
        );
    }

    #[test]
    fn zero_party_max_means_no_max() {
        let presence = MemberPresence::from_activities(&[playing("Game", Some([1, 0]))]);
        assert_eq!(presence.party.expect("party").max, None);
    }

    #[test]
    fn room_facts_follow_the_members() {
        let owner = MemberPresence::from_activities(&[playing("apex legends", Some([2, 3]))]);
        let friend = MemberPresence::from_activities(&[playing("Apex Legends", Some([2, 3]))]);
        let streamer = MemberPresence::from_activities(&[streaming(None)]);
        let aliases = AliasTable::from_entries([("apex legends", "Apex")]).expect("aliases");
        let mut context = RoomContext::default();
        let mut conditions = ConditionFacts::default();
        apply_room_presence(
            &mut context,
            &mut conditions,
            &[
                OccupantPresence {
                    presence: Some(&owner),
                    live_discord: true,
                    is_owner: true,
                },
                OccupantPresence {
                    presence: Some(&friend),
                    live_discord: false,
                    is_owner: false,
                },
                OccupantPresence {
                    presence: Some(&streamer),
                    live_discord: false,
                    is_owner: false,
                },
                OccupantPresence {
                    presence: None,
                    live_discord: false,
                    is_owner: false,
                },
            ],
            &aliases,
            &options(),
        );
        assert_eq!(context.game_name, "Apex");
        assert_eq!(conditions.games, vec!["Apex".to_owned()]);
        assert_eq!(context.members_playing, 2);
        assert_eq!(context.live_count, 2);
        assert_eq!(conditions.live_discord_count, 1);
        assert_eq!(conditions.live_external_count, 1);
        assert!(conditions.owner_playing);
        assert!(conditions.owner_live_discord);
        assert!(!conditions.owner_live_external);
        assert_eq!(context.stream_title, "");
        assert_eq!(context.parties.len(), 1, "identical parties collapse");
    }

    #[test]
    fn nobody_playing_shows_the_no_game_label() {
        let mut context = RoomContext::default();
        let mut conditions = ConditionFacts::default();
        let streamer = MemberPresence::from_activities(&[streaming(Some("Just chatting"))]);
        apply_room_presence(
            &mut context,
            &mut conditions,
            &[OccupantPresence {
                presence: Some(&streamer),
                live_discord: false,
                is_owner: true,
            }],
            &AliasTable::new(),
            &options(),
        );
        assert_eq!(context.game_name, "Hangout");
        assert!(conditions.games.is_empty());
        assert_eq!(context.members_playing, 0);
        assert!(!conditions.owner_playing);
        assert!(conditions.owner_live_external);
        assert_eq!(context.stream_title, "Just chatting");
        assert_eq!(context.live_count, 1);
    }

    #[test]
    fn empty_room_resets_presence_fields() {
        let mut context = RoomContext {
            game_name: "stale".to_owned(),
            members_playing: 4,
            live_count: 2,
            ..RoomContext::default()
        };
        let mut conditions = ConditionFacts {
            owner_playing: true,
            games: vec!["stale".to_owned()],
            ..ConditionFacts::default()
        };
        apply_room_presence(
            &mut context,
            &mut conditions,
            &[],
            &AliasTable::new(),
            &options(),
        );
        assert_eq!(context.game_name, "Hangout");
        assert_eq!(context.members_playing, 0);
        assert_eq!(context.live_count, 0);
        assert!(!conditions.owner_playing);
        assert!(conditions.games.is_empty());
    }
}
