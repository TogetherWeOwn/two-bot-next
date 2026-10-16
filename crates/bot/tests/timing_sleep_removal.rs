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

fn function_body(source: &str, name: &str) -> String {
    let signature = source
        .find(&format!("fn {name}("))
        .unwrap_or_else(|| panic!("missing function {name}"));
    let body_start = source[signature..]
        .find('{')
        .map(|offset| signature + offset)
        .unwrap_or_else(|| panic!("missing body for function {name}"));
    let mut depth = 0;
    for (offset, byte) in source[body_start..].bytes().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return source[body_start..=body_start + offset].to_owned();
                }
            }
            _ => {}
        }
    }
    panic!("unterminated body for function {name}");
}

fn source_between(source: &str, start: &str, end: &str) -> String {
    let start = source
        .find(start)
        .unwrap_or_else(|| panic!("missing source boundary {start:?}"));
    let end = source[start..]
        .find(end)
        .map(|offset| start + offset)
        .unwrap_or_else(|| panic!("missing source boundary {end:?}"));
    source[start..end].to_owned()
}

fn assert_poll_site(source: &str, expected_sleep: &str) {
    assert_eq!(
        source.matches("sleep(").count(),
        1,
        "each poll site must contain exactly one sleep call"
    );
    assert_eq!(
        source.matches(expected_sleep).count(),
        1,
        "each poll site must use its literal bounded backoff"
    );
}

fn assert_settle_window(source: &str) {
    assert!(
        source.contains("Duration::from_millis(200)"),
        "post-ready settle window must remain 200 ms"
    );
    assert!(
        source.contains("while tokio::time::Instant::now() < settle_until"),
        "post-ready settle window must poll until its deadline"
    );
    assert_eq!(
        source.matches("bot.assert_alive();").count(),
        2,
        "settle loop must assert liveness inside and after the window"
    );
    assert_poll_site(source, "sleep(Duration::from_millis(10)).await;");
}

#[test]
fn alive_polling_uses_bounded_backoff_under_deadline() {
    let wait_http = function_body(ALIVE, "wait_http");
    assert_poll_site(&wait_http, "sleep(Duration::from_millis(10)).await;");
    assert!(
        wait_http.contains("timeout(STEP"),
        "wait_http must retain its STEP deadline"
    );

    let checkpoint = function_body(ALIVE, "checkpoint");
    assert_poll_site(&checkpoint, "sleep(Duration::from_millis(10)).await;");
    assert!(
        checkpoint.contains("timeout(STEP"),
        "checkpoint must retain its STEP deadline"
    );

    let lifecycle = function_body(ALIVE, "lifecycle");
    let events_tick = source_between(&lifecycle, "let ticked = timeout(STEP", "ticked.is_ok()");
    assert_poll_site(&events_tick, "sleep(Duration::from_millis(10)).await;");
    let settle = source_between(&lifecycle, "let settle_until =", "assert_ready_correlation");
    assert_settle_window(&settle);

    assert!(
        !ALIVE.contains("yield_now"),
        "alive.rs must not busy-spin on bare scheduler yields"
    );
}

#[test]
fn ops_backfill_spawn_uses_bounded_backoff_under_deadline() {
    let spawn_bot = function_body(OPS_BACKFILL, "spawn_bot");
    assert_poll_site(&spawn_bot, "thread::sleep(Duration::from_millis(20));");
    assert!(
        spawn_bot.contains("deadline"),
        "ops_backfill.rs health poll must stay under its deadline"
    );
    assert!(
        !spawn_bot.contains("yield_now"),
        "ops_backfill.rs must not busy-spin on bare scheduler yields"
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

#[test]
fn polling_guard_rejects_missing_or_unbounded_site_backoffs() {
    let wait_http = function_body(ALIVE, "wait_http");
    assert_poll_site(&wait_http, "sleep(Duration::from_millis(10)).await;");
    assert!(std::panic::catch_unwind(|| {
        assert_poll_site(
            &wait_http.replace("sleep(Duration::from_millis(10)).await;", ""),
            "sleep(Duration::from_millis(10)).await;",
        );
    })
    .is_err());
    assert!(std::panic::catch_unwind(|| {
        assert_poll_site(
            &wait_http.replace(
                "sleep(Duration::from_millis(10)).await;",
                "sleep(Duration::from_millis(backoff_ms)).await;",
            ),
            "sleep(Duration::from_millis(10)).await;",
        );
    })
    .is_err());
    assert!(std::panic::catch_unwind(|| {
        assert_poll_site(
            &wait_http.replace(
                "sleep(Duration::from_millis(10)).await;",
                "sleep(Duration::from_secs(1)).await;",
            ),
            "sleep(Duration::from_millis(10)).await;",
        );
    })
    .is_err());

    let lifecycle = function_body(ALIVE, "lifecycle");
    let settle = source_between(&lifecycle, "let settle_until =", "assert_ready_correlation");
    assert_settle_window(&settle);
    assert!(std::panic::catch_unwind(|| {
        assert_settle_window(
            &settle.replace("Duration::from_millis(200)", "Duration::from_millis(0)"),
        );
    })
    .is_err());
    assert!(std::panic::catch_unwind(|| {
        assert_settle_window(
            &settle.replace("bot.assert_alive();", "// liveness assertion removed"),
        );
    })
    .is_err());
}
