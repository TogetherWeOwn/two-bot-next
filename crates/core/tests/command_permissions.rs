//! Independent parity-document, publication and runtime authorization contract.

use std::collections::HashSet;
use two_bot_core::command_permissions::{
    command_permission, CommandSurface, PolicyHook, COMMAND_PERMISSIONS,
};
use two_bot_core::commands::PERM_MANAGE_GUILD;
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
fn documented_legacy_rows_equal_parity_section1() {
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
    // (parity row 31 — legacy has no help command), `/privacy` member
    // self-serve (row 32), management-only `/command-inspect` (row 33),
    // `/reopen` picker recovery (row 34), `/purge-preview` (35),
    // `/channel-inspect` (36), `/giveaway` timed entry (row 37),
    // `/lfg-list` current-channel list (row 38), `/feed-inspect` relay detail
    // (row 39), plus `/lfg-edit` sharing row 26, and pause/resume
    // `/command-disable` and `/command-enable` (rows 40 and 41),
    // `/welcome-preview` (42), `/game-mix` (43), feed pause/resume (44/45),
    // `/tempban-list` saved expiry inspection (46), and `/command-clone` (47).
    // No Next-only row appears in the legacy §1 table above;
    // the registry-equality test below covers every published builtin instead.
    // 48 entries: 47 contiguous parity rows plus the `/lfg-edit` companion
    // sharing LFG row 26.
    assert_eq!(COMMAND_PERMISSIONS.len(), 48);
    assert_eq!(
        COMMAND_PERMISSIONS
            .iter()
            .map(|row| row.command)
            .collect::<HashSet<_>>()
            .len(),
        48
    );
    assert_eq!(
        COMMAND_PERMISSIONS
            .iter()
            .map(|row| row.parity_row)
            .collect::<HashSet<_>>(),
        (1..=47).collect::<HashSet<_>>(),
        "parity rows are contiguous 1..=47"
    );
    for (name, parity_row, required_permissions) in
        [("command-inspect", 33, 1 << 5), ("lfg-list", 38, 1 << 33)]
    {
        let row = command_permission(name).expect("Next-only command has a permission row");
        assert_eq!(row.parity_row, parity_row, "{name}");
        assert_eq!(row.required_permissions, required_permissions, "{name}");
        assert_eq!(row.surface, CommandSurface::BuiltinSlash, "{name}");
    }
    let help = command_permission("help").expect("help has a permission row");
    assert_eq!(help.required_permissions, 0);
    assert_eq!(help.surface, CommandSurface::BuiltinSlash);
    let inspect = command_permission("command-inspect").expect("inspection has a permission row");
    assert_eq!(inspect.parity_row, 33);
    assert_eq!(inspect.required_permissions, 32, "ManageGuild bit");
    assert_eq!(inspect.surface, CommandSurface::BuiltinSlash);
    assert_eq!(inspect.policy_hook, None);
    let reopen = command_permission("reopen").expect("reopen has a permission row");
    assert_eq!(reopen.parity_row, 34);
    assert_eq!(reopen.required_permissions, 0);
    assert_eq!(reopen.surface, CommandSurface::BuiltinSlash);
    assert_eq!(reopen.policy_hook, None);
    let giveaway = command_permission("giveaway").expect("giveaway has a permission row");
    assert_eq!(giveaway.parity_row, 37);
    assert_eq!(giveaway.required_permissions, 32, "ManageGuild bit");
    assert_eq!(giveaway.surface, CommandSurface::BuiltinSlash);
    assert_eq!(giveaway.policy_hook, None);
    let feed_inspect =
        command_permission("feed-inspect").expect("feed-inspect has a permission row");
    assert_eq!(feed_inspect.parity_row, 39);
    assert_eq!(feed_inspect.required_permissions, 32, "ManageGuild bit");
    assert_eq!(feed_inspect.surface, CommandSurface::BuiltinSlash);
    assert_eq!(feed_inspect.policy_hook, None);
    let game_mix = command_permission("game-mix").expect("game-mix has a permission row");
    assert_eq!(game_mix.parity_row, 43);
    assert_eq!(game_mix.required_permissions, 32, "ManageGuild bit");
    assert_eq!(game_mix.surface, CommandSurface::BuiltinSlash);
    assert_eq!(game_mix.policy_hook, None);
    let pause = command_permission("feed-pause").expect("feed-pause has a permission row");
    assert_eq!(pause.parity_row, 44);
    assert_eq!(pause.required_permissions, 32, "ManageGuild bit");
    assert_eq!(pause.surface, CommandSurface::BuiltinSlash);
    assert_eq!(pause.policy_hook, None);
    let resume = command_permission("feed-resume").expect("feed-resume has a permission row");
    assert_eq!(resume.parity_row, 45);
    assert_eq!(resume.required_permissions, 32, "ManageGuild bit");
    assert_eq!(resume.surface, CommandSurface::BuiltinSlash);
    assert_eq!(resume.policy_hook, None);
    for (name, parity_row) in [("command-disable", 40), ("command-enable", 41)] {
        let row = command_permission(name).expect("lifecycle command has a permission row");
        assert_eq!(row.parity_row, parity_row, "{name}");
        assert_eq!(row.required_permissions, 32, "{name} ManageGuild bit");
        assert_eq!(row.surface, CommandSurface::BuiltinSlash, "{name}");
        assert_eq!(row.policy_hook, None, "{name}");
    }
    let preview =
        command_permission("welcome-preview").expect("welcome-preview has a permission row");
    assert_eq!(preview.parity_row, 42);
    assert_eq!(preview.required_permissions, PERM_MANAGE_GUILD);
    assert_eq!(preview.surface, CommandSurface::BuiltinSlash);
    assert_eq!(preview.policy_hook, None);
    let clone = command_permission("command-clone").expect("clone has a permission row");
    assert_eq!(clone.parity_row, 47);
    assert_eq!(clone.required_permissions, 32, "ManageGuild bit");
    assert_eq!(clone.surface, CommandSurface::BuiltinSlash);
    assert_eq!(clone.policy_hook, None);
}

#[test]
fn lfg_edit_is_the_only_companion_of_the_canonical_lfg_parity_row() {
    let canonical = COMMAND_PERMISSIONS
        .iter()
        .find(|row| row.parity_row == 26)
        .expect("legacy LFG row remains canonical");
    assert_eq!(canonical.command, "lfg");
    let edit = command_permission("lfg-edit").expect("Next-only metadata companion");
    assert_eq!(edit.parity_row, 26);
    assert_eq!(edit.required_permissions, 1 << 33);
    assert_eq!(edit.surface, CommandSurface::BuiltinSlash);
    assert_eq!(edit.policy_hook, None);
    let mut row26: Vec<_> = COMMAND_PERMISSIONS
        .iter()
        .filter(|row| row.parity_row == 26)
        .map(|row| row.command)
        .collect();
    row26.sort_unstable();
    assert_eq!(row26, ["lfg", "lfg-edit"]);
    let mut other_rows = HashSet::new();
    for row in COMMAND_PERMISSIONS
        .iter()
        .filter(|row| row.parity_row != 26)
    {
        assert!(
            other_rows.insert(row.parity_row),
            "duplicate non-LFG parity row"
        );
    }
}

#[test]
fn next_only_read_permission_rows_do_not_become_mutation_policy_hooks() {
    for (name, number, bits) in [
        ("purge-preview", 35, 8192),
        ("channel-inspect", 36, 16),
        ("tempban-list", 46, 4),
    ] {
        let row = command_permission(name).expect("read has a permission row");
        assert_eq!(row.parity_row, number);
        assert_eq!(row.required_permissions, bits);
        assert_eq!(row.surface, CommandSurface::BuiltinSlash);
        assert_eq!(row.policy_hook, None);
        assert!(!ModerationAction::ALL
            .iter()
            .any(|action| action.command_name() == name));
    }
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
    assert_eq!(builtins.len(), 45);
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
        for permissions in [
            None,
            Some(0),
            Some(1 << 3),
            Some(!row.required_permissions),
            Some(!(row.required_permissions | (1 << 3))),
        ] {
            let outcome = router.route_slash(&ctx(permissions));
            // Only game-mix accepts Administrator without its advertised bit;
            // every legacy command retains the same exact-bit runtime check.
            let game_mix_admin =
                row.command == "game-mix" && permissions.is_some_and(|bits| bits & (1 << 3) != 0);
            if row.required_permissions == 0 || game_mix_admin {
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
    assert_eq!(routed, 45, "every retained builtin routes");
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
