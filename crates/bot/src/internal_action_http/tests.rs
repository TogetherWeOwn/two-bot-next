#![cfg(test)]

use super::*;
use axum::body::Body;
use std::{
    collections::HashMap,
    sync::atomic::{AtomicUsize, Ordering},
};
use tower::ServiceExt;
use two_bot_core::internal_actions::sign;
use two_bot_testsupport::TestDatabase;

mod adapter;

fn secret(index: usize) -> String {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../core/tests/fixtures/internal-action-signing.json"
    ))
    .unwrap();
    fixture["vectors"][index]["secret"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn config() -> InternalActionConfig {
    InternalActionConfig::from_map(&HashMap::from([
        ("TWO_INTERNAL_ACTIONS".to_owned(), "1".to_owned()),
        ("TWO_INTERNAL_BIND".to_owned(), "127.0.0.1:8091".to_owned()),
        (
            "TWO_INTERNAL_KEYS".to_owned(),
            format!("old:{},new:{}", secret(0), secret(1)),
        ),
        (
            "TWO_INTERNAL_CALLERS".to_owned(),
            "old:website-staging,new:website-staging".to_owned(),
        ),
        (
            "TWO_INTERNAL_CHANNEL_KEYS".to_owned(),
            "ann:333333333333333333".to_owned(),
        ),
    ]))
    .unwrap()
    .unwrap()
}

fn lazy_pool() -> sqlx::PgPool {
    sqlx::postgres::PgPoolOptions::new().connect_lazy_with(
        sqlx::postgres::PgConnectOptions::new()
            .host("agent-testdb")
            .port(5432)
            .username("agent_test")
            .database("internal_http_unit"),
    )
}

#[derive(Clone, Copy)]
enum MockOutcome {
    Success,
    Unknown,
    NoEffect,
    Wait,
}

struct MockEffect {
    calls: AtomicUsize,
    outcome: MockOutcome,
    entered: Semaphore,
    release: Semaphore,
    break_finish: Option<sqlx::PgPool>,
}

impl MockEffect {
    fn new(outcome: MockOutcome) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            outcome,
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
            break_finish: None,
        }
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ActionEffect for MockEffect {
    fn execute<'a>(&'a self, _: &'a Map<String, Value>) -> BoxFuture<'a, Effect> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.add_permits(1);
            if matches!(self.outcome, MockOutcome::Wait) {
                self.release.acquire().await.unwrap().forget();
            }
            if let Some(pool) = &self.break_finish {
                // Dispose only a migrated TestDatabase relation, not a live DB.
                sqlx::query("ALTER TABLE internal_action_log RENAME TO receiver_hidden_audit")
                    .execute(pool)
                    .await
                    .unwrap();
            }
            match self.outcome {
                MockOutcome::Unknown => Effect::Unknown,
                MockOutcome::NoEffect => {
                    Effect::Terminal(TerminalResponse::Failure(TerminalFailure::NoEffect))
                }
                MockOutcome::Success | MockOutcome::Wait => {
                    Effect::Terminal(TerminalResponse::Success {
                        resource_id: Some(DiscordId::new("444444444444444444").unwrap()),
                        affected: 1,
                    })
                }
            }
        })
    }
}

fn state(pool: sqlx::PgPool, effect: Arc<MockEffect>) -> Arc<ReceiverState> {
    state_with_reads(pool, effect, Arc::new(MockEventRead::default()))
}

fn state_with_reads(
    pool: sqlx::PgPool,
    effect: Arc<MockEffect>,
    reads: Arc<MockEventRead>,
) -> Arc<ReceiverState> {
    state_full(pool, effect, reads, Arc::new(MockModeration::default()))
}

fn state_full(
    pool: sqlx::PgPool,
    effect: Arc<MockEffect>,
    reads: Arc<MockEventRead>,
    moderation: Arc<MockModeration>,
) -> Arc<ReceiverState> {
    Arc::new(ReceiverState::new(
        config(),
        pool,
        effect,
        reads,
        moderation,
    ))
}

/// Offline moderation double: the auth/key/flag fences must refuse before
/// this is ever called, so deny-path tests assert `calls() == 0`.
#[derive(Default)]
struct MockModeration {
    calls: AtomicUsize,
}

impl MockModeration {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ModerationEffect for MockModeration {
    fn execute_moderation<'a>(
        &'a self,
        request: &'a InternalMemberRequest,
        _: &'a str,
        _: &'a str,
        _: i64,
    ) -> BoxFuture<'a, Result<TerminalResponse, ActionError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let target = DiscordId::new(request.target_id()).expect("validated target");
            Ok(TerminalResponse::Success {
                resource_id: Some(target),
                affected: 1,
            })
        })
    }
}

/// Offline read double: the auth/key/flag fences must refuse before this is
/// ever called, so most tests assert `calls() == 0`.
#[derive(Default)]
struct MockEventRead {
    calls: AtomicUsize,
}

impl MockEventRead {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl EventReadEffect for MockEventRead {
    fn execute_read<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a str,
    ) -> BoxFuture<'a, Result<Value, EventActionError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"outcome": "read"}))
        })
    }
}

/// The receiver reads its enabled set from the process environment, so
/// flag-dependent tests serialize on this lock and always restore the var.
/// Async-aware: the guard is held across `.await` points by design.
static EVENT_READ_FLAG_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn set_event_read_flag(on: bool) {
    if on {
        std::env::set_var("TWO_INTERNAL_ALLOW_EVENT_READ", "1");
    } else {
        std::env::remove_var("TWO_INTERNAL_ALLOW_EVENT_READ");
    }
}

/// The moderation flag gate needs BOTH vars: the internal allowlist plus the
/// moderation publish gate. Tests serialize on their own lock and always
/// restore both vars.
static MODERATION_FLAG_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn set_moderation_flags(on: bool) {
    if on {
        std::env::set_var("TWO_INTERNAL_ALLOW_MODERATION", "1");
        std::env::set_var("TWO_MODERATION", "1");
    } else {
        std::env::remove_var("TWO_INTERNAL_ALLOW_MODERATION");
        std::env::remove_var("TWO_MODERATION");
    }
}

fn moderation_payload(action: &str) -> String {
    let mut body = serde_json::json!({
        "action": action,
        "actor_id": "111111111111111111",
        "discord_id": "333333333333333333",
        "reason": "spam",
    });
    if action == "moderation.tempban" {
        body["duration_seconds"] = serde_json::json!(3600);
    }
    body.to_string()
}

fn moderation_payload_with(action: &str, extra: Value) -> String {
    let mut body = serde_json::json!({
        "action": action,
        "actor_id": "111111111111111111",
        "discord_id": "333333333333333333",
        "reason": "spam",
    });
    if action == "moderation.tempban" && extra.get("duration_seconds").is_none() {
        body["duration_seconds"] = serde_json::json!(3600);
    }
    for (key, value) in extra.as_object().unwrap() {
        body[key] = value.clone();
    }
    body.to_string()
}

fn moderation_outcome_for(action: &str) -> &'static str {
    match action {
        "moderation.ban" => "banned",
        "moderation.tempban" => "temporarily_banned",
        "moderation.kick" => "kicked",
        _ => "warned",
    }
}

fn read_payload(key: &str) -> String {
    serde_json::json!({"action": "event.read", "event_key": key}).to_string()
}

/// Keyless read signing: no `Idempotency-Key` header is sent.
fn signed_read(raw: &str, key: &str) -> Request {
    signed_read_with_nonce(raw, key, &nonce())
}

fn signed_read_with_nonce(raw: &str, key: &str, nonce_value: &str) -> Request {
    let timestamp = (now_ms() / 1000).to_string();
    let signature = sign(
        secret(usize::from(key == "new")).as_bytes(),
        &timestamp,
        nonce_value,
        raw.as_bytes(),
    );
    Request::builder()
        .method(Method::POST)
        .uri(ACTIONS_PATH)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-two-key-id", key)
        .header("x-two-timestamp", timestamp)
        .header("x-two-nonce", nonce_value)
        .header("x-two-signature", signature)
        .body(Body::from(raw.to_owned()))
        .unwrap()
}

fn payload() -> &'static str {
    r#"{"action":"announcement.post","channel_key":"ann","body":"fixture announcement"}"#
}

fn nonce() -> String {
    hex::encode(rand::random::<[u8; 16]>())
}

fn signed(raw: &str, key: &str, intent: &str) -> Request {
    signed_with_nonce(raw, key, &nonce(), intent)
}

fn signed_with_nonce(raw: &str, key: &str, nonce: &str, intent: &str) -> Request {
    let timestamp = (now_ms() / 1000).to_string();
    let signature = sign(
        secret(usize::from(key == "new")).as_bytes(),
        &timestamp,
        nonce,
        raw.as_bytes(),
    );
    Request::builder()
        .method(Method::POST)
        .uri(ACTIONS_PATH)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-two-key-id", key)
        .header("x-two-timestamp", timestamp)
        .header("x-two-nonce", nonce)
        .header("x-two-signature", signature)
        .header("idempotency-key", intent)
        .body(Body::from(raw.to_owned()))
        .unwrap()
}

async fn answer(app: Router, request: Request) -> (StatusCode, HeaderMap, Value) {
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let raw = to_bytes(response.into_body(), 8192).await.unwrap();
    (status, headers, serde_json::from_slice(&raw).unwrap())
}

async fn database() -> Option<TestDatabase> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must provide a guarded test database"
        );
        eprintln!("SKIP internal-action receiver DB acceptance: TWO_TEST_DATABASE_URL is unset");
        return None;
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .unwrap(),
    )
}

#[tokio::test]
async fn method_path_media_headers_and_body_are_bounded_before_authentication() {
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let state = state(lazy_pool(), effect.clone());
    for (method, path, status) in [
        (Method::GET, ACTIONS_PATH, StatusCode::METHOD_NOT_ALLOWED),
        (Method::HEAD, ACTIONS_PATH, StatusCode::METHOD_NOT_ALLOWED),
        (
            Method::POST,
            "/internal/actions?probe=secret-sentinel",
            StatusCode::NOT_FOUND,
        ),
        (Method::POST, "/internal/actions/", StatusCode::NOT_FOUND),
        (Method::POST, "/%69nternal/actions", StatusCode::NOT_FOUND),
        (Method::POST, "/health", StatusCode::NOT_FOUND),
    ] {
        let request = Request::builder()
            .method(method.clone())
            .uri(path)
            .body(Body::empty())
            .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), status);
        if status == StatusCode::METHOD_NOT_ALLOWED {
            assert_eq!(response.headers()[header::ALLOW], "POST");
        }
        // HEAD semantics intentionally suppress the envelope body.
        if method != Method::HEAD {
            let raw = to_bytes(response.into_body(), 8192).await.unwrap();
            assert!(!String::from_utf8_lossy(&raw).contains("secret-sentinel"));
        }
    }
    for name in [
        "x-two-key-id",
        "x-two-timestamp",
        "x-two-nonce",
        "x-two-signature",
        "idempotency-key",
        "content-type",
    ] {
        let mut request = signed(payload(), "old", "intent-fixture");
        let value = request.headers()[name].clone();
        request.headers_mut().append(name, value);
        let (status, _, _) = answer(router(state.clone()), request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name}");
    }
    for (media, encoding) in [("text/plain", None), ("application/json", Some("gzip"))] {
        let mut request = signed(payload(), "old", "intent-fixture");
        request
            .headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static(media));
        if let Some(encoding) = encoding {
            request
                .headers_mut()
                .insert(header::CONTENT_ENCODING, HeaderValue::from_static(encoding));
        }
        assert_eq!(
            answer(router(state.clone()), request).await.0,
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
    }
    let mut request = signed(payload(), "old", "intent-fixture");
    request.headers_mut().insert(
        "x-padding",
        HeaderValue::from_str(&"p".repeat(MAX_HEADER_BYTES)).unwrap(),
    );
    assert_eq!(
        answer(router(state.clone()), request).await.0,
        StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
    );
    let mut request = signed(payload(), "old", "intent-fixture");
    request.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&(MAX_BODY_BYTES + 1).to_string()).unwrap(),
    );
    assert_eq!(
        answer(router(state.clone()), request).await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let mut request = signed(payload(), "old", "intent-fixture");
    *request.body_mut() = Body::from(vec![b'p'; MAX_BODY_BYTES + 1]);
    assert_eq!(
        answer(router(state.clone()), request).await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(effect.calls(), 0);
    assert_eq!(state.clock.lock().unwrap().high_water_ms(), None);
}

#[tokio::test]
async fn exhausted_request_capacity_refuses_without_authentication_or_effect() {
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let state = state(lazy_pool(), effect.clone());
    let permit = state
        .capacity
        .acquire_many(MAX_REQUESTS as u32)
        .await
        .unwrap();
    let (status, _, body) = answer(
        router(state.clone()),
        signed(payload(), "old", "intent-fixture"),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "busy");
    assert_eq!(body["error"]["retryable"], true);
    assert_eq!(effect.calls(), 0);
    assert_eq!(state.clock.lock().unwrap().high_water_ms(), None);
    drop(permit);
    assert_eq!(state.capacity.available_permits(), MAX_REQUESTS);
}

#[tokio::test(start_paused = true)]
async fn stalled_body_collection_times_out_and_releases_capacity_before_authentication() {
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let state = state(lazy_pool(), effect.clone());
    let mut request = signed(payload(), "old", "intent-fixture");
    *request.body_mut() = Body::from_stream(futures_util::stream::pending::<
        Result<axum::body::Bytes, std::io::Error>,
    >());
    let (status, _, body) = answer(router(state.clone()), request).await;
    assert_eq!(status, StatusCode::REQUEST_TIMEOUT);
    assert_eq!(body["error"]["code"], "request_timeout");
    assert_eq!(effect.calls(), 0);
    assert_eq!(state.clock.lock().unwrap().high_water_ms(), None);
    assert_eq!(state.capacity.available_permits(), MAX_REQUESTS);
}

#[tokio::test]
async fn forged_and_unknown_authentication_share_redacted_wire_and_do_not_touch_state() {
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let state = state(lazy_pool(), effect.clone());
    for key in ["old", "unknown", "secret-sentinel"] {
        let mut request = signed(payload(), "old", "intent-fixture");
        request
            .headers_mut()
            .insert("x-two-key-id", HeaderValue::from_str(key).unwrap());
        request.headers_mut().insert(
            "x-two-signature",
            HeaderValue::from_static("sha256=bad-signature"),
        );
        let (status, _, body) = answer(router(state.clone()), request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            body["error"],
            json!({"code":"unauthorized", "message":"Signature verification failed", "retryable":false})
        );
        assert!(!body.to_string().contains("secret-sentinel"));
    }
    assert_eq!(state.clock.lock().unwrap().high_water_ms(), None);
    assert_eq!(effect.calls(), 0);
}

#[tokio::test]
async fn signed_malformed_json_burns_before_parse_and_nonce_replay_is_refused() {
    let Some(db) = database().await else { return };
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let app = router(state(db.pool().clone(), effect.clone()));
    for raw in [
        "{",
        r#"{"action":"announcement.post","action":"announcement.post"}"#,
    ] {
        let nonce = nonce();
        assert_eq!(
            answer(
                app.clone(),
                signed_with_nonce(raw, "old", &nonce, "intent-fixture")
            )
            .await
            .2["error"]["code"],
            "malformed"
        );
        assert_eq!(
            answer(
                app.clone(),
                signed_with_nonce(raw, "old", &nonce, "intent-fixture")
            )
            .await
            .2["error"]["code"],
            "replayed"
        );
    }
    let burned: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM internal_nonces")
        .fetch_one(db.pool())
        .await
        .unwrap();
    let intents: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM internal_idempotency")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!((burned, intents, effect.calls()), (2, 0, 0));
    db.close().await.unwrap();
}

#[tokio::test]
async fn success_replays_across_key_rotation_and_restart_but_changed_bytes_conflict() {
    let Some(db) = database().await else { return };
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let app = router(state(db.pool().clone(), effect.clone()));
    let (status, headers, first) =
        answer(app.clone(), signed(payload(), "old", "intent-fixture")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key("idempotent-replay"));
    assert_eq!(first["result"], json!({"message_id":"444444444444444444"}));
    assert_eq!(first["request_id"].as_str().unwrap().len(), 26);
    let restarted_pool = db.independent_pool().await.unwrap();
    let restarted = router(state(restarted_pool.clone(), effect.clone()));
    let (status, headers, replay) = answer(
        restarted.clone(),
        signed(payload(), "new", "intent-fixture"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["idempotent-replay"], "true");
    assert_eq!(first["result"], replay["result"]);
    // Same JSON intent with changed exact signed representation is a mismatch.
    let changed = format!("{} ", payload());
    let (status, _, refusal) = answer(restarted, signed(&changed, "new", "intent-fixture")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(refusal["error"]["retryable"], false);
    assert_eq!(effect.calls(), 1);
    restarted_pool.close().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn concurrent_duplicate_gets_in_progress_then_receipt_without_second_effect() {
    let Some(db) = database().await else { return };
    let effect = Arc::new(MockEffect::new(MockOutcome::Wait));
    let app = router(state(db.pool().clone(), effect.clone()));
    let first = tokio::spawn(answer(
        app.clone(),
        signed(payload(), "old", "intent-fixture"),
    ));
    tokio::time::timeout(Duration::from_secs(5), effect.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let (status, _, duplicate) =
        answer(app.clone(), signed(payload(), "old", "intent-fixture")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        duplicate["error"],
        json!({"code":"in_progress", "message":"Internal action refused", "retryable":true})
    );
    assert_eq!(effect.calls(), 1);
    effect.release.add_permits(1);
    assert_eq!(first.await.unwrap().0, StatusCode::OK);
    assert_eq!(
        answer(app, signed(payload(), "old", "intent-fixture"))
            .await
            .1["idempotent-replay"],
        "true"
    );
    assert_eq!(effect.calls(), 1);
    db.close().await.unwrap();
}

#[tokio::test]
async fn no_effect_and_unknown_outcomes_never_resend_the_intent() {
    let Some(db) = database().await else { return };
    for (intent, outcome, code, replayed) in [
        ("intent-no-effect", MockOutcome::NoEffect, "no_effect", true),
        (
            "intent-unknown",
            MockOutcome::Unknown,
            "needs_reconciliation",
            false,
        ),
    ] {
        let effect = Arc::new(MockEffect::new(outcome));
        let app = router(state(db.pool().clone(), effect.clone()));
        let (_, _, first) = answer(app.clone(), signed(payload(), "old", intent)).await;
        assert_eq!(first["error"]["code"], code);
        assert_eq!(first["error"]["retryable"], false);
        let (_, headers, second) = answer(app, signed(payload(), "old", intent)).await;
        assert_eq!(second["error"]["code"], code);
        assert_eq!(headers.contains_key("idempotent-replay"), replayed);
        assert_eq!(effect.calls(), 1);
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn cancellation_after_committed_claim_never_reclaims_even_when_stale() {
    let Some(db) = database().await else { return };
    let effect = Arc::new(MockEffect::new(MockOutcome::Wait));
    let app = router(state(db.pool().clone(), effect.clone()));
    let first = tokio::spawn(answer(
        app.clone(),
        signed(payload(), "old", "intent-fixture"),
    ));
    tokio::time::timeout(Duration::from_secs(5), effect.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    sqlx::query(
        "UPDATE internal_idempotency SET created_at = clock_timestamp() - interval '61 seconds'",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let (_, _, next) = answer(app, signed(payload(), "old", "intent-fixture")).await;
    assert_eq!(next["error"]["code"], "needs_reconciliation");
    assert_eq!(next["error"]["retryable"], false);
    assert_eq!(effect.calls(), 1);
    db.close().await.unwrap();
}

#[tokio::test]
async fn missing_nonce_or_claim_storage_prevents_any_effect() {
    let Some(db) = database().await else { return };
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let app = router(state(db.pool().clone(), effect.clone()));
    sqlx::query("ALTER TABLE internal_nonces RENAME TO receiver_hidden_nonces")
        .execute(db.pool())
        .await
        .unwrap();
    assert_eq!(
        answer(app.clone(), signed(payload(), "old", "intent-fixture"))
            .await
            .2["error"]["code"],
        "internal"
    );
    sqlx::query("ALTER TABLE receiver_hidden_nonces RENAME TO internal_nonces")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("ALTER TABLE internal_idempotency RENAME TO receiver_hidden_intents")
        .execute(db.pool())
        .await
        .unwrap();
    assert_eq!(
        answer(app, signed(payload(), "old", "intent-fixture"))
            .await
            .2["error"]["code"],
        "internal"
    );
    assert_eq!(effect.calls(), 0);
    db.close().await.unwrap();
}

#[tokio::test]
async fn finish_failure_after_effect_is_not_success_or_permission_to_resend() {
    let Some(db) = database().await else { return };
    let mut mock = MockEffect::new(MockOutcome::Success);
    mock.break_finish = Some(db.pool().clone());
    let effect = Arc::new(mock);
    let app = router(state(db.pool().clone(), effect.clone()));
    let (_, _, first) = answer(app.clone(), signed(payload(), "old", "intent-fixture")).await;
    assert_eq!(first["error"]["code"], "needs_reconciliation");
    sqlx::query("ALTER TABLE receiver_hidden_audit RENAME TO internal_action_log")
        .execute(db.pool())
        .await
        .unwrap();
    let (_, _, second) = answer(app, signed(payload(), "old", "intent-fixture")).await;
    assert_eq!(second["error"]["code"], "in_progress");
    assert_eq!(effect.calls(), 1);
    db.close().await.unwrap();
}

#[tokio::test]
async fn authenticated_unsupported_actions_and_bad_channel_keys_stay_redacted() {
    let Some(db) = database().await else { return };
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let app = router(state(db.pool().clone(), effect.clone()));
    for raw in [
        r#"{"action":"role.assign","discord_id":"111111111111111111","role_key":"fixture"}"#,
        r#"{"action":"announcement.post","channel_key":"secret-sentinel","body":"secret-sentinel"}"#,
    ] {
        let (_, _, body) = answer(app.clone(), signed(raw, "old", "intent-fixture")).await;
        assert_eq!(body["error"]["code"], "action_not_allowed");
        assert!(!body.to_string().contains("secret-sentinel"));
    }
    assert_eq!(effect.calls(), 0);
    db.close().await.unwrap();
}

#[tokio::test]
async fn health_router_has_no_action_route() {
    let state = crate::server::SharedState::new(
        Arc::new(tokio::sync::RwLock::new(
            crate::gateway::GatewayState::Unconfigured,
        )),
        None,
    );
    let response = crate::server::router(state)
        .oneshot(signed(payload(), "old", "intent-fixture"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[test]
fn receiver_prerequisites_refuse_incomplete_or_non_staging_runtime() {
    let mut config = two_bot_core::Config {
        discord_token: None,
        database_url: None,
        guild_id: None,
        listen_addr: "127.0.0.1:8080".to_owned(),
    };
    assert!(crate::internal_receiver_prerequisites(&config).is_err());
    config.discord_token = Some("fixture-only-bot-token".to_owned().into());
    config.database_url = Some(
        "postgres://agent_test@agent-testdb:5432/agent_test"
            .to_owned()
            .into(),
    );
    config.guild_id = Some(
        two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID
            .parse()
            .unwrap(),
    );
    assert!(crate::internal_receiver_prerequisites(&config).is_ok());
    config.guild_id = Some(111111111111111111);
    assert!(crate::internal_receiver_prerequisites(&config).is_err());
}

/// Loopback Discord double answering scheduled-event GETs. Records every
/// request so tests prove refusals happen before any Discord call.
struct MockEventApi {
    origin: String,
    requests: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}

const READ_EVENT_ID: &str = "100000000000000007";

fn staging_guild() -> &'static str {
    two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID
}

fn discord_event() -> Value {
    json!({
        "id": READ_EVENT_ID,
        "guild_id": staging_guild(),
        "name": "Launch Night",
        "scheduled_start_time": "2026-09-01T20:00:00.000Z",
        "channel_id": Value::Null,
        "description": Value::Null,
        "entity_metadata": {"location": "The Hall"},
        "status": 1,
    })
}

impl MockEventApi {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let reply = discord_event();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().fallback(|request: Request| async move {
                    let (parts, _) = request.into_parts();
                    seen.lock().unwrap().push(json!({
                        "method": parts.method.as_str(),
                        "path": parts.uri.path(),
                    }));
                    (StatusCode::OK, Json(reply))
                }),
            )
            .await
            .unwrap();
        });
        Self {
            origin,
            requests,
            task,
        }
    }

    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl Drop for MockEventApi {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn read_app(pool: sqlx::PgPool, api: &MockEventApi) -> Router {
    let executor =
        ActionExecutor::with_proxy("not-a-credential".to_owned(), Some(api.origin.clone()))
            .unwrap();
    let reads: Arc<dyn EventReadEffect> = Arc::new(EventReadExecutor::new(executor, pool.clone()));
    let effect: Arc<dyn ActionEffect> = Arc::new(MockEffect::new(MockOutcome::Success));
    router(Arc::new(ReceiverState::new(
        config(),
        pool,
        effect,
        reads,
        Arc::new(MockModeration::default()),
    )))
}

async fn map_launch(pool: &sqlx::PgPool) {
    InternalActionStore::new(pool.clone())
        .put_event_key(staging_guild(), "launch", READ_EVENT_ID)
        .await
        .unwrap();
}

#[tokio::test]
async fn event_read_forged_signature_is_unauthorized_before_any_effect() {
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let reads = Arc::new(MockEventRead::default());
    let state = state_with_reads(lazy_pool(), effect.clone(), reads.clone());
    let _flag = EVENT_READ_FLAG_LOCK.lock().await;
    set_event_read_flag(true);
    let mut request = signed_read(&read_payload("launch"), "old");
    request.headers_mut().insert(
        "x-two-signature",
        HeaderValue::from_static("sha256=bad-signature"),
    );
    let (status, _, body) = answer(router(state.clone()), request).await;
    set_event_read_flag(false);
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "unauthorized");
    assert_eq!(effect.calls(), 0);
    assert_eq!(reads.calls(), 0);
    assert_eq!(state.clock.lock().unwrap().high_water_ms(), None);
}

#[tokio::test]
async fn event_read_mapped_key_returns_the_seven_fields_keyless() {
    let Some(db) = database().await else { return };
    let _flag = EVENT_READ_FLAG_LOCK.lock().await;
    set_event_read_flag(true);
    map_launch(db.pool()).await;
    let api = MockEventApi::start().await;
    // No Idempotency-Key header: reads are keyless by construction.
    let (status, headers, body) = answer(
        read_app(db.pool().clone(), &api),
        signed_read(&read_payload("launch"), "old"),
    )
    .await;
    set_event_read_flag(false);
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key("idempotent-replay"));
    let result = &body["result"];
    assert_eq!(
        result.as_object().unwrap().len(),
        7,
        "legacy 7-field read contract: {result}"
    );
    assert_eq!(result["outcome"], "read");
    assert_eq!(result["event_id"], READ_EVENT_ID);
    assert_eq!(result["name"], "Launch Night");
    assert_eq!(result["starts_at"], "2026-09-01T20:00:00.000Z");
    assert_eq!(result["location"], "The Hall");
    assert_eq!(result["status"], "SCHEDULED");
    assert!(result["observed_at"].is_string());
    assert_eq!(api.count(), 1, "one Discord GET for one mapped read");
    assert_eq!(api.requests.lock().unwrap()[0]["method"], "GET");
    assert!(api.requests.lock().unwrap()[0]["path"]
        .as_str()
        .unwrap()
        .starts_with(&format!(
            "/api/v10/guilds/{}/scheduled-events/{READ_EVENT_ID}",
            staging_guild()
        )));
    let mirrored: String = sqlx::query_scalar(
        "SELECT name FROM scheduled_events WHERE guild_id = $1 AND event_id = $2",
    )
    .bind(staging_guild())
    .bind(READ_EVENT_ID)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(mirrored, "Launch Night");
    db.close().await.unwrap();
}

#[tokio::test]
async fn event_read_unmapped_key_is_refused_before_any_discord_call() {
    let Some(db) = database().await else { return };
    let _flag = EVENT_READ_FLAG_LOCK.lock().await;
    set_event_read_flag(true);
    let api = MockEventApi::start().await;
    let (status, _, body) = answer(
        read_app(db.pool().clone(), &api),
        signed_read(&read_payload("ghost"), "old"),
    )
    .await;
    set_event_read_flag(false);
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "action_not_allowed");
    assert_eq!(body["error"]["retryable"], false);
    assert_eq!(api.count(), 0, "unmapped keys never reach Discord");
    db.close().await.unwrap();
}

fn moderation_app(pool: sqlx::PgPool, moderation: Arc<MockModeration>) -> Router {
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    router(state_full(
        pool,
        effect,
        Arc::new(MockEventRead::default()),
        moderation,
    ))
}

/// Moderation signing without an `Idempotency-Key` header, for the
/// missing-key refusal path.
fn signed_moderation_without_key(raw: &str, key: &str) -> Request {
    let timestamp = (now_ms() / 1000).to_string();
    let nonce_value = nonce();
    let signature = sign(
        secret(usize::from(key == "new")).as_bytes(),
        &timestamp,
        &nonce_value,
        raw.as_bytes(),
    );
    Request::builder()
        .method(Method::POST)
        .uri(ACTIONS_PATH)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-two-key-id", key)
        .header("x-two-timestamp", timestamp)
        .header("x-two-nonce", nonce_value)
        .header("x-two-signature", signature)
        .body(Body::from(raw.to_owned()))
        .unwrap()
}

#[test]
fn moderation_gates_are_verb_specific() {
    assert_eq!(
        moderation_required_permission(ModerationAction::Ban),
        PERM_BAN_MEMBERS
    );
    assert_eq!(
        moderation_required_permission(ModerationAction::TempBan),
        PERM_BAN_MEMBERS
    );
    assert_eq!(
        moderation_required_permission(ModerationAction::Kick),
        PERM_KICK_MEMBERS
    );
    assert_eq!(
        moderation_required_permission(ModerationAction::Warn),
        PERM_MODERATE_MEMBERS
    );
    assert!(moderation_tolerates_departed_target(ModerationAction::Ban));
    assert!(moderation_tolerates_departed_target(
        ModerationAction::TempBan
    ));
    assert!(!moderation_tolerates_departed_target(
        ModerationAction::Kick
    ));
    assert!(!moderation_tolerates_departed_target(
        ModerationAction::Warn
    ));
    for verb in [
        ModerationAction::Ban,
        ModerationAction::TempBan,
        ModerationAction::Kick,
        ModerationAction::Warn,
    ] {
        assert!(is_wired_moderation_verb(verb), "{verb:?}");
    }
    for verb in [
        ModerationAction::Timeout,
        ModerationAction::Purge,
        ModerationAction::Slowmode,
        ModerationAction::Lockdown,
        ModerationAction::Unlock,
    ] {
        assert!(!is_wired_moderation_verb(verb), "{verb:?}");
    }
}

#[tokio::test]
async fn moderation_flag_off_is_refused_before_any_effect() {
    let Some(db) = database().await else { return };
    let _flag = MODERATION_FLAG_LOCK.lock().await;
    set_moderation_flags(false);
    let moderation = Arc::new(MockModeration::default());
    let app = moderation_app(db.pool().clone(), moderation.clone());
    for verb in [
        "moderation.ban",
        "moderation.tempban",
        "moderation.kick",
        "moderation.warn",
    ] {
        let (status, _, body) = answer(
            app.clone(),
            signed(
                &moderation_payload(verb),
                "old",
                &format!("intent-{verb}-off"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{verb}");
        assert_eq!(body["error"]["code"], "action_not_allowed", "{verb}");
        assert_eq!(body["error"]["retryable"], false, "{verb}");
    }
    set_moderation_flags(true);
    assert_eq!(
        moderation.calls(),
        0,
        "disabled verbs never reach the effect"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn moderation_missing_idempotency_key_is_refused_before_any_effect() {
    let Some(db) = database().await else { return };
    let _flag = MODERATION_FLAG_LOCK.lock().await;
    set_moderation_flags(true);
    let moderation = Arc::new(MockModeration::default());
    let app = moderation_app(db.pool().clone(), moderation.clone());
    let (status, _, body) = answer(
        app,
        signed_moderation_without_key(&moderation_payload("moderation.ban"), "old"),
    )
    .await;
    set_moderation_flags(false);
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "malformed");
    assert_eq!(
        moderation.calls(),
        0,
        "keyless calls never reach the effect"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn moderation_malformed_bodies_are_refused_before_any_effect() {
    let Some(db) = database().await else { return };
    let _flag = MODERATION_FLAG_LOCK.lock().await;
    set_moderation_flags(true);
    let moderation = Arc::new(MockModeration::default());
    let app = moderation_app(db.pool().clone(), moderation.clone());
    for (verb, extra) in [
        (
            "moderation.ban",
            serde_json::json!({"actor_id": "00000000000000000"}),
        ),
        (
            "moderation.kick",
            serde_json::json!({"discord_id": "99999999999999999999"}),
        ),
        (
            "moderation.tempban",
            serde_json::json!({"duration_seconds": 59}),
        ),
        (
            "moderation.tempban",
            serde_json::json!({"duration_seconds": "3600"}),
        ),
        ("moderation.warn", serde_json::json!({"reason": ""})),
        (
            "moderation.warn",
            serde_json::json!({"reason": "x".repeat(513)}),
        ),
    ] {
        let (status, _, body) = answer(
            app.clone(),
            signed(
                &moderation_payload_with(verb, extra),
                "old",
                &format!("intent-malformed-{verb}"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{verb}");
        assert_eq!(body["error"]["code"], "malformed", "{verb}");
    }
    set_moderation_flags(false);
    assert_eq!(
        moderation.calls(),
        0,
        "malformed bodies never reach the effect"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn moderation_success_replays_without_second_effect_but_changed_bytes_conflict() {
    let Some(db) = database().await else { return };
    let _flag = MODERATION_FLAG_LOCK.lock().await;
    set_moderation_flags(true);
    let moderation = Arc::new(MockModeration::default());
    let app = moderation_app(db.pool().clone(), moderation.clone());
    for verb in [
        "moderation.ban",
        "moderation.tempban",
        "moderation.kick",
        "moderation.warn",
    ] {
        let intent = format!("intent-moderation-{verb}");
        let raw = moderation_payload(verb);
        let (status, headers, first) = answer(app.clone(), signed(&raw, "old", &intent)).await;
        assert_eq!(status, StatusCode::OK, "{verb}");
        assert!(!headers.contains_key("idempotent-replay"), "{verb}");
        assert_eq!(
            first["result"]["outcome"],
            moderation_outcome_for(verb),
            "{verb}"
        );
        assert_eq!(first["request_id"].as_str().unwrap().len(), 26, "{verb}");
        // Same intent, fresh nonce: the stored receipt replays, no second effect.
        let (_, headers, replay) = answer(app.clone(), signed(&raw, "new", &intent)).await;
        assert_eq!(headers["idempotent-replay"], "true", "{verb}");
        assert_eq!(first["result"], replay["result"], "{verb}");
        // Same intent with changed exact signed bytes is a caller bug.
        let changed = format!("{raw} ");
        let (status, _, refusal) = answer(app.clone(), signed(&changed, "new", &intent)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{verb}");
        assert_eq!(refusal["error"]["code"], "version_conflict", "{verb}");
        assert_eq!(refusal["error"]["retryable"], false, "{verb}");
    }
    set_moderation_flags(false);
    assert_eq!(
        moderation.calls(),
        4,
        "one effect per verb, replays effect-free"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn unwired_moderation_verbs_stay_refused_with_flags_on() {
    let Some(db) = database().await else { return };
    let _flag = MODERATION_FLAG_LOCK.lock().await;
    set_moderation_flags(true);
    let moderation = Arc::new(MockModeration::default());
    let app = moderation_app(db.pool().clone(), moderation.clone());
    // `moderation.timeout` belongs to the timeout family slice and is still
    // unwired on this branch; the channel verbs belong to the channel slice.
    // This assertion drops its timeout line when the timeout slice merges.
    for (verb, extra) in [
        (
            "moderation.timeout",
            serde_json::json!({"duration_seconds": 60}),
        ),
        ("moderation.purge", serde_json::json!({"count": 10})),
        ("moderation.slowmode", serde_json::json!({"seconds": 5})),
        ("moderation.lockdown", serde_json::json!({})),
        ("moderation.unlock", serde_json::json!({})),
    ] {
        let (status, _, body) = answer(
            app.clone(),
            signed(
                &moderation_payload_with(verb, extra),
                "old",
                &format!("intent-unwired-{verb}"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{verb}");
        assert_eq!(body["error"]["code"], "action_not_allowed", "{verb}");
        assert_eq!(body["error"]["retryable"], false, "{verb}");
    }
    set_moderation_flags(false);
    assert_eq!(
        moderation.calls(),
        0,
        "unwired verbs never reach the effect"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn event_read_flag_off_is_refused_before_any_discord_call() {
    let Some(db) = database().await else { return };
    let _flag = EVENT_READ_FLAG_LOCK.lock().await;
    set_event_read_flag(false);
    map_launch(db.pool()).await;
    let api = MockEventApi::start().await;
    let (status, _, body) = answer(
        read_app(db.pool().clone(), &api),
        signed_read(&read_payload("launch"), "old"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "action_not_allowed");
    assert_eq!(api.count(), 0, "disabled reads never reach Discord");
    db.close().await.unwrap();
}

#[tokio::test]
async fn event_read_malformed_key_is_refused_before_any_discord_call() {
    let Some(db) = database().await else { return };
    let _flag = EVENT_READ_FLAG_LOCK.lock().await;
    set_event_read_flag(true);
    let api = MockEventApi::start().await;
    let app = read_app(db.pool().clone(), &api);
    for raw in [
        r#"{"action":"event.read"}"#,
        r#"{"action":"event.read","event_key":""}"#,
        r#"{"action":"event.read","event_key":"has space"}"#,
    ] {
        let (status, _, body) = answer(app.clone(), signed_read(raw, "old")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{raw}");
        assert_eq!(body["error"]["code"], "malformed");
    }
    set_event_read_flag(false);
    assert_eq!(api.count(), 0, "malformed reads never reach Discord");
    db.close().await.unwrap();
}

#[tokio::test]
async fn event_read_replayed_nonce_is_refused_without_a_second_discord_call() {
    let Some(db) = database().await else { return };
    let _flag = EVENT_READ_FLAG_LOCK.lock().await;
    set_event_read_flag(true);
    map_launch(db.pool()).await;
    let api = MockEventApi::start().await;
    let app = read_app(db.pool().clone(), &api);
    let raw = read_payload("launch");
    let replay = nonce();
    let (first, _, _) = answer(app.clone(), signed_read_with_nonce(&raw, "old", &replay)).await;
    assert_eq!(first, StatusCode::OK);
    let (second, _, refused) = answer(app, signed_read_with_nonce(&raw, "old", &replay)).await;
    set_event_read_flag(false);
    assert_eq!(second, StatusCode::CONFLICT);
    assert_eq!(refused["error"]["code"], "replayed");
    assert_eq!(refused["error"]["retryable"], false);
    assert_eq!(api.count(), 1, "the replay must not reach Discord again");
    db.close().await.unwrap();
}

#[tokio::test]
async fn listeners_fail_closed_and_drain_sibling_on_unexpected_exit() {
    let (shutdown, stopping) = tokio::sync::watch::channel(false);
    let sibling = async {
        crate::server::shutdown_requested(stopping).await;
        Ok(())
    };
    let result =
        crate::website_jobs::serve_listeners(std::future::ready(Ok(())), sibling, shutdown.clone())
            .await;
    assert!(result.is_err());
    assert!(*shutdown.borrow());
}
