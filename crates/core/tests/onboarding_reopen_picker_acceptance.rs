//! Acceptance: one self-service slash command reopens the existing pickers.
//!
//! BUILDER CONTRACT: this file is frozen. Build on the acceptance branch,
//! keep every test here verbatim, and make the RED tests pass by changing
//! production code only (the slash registry, router, permission table, help
//! groups and their generated references). Do not edit the fixtures or
//! assertions: they are the spec.
//!
//! The missing behavior: an existing member whose welcome is lost — or whose
//! picker reply says "open the menu and try again" — has no way to get a
//! fresh picker without rejoining or asking staff. The fix is ONE
//! discoverable self-service command:
//!
//! - name `reopen`, open to every member (no permission gate, like `/privacy`);
//! - one required string option `kind` with exactly the choices `games` and
//!   `session` (no other-member target, no other options);
//! - guild-only, always published (routing never refuses for enablement; the
//!   handler gives no-effect guidance for disabled/unavailable kinds after
//!   the defer, where the kind option is visible — the router never sees
//!   options, so kind-level enablement cannot live there);
//! - routed through the shared `route_slash` table to a registered handler,
//!   reserved against custom rows exactly like every other builtin, with a
//!   permission row and a `/help` discovery entry in the same PR.
//!
//! RED at baseline (7 tests): the name is unknown to the registry, the
//! router, the permission table, publish and help, and a custom row can
//! still squat the name.
//!
//! Controls (2 tests): the guild fence and the unknown-name reply are
//! unchanged.
//!
//! The runtime half of this contract (defer-before-reads, fresh menus, zero
//! mutations, failure guidance, reuse of the existing selection path) lives
//! in the bot crate's frozen `onboarding_reopen_picker_acceptance_tests`.
//! The Builder-owned compatibility updates this command forces on existing
//! pins (counts, catalogs, generated references) are enumerated in the
//! `test-handoff` document on the acceptance card, not changed here: those
//! pins stay green at baseline and fail only once the command exists, which
//! is exactly when the Builder must update them.
//!
//! Offline only: no Discord, no network, no database.

use std::collections::HashSet;
use two_bot_core::command_permissions::command_permission;
use two_bot_core::commands::CommandOptionType;
use two_bot_core::{
    help_text, CustomCommand, InteractionRouter, RouterGates, SlashContext, SlashOutcome,
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

// RED at baseline: no definition named `reopen` publishes, so there is no
// option shape to check. The shape is the invocation contract: one required
// `kind` choice (games/session), nothing else — in particular no
// other-member target and no role-management surface.
#[test]
fn reopen_publishes_with_a_kind_option_and_no_target() {
    let set = InteractionRouter::new(all_on())
        .publish_set(&[])
        .expect("the full set assembles");
    let def = set
        .iter()
        .find(|def| def.name == "reopen")
        .expect("one discoverable reopen command publishes");
    assert!(
        !def.description.is_empty(),
        "the published command carries a description"
    );
    assert!(
        !def.dm_permission,
        "reopen is guild-only: pickers are guild menus"
    );
    assert_eq!(
        def.default_member_permissions, None,
        "reopen is open to every member: Discord picker hiding is not authorization"
    );
    assert_eq!(
        def.options.len(),
        1,
        "exactly one option: the picker kind, got {:?}",
        def.options.iter().map(|o| &o.name).collect::<Vec<_>>()
    );
    let kind = &def.options[0];
    assert_eq!(kind.name, "kind");
    assert_eq!(
        kind.kind,
        CommandOptionType::String.as_u8(),
        "kind is a string choice, not a user/channel/role picker"
    );
    assert_eq!(kind.required, Some(true), "kind is required");
    let values: HashSet<_> = kind.choices.iter().map(|c| c.value.as_str()).collect();
    assert_eq!(
        values,
        HashSet::from(["games", "session"]),
        "exactly the two existing picker kinds, got {values:?}"
    );
    for choice in &kind.choices {
        assert!(
            !choice.name.is_empty(),
            "every kind choice carries a display name"
        );
    }
}

// RED at baseline: routing refuses nothing for enablement — the handler
// gives kind-level no-effect guidance after the defer, where the kind is
// visible. A member with zero permission bits is the point of the command.
#[test]
fn reopen_routes_handled_for_every_member() {
    let router = InteractionRouter::new(all_on());
    for perms in [Some(0), None, Some(u64::MAX)] {
        let outcome = router.route_slash(&ctx("reopen", Some(GUILD), perms));
        assert!(
            matches!(outcome, SlashOutcome::Handled { .. }),
            "reopen routes for permission bits {perms:?}, got {outcome:?}"
        );
    }
}

// RED at baseline: with no route there is no handler to register. The
// assertion names no new variant (it cannot exist yet): whatever the route
// names must be registered in the production composition, with and without
// the member consumer.
#[test]
fn reopen_handler_registers_in_production_wiring() {
    for member_present in [true, false] {
        let router = InteractionRouter::production_router(all_on(), member_present);
        let outcome = router.route_slash(&ctx("reopen", Some(GUILD), Some(0)));
        let SlashOutcome::Handled { handler } = outcome else {
            panic!("reopen must route before it can register, got {outcome:?}");
        };
        assert!(
            router.handler_for(&handler).is_some(),
            "the routed reopen handler is registered (member_present={member_present})"
        );
    }
}

// RED at baseline: the name publishes nowhere, so help cannot list it.
// Discovery ships in the same PR as the command.
#[test]
fn reopen_appears_in_help_discovery() {
    let router = InteractionRouter::new(all_on());
    let set = router.publish_set(&[]).expect("the full set assembles");
    let text = help_text(&set);
    assert!(
        text.contains("/reopen"),
        "help lists the reopen command, got:\n{text}"
    );
}

// RED at baseline: `reopen` is not a builtin, so a stored custom row under
// the name publishes and runs today. Like every builtin, the name must be
// reserved: withheld from publish and never dispatched to the custom
// handler, in every gate state.
#[test]
fn reopen_name_is_reserved_against_custom_rows() {
    let row = CustomCommand {
        name: "reopen".to_owned(),
        description: "shadow".to_owned(),
        enabled: true,
    };
    for (gates, label) in [(all_on(), "all-on"), (all_off(), "all-off")] {
        let router = InteractionRouter::new(gates);
        let set = router
            .publish_set(std::slice::from_ref(&row))
            .expect("the set assembles");
        let published: Vec<_> = set.iter().filter(|def| def.name == "reopen").collect();
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
            ..ctx("reopen", Some(GUILD), Some(0))
        };
        let outcome = router.route_slash(&context);
        assert!(
            matches!(outcome, SlashOutcome::Handled { .. }),
            "reopen routes to its builtin handler, never the custom row ({label}), got {outcome:?}"
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
}

// RED at baseline: no permission row exists, and dispatch expects one for
// every builtin name. Zero required bits: every member may reopen their
// own pickers, including one whose resolved permissions are missing.
#[test]
fn reopen_permission_row_opens_to_everyone() {
    let row = command_permission("reopen").expect("reopen has a permission row");
    assert_eq!(row.required_permissions, 0);
    assert!(row.allows(Some(0)));
    assert!(row.allows(None));
    assert!(row.allows(Some(u64::MAX)));
}

// RED at baseline: enablement never unpublishes the command — a member in
// any gate state gets the routed command and, for an unavailable kind,
// private no-effect guidance instead of a refusal or silence.
#[test]
fn reopen_publishes_with_all_feature_gates_off() {
    let set = InteractionRouter::new(all_off())
        .publish_set(&[])
        .expect("the core-only set assembles");
    assert!(
        set.iter().any(|def| def.name == "reopen"),
        "reopen publishes with every feature gate off, got {:?}",
        set.iter().map(|def| &def.name).collect::<Vec<_>>()
    );
}

// Control: the guild fence binds the new command exactly like the other
// always-on builtins — foreign and missing guilds stay silent.
#[test]
fn reopen_guild_fence_controls() {
    let router = InteractionRouter::new(all_on());
    assert_eq!(
        router.route_slash(&ctx("reopen", Some(9999), Some(u64::MAX))),
        SlashOutcome::Ignore,
        "foreign guilds stay silent"
    );
    assert_eq!(
        router.route_slash(&ctx("reopen", None, None)),
        SlashOutcome::Ignore,
        "missing guilds stay silent"
    );
    let unconfigured = InteractionRouter::new(RouterGates {
        configured_guild: None,
        ..all_on()
    });
    assert_eq!(
        unconfigured.route_slash(&ctx("reopen", Some(GUILD), Some(u64::MAX))),
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
