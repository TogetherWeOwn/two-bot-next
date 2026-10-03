//! Independent parity-document, publication and runtime authorization contract.

use std::collections::HashSet;
use two_bot_core::command_permissions::{
    command_permission, CommandSurface, PolicyHook, COMMAND_PERMISSIONS,
};
use two_bot_core::{
    CustomCommand, InteractionRouter, ModerationAction, RouterGates, SlashContext, SlashOutcome,
};

const GUILD: u64 = 2222;

fn all_on() -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD),
        scorecard: true,
        automations: true,
        announcements: true,
        moderation: true,
        voice: true,
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
    // (parity row 31 — legacy has no help command) and the Next-only voice
    // vote-kick (parity row 32 — legacy has no vote-kick slash).
    assert_eq!(COMMAND_PERMISSIONS.len(), 32);
    assert_eq!(
        COMMAND_PERMISSIONS
            .iter()
            .map(|row| row.command)
            .collect::<HashSet<_>>()
            .len(),
        32
    );
    let help = command_permission("help").expect("help has a permission row");
    assert_eq!(help.required_permissions, 0);
    assert_eq!(help.surface, CommandSurface::BuiltinSlash);
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
    assert_eq!(builtins.len(), 29);
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
