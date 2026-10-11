//! V5b wiring acceptance: the naming engine drives room create/rename names
//! through the public core API, with raw-name fallback on template error.
//!
//! The room-lifecycle runtime (V1) does not exist yet, so these tests model
//! its future call path: allocate a number, build a [`RoomContext`] from the
//! creator's template and current room state, and render via
//! [`resolve_room_name`]. Re-render on membership change models rename.

use std::collections::HashMap;

use two_bot_core::voice_naming::{
    allocate_room_number, resolve_room_name, ChannelKind, RoomContext, DEFAULT_FALLBACK_NAME,
    DEFAULT_NAME_TEMPLATE, MAX_NAME_LEN,
};

fn ctx(owner: &str, members: u32, room_number: u32, seed: u64) -> RoomContext {
    RoomContext {
        channel_kind: ChannelKind::Temporary,
        room_number,
        owner_name: owner.to_string(),
        original_creator_name: owner.to_string(),
        member_count: members,
        owner_present: true,
        live_count: 0,
        user_limit: 0,
        game_name: String::new(),
        stream_title: String::new(),
        members_playing: 0,
        parties: Vec::new(),
        timestamp: 1_790_683_200, // 2026-09-29 12:00:00 UTC (a Tuesday).
        room_minutes: 0,
        game_minutes: 0,
        tz_offset_minutes: 0,
        seed,
        named_lists: HashMap::new(),
        fallback_name: "Hangout".to_string(),
    }
}

/// Create path: number allocation + template render produce the channel name.
#[test]
fn create_renders_templated_name_with_allocated_number() {
    let number = allocate_room_number(&[1, 2], 1).expect("free number");
    assert_eq!(number, 3);
    let name = resolve_room_name("@@owner@@ ##", &ctx("Ava", 1, number, 7), "Hangout");
    assert_eq!(name, "Ava #3");
}

/// Create path with the spec default template (spec V5).
#[test]
fn create_default_template_names_owner_room() {
    let name = resolve_room_name(DEFAULT_NAME_TEMPLATE, &ctx("Ava", 1, 1, 42), "Hangout");
    assert!(name.contains("Ava's "), "unexpected {name:?}");
    assert!(name.chars().count() <= MAX_NAME_LEN);
}

/// Rename path: join/leave re-render, random picks stable across renames.
#[test]
fn rename_rerenders_on_membership_change_without_reroll() {
    let template = "@@random_emoji@@ @@num@@ <<person/people>>";
    let solo = resolve_room_name(template, &ctx("Ava", 1, 1, 9), "old name");
    assert!(solo.ends_with("1 person"), "unexpected {solo:?}");
    let busy = resolve_room_name(template, &ctx("Ava", 4, 1, 9), &solo);
    assert!(busy.ends_with("4 people"), "unexpected {busy:?}");
    assert_eq!(
        solo.split(' ').next(),
        busy.split(' ').next(),
        "rename re-rolled the seeded pick"
    );
}

/// Owner change (caretaker succession) renames after the new owner.
#[test]
fn rename_follows_ownership_change() {
    assert_eq!(
        resolve_room_name("@@owner@@ ##", &ctx("Ava", 2, 1, 1), "old"),
        "Ava #1"
    );
    let mut caretaker = ctx("Ava", 2, 1, 1);
    caretaker.owner_name = "Bo".to_string();
    assert_eq!(
        resolve_room_name("@@owner@@ ##", &caretaker, "Ava #1"),
        "Bo #1"
    );
}

/// Bad template (blank, oversized, empty-rendering) falls back to the raw
/// name without error; a blank raw name degrades to the built-in default.
#[test]
fn bad_template_falls_back_to_raw_name() {
    let c = ctx("Ava", 1, 1, 1);
    assert_eq!(resolve_room_name("", &c, "Hangout"), "Hangout");
    assert_eq!(
        resolve_room_name(&"x".repeat(4097), &c, "Hangout"),
        "Hangout"
    );
    assert_eq!(resolve_room_name("@@bogus@@", &c, "Hangout"), "Hangout");
    assert_eq!(
        resolve_room_name("@@bogus@@", &c, "   "),
        DEFAULT_FALLBACK_NAME
    );
    for name in [
        resolve_room_name("", &c, "Hangout"),
        resolve_room_name("@@bogus@@", &c, "   "),
        resolve_room_name(DEFAULT_NAME_TEMPLATE, &c, "Hangout"),
    ] {
        assert!(!name.is_empty());
        assert!(name.chars().count() <= MAX_NAME_LEN);
    }
}
