//! Bounded-backoff acceptance (roadmap M2.4).
//!
//! Paired acceptance for `tests: replace timing sleeps with bounded polling`:
//! every guard below FAILS while its target busy-spins on bare scheduler
//! yields or waits on an unbounded fixed delay, and passes once the site
//! polls with a bounded millisecond backoff under its deadline. A guard goes
//! red again if a long sleep returns or the backoff is dropped, so the sites
//! stay pinned without retries and without ignored tests (house flake-ledger
//! policy forbids both).
//!
//! Targets, all test files only (no production code-path changes):
//!
//! 1. `alive.rs`: three 10 ms polls (`wait_http`, `checkpoint`, first-boot
//!    events tick) each under the STEP deadline, plus a 200 ms post-ready
//!    settle window that polls liveness on a 10 ms backoff before SIGTERM.
//! 2. `ops_backfill.rs`: 20 ms blocking backoff in the `spawn_bot` health
//!    poll under its 15 s deadline.
//! 3. `preflight.rs`: the 429 / `retry_after` lane-hold tests, sleep-free;
//!    the guard below locks that property plus the exact
//!    one-attempt-no-retry pins.
//!
//! Builder contract (renegotiate on the card, never by editing this file to
//! loosen it): keep every poll backoff at or under 50 ms and under a
//! deadline, keep the post-ready settle window, and keep every other
//! assertion in those files byte-identical. This file itself never sleeps.

use std::time::Duration;

const ALIVE: &str = include_str!("alive.rs");
const OPS_BACKFILL: &str = include_str!("ops_backfill.rs");
const PREFLIGHT: &str = include_str!("preflight.rs");

/// Every `sleep(Duration::from_millis(N))` value in a target. Second-based
/// sleeps are rejected separately, so only millisecond backoffs are listed.
fn sleep_millis_values(source: &str) -> Vec<u64> {
    let needle = "sleep(Duration::from_millis(";
    let mut values = Vec::new();
    let mut rest = source;
    while let Some(idx) = rest.find(needle) {
        rest = &rest[idx + needle.len()..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(millis) = digits.parse::<u64>() {
            values.push(millis);
        }
    }
    values
}

/// Shared bound: at least one millisecond backoff exists, none exceeds the
/// 50 ms cap, and none sleeps in whole seconds. Per-attempt `timeout(...)`
/// values are not sleeps and are not capped here.
fn assert_bounded_backoff(source: &str, target: &str) {
    assert!(
        !source.contains("sleep(Duration::from_secs"),
        "{target} must not sleep in whole seconds; keep poll backoffs in milliseconds"
    );
    let values = sleep_millis_values(source);
    assert!(
        !values.is_empty(),
        "{target} must poll with a bounded millisecond backoff under its deadline"
    );
    for millis in &values {
        assert!(
            *millis <= 50,
            "{target} sleep of {millis} ms exceeds the 50 ms bounded-backoff cap"
        );
    }
}

#[test]
fn alive_polling_uses_bounded_backoff_under_deadline() {
    assert_bounded_backoff(ALIVE, "alive.rs");
    assert!(
        ALIVE.contains("STEP"),
        "alive.rs polls must stay under the STEP deadline"
    );
    assert!(
        ALIVE.contains("timeout("),
        "alive.rs polls must stay under a timeout deadline"
    );
    assert!(
        !ALIVE.contains("yield_now"),
        "alive.rs must not busy-spin on bare scheduler yields; use a bounded millisecond backoff"
    );
    assert!(
        ALIVE.contains("settle_until"),
        "alive.rs must keep the post-ready settle window that polls liveness before SIGTERM"
    );
}

#[test]
fn ops_backfill_spawn_uses_bounded_backoff_under_deadline() {
    assert_bounded_backoff(OPS_BACKFILL, "ops_backfill.rs");
    assert!(
        OPS_BACKFILL.contains("deadline"),
        "ops_backfill.rs health poll must stay under its deadline"
    );
    assert!(
        !OPS_BACKFILL.contains("yield_now"),
        "ops_backfill.rs must not busy-spin on bare scheduler yields; use a bounded millisecond backoff"
    );
}

#[test]
fn preflight_429_lane_hold_stays_sleep_free_with_one_attempt_pins() {
    assert!(
        !PREFLIGHT.contains("sleep("),
        "preflight.rs must keep its lane-hold tests free of timing sleeps"
    );
    assert!(
        PREFLIGHT.contains("lane held, no retry"),
        "preflight.rs must keep the lane-held-no-retry assertion on the \
         429/server-error budget reads"
    );
    assert!(
        PREFLIGHT.contains("mock.requests().len()"),
        "preflight.rs must keep the exact one-attempt request-count pins"
    );
}

/// The replacement pattern itself, with no sleep anywhere in this file: a
/// producer flips a shared lock and wakes a `Notify`; the consumer waits on
/// that wakeup under a deadline. If the notify were replaced by a timing
/// sleep, the consumer would hang to its deadline instead of waking early.
#[tokio::test]
async fn deterministic_notify_on_a_shared_lock_needs_no_sleep() {
    use std::sync::{Arc, Mutex};
    use tokio::sync::Notify;

    let ready = Arc::new(Mutex::new(false));
    let notify = Arc::new(Notify::new());
    let producer_ready = ready.clone();
    let producer_notify = notify.clone();
    tokio::spawn(async move {
        *producer_ready.lock().expect("shared readiness flag") = true;
        producer_notify.notify_one();
    });
    tokio::time::timeout(Duration::from_secs(5), notify.notified())
        .await
        .expect("deterministic wakeup before the deadline");
    assert!(*ready.lock().expect("shared readiness flag"));
}
