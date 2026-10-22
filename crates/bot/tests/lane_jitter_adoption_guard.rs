//! Lane-jitter adoption guard: contended panel claims must sleep jittered backoffs.
//!
//! RED now (behavioral): both `PanelClaimResult::Busy` arms in the binary-only
//! `self_role_runtime` sleep the bare `LANE_BACKOFF` const, so every contender
//! retries in exact 100 ms lockstep. This guard compiles and runs on the
//! unmodified tree (genuine assertion RED, not a build failure), following the
//! `include_str!` guard precedent in `reconnect_adoption_guard.rs`.
//!
//! The seeded distribution itself is pinned in `lane_jitter_acceptance.rs`
//! (compile-RED until the Builder adds the library function). This file pins
//! adoption at both call sites plus the preserved semantics: 100 ms base,
//! 20 s deadline, `panel_busy` rejection. Offline: no database, no Discord,
//! no clock; this file itself never sleeps.
//!
//! Builder contract: route both Busy arms through
//! `two_bot::lane_jitter::lane_backoff_jittered` (see the acceptance file).
//! Vary the seed across retries so contenders spread. If an arm moves or the
//! contract needs to change, renegotiate on the card — never edit this file
//! to loosen it.

const SRC: &str = include_str!("../src/self_role_runtime.rs");

/// Production code only: the `#[path]` test modules below also mention panel
/// claims and must never satisfy (or trip) these pins.
fn production() -> &'static str {
    SRC.split_once("#[path = \"self_role_runtime_tests.rs\"]")
        .expect(
            "renegotiate: self_role_runtime.rs test-module marker moved; update this guard on the card",
        )
        .0
}

/// Line windows starting at each deadline-guarded Busy arm.
fn busy_arm_windows() -> Vec<Vec<&'static str>> {
    let prod = production();
    let mut windows = Vec::new();
    let mut rest = prod;
    while let Some(at) = rest.find("PanelClaimResult::Busy if") {
        let window: Vec<&str> = rest[at..].lines().take(14).collect();
        windows.push(window);
        rest = &rest[at + "PanelClaimResult::Busy if".len()..];
    }
    windows
}

#[test]
fn both_busy_arms_sleep_a_jittered_backoff() {
    let windows = busy_arm_windows();
    assert_eq!(
        windows.len(),
        2,
        "renegotiate: expected exactly the recover_terminal + prepare_audit Busy arms; update this guard on the card"
    );
    for (index, window) in windows.iter().enumerate() {
        let site = ["recover_terminal", "prepare_audit"][index];
        assert!(
            window.iter().any(|line| line.contains("sleep")),
            "{site}: Busy arm stopped sleeping entirely (hot loop): keep a backoff sleep"
        );
        assert!(
            window
                .iter()
                .any(|line| line.contains("lane_backoff_jittered")),
            "{site}: Busy arm still sleeps bare LANE_BACKOFF in exact 100 ms lockstep: \
             route it through two_bot::lane_jitter::lane_backoff_jittered with a \
             per-retry seed so contenders spread"
        );
    }
}

#[test]
fn lane_base_stays_one_hundred_ms() {
    assert!(
        production().contains("const LANE_BACKOFF: Duration = Duration::from_millis(100);"),
        "renegotiate: the LANE_BACKOFF base moved or was retuned; the jittered \
         sleeps must stay within the 100 ms base, update this guard on the card"
    );
}

#[test]
fn contention_stays_deadline_bounded_with_busy_rejection() {
    let prod = production();
    assert!(
        prod.contains("LANE_WAIT"),
        "renegotiate: the LANE_WAIT deadline is gone; contention must stay deadline-bounded"
    );
    assert!(
        prod.contains("panel_busy"),
        "renegotiate: the panel_busy rejection is gone; Busy/Rejected semantics must not change"
    );
}
