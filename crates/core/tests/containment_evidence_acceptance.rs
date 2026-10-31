//! Acceptance: quarantine planning refuses incomplete or contradictory role
//! evidence before it can produce removals or an empty-plan success.
//!
//! A guild-role snapshot is evidence, not ground truth. Planning may only
//! proceed when the snapshot carries unique, nonzero role identities with
//! usable (non-negative) hierarchy positions, contains the guild baseline
//! role (`id == guild_id`, the @everyone role), and covers every role ID in
//! both fetched held-role sets (the executor's and the bot's). Anything else
//! fails closed: no `RemoveRoles` plan — in particular never the empty
//! `RemoveRoles` plan that downstream completion reads as "contained" — and
//! no dry-run plan either when the caller asked for a real plan.
//!
//! Controls pin the behavior that must not change: a complete snapshot with
//! a dangerous executor role still plans its removal, a complete safe-only
//! snapshot still plans the genuine empty removal list, and the managed-role
//! and hierarchy whole-plan refusals keep their existing reasons.

use two_bot_core::{plan_quarantine, ContainmentRole, QuarantinePlan, QuarantineRefusal};

/// Administrator bit: a dangerous permission.
const DANGEROUS: u64 = 1 << 3;
/// Send-messages-adjacent harmless bit used for safe roles.
const SAFE: u64 = 1 << 11;

fn role(id: &str, position: i64, permissions: u64, managed: bool) -> ContainmentRole {
    ContainmentRole {
        id: id.into(),
        position,
        permissions,
        managed,
    }
}

fn ids(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

/// Guild baseline (@everyone): identity equals the guild ID, position 0.
fn baseline() -> ContainmentRole {
    role("22", 0, SAFE, false)
}

fn bot_role() -> ContainmentRole {
    role("88", 5, 0, false)
}

fn danger() -> ContainmentRole {
    role("77", 1, DANGEROUS, false)
}

fn safe() -> ContainmentRole {
    role("76", 1, SAFE, false)
}

/// A complete snapshot covering held sets `["77"]` (executor) and `["88"]`
/// (bot).
fn complete() -> Vec<ContainmentRole> {
    vec![baseline(), danger(), safe(), bot_role()]
}

fn assert_refused(plan: QuarantinePlan, case: &str) {
    assert!(
        !matches!(
            plan,
            QuarantinePlan::RemoveRoles { .. } | QuarantinePlan::DryRun
        ),
        "{case}: incomplete role evidence must refuse before planning, got {plan:?}"
    );
}

// ---------------------------------------------------------------------------
// Incomplete or contradictory evidence: refuse, never plan.
// ---------------------------------------------------------------------------

#[test]
fn empty_snapshot_with_held_roles_refuses_instead_of_empty_plan_success() {
    assert_refused(
        plan_quarantine("22", &ids(&["77"]), &ids(&["88"]), &[], false),
        "empty snapshot, both members hold roles",
    );
}

#[test]
fn missing_executor_role_refuses() {
    let roles = vec![baseline(), safe(), bot_role()];
    assert_refused(
        plan_quarantine("22", &ids(&["77"]), &ids(&["88"]), &roles, false),
        "executor's held role absent from the snapshot",
    );
}

#[test]
fn missing_safe_executor_role_still_refuses_for_lack_of_coverage() {
    // Coverage is about evidence completeness, not dangerousness: an
    // unobserved held role might be the dangerous one.
    let roles = vec![baseline(), danger(), bot_role()];
    assert_refused(
        plan_quarantine("22", &ids(&["76", "77"]), &ids(&["88"]), &roles, false),
        "one of the executor's held roles absent from the snapshot",
    );
}

#[test]
fn missing_bot_role_refuses_even_when_executor_holds_only_safe_roles() {
    let roles = vec![baseline(), safe()];
    assert_refused(
        plan_quarantine("22", &ids(&["76"]), &ids(&["88"]), &roles, false),
        "bot's held role absent: bot hierarchy position is unproven",
    );
}

#[test]
fn missing_guild_baseline_refuses() {
    let roles = vec![danger(), safe(), bot_role()];
    assert_refused(
        plan_quarantine("22", &ids(&["77"]), &ids(&["88"]), &roles, false),
        "no role with id == guild_id (@everyone baseline)",
    );
}

#[test]
fn duplicate_role_identity_refuses() {
    let mut roles = complete();
    roles.push(danger());
    assert_refused(
        plan_quarantine("22", &ids(&["77"]), &ids(&["88"]), &roles, false),
        "same role id twice in the snapshot",
    );
}

#[test]
fn conflicting_duplicate_role_identity_refuses() {
    // One row says the held role is safe, the other says dangerous: the
    // snapshot contradicts itself and neither reading may be planned on.
    let roles = vec![
        baseline(),
        role("77", 1, SAFE, false),
        role("77", 1, DANGEROUS, false),
        bot_role(),
    ];
    assert_refused(
        plan_quarantine("22", &ids(&["77"]), &ids(&["88"]), &roles, false),
        "same role id with conflicting permissions",
    );
    let roles = vec![
        baseline(),
        role("77", 1, DANGEROUS, false),
        role("77", 2, DANGEROUS, false),
        bot_role(),
    ];
    assert_refused(
        plan_quarantine("22", &ids(&["77"]), &ids(&["88"]), &roles, false),
        "same role id with conflicting hierarchy positions",
    );
}

#[test]
fn zero_and_empty_role_identities_refuse() {
    for malformed in ["0", ""] {
        let mut roles = complete();
        roles.push(role(malformed, 2, DANGEROUS, false));
        assert_refused(
            plan_quarantine("22", &ids(&["77"]), &ids(&["88"]), &roles, false),
            "snapshot carries a zero/empty role identity",
        );
    }
}

#[test]
fn negative_hierarchy_positions_refuse() {
    let roles = vec![baseline(), role("77", -1, DANGEROUS, false), bot_role()];
    assert_refused(
        plan_quarantine("22", &ids(&["77"]), &ids(&["88"]), &roles, false),
        "dangerous role with a negative position",
    );
    let roles = vec![baseline(), safe(), role("88", -1, 0, false)];
    assert_refused(
        plan_quarantine("22", &ids(&["76"]), &ids(&["88"]), &roles, false),
        "bot role with a negative position: hierarchy comparison unusable",
    );
    let roles = vec![role("22", -1, SAFE, false), danger(), bot_role()];
    assert_refused(
        plan_quarantine("22", &ids(&["77"]), &ids(&["88"]), &roles, false),
        "guild baseline with a negative position",
    );
}

// ---------------------------------------------------------------------------
// Controls: complete evidence keeps today's behavior.
// ---------------------------------------------------------------------------

#[test]
fn complete_snapshot_with_dangerous_role_still_plans_removal() {
    assert_eq!(
        plan_quarantine("22", &ids(&["77"]), &ids(&["88"]), &complete(), false),
        QuarantinePlan::RemoveRoles {
            role_ids: ids(&["77"])
        }
    );
}

#[test]
fn complete_safe_only_snapshot_still_plans_genuine_empty_removal_list() {
    assert_eq!(
        plan_quarantine("22", &ids(&["76"]), &ids(&["88"]), &complete(), false),
        QuarantinePlan::RemoveRoles { role_ids: vec![] }
    );
}

#[test]
fn held_baseline_role_is_covered_and_never_planned_for_removal() {
    // Held-role fetches may or may not list @everyone; listing it is complete
    // evidence, and the baseline itself is never a removal candidate even
    // with dangerous permission bits.
    let roles = vec![role("22", 0, DANGEROUS, false), danger(), bot_role()];
    assert_eq!(
        plan_quarantine("22", &ids(&["22", "77"]), &ids(&["88"]), &roles, false),
        QuarantinePlan::RemoveRoles {
            role_ids: ids(&["77"])
        }
    );
}

#[test]
fn complete_managed_role_refusal_is_unchanged() {
    let roles = vec![baseline(), role("77", 1, DANGEROUS, true), bot_role()];
    assert_eq!(
        plan_quarantine("22", &ids(&["77"]), &ids(&["88"]), &roles, false),
        QuarantinePlan::Refused {
            reason: QuarantineRefusal::ManagedRole,
            blocked_role_ids: ids(&["77"]),
        }
    );
}

#[test]
fn complete_hierarchy_refusal_is_unchanged() {
    let roles = vec![baseline(), role("77", 5, DANGEROUS, false), bot_role()];
    assert_eq!(
        plan_quarantine("22", &ids(&["77"]), &ids(&["88"]), &roles, false),
        QuarantinePlan::Refused {
            reason: QuarantineRefusal::Hierarchy,
            blocked_role_ids: ids(&["77"]),
        }
    );
}

#[test]
fn dry_run_on_complete_snapshot_still_plans_no_removals() {
    assert_eq!(
        plan_quarantine("22", &ids(&["77"]), &ids(&["88"]), &complete(), true),
        QuarantinePlan::DryRun
    );
}
