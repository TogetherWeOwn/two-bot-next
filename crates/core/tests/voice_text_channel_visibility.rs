//! V9 companion visibility-matrix acceptance (TOG-12505).
//!
//! Pure acceptance slice against the existing
//! `two_bot_core::voice_text_channel` API (`text_channel_plan`,
//! `visible_to_all`, `occupancy_diff`, `plan_companion_deletion`,
//! `sanitise_channel_name`). No `src` changes; this file only pins the
//! visibility matrix, companion-deletion lifecycle, later-channels-only
//! settings semantics, and occupancy grant/revoke behaviour.

use two_bot_core::voice_text_channel::{
    occupancy_diff, plan_companion_deletion, text_channel_plan, ChannelOverwrite, OverwriteTarget,
    TextChannelSettings, VoiceRoomFacts, DEFAULT_TEXT_CHANNEL_NAME,
};

const GUILD_ID: u64 = 7;
const ROOM_ID: u64 = 11;
const CATEGORY_ID: u64 = 13;
const OCCUPANT_A: u64 = 21;
const OCCUPANT_B: u64 = 22;
const ADMIN: u64 = 9;
const VIEWER_ROLE: u64 = 42;

fn room<'a>(occupants: &'a [u64], admin_ids: &'a [u64]) -> VoiceRoomFacts<'a> {
    VoiceRoomFacts {
        guild_id: GUILD_ID,
        room_id: ROOM_ID,
        category_id: CATEGORY_ID,
        occupants,
        admin_ids,
    }
}

fn enabled(name: Option<&str>, viewer_role_id: Option<u64>) -> TextChannelSettings {
    TextChannelSettings {
        enabled: true,
        configured_name: name.map(str::to_owned),
        viewer_role_id,
    }
}

fn allow(target: OverwriteTarget) -> ChannelOverwrite {
    ChannelOverwrite {
        target,
        allow_view: true,
        deny_view: false,
    }
}

fn deny_everyone() -> ChannelOverwrite {
    ChannelOverwrite {
        target: OverwriteTarget::Everyone,
        allow_view: false,
        deny_view: true,
    }
}

fn allow_everyone() -> ChannelOverwrite {
    ChannelOverwrite {
        target: OverwriteTarget::Everyone,
        allow_view: true,
        deny_view: false,
    }
}

#[test]
fn matrix_occupants_only_denies_everyone_with_default_name() {
    let plan = text_channel_plan(&enabled(None, None), &room(&[OCCUPANT_A], &[])).unwrap();
    assert!(
        !plan.visible_to_all(),
        "occupants-only is not visible to all"
    );
    assert_eq!(plan.name, DEFAULT_TEXT_CHANNEL_NAME);
    assert_eq!(plan.name, "voice-chat");
    assert!(plan.overwrites.contains(&deny_everyone()));
    assert!(plan
        .overwrites
        .contains(&allow(OverwriteTarget::Member(OCCUPANT_A))));
}

#[test]
fn matrix_admins_keep_view_without_occupancy() {
    // The admin is not an occupant but still gets a View overwrite.
    let plan = text_channel_plan(&enabled(None, None), &room(&[OCCUPANT_A], &[ADMIN])).unwrap();
    assert!(!plan.visible_to_all());
    assert!(plan.overwrites.contains(&deny_everyone()));
    assert!(plan
        .overwrites
        .contains(&allow(OverwriteTarget::Member(ADMIN))));
    assert!(plan
        .overwrites
        .contains(&allow(OverwriteTarget::Member(OCCUPANT_A))));
}

#[test]
fn matrix_viewer_role_grants_role_while_everyone_stays_denied() {
    let plan = text_channel_plan(
        &enabled(None, Some(VIEWER_ROLE)),
        &room(&[OCCUPANT_A], &[ADMIN]),
    )
    .unwrap();
    assert!(!plan.visible_to_all(), "a plain role is not @everyone");
    assert!(plan.overwrites.contains(&deny_everyone()));
    assert!(plan
        .overwrites
        .contains(&allow(OverwriteTarget::Role(VIEWER_ROLE))));
    assert!(plan
        .overwrites
        .contains(&allow(OverwriteTarget::Member(ADMIN))));
}

#[test]
fn matrix_everyone_viewer_implies_visible_to_all() {
    let plan = text_channel_plan(
        &enabled(None, Some(GUILD_ID)),
        &room(&[OCCUPANT_A], &[ADMIN]),
    )
    .unwrap();
    assert!(
        plan.visible_to_all(),
        "@everyone viewer implies visible_to_all"
    );
    assert!(plan.overwrites.contains(&allow_everyone()));
    assert!(
        !plan
            .overwrites
            .iter()
            .any(|o| matches!(o.target, OverwriteTarget::Role(_))),
        "no separate role overwrite when the viewer role is @everyone"
    );
}

#[test]
fn custom_name_is_sanitised_while_default_survives() {
    let custom = text_channel_plan(
        &enabled(Some("Lounge Area"), None),
        &room(&[OCCUPANT_A], &[]),
    )
    .unwrap();
    assert_eq!(custom.name, "lounge-area");
    let blank = text_channel_plan(&enabled(Some("   "), None), &room(&[OCCUPANT_A], &[])).unwrap();
    assert_eq!(blank.name, DEFAULT_TEXT_CHANNEL_NAME);
}

#[test]
fn companion_is_deleted_with_its_room() {
    let deletion = plan_companion_deletion(ROOM_ID);
    assert_eq!(deletion.room_id, ROOM_ID);
}

#[test]
fn setting_change_affects_only_later_channels() {
    let facts = room(&[OCCUPANT_A], &[ADMIN]);
    let before = enabled(Some("Lounge"), None);
    let first = text_channel_plan(&before, &facts).unwrap();
    assert_eq!(first.name, "lounge");
    assert_eq!(first.settings, before);

    // The creator later adds a viewer role; the already-planned channel keeps
    // its snapshot while the next channel picks the change up.
    let after = enabled(Some("Other Room"), Some(VIEWER_ROLE));
    let second = text_channel_plan(&after, &facts).unwrap();
    assert_eq!(second.name, "other-room");
    assert_eq!(first.name, "lounge", "earlier plan is untouched");
    assert_eq!(first.settings, before, "earlier snapshot is untouched");
    assert_ne!(first.settings, second.settings);
    assert!(
        second
            .overwrites
            .contains(&allow(OverwriteTarget::Role(VIEWER_ROLE))),
        "later channel sees the new viewer role"
    );
    assert!(
        !first
            .overwrites
            .iter()
            .any(|o| matches!(o.target, OverwriteTarget::Role(_))),
        "earlier channel has no role overwrite"
    );
}

#[test]
fn occupancy_join_grants_leave_removes_role_holders_untouched() {
    // A join grants the newcomer.
    let joined = occupancy_diff(&[OCCUPANT_A], &[OCCUPANT_A, OCCUPANT_B], &[]);
    assert_eq!(joined.grants, vec![OCCUPANT_B]);
    assert!(joined.revokes.is_empty());
    // A leave revokes the leaver.
    let left = occupancy_diff(&[OCCUPANT_A, OCCUPANT_B], &[OCCUPANT_A], &[]);
    assert!(left.grants.is_empty());
    assert_eq!(left.revokes, vec![OCCUPANT_B]);
    // Viewer-role holders and admins who leave are never revoked.
    let protected_left = occupancy_diff(
        &[OCCUPANT_A, VIEWER_ROLE, ADMIN],
        &[OCCUPANT_A],
        &[VIEWER_ROLE, ADMIN],
    );
    assert!(protected_left.grants.is_empty());
    assert!(protected_left.revokes.is_empty(), "role-holders untouched");
}
