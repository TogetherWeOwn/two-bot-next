//! Acceptance tests: gateway supervised reconnect with bounded backoff.
//!
//! BUILDER CONTRACT: keep the policies, expected exits, and
//! behavior assertions below as the spec. The paused-time driver settles the
//! supervisor before each advance; timer gaps allow Tokio's 1ms resolution.
//!
//! Suite map (all offline: no DB, no live Discord, no wall-clock sleeps —
//! paused Tokio timers, notifications, and bounded yields only):
//!
//! - schedule pins (green now, guard the builder's edits): cap, jitter
//!   bounds, per-attempt doubling at windows derived independently of
//!   `reconnect_delay`, determinism, budget end, no-panic extremes, sane
//!   defaults.
//! - readyz pins (green now): backoff serves `starting` (never `ready`, never
//!   `parked`); a parked gateway still fails the strict gate.
//! - loop pins: a flapping gateway reconnects on the bounded schedule with
//!   timer-resolution-bounded gaps, and an always-failing gateway stops after
//!   exactly the budgeted attempts.
//! - shutdown pins: a pending shutdown ends the episode before any attempt, a
//!   SIGTERM during a backoff sleep preempts the sleep instead of waiting it
//!   out, a SIGTERM during a hanging connect preempts the connect without
//!   waiting for it, and a shutdown that is already ready never starts an
//!   attempt (the connector is built lazily inside the raced branch).
//! - draining pins: Draining published mid-episode (during the connect await
//!   or the backoff sleep) is never overwritten by the Connected/Armed
//!   publishes and ends the episode with no further attempts.

use super::*;
use crate::gateway_backoff::{
    reconnect_delay, run_supervised, AttemptOutcome, ReconnectPolicy, SupervisedExit,
};
use std::future::Future;
use two_bot_core::ComponentStatus;

const FLAP_SEED: u64 = 0xC0FFEE123456789;
const BUDGET_SEED: u64 = 0x5EED5EED5EED5EED;

fn flap_policy() -> ReconnectPolicy {
    ReconnectPolicy {
        base_delay: Duration::from_secs(1),
        max_delay: Duration::from_secs(8),
        max_attempts: 5,
        seed: FLAP_SEED,
    }
}

fn tiny_policy() -> ReconnectPolicy {
    ReconnectPolicy {
        base_delay: Duration::from_secs(1),
        max_delay: Duration::from_secs(8),
        max_attempts: 3,
        seed: BUDGET_SEED,
    }
}

/// Record of what one connector invocation saw: its index, the paused-clock
/// timestamp, and the gateway state readyz served at that moment.
struct AttemptRecord {
    attempt: u32,
    at: tokio::time::Instant,
    observed: GatewayState,
}

struct Script {
    records: Arc<std::sync::Mutex<Vec<AttemptRecord>>>,
    attempted: Arc<tokio::sync::Notify>,
    state: Arc<RwLock<GatewayState>>,
    failures_before_success: u32,
}

impl Script {
    fn new(state: Arc<RwLock<GatewayState>>, failures_before_success: u32) -> Self {
        Self {
            records: Arc::new(std::sync::Mutex::new(Vec::new())),
            attempted: Arc::new(tokio::sync::Notify::new()),
            state,
            failures_before_success,
        }
    }

    fn connector(
        &self,
    ) -> impl FnMut(u32) -> Box<dyn Future<Output = AttemptOutcome> + Send> + Send {
        let records = Arc::clone(&self.records);
        let attempted = Arc::clone(&self.attempted);
        let state = Arc::clone(&self.state);
        let failures = self.failures_before_success;
        move |attempt: u32| {
            let records = Arc::clone(&records);
            let attempted = Arc::clone(&attempted);
            let state = Arc::clone(&state);
            Box::new(async move {
                // The runner never holds the state lock across `connect`, so a
                // plain read here observes exactly what readyz served.
                let observed = *state.read().await;
                records.lock().expect("attempt log").push(AttemptRecord {
                    attempt,
                    at: tokio::time::Instant::now(),
                    observed,
                });
                attempted.notify_one();
                if attempt < failures {
                    AttemptOutcome::Retryable
                } else {
                    AttemptOutcome::Connected
                }
            }) as Box<dyn Future<Output = AttemptOutcome> + Send>
        }
    }

    fn attempts(&self) -> u32 {
        self.records.lock().expect("attempt log").len() as u32
    }
}

/// Drive an episode to completion on paused time with no open-ended waits:
/// settle the spawned task until each attempt is recorded, then advance only
/// that attempt's delay. Bounded yields make an unbounded retry loop fail here
/// instead of hanging CI.
async fn drive_to_exit(
    state: Arc<RwLock<GatewayState>>,
    policy: ReconnectPolicy,
    script: &Script,
) -> SupervisedExit {
    let mut supervised = tokio::spawn(run_supervised(
        Arc::clone(&state),
        policy,
        script.connector(),
        std::future::pending(),
    ));
    for attempt in 0..policy.max_attempts {
        let next_attempt = script.attempted.notified();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), next_attempt)
                .await
                .is_ok(),
            "supervisor must reach each attempt within a bounded paused-time wait"
        );
        assert_eq!(script.attempts(), attempt + 1);
        // The attempt record is written immediately before the connector
        // returns. Let the supervisor consume that outcome and register its
        // sleep before advancing paused time.
        for _ in 0..10 {
            if supervised.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        if supervised.is_finished() {
            break;
        }
        let delay = reconnect_delay(&policy, attempt).expect("attempt is within budget");
        tokio::time::advance(delay).await;
    }
    let result = tokio::time::timeout(Duration::from_secs(1), &mut supervised)
        .await
        .expect("supervisor must terminate within its bounded budget");
    result.expect("supervisor task")
}

fn assert_gap_within_timer_resolution(gap: Duration, expected: Duration, attempt: usize) {
    let difference = gap.abs_diff(expected);
    assert!(
        difference <= Duration::from_millis(1),
        "gap before attempt {} was {gap:?}, expected {expected:?} within Tokio's 1ms timer resolution",
        attempt + 1
    );
}

fn records(script: &Script) -> Vec<(u32, tokio::time::Instant, GatewayState)> {
    script
        .records
        .lock()
        .expect("attempt log")
        .iter()
        .map(|record| (record.attempt, record.at, record.observed))
        .collect()
}

#[test]
fn reconnect_schedule_never_exceeds_the_cap() {
    let policy = ReconnectPolicy {
        max_attempts: 100,
        seed: 7,
        ..ReconnectPolicy::default()
    };
    for attempt in 0..policy.max_attempts {
        let delay = reconnect_delay(&policy, attempt).expect("within budget");
        assert!(
            delay <= policy.max_delay,
            "attempt {attempt}: {delay:?} exceeds cap {:?}",
            policy.max_delay
        );
    }
}

#[test]
fn reconnect_schedule_carries_bounded_jitter() {
    // base 4s: grown is exactly 4s at attempt 0, so equal jitter pins [2s, 4s].
    let policy = ReconnectPolicy {
        base_delay: Duration::from_secs(4),
        max_delay: Duration::from_secs(3600),
        max_attempts: 4,
        seed: 11,
    };
    let delay = reconnect_delay(&policy, 0).expect("within budget");
    assert!(
        delay >= Duration::from_secs(2) && delay <= Duration::from_secs(4),
        "attempt 0 must lie in [grown/2, grown], got {delay:?}"
    );
    // Past the cap the window pins to [cap/2, cap], never above the cap.
    let policy = ReconnectPolicy {
        base_delay: Duration::from_secs(64),
        max_delay: Duration::from_secs(30),
        max_attempts: 4,
        seed: 11,
    };
    let delay = reconnect_delay(&policy, 0).expect("within budget");
    assert!(
        delay >= Duration::from_secs(15) && delay <= Duration::from_secs(30),
        "capped delay must lie in [cap/2, cap], got {delay:?}"
    );
}

#[test]
fn reconnect_schedule_doubles_per_attempt() {
    // Windows are derived by hand from "base * 2^attempt, equal jitter in
    // [grown/2, grown]" — never from reconnect_delay itself — so a schedule
    // that fails to grow (grown = base on every attempt) lands in [2s, 4s]
    // at attempts 1 and 2 and fails both windows below.
    let policy = ReconnectPolicy {
        base_delay: Duration::from_secs(4),
        max_delay: Duration::from_secs(3600),
        max_attempts: 4,
        seed: 11,
    };
    let first = reconnect_delay(&policy, 1).expect("within budget");
    assert!(
        first >= Duration::from_secs(4) && first <= Duration::from_secs(8),
        "attempt 1 must lie in [grown/2, grown] for grown 8s, got {first:?}"
    );
    let second = reconnect_delay(&policy, 2).expect("within budget");
    assert!(
        second >= Duration::from_secs(8) && second <= Duration::from_secs(16),
        "attempt 2 must lie in [grown/2, grown] for grown 16s, got {second:?}"
    );
}

#[test]
fn reconnect_schedule_is_deterministic_for_a_seed() {
    let policy = flap_policy();
    for attempt in 0..policy.max_attempts {
        assert_eq!(
            reconnect_delay(&policy, attempt),
            reconnect_delay(&policy, attempt),
            "same seed must replay the same delay"
        );
    }
}

#[test]
fn reconnect_delays_vary_with_seed() {
    // Decorrelation is the point of jitter: two seeds must not march in lockstep.
    let left = ReconnectPolicy {
        seed: 1,
        ..flap_policy()
    };
    let right = ReconnectPolicy {
        seed: 2,
        ..flap_policy()
    };
    let differing = (0..24u32)
        .filter(|attempt| reconnect_delay(&left, *attempt) != reconnect_delay(&right, *attempt))
        .count();
    assert!(
        differing > 0,
        "different seeds must decorrelate at least one delay"
    );
}

#[test]
fn reconnect_budget_ends_retry() {
    let policy = tiny_policy();
    assert!(reconnect_delay(&policy, policy.max_attempts - 1).is_some());
    assert_eq!(reconnect_delay(&policy, policy.max_attempts), None);
    assert_eq!(reconnect_delay(&policy, u32::MAX), None);
}

#[test]
fn reconnect_delay_never_panics_on_extreme_attempts() {
    let unbounded = ReconnectPolicy {
        max_attempts: u32::MAX,
        ..ReconnectPolicy::default()
    };
    for attempt in [0, 1, 30, 31, 32, 100, u32::MAX - 1] {
        let delay = reconnect_delay(&unbounded, attempt).expect("within budget");
        assert!(
            delay <= unbounded.max_delay,
            "attempt {attempt}: {delay:?} exceeds cap"
        );
    }
}

#[test]
fn default_policy_is_sane() {
    let policy = ReconnectPolicy::default();
    assert!(policy.base_delay > Duration::ZERO);
    assert!(policy.base_delay <= policy.max_delay);
    assert!(policy.max_attempts > 0);
    assert!(reconnect_delay(&policy, 0).is_some());
}

#[tokio::test]
async fn backoff_state_reports_starting_never_ready_nor_parked() {
    // During backoff the supervisor holds Armed: readyz must serve 503 with
    // gateway=starting — the strict gate treats Starting as not
    // ready, and it must never read as Connected (ready) or Unconfigured
    // (parked/down).
    assert_eq!(GatewayState::Armed.status(), ComponentStatus::Starting);
    let gateway = Arc::new(RwLock::new(GatewayState::Armed));
    let shared = crate::server::SharedState::new(Arc::clone(&gateway), None);
    let jobs = crate::jobs::statuses(&[], true);
    let (code, report) = crate::server::readiness(&shared, &jobs, false).await;
    assert_eq!(code, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    let gateway_status = report
        .health
        .components
        .iter()
        .find(|(name, _)| name == "gateway")
        .expect("readyz names the gateway component")
        .1;
    assert_eq!(gateway_status, ComponentStatus::Starting);
    assert!(
        !report.health.ready(),
        "starting must fail the strict readyz gate"
    );
}

#[tokio::test]
async fn parked_gateway_still_fails_the_strict_readyz_gate() {
    // Missing prerequisites park the gateway as
    // Unconfigured (down), and the strict gate keeps failing. The supervised
    // reconnect must not blur this into a retrying state.
    assert_eq!(GatewayState::Unconfigured.status(), ComponentStatus::Down);
    let gateway = Arc::new(RwLock::new(GatewayState::Unconfigured));
    let shared = crate::server::SharedState::new(gateway, None);
    let jobs = crate::jobs::statuses(&[], true);
    let (code, report) = crate::server::readiness(&shared, &jobs, false).await;
    assert_eq!(code, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    let gateway_status = report
        .health
        .components
        .iter()
        .find(|(name, _)| name == "gateway")
        .expect("readyz names the gateway component")
        .1;
    assert_eq!(gateway_status, ComponentStatus::Down);
    assert!(
        !report.health.ready(),
        "parked must fail the strict readyz gate"
    );
}

#[tokio::test(start_paused = true)]
async fn supervisor_retries_a_flapping_gateway_on_the_bounded_schedule() {
    // Two transient failures, then recovery: the supervisor must attempt
    // exactly 3 times, serve Starting between attempts, space the attempts
    // within timer resolution of the scheduled delays, and publish Connected.
    let policy = flap_policy();
    let state = Arc::new(RwLock::new(GatewayState::Connected));
    let script = Script::new(Arc::clone(&state), 2);
    let exit = drive_to_exit(Arc::clone(&state), policy, &script).await;
    assert_eq!(exit, SupervisedExit::Reconnected);
    assert_eq!(
        script.attempts(),
        3,
        "two failures then recovery is 3 attempts"
    );
    let history = records(&script);
    assert_eq!(
        history
            .iter()
            .map(|(attempt, _, _)| *attempt)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(
        history
            .iter()
            .map(|(_, _, observed)| *observed)
            .collect::<Vec<_>>(),
        vec![
            GatewayState::Connected,
            GatewayState::Armed,
            GatewayState::Armed
        ],
        "backoff attempts must observe starting, never ready or parked"
    );
    for (index, window) in history.windows(2).enumerate() {
        let gap = window[1].1 - window[0].1;
        let expected = reconnect_delay(&policy, index as u32).expect("within budget");
        assert_gap_within_timer_resolution(gap, expected, index);
    }
    assert_eq!(*state.read().await, GatewayState::Connected);
}

#[tokio::test(start_paused = true)]
async fn supervisor_stops_after_the_bounded_budget() {
    // An always-failing gateway must stop after exactly max_attempts attempts
    // and fail readyz closed — bounded retries, never a hot loop.
    let policy = tiny_policy();
    let state = Arc::new(RwLock::new(GatewayState::Connected));
    let script = Script::new(Arc::clone(&state), u32::MAX);
    let exit = drive_to_exit(Arc::clone(&state), policy, &script).await;
    assert_eq!(exit, SupervisedExit::BudgetExhausted);
    assert_eq!(
        script.attempts(),
        policy.max_attempts,
        "an always-failing gateway stops after exactly the budgeted attempts"
    );
    let history = records(&script);
    assert_eq!(
        history
            .iter()
            .map(|(_, _, observed)| *observed)
            .collect::<Vec<_>>(),
        vec![
            GatewayState::Connected,
            GatewayState::Armed,
            GatewayState::Armed
        ]
    );
    for (index, window) in history.windows(2).enumerate() {
        let gap = window[1].1 - window[0].1;
        let expected = reconnect_delay(&policy, index as u32).expect("within budget");
        assert_gap_within_timer_resolution(gap, expected, index);
    }
    assert_eq!(*state.read().await, GatewayState::Draining);
}

#[tokio::test(start_paused = true)]
async fn shutdown_wins_before_any_attempt() {
    let state = Arc::new(RwLock::new(GatewayState::Connected));
    let script = Script::new(Arc::clone(&state), 0);
    let supervised = tokio::spawn(run_supervised(
        Arc::clone(&state),
        tiny_policy(),
        script.connector(),
        std::future::ready(()),
    ));
    for _ in 0..100 {
        if supervised.is_finished() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        supervised.is_finished(),
        "shutdown must end the episode promptly"
    );
    assert_eq!(
        supervised.await.expect("supervisor task"),
        SupervisedExit::ShutdownRequested
    );
    assert_eq!(script.attempts(), 0, "no attempt after shutdown");
}

#[tokio::test(start_paused = true)]
async fn shutdown_during_backoff_sleep_ends_episode_without_waiting_out_the_delay() {
    // SIGTERM during a backoff must end the episode immediately, not after
    // the full sleep: the sleep is raced against shutdown (biased), the same
    // "SIGTERM never waits" shape as publish_gateway_failure in main.rs.
    let policy = ReconnectPolicy {
        base_delay: Duration::from_secs(30),
        max_delay: Duration::from_secs(30),
        max_attempts: 5,
        seed: FLAP_SEED,
    };
    let delay = reconnect_delay(&policy, 0).expect("attempt is within budget");
    assert!(
        delay >= Duration::from_secs(15),
        "the skipped wait must be meaningful, got {delay:?}"
    );
    let state = Arc::new(RwLock::new(GatewayState::Connected));
    let script = Script::new(Arc::clone(&state), u32::MAX);
    let shutdown_signal = Arc::new(tokio::sync::Notify::new());
    let shutdown_waiter = Arc::clone(&shutdown_signal);
    let supervised = tokio::spawn(run_supervised(
        Arc::clone(&state),
        policy,
        script.connector(),
        async move {
            shutdown_waiter.notified().await;
        },
    ));
    let next_attempt = script.attempted.notified();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), next_attempt)
            .await
            .is_ok(),
        "supervisor must reach the first attempt within a bounded wait"
    );
    for _ in 0..10 {
        if supervised.is_finished() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        *state.read().await,
        GatewayState::Armed,
        "supervisor must be inside the backoff before shutdown fires"
    );
    let before = tokio::time::Instant::now();
    shutdown_signal.notify_one();
    for _ in 0..100 {
        if supervised.is_finished() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        supervised.is_finished(),
        "shutdown during backoff must end the episode without waiting out the full sleep"
    );
    assert_eq!(
        supervised.await.expect("supervisor task"),
        SupervisedExit::ShutdownRequested
    );
    assert_eq!(script.attempts(), 1, "no attempt may run after shutdown");
    let elapsed = tokio::time::Instant::now() - before;
    assert!(
        elapsed < delay,
        "episode waited {elapsed:?}; shutdown must preempt the {delay:?} backoff sleep"
    );
    assert_eq!(
        *state.read().await,
        GatewayState::Armed,
        "shutdown leaves the backoff state for its owner to drain"
    );
}

#[tokio::test(start_paused = true)]
async fn shutdown_during_hanging_connect_ends_episode_without_waiting_for_it() {
    // A hanging connect must not block shutdown: the connect is raced against
    // shutdown (biased, shutdown first), so a SIGTERM mid-connect ends the
    // episode promptly instead of waiting for the connector. A plain
    // `connect(attempt).await` hangs here and fails the prompt-finish assert.
    // The invocation counter lives outside the returned future so the test
    // also proves the hanging attempt actually started (exactly one).
    let policy = tiny_policy();
    let state = Arc::new(RwLock::new(GatewayState::Connected));
    let invocations = Arc::new(std::sync::Mutex::new(0_u32));
    let counting = Arc::clone(&invocations);
    let shutdown_signal = Arc::new(tokio::sync::Notify::new());
    let shutdown_waiter = Arc::clone(&shutdown_signal);
    let supervised = tokio::spawn(run_supervised(
        Arc::clone(&state),
        policy,
        move |_| {
            *counting.lock().expect("invocation counter") += 1;
            Box::new(std::future::pending()) as Box<dyn Future<Output = AttemptOutcome> + Send>
        },
        async move {
            shutdown_waiter.notified().await;
        },
    ));
    for _ in 0..100 {
        if *invocations.lock().expect("invocation counter") == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        *invocations.lock().expect("invocation counter"),
        1,
        "supervisor must start exactly one hanging attempt before shutdown fires"
    );
    // Let the supervisor enter the connect-vs-shutdown race before firing.
    for _ in 0..10 {
        if supervised.is_finished() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        !supervised.is_finished(),
        "a hanging connect must not finish on its own"
    );
    let before = tokio::time::Instant::now();
    shutdown_signal.notify_one();
    for _ in 0..100 {
        if supervised.is_finished() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        supervised.is_finished(),
        "shutdown during a hanging connect must end the episode promptly"
    );
    assert_eq!(
        supervised.await.expect("supervisor task"),
        SupervisedExit::ShutdownRequested
    );
    assert_eq!(
        *invocations.lock().expect("invocation counter"),
        1,
        "shutdown must win without starting a spurious second attempt"
    );
    let elapsed = tokio::time::Instant::now() - before;
    assert_eq!(
        elapsed,
        Duration::ZERO,
        "preempting a hanging connect must not advance the paused clock, got {elapsed:?}"
    );
    // No outcome was ever reported, so no publish ran: the state is untouched.
    assert_eq!(*state.read().await, GatewayState::Connected);
}

#[tokio::test(start_paused = true)]
async fn shutdown_ready_before_connect_starts_no_attempt() {
    // The connector is built lazily inside the raced branch: a shutdown that
    // is already ready when the connect race is entered never invokes the
    // connector. Building `connect(attempt)` eagerly while entering `select!`
    // constructs (and drops unpolled) one spurious attempt and fails the
    // zero-invocation assert below.
    let policy = tiny_policy();
    let state = Arc::new(RwLock::new(GatewayState::Connected));
    let invocations = Arc::new(std::sync::Mutex::new(0_u32));
    let counting = Arc::clone(&invocations);
    let shutdown_signal = Arc::new(tokio::sync::Notify::new());
    let shutdown_waiter = Arc::clone(&shutdown_signal);
    // Hold the state write lock so the supervisor blocks on its Draining
    // check after the pre-attempt shutdown poll: firing shutdown while it is
    // blocked guarantees shutdown is already ready when the connect race is
    // entered, with no timing race either way.
    let guard = state.write().await;
    let supervised = tokio::spawn(run_supervised(
        Arc::clone(&state),
        policy,
        move |_| {
            *counting.lock().expect("invocation counter") += 1;
            Box::new(std::future::pending()) as Box<dyn Future<Output = AttemptOutcome> + Send>
        },
        async move {
            shutdown_waiter.notified().await;
        },
    ));
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(
        !supervised.is_finished(),
        "supervisor must block on the held state lock"
    );
    assert_eq!(
        *invocations.lock().expect("invocation counter"),
        0,
        "no attempt may start while the supervisor is still on its Draining check"
    );
    shutdown_signal.notify_one();
    drop(guard);
    for _ in 0..100 {
        if supervised.is_finished() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        supervised.is_finished(),
        "a ready shutdown must end the episode without starting an attempt"
    );
    assert_eq!(
        supervised.await.expect("supervisor task"),
        SupervisedExit::ShutdownRequested
    );
    assert_eq!(
        *invocations.lock().expect("invocation counter"),
        0,
        "a shutdown that wins the connect race must never invoke the connector"
    );
    assert_eq!(*state.read().await, GatewayState::Connected);
}

/// Connector that stands in for the outside drain owner: it publishes
/// Draining during the supervisor's unlocked connect await (the race the
/// publish guards exist for), then reports `outcome`.
fn draining_connector(
    state: Arc<RwLock<GatewayState>>,
    attempts: Arc<std::sync::Mutex<u32>>,
    outcome: AttemptOutcome,
) -> impl FnMut(u32) -> Box<dyn Future<Output = AttemptOutcome> + Send> {
    move |_| {
        let state = Arc::clone(&state);
        let attempts = Arc::clone(&attempts);
        Box::new(async move {
            *attempts.lock().expect("attempt counter") += 1;
            *state.write().await = GatewayState::Draining;
            outcome
        }) as Box<dyn Future<Output = AttemptOutcome> + Send>
    }
}

#[tokio::test]
async fn draining_during_connect_is_never_overwritten_by_connected() {
    // Draining begins while the connect runs; the Connected publish must not
    // resurrect readyz to ready, and the episode ends without claiming a
    // reconnect the drain has already superseded.
    let state = Arc::new(RwLock::new(GatewayState::Connected));
    let attempts = Arc::new(std::sync::Mutex::new(0_u32));
    let exit = run_supervised(
        Arc::clone(&state),
        tiny_policy(),
        draining_connector(
            Arc::clone(&state),
            Arc::clone(&attempts),
            AttemptOutcome::Connected,
        ),
        std::future::pending(),
    )
    .await;
    assert_eq!(exit, SupervisedExit::ShutdownRequested);
    assert_eq!(*attempts.lock().expect("attempt counter"), 1);
    assert_eq!(
        *state.read().await,
        GatewayState::Draining,
        "Connected publish must never overwrite Draining"
    );
}

#[tokio::test]
async fn draining_during_final_retryable_connect_returns_shutdown_requested() {
    let mut policy = tiny_policy();
    policy.max_attempts = 1;
    let state = Arc::new(RwLock::new(GatewayState::Connected));
    let attempts = Arc::new(std::sync::Mutex::new(0_u32));
    let exit = run_supervised(
        Arc::clone(&state),
        policy,
        draining_connector(
            Arc::clone(&state),
            Arc::clone(&attempts),
            AttemptOutcome::Retryable,
        ),
        std::future::pending(),
    )
    .await;
    assert_eq!(exit, SupervisedExit::ShutdownRequested);
    assert_eq!(*attempts.lock().expect("attempt counter"), 1);
    assert_eq!(*state.read().await, GatewayState::Draining);
}

#[tokio::test]
async fn draining_during_connect_is_never_overwritten_by_armed() {
    // Same race on the retry path: the Armed publish must not resurrect
    // readyz to starting, and no further attempt runs into the drain.
    let state = Arc::new(RwLock::new(GatewayState::Connected));
    let attempts = Arc::new(std::sync::Mutex::new(0_u32));
    let exit = run_supervised(
        Arc::clone(&state),
        tiny_policy(),
        draining_connector(
            Arc::clone(&state),
            Arc::clone(&attempts),
            AttemptOutcome::Retryable,
        ),
        std::future::pending(),
    )
    .await;
    assert_eq!(exit, SupervisedExit::ShutdownRequested);
    assert_eq!(*attempts.lock().expect("attempt counter"), 1);
    assert_eq!(
        *state.read().await,
        GatewayState::Draining,
        "Armed publish must never overwrite Draining"
    );
}

#[tokio::test(start_paused = true)]
async fn draining_during_backoff_sleep_ends_the_episode_before_the_next_attempt() {
    // First attempt fails and the supervisor backs off (Armed). Draining
    // begins during that sleep: when the timer fires the episode must end
    // instead of making another attempt or publishing over Draining.
    let policy = tiny_policy();
    let state = Arc::new(RwLock::new(GatewayState::Connected));
    let script = Script::new(Arc::clone(&state), u32::MAX);
    let supervised = tokio::spawn(run_supervised(
        Arc::clone(&state),
        policy,
        script.connector(),
        std::future::pending(),
    ));
    let next_attempt = script.attempted.notified();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), next_attempt)
            .await
            .is_ok(),
        "supervisor must reach the first attempt within a bounded wait"
    );
    for _ in 0..10 {
        if supervised.is_finished() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        *state.read().await,
        GatewayState::Armed,
        "supervisor must be inside the backoff before draining begins"
    );
    *state.write().await = GatewayState::Draining;
    let delay = reconnect_delay(&policy, 0).expect("attempt is within budget");
    tokio::time::advance(delay).await;
    let exit = tokio::time::timeout(Duration::from_secs(1), supervised)
        .await
        .expect("episode must end when the backoff timer fires")
        .expect("supervisor task");
    assert_eq!(exit, SupervisedExit::ShutdownRequested);
    assert_eq!(script.attempts(), 1, "no attempt may run after draining");
    assert_eq!(*state.read().await, GatewayState::Draining);
}
