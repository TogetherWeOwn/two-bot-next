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
    // `leaderboard` and the Next-only `welcome-preview` take none, while
    // `game-mix` takes the three optional browse selectors pinned below.
    // Help stays open to everyone and names are not frozen to builtin choices.
    assert_eq!(core[0].options.len(), 1);
    assert_eq!(core[0].options[0].kind, 6);
    assert!(core[0].options[0].required.is_none());
    assert!(core[1].options.is_empty());
    assert_eq!(core[2].options.len(), 2);
    assert_eq!(core[2].options[0].name, "command");
    assert_eq!(core[2].options[0].kind, 3);
    assert!(core[2].options[0].required.is_none());
    assert_eq!(core[2].options[0].max_length, Some(32));
    assert!(core[2].options[0].choices.is_empty());
    // Optional focused subcommand-path selector: space-separated exact
    // segments within the selected root. 96 wire chars carry the deepest
    // Discord path (two 32-char segments plus separators) with headroom.
    assert_eq!(core[2].options[1].name, "subcommand");
    assert_eq!(
        core[2].options[1].description,
        "Subcommand path, space-separated (requires command)"
    );
    assert_eq!(core[2].options[1].kind, 3);
    assert!(core[2].options[1].required.is_none());
    assert_eq!(core[2].options[1].max_length, Some(96));
    assert!(core[2].options[1].choices.is_empty());
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
    // The Next-only `game-mix` staff report takes exactly three optional
    // browse selectors and is gated on Manage Server. It has no guild or
    // member selector and no live-guild opt-in.
    assert_eq!(core[6].options.len(), 3);
    let mut selectors: Vec<&str> = core[6].options.iter().map(|o| o.name.as_str()).collect();
    selectors.sort_unstable();
    assert_eq!(selectors, ["format", "page", "sort"]);
    for option in &core[6].options {
        assert_ne!(option.required, Some(true));
        assert!(option.options.is_empty());
    }
    let page = core[6]
        .options
        .iter()
        .find(|o| o.name == "page")
        .expect("page selector");
    assert_eq!(page.kind, CommandOptionType::Integer.as_u8());
    assert_eq!(page.min_value, Some(1));
    assert!(
        page.max_value
            .is_some_and(|max| (2..=i64::from(i32::MAX)).contains(&max)),
        "finite published page bound"
    );
    assert!(page.choices.is_empty());
    for (name, expected) in [
        ("sort", ["play", "voice"]),
        ("format", ["json", "summary"]),
    ] {
        let selector = core[6]
            .options
            .iter()
            .find(|o| o.name == name)
            .expect("browse selector");
        assert_eq!(selector.kind, CommandOptionType::String.as_u8());
        let mut values: Vec<&str> = selector
            .choices
            .iter()
            .map(|choice| choice.value.as_str())
            .collect();
        values.sort_unstable();
        let mut expected = expected;
        expected.sort_unstable();
        assert_eq!(values, expected);
    }
    assert_eq!(
        core[6].default_member_permissions,
        Some(PERM_MANAGE_GUILD.to_string())
    );
}
