//! Protected acceptance contract: staff inspect a synthetic goodbye, never deliver it.
//! Builders keep this file unchanged. Existing count/golden/reference pins are
//! compatibility updates, not part of this protected oracle.

use two_bot_core::command_permissions::command_permission;
use two_bot_core::commands::PERM_MANAGE_GUILD;
use two_bot_core::{
    help_text, CustomCommand, HandlerId, InteractionRouter, RouterGates, RouterRefusal,
    SlashContext, SlashOutcome,
};

fn gates(enabled: bool) -> RouterGates {
    RouterGates {
        configured_guild: Some(22),
        scorecard: enabled,
        automations: enabled,
        announcements: enabled,
        moderation: enabled,
        voice: false,
        voice_assistant: false,
        tickets: enabled,
        self_roles: enabled,
        onboarding_picker: enabled,
        session_picker: enabled,
    }
}

fn context(permissions: Option<u64>) -> SlashContext<'static> {
    SlashContext {
        name: "goodbye-preview",
        guild_id: Some(22),
        actor_permissions: permissions,
        custom_row: None,
    }
}

// Missing publication, accidental targeting options or a weaker picker gate.
#[test]
fn goodbye_preview_is_one_guild_only_optionless_staff_command_even_when_features_are_off() {
    for enabled in [false, true] {
        let set = InteractionRouter::new(gates(enabled))
            .publish_set(&[])
            .unwrap();
        let rows: Vec<_> = set
            .iter()
            .filter(|row| row.name == "goodbye-preview")
            .collect();
        assert_eq!(
            rows.len(),
            1,
            "publish the preview independently of delivery enablement"
        );
        let row = rows[0];
        assert!(!row.description.is_empty());
        assert!(!row.dm_permission);
        assert!(
            row.options.is_empty(),
            "no real member or delivery target input"
        );
        assert_eq!(
            row.default_member_permissions,
            Some(PERM_MANAGE_GUILD.to_string())
        );
    }
}

// A publication-only change must not leave a dead runtime route.
#[test]
fn goodbye_preview_routes_to_a_registered_handler_in_both_production_compositions() {
    for enabled in [false, true] {
        for member_present in [false, true] {
            let router = InteractionRouter::production_router(gates(enabled), member_present);
            for permissions in [PERM_MANAGE_GUILD, u64::MAX] {
                let outcome = router.route_slash(&context(Some(permissions)));
                let SlashOutcome::Handled { handler } = outcome else {
                    panic!("staff preview must route independently of mode: {outcome:?}");
                };
                assert!(router.handler_for(&handler).is_some());
                assert_ne!(handler, HandlerId::AutomationCustom);
            }
        }
    }
}

// Discord defaults alone are not authorization; use welcome-preview's exact gate.
#[test]
fn goodbye_preview_permission_registry_and_runtime_refusal_match_welcome_preview() {
    let row = command_permission("goodbye-preview").expect("preview permission row");
    let welcome = command_permission("welcome-preview").unwrap();
    assert_eq!(row.required_permissions, PERM_MANAGE_GUILD);
    assert_eq!(row.required_permissions, welcome.required_permissions);
    for permissions in [None, Some(0), Some(8), Some(16), Some(32), Some(u64::MAX)] {
        assert_eq!(row.allows(permissions), welcome.allows(permissions));
        let router = InteractionRouter::new(gates(true));
        if row.allows(permissions) {
            assert!(matches!(
                router.route_slash(&context(permissions)),
                SlashOutcome::Handled { .. }
            ));
        } else {
            assert_eq!(
                router.route_slash(&context(permissions)),
                SlashOutcome::Refuse {
                    refusal: RouterRefusal::ManageServerRequired,
                }
            );
        }
    }
}

// A custom command must not squat the preview name or receive its invocations.
#[test]
fn goodbye_preview_is_reserved_against_custom_rows() {
    let custom = CustomCommand {
        name: "goodbye-preview".into(),
        description: "custom shadow sentinel".into(),
        enabled: true,
    };
    for enabled in [false, true] {
        let router = InteractionRouter::new(gates(enabled));
        let set = router.publish_set(std::slice::from_ref(&custom)).unwrap();
        let rows: Vec<_> = set.iter().filter(|row| row.name == custom.name).collect();
        assert_eq!(rows.len(), 1);
        assert_ne!(rows[0].description, custom.description);
        let outcome = router.route_slash(&SlashContext {
            custom_row: Some(true),
            ..context(Some(PERM_MANAGE_GUILD))
        });
        assert!(
            matches!(outcome, SlashOutcome::Handled { handler } if handler != HandlerId::AutomationCustom)
        );
    }
}

// Merely falling back into the Custom group is not staff discovery.
#[test]
fn goodbye_preview_help_explains_the_manage_server_boundary() {
    let set = InteractionRouter::new(gates(false))
        .publish_set(&[])
        .unwrap();
    let text = help_text(&set);
    let line = text
        .lines()
        .position(|line| line.contains("/goodbye-preview"))
        .expect("help discovers goodbye-preview");
    let heading = text
        .lines()
        .take(line + 1)
        .filter(|line| line.starts_with("**"))
        .last()
        .unwrap_or_default();
    assert!(
        heading.contains("Manage Server"),
        "preview needs a staff help group: {heading}"
    );
    let definition = set
        .iter()
        .find(|row| row.name == "goodbye-preview")
        .unwrap();
    let detail = definition.help_detail_text();
    assert!(detail.contains("/goodbye-preview"));
    assert!(detail.contains("Manage Server"));
}

// Controls: a new preview must not weaken the shared guild or unknown-name fences.
#[test]
fn goodbye_preview_guild_and_unknown_controls() {
    let router = InteractionRouter::new(gates(true));
    for guild_id in [None, Some(23)] {
        assert_eq!(
            router.route_slash(&SlashContext {
                guild_id,
                ..context(Some(32))
            }),
            SlashOutcome::Ignore
        );
    }
    assert_eq!(
        router.route_slash(&SlashContext {
            name: "not-a-built-in-preview",
            ..context(Some(32))
        }),
        SlashOutcome::Unknown
    );
}
