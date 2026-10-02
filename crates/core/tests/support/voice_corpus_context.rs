//! The shared voice-template corpus context (`tests/voice_templates/corpus.json`)
//! and its adaptation to the V5 engine's [`RoomContext`]. One mapping serves
//! every corpus that inlines these contexts, so their renders agree.

use std::collections::HashMap;

use serde::Deserialize;
use time::{Date, Month};
use two_bot_core::voice_naming::{
    resolve_majority_game, ChannelKind, GameOptions, PartyInfo, RoomContext,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorpusContext {
    pub channel_kind: String,
    pub number: u32,
    pub limit: u32,
    /// Read by condition facts, not by the room context.
    #[allow(dead_code)]
    pub private: bool,
    pub seed: String,
    pub owner_id: String,
    pub original_creator_name: String,
    pub members: Vec<Member>,
    pub clock: Clock,
    pub settings: Settings,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Member {
    pub id: String,
    pub display_name: String,
    pub nick: Option<String>,
    /// Read by condition facts, not by the room context.
    #[allow(dead_code)]
    pub roles: Vec<String>,
    pub game: Option<String>,
    pub live_discord: bool,
    pub live_external: bool,
    pub stream_title: Option<String>,
    pub party: Option<Party>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Party {
    pub id: String,
    pub size: u32,
    pub maximum: Option<u32>,
    pub state: String,
    pub details: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Clock {
    pub weekday: String,
    pub month: String,
    pub hour: u8,
    pub timezone: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub no_game: String,
    pub aliases: HashMap<String, String>,
    pub named_lists: HashMap<String, Vec<String>>,
    pub force_single_game: bool,
    pub include_inactive: bool,
}

/// Adapt the implementation-independent snapshot to the engine's context,
/// following V5 and V7: owner name prefers `/nick`, the game is the
/// alias-resolved majority, and parties are deduplicated by ID in first-seen
/// order.
pub fn room_context(context: &CorpusContext) -> RoomContext {
    let owner = context.members.iter().find(|m| m.id == context.owner_id);
    let mut parties: Vec<(&str, PartyInfo)> = Vec::new();
    for party in context.members.iter().filter_map(|m| m.party.as_ref()) {
        if !parties.iter().any(|(id, _)| *id == party.id) {
            parties.push((
                party.id.as_str(),
                PartyInfo {
                    size: party.size,
                    max: party.maximum,
                    state: party.state.clone(),
                    details: party.details.clone(),
                },
            ));
        }
    }
    let activities: Vec<Option<String>> = context.members.iter().map(|m| m.game.clone()).collect();
    let game_options = GameOptions {
        aliases: context.settings.aliases.clone(),
        force_single: context.settings.force_single_game,
        count_idle_toward_majority: context.settings.include_inactive,
        no_game_label: context.settings.no_game.clone(),
    };
    RoomContext {
        channel_kind: match context.channel_kind.as_str() {
            "temporary" => ChannelKind::Temporary,
            "standalone" | "stage" => ChannelKind::Standalone,
            other => panic!("unknown channel kind {other}"),
        },
        room_number: context.number,
        owner_name: owner
            .map(|m| m.nick.clone().unwrap_or_else(|| m.display_name.clone()))
            .unwrap_or_default(),
        original_creator_name: context.original_creator_name.clone(),
        member_count: context.members.len() as u32,
        owner_present: owner.is_some(),
        live_count: context.members.iter().filter(|m| is_live(m)).count() as u32,
        user_limit: context.limit,
        game_name: resolve_majority_game(
            &activities,
            owner.and_then(|m| m.game.as_deref()),
            &game_options,
        ),
        stream_title: owner
            .filter(|m| is_live(m))
            .and_then(|m| m.stream_title.clone())
            .unwrap_or_default(),
        members_playing: context.members.iter().filter(|m| m.game.is_some()).count() as u32,
        parties: parties.into_iter().map(|(_, party)| party).collect(),
        timestamp: civil_timestamp(&context.clock),
        tz_offset_minutes: 0,
        seed: seed(&context.seed),
        named_lists: context.settings.named_lists.clone(),
        fallback_name: String::new(),
    }
}

/// V5: `@@num_live@@` counts members streaming in Discord or externally.
fn is_live(member: &Member) -> bool {
    member.live_discord || member.live_external
}

/// The corpus clock is already guild-local civil time. Pick the first day of
/// that month in 2026 with that weekday, at that hour, as a UTC timestamp.
fn civil_timestamp(clock: &Clock) -> i64 {
    assert_eq!(clock.timezone, "UTC", "corpus clocks are UTC");
    let month = (1..=12u8)
        .map(|m| Month::try_from(m).expect("month"))
        .find(|m| m.to_string() == clock.month)
        .unwrap_or_else(|| panic!("unknown month {}", clock.month));
    let mut date = Date::from_calendar_date(2026, month, 1).expect("date");
    while date.weekday().to_string() != clock.weekday {
        date = date.next_day().expect("next day");
        assert_eq!(date.month(), month, "unknown weekday {}", clock.weekday);
    }
    date.with_hms(clock.hour, 0, 0)
        .expect("hour")
        .assume_utc()
        .unix_timestamp()
}

/// The corpus seed is opaque; map it consistently with 64-bit FNV-1a.
fn seed(value: &str) -> u64 {
    value.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}
