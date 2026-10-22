//! Independent parity-document, publication and runtime authorization contract.

use std::collections::HashSet;
use two_bot_core::command_permissions::{
    command_permission, CommandSurface, PolicyHook, COMMAND_PERMISSIONS,
};
use two_bot_core::{
    CustomCommand, HandlerId, InteractionRouter, ModerationAction, RouterGates, SlashContext,
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

#[test]
fn all_30_rows_equal_parity_section1() {
    let section = include_str!("../../../docs/parity.md")
        .split("## 1.")
        .nth(1)
        .expect("parity section 1")
        .split("## 2.")
        .next()
        .unwrap();
    let mut seen = HashSet::new();
    for line in section.lines().filter(|line| line.starts_with("| ")) {
        let columns: Vec<_> = line.split('|').map(str::trim).collect();
        let Ok(number) = columns[1].parse::<u8>() else {
            continue;
        };
        assert!(seen.insert(number), "duplicate parity row {number}");
        let row = COMMAND_PERMISSIONS
            .iter()
            .find(|row| row.parity_row == number)
            .expect("every documented row must be represented");
        let legacy_name = columns[2].split('`').nth(1).unwrap();
        let name = if number == 25 {
            assert_eq!(legacy_name, "/attendance");
            "rsvp-attendance"
        } else {
            legacy_name.trim_start_matches(['/', '!'])
        };
        assert_eq!(row.command, name, "parity row {number}");
        // Literal Discord bits, independent of the table's permission constants.
        let required = if columns[4].starts_with("everyone") {
            0
        } else {
            match columns[4].split('`').nth(1).unwrap() {
                "BanMembers" => 1 << 2,
                "KickMembers" => 1 << 1,
                "ModerateMembers" => 1 << 40,
                "ManageMessages" => 1 << 13,
                "ManageChannels" => 1 << 4,
                "ManageEvents" => 1 << 33,
                "ManageGuild" => 1 << 5,
                other => panic!("unmapped permission {other} on row {number}"),
            }
        };
        assert_eq!(row.required_permissions, required, "parity row {number}");
        let surface = if columns[5].contains("**DROP**") {
            CommandSurface::Dropped
        } else if legacy_name.starts_with('!') {
            CommandSurface::Prefix
        } else if legacy_name == "/<custom>" {
            CommandSurface::DynamicSlash
        } else {
            CommandSurface::BuiltinSlash
        };
        assert_eq!(row.surface, surface, "parity row {number}");
        let policy_hook = match number {
            3..=7 => Some(PolicyHook::MemberModeration),
            13 => Some(PolicyHook::ConfiguredPrimaryActor),
            22 => Some(PolicyHook::AutomationsEnabled),
            23 => Some(PolicyHook::TextCommandsEnabled),
            _ => None,
        };
        assert_eq!(row.policy_hook, policy_hook, "parity row {number}");
    }
    assert_eq!(seen, (1..=30).collect::<HashSet<_>>());
    // The 30 legacy parity rows plus the Next-only `/help` discovery command
    // (parity row 31 — legacy has no help command), Next-only `/privacy`
    // member self-serve (parity row 32), and Next-only `/birthday` (row 33).
    // These Next-only rows do not appear in the legacy §1 table above;
    // the registry-equality test below covers every published builtin instead.
    assert_eq!(COMMAND_PERMISSIONS.len(), 33);
    assert_eq!(
        COMMAND_PERMISSIONS
            .iter()
            .map(|row| row.command)
            .collect::<HashSet<_>>()
            .len(),
        33
    );
    let help = command_permission("help").expect("help has a permission row");
    assert_eq!(help.required_permissions, 0);
    assert_eq!(help.surface, CommandSurface::BuiltinSlash);
    let birthday = command_permission("birthday").expect("birthday has a permission row");
    assert_eq!(
        birthday.required_permissions, 0,
        "own birthday is open by default"
    );
    assert_eq!(birthday.surface, CommandSurface::BuiltinSlash);
    assert_eq!(birthday.parity_row, 33);
}

#[test]
fn permission_table_equals_the_complete_published_registry() {
    let router = InteractionRouter::new(all_on());
    let custom = CustomCommand {
        name: "faq".to_owned(),
        description: "Custom command".to_owned(),
        enabled: true,
    };
    let published = router.publish_set(&[custom]).unwrap();
    let builtins: Vec<_> = COMMAND_PERMISSIONS
        .iter()
        .filter(|row| row.surface == CommandSurface::BuiltinSlash)
        .collect();
    assert_eq!(builtins.len(), 30);
    assert_eq!(published.len(), builtins.len() + 1);
    for row in builtins {
        let definition = published
            .iter()
            .find(|def| def.name == row.command)
            .unwrap();
        let expected =
            (row.required_permissions != 0).then(|| row.required_permissions.to_string());
        assert_eq!(
            definition.default_member_permissions, expected,
            "{}",
            row.command
        );
    }
    for definition in &published {
        if definition.name == "faq" {
            assert_eq!(definition.default_member_permissions, None);
        } else {
            assert!(command_permission(&definition.name).is_some());
        }
    }
    for row in COMMAND_PERMISSIONS
        .iter()
        .filter(|row| row.surface != CommandSurface::BuiltinSlash)
    {
        assert!(command_permission(row.command).is_none());
        assert!(published.iter().all(|def| def.name != row.command));
    }
}

#[test]
fn every_retained_command_rechecks_resolved_permissions_before_returning_a_handler() {
    let router = InteractionRouter::new(all_on());
    for row in COMMAND_PERMISSIONS
        .iter()
        .filter(|row| row.surface == CommandSurface::BuiltinSlash)
    {
        let ctx = |actor_permissions| SlashContext {
            name: row.command,
            guild_id: Some(GUILD),
            actor_permissions,
            // Even a same-named enabled custom row cannot bypass a builtin.
            custom_row: Some(true),
        };
        for permissions in [None, Some(0), Some(1 << 3), Some(!row.required_permissions)] {
            let outcome = router.route_slash(&ctx(permissions));
            if row.required_permissions == 0 {
                assert!(
                    matches!(outcome, SlashOutcome::Handled { .. }),
                    "{}: {outcome:?}",
                    row.command
                );
            } else {
                assert!(
                    matches!(outcome, SlashOutcome::Refuse { .. }),
                    "{}: {outcome:?}",
                    row.command
                );
            }
        }
        for permissions in [
            row.required_permissions,
            row.required_permissions | (1 << 60),
            u64::MAX,
        ] {
            assert!(
                matches!(
                    router.route_slash(&ctx(Some(permissions))),
                    SlashOutcome::Handled { .. }
                ),
                "{}",
                row.command
            );
        }
    }
}

#[test]
fn every_routed_builtin_name_has_a_permission_row() {
    let router = InteractionRouter::new(all_on());
    let published = router.publish_set(&[]).unwrap();
    let mut universe: HashSet<&str> = published
        .iter()
        .map(|definition| definition.name.as_str())
        .collect();
    universe.extend(
        COMMAND_PERMISSIONS
            .iter()
            .filter(|row| row.surface == CommandSurface::BuiltinSlash)
            .map(|row| row.command),
    );
    universe.extend(
        ModerationAction::ALL
            .iter()
            .map(|action| action.command_name()),
    );
    // The router safety argument, pinned: `permission_allowed` unwraps the
    // permission row for every name `route_builtin` matches, so each one must
    // resolve. Probing through `route_slash` with no custom row keeps an
    // unrouted name visible as `Unknown` instead of the custom fallthrough.
    let mut routed = 0;
    for name in universe {
        match router.route_slash(&SlashContext {
            name,
            guild_id: Some(GUILD),
            actor_permissions: Some(u64::MAX),
            custom_row: None,
        }) {
            SlashOutcome::Handled { handler } => {
                assert_ne!(
                    handler,
                    HandlerId::AutomationCustom,
                    "{name} fell through to the custom path"
                );
                assert!(
                    command_permission(name).is_some(),
                    "{name} routes as a builtin but has no permission row"
                );
                routed += 1;
            }
            other => panic!("{name} is published or retained but does not route: {other:?}"),
        }
    }
    assert_eq!(routed, 30, "every retained builtin routes");
}

#[test]
fn birthday_has_its_own_handler_and_announcement_gate() {
    let context = SlashContext {
        name: "birthday",
        guild_id: Some(GUILD),
        actor_permissions: Some(0),
        custom_row: Some(true),
    };
    match InteractionRouter::new(all_on()).route_slash(&context) {
        SlashOutcome::Handled { handler } => {
            // Keep this acceptance fixture compilable before the new variant
            // exists while still pinning the selected HandlerId exactly.
            assert_eq!(format!("{handler:?}"), "Birthday");
        }
        other => panic!("ordinary members route to the birthday builtin: {other:?}"),
    }
    let off = RouterGates {
        announcements: false,
        ..all_on()
    };
    let router = InteractionRouter::new(off);
    assert_eq!(
        router.route_slash(&context),
        SlashOutcome::Refuse {
            refusal: two_bot_core::router::RouterRefusal::AnnouncementsDisabled
        },
        "the announcement gate fences the birthday handler"
    );
    assert!(router
        .publish_set(&[])
        .expect("publish set")
        .iter()
        .all(|def| def.name != "birthday"));
}

#[test]
fn member_moderation_policy_hooks_are_consumed_by_the_existing_policy() {
    for action in ModerationAction::ALL {
        let row = command_permission(action.command_name()).unwrap();
        assert_eq!(action.required_permission(), row.required_permissions);
        assert_eq!(
            action.targets_member(),
            matches!(
                action,
                ModerationAction::Ban
                    | ModerationAction::TempBan
                    | ModerationAction::Kick
                    | ModerationAction::Timeout
                    | ModerationAction::Warn
            )
        );
        assert_eq!(
            action.targets_member(),
            row.policy_hook == Some(PolicyHook::MemberModeration)
        );
    }
}
