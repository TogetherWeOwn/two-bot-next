//! Timing-sleep removal acceptance (roadmap M2.4).
//!
//! Paired failing-tests card for `tests: replace timing sleeps with
//! deterministic sync`: every guard below FAILS while its target still polls
//! on a wall-clock sleep and passes once the builder rewires that site to a
//! deterministic sync (notify / waker / deadline poll on a shared lock). A
//! guard goes red again if the sleep ever returns, so the three sites stay
//! pinned without retries and without `#[ignore]` (house flake-ledger
//! policy).
//!
//! Targets, all test files only (no production code-path changes):
//!
//! 1. `alive.rs`: three 10 ms polls (`wait_http`, `checkpoint`, first-boot
//!    events tick) plus one fixed 100 ms delay after the first boot.
//! 2. `ops_backfill.rs`: 20 ms `thread::sleep` in the `spawn_bot` health
//!    poll.
//! 3. `preflight.rs`: the 429 / `retry_after` lane-hold tests, already
//!    sleep-free; the guard below locks that property plus the exact
//!    one-attempt-no-retry pins.
//!
//! Builder contract (renegotiate on the card, never by editing this file):
//! remove every `sleep(Duration::from_millis(..))` call from targets 1-2,
//! synchronize each site on one of [`SYNC_PRIMITIVES`], and keep every other
//! assertion in those files byte-identical. This file itself never sleeps.

use std::time::Duration;

const ALIVE: &str = include_str!("alive.rs");
const OPS_BACKFILL: &str = include_str!("ops_backfill.rs");
const PREFLIGHT: &str = include_str!("preflight.rs");

/// Deterministic-sync primitives the builder may synchronize on. `Notify` /
/// `notified()` covers tokio wakeups, `Waker` / `poll_fn` covers manual
/// polling, `yield_now` covers scheduler-driven re-polling with a deadline,
/// `Condvar` covers blocking threads, `watch::` covers state broadcast, and
/// `AtomicBool` covers flag handoff under a deadline.
const SYNC_PRIMITIVES: [&str; 8] = [
    "Notify",
    "notified()",
    "Waker",
    "poll_fn",
    "yield_now",
    "Condvar",
    "watch::",
    "AtomicBool",
];

fn has_sync_primitive(source: &str) -> bool {
    SYNC_PRIMITIVES.iter().any(|token| source.contains(token))
}

#[test]
fn alive_polling_uses_deterministic_sync_not_timing_sleeps() {
    assert!(
        !ALIVE.contains("sleep(Duration::from_millis"),
        "alive.rs still polls on a timing sleep; replace the 10 ms polls and \
         the 100 ms fixed delay with a Notify/waker/deadline poll on the \
         shared lock"
    );
    assert!(
        has_sync_primitive(ALIVE),
        "alive.rs must synchronize on a deterministic primitive ({SYNC_PRIMITIVES:?})"
    );
}

#[test]
fn ops_backfill_spawn_uses_deterministic_sync_not_thread_sleep() {
    assert!(
        !OPS_BACKFILL.contains("sleep(Duration::from_millis"),
        "ops_backfill.rs still polls health on thread::sleep(20 ms); replace \
         it with a Notify/Condvar/deadline poll on the shared lock"
    );
    assert!(
        has_sync_primitive(OPS_BACKFILL),
        "ops_backfill.rs must synchronize on a deterministic primitive ({SYNC_PRIMITIVES:?})"
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
