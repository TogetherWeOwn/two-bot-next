//! Protected test-first contract for the additive leveling read inputs.
//! Builder also runs `reference_docs` and registry parity, updating their
//! generated/reference inputs without weakening those existing assertions.
//! `/leaderboard` without options remains the public seasonal top ten.
//! Explicit `page` is one-based; `mine:true` opens the invoking member's
//! seasonal page. Combining the two is refused, not silently ambiguous.

use two_bot_core::commands::core_commands;

#[test]
fn leaderboard_publishes_optional_bounded_integer_page_and_own_page_route() {
    let definitions = core_commands();
    let board = definitions
        .iter()
        .find(|command| command.name == "leaderboard")
        .expect("existing leaderboard definition");
    let page = board.options.iter().find(|option| option.name == "page");
    assert!(page.is_some(), "members need a discoverable page input");
    let page = page.unwrap();
    assert_eq!(page.kind, 4, "Discord Integer, not text or float");
    assert_ne!(page.required, Some(true), "omitted input stays valid");
    assert_eq!(page.min_value, Some(1), "page labels are one-based");
    assert!(
        page.max_value
            .is_some_and(|max| (3..=1_000_000).contains(&max)),
        "advertise a finite useful cap so overflow is refused before SQL"
    );
    let mine = board.options.iter().find(|option| option.name == "mine");
    assert!(mine.is_some(), "expose the route to the invoker's own page");
    let mine = mine.unwrap();
    assert_eq!(
        mine.kind, 5,
        "mine is a Boolean, not a target-member picker"
    );
    assert_ne!(mine.required, Some(true));
    assert!(!page.description.trim().is_empty());
    assert!(!mine.description.trim().is_empty());
    assert!(!board.dm_permission, "a seasonal board is guild-scoped");
    assert!(board.default_member_permissions.is_none());
    assert_eq!(board.options.len(), 2, "no new XP/reward policy inputs");
}

#[test]
fn control_rank_remains_optional_member_and_needs_no_new_permissions() {
    let definitions = core_commands();
    let rank = definitions.iter().find(|c| c.name == "rank").unwrap();
    assert!(!rank.dm_permission);
    assert!(rank.default_member_permissions.is_none());
    assert_eq!(rank.options.len(), 1);
    assert_eq!(rank.options[0].name, "member");
    assert_eq!(rank.options[0].kind, 6);
    assert_ne!(rank.options[0].required, Some(true));
}
