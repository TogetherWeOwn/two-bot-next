#![cfg(test)]
//! Acceptance: `JobGate` single-flight around the Events tick.
//!
//! Failing-first contract for wiring the existing `JobGate` around the whole
//! Events tick in the scheduler: overlapping ticks must skip (the second
//! tick issues no fetch, publishes nothing, queues behind nothing and
//! returns no error) while sequential ticks are unaffected, the gate itself
//! is untouched, and cadence plus job registration are unchanged.
//!
//! RED NOW (baseline): `Kind::run_events_tick` does not exist yet, so every
//! call below resolves to the test-local `BaselineEventsTick` adapter,
//! which replays the current unwired behavior (straight to the events
//! fetch, gate ignored). The overlap test fails on the fetch count (2
//! scheduled-events GETs, expected 1) and on the first tick's result.
//!
//! GREEN (builder): add ONE inherent method with EXACTLY this signature:
//! ```ignore
//! impl Kind {
//!     pub async fn run_events_tick(
//!         pool: &PgPool,
//!         rest: &ActionExecutor,
//!         guild: &str,
//!         observation: &Mutex<()>,
//!         shutdown: &watch::Receiver<bool>,
//!         rank_heal: bool,
//!         gate: &JobGate,
//!     ) -> Result<(), ErrorClass>;
//! }
//! ```
//! The inherent tries the gate first and returns `Ok(())` at once on
//! failure (skip: no fetch, no publish, no queue, no error); otherwise it
//! runs the real Events tick with the guard held across it. Delegating to
//! the unchanged `snapshot_once` keeps every existing test compiling.
//! Inherent methods shadow trait methods, so these calls flip from the
//! adapter to the wired tick with no test edit. Then point the scheduler's
//! Events closure at the new method; that one-line swap is review-verified
//! (it is observationally quiet under today's supervisor, which already
//! discards busy deadlines — these tests pin the tick-level mechanism for
//! direct or concurrent callers, which is what the tick recipe documents).
//!
//! Shapes accepted: only this inherent method (exact name and signature). A
//! second gate-check on the same gate along the delegated path self-skips
//! every tick; the sequential test catches that (0 fetches, expected 2).
//!
//! Seams this file depends on (builder: preserve them, or return corrections
//! to the test author): `MockRest::start`, `start_gated_at`, `requests` and
//! `shutdown`; `ActionExecutor::with_proxy` and `get_json`;
//! `admission_pool_with_tls`; the three interval constants; `NAMES`;
//! `settings_jobs::NAME`; `JobGate::try_acquire` (frozen: the gate itself
//! must not change).
//!
//! The existing `job_gate_is_single_flight` unit test is the
//! no-`JobGate`-behavior-change pin: it is untouched and must stay green.
//! `baseline_adapter_keeps_the_unwired_path_callable` is load-bearing for
//! CI: its explicit `<Kind as BaselineEventsTick>::` call keeps the adapter
//! live after GREEN, else clippy `-D warnings` fails on a never-used trait
//! the builder is not allowed to fix here.

use super::*;
use serde_json::json;
use two_bot_core::JobGate;

use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};

const GUILD: &str = "2222";

/// Baseline adapter: the specified tick entry point, replaying today's
/// unwired behavior (fetch straight through, gate ignored) until the
/// builder's inherent method shadows it. The fetch-only body is deliberate:
/// the specified skip happens before any fetch, and the normalize/store
/// layers below are owned by parallel work, so the adapter must not depend
/// on them.
trait BaselineEventsTick {
    async fn run_events_tick(
        pool: &PgPool,
        rest: &ActionExecutor,
        guild: &str,
        observation: &Mutex<()>,
        shutdown: &watch::Receiver<bool>,
        rank_heal: bool,
        gate: &JobGate,
    ) -> Result<(), ErrorClass>;
}

impl BaselineEventsTick for Kind {
    async fn run_events_tick(
        _pool: &PgPool,
        rest: &ActionExecutor,
        guild: &str,
        _observation: &Mutex<()>,
        _shutdown: &watch::Receiver<bool>,
        _rank_heal: bool,
        _gate: &JobGate,
    ) -> Result<(), ErrorClass> {
        // Unwired baseline: no gate check, straight to the events feed.
        rest.get_json(&format!(
            "/guilds/{guild}/scheduled-events?with_user_count=true"
        ))
        .await
        .map_err(|_| ErrorClass::Rest)?
        .ok_or(ErrorClass::Rest)
        .map(|_| ())
    }
}

/// One valid scheduled-event wire row in an array, mirroring the website
/// jobs fixture. The wired tick normalizes this, so it must stay valid:
/// matching guild, canonical snowflake ids.
fn events_snapshot() -> Value {
    json!([{
        "id": "3000",
        "guild_id": GUILD,
        "name": "Test night",
        "scheduled_start_time": "2026-10-01T19:00:00Z",
        "status": 1,
        "channel_id": null,
        "description": "fixture",
    }])
}

fn executor(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy(
        "synthetic-jobgate-test-token".to_owned(),
        Some(mock.origin()),
    )
    .unwrap()
}

fn offline_pool() -> PgPool {
    // Proven `LocalOnly` happy path: lazy, so no socket opens here; the
    // refused loopback connect fails fast on first use (never a hanging
    // unroutable host).
    admission_pool_with_tls(
        "postgres://fixture:fixture-password@127.0.0.1:1/fixture?sslmode=disable",
        two_bot_core::database_tls::TlsPolicy::LocalOnly,
    )
    .expect("fixture admission URL builds a lazy pool")
}

fn events_gets(requests: &[RestRequest]) -> usize {
    requests
        .iter()
        .filter(|request| request.method == "GET" && request.path.contains("scheduled-events"))
        .count()
}

/// OVERLAP (fails now): with the first tick blocked in its fetch, the
/// second tick must complete first (no queue), return `Ok` (no error) and
/// issue no fetch of its own (hence publish nothing), while the first tick
/// runs the full wired tick (offline: the refused DB).
#[tokio::test]
async fn overlapping_events_ticks_skip_the_second_without_fetch_error_or_queue() {
    let (mock, first_arrival) = MockRest::start_gated_at(
        vec![ScriptedResponse::json(200, events_snapshot())],
        ScriptedResponse::json(200, events_snapshot()),
        0,
    )
    .await;
    let rest = executor(&mock);
    let pool = offline_pool();
    let observation = Mutex::new(());
    let (_stop, shutdown) = watch::channel(false);
    let gate = JobGate::default();
    let order = Arc::new(std::sync::Mutex::new(Vec::<&str>::new()));
    let order_first = Arc::clone(&order);
    let order_second = Arc::clone(&order);
    let first = async {
        let result =
            Kind::run_events_tick(&pool, &rest, GUILD, &observation, &shutdown, false, &gate).await;
        order_first
            .lock()
            .expect("completion order")
            .push("first-done");
        result
    };
    let second = async {
        // Safety net only: the first GET always arrives (a fresh gate never
        // skips), so this fires solely on mock/executor breakage.
        tokio::time::timeout(Duration::from_secs(30), first_arrival.wait_for_request())
            .await
            .expect("first tick reaches REST");
        let result =
            Kind::run_events_tick(&pool, &rest, GUILD, &observation, &shutdown, false, &gate).await;
        order_second
            .lock()
            .expect("completion order")
            .push("second-done");
        first_arrival.release();
        result
    };
    let (first_result, second_result) = tokio::join!(first, second);
    assert_eq!(second_result, Ok(()), "second tick must skip without error");
    assert_eq!(
        *order.lock().expect("completion order"),
        vec!["second-done", "first-done"],
        "second tick must complete while the first is still blocked (no queue)"
    );
    let requests = mock.requests();
    assert_eq!(
        events_gets(&requests),
        1,
        "overlapping second tick must not issue its own fetch"
    );
    mock.shutdown().await;
    assert_eq!(
        first_result,
        Err(ErrorClass::Database),
        "first tick must run the full wired tick (offline DB refuses)"
    );
    pool.close().await;
}

/// SEQUENTIAL (passes now and after): back-to-back ticks each fetch. A
/// stuck-closed gate (every tick skipped, 0 fetches) or an asymmetric skip
/// (results differ) fails here, fast and without gating.
#[tokio::test]
async fn sequential_events_ticks_each_fetch_after_release() {
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, events_snapshot())).await;
    let rest = executor(&mock);
    let pool = offline_pool();
    let observation = Mutex::new(());
    let (_stop, shutdown) = watch::channel(false);
    let gate = JobGate::default();
    let first =
        Kind::run_events_tick(&pool, &rest, GUILD, &observation, &shutdown, false, &gate).await;
    let second =
        Kind::run_events_tick(&pool, &rest, GUILD, &observation, &shutdown, false, &gate).await;
    assert_eq!(
        first, second,
        "sequential ticks must behave identically (gate released between ticks)"
    );
    let requests = mock.requests();
    assert_eq!(
        events_gets(&requests),
        2,
        "sequential ticks must each fetch (no stuck-closed gate)"
    );
    mock.shutdown().await;
    pool.close().await;
}

/// PINS (pass now and after): 10-minute Events cadence, unchanged
/// Counter/Rank cadences, unchanged job registration.
#[test]
fn events_cadence_and_job_registration_are_pinned() {
    assert_eq!(
        SCHEDULED_EVENTS_INTERVAL_MS,
        10 * 60 * 1000,
        "events cadence stays 10 min"
    );
    assert_eq!(
        LIVE_COUNTER_INTERVAL_MS, 60_000,
        "counter cadence unchanged"
    );
    assert_eq!(RANK_SNAPSHOT_INTERVAL_MS, 600_000, "rank cadence unchanged");
    assert_eq!(
        NAMES,
        [
            "counter",
            "rank",
            "scheduled_events",
            crate::settings_jobs::NAME
        ],
        "job registration unchanged"
    );
}

/// KEEP-ALIVE (passes now and after): the explicit qualified call proves the
/// adapter is the unwired path (it fetches despite a held gate) and keeps
/// the trait live after GREEN, when plain calls shadow to the inherent
/// method. Without this, clippy `-D warnings` fails the builder's build on
/// a never-used trait in this protected file.
#[tokio::test]
async fn baseline_adapter_keeps_the_unwired_path_callable() {
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, events_snapshot())).await;
    let rest = executor(&mock);
    let pool = offline_pool();
    let observation = Mutex::new(());
    let (_stop, shutdown) = watch::channel(false);
    let gate = JobGate::default();
    let _held = gate.try_acquire().expect("hold the gate");
    // Qualified: hits the trait even after the inherent lands (inherent
    // methods never shadow qualified calls).
    let result = <Kind as BaselineEventsTick>::run_events_tick(
        &pool,
        &rest,
        GUILD,
        &observation,
        &shutdown,
        false,
        &gate,
    )
    .await;
    assert_eq!(
        result,
        Ok(()),
        "baseline adapter ignores the gate (unwired fetch-through)"
    );
    let requests = mock.requests();
    assert_eq!(
        events_gets(&requests),
        1,
        "baseline adapter fetches despite the held gate"
    );
    mock.shutdown().await;
    pool.close().await;
}
