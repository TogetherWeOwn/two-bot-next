//! Loop-adoption guard: the Reconnect arm sleeps (RED now — behavioral).
//!
//! RED now: the gateway receive loop's `Reconnect` arm is `warn!` + bare
//! `continue`, so back-to-back Reconnect errors hot-loop reception. This
//! source guard fails until the Builder routes the arm through the bounded
//! reconnect schedule. It lives in its own file (separate from the
//! compile-RED `jitter_backoff_acceptance.rs`) so it compiles and runs on the
//! unmodified tree: genuine behavioral RED, not a build failure.
//!
//! Follows the `include_str!` guard precedent in `timing_sleep_removal.rs`;
//! this file itself never sleeps. Offline: no database, no Discord, no clock.
//!
//! Builder contract: route the arm through the bounded schedule (sleep /
//! `reconnect_delay` / `run_supervised`). If the arm shape needs to change,
//! renegotiate on the card — never edit this file to loosen it.

const GATEWAY_SRC: &str = include_str!("../src/gateway.rs");

#[test]
fn reconnect_arm_sleeps_instead_of_bare_continue() {
    // Scan a line window past the arm marker so reformatting does not trip
    // the guard; if the marker itself moved, the expect below fails loudly
    // (renegotiate) instead of passing silently.
    let marker = "ReceiveMessageErrorType::Reconnect";
    let at = GATEWAY_SRC.find(marker).expect(
        "renegotiate: Reconnect arm marker moved in gateway.rs; update this guard on the card",
    );
    let window: Vec<&str> = GATEWAY_SRC[at..].lines().take(30).collect();
    assert!(
        window.iter().any(|line| line.contains("continue")),
        "renegotiate: Reconnect arm shape changed in gateway.rs; update this guard on the card"
    );
    let adopted = window.iter().any(|line| {
        line.contains("sleep")
            || line.contains("reconnect_delay")
            || line.contains("run_supervised")
            || line.contains("gateway_backoff")
    });
    assert!(
        adopted,
        "Reconnect arm still bare `continue`: route it through the bounded reconnect \
         schedule (sleep / reconnect_delay / run_supervised) so a Reconnect storm \
         cannot spin the shard's hottest loop"
    );
}
