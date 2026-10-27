//! Protected publication and routing controls for XP visibility.
//! Do not freeze option counts: separately owned navigation options may coexist.

use two_bot_core::{
    commands::core_commands, HandlerId, InteractionRouter, RouterGates, SlashContext, SlashOutcome,
};

fn gates() -> RouterGates {
    RouterGates {
        configured_guild: Some(1545644954272137297),
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

#[test]
fn rank_publication_adds_optional_boolean_details_without_losing_optional_member() {
    let commands = core_commands();
    let rank = commands.iter().find(|c| c.name == "rank").unwrap();
    assert!(!rank.dm_permission);
    assert!(rank.default_member_permissions.is_none());
    let member = rank.options.iter().find(|o| o.name == "member").unwrap();
    assert_eq!(member.kind, 6);
    assert_ne!(member.required, Some(true));
    let details = rank
        .options
        .iter()
        .find(|o| o.name == "details")
        .expect("published /rank must offer optional XP details");
    assert_eq!(details.kind, 5);
    assert_ne!(details.required, Some(true));
    assert_eq!(
        rank.options.iter().filter(|o| o.name == "details").count(),
        1
    );
}

#[test]
fn leaderboard_publication_offers_only_explicit_seasonal_and_lifetime_scope_choices() {
    let commands = core_commands();
    let board = commands.iter().find(|c| c.name == "leaderboard").unwrap();
    assert!(!board.dm_permission);
    assert!(board.default_member_permissions.is_none());
    let scope = board
        .options
        .iter()
        .find(|o| o.name == "scope")
        .expect("published /leaderboard must offer explicit scope");
    assert_eq!(scope.kind, 3);
    assert_ne!(scope.required, Some(true));
    let mut choices: Vec<&str> = scope.choices.iter().map(|c| c.value.as_str()).collect();
    choices.sort_unstable();
    assert_eq!(choices, ["lifetime", "seasonal"]);
    assert_eq!(
        board.options.iter().filter(|o| o.name == "scope").count(),
        1
    );
}

#[test]
fn leveling_routes_with_optional_features_off_but_not_outside_configured_guild() {
    let router = InteractionRouter::new(gates());
    for (name, handler) in [
        ("rank", HandlerId::Rank),
        ("leaderboard", HandlerId::Leaderboard),
    ] {
        let own = SlashContext {
            name,
            guild_id: Some(1545644954272137297),
            actor_permissions: Some(0),
            custom_row: None,
        };
        assert_eq!(router.route_slash(&own), SlashOutcome::Handled { handler });
        for guild_id in [None, Some(1545644954272137298)] {
            let foreign = SlashContext { guild_id, ..own };
            assert!(!matches!(
                router.route_slash(&foreign),
                SlashOutcome::Handled { .. }
            ));
        }
    }
}
