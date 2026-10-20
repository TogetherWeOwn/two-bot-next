//! Acceptance: a bare 429 — `Retry-After` only, with no `X-RateLimit-*`
//! bucket headers and no `retry_after`/`global`/`scope` body signal — still
//! teaches the shared guard state, so every executor sharing the guard waits
//! out the same route deadline instead of spending another wire request to
//! relearn it.
//!
//! Contract under test:
//! - A 429 with `Retry-After`-only installs
//!   `max(existing, observed_at + retry_after + 250 ms pad)` in shared guard
//!   state for its route, never shortening an existing deadline.
//! - Two workers against a `[429 Retry-After: 2, 200]` script produce
//!   exactly 2 wire requests with a gap of at least 1.9 s.
//! - The knowledge is route-scoped: it must not become a global pause.
//! - Single-worker retries and the full-header bucket path are unchanged.
#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use std::{sync::Arc, time::Duration};
use tokio::time::Instant;
use two_bot_discord::{ratelimit_guard::RateLimitGuard, ActionExecutor, DiscordError};

const ROUTE: &str = "GET /guilds/2222/members";
const OTHER_ROUTE: &str = "GET /channels/4444/messages";

fn executor(mock: &MockRest, guard: &Arc<RateLimitGuard>) -> ActionExecutor {
    ActionExecutor::with_proxy_and_guard(
        "bare-429-fixture-token".to_owned(),
        Some(mock.origin()),
        guard.clone(),
    )
    .unwrap()
}

async fn ban(exec: &ActionExecutor) -> Result<(), DiscordError> {
    exec.ban("2222", "3333", "bare 429 test").await
}

fn guard() -> Arc<RateLimitGuard> {
    Arc::new(RateLimitGuard::new(Default::default()).unwrap())
}

/// A 429 carrying only a `Retry-After` header: empty body, no bucket hash,
/// no remaining/reset headers, no global or scope signal.
fn bare_429(retry_after: &str) -> ScriptedResponse {
    let mut response = ScriptedResponse::status(429);
    response
        .headers
        .push(("Retry-After".into(), retry_after.into()));
    response
}

fn bare_headers(retry_after: &str) -> http::HeaderMap {
    http::HeaderMap::from_iter([(
        http::header::HeaderName::from_static("retry-after"),
        http::HeaderValue::from_str(retry_after).unwrap(),
    )])
}

fn full_headers(reset_after: &str) -> http::HeaderMap {
    http::HeaderMap::from_iter([
        (
            http::header::HeaderName::from_static("x-ratelimit-bucket"),
            http::HeaderValue::from_static("bucket-bare-fixture"),
        ),
        (
            http::header::HeaderName::from_static("x-ratelimit-remaining"),
            http::HeaderValue::from_static("0"),
        ),
        (
            http::header::HeaderName::from_static("x-ratelimit-reset-after"),
            http::HeaderValue::from_str(reset_after).unwrap(),
        ),
    ])
}

// --- Shared guard state, observed directly through the public seam the ---
// --- executor feeds on every response (`observe_bucket` in,           ---
// --- `admit_route` out).                                               ---

#[tokio::test]
async fn bare_retry_after_installs_a_padded_shared_deadline() {
    let guard = guard();
    guard.observe_bucket(ROUTE, &bare_headers("0.4"));
    // Route-scoped knowledge only: a bare 429 must not open a global pause.
    assert_eq!(guard.snapshot().global_pause_remaining, Duration::ZERO);
    assert_eq!(guard.snapshot().global_pauses_total, 0);
    let started = Instant::now();
    guard.admit_route(ROUTE, false).await.unwrap();
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(550),
        "admission skipped the bare Retry-After deadline (0.4 s + 250 ms pad): {waited:?}"
    );
    assert!(
        waited < Duration::from_secs(2),
        "admission waited far past retry_after + pad: {waited:?}"
    );
}

#[tokio::test]
async fn bare_retry_after_never_shortens_an_existing_deadline() {
    let guard = guard();
    // Existing full-header deadline: 1.0 s reset (+ the standing pad).
    guard.observe_bucket(ROUTE, &full_headers("1.0"));
    // A bare 429 with a much smaller Retry-After must not pull it in.
    guard.observe_bucket(ROUTE, &bare_headers("0.05"));
    let started = Instant::now();
    guard.admit_route(ROUTE, false).await.unwrap();
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(1000),
        "the bare Retry-After shortened the existing deadline: {waited:?}"
    );
}

#[tokio::test]
async fn bare_retry_after_extends_a_shorter_existing_deadline() {
    let guard = guard();
    guard.observe_bucket(ROUTE, &full_headers("0.05"));
    guard.observe_bucket(ROUTE, &bare_headers("0.8"));
    let started = Instant::now();
    guard.admit_route(ROUTE, false).await.unwrap();
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(900),
        "the bare Retry-After did not extend the deadline to 0.8 s + pad: {waited:?}"
    );
    assert!(waited < Duration::from_secs(2), "wait overshot: {waited:?}");
}

#[tokio::test]
async fn bare_retry_after_gates_only_its_own_route() {
    let guard = guard();
    guard.observe_bucket(ROUTE, &bare_headers("0.8"));
    let started = Instant::now();
    guard.admit_route(OTHER_ROUTE, false).await.unwrap();
    let waited = started.elapsed();
    assert!(
        waited < Duration::from_millis(300),
        "a bare 429 on one route stalled an unrelated route: {waited:?}"
    );
}

// --- End to end over the loopback REST double: two executors, one guard. ---

#[tokio::test]
async fn two_workers_share_bare_429_retry_after_knowledge() {
    let mock = MockRest::start(
        vec![bare_429("2")],
        ScriptedResponse::json(200, serde_json::json!([])),
    )
    .await;
    let guard = guard();
    let first = executor(&mock, &guard);
    let second = executor(&mock, &guard);
    let path = "/guilds/2222/members?limit=1000";

    assert_eq!(first.get_json_once(path).await.unwrap(), None);
    assert_eq!(mock.requests().len(), 1, "a 429 must not retry the page");
    assert_eq!(
        second.get_json_once(path).await.unwrap(),
        Some(serde_json::json!([]))
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 2, "exactly two wire requests");
    let gap = requests[1]
        .received_at
        .duration_since(requests[0].received_at);
    assert!(
        gap >= Duration::from_millis(1900),
        "the second worker skipped the shared Retry-After wait: {gap:?}"
    );
    assert!(gap < Duration::from_secs(4), "the wait overshot: {gap:?}");
    mock.shutdown().await;
}

#[tokio::test]
async fn concurrent_workers_wait_out_the_shared_bare_deadline_once() {
    let mock = MockRest::start(vec![bare_429("0.6")], ScriptedResponse::status(204)).await;
    let guard = guard();
    let first = executor(&mock, &guard);
    let second = executor(&mock, &guard);

    assert_eq!(ban(&first).await, Err(DiscordError::RateLimited));
    let started = Instant::now();
    let (a, b, c) = tokio::join!(ban(&first), ban(&second), ban(&second));
    a.unwrap();
    b.unwrap();
    c.unwrap();
    let requests = mock.requests();
    assert_eq!(requests.len(), 4);
    for request in &requests[1..] {
        assert!(
            request.received_at.duration_since(requests[0].received_at)
                >= Duration::from_millis(800),
            "a concurrent worker skipped the shared bare-429 deadline"
        );
    }
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "cooldowns stacked additively across workers"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn bare_429_on_one_route_does_not_stall_another_route() {
    let mock = MockRest::start(
        vec![bare_429("2")],
        ScriptedResponse::json(200, serde_json::json!([])),
    )
    .await;
    let guard = guard();
    let first = executor(&mock, &guard);
    let second = executor(&mock, &guard);

    assert_eq!(
        first
            .get_json_once("/guilds/2222/members?limit=1000")
            .await
            .unwrap(),
        None
    );
    let started = Instant::now();
    assert_eq!(
        second
            .get_json_once("/channels/4444/messages?limit=100")
            .await
            .unwrap(),
        Some(serde_json::json!([]))
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "bare-429 knowledge leaked across routes as a global stall"
    );
    assert_eq!(mock.requests().len(), 2);
    mock.shutdown().await;
}

// --- Unchanged paths: these pin behavior that already holds today. ---

#[tokio::test]
async fn single_worker_bare_429_retry_is_unchanged() {
    let mock = MockRest::start(
        vec![bare_429("0.2")],
        ScriptedResponse::json(200, serde_json::json!({"ok": true})),
    )
    .await;
    let guard = guard();
    let exec = executor(&mock, &guard);
    let data = tokio::time::timeout(Duration::from_secs(8), exec.get_json("/guilds/2222"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(data, Some(serde_json::json!({"ok": true})));
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let gap = requests[1]
        .received_at
        .duration_since(requests[0].received_at);
    assert!(
        gap >= Duration::from_millis(350),
        "the single-worker retry skipped its Retry-After wait: {gap:?}"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn full_header_bucket_path_is_unchanged() {
    let mut response = ScriptedResponse::json(
        429,
        serde_json::json!({"retry_after": 0.6, "global": false}),
    );
    response.headers.push(("Retry-After".into(), "0.6".into()));
    response
        .headers
        .push(("X-RateLimit-Bucket".into(), "bucket-bare-fixture".into()));
    response
        .headers
        .push(("X-RateLimit-Remaining".into(), "0".into()));
    response
        .headers
        .push(("X-RateLimit-Reset-After".into(), "0.6".into()));
    let mock = MockRest::start(
        vec![response],
        ScriptedResponse::json(200, serde_json::json!([])),
    )
    .await;
    let guard = guard();
    let first = executor(&mock, &guard);
    let second = executor(&mock, &guard);
    let path = "/guilds/2222/members?limit=1000";

    assert_eq!(first.get_json_once(path).await.unwrap(), None);
    assert_eq!(
        second.get_json_once(path).await.unwrap(),
        Some(serde_json::json!([]))
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let gap = requests[1]
        .received_at
        .duration_since(requests[0].received_at);
    assert!(
        gap >= Duration::from_millis(700),
        "the full-header bucket deadline stopped gating the second worker: {gap:?}"
    );
    mock.shutdown().await;
}
