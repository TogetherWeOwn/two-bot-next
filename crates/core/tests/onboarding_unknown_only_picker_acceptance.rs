//! Acceptance: a nonempty game-picker submission whose keys are all
//! unrecognized (a stale menu) must not be treated as an intentional clear.
//!
//! Contract under test:
//! - unknown-only nonempty input preserves every held role, records no
//!   successful selection or routing fact, and answers with an ephemeral
//!   stale-menu/retry explanation (never the cleared reply);
//! - an explicitly empty selection still clears held catalog roles and
//!   records nothing;
//! - known-only and mixed known/stale selections keep existing behaviour:
//!   roles are matched to the valid picks, unrelated roles survive, and the
//!   selection/routing facts are recorded;
//! - dry-run and session-mode rejection are unchanged.

use two_bot_core::onboarding::*;

fn role(key: &str) -> &'static str {
    pick_by_key(key).expect("catalog pick").role_id
}

fn see_everything() -> impl Fn(&str) -> bool {
    |_| true
}

#[test]
fn unknown_only_submission_preserves_held_roles_and_records_nothing() {
    for mode in [OnboardingMode::Legacy, OnboardingMode::Anchor] {
        let out = adjudicate_game_select(
            mode,
            &["retired-game-key"],
            &[role("shooters"), role("horror"), "unrelated-role"],
            &see_everything(),
            1,
            false,
        )
        .expect("picker modes still answer an unknown-only submission");
        assert!(
            out.add_role_ids.is_empty(),
            "{mode:?}: unknown-only input must add no roles, got {:?}",
            out.add_role_ids
        );
        assert!(
            out.remove_role_ids.is_empty(),
            "{mode:?}: unknown-only input must remove no roles, got {:?}",
            out.remove_role_ids
        );
        assert!(
            !out.record_selected,
            "{mode:?}: no successful selection may be recorded"
        );
        assert!(
            !out.record_routed,
            "{mode:?}: no routing fact may be recorded"
        );
        assert!(out.routed.role_ids.is_empty(), "{mode:?}: no routed roles");
        assert!(
            out.routed.destinations.is_empty(),
            "{mode:?}: no routed destinations"
        );
        assert_eq!(
            out.routed.unknown_keys,
            vec!["retired-game-key".to_owned()],
            "{mode:?}: the stale key is reported, not silently dropped"
        );
    }
}

#[test]
fn unknown_only_submission_answers_with_an_ephemeral_retry_explanation() {
    let out = adjudicate_game_select(
        OnboardingMode::Legacy,
        &["retired-game-key"],
        &[role("shooters")],
        &see_everything(),
        1,
        false,
    )
    .expect("legacy serves the picker");
    assert!(out.ephemeral, "only the clicker sees the stale-menu answer");
    assert_eq!(
        out.reply,
        "That menu looks stale - I did not recognize those choices.\nNothing was changed. Open the picker again and choose afresh."
    );
    assert_ne!(
        out.reply, PICKER_CLEARED_REPLY,
        "unknown-only input must never claim the roles were cleared"
    );
    assert_ne!(
        out.reply, PICKER_DRY_RUN_REPLY,
        "unknown-only input is not a dry run"
    );
    assert!(
        !out.reply.trim().is_empty(),
        "the member gets an explanation"
    );
    assert!(
        out.reply.to_lowercase().contains("again"),
        "reply must invite a retry from a fresh menu, got {:?}",
        out.reply
    );
}

#[test]
fn unknown_only_submission_with_nothing_held_is_still_not_a_clear() {
    let out = adjudicate_game_select(
        OnboardingMode::Legacy,
        &["retired-game-key", "another-gone-key", "retired-game-key"],
        &["unrelated-role"],
        &see_everything(),
        1,
        false,
    )
    .expect("legacy serves the picker");
    assert!(out.add_role_ids.is_empty() && out.remove_role_ids.is_empty());
    assert!(!out.record_selected && !out.record_routed);
    assert_ne!(
        out.reply, PICKER_CLEARED_REPLY,
        "nothing to clear must not be reported as a clear"
    );
    assert_eq!(
        out.routed.unknown_keys,
        vec!["retired-game-key".to_owned(), "another-gone-key".to_owned()],
        "distinct stale keys are reported once, in submission order"
    );
}

#[test]
fn explicit_empty_selection_still_clears_held_catalog_roles() {
    let out = adjudicate_game_select(
        OnboardingMode::Legacy,
        &[],
        &[role("shooters"), role("horror"), "unrelated-role"],
        &see_everything(),
        1,
        false,
    )
    .expect("legacy serves the picker");
    let mut removed = out.remove_role_ids.clone();
    removed.sort();
    let mut expected = vec![role("shooters").to_owned(), role("horror").to_owned()];
    expected.sort();
    assert_eq!(
        removed, expected,
        "a raw empty selection still clears held catalog roles (and only those)"
    );
    assert!(out.add_role_ids.is_empty());
    assert_eq!(out.reply, PICKER_CLEARED_REPLY);
    assert!(
        !out.record_selected && !out.record_routed,
        "the clear branch keeps its no-recording semantics"
    );
}

#[test]
fn known_only_selection_still_matches_roles_to_the_menu() {
    let out = adjudicate_game_select(
        OnboardingMode::Legacy,
        &["horror"],
        &[role("shooters"), "unrelated-role"],
        &see_everything(),
        1,
        false,
    )
    .expect("legacy serves the picker");
    assert_eq!(out.add_role_ids, vec![role("horror").to_owned()]);
    assert_eq!(out.remove_role_ids, vec![role("shooters").to_owned()]);
    assert!(out.ephemeral);
    assert!(out.record_selected && out.record_routed);
    assert!(out.routed.unknown_keys.is_empty());
}

#[test]
fn mixed_known_and_stale_selection_still_applies_the_known_picks() {
    let out = adjudicate_game_select(
        OnboardingMode::Legacy,
        &["retired-game-key", "horror"],
        &[role("shooters"), "unrelated-role"],
        &see_everything(),
        1,
        false,
    )
    .expect("legacy serves the picker");
    assert_eq!(
        out.add_role_ids,
        vec![role("horror").to_owned()],
        "mixed input is not a blanket refusal: valid picks still apply"
    );
    assert_eq!(out.remove_role_ids, vec![role("shooters").to_owned()]);
    assert!(out.record_selected && out.record_routed);
    assert_ne!(out.reply, PICKER_CLEARED_REPLY);
    assert_eq!(
        out.routed.unknown_keys,
        vec!["retired-game-key".to_owned()],
        "the stale half of a mixed submission is still reported"
    );
}

#[test]
fn dry_run_and_session_mode_rejection_are_unchanged_for_unknown_only_input() {
    let dry = adjudicate_game_select(
        OnboardingMode::Legacy,
        &["retired-game-key"],
        &[role("shooters")],
        &see_everything(),
        1,
        true,
    )
    .expect("dry run still answers");
    assert_eq!(dry.reply, PICKER_DRY_RUN_REPLY);
    assert!(dry.add_role_ids.is_empty() && dry.remove_role_ids.is_empty());
    assert!(!dry.record_selected && !dry.record_routed);
    assert_eq!(
        adjudicate_game_select(
            OnboardingMode::Session,
            &["retired-game-key"],
            &[role("shooters")],
            &see_everything(),
            1,
            false
        ),
        None,
        "session mode must not route the game select at all"
    );
}
