//! The shared voice-template corpus context (`tests/voice_templates/corpus.json`)
//! and its adaptation to the engine: the V5 name tokens ([`RoomContext`]) and
//! the V6 condition facts ([`ConditionFacts`]). One mapping serves every
//! corpus that inlines these contexts, so their renders agree.

use std::collections::HashMap;

use serde::Deserialize;
use time::{Date, Month};
use two_bot_core::voice_conditions::ConditionFacts;
use two_bot_core::voice_naming::{
    majority_games, resolve_majority_game, ChannelKind, GameOptions, PartyInfo, RoomContext,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorpusContext {
    pub channel_kind: String,
    pub number: u32,
    pub limit: u32,
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

/// The majority inputs every mapping derives from one snapshot: one activity
/// entry per member (`None` = no visible activity) plus the corpus majority
/// settings.
fn game_inputs(context: &CorpusContext) -> (Vec<Option<String>>, GameOptions) {
    let activities = context.members.iter().map(|m| m.game.clone()).collect();
    let options = GameOptions {
        aliases: context.settings.aliases.clone(),
        force_single: context.settings.force_single_game,
        count_idle_toward_majority: context.settings.include_inactive,
        no_game_label: context.settings.no_game.clone(),
    };
    (activities, options)
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
    let (activities, game_options) = game_inputs(context);
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

/// Facts the parent runtime resolves from guild state, read from the same
/// snapshot as [`room_context`]: the owner and members by ID, their roles,
/// the owner's game and streams, and the lock flag. `games` comes from
/// [`majority_games`] over the same inputs as `game_name`, so the `GAME`
/// condition always agrees with `@@game_name@@`.
pub fn condition_facts(context: &CorpusContext) -> ConditionFacts {
    let owner = context.members.iter().find(|m| m.id == context.owner_id);
    let (activities, game_options) = game_inputs(context);
    let owner_game = owner.and_then(|m| m.game.as_deref());
    let count = |is: fn(&Member) -> bool| context.members.iter().filter(|m| is(m)).count() as u32;
    ConditionFacts {
        owner_id: Some(context.owner_id.clone()),
        owner_role_ids: owner.map(|m| m.roles.clone()).unwrap_or_default(),
        member_ids: context.members.iter().map(|m| m.id.clone()).collect(),
        member_role_ids: context
            .members
            .iter()
            .flat_map(|m| m.roles.iter().cloned())
            .collect(),
        owner_playing: owner_game.is_some(),
        owner_live_discord: owner.is_some_and(|m| m.live_discord),
        owner_live_external: owner.is_some_and(|m| m.live_external),
        live_discord_count: count(|m| m.live_discord),
        live_external_count: count(|m| m.live_external),
        games: majority_games(&activities, owner_game, &game_options),
        private: context.private,
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
