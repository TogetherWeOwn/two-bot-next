//! Protected acceptance: level-up reward plans refuse over-cap role writes.
//!
//! BUILDER CONTRACT: this file is frozen. Build on the acceptance branch, keep
//! every test here verbatim, and make the RED tests pass by changing
//! production code only. Do not edit the fixtures or assertions: they are the
//! spec. The existing `leveling.rs` unit tests and other `plan_reward_roles`
//! callers are NOT protected; update them for the new signature.
//!
//! Baseline behavior under test: `plan_reward_roles` turns every reward row at
//! or below the new level into a grant (and every held over-level ladder role
//! into a revoke) with no list cap, returning a bare `RewardRolePlan`. The
//! executor then fans out one Discord REST call per entry.
//!
//! Required behavior:
//! - Each list carries an explicit cap of exactly [`ROLE_WRITE_CAP`] (10).
//!   Eleven grants refuse; eleven revokes refuse; ten of either pass.
//! - The cap applies per list, not to the combined total: six grants plus six
//!   revokes pass.
//! - A cap hit refuses with an error whose message contains the offending list
//!   (`grant` or `revoke`), the word `cap`, the offending count and the cap.
//! - Under-cap plans are byte-identical to today, including audit reasons.
//! - Suppressed writes (`role_writes = false`) stay an empty plan and never
//!   refuse, even for an over-cap reward set.
//!
//! Shape freedom: `plan_reward_roles` must return
//! `Result<RewardRolePlan, E>` for some error type `E: Debug + Display` of
//! the builder's choice. The [`PlanResult`] adapter accepts both the legacy
//! bare plan and any `Result` shape, so this file compiles on the baseline
//! and fails on behavior (over-cap yields `Ok`, never `Err`), not on a
//! missing API. Do NOT change the planner's grant/revoke/reason rules for
//! under-cap inputs: the controls pin them exactly.
//!
//! Offline only: pure planner calls, no database, no Discord, no environment.

use std::fmt::{Debug, Display};

use two_bot_core::leveling::{plan_reward_roles, LevelRoleReward, RewardRolePlan};

/// Explicit per-list role-write cap pinned by this acceptance.
const ROLE_WRITE_CAP: usize = 10;

/// Adapter over the planner's return shape: the legacy bare plan maps to
/// `Ok`, and any fallible replacement maps through its message. Stays called
/// in both shapes, so no shadowed-trait liveness hack is needed.
trait PlanResult {
    fn acceptance_result(self) -> Result<RewardRolePlan, String>;
}

impl PlanResult for RewardRolePlan {
    fn acceptance_result(self) -> Result<RewardRolePlan, String> {
        Ok(self)
    }
}

impl<E: Debug + Display> PlanResult for Result<RewardRolePlan, E> {
    fn acceptance_result(self) -> Result<RewardRolePlan, String> {
        self.map_err(|error| format!("{error}; {error:?}"))
    }
}

/// `count` rewards at ascending levels starting at `first`, with zero-padded
/// short ids (`prefix` + level). The planner sorts grants, and zero padding
/// keeps lexicographic order identical to numeric order.
fn rewards(first: u64, count: usize, prefix: &str) -> Vec<LevelRoleReward> {
    (0..count)
        .map(|index| {
            let level = first + index as u64;
            LevelRoleReward {
                level,
                role_id: format!("{prefix}{level:02}"),
            }
        })
        .collect()
}

#[test]
fn over_cap_grants_refuse_with_a_fixed_reason() {
    let ladder = rewards(1, ROLE_WRITE_CAP + 1, "g");
    let result = plan_reward_roles(11, &ladder, &[], true, false).acceptance_result();
    let message = result.expect_err("eleven grants must refuse the role-write portion");
    for needle in [
        "grant",
        "cap",
        &(ROLE_WRITE_CAP + 1).to_string(),
        &ROLE_WRITE_CAP.to_string(),
    ] {
        assert!(
            message.contains(needle),
            "refusal must name the list, the cap and both counts: {message}"
        );
    }
}

#[test]
fn over_cap_revokes_refuse_with_a_fixed_reason() {
    // Eleven ladder rungs above the member's level, all held: eleven revokes.
    let ladder = rewards(12, ROLE_WRITE_CAP + 1, "v");
    let member_roles: Vec<String> = ladder.iter().map(|reward| reward.role_id.clone()).collect();
    let result = plan_reward_roles(5, &ladder, &member_roles, true, true).acceptance_result();
    let message = result.expect_err("eleven revokes must refuse the role-write portion");
    for needle in [
        "revoke",
        "cap",
        &(ROLE_WRITE_CAP + 1).to_string(),
        &ROLE_WRITE_CAP.to_string(),
    ] {
        assert!(
            message.contains(needle),
            "refusal must name the list, the cap and both counts: {message}"
        );
    }
}

#[test]
fn exactly_cap_grants_pass_byte_identical() {
    let ladder = rewards(1, ROLE_WRITE_CAP, "g");
    let plan = plan_reward_roles(10, &ladder, &[], true, false)
        .acceptance_result()
        .expect("ten grants are within the cap");
    let expected_grant: Vec<String> = (1..=ROLE_WRITE_CAP as u64)
        .map(|level| format!("g{level:02}"))
        .collect();
    assert_eq!(
        plan,
        RewardRolePlan {
            grant: expected_grant,
            revoke: Vec::new(),
            grant_reason: "TWO leveling: reached level 10".to_owned(),
            revoke_reason: None,
        }
    );
}

#[test]
fn exactly_cap_revokes_pass_byte_identical() {
    let ladder = rewards(12, ROLE_WRITE_CAP, "v");
    let member_roles: Vec<String> = ladder.iter().map(|reward| reward.role_id.clone()).collect();
    let plan = plan_reward_roles(5, &ladder, &member_roles, true, true)
        .acceptance_result()
        .expect("ten revokes are within the cap");
    let expected_revoke: Vec<String> = (12..12 + ROLE_WRITE_CAP as u64)
        .map(|level| format!("v{level:02}"))
        .collect();
    assert_eq!(
        plan,
        RewardRolePlan {
            grant: Vec::new(),
            revoke: expected_revoke,
            grant_reason: "TWO leveling: reached level 5".to_owned(),
            revoke_reason: Some("TWO leveling: below level thresholds at level 5".to_owned()),
        }
    );
}

#[test]
fn mixed_lists_are_capped_per_list_not_in_total() {
    // Six grants plus six revokes: twelve writes total, six per list. Passes.
    let mut ladder = rewards(1, 6, "a");
    ladder.extend(rewards(20, 6, "b"));
    let member_roles: Vec<String> = (20..26).map(|level| format!("b{level:02}")).collect();
    let plan = plan_reward_roles(6, &ladder, &member_roles, true, true)
        .acceptance_result()
        .expect("six grants plus six revokes are within the per-list cap");
    assert_eq!(
        plan.grant,
        (1..=6)
            .map(|level| format!("a{level:02}"))
            .collect::<Vec<_>>()
    );
    assert_eq!(plan.revoke, member_roles);

    // Ten grants plus ten revokes: twenty writes total, ten per list. Passes.
    let mut ladder = rewards(1, 10, "a");
    ladder.extend(rewards(20, 10, "b"));
    let member_roles: Vec<String> = (20..30).map(|level| format!("b{level:02}")).collect();
    let plan = plan_reward_roles(10, &ladder, &member_roles, true, true)
        .acceptance_result()
        .expect("ten grants plus ten revokes are within the per-list cap");
    assert_eq!(plan.grant.len(), ROLE_WRITE_CAP);
    assert_eq!(plan.revoke.len(), ROLE_WRITE_CAP);
}

#[test]
fn under_cap_classic_plans_are_byte_identical() {
    let ladder = vec![
        LevelRoleReward {
            level: 5,
            role_id: "r05".to_owned(),
        },
        LevelRoleReward {
            level: 10,
            role_id: "r10".to_owned(),
        },
    ];
    // Level 10 holding nothing: grants both rungs.
    let plan = plan_reward_roles(10, &ladder, &[], true, false)
        .acceptance_result()
        .expect("under-cap plans stay accepted");
    assert_eq!(
        plan,
        RewardRolePlan {
            grant: vec!["r05".to_owned(), "r10".to_owned()],
            revoke: Vec::new(),
            grant_reason: "TWO leveling: reached level 10".to_owned(),
            revoke_reason: None,
        }
    );
    // Re-planning after applying is empty (idempotent grants).
    let replan = plan_reward_roles(10, &ladder, &plan.grant, true, false)
        .acceptance_result()
        .expect("under-cap replans stay accepted");
    assert!(replan.grant.is_empty() && replan.revoke.is_empty());
    // Level 5 holding the level-10 role: keeps 5, revokes 10, grants nothing;
    // non-ladder roles are never touched.
    let plan = plan_reward_roles(
        5,
        &ladder,
        &["r05".to_owned(), "r10".to_owned(), "outsider".to_owned()],
        true,
        true,
    )
    .acceptance_result()
    .expect("under-cap mixed plans stay accepted");
    assert!(plan.grant.is_empty());
    assert_eq!(plan.revoke, vec!["r10".to_owned()]);
    assert_eq!(
        plan.revoke_reason.as_deref(),
        Some("TWO leveling: below level thresholds at level 5")
    );
}

#[test]
fn suppressed_writes_stay_empty_and_never_refuse() {
    // Session mode suppresses every role write: even an over-cap reward set
    // plans nothing, and there is nothing to refuse.
    let ladder = rewards(1, ROLE_WRITE_CAP + 1, "g");
    let plan = plan_reward_roles(11, &ladder, &[], false, true)
        .acceptance_result()
        .expect("suppressed writes never refuse");
    assert!(plan.grant.is_empty() && plan.revoke.is_empty());
    assert_eq!(plan.grant_reason, "TWO leveling: reached level 11");
    assert!(plan.revoke_reason.is_none());
}
