#![cfg(test)]
//! Scheduler dispatch around the Events single-flight entry point.
//!
//! `Context::run_tick` is the one routing call the `serve` closure makes:
//! Events ticks go through the shared `JobGate` (a held gate skips at once
//! with `Ok` and no fetch), while every other kind runs direct via
//! `run_once` and never touches the gate. Same offline doubles as the
//! `JobGate` acceptance suite (mock REST, lazy refused pool). Unlike that
//! protected file, this dispatch layer is owned here and may change with it.

use super::*;
use serde_json::json;
use two_bot_core::JobGate;

use crate::discord_test_common::{MockRest, ScriptedResponse};

const GUILD: &str = "2222";

fn executor(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy(
        "synthetic-dispatch-test-token".to_owned(),
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

fn wired_context(mock: &MockRest) -> Context {
    // Pre-seed the pool cell so dispatch never dials: the skip path must not
    // need a database, and the direct path must fail fast on the refused one.
    let pool = OnceCell::new();
    pool.set(offline_pool()).expect("seed the pool cell");
    Context {
        url: "postgres://fixture:fixture-password@127.0.0.1:1/fixture?sslmode=disable".to_owned(),
        pool,
        rest: executor(mock),
        guild: GUILD.to_owned(),
        observation: Mutex::new(()),
        events_gate: JobGate::default(),
        event_voice: event_voice_preflight::Tick::default(),
        event_voice_source: event_voice_source::Reader::default(),
        event_voice_reconcile: event_voice_reconcile::Reconciler::default(),
        voice: None,
    }
}

fn idle_mock_response() -> ScriptedResponse {
    // Neither dispatch test needs a fetch to succeed: the skip path never
    // reaches REST, and the counter path fails fast on the refused pool
    // before its first fetch. Any valid body keeps the mock total.
    ScriptedResponse::json(200, json!([]))
}

/// A held gate skips the dispatched Events tick: `Ok`, no fetch, no queue.
#[tokio::test]
async fn dispatch_skips_events_tick_while_gate_held() {
    let mock = MockRest::start(vec![], idle_mock_response()).await;
    let context = wired_context(&mock);
    let (_stop, shutdown) = watch::channel(false);
    let _held = context.events_gate.try_acquire().expect("hold the gate");
    let result = context.run_tick(Kind::Events, &shutdown, false).await;
    assert_eq!(result, Ok(()), "held gate must skip the Events tick");
    assert!(
        mock.requests().is_empty(),
        "skipped tick must not reach REST"
    );
    mock.shutdown().await;
}

/// Counter ticks ignore the gate: a held gate still runs the tick, which
/// reaches the database and reports the refused offline pool.
#[tokio::test]
async fn dispatch_runs_counter_tick_direct_despite_held_gate() {
    let mock = MockRest::start(vec![], idle_mock_response()).await;
    let context = wired_context(&mock);
    let (_stop, shutdown) = watch::channel(false);
    let _held = context.events_gate.try_acquire().expect("hold the gate");
    let result = context.run_tick(Kind::Counter, &shutdown, false).await;
    assert_eq!(
        result,
        Err(ErrorClass::Database),
        "counter tick must run past the held gate onto the refused pool"
    );
    mock.shutdown().await;
}
