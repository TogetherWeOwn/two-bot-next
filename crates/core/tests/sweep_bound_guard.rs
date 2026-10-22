//! Sweep-budget adoption guard: the inactivity scan must page under a budget.
//!
//! RED now (behavioral): `run_sweep` re-runs the keyset select until a page
//! comes back empty or short, with no page cap, no deadline and no resume
//! cursor — a large membership pages past any bound in one pass. This guard
//! compiles and runs on the unmodified tree (genuine assertion RED, not a
//! build failure), following the `include_str!` guard precedent in
//! `reconnect_adoption_guard.rs`.
//!
//! Exact budget/resume semantics are pinned behaviorally in
//! `sweep_budget_acceptance.rs` (offline child-compiled subject). This file
//! pins adoption plus the preserved semantics: `run_sweep` keeps its complete
//! signature and outcome, the keyset select keeps its shape, suppression and
//! the write-gated outcome filter stay. Offline: no database, no Discord,
//! no clock.
//!
//! Builder contract: add `SweepBudget { max_pages, deadline }`,
//! `SweepCursor`, and `run_sweep_budgeted` (see the acceptance file), keep
//! `run_sweep(pool, now, days)` complete, and keep the paged SELECT text. If
//! the contract needs to change, renegotiate on the card — never edit this
//! file to loosen it.

const SRC: &str = include_str!("../src/inactivity_store.rs");

/// Production code only: unit tests below also mention sweeps and budgets.
fn production() -> &'static str {
    SRC.split_once("#[cfg(test)]")
        .expect(
            "renegotiate: inactivity_store.rs production/test boundary moved; \
             update this guard on the card",
        )
        .0
}

#[test]
fn sweep_exposes_a_budgeted_resumable_pass() {
    let prod = production();
    for pinned in ["fn run_sweep_budgeted", "SweepBudget", "SweepCursor"] {
        assert!(
            prod.contains(pinned),
            "run_sweep still pages with no budget or resume cursor: add \
             run_sweep_budgeted with a SweepBudget (max_pages + deadline) that \
             returns a SweepCursor when more work may remain, so large \
             memberships complete across passes (missing: {pinned})"
        );
    }
}

#[test]
fn budgeted_pass_checks_pages_and_deadline() {
    let prod = production();
    let Some(at) = prod.find("fn run_sweep_budgeted") else {
        panic!(
            "run_sweep_budgeted is not implemented yet: the budgeted pass must \
             stop after max_pages selects and when the deadline passes"
        );
    };
    let window: Vec<&str> = prod[at..].lines().take(120).collect();
    for pinned in ["max_pages", "deadline"] {
        assert!(
            window.iter().any(|line| line.contains(pinned)),
            "renegotiate: run_sweep_budgeted does not reference {pinned}; the \
             budgeted pass must enforce both the page cap and the deadline"
        );
    }
}

#[test]
fn complete_sweep_and_select_shape_are_preserved() {
    let prod = production();
    for pinned in [
        "pub async fn run_sweep(",
        "SWEEP_PAGE_ROWS",
        "ORDER BY guild_id, member_id",
        "privacy_opt_outs",
        "written_set",
    ] {
        assert!(
            prod.contains(pinned),
            "renegotiate: {pinned} left the sweep; run_sweep keeps its complete \
             signature, the keyset/suppression select keeps its shape, and the \
             outcome keeps its write gate"
        );
    }
}
