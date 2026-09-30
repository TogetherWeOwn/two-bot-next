//! Plain-data executor contract tests; no Discord transport or credentials.
use std::collections::HashSet;

use two_bot_core::onboarding::*;

#[test]
fn game_role_effects_preserve_unrelated_roles_and_resolve_after_grant() {
    for mode in [OnboardingMode::Legacy, OnboardingMode::Anchor] {
        let old_game = pick_by_key("shooters").unwrap();
        let new_game = pick_by_key("horror").unwrap();
        let mut mock_roles: HashSet<String> = [old_game.role_id, "unrelated-role"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let outcome = adjudicate_game_select(
            mode,
            &["horror"],
            &[old_game.role_id, "unrelated-role"],
            &|_| false,
            1,
            false,
        )
        .unwrap();
        assert!(outcome.ephemeral);
        assert!(outcome.reply.contains(GAME_HUB_CHANNEL_ID));
        for role in &outcome.add_role_ids {
            mock_roles.insert(role.clone());
        }
        for role in &outcome.remove_role_ids {
            mock_roles.remove(role);
        }
        assert_eq!(
            mock_roles,
            [new_game.role_id, "unrelated-role"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
        let final_plan =
            plan_game_selection(&["horror"], &|_| mock_roles.contains(new_game.role_id));
        let reply = game_picker_reply(&final_plan, 1);
        assert!(reply.contains(new_game.primary_channel_id.unwrap()));
        assert!(
            !reply.contains(GAME_HUB_CHANNEL_ID),
            "role unlocked the room"
        );
        assert_eq!(final_plan.degraded_count, 0);
    }
}

#[test]
fn session_has_no_game_role_effect_and_hidden_picks_never_link() {
    assert!(adjudicate_game_select(
        OnboardingMode::Session,
        &["shooters"],
        &[],
        &|_| true,
        1,
        false
    )
    .is_none());
    let picks = build_session_picks("10", "11");
    let outcome =
        adjudicate_session_select(&["find-players", "join-voice"], &|id| id == "10", &picks);
    assert!(outcome.ephemeral);
    assert_eq!(outcome.routed.channel_ids, vec!["10"]);
    assert_eq!(outcome.routed.unavailable, vec!["join-voice"]);
    assert!(!outcome.reply.contains("<#11>"));
}

#[test]
fn mention_like_goodbye_username_still_requires_no_mentions() {
    let outcome = adjudicate_goodbye(
        OnboardingMode::Session,
        "@everyone <@42> <@&99>",
        Some(1),
        Some("3"),
        false,
    )
    .unwrap();
    match outcome {
        GoodbyeEffect::Post {
            mentions, content, ..
        } => {
            assert_eq!(mentions, MentionPolicy::None);
            assert!(content.contains("left the server"));
        }
        GoodbyeEffect::Skip { .. } => panic!("session goodbye is configured"),
    }
}
