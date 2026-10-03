//! Hermetic V9 companion text-channel lifecycle acceptance.
//!
//! Pins the existing `two_bot_core::voice_text_channel` public API to the
//! lifecycle in `docs/voice-rooms.md` V9 and
//! `docs/voice-text-channel-core.md`: visibility matrix, name sanitisation,
//! occupancy transitions and companion deletion. No database, Redis, Discord
//! or staging identity is used.

use two_bot_core::voice_text_channel::{
    occupancy_diff, plan_companion_deletion, sanitise_channel_name, text_channel_plan,
    ChannelOverwrite, OverwriteTarget, TextChannelSettings, VoiceRoomFacts,
    DEFAULT_TEXT_CHANNEL_NAME, MAX_TEXT_CHANNEL_NAME_CHARS,
};

const GUILD: u64 = 7;
const ROOM: u64 = 11;
const CATEGORY: u64 = 13;
const ADMIN: u64 = 9;
const OWNER: u64 = 21;
const MEMBER: u64 = 22;
const THIRD: u64 = 23;
const VIEWER_ROLE: u64 = 42;

fn enabled() -> TextChannelSettings {
    TextChannelSettings {
        enabled: true,
        ..TextChannelSettings::default()
    }
}

fn room<'a>(occupants: &'a [u64], admin_ids: &'a [u64]) -> VoiceRoomFacts<'a> {
    VoiceRoomFacts {
        guild_id: GUILD,
        room_id: ROOM,
        category_id: CATEGORY,
        occupants,
        admin_ids,
        admin_role_ids: &[],
    }
}

fn allow(target: OverwriteTarget) -> ChannelOverwrite {
    ChannelOverwrite {
        target,
        allow_view: true,
        deny_view: false,
    }
}

fn deny(target: OverwriteTarget) -> ChannelOverwrite {
    ChannelOverwrite {
        target,
        allow_view: false,
        deny_view: true,
    }
}

// ---- (1) text_channel_plan visibility matrix ----

#[test]
fn toggle_off_plans_nothing() {
    assert_eq!(
        text_channel_plan(&TextChannelSettings::default(), &room(&[OWNER], &[])),
        None
    );
}

#[test]
fn solo_room_is_visible_only_to_its_occupant() {
    let plan = text_channel_plan(&enabled(), &room(&[OWNER], &[])).unwrap();
    assert_eq!(plan.room_id, ROOM);
    assert_eq!(plan.guild_id, GUILD);
    assert_eq!(plan.category_id, CATEGORY);
    assert_eq!(plan.name, DEFAULT_TEXT_CHANNEL_NAME);
    assert!(!plan.visible_to_all());
    assert_eq!(
        plan.overwrites,
        vec![
            deny(OverwriteTarget::Everyone),
            allow(OverwriteTarget::Member(OWNER)),
        ]
    );
}

#[test]
fn group_room_is_visible_to_occupants_and_admins() {
    let occupants = [OWNER, MEMBER, THIRD];
    let admins = [ADMIN];
    let plan = text_channel_plan(&enabled(), &room(&occupants, &admins)).unwrap();
    assert!(!plan.visible_to_all());
    assert_eq!(
        plan.overwrites,
        vec![
            deny(OverwriteTarget::Everyone),
            allow(OverwriteTarget::Member(ADMIN)),
            allow(OverwriteTarget::Member(OWNER)),
            allow(OverwriteTarget::Member(MEMBER)),
            allow(OverwriteTarget::Member(THIRD)),
        ]
    );
}

#[test]
fn locked_room_adds_the_viewer_role_but_stays_hidden_from_everyone() {
    let settings = TextChannelSettings {
        enabled: true,
        viewer_role_id: Some(VIEWER_ROLE),
        ..TextChannelSettings::default()
    };
    let occupants = [OWNER, MEMBER];
    let admins = [ADMIN];
    let plan = text_channel_plan(&settings, &room(&occupants, &admins)).unwrap();
    assert!(!plan.visible_to_all());
    assert!(plan.overwrites.contains(&deny(OverwriteTarget::Everyone)));
    assert!(plan
        .overwrites
        .contains(&allow(OverwriteTarget::Role(VIEWER_ROLE))));
    assert!(plan
        .overwrites
        .contains(&allow(OverwriteTarget::Member(ADMIN))));
    assert!(plan
        .overwrites
        .contains(&allow(OverwriteTarget::Member(OWNER))));
    assert!(plan
        .overwrites
        .contains(&allow(OverwriteTarget::Member(MEMBER))));
}

#[test]
fn everyone_viewer_role_makes_the_companion_visible_to_all() {
    let settings = TextChannelSettings {
        enabled: true,
        viewer_role_id: Some(GUILD),
        ..TextChannelSettings::default()
    };
    let plan = text_channel_plan(&settings, &room(&[OWNER], &[])).unwrap();
    assert!(plan.visible_to_all());
    assert!(plan.overwrites.contains(&allow(OverwriteTarget::Everyone)));
    assert!(
        !plan
            .overwrites
            .iter()
            .any(|o| matches!(o.target, OverwriteTarget::Role(_))),
        "no separate role overwrite when the viewer role is @everyone"
    );
}

// ---- (2) sanitise_channel_name ----

#[test]
fn empty_names_fall_back_to_the_default() {
    for configured in [None, Some(""), Some("   "), Some("---")] {
        assert_eq!(
            sanitise_channel_name(configured),
            DEFAULT_TEXT_CHANNEL_NAME,
            "configured: {configured:?}"
        );
    }
}

#[test]
fn oversized_names_are_truncated_to_the_limit() {
    let long = "b".repeat(MAX_TEXT_CHANNEL_NAME_CHARS + 40);
    let trimmed = sanitise_channel_name(Some(&long));
    assert_eq!(trimmed.chars().count(), MAX_TEXT_CHANNEL_NAME_CHARS);
    assert!(trimmed.chars().all(|c| c == 'b'));
    let exact = "c".repeat(MAX_TEXT_CHANNEL_NAME_CHARS);
    assert_eq!(sanitise_channel_name(Some(&exact)), exact);
}

#[test]
fn unknown_tokens_with_no_lowercase_mapping_are_dropped() {
    // U+1D400 has no lowercase mapping: it is dropped, not passed through.
    assert_eq!(sanitise_channel_name(Some("Room\u{1d400}")), "room");
    assert_eq!(
        sanitise_channel_name(Some("\u{1d400}")),
        DEFAULT_TEXT_CHANNEL_NAME
    );
}

#[test]
fn ordinary_names_are_lowercased_and_dashed() {
    assert_eq!(sanitise_channel_name(Some("Voice Chat")), "voice-chat");
    assert_eq!(sanitise_channel_name(Some("LOBBY  2")), "lobby-2");
}

// ---- (3) occupancy_diff transitions ----

#[test]
fn join_grants_the_new_occupant() {
    let diff = occupancy_diff(&[OWNER], &[OWNER, MEMBER], &[]);
    assert_eq!(diff.grants, vec![MEMBER]);
    assert!(diff.revokes.is_empty());
}

#[test]
fn leave_revokes_the_departed_occupant() {
    let diff = occupancy_diff(&[OWNER, MEMBER], &[OWNER], &[]);
    assert!(diff.grants.is_empty());
    assert_eq!(diff.revokes, vec![MEMBER]);
}

#[test]
fn unchanged_occupancy_is_a_noop() {
    let diff = occupancy_diff(&[OWNER, MEMBER], &[MEMBER, OWNER], &[]);
    assert!(diff.grants.is_empty());
    assert!(diff.revokes.is_empty());
    let duplicated = occupancy_diff(&[OWNER, OWNER], &[OWNER, OWNER], &[]);
    assert!(duplicated.grants.is_empty());
    assert!(duplicated.revokes.is_empty());
}

#[test]
fn protected_viewers_and_admins_are_never_revoked() {
    let diff = occupancy_diff(
        &[OWNER, VIEWER_ROLE, ADMIN],
        &[OWNER],
        &[VIEWER_ROLE, ADMIN],
    );
    assert!(diff.grants.is_empty());
    assert!(diff.revokes.is_empty());
}

// ---- (4) plan_companion_deletion ----

#[test]
fn deleted_room_maps_to_companion_deletion() {
    let deletion = plan_companion_deletion(ROOM);
    assert_eq!(deletion.room_id, ROOM);
}

// ---- (5) companion deletion never touches the voice room itself ----

#[test]
fn companion_deletion_leaves_the_voice_room_facts_untouched() {
    let occupants = [OWNER, MEMBER];
    let admins = [ADMIN];
    let facts = room(&occupants, &admins);
    let plan_before = text_channel_plan(&enabled(), &facts).unwrap();

    let deletion = plan_companion_deletion(facts.room_id);
    assert_eq!(deletion.room_id, plan_before.room_id);

    // The voice room itself is unchanged: same IDs, same occupants, and the
    // companion can still be planned from those facts.
    assert_eq!(facts.room_id, ROOM);
    assert_eq!(facts.guild_id, GUILD);
    assert_eq!(facts.category_id, CATEGORY);
    assert_eq!(facts.occupants, &[OWNER, MEMBER]);
    assert_eq!(facts.admin_ids, &[ADMIN]);
    let plan_after = text_channel_plan(&enabled(), &facts).unwrap();
    assert_eq!(plan_before, plan_after);
}
