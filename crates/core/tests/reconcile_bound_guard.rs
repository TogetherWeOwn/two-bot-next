//! Reconcile-bound adoption guard: the boundary scan must stop at a page cap.
//!
//! RED now (behavioral): `deliver_reconcile` pages `channel_history` until the
//! marker is proven, the floor drops, or history runs out — a deep channel
//! pages past any bound. This guard compiles and runs on the unmodified tree
//! (genuine assertion RED, not a build failure), following the `include_str!`
//! guard precedent in `reconnect_adoption_guard.rs`.
//!
//! Exact stop/count semantics are pinned behaviorally in
//! `reconcile_bound_acceptance.rs` (DB-backed, `#[ignore]`). This file pins
//! adoption in the loop plus the preserved semantics: marker identity, floor
//! and empty-history outcomes, and the untouched Send dedup preflight.
//! Offline: no database, no Discord, no clock.
//!
//! Builder contract: give `deliver_reconcile` a documented `RECONCILE_MAX_PAGES`
//! cap (at most 16; see the acceptance file) and stop with the fixed
//! `DeliverOutcome::ScanBoundReached` marker when the cap is reached, leaving
//! the row reconcile-eligible for the next owner. If the function moves or the
//! contract needs to change, renegotiate on the card — never edit this file
//! to loosen it.

const SRC: &str = include_str!("../src/audit_service.rs");

/// The `deliver_reconcile` method body: from its marker to the next method.
fn reconcile_loop() -> &'static str {
    let start = SRC.find("async fn deliver_reconcile").expect(
        "renegotiate: deliver_reconcile moved in audit_service.rs; update this guard on the card",
    );
    let tail = &SRC[start + "async fn deliver_reconcile".len()..];
    match tail.find("\n    async fn ") {
        Some(end) => &SRC[start..start + "async fn deliver_reconcile".len() + end],
        None => &SRC[start..],
    }
}

#[test]
fn reconcile_scan_counts_pages_against_a_cap() {
    let window = reconcile_loop();
    assert!(
        window.contains("RECONCILE_MAX_PAGES"),
        "deliver_reconcile still pages channel_history with no cap: count pages \
         against a documented RECONCILE_MAX_PAGES (at most 16) so a deep channel \
         cannot page past any bound"
    );
}

#[test]
fn reconcile_cap_returns_the_fixed_marker() {
    let window = reconcile_loop();
    assert!(
        window.contains("ScanBoundReached"),
        "deliver_reconcile has no fixed stop marker: return \
         DeliverOutcome::ScanBoundReached when the page cap is reached, leaving \
         the row reconcile-eligible for the next owner (never quarantine a \
         merely-unscanned row)"
    );
}

#[test]
fn marker_floor_and_empty_outcomes_are_preserved() {
    let window = reconcile_loop();
    for pinned in [
        "find_mirror_in_page",
        "MarkerMissing",
        "validate_history_page",
        "snowflake_at_or_after",
    ] {
        assert!(
            window.contains(pinned),
            "renegotiate: {pinned} left deliver_reconcile; marker identity, floor \
             and empty-history outcomes must be preserved within the bound"
        );
    }
}

#[test]
fn send_dedup_preflight_is_untouched() {
    assert!(
        SRC.contains("DEDUP_PAGE_LIMIT"),
        "renegotiate: the Send dedup preflight changed; this slice bounds only \
         the Reconcile scan"
    );
}
