// Included only inside commands::tests. Protected compatibility fixture: the
// source command builder remains editable without editing this test file.

#[test]
fn core_commands_match_legacy_names() {
    let core = core_commands();
    assert_eq!(core.len(), 4);
    assert_eq!(core[0].name, "rank");
    assert_eq!(core[1].name, "leaderboard");
    assert_eq!(core[2].name, "help");
    assert_eq!(core[3].name, "privacy");
    // Guild-only, DM off (legacy `setDMPermission(false)`).
    assert!(core.iter().all(|c| !c.dm_permission));
    // `rank` and `help` have optional selectors; `leaderboard` takes none.
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
}
