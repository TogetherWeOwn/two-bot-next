//! Hermetic V7c acceptance cases against the public core API.
//!
//! Table tests pin every resolved variable, all six preview states and the
//! owner-or-admin inspection gate; a property test proves every preview
//! renders without an empty-name violation under the V5 fallback contract.
//!
//! No tests in this fixture use a database, Redis, Discord, or a staging
//! identity.

use std::collections::{HashMap, HashSet};
use two_bot_core::voice_channelinfo::{
    may_inspect, preview_states, resolve_variables, ChannelInfoContext, PreviewState, TemplateKind,
    VARIABLE_COUNT,
};
use two_bot_core::voice_naming::{ChannelKind, PartyInfo, RoomContext, MAX_NAME_LEN};

fn room() -> RoomContext {
    RoomContext {
        channel_kind: ChannelKind::Temporary,
        room_number: 3,
        owner_name: "Ava".to_string(),
        original_creator_name: "Ben".to_string(),
        member_count: 4,
        owner_present: true,
        live_count: 1,
        user_limit: 10,
        game_name: "Apex".to_string(),
        stream_title: "Build day".to_string(),
        members_playing: 2,
        parties: vec![PartyInfo {
            size: 2,
            max: Some(5),
            state: "ready".to_string(),
            details: "map".to_string(),
        }],
        timestamp: 1_790_683_200, // 2026-09-29 12:00:00 UTC (a Tuesday).
        tz_offset_minutes: 0,
        seed: 42,
        named_lists: HashMap::new(),
        fallback_name: "Lounge".to_string(),
    }
}

fn ctx() -> ChannelInfoContext {
    ChannelInfoContext {
        room: room(),
        private: false,
        locked: false,
        name_template: "@@game_name@@ ##".to_string(),
        status_template: Some("@@num@@ playing".to_string()),
    }
}

#[test]
fn variable_map_has_exact_count_unique_names_and_display_order() {
    let map = resolve_variables(&ctx());
    assert_eq!(map.len(), VARIABLE_COUNT);
    assert!(!map.is_empty());
    let names: Vec<&str> = map.iter().map(|entry| entry.name).collect();
    let unique: HashSet<&&str> = names.iter().collect();
    assert_eq!(names.len(), unique.len(), "duplicate variable names");
    assert_eq!(
        names,
        [
            "##",
            "$#",
            "$0#",
            "$00#",
            "+#",
            "@@nato@@",
            "@@owner@@",
            "@@creator@@",
            "@@original_creator@@",
            "@@num@@",
            "@@num_others@@",
            "@@num_live@@",
            "@@limit@@",
            "@@slots@@",
            "@@game_name@@",
            "@@stream_name@@",
            "@@num_playing@@",
            "@@party_size@@",
            "@@party_state@@",
            "@@party_details@@",
            "@@weekday@@",
            "@@month@@",
            "@@hour@@",
            "@@random_emoji@@",
            "FULL",
            "PRIVATE",
            "LOCKED",
            "occupant_bucket",
            "room_number",
        ]
    );
    assert_eq!(map.get("@@bogus@@"), None);
}

#[test]
fn every_variable_resolves_to_its_current_value() {
    let map = resolve_variables(&ctx());
    for (name, expected) in [
        ("##", "#3"),
        ("$#", "3"),
        ("$0#", "03"),
        ("$00#", "003"),
        ("+#", "III"),
        ("@@nato@@", "Charlie"),
        ("@@owner@@", "Ava"),
        ("@@creator@@", "Ava"),
        ("@@original_creator@@", "Ben"),
        ("@@num@@", "4"),
        ("@@num_others@@", "3"),
        ("@@num_live@@", "1"),
        ("@@limit@@", "10"),
        ("@@slots@@", "6"),
        ("@@game_name@@", "Apex"),
        ("@@stream_name@@", "Build day"),
        ("@@num_playing@@", "2"),
        ("@@party_size@@", "5"),
        ("@@party_state@@", "ready"),
        ("@@party_details@@", "map"),
        ("@@weekday@@", "Tuesday"),
        ("@@month@@", "September"),
        ("@@hour@@", "12"),
        ("FULL", "false"),
        ("PRIVATE", "false"),
        ("LOCKED", "false"),
        ("occupant_bucket", "group"),
        ("room_number", "3"),
    ] {
        assert_eq!(map.get(name), Some(expected), "variable {name}");
    }
    // Seeded pick: stable across resolves, never empty.
    let emoji = map.get("@@random_emoji@@").expect("random emoji row");
    assert!(!emoji.is_empty());
    assert_eq!(
        emoji,
        resolve_variables(&ctx()).get("@@random_emoji@@").unwrap()
    );
}

#[test]
fn empty_states_surface_as_empty_strings() {
    let mut bare = ctx();
    bare.room.user_limit = 0;
    bare.room.stream_title.clear();
    bare.room.game_name.clear();
    bare.room.parties.clear();
    bare.room.members_playing = 0;
    let map = resolve_variables(&bare);
    // Raw substitutions, not trimmed/fallback names: blank states stay blank.
    assert_eq!(map.get("@@slots@@"), Some(""));
    assert_eq!(map.get("@@stream_name@@"), Some(""));
    assert_eq!(map.get("@@game_name@@"), Some(""));
    assert_eq!(map.get("@@party_size@@"), Some("0"));
    assert_eq!(map.get("@@party_state@@"), Some(""));
    assert_eq!(map.get("@@party_details@@"), Some(""));
}

#[test]
fn derived_state_flags_follow_v6_rules() {
    // FULL needs a limit and headcount at (or over) it.
    let mut full = ctx();
    full.room.member_count = 10;
    assert_eq!(resolve_variables(&full).get("FULL"), Some("true"));
    full.room.user_limit = 0;
    assert_eq!(resolve_variables(&full).get("FULL"), Some("false"));

    // PRIVATE is always false on standalone channels (V6), even when set.
    let mut standalone = ctx();
    standalone.room.channel_kind = ChannelKind::Standalone;
    standalone.private = true;
    assert_eq!(resolve_variables(&standalone).get("PRIVATE"), Some("false"));
    // On temporary rooms it follows the room's privacy flag.
    let mut private = ctx();
    private.private = true;
    assert_eq!(resolve_variables(&private).get("PRIVATE"), Some("true"));

    // LOCKED mirrors the opaque V3 lock flag.
    let mut locked = ctx();
    locked.locked = true;
    assert_eq!(resolve_variables(&locked).get("LOCKED"), Some("true"));

    // Occupant buckets never name members.
    for (members, bucket) in [
        (0, "empty"),
        (1, "solo"),
        (2, "duo"),
        (3, "group"),
        (99, "group"),
    ] {
        let mut c = ctx();
        c.room.member_count = members;
        assert_eq!(
            resolve_variables(&c).get("occupant_bucket"),
            Some(bucket),
            "members={members}"
        );
    }
}

#[test]
fn all_six_preview_states_render_in_button_order() {
    let previews = preview_states(&ctx());
    assert_eq!(previews.len(), PreviewState::ALL.len() * 2);
    for (index, state) in PreviewState::ALL.iter().enumerate() {
        let pair = &previews[index * 2..index * 2 + 2];
        assert_eq!(pair[0].state, *state);
        assert_eq!(pair[0].template_kind, TemplateKind::Name);
        assert_eq!(pair[0].template, "@@game_name@@ ##");
        assert_eq!(pair[1].state, *state);
        assert_eq!(pair[1].template_kind, TemplateKind::Status);
        assert_eq!(pair[1].template, "@@num@@ playing");
        for preview in pair {
            assert!(!preview.rendered.is_empty(), "empty {state:?} preview");
            assert!(
                preview.rendered.chars().count() <= MAX_NAME_LEN,
                "overlong {state:?} preview"
            );
        }
    }
    // Spot-check the scenario each state models.
    let name = |state: PreviewState| {
        previews
            .iter()
            .find(|p| p.state == state && p.template_kind == TemplateKind::Name)
            .unwrap()
            .rendered
            .clone()
    };
    assert_eq!(name(PreviewState::SoloNoGame), "#3");
    assert_eq!(name(PreviewState::InGame), "Apex #3");
    assert_eq!(name(PreviewState::Streaming), "Apex #3");
}

#[test]
fn previews_without_status_template_yield_name_only() {
    let mut c = ctx();
    c.status_template = None;
    let previews = preview_states(&c);
    assert_eq!(previews.len(), PreviewState::ALL.len());
    assert!(previews
        .iter()
        .all(|p| p.template_kind == TemplateKind::Name));
}

#[test]
fn full_and_locked_previews_render_at_limit_headcounts() {
    let mut c = ctx();
    c.name_template = "@@num@@/@@limit@@".to_string();
    c.status_template = None;
    let rendered = |state: PreviewState| {
        preview_states(&c)
            .into_iter()
            .find(|p| p.state == state)
            .unwrap()
            .rendered
    };
    assert_eq!(rendered(PreviewState::Full), "10/10");
    // A V3 headcount lock sets the limit to the current headcount, so locked
    // renders identically under V5 tokens; the state label carries the lock.
    assert_eq!(rendered(PreviewState::Locked), "10/10");
    // Unlimited rooms preview full/locked against a 4-headcount scenario.
    c.room.user_limit = 0;
    assert_eq!(
        preview_states(&c)
            .into_iter()
            .find(|p| p.state == PreviewState::Full)
            .unwrap()
            .rendered,
        "4/4"
    );
}

#[test]
fn inspect_gate_is_owner_or_admin_with_zero_ids_refused() {
    const OWNER: u64 = 7;
    const OTHER: u64 = 9;
    // Owner inspects their own room; strangers are refused.
    assert!(may_inspect(OWNER, false, OWNER));
    assert!(!may_inspect(OTHER, false, OWNER));
    // Admins may inspect any room, including their own.
    assert!(may_inspect(OTHER, true, OWNER));
    assert!(may_inspect(OWNER, true, OWNER));
    assert!(may_inspect(OWNER, true, OTHER));
    // Zero IDs are never authorized.
    assert!(!may_inspect(0, false, OWNER));
    assert!(!may_inspect(OWNER, false, 0));
    assert!(!may_inspect(0, true, OWNER));
    assert!(!may_inspect(OWNER, true, 0));
    assert!(!may_inspect(0, true, 0));
}

/// Deterministic generator so the property run is reproducible without extra
/// dev-dependencies.
struct Gen {
    state: u64,
}

impl Gen {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.state >> 33
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % (n as u64).max(1)) as usize
    }

    fn ctx(&mut self) -> ChannelInfoContext {
        let games = ["", "Apex", "Valorant"];
        let streams = ["", "Live build"];
        ChannelInfoContext {
            room: RoomContext {
                channel_kind: if self.below(2) == 0 {
                    ChannelKind::Temporary
                } else {
                    ChannelKind::Standalone
                },
                room_number: self.below(60) as u32,
                owner_name: ["Ava", "  ", "Zoë🎮"][self.below(3)].to_string(),
                original_creator_name: "Ben".to_string(),
                member_count: self.below(12) as u32,
                owner_present: self.below(2) == 0,
                live_count: self.below(4) as u32,
                user_limit: [0, 2, 5, 10][self.below(4)],
                game_name: games[self.below(games.len())].to_string(),
                stream_title: streams[self.below(streams.len())].to_string(),
                members_playing: self.below(6) as u32,
                parties: Vec::new(),
                timestamp: self.next() as i64,
                tz_offset_minutes: [-780, -720, 0, 180][self.below(4)],
                seed: self.next(),
                named_lists: HashMap::new(),
                fallback_name: ["Lounge", "", "   "][self.below(3)].to_string(),
            },
            private: self.below(2) == 0,
            locked: self.below(2) == 0,
            name_template: [
                "@@game_name@@ ##",
                "@@owner@@'s room (@@num@@/@@limit@@)",
                "",
                "   ",
                "@@bogus@@",
            ][self.below(5)]
            .to_string(),
            status_template: if self.below(2) == 0 {
                None
            } else {
                Some("@@num@@ playing @@stream_name@@".to_string())
            },
        }
    }
}

#[test]
fn prop_every_preview_renders_under_the_v5_fallback_contract() {
    let mut gen = Gen::new(0xC11C);
    for _ in 0..5_000 {
        let context = gen.ctx();
        for preview in preview_states(&context) {
            assert!(
                !preview.rendered.is_empty(),
                "empty {:?} preview for {context:?}",
                preview.state
            );
            assert!(
                preview.rendered.chars().count() <= MAX_NAME_LEN,
                "overlong {:?} preview for {context:?}",
                preview.state
            );
        }
    }
}
