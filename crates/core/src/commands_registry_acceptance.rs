// Included only inside commands::tests. Protected compatibility fixture: the
// source command builder remains editable without editing this test file.

#[test]
fn core_commands_match_legacy_names() {
    let core = core_commands();
    assert_eq!(core.len(), 7);
    assert_eq!(core[0].name, "rank");
    assert_eq!(core[1].name, "leaderboard");
    assert_eq!(core[2].name, "help");
    assert_eq!(core[3].name, "privacy");
    assert_eq!(core[4].name, "reopen");
    assert_eq!(core[5].name, "welcome-preview");
    assert_eq!(core[6].name, "game-mix");
    // Guild-only, DM off (legacy `setDMPermission(false)`).
    assert!(core.iter().all(|c| !c.dm_permission));
    // `rank` and `help` have optional selectors, `reopen` a required one;
    // `leaderboard` and the Next-only `welcome-preview` and `game-mix` take none.
    // Help stays open to everyone and names are not frozen to builtin choices.
    assert_eq!(core[0].options.len(), 1);
    assert_eq!(core[0].options[0].kind, 6);
    assert!(core[0].options[0].required.is_none());
    assert!(core[1].options.is_empty());
    assert_eq!(core[2].options.len(), 1);
    assert_eq!(core[2].options[0].name, "command");
    assert_eq!(core[2].options[0].kind, 3);
    assert!(core[2].options[0].required.is_none());
    assert_eq!(core[2].options[0].max_length, Some(32));
    assert!(core[2].options[0].choices.is_empty());
    assert_eq!(core[2].description, HELP_DESCRIPTION);
    assert!(core[2].default_member_permissions.is_none());
    // The Next-only `privacy` self-serve carries analytics and deletion
    // subcommands, is open to everyone, and takes no target-user option.
    assert_eq!(core[3].description, PRIVACY_DESCRIPTION);
    assert!(core[3].default_member_permissions.is_none());
    assert_eq!(core[3].options.len(), 6);
    let mut subcommands: Vec<&str> = core[3].options.iter().map(|o| o.name.as_str()).collect();
    subcommands.sort_unstable();
    assert_eq!(subcommands, ["delete", "deletion-receipt", "deletion-status", "opt-in", "opt-out", "status"]);
    for option in &core[3].options {
        assert_eq!(option.kind, CommandOptionType::SubCommand.as_u8());
        assert!(option.required.is_none());
        if matches!(option.name.as_str(), "deletion-status" | "deletion-receipt") {
            assert_eq!(option.options.len(), 1);
            assert_eq!(option.options[0].name, "request-id");
            assert_eq!(option.options[0].kind, CommandOptionType::String.as_u8());
            assert_eq!(option.options[0].required, Some(true));
            assert_eq!(option.options[0].max_length, Some(128));
        } else {
            assert!(option.options.is_empty());
        }
    }
    // The Next-only `reopen` picker recovery takes one required `kind`
    // choice (games/session), is open to everyone, and takes no
    // other-member target or role-management option.
    assert_eq!(core[4].description, REOPEN_DESCRIPTION);
    assert!(core[4].default_member_permissions.is_none());
    assert_eq!(core[4].options.len(), 1);
    let kind = &core[4].options[0];
    assert_eq!(kind.name, "kind");
    assert_eq!(kind.kind, CommandOptionType::String.as_u8());
    assert_eq!(kind.required, Some(true));
    let choices: Vec<(&str, &str)> = kind
        .choices
        .iter()
        .map(|choice| (choice.name.as_str(), choice.value.as_str()))
        .collect();
    assert_eq!(choices, [("Games", "games"), ("Session", "session")]);
    // The Next-only `welcome-preview` admin rendering takes no options and
    // is gated on Manage Server.
    assert!(core[5].options.is_empty());
    assert_eq!(
        core[5].default_member_permissions,
        Some(PERM_MANAGE_GUILD.to_string())
    );
    // The Next-only `game-mix` staff report takes no options and is gated
    // on Manage Server.
    assert!(core[6].options.is_empty());
    assert_eq!(
        core[6].default_member_permissions,
        Some(PERM_MANAGE_GUILD.to_string())
    );
}
