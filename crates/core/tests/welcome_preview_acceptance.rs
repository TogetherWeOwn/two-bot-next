//! Acceptance: one admin-gated slash command previews the current welcome.
//!
//! BUILDER CONTRACT: this file is frozen. Build on the acceptance branch,
//! keep every test here verbatim, and make the RED tests pass by changing
//! production code only (the slash registry, router, permission table, help
//! groups and their generated references). Do not edit the fixtures or
//! assertions: they are the spec.
//!
//! The missing behavior: the welcome renderer ships
//! (`adjudicate_welcome` + `picker_components` over legacy/session/anchor
//! modes) but staff cannot see what a newcomer sees without staging a real
//! join. The fix is ONE discoverable admin command:
//!
//! - name `welcome-preview`, gated to Manage Server (bit 32, the server-admin
//!   permission — the same gate as the automation/feed admin commands), with
//!   the shared `ManageServerRequired` refusal for everyone else;
//! - guild-only, always published (routing never refuses for enablement; the
//!   handler gives no-effect guidance for disabled/unconfigured welcomes
//!   after the defer — the router never sees config, so enablement cannot
//!   live there);
//! - ZERO options: the preview renders a fixed documented fixture identity
//!   (pinned in the bot crate's frozen `welcome_preview_acceptance_tests`),
//!   never a targeted member — in particular no user option and no
//!   role/channel management surface;
//! - routed through the shared `route_slash` table to a registered handler,
//!   reserved against custom rows exactly like every other builtin, with a
//!   permission row and a `/help` discovery entry in the same PR.
//!
//! RED at baseline (8 tests): the name is unknown to the registry, the
//! router, the permission table, publish and help, and a custom row can
//! still squat the name.
//!
//! Controls (2 tests): the guild fence and the unknown-name reply are
//! unchanged.
//!
//! The runtime half of this contract (defer-before-reads, live-renderer
//! parity for the fixture, preview banner, zero mutations, gap markers,
//! failure guidance) lives in the bot crate's frozen
//! `welcome_preview_acceptance_tests`. The Builder-owned compatibility
//! updates this command forces on existing pins (counts, catalogs,
//! generated references) are enumerated in the `test-handoff` document on
//! the acceptance card, not changed here: those pins stay green at baseline
//! and fail only once the command exists, which is exactly when the Builder
//! must update them.
//!
//! Offline only: no Discord, no network, no database.

use std::collections::HashSet;
use two_bot_core::command_permissions::command_permission;
use two_bot_core::commands::PERM_MANAGE_GUILD;
use two_bot_core::{
    help_text, CustomCommand, InteractionRouter, RouterGates, RouterRefusal, SlashContext,
    SlashOutcome,
};

const GUILD: u64 = 2222;

fn all_on() -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD),
        scorecard: true,
        automations: true,
        announcements: true,
        moderation: true,
        voice: false,
        voice_assistant: false,
        tickets: true,
        self_roles: true,
        onboarding_picker: true,
        session_picker: true,
    }
}

fn all_off() -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD),
        scorecard: false,
        automations: false,
        announcements: false,
        moderation: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    }
}

fn ctx(name: &str, guild_id: Option<u64>, perms: Option<u64>) -> SlashContext<'_> {
    SlashContext {
        name,
        guild_id,
        actor_permissions: perms,
        custom_row: None,
    }
}

// RED at baseline: no definition named `welcome-preview` publishes. The shape
// is the invocation contract: admin-gated, guild-only, no options — the
// fixture is fixed, so there is nothing to target.
#[test]
fn welcome_preview_publishes_guild_only_with_admin_gate_and_no_options() {
    let set = InteractionRouter::new(all_on())
        .publish_set(&[])
        .expect("the full set assembles");
    let def = set
        .iter()
        .find(|def| def.name == "welcome-preview")
        .expect("one discoverable welcome-preview command publishes");
    assert!(
        !def.description.is_empty(),
        "the published command carries a description"
    );
    assert!(
        !def.dm_permission,
        "welcome-preview is guild-only: welcomes are guild state"
    );
    assert_eq!(
        def.default_member_permissions,
        Some(PERM_MANAGE_GUILD.to_string()),
        "welcome-preview hides from non-admin pickers via Manage Server (32)"
    );
    assert!(
        def.options.is_empty(),
        "zero options: the fixture is fixed, got {:?}",
        def.options.iter().map(|o| &o.name).collect::<Vec<_>>()
    );
}

// RED at baseline: the name is unknown, so nothing routes. Admin bits — the
// exact Manage Server bit and the full mask — reach the handler.
#[test]
fn welcome_preview_routes_handled_for_admins() {
    let router = InteractionRouter::new(all_on());
    for perms in [Some(PERM_MANAGE_GUILD), Some(u64::MAX)] {
        let outcome = router.route_slash(&ctx("welcome-preview", Some(GUILD), perms));
        assert!(
            matches!(outcome, SlashOutcome::Handled { .. }),
            "welcome-preview routes for admin bits {perms:?}, got {outcome:?}"
        );
    }
}

// RED at baseline: the name is unknown, so non-admins get the generic
// unknown reply instead of the actionable Manage Server refusal.
#[test]
fn welcome_preview_refuses_non_admins_with_manage_server() {
    let router = InteractionRouter::new(all_on());
    for perms in [Some(0), None] {
        assert_eq!(
            router.route_slash(&ctx("welcome-preview", Some(GUILD), perms)),
            SlashOutcome::Refuse {
                refusal: RouterRefusal::ManageServerRequired,
            },
            "non-admin bits {perms:?} get the Manage Server refusal"
        );
    }
    assert_eq!(
        RouterRefusal::ManageServerRequired.message(),
        "You need the Manage Server permission to use this command. Ask a server admin to grant it.",
        "the refusal names the permission and the grant path"
    );
}

// RED at baseline: with no route there is no handler to register. The
// assertion names no new variant (it cannot exist yet): whatever the route
// names must be registered in the production composition, with and without
// the member consumer.
#[test]
fn welcome_preview_handler_registers_in_production_wiring() {
    for member_present in [true, false] {
        let router = InteractionRouter::production_router(all_on(), member_present);
        let outcome = router.route_slash(&ctx("welcome-preview", Some(GUILD), Some(u64::MAX)));
        let SlashOutcome::Handled { handler } = outcome else {
            panic!("welcome-preview must route before it can register, got {outcome:?}");
        };
        assert!(
            router.handler_for(&handler).is_some(),
            "the routed welcome-preview handler is registered (member_present={member_present})"
        );
    }
}

// RED at baseline: the name publishes nowhere, so help cannot list it.
// Discovery ships in the same PR as the command.
#[test]
fn welcome_preview_appears_in_help_discovery() {
    let router = InteractionRouter::new(all_on());
    let set = router.publish_set(&[]).expect("the full set assembles");
    let text = help_text(&set);
    assert!(
        text.contains("/welcome-preview"),
        "help lists the welcome-preview command, got:\n{text}"
    );
}

// RED at baseline: `welcome-preview` is not a builtin, so a stored custom
// row under the name publishes and runs today. Like every builtin, the name
// must be reserved: withheld from publish and never dispatched to the
// custom handler, in every gate state.
#[test]
fn welcome_preview_name_is_reserved_against_custom_rows() {
    let row = CustomCommand {
        name: "welcome-preview".to_owned(),
        description: "shadow".to_owned(),
        enabled: true,
    };
    for (gates, label) in [(all_on(), "all-on"), (all_off(), "all-off")] {
        let router = InteractionRouter::new(gates);
        let set = router
            .publish_set(std::slice::from_ref(&row))
            .expect("the set assembles");
        let published: Vec<_> = set
            .iter()
            .filter(|def| def.name == "welcome-preview")
            .collect();
        if gates.automations {
            assert_eq!(
                published.len(),
                1,
                "exactly the builtin publishes while automations are on ({label})"
            );
            assert_ne!(
                published[0].description, "shadow",
                "the custom row never shadows the builtin ({label})"
            );
        } else {
            assert!(
                published.iter().all(|def| def.description != "shadow"),
                "the custom row never publishes under the builtin name ({label})"
            );
        }
        let context = SlashContext {
            custom_row: Some(true),
            ..ctx("welcome-preview", Some(GUILD), Some(u64::MAX))
        };
        let outcome = router.route_slash(&context);
        assert!(
            matches!(outcome, SlashOutcome::Handled { .. }),
            "welcome-preview routes to its builtin handler, never the custom row ({label}), got {outcome:?}"
        );
        assert!(
            !matches!(
                outcome,
                SlashOutcome::Handled {
                    handler: two_bot_core::HandlerId::AutomationCustom,
                }
            ),
            "a stored custom row never executes under the builtin name ({label})"
        );
    }
    // The reserved-name set the publish merge consults must contain the name
    // too: a second custom row under a builtin-adjacent name still cannot
    // collide once the builtin exists.
    let names: HashSet<String> = InteractionRouter::new(all_on())
        .publish_set(&[row])
        .expect("the set assembles")
        .into_iter()
        .map(|def| def.name)
        .collect();
    assert!(
        names.contains("welcome-preview"),
        "the builtin name survives the publish merge"
    );
}

// RED at baseline: no permission row exists, and dispatch expects one for
// every builtin name. Manage Server (32): exactly the server-admin bit,
// denied for zero and missing resolved permissions.
#[test]
fn welcome_preview_permission_row_requires_manage_server() {
    let row = command_permission("welcome-preview").expect("welcome-preview has a permission row");
    assert_eq!(row.required_permissions, PERM_MANAGE_GUILD);
    assert!(row.allows(Some(PERM_MANAGE_GUILD)));
    assert!(row.allows(Some(u64::MAX)));
    assert!(!row.allows(Some(0)));
    assert!(!row.allows(None));
}

// RED at baseline: enablement never unpublishes the command — an admin in
// any gate state gets the routed command and, for a disabled welcome,
// private no-effect guidance instead of a refusal or silence.
#[test]
fn welcome_preview_publishes_with_all_feature_gates_off() {
    let set = InteractionRouter::new(all_off())
        .publish_set(&[])
        .expect("the core-only set assembles");
    assert!(
        set.iter().any(|def| def.name == "welcome-preview"),
        "welcome-preview publishes with every feature gate off, got {:?}",
        set.iter().map(|def| &def.name).collect::<Vec<_>>()
    );
}

// Control: the guild fence binds the new command exactly like the other
// always-on builtins — foreign and missing guilds stay silent.
#[test]
fn welcome_preview_guild_fence_controls() {
    let router = InteractionRouter::new(all_on());
    assert_eq!(
        router.route_slash(&ctx("welcome-preview", Some(9999), Some(u64::MAX))),
        SlashOutcome::Ignore,
        "foreign guilds stay silent"
    );
    assert_eq!(
        router.route_slash(&ctx("welcome-preview", None, None)),
        SlashOutcome::Ignore,
        "missing guilds stay silent"
    );
    let unconfigured = InteractionRouter::new(RouterGates {
        configured_guild: None,
        ..all_on()
    });
    assert_eq!(
        unconfigured.route_slash(&ctx("welcome-preview", Some(GUILD), Some(u64::MAX))),
        SlashOutcome::Ignore,
        "an unconfigured router stays silent"
    );
}

// Control: unrelated unknown names keep the uniform in-guild reply path.
#[test]
fn unrelated_unknown_names_still_reply_control() {
    let router = InteractionRouter::new(all_on());
    assert_eq!(
        router.route_slash(&ctx(
            "definitely-not-a-command",
            Some(GUILD),
            Some(u64::MAX)
        )),
        SlashOutcome::Unknown
    );
}
