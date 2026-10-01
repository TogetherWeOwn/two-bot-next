//! Plain-data executor contract tests; no Discord transport or credentials.
//! Post-freeze contracts: legacy b948b89, b3747a8, a40d4a5 and d3d9afe,
//! included in the bffccf3 parity baseline.
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
            &|id| id == GAME_HUB_CHANNEL_ID,
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
fn stale_duplicate_and_reordered_session_fixtures_keep_valid_choices_roleless() {
    let catalog = build_session_picks("10", "11");
    for keys in [
        vec!["retired", "join-voice", "find-players"],
        vec!["join-voice", "find-players", "retired"],
        vec![
            "join-voice",
            "retired",
            "join-voice",
            "find-players",
            "find-players",
        ],
    ] {
        let outcome = adjudicate_session_select(&keys, &|id| id == "10", &catalog);
        assert!(outcome.ephemeral && outcome.record_routed);
        assert_eq!(outcome.routed.picks, vec!["find-players", "join-voice"]);
        assert_eq!(outcome.routed.channel_ids, vec!["10"]);
        assert_eq!(outcome.routed.unavailable, vec!["join-voice"]);
        assert_eq!(outcome.routed.unknown_keys, vec!["retired"]);
        assert_eq!(outcome.reply, "On it - head to <#10>. One choice was gone or stale, so I skipped it - open the picker again if you want to re-pick that part.");
        assert!(!outcome.reply.contains("<#11>"));
        let row = session_routed_row("1", "2", &outcome.routed, "2026-10-01T00:00:00Z");
        assert_eq!(row.event_type, EVENT_CHANNEL_ROUTED);
        assert_eq!(row.source, SOURCE_SESSION_PICKER);
        let metadata: serde_json::Value =
            serde_json::from_str(row.metadata.as_deref().unwrap()).unwrap();
        assert_eq!(
            metadata,
            serde_json::json!({
                "picks": ["find-players", "join-voice"],
                "channels": ["10"],
                "unavailable": ["join-voice"],
            })
        );
    }
}

#[test]
fn unroutable_session_fixtures_record_nothing_and_never_link() {
    let catalog = build_session_picks("10", "11");
    for keys in [
        vec![],
        vec!["retired"],
        vec!["join-voice", "find-players", "retired", "join-voice"],
    ] {
        let outcome = adjudicate_session_select(&keys, &|_| false, &catalog);
        assert!(outcome.ephemeral);
        assert!(!outcome.record_routed);
        assert!(outcome.routed.channel_ids.is_empty());
        assert!(!outcome.reply.contains("<#"));
        assert!(outcome.reply.contains("Nothing was changed"));
        if keys.contains(&"find-players") {
            assert_eq!(outcome.routed.picks, vec!["find-players", "join-voice"]);
            assert_eq!(
                outcome.routed.unavailable,
                vec!["find-players", "join-voice"]
            );
        }
        if keys.contains(&"retired") {
            assert!(outcome.reply.contains("stale"));
        }
    }
}

#[test]
fn game_visibility_fixtures_save_deduped_roles_but_only_link_visible_destinations() {
    let shooters = pick_by_key("shooters").unwrap();
    let rocketleague = pick_by_key("rocketleague").unwrap();
    for mode in [OnboardingMode::Legacy, OnboardingMode::Anchor] {
        for (visible_channel, expected_channels, expected_degraded) in [
            (None, vec![], 0),
            (
                shooters.primary_channel_id,
                vec![shooters.primary_channel_id.unwrap()],
                0,
            ),
            (Some(GAME_HUB_CHANNEL_ID), vec![GAME_HUB_CHANNEL_ID], 1),
        ] {
            let outcome = adjudicate_game_select(
                mode,
                &["shooters", "shooters", "rocketleague", "gone", "gone"],
                &[],
                &|id| Some(id) == visible_channel,
                1,
                false,
            )
            .unwrap();
            assert!(outcome.ephemeral && outcome.record_selected);
            assert_eq!(
                outcome.add_role_ids,
                vec![shooters.role_id, rocketleague.role_id]
            );
            assert_eq!(outcome.routed.unknown_keys, vec!["gone"]);
            assert_eq!(outcome.routed.destinations.len(), 2);
            assert_eq!(outcome.routed.degraded_count, expected_degraded);
            assert_eq!(outcome.routed.channel_ids, expected_channels);
            assert_eq!(outcome.record_routed, visible_channel.is_some());
            for destination in &outcome.routed.destinations {
                if let Some(channel_id) = &destination.channel_id {
                    assert_eq!(Some(channel_id.as_str()), visible_channel);
                    assert!(outcome.reply.contains(&channel_link(1, channel_id)));
                } else {
                    assert!(!destination.degraded);
                    assert!(outcome
                        .reply
                        .contains("No channel is available to you right now."));
                }
            }
            match visible_channel {
                None => {
                    assert!(outcome.reply.starts_with("Game roles saved."));
                    assert!(
                        !outcome.reply.contains("discord.com/channels")
                            && !outcome.reply.contains("<#")
                    );
                }
                Some(id) => {
                    assert!(outcome.reply.starts_with("Done. Here is where to go:"));
                    if id != GAME_HUB_CHANNEL_ID {
                        assert!(!outcome.reply.contains(GAME_HUB_CHANNEL_ID));
                    }
                }
            }
        }
    }
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
