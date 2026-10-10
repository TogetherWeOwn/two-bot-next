#![cfg(test)]

use super::*;
use axum::body::Body;
use std::{
    collections::{HashMap, HashSet},
    sync::atomic::{AtomicUsize, Ordering},
};
use tower::ServiceExt;
use two_bot_core::internal_actions::{sign, ActionError, ErrorCode};
use two_bot_core::internal_actions::{GuildAddMemberRequest, RoleAssignRequest};
use two_bot_discord::executor::member::MemberOutcome;
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
        (
            "TWO_INTERNAL_ROLE_KEYS".to_owned(),
            "member:222222222222222222".to_owned(),
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

impl MemberEffect for MockEffect {
    fn execute_assign<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a RoleAssignRequest<'a>,
    ) -> BoxFuture<'a, Result<MemberOutcome, ActionError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.outcome {
                MockOutcome::Success | MockOutcome::Wait => Ok(MemberOutcome::Assigned),
                MockOutcome::NoEffect => Err(ActionError::new(
                    ErrorCode::DiscordRejected,
                    "Discord refused the request",
                    "discord_rejected",
                )),
                MockOutcome::Unknown => Err(ActionError::new(
                    ErrorCode::DiscordUnavailable,
                    "Discord was unreachable",
                    "discord_unreachable",
                )),
            }
        })
    }

    fn execute_add<'a>(
        &'a self,
        _: &'a str,
        _: &'a GuildAddMemberRequest<'a>,
        _: &'a str,
    ) -> BoxFuture<'a, Result<MemberOutcome, ActionError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.outcome {
                MockOutcome::Success | MockOutcome::Wait => Ok(MemberOutcome::Added),
                MockOutcome::NoEffect => Err(ActionError::new(
                    ErrorCode::DiscordRejected,
                    "Discord refused the request",
                    "discord_rejected",
                )),
                MockOutcome::Unknown => Err(ActionError::new(
                    ErrorCode::DiscordUnavailable,
                    "Discord was unreachable",
                    "discord_unreachable",
                )),
            }
        })
    }

    fn resolve_bot<'a>(&'a self) -> BoxFuture<'a, Result<String, ActionError>> {
        Box::pin(async move { Ok("999999999999999999".to_owned()) })
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
    // The announcement double also stands in as the membership double:
    // membership tests share the mock and assert its calls, while moderation
    // tests never reach the member effect.
    Arc::new(ReceiverState::new(
        config(),
        pool,
        effect.clone(),
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

/// `guild.add_member` stays dark until the CEO allowlist decision; the test
/// flag serializes on its own lock so parallel event-read tests are unaffected.
static ADD_MEMBER_FLAG_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn set_add_member_flag(on: bool) {
    if on {
        std::env::set_var("TWO_INTERNAL_ALLOW_ADD_MEMBER", "1");
    } else {
        std::env::remove_var("TWO_INTERNAL_ALLOW_ADD_MEMBER");
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
    if action == "moderation.tempban" || action == "moderation.timeout" {
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
    if (action == "moderation.tempban" || action == "moderation.timeout")
        && extra.get("duration_seconds").is_none()
    {
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
        "moderation.timeout" => "timed_out",
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
    // `role.assign` is now a supported membership action (see the membership
    // test below); unknown verbs and unmapped channel keys still refuse before
    // any effect with no secret echo.
    for (raw, intent) in [
        (
            r#"{"action":"settings.get","key":"secret-sentinel"}"#,
            "intent-unsupported-verb",
        ),
        (
            r#"{"action":"announcement.post","channel_key":"secret-sentinel","body":"secret-sentinel"}"#,
            "intent-bad-channel",
        ),
    ] {
        let (_, _, body) = answer(app.clone(), signed(raw, "old", intent)).await;
        assert_eq!(body["error"]["code"], "action_not_allowed");
        assert!(!body.to_string().contains("secret-sentinel"));
    }
    assert_eq!(effect.calls(), 0);
    db.close().await.unwrap();
}

#[tokio::test]
async fn authenticated_membership_actions_succeed_and_refusals_stay_redacted() {
    let Some(db) = database().await else { return };
    let _flag = ADD_MEMBER_FLAG_LOCK.lock().await;
    // The add-member fence starts dark: without the flag the same signed bytes
    // refuse before any Discord call.
    set_add_member_flag(false);
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let app = router(state(db.pool().clone(), effect.clone()));
    let dark = r#"{"action":"guild.add_member","discord_id":"111111111111111111","access_token":"fixture-oauth"}"#;
    let (_, _, refused) = answer(app.clone(), signed(dark, "old", "intent-add-dark")).await;
    assert_eq!(refused["error"]["code"], "action_not_allowed");
    assert_eq!(effect.calls(), 0);

    set_add_member_flag(true);
    // Happy paths: allowlisted role key and OAuth-backed join through the
    // signed path. Distinct intents so the second claim does not mismatch.
    let role_ok =
        r#"{"action":"role.assign","discord_id":"111111111111111111","role_key":"member"}"#;
    let (status, _, role_first) =
        answer(app.clone(), signed(role_ok, "old", "intent-role-ok")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(role_first["result"], json!({"outcome": "assigned"}));
    assert!(role_first["request_id"].as_str().is_some());
    assert_eq!(effect.calls(), 1);
    // Same intent, fresh nonce: idempotent replay without a second effect.
    let (status, headers, role_replay) =
        answer(app.clone(), signed(role_ok, "new", "intent-role-ok")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["idempotent-replay"], "true");
    assert_eq!(role_replay["result"], json!({"outcome": "assigned"}));
    assert_eq!(effect.calls(), 1);

    let add_ok = r#"{"action":"guild.add_member","discord_id":"111111111111111111","access_token":"fixture-oauth"}"#;
    let (status, _, add_first) = answer(app.clone(), signed(add_ok, "old", "intent-add-ok")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(add_first["result"], json!({"outcome": "added"}));
    assert_eq!(effect.calls(), 2);
    let (status, headers, add_replay) =
        answer(app.clone(), signed(add_ok, "new", "intent-add-ok")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["idempotent-replay"], "true");
    assert_eq!(add_replay["result"], json!({"outcome": "added"}));
    assert_eq!(effect.calls(), 2);

    // Refusal-before-effect: malformed shapes and unknown keys refuse cleanly
    // with no secret echo and no Discord call. Each uses a fresh intent so a
    // refusal never collides with the happy-path claims above.
    for (raw, intent, code) in [
        (
            r#"{"action":"role.assign","discord_id":"111111111111111111","role_key":"secret-sentinel"}"#,
            "intent-role-unknown",
            "action_not_allowed",
        ),
        (
            r#"{"action":"role.assign","discord_id":"bad","role_key":"member"}"#,
            "intent-role-malformed",
            "malformed",
        ),
        (
            r#"{"action":"guild.add_member","discord_id":"bad","access_token":"secret-sentinel"}"#,
            "intent-add-malformed",
            "malformed",
        ),
        (
            r#"{"action":"guild.add_member","discord_id":"111111111111111111"}"#,
            "intent-add-missing-token",
            "malformed",
        ),
    ] {
        let (status, _, body) = answer(app.clone(), signed(raw, "old", intent)).await;
        assert_eq!(body["error"]["code"], code, "{raw}");
        assert!(
            !body.to_string().contains("secret-sentinel"),
            "redacted refusal for {raw}"
        );
        assert!(status == StatusCode::BAD_REQUEST || status == StatusCode::FORBIDDEN);
    }
    assert_eq!(effect.calls(), 2, "refusals must not reach Discord");

    // Receipts are durable: both happy-path intents completed with the
    // stored-member `None` + `affected` contract.
    let completed: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM internal_idempotency WHERE state = 'completed'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(completed, 2);
    set_add_member_flag(false);
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
    let effect: Arc<MockEffect> = Arc::new(MockEffect::new(MockOutcome::Success));
    router(Arc::new(ReceiverState::new(
        config(),
        pool,
        effect.clone(),
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
    assert_eq!(
        moderation_required_permission(ModerationAction::Timeout),
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
    assert!(!moderation_tolerates_departed_target(
        ModerationAction::Timeout
    ));
    for verb in [
        ModerationAction::Ban,
        ModerationAction::TempBan,
        ModerationAction::Kick,
        ModerationAction::Warn,
        ModerationAction::Timeout,
    ] {
        assert!(is_wired_moderation_verb(verb), "{verb:?}");
    }
    for verb in [
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
        "moderation.timeout",
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
        (
            "moderation.timeout",
            serde_json::json!({"duration_seconds": 59}),
        ),
        (
            "moderation.timeout",
            serde_json::json!({"duration_seconds": 28 * 24 * 60 * 60 + 1}),
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
        "moderation.timeout",
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
        5,
        "one effect per verb, replays effect-free"
    );
    db.close().await.unwrap();
}

/// A disabled moderation executor must bind despite invalid moderation
/// settings: the executor never parses gates or secrets while the website
/// verbs are off, so malformed settings must not stop the receiver from
/// starting. Enabled misconfiguration stays fatal (next test).
#[tokio::test]
async fn moderation_executor_disabled_tolerates_invalid_settings() {
    let _flag = MODERATION_FLAG_LOCK.lock().await;
    let prev_allow = std::env::var("TWO_INTERNAL_ALLOW_MODERATION").ok();
    let prev_mod = std::env::var("TWO_MODERATION").ok();
    let prev_owen = std::env::var("TWO_OWEN_USER_ID").ok();
    let prev_protected = std::env::var("TWO_MODERATION_PROTECTED_ROLE_IDS").ok();
    // Stray moderation publish gate without the internal allowlist: every
    // website verb is off, with both invalid-settings shapes present at once.
    std::env::remove_var("TWO_INTERNAL_ALLOW_MODERATION");
    std::env::set_var("TWO_MODERATION", "1");
    std::env::remove_var("TWO_OWEN_USER_ID");
    std::env::set_var("TWO_MODERATION_PROTECTED_ROLE_IDS", "not-a-snowflake");
    let api = MockEventApi::start().await;
    let discord =
        ActionExecutor::with_proxy("not-a-credential".to_owned(), Some(api.origin.clone()))
            .unwrap();
    let result = moderation_executor_from_env(lazy_pool(), discord);
    if let Some(value) = prev_allow {
        std::env::set_var("TWO_INTERNAL_ALLOW_MODERATION", value);
    } else {
        std::env::remove_var("TWO_INTERNAL_ALLOW_MODERATION");
    }
    if let Some(value) = prev_mod {
        std::env::set_var("TWO_MODERATION", value);
    } else {
        std::env::remove_var("TWO_MODERATION");
    }
    if let Some(value) = prev_owen {
        std::env::set_var("TWO_OWEN_USER_ID", value);
    } else {
        std::env::remove_var("TWO_OWEN_USER_ID");
    }
    if let Some(value) = prev_protected {
        std::env::set_var("TWO_MODERATION_PROTECTED_ROLE_IDS", value);
    } else {
        std::env::remove_var("TWO_MODERATION_PROTECTED_ROLE_IDS");
    }
    assert!(
        result.is_ok(),
        "disabled executor must bind despite invalid settings"
    );
}

/// Enabled misconfiguration stays fatal: `TWO_MODERATION=1` with the website
/// verbs on but no valid `TWO_OWEN_USER_ID` must refuse the receiver bind.
#[tokio::test]
async fn moderation_executor_enabled_rejects_invalid_gates() {
    let _flag = MODERATION_FLAG_LOCK.lock().await;
    let prev_owen = std::env::var("TWO_OWEN_USER_ID").ok();
    let prev_protected = std::env::var("TWO_MODERATION_PROTECTED_ROLE_IDS").ok();
    set_moderation_flags(true);
    std::env::remove_var("TWO_OWEN_USER_ID");
    std::env::remove_var("TWO_MODERATION_PROTECTED_ROLE_IDS");
    let api = MockEventApi::start().await;
    let discord =
        ActionExecutor::with_proxy("not-a-credential".to_owned(), Some(api.origin.clone()))
            .unwrap();
    let result = moderation_executor_from_env(lazy_pool(), discord);
    set_moderation_flags(false);
    if let Some(value) = prev_owen {
        std::env::set_var("TWO_OWEN_USER_ID", value);
    } else {
        std::env::remove_var("TWO_OWEN_USER_ID");
    }
    if let Some(value) = prev_protected {
        std::env::set_var("TWO_MODERATION_PROTECTED_ROLE_IDS", value);
    } else {
        std::env::remove_var("TWO_MODERATION_PROTECTED_ROLE_IDS");
    }
    assert!(
        result.is_err(),
        "enabled executor must refuse invalid gates"
    );
}

#[tokio::test]
async fn unwired_moderation_verbs_stay_refused_with_flags_on() {
    let Some(db) = database().await else { return };
    let _flag = MODERATION_FLAG_LOCK.lock().await;
    set_moderation_flags(true);
    let moderation = Arc::new(MockModeration::default());
    let app = moderation_app(db.pool().clone(), moderation.clone());
    // The channel verbs belong to the channel family slice and stay refused.
    for (verb, extra) in [
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

#[test]
fn moderation_permission_union_is_owner_admin_or_role_bits() {
    let guild = staging_guild().to_owned();
    let owner = "123456789012345678".to_owned();
    let admin_role = "199999999999999999".to_owned();
    let mod_role = "222222222222222222".to_owned();
    let facts = ModerationGuildFacts {
        guild_id: guild.clone(),
        owner_id: owner.clone(),
        positions: HashMap::from([
            (guild.clone(), 0),
            (mod_role.clone(), 5),
            (admin_role.clone(), 9),
        ]),
        permissions: HashMap::from([
            (guild.clone(), 0),
            (mod_role.clone(), PERM_KICK_MEMBERS | PERM_MODERATE_MEMBERS),
            (admin_role.clone(), ADMINISTRATOR_BIT),
        ]),
    };
    // The guild owner passes everything, whatever they hold.
    assert_eq!(moderation_permissions(&[], &facts, &owner), Some(u64::MAX));
    // ADMINISTRATOR passes everything for non-owners.
    assert_eq!(
        moderation_permissions(
            &[guild.clone(), admin_role.clone()],
            &facts,
            "111111111111111111"
        ),
        Some(u64::MAX)
    );
    // Ordinary members get the union of the held role bits plus @everyone.
    assert_eq!(
        moderation_permissions(
            &[guild.clone(), mod_role.clone()],
            &facts,
            "111111111111111111"
        ),
        Some(PERM_KICK_MEMBERS | PERM_MODERATE_MEMBERS)
    );
    // An unknown held role fails closed: the snapshot is incomplete, never
    // unprotected.
    assert_eq!(
        moderation_permissions(
            &[guild.clone(), "299999999999999999".to_owned()],
            &facts,
            "111111111111111111"
        ),
        None
    );
    // @everyone membership is additive and idempotent.
    assert_eq!(
        moderation_with_everyone(vec![mod_role.clone()], &guild),
        vec![mod_role.clone(), guild.clone()]
    );
    assert_eq!(
        moderation_with_everyone(vec![mod_role.clone(), guild.clone()], &guild),
        vec![mod_role.clone(), guild.clone()]
    );
    // Hierarchy resolves against the snapshot; unknown roles fail closed.
    assert_eq!(
        moderation_top_position(&[guild.clone(), mod_role.clone()], &facts),
        Some(5)
    );
    assert_eq!(
        moderation_top_position(&["299999999999999999".to_owned()], &facts),
        None
    );
}

/// Loopback Discord double routed by path for the resolve-path tests: guild
/// snapshot, bot identity, member snapshots. The reply set is fixed per test
/// so an unexpected extra Discord call is a visible path, not a shifted
/// canned reply.
struct MockMembers {
    origin: String,
    requests: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}

const RESOLVE_OWNER_ID: &str = "123456789012345678";
const RESOLVE_BOT_ID: &str = "999999999999999999";
const RESOLVE_ACTOR_ID: &str = "111111111111111111";
const RESOLVE_TARGET_ID: &str = "333333333333333333";
const RESOLVE_MOD_ROLE: &str = "222222222222222222";
const RESOLVE_TOP_ROLE: &str = "444444444444444444";

impl MockMembers {
    async fn start(guild_status: u16, target_present: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let guild_id = staging_guild().to_owned();
        let task = tokio::spawn(async move {
            // `fallback` needs a 'static handler: take ownership with `move`
            // and clone the shared log and guild id per request.
            let app = Router::new().fallback(move |request: Request| {
                let guild_id = guild_id.clone();
                let seen = seen.clone();
                async move {
                    let (parts, _) = request.into_parts();
                    let path = parts.uri.path().to_owned();
                    let method = parts.method.clone();
                    seen.lock().unwrap().push(json!({
                        "method": method.as_str(),
                        "path": path,
                    }));
                    let guild_path = format!("/api/v10/guilds/{guild_id}");
                    if path == "/api/v10/users/@me" {
                        return (
                            StatusCode::OK,
                            Json(json!({"id": RESOLVE_BOT_ID, "bot": true})),
                        );
                    }
                    if path == guild_path {
                        if guild_status == StatusCode::OK.as_u16() {
                            return (
                                StatusCode::OK,
                                Json(json!({
                                    "id": guild_id,
                                    "owner_id": RESOLVE_OWNER_ID,
                                    "roles": [
                                        {"id": guild_id, "position": 0, "permissions": "0"},
                                        {"id": RESOLVE_MOD_ROLE, "position": 5,
                                         "permissions": (PERM_KICK_MEMBERS | PERM_MODERATE_MEMBERS).to_string()},
                                        {"id": RESOLVE_TOP_ROLE, "position": 50, "permissions": "0"},
                                    ],
                                })),
                            );
                        }
                        return (
                            StatusCode::from_u16(guild_status)
                                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                            Json(Value::Null),
                        );
                    }
                    let members_prefix = format!("{guild_path}/members/");
                    if let Some(user_id) = path.strip_prefix(members_prefix.as_str()) {
                        let member = |id: &str, roles: Vec<Value>, bot: bool| {
                            json!({"user": {"id": id, "bot": bot}, "roles": roles})
                        };
                        if user_id == RESOLVE_BOT_ID {
                            return (
                                StatusCode::OK,
                                Json(member(
                                    RESOLVE_BOT_ID,
                                    vec![json!(RESOLVE_TOP_ROLE)],
                                    true,
                                )),
                            );
                        }
                        if user_id == RESOLVE_ACTOR_ID {
                            return (
                                StatusCode::OK,
                                Json(member(
                                    RESOLVE_ACTOR_ID,
                                    vec![json!(RESOLVE_MOD_ROLE)],
                                    false,
                                )),
                            );
                        }
                        if user_id == RESOLVE_TARGET_ID && target_present {
                            return (
                                StatusCode::OK,
                                Json(member(RESOLVE_TARGET_ID, vec![], false)),
                            );
                        }
                        return (StatusCode::NOT_FOUND, Json(Value::Null));
                    }
                    (StatusCode::NOT_FOUND, Json(Value::Null))
                }
            });
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            origin,
            requests,
            task,
        }
    }

    fn paths(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter_map(|seen| seen.get("path").and_then(Value::as_str).map(str::to_owned))
            .collect()
    }

    fn methods(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter_map(|seen| {
                seen.get("method")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect()
    }
}

impl Drop for MockMembers {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The real [`ModerationExecutor`] over loopback Discord: deny paths run the
/// production resolver and error mapping, not the offline double.
fn moderation_resolve_app(pool: sqlx::PgPool, mock: &MockMembers) -> Router {
    let discord =
        ActionExecutor::with_proxy("not-a-credential".to_owned(), Some(mock.origin.clone()))
            .unwrap();
    let store = PgMemberModerationStore::new(pool.clone(), staging_guild().to_owned());
    let inner = InternalMemberExecutor::new(
        store,
        discord.clone(),
        InternalMemberConfig {
            guild_id: staging_guild().to_owned(),
            enabled: true,
            policy: ModerationPolicy {
                owen_user_id: RESOLVE_OWNER_ID.to_owned(),
                protected_role_ids: HashSet::new(),
                bot_user_id: None,
            },
            audit_secret: None,
        },
    )
    .unwrap();
    let moderation: Arc<dyn ModerationEffect> = Arc::new(ModerationExecutor::new(inner, discord));
    let effect: Arc<dyn ActionEffect> = Arc::new(MockEffect::new(MockOutcome::Success));
    router(Arc::new(ReceiverState::new(
        config(),
        pool,
        effect,
        Arc::new(MockEventRead::default()),
        moderation,
    )))
}

#[tokio::test]
async fn moderation_resolve_refuses_actor_without_verb_permission() {
    let Some(db) = database().await else { return };
    let _flag = MODERATION_FLAG_LOCK.lock().await;
    set_moderation_flags(true);
    // The fixture actor holds Kick + Moderate Members, never Ban Members.
    let mock = MockMembers::start(200, true).await;
    let app = moderation_resolve_app(db.pool().clone(), &mock);
    let (status, _, body) = answer(
        app,
        signed(
            &moderation_payload("moderation.ban"),
            "old",
            "intent-resolve-forbidden",
        ),
    )
    .await;
    set_moderation_flags(false);
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "action_not_allowed");
    assert_eq!(body["error"]["retryable"], false);
    let paths = mock.paths();
    assert!(
        paths
            .iter()
            .any(|path| path.ends_with(&format!("/guilds/{}", staging_guild()))),
        "the resolver must read live guild facts: {paths:?}"
    );
    assert!(
        !paths.iter().any(|path| path.contains(RESOLVE_TARGET_ID)),
        "the forbidden actor refuses before the target resolves: {paths:?}"
    );
    assert!(
        !mock.methods().iter().any(|method| method != "GET"),
        "a refusal performs no Discord mutation: {:?}",
        mock.paths()
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn moderation_resolve_refuses_departed_kick_target() {
    let Some(db) = database().await else { return };
    let _flag = MODERATION_FLAG_LOCK.lock().await;
    set_moderation_flags(true);
    let mock = MockMembers::start(200, false).await;
    let app = moderation_resolve_app(db.pool().clone(), &mock);
    let (status, _, body) = answer(
        app,
        signed(
            &moderation_payload("moderation.kick"),
            "old",
            "intent-resolve-departed",
        ),
    )
    .await;
    set_moderation_flags(false);
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "action_not_allowed");
    assert_eq!(body["error"]["retryable"], false);
    let paths = mock.paths();
    assert!(
        paths.iter().any(|path| path.contains(RESOLVE_TARGET_ID)),
        "the departed target must be observed, not synthesized: {paths:?}"
    );
    assert!(
        !mock.methods().iter().any(|method| method != "GET"),
        "a refusal performs no Discord mutation: {paths:?}"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn moderation_snapshot_unavailable_stays_fenced_without_effect() {
    let Some(db) = database().await else { return };
    let _flag = MODERATION_FLAG_LOCK.lock().await;
    set_moderation_flags(true);
    let mock = MockMembers::start(500, true).await;
    let app = moderation_resolve_app(db.pool().clone(), &mock);
    let raw = moderation_payload("moderation.ban");
    let (status, _, refusal) = answer(
        app.clone(),
        signed(&raw, "old", "intent-resolve-unavailable"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(refusal["error"]["code"], "needs_reconciliation");
    assert_eq!(refusal["error"]["retryable"], false);
    let calls = mock.paths().len();
    // Same intent, fresh nonce: the claim is retained for reconciliation, so
    // the snapshot is not re-read and no effect runs.
    let (status, _, replay) = answer(
        app,
        signed_with_nonce(&raw, "new", &nonce(), "intent-resolve-unavailable"),
    )
    .await;
    set_moderation_flags(false);
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(replay["error"]["code"], "needs_reconciliation");
    assert_eq!(
        mock.paths().len(),
        calls,
        "reconciliation replays must not touch Discord again"
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
