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
        let outcome = outcome.finalize_after_grant(&|_| mock_roles.contains(new_game.role_id), 1);
        assert!(outcome.reply.contains(new_game.primary_channel_id.unwrap()));
        assert!(
            !outcome.reply.contains(GAME_HUB_CHANNEL_ID),
            "role unlocked the room"
        );
        assert!(outcome.record_selected && outcome.record_routed);
        assert_eq!(outcome.routed.degraded_count, 0);
    }
}

#[test]
fn game_finalization_keeps_reply_plan_and_recording_in_sync_when_visibility_changes() {
    let pick = pick_by_key("horror").unwrap();
    let primary = pick.primary_channel_id.unwrap();
    for mode in [OnboardingMode::Legacy, OnboardingMode::Anchor] {
        for initially_visible in [false, true] {
            let outcome = adjudicate_game_select(
                mode,
                &["horror", "horror", "gone", "gone"],
                &[],
                &|id| initially_visible && id == primary,
                1,
                false,
            )
            .unwrap();
            assert_eq!(outcome.record_routed, initially_visible);
            let role_effects = (
                outcome.add_role_ids.clone(),
                outcome.remove_role_ids.clone(),
            );
            let final_visible = !initially_visible;
            let outcome = outcome.finalize_after_grant(&|id| final_visible && id == primary, 1);
            assert!(outcome.ephemeral && outcome.record_selected);
            assert_eq!(outcome.record_routed, final_visible);
            assert_eq!(
                (outcome.add_role_ids, outcome.remove_role_ids),
                role_effects
            );
            assert_eq!(outcome.routed.role_ids, vec![pick.role_id]);
            assert_eq!(outcome.routed.unknown_keys, vec!["gone"]);
            assert_eq!(outcome.routed.destinations.len(), 1);
            assert_eq!(outcome.routed.degraded_count, 0);
            assert!(!outcome.reply.contains(GAME_HUB_CHANNEL_ID));
            if final_visible {
                assert_eq!(outcome.routed.channel_ids, vec![primary]);
                assert_eq!(
                    outcome.routed.destinations[0].channel_id.as_deref(),
                    Some(primary)
                );
                assert!(outcome.reply.starts_with("Done. Here is where to go:"));
                assert!(outcome.reply.contains(&channel_link(1, primary)));
            } else {
                assert!(outcome.routed.channel_ids.is_empty());
                assert_eq!(outcome.routed.destinations[0].channel_id, None);
                assert!(outcome.reply.starts_with("Game roles saved."));
                assert!(outcome
                    .reply
                    .contains("No channel is available to you right now."));
                assert!(!outcome.reply.contains("discord.com/channels"));
            }
        }
    }
}

#[test]
fn game_finalization_preserves_dry_run_and_clear_outcomes() {
    for mode in [OnboardingMode::Legacy, OnboardingMode::Anchor] {
        for (keys, dry_run) in [(vec!["horror"], true), (vec![], false)] {
            let outcome = adjudicate_game_select(
                mode,
                &keys,
                &[pick_by_key("shooters").unwrap().role_id],
                &|_| false,
                1,
                dry_run,
            )
            .unwrap();
            let finalized = outcome.clone().finalize_after_grant(
                &|_| panic!("dry runs and clears must not resolve routes"),
                1,
            );
            assert_eq!(finalized, outcome);
            assert!(!finalized.record_selected && !finalized.record_routed);
        }
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
