//! Hermetic V9 acceptance cases against the public core API.

use std::collections::BTreeSet;

use proptest::prelude::*;
use two_bot_core::voice_text_channel::{
    admin_view_roles, occupancy_diff, plan_companion_deletion, sanitise_channel_name,
    text_channel_plan, ChannelOverwrite, OverwriteTarget, TextChannelSettings, VoiceRoomFacts,
    DEFAULT_TEXT_CHANNEL_NAME, MAX_TEXT_CHANNEL_NAME_CHARS, PERMISSION_BIT_ADMINISTRATOR,
    PERMISSION_BIT_MANAGE_CHANNELS,
};

fn settings(enabled: bool) -> TextChannelSettings {
    TextChannelSettings {
        enabled,
        ..TextChannelSettings::default()
    }
}

fn room<'a>(occupants: &'a [u64], admin_ids: &'a [u64]) -> VoiceRoomFacts<'a> {
    VoiceRoomFacts {
        guild_id: 7,
        room_id: 11,
        category_id: 13,
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

#[test]
fn toggle_off_by_default_yields_no_plan() {
    assert_eq!(
        text_channel_plan(&TextChannelSettings::default(), &room(&[1], &[])),
        None
    );
    assert!(!TextChannelSettings::default().enabled);
}

#[test]
fn toggle_on_plans_default_name_category_and_initial_overrides() {
    let plan = text_channel_plan(&settings(true), &room(&[21, 22], &[9])).unwrap();
    assert_eq!(plan.name, DEFAULT_TEXT_CHANNEL_NAME);
    assert_eq!(plan.category_id, 13);
    assert_eq!(plan.room_id, 11);
    assert!(!plan.visible_to_all());
    assert_eq!(
        plan.overwrites,
        vec![
            deny(OverwriteTarget::Everyone),
            allow(OverwriteTarget::Member(9)),
            allow(OverwriteTarget::Member(21)),
            allow(OverwriteTarget::Member(22)),
        ]
    );
}

#[test]
fn viewer_role_gets_an_overwrite() {
    let settings = TextChannelSettings {
        enabled: true,
        viewer_role_id: Some(42),
        ..TextChannelSettings::default()
    };
    let plan = text_channel_plan(&settings, &room(&[21], &[9])).unwrap();
    assert!(!plan.visible_to_all());
    assert!(plan.overwrites.contains(&allow(OverwriteTarget::Role(42))));
    assert!(plan.overwrites.contains(&deny(OverwriteTarget::Everyone)));
}

#[test]
fn viewer_role_of_everyone_makes_the_channel_visible_to_all() {
    let settings = TextChannelSettings {
        enabled: true,
        viewer_role_id: Some(7),
        ..TextChannelSettings::default()
    };
    let plan = text_channel_plan(&settings, &room(&[21], &[])).unwrap();
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

#[test]
fn name_sanitisation_table() {
    let cases = [
        (None, "voice-chat"),
        (Some(""), "voice-chat"),
        (Some("   "), "voice-chat"),
        (Some("Voice Chat"), "voice-chat"),
        (Some("LOBBY  2"), "lobby-2"),
        (Some("-trailing-"), "trailing"),
        (Some("tab\tseparated"), "tab-separated"),
        (Some("Ünïcodé Room"), "ünïcodé-room"),
        // U+1D400 has no lowercase mapping: dropped, not passed through.
        (Some("Room\u{1d400}"), "room"),
        (Some("\u{1d400}"), "voice-chat"),
    ];
    for (configured, expected) in cases {
        assert_eq!(
            sanitise_channel_name(configured),
            expected,
            "configured: {configured:?}"
        );
    }
    let long = "a".repeat(MAX_TEXT_CHANNEL_NAME_CHARS + 40);
    let trimmed = sanitise_channel_name(Some(&long));
    assert_eq!(trimmed.len(), MAX_TEXT_CHANNEL_NAME_CHARS);
    assert!(trimmed.chars().all(|c| c == 'a'));
}

#[test]
fn join_grants_and_leave_revokes() {
    let diff = occupancy_diff(&[21, 22], &[22, 23], &[]);
    assert_eq!(diff.grants, vec![23]);
    assert_eq!(diff.revokes, vec![21]);
}

#[test]
fn protected_ids_are_never_revoked() {
    // Viewer role member 42 and admin 9 both left; neither is revoked.
    let diff = occupancy_diff(&[21, 42, 9], &[21], &[42, 9]);
    assert_eq!(diff.grants, Vec::<u64>::new());
    assert_eq!(diff.revokes, Vec::<u64>::new());
}

#[test]
fn diff_is_idempotent_and_deduplicated() {
    let diff = occupancy_diff(&[21, 21, 0], &[21, 21, 22, 22], &[]);
    assert_eq!(diff.grants, vec![22]);
    assert_eq!(diff.revokes, Vec::<u64>::new());
    let same = occupancy_diff(&[21, 22], &[21, 22], &[9]);
    assert_eq!(same.grants, Vec::<u64>::new());
    assert_eq!(same.revokes, Vec::<u64>::new());
}

#[test]
fn removing_protection_does_not_revoke_by_itself() {
    let first = occupancy_diff(&[21], &[21], &[21]);
    assert!(first.revokes.is_empty());
    let second = occupancy_diff(&[21], &[21], &[]);
    assert!(second.revokes.is_empty());
}

#[test]
fn plan_carries_the_settings_snapshot() {
    let before = TextChannelSettings {
        enabled: true,
        configured_name: Some("Lounge".to_owned()),
        viewer_role_id: Some(42),
    };
    let plan = text_channel_plan(&before, &room(&[21], &[])).unwrap();
    assert_eq!(plan.settings, before);
    assert_eq!(plan.name, "lounge");
    // A later settings change does not alter the already-taken snapshot.
    let changed = TextChannelSettings {
        configured_name: Some("Other".to_owned()),
        ..before.clone()
    };
    assert_ne!(plan.settings, changed);
    assert_eq!(plan.name, "lounge");
}

#[test]
fn deleted_room_maps_to_companion_deletion() {
    let deletion = plan_companion_deletion(11);
    assert_eq!(deletion.room_id, 11);
}

#[test]
fn admin_view_roles_keep_manage_channels_without_admin_or_everyone() {
    // Manage Channels role qualifies; Administrator bypasses overwrites and
    // @everyone is covered by the @everyone entry, so neither gets an entry.
    // Zero IDs are dropped; output is sorted and deduplicated.
    assert_eq!(
        admin_view_roles(
            7,
            &[
                (51, PERMISSION_BIT_MANAGE_CHANNELS),
                (
                    52,
                    PERMISSION_BIT_ADMINISTRATOR | PERMISSION_BIT_MANAGE_CHANNELS
                ),
                (7, PERMISSION_BIT_MANAGE_CHANNELS),
                (53, 0),
                (0, PERMISSION_BIT_MANAGE_CHANNELS),
                (51, PERMISSION_BIT_MANAGE_CHANNELS),
                (50, PERMISSION_BIT_MANAGE_CHANNELS),
            ]
        ),
        vec![50, 51]
    );
    assert_eq!(admin_view_roles(7, &[]), Vec::<u64>::new());
}

fn room_with_roles<'a>(
    occupants: &'a [u64],
    admin_ids: &'a [u64],
    admin_role_ids: &'a [u64],
) -> VoiceRoomFacts<'a> {
    VoiceRoomFacts {
        guild_id: 7,
        room_id: 11,
        category_id: 13,
        occupants,
        admin_ids,
        admin_role_ids,
    }
}

#[test]
fn admin_roles_become_role_allows() {
    // A newly promoted admin role covers the room without any per-member
    // update: the Role allow is in the initial overwrite set.
    let plan = text_channel_plan(&settings(true), &room_with_roles(&[21], &[], &[51, 52])).unwrap();
    assert_eq!(
        plan.overwrites,
        vec![
            deny(OverwriteTarget::Everyone),
            allow(OverwriteTarget::Role(51)),
            allow(OverwriteTarget::Role(52)),
            allow(OverwriteTarget::Member(21)),
        ]
    );
}

#[test]
fn admin_role_matching_viewer_role_is_emitted_once() {
    let settings = TextChannelSettings {
        enabled: true,
        viewer_role_id: Some(42),
        ..TextChannelSettings::default()
    };
    let plan = text_channel_plan(&settings, &room_with_roles(&[21], &[], &[42])).unwrap();
    assert_eq!(
        plan.overwrites
            .iter()
            .filter(|o| o.target == OverwriteTarget::Role(42))
            .count(),
        1,
        "viewer role and admin role collapse to one Role allow"
    );
}

#[test]
fn admin_role_matching_everyone_and_zero_are_dropped() {
    let plan = text_channel_plan(&settings(true), &room_with_roles(&[21], &[], &[7, 0])).unwrap();
    assert!(
        !plan
            .overwrites
            .iter()
            .any(|o| matches!(o.target, OverwriteTarget::Role(_))),
        "@everyone is covered by the @everyone entry; zero is never emitted"
    );
}

fn apply(before: &[u64], grants: &[u64], revokes: &[u64]) -> BTreeSet<u64> {
    let mut current: BTreeSet<u64> = before.iter().copied().filter(|id| *id != 0).collect();
    for id in revokes {
        current.remove(id);
    }
    current.extend(grants.iter().copied());
    current
}

fn live(ids: &[u64]) -> BTreeSet<u64> {
    ids.iter().copied().filter(|id| *id != 0).collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn property_diff_applies_cleanly_to_after(
        before in proptest::collection::vec(0u64..8, 0..8),
        after in proptest::collection::vec(0u64..8, 0..8),
    ) {
        let diff = occupancy_diff(&before, &after, &[]);
        prop_assert_eq!(apply(&before, &diff.grants, &diff.revokes), live(&after));
    }

    #[test]
    fn property_protected_ids_are_never_revoked(
        before in proptest::collection::vec(0u64..8, 0..8),
        after in proptest::collection::vec(0u64..8, 0..8),
        protected in proptest::collection::vec(0u64..8, 0..4),
    ) {
        let diff = occupancy_diff(&before, &after, &protected);
        let protected_set = live(&protected);
        prop_assert!(diff.revokes.iter().all(|id| !protected_set.contains(id)));
        // Grants and revokes stay disjoint, sorted and deduplicated.
        prop_assert!(diff.grants.windows(2).all(|w| w[0] < w[1]));
        prop_assert!(diff.revokes.windows(2).all(|w| w[0] < w[1]));
        let grants: BTreeSet<u64> = diff.grants.iter().copied().collect();
        let revokes: BTreeSet<u64> = diff.revokes.iter().copied().collect();
        prop_assert!(grants.is_disjoint(&revokes));
        prop_assert!(grants.is_subset(&live(&after)));
        prop_assert!(revokes.is_subset(&live(&before)));
        // Applying still reaches `after` plus any protected members who left.
        let mut expected = live(&after);
        expected.extend(live(&before).intersection(&protected_set).copied());
        prop_assert_eq!(apply(&before, &diff.grants, &diff.revokes), expected);
    }

    #[test]
    fn property_name_sanitisation_stays_within_channel_rules(
        raw in proptest::collection::vec(any::<char>(), 0..200),
    ) {
        let name: String = raw.into_iter().collect();
        let clean = sanitise_channel_name(Some(&name));
        prop_assert!(clean.chars().count() <= MAX_TEXT_CHANNEL_NAME_CHARS);
        prop_assert!(!clean.chars().any(|c| c.is_whitespace() || c.is_uppercase()));
        prop_assert!(!clean.is_empty());
        let again = sanitise_channel_name(Some(&clean));
        prop_assert_eq!(clean, again, "sanitising is fixed-point");
    }
}
