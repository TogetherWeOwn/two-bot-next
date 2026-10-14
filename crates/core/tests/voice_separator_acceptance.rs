//! Acceptance tests for #957 voice separator (〢 U+3022).
//!
//! Failing-first companion for `feat(voice): use 〢 as the separator after
//! leading emoji in room names`. Owner scope (2026-10-13, narrowed): keep
//! existing naming logic; ONLY the separator between emoji and name becomes
//! 〢 with no spaces (e.g. `👾〢WARDOGS deep dive`), on create + rename paths.
//!
//! These tests use the public core API only (`resolve_room_name` plus the
//! rename coalescer) so they FAIL on pre-#957 `main` (space separator) and
//! PASS once #957 lands. No `src/` changes here.

use std::collections::HashMap;

use two_bot_core::voice_naming::{
    resolve_room_name, ChannelKind, RoomContext, DEFAULT_NAME_TEMPLATE,
};
use two_bot_core::voice_rename_coalescer::{should_rename, RenameCoalescer};

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

/// Create path: a literal emoji-led name mints with 〢 and no spaces.
#[test]
fn create_literal_emoji_name_uses_separator_with_no_spaces() {
    let c = ctx("Ava", 1, 1, 7);
    assert_eq!(
        resolve_room_name("👾 WARDOGS deep dive", &c, "fallback"),
        "👾〢WARDOGS deep dive"
    );
}

/// Create path: an emoji-led template renders emoji + 〢 + name, no spaces.
#[test]
fn create_emoji_template_uses_separator_with_no_spaces() {
    let name = resolve_room_name("👾 @@owner@@'s room", &ctx("Ava", 1, 1, 7), "Hangout");
    assert_eq!(name, "👾〢Ava's room");
}

/// Create path: the spec default template joins its random emoji with 〢.
#[test]
fn create_default_template_joins_emoji_with_separator() {
    let name = resolve_room_name(DEFAULT_NAME_TEMPLATE, &ctx("Ava", 1, 1, 42), "Hangout");
    assert!(
        name.contains("〢Ava's"),
        "default template should join emoji with 〢: {name:?}"
    );
}

/// Rename path: membership-change re-renders carry 〢 and keep the seeded
/// emoji head stable across renames.
#[test]
fn rename_rerender_applies_separator_with_stable_head() {
    let template = "@@random_emoji@@ @@num@@ <<person/people>>";
    let solo = resolve_room_name(template, &ctx("Ava", 1, 1, 9), "old name");
    let busy = resolve_room_name(template, &ctx("Ava", 4, 1, 9), &solo);
    assert!(solo.contains('〢'), "solo rename missing 〢: {solo:?}");
    assert!(busy.contains('〢'), "busy rename missing 〢: {busy:?}");
    assert_eq!(
        solo.split('〢').next(),
        busy.split('〢').next(),
        "rename re-rolled the seeded pick: {solo:?} vs {busy:?}"
    );
}

/// Rename path: an already-separated render is never doubled, including
/// through the rename-coalescer queue.
#[test]
fn rename_does_not_double_existing_separator() {
    let c = ctx("Ava", 1, 1, 7);
    let name = resolve_room_name("👾〢 @@owner@@'s room", &c, "Hangout");
    assert_eq!(name, "👾〢Ava's room");
    assert!(!name.contains("〢〢"), "doubled separator: {name:?}");
    assert!(
        !name.contains("〢 "),
        "separator/by-name space left: {name:?}"
    );

    // The queued rename carries the single-separator name verbatim.
    let mut coalescer = RenameCoalescer::new();
    let current = "👾 Ava's room";
    let desired = resolve_room_name("👾 @@owner@@'s room", &c, current);
    assert!(should_rename(current, &desired));
    coalescer
        .queue_rename(10, &desired)
        .expect("rendered names are queueable");
    assert_eq!(coalescer.pending(10), Some("👾〢Ava's room"));
}

/// Non-emoji names pass through with no separator inserted.
#[test]
fn plain_names_have_no_separator_inserted() {
    let c = ctx("Ava", 1, 3, 7);
    assert_eq!(resolve_room_name("@@owner@@ ##", &c, "Hangout"), "Ava #3");
    assert_eq!(resolve_room_name("plain name", &c, "Hangout"), "plain name");
    assert_eq!(
        resolve_room_name("@@owner@@'s room", &c, "Hangout"),
        "Ava's room"
    );
    for name in [
        resolve_room_name("@@owner@@ ##", &c, "Hangout"),
        resolve_room_name("plain name", &c, "Hangout"),
        resolve_room_name("@@owner@@'s room", &c, "Hangout"),
    ] {
        assert!(!name.contains('〢'), "plain name gained 〢: {name:?}");
    }
}
