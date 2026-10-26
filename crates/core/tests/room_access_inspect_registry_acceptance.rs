//! Protected publication contract for the owner's read-only room-access view.
//! Existing registry/reference suites still own full-set and rendered-doc parity.

use two_bot_core::{InteractionRouter, RouterGates};

fn router(voice: bool) -> InteractionRouter {
    InteractionRouter::new(RouterGates {
        configured_guild: Some(2222),
        scorecard: true,
        automations: true,
        announcements: true,
        moderation: true,
        voice,
        voice_assistant: true,
        tickets: true,
        self_roles: true,
        onboarding_picker: true,
        session_picker: true,
    })
}

#[test]
fn owner_access_inspection_is_published_without_staff_permissions_or_room_selectors() {
    let defs = router(true).publish_set(&[]).expect("valid voice registry");
    let inspect = defs
        .iter()
        .find(|def| def.name == "room-access-inspect")
        .expect("room owners need a discoverable room-access-inspect command");
    assert!(!inspect.dm_permission, "room reads are guild-only");
    assert!(
        inspect.default_member_permissions.is_none()
            || inspect.default_member_permissions.as_deref() == Some("0"),
        "owning a room, not a staff permission, authorizes this read"
    );
    assert!(
        inspect.options.iter().all(|option| {
            option.name == "page" && option.kind == 4 && !option.required.unwrap_or(false)
        }),
        "only an optional page selector is allowed; never select another room/member"
    );
    for option in &inspect.options {
        assert!(
            option.min_value.is_some_and(|minimum| minimum >= 1),
            "page zero must not be advertised"
        );
    }
    assert!(!inspect.description.trim().is_empty());
}

#[test]
fn voice_disabled_keeps_room_access_inspection_unpublished() {
    let defs = router(false).publish_set(&[]).expect("voice-off registry");
    assert!(defs.iter().all(|def| def.name != "room-access-inspect"));
}

#[test]
fn help_discovers_the_read_with_owner_commands_not_creator_access_settings() {
    let defs = router(true).publish_set(&[]).expect("voice registry");
    let help = two_bot_core::help_text(&defs);
    let owner_section = help
        .split("**Voice rooms**")
        .nth(1)
        .expect("owner voice help group")
        .split("**")
        .next()
        .expect("owner voice help body");
    assert!(
        owner_section.contains("/room-access-inspect"),
        "the read must be discoverable alongside /private and /public: {help}"
    );
    assert!(owner_section.contains("/private"));
    assert!(owner_section.contains("/public"));
    assert!(!owner_section.contains("/access ·"));
}
