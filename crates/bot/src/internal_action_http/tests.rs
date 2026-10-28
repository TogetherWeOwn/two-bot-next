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
mod automations;
mod drain_shutdown;
mod receipt_lifecycle;
mod settings;

/// Stored `internal_idempotency` receipt row: action, state, response code,
/// status, resource, affected count, guild, actor, target, resolved role.
type StoredReceiptRow = (
    String,
    String,
    Option<String>,
    Option<i32>,
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// Terminal `internal_action_log` row: action, phase, response code, evidence
/// code, status, guild, target, resolved role.
type TerminalAuditRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<i32>,
    Option<String>,
    Option<String>,
    Option<String>,
);

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
                        outcome: None,
                        stop_reason: None,
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
    state_full(
        pool,
        effect,
        reads,
        Arc::new(MockEventMutate),
        Arc::new(MockModeration::default()),
    )
}

fn state_full(
    pool: sqlx::PgPool,
    effect: Arc<MockEffect>,
    reads: Arc<MockEventRead>,
    mutates: Arc<MockEventMutate>,
    moderation: Arc<MockModeration>,
) -> Arc<ReceiverState> {
    state_full_with_channel(
        pool,
        effect,
        reads,
        mutates,
        moderation,
        Arc::new(MockChannel::default()),
    )
}

fn state_full_with_channel(
    pool: sqlx::PgPool,
    effect: Arc<MockEffect>,
    reads: Arc<MockEventRead>,
    mutates: Arc<MockEventMutate>,
    moderation: Arc<MockModeration>,
    channel: Arc<MockChannel>,
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
        mutates,
        moderation,
        channel,
    ))
}

/// Offline mutation placeholder for harnesses that never send event verbs: the
/// auth/flag/validation fences refuse before this is ever called.
struct MockEventMutate;

impl EventMutateEffect for MockEventMutate {
    fn execute_mutation<'a>(
        &'a self,
        _: &'a str,
        _: &'a EventCall,
    ) -> BoxFuture<'a, Result<Value, EventActionError>> {
        Box::pin(async move { Ok(json!({"outcome": "updated", "event_id": READ_EVENT_ID})) })
    }
}

/// Offline moderation double: the auth/key/flag fences must refuse before
/// this is ever called, so deny-path tests assert `calls() == 0`.
#[derive(Default)]
struct MockModeration {
    calls: AtomicUsize,
    request_ids: std::sync::Mutex<Vec<String>>,
}

impl MockModeration {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn request_ids(&self) -> Vec<String> {
        self.request_ids.lock().unwrap().clone()
    }
}

impl ModerationEffect for MockModeration {
    fn execute_moderation<'a>(
        &'a self,
        request: &'a InternalMemberRequest,
        request_id: &'a str,
        _: &'a str,
        _: i64,
    ) -> BoxFuture<'a, Result<TerminalResponse, ActionError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.request_ids.lock().unwrap().push(request_id.to_owned());
            let target = DiscordId::new(request.target_id()).expect("validated target");
            Ok(TerminalResponse::Success {
                resource_id: Some(target),
                affected: 1,
                outcome: None,
                stop_reason: None,
            })
        })
    }
}

/// Offline channel-moderation double: the auth/key/flag fences must refuse
/// before this is ever called, so deny-path tests assert `calls() == 0`.
/// Success mirrors the planner outcomes (`purged` with its count,
/// `slowmode_updated`, `locked_down`, `unlocked`) without touching Discord.
#[derive(Default)]
struct MockChannel {
    calls: AtomicUsize,
    stop_reason: Option<two_bot_core::PurgeStopReason>,
}

impl MockChannel {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ChannelModerationEffect for MockChannel {
    fn execute_channel<'a>(
        &'a self,
        request: &'a InternalChannelRequest,
        _: &'a str,
        _: &'a str,
        _: &'a str,
    ) -> BoxFuture<'a, Result<InternalChannelResult, ActionError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let (outcome, affected) = match request.action().action_name() {
                "moderation.purge" => (
                    "purged".to_owned(),
                    Some(if self.stop_reason.is_some() { 1 } else { 3 }),
                ),
                "moderation.slowmode" => ("slowmode_updated".to_owned(), None),
                "moderation.lockdown" => ("locked_down".to_owned(), None),
                "moderation.unlock" => ("unlocked".to_owned(), None),
                _ => {
                    return Err(ActionError::new(
                        ErrorCode::ActionNotAllowed,
                        "not a channel moderation action",
                        "moderation_action_mismatch",
                    ));
                }
            };
            Ok(InternalChannelResult {
                outcome,
                affected,
                stop_reason: self.stop_reason,
                replayed: false,
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
static INTERNAL_FLAG_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn set_event_read_flag(on: bool) {
    if on {
        std::env::set_var("TWO_INTERNAL_ALLOW_EVENT_READ", "1");
    } else {
        std::env::remove_var("TWO_INTERNAL_ALLOW_EVENT_READ");
    }
}

fn set_event_cancel_flag(on: bool) {
    if on {
        std::env::set_var("TWO_INTERNAL_ALLOW_EVENT_CANCEL", "1");
    } else {
        std::env::remove_var("TWO_INTERNAL_ALLOW_EVENT_CANCEL");
    }
}

fn upsert_payload(key: &str) -> String {
    serde_json::json!({"action": "event.upsert", "event_key": key, "name": "Launch Night",
        "starts_at": "2026-09-01T19:00:00Z", "ends_at": "2026-09-01T22:00:00Z",
        "location": "The Hall"})
    .to_string()
}

fn cancel_payload(key: &str) -> String {
    serde_json::json!({"action": "event.cancel", "event_key": key}).to_string()
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

/// Serializes the tests that assert exact `internal_action_*_persist_failed`
/// diagnostic counts on the process-global capture, plus the tests that emit
/// those diagnostics: without it a concurrent emitter lands inside another
/// test's window and its absolute count assertion fails (and a panic before a
/// flag reset cascades into flag-gated tests). Acquire FIRST, before any flag
/// lock; flag locks are only ever leaves under it. Held together with
/// `TRACING_DIAGNOSTIC_LOCK` (same capture): acquire this one first.
static PERSIST_DIAGNOSTIC_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn set_moderation_flags(on: bool) {
    if on {
        std::env::set_var("TWO_INTERNAL_ALLOW_MODERATION", "1");
        std::env::set_var("TWO_MODERATION", "1");
    } else {
        std::env::remove_var("TWO_INTERNAL_ALLOW_MODERATION");
        std::env::remove_var("TWO_MODERATION");
    }
}

/// Tests that assert on the process-wide tracing capture serialize here.
/// The capture is global to the test binary and each reader slices from its
/// own start offset, so a parallel test emitting the same diagnostic lines
/// pollutes the slice (seen as `left: 1` vs `0` in the pre-claim test).
/// Hold this lock for the whole test, before any flag lock, in every test
/// that reads or writes these diagnostics. Held together with
/// `PERSIST_DIAGNOSTIC_LOCK` (same capture): acquire that one first.
static TRACING_DIAGNOSTIC_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

fn channel_payload(action: &str) -> String {
    let mut body = serde_json::json!({
        "action": action,
        "actor_id": "111111111111111111",
        "channel_id": "222222222222222222",
        "reason": "spam",
    });
    match action {
        "moderation.purge" => body["count"] = serde_json::json!(10),
        "moderation.slowmode" => body["seconds"] = serde_json::json!(30),
        _ => {}
    }
    body.to_string()
}

fn channel_payload_with(action: &str, extra: Value) -> String {
    let mut body = serde_json::from_str::<Value>(&channel_payload(action)).unwrap();
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

fn unknown_persist_failures(start: usize) -> Vec<String> {
    crate::tracing_capture::global().text()[start..]
        .lines()
        .filter(|line| line.contains(r#"msg="internal_action_unknown_state_persist_failed""#))
        .map(str::to_owned)
        .collect()
}

fn terminal_persist_failures(start: usize) -> Vec<String> {
    crate::tracing_capture::global().text()[start..]
        .lines()
        .filter(|line| line.contains(r#"msg="internal_action_terminal_persist_failed""#))
        .map(str::to_owned)
        .collect()
}

#[test]
fn unknown_persist_class_is_a_fixed_vocabulary() {
    assert_eq!(
        unknown_persist_class(InternalStoreError::InvalidInput),
        "invalid_input"
    );
    assert_eq!(
        unknown_persist_class(InternalStoreError::Unavailable),
        "storage_unavailable"
    );
    assert_eq!(
        unknown_persist_class(InternalStoreError::TransitionRefused),
        "transition_refused"
    );
}

#[tokio::test]
async fn terminal_persist_helper_emits_one_fixed_vocabulary_line_per_op() {
    // Union of main (#941) and PR locks: both guard the same process-global
    // capture. Hold both, always in this order, before any flag lock.
    let _capture = PERSIST_DIAGNOSTIC_LOCK.lock().await;
    let _tracing = TRACING_DIAGNOSTIC_LOCK.lock().await;
    let start = crate::tracing_capture::global().text().len();
    // The post-claim `Malformed` revalidations in `execute_member` use the
    // same deterministic inputs as their pre-claim checks, so no signed HTTP
    // body can make the second validation fail while the first passes; the
    // `finish` failure on those defensive branches is proven here at the
    // helper level, with the HTTP-level no-false-positive check in
    // `member_preclaim_malformed_emits_no_terminal_diagnostic` below.
    for (op, error, class) in [
        (
            "finish",
            InternalStoreError::Unavailable,
            "storage_unavailable",
        ),
        (
            "finish",
            InternalStoreError::TransitionRefused,
            "transition_refused",
        ),
        (
            "release",
            InternalStoreError::Unavailable,
            "storage_unavailable",
        ),
        (
            "release",
            InternalStoreError::TransitionRefused,
            "transition_refused",
        ),
        (
            "event_key",
            InternalStoreError::Unavailable,
            "storage_unavailable",
        ),
        (
            "event_key",
            InternalStoreError::TransitionRefused,
            "transition_refused",
        ),
    ] {
        let before = terminal_persist_failures(start).len();
        log_terminal_persist_failed(ActionLabel::Known("announcement.post"), op, error);
        let after = terminal_persist_failures(start);
        assert_eq!(after.len(), before + 1, "{after:?}");
        let line = after.last().unwrap();
        assert!(line.contains(r#"msg="internal_action_terminal_persist_failed""#));
        assert!(line.contains(&format!(r#"persist_op="{op}""#)));
        assert!(line.contains(&format!(r#"persist_class="{class}""#)));
        assert!(line.contains(r#"action="announcement.post""#));
        for secret in ["intent-fixture", "fixture announcement", "secret-sentinel"] {
            assert!(!line.contains(secret));
        }
    }
    assert_eq!(terminal_persist_failures(start).len(), 6);
}

#[tokio::test]
async fn finish_failure_after_effect_is_not_success_or_permission_to_resend() {
    let Some(db) = database().await else { return };
    // Union of main (#941) and PR locks: same global capture, same order.
    let _capture = PERSIST_DIAGNOSTIC_LOCK.lock().await;
    let _tracing = TRACING_DIAGNOSTIC_LOCK.lock().await;
    let start = crate::tracing_capture::global().text().len();
    let mut mock = MockEffect::new(MockOutcome::Success);
    mock.break_finish = Some(db.pool().clone());
    let effect = Arc::new(mock);
    let app = router(state(db.pool().clone(), effect.clone()));
    let (_, _, first) = answer(app.clone(), signed(payload(), "old", "intent-fixture")).await;
    assert_eq!(first["error"]["code"], "needs_reconciliation");
    // A failed `finish` is now diagnosed even when the follow-up
    // `mark_unknown` also fails: one terminal line plus one unknown-state
    // line. Previously only the unknown-state line existed, so a failed
    // `finish` followed by a successful `mark_unknown` was silent.
    let terminal = terminal_persist_failures(start);
    assert_eq!(terminal.len(), 1, "{terminal:?}");
    assert!(terminal[0].contains(r#"persist_op="finish""#));
    assert!(terminal[0].contains(r#"persist_class="storage_unavailable""#));
    assert!(terminal[0].contains(r#"action="announcement.post""#));
    let failures = unknown_persist_failures(start);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains(r#"persist_class="storage_unavailable""#));
    assert!(failures[0].contains(r#"action="announcement.post""#));
    for secret in [
        first["request_id"].as_str().unwrap(),
        "intent-fixture",
        "fixture announcement",
    ] {
        assert!(!terminal[0].contains(secret));
        assert!(!failures[0].contains(secret));
    }
    sqlx::query("ALTER TABLE receiver_hidden_audit RENAME TO internal_action_log")
        .execute(db.pool())
        .await
        .unwrap();
    let (_, _, second) = answer(app.clone(), signed(payload(), "old", "intent-fixture")).await;
    assert_eq!(second["error"]["code"], "in_progress");
    assert_eq!(effect.calls(), 1);
    assert_eq!(terminal_persist_failures(start).len(), 1);
    assert_eq!(unknown_persist_failures(start).len(), 1);

    let healthy = Arc::new(MockEffect::new(MockOutcome::Unknown));
    let healthy_app = router(state(db.pool().clone(), healthy.clone()));
    let (_, _, marked) = answer(
        healthy_app.clone(),
        signed(payload(), "old", "intent-marked"),
    )
    .await;
    assert_eq!(marked["error"]["code"], "needs_reconciliation");
    let (_, _, repeat) = answer(healthy_app, signed(payload(), "old", "intent-marked")).await;
    assert_eq!(repeat["error"]["code"], "needs_reconciliation");
    assert_eq!(healthy.calls(), 1);
    assert_eq!(terminal_persist_failures(start).len(), 1);
    assert_eq!(unknown_persist_failures(start).len(), 1);

    let mut hidden = MockEffect::new(MockOutcome::Unknown);
    hidden.break_finish = Some(db.pool().clone());
    let hidden = Arc::new(hidden);
    let hidden_app = router(state(db.pool().clone(), hidden.clone()));
    let (_, _, unknown_failed) = answer(
        hidden_app,
        signed(payload(), "old", "intent-unknown-failed"),
    )
    .await;
    assert_eq!(unknown_failed["error"]["code"], "needs_reconciliation");
    sqlx::query("ALTER TABLE receiver_hidden_audit RENAME TO internal_action_log")
        .execute(db.pool())
        .await
        .unwrap();
    // The unknown-only path takes no `finish`, so no terminal line is added
    // here; only the unknown-state write fails.
    assert_eq!(terminal_persist_failures(start).len(), 1);
    let failures = unknown_persist_failures(start);
    assert_eq!(failures.len(), 2, "{failures:?}");
    assert!(failures.iter().all(|line| !line.contains("intent-")));
    assert!(terminal_persist_failures(start)
        .iter()
        .all(|line| !line.contains("intent-")));
    db.close().await.unwrap();
}

/// Counted scripted mutation double for the release/event-key-failure tests:
/// records every entry so a second dispatch fails the test instead of
/// panicking on an exhausted reply queue. `break_release` renames the audit
/// relation inside the effect (after the claim committed, like
/// `MockEffect::break_finish`), so the follow-up `release_proven_not_sent`
/// fails while the claim itself succeeded. `break_event_keys` renames the
/// event-key map instead, so the follow-up `put_event_key` fails after a
/// confirmed write.
struct CountedScriptedMutate {
    calls: AtomicUsize,
    replies: Mutex<Vec<Result<Value, EventActionError>>>,
    break_release: Option<sqlx::PgPool>,
    break_event_keys: Option<sqlx::PgPool>,
}

impl EventMutateEffect for CountedScriptedMutate {
    fn execute_mutation<'a>(
        &'a self,
        _: &'a str,
        _: &'a EventCall,
    ) -> BoxFuture<'a, Result<Value, EventActionError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(pool) = &self.break_release {
                // Dispose only a migrated TestDatabase relation, not a live DB.
                sqlx::query("ALTER TABLE internal_action_log RENAME TO receiver_hidden_audit")
                    .execute(pool)
                    .await
                    .unwrap();
            }
            if let Some(pool) = &self.break_event_keys {
                // Dispose only a migrated TestDatabase relation, not a live DB.
                sqlx::query("ALTER TABLE internal_event_keys RENAME TO receiver_hidden_event_keys")
                    .execute(pool)
                    .await
                    .unwrap();
            }
            self.replies.lock().unwrap().remove(0)
        })
    }
}

#[tokio::test]
async fn event_release_failure_is_logged_without_releasing_claim() {
    let Some(db) = database().await else { return };
    // Union of main (#941) and PR locks: same global capture, same order.
    let _capture = PERSIST_DIAGNOSTIC_LOCK.lock().await;
    let _tracing = TRACING_DIAGNOSTIC_LOCK.lock().await;
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
    set_event_cancel_flag(false);
    let start = crate::tracing_capture::global().text().len();
    let blocked = two_bot_core::send_admission::AdmissionError::Blocked.to_string();
    let mutates: Arc<CountedScriptedMutate> = Arc::new(CountedScriptedMutate {
        calls: AtomicUsize::new(0),
        replies: Mutex::new(vec![Err(EventActionError::Discord(
            DiscordError::Unavailable(blocked),
        ))]),
        break_release: Some(db.pool().clone()),
        break_event_keys: None,
    });
    let app = router(Arc::new(ReceiverState::new(
        config(),
        db.pool().clone(),
        Arc::new(MockEffect::new(MockOutcome::Success)),
        Arc::new(MockEffect::new(MockOutcome::Success)),
        Arc::new(MockEventRead::default()),
        mutates.clone(),
        Arc::new(MockModeration::default()),
        Arc::new(MockChannel::default()),
    )));
    // The effect renames the audit relation after the claim committed, so
    // `release_proven_not_sent` fails after the admission refusal proved
    // nothing was sent.
    let raw = upsert_payload("release-fail");
    let (status, _, first) = answer(app.clone(), signed(&raw, "old", "intent-release-fail")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(first["error"]["code"], "internal");
    let terminal = terminal_persist_failures(start);
    assert_eq!(terminal.len(), 1, "{terminal:?}");
    assert!(terminal[0].contains(r#"persist_op="release""#));
    assert!(terminal[0].contains(r#"persist_class="storage_unavailable""#));
    assert!(terminal[0].contains(r#"action="event.upsert""#));
    assert_eq!(unknown_persist_failures(start).len(), 0);
    for secret in [
        first["request_id"].as_str().unwrap(),
        "intent-release-fail",
        "release-fail",
    ] {
        assert!(!terminal[0].contains(secret));
    }
    assert_eq!(mutates.calls.load(Ordering::SeqCst), 1);
    sqlx::query("ALTER TABLE receiver_hidden_audit RENAME TO internal_action_log")
        .execute(db.pool())
        .await
        .unwrap();
    // The failed release leaves the claim occupied: the same key sees
    // `in_progress` with no second dispatch, no new diagnostic and no
    // released claim.
    let (status, _, repeat) = answer(app.clone(), signed(&raw, "old", "intent-release-fail")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(repeat["error"]["code"], "in_progress");
    assert_eq!(mutates.calls.load(Ordering::SeqCst), 1);
    assert_eq!(terminal_persist_failures(start).len(), 1);
    assert_eq!(unknown_persist_failures(start).len(), 0);
    let state: String =
        sqlx::query_scalar("SELECT state FROM internal_idempotency WHERE action = 'event.upsert'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(state, "in_flight");

    // Success path with a fresh intent: an admission refusal that releases
    // cleanly answers `discord_unavailable` with no diagnostic, and the same
    // key may then retry to a stored receipt.
    let blocked = two_bot_core::send_admission::AdmissionError::Blocked.to_string();
    let ok_mutates: Arc<CountedScriptedMutate> = Arc::new(CountedScriptedMutate {
        calls: AtomicUsize::new(0),
        replies: Mutex::new(vec![
            Err(EventActionError::Discord(DiscordError::Unavailable(
                blocked,
            ))),
            Ok(json!({"event_id": CREATE_EVENT_ID})),
        ]),
        break_release: None,
        break_event_keys: None,
    });
    let ok_app = router(Arc::new(ReceiverState::new(
        config(),
        db.pool().clone(),
        Arc::new(MockEffect::new(MockOutcome::Success)),
        Arc::new(MockEffect::new(MockOutcome::Success)),
        Arc::new(MockEventRead::default()),
        ok_mutates.clone(),
        Arc::new(MockModeration::default()),
        Arc::new(MockChannel::default()),
    )));
    let raw = upsert_payload("release-ok");
    let (status, _, refused) =
        answer(ok_app.clone(), signed(&raw, "old", "intent-release-ok")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(refused["error"]["code"], "discord_unavailable");
    let (status, _, created) =
        answer(ok_app.clone(), signed(&raw, "old", "intent-release-ok")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        created["result"],
        json!({"outcome": "created", "event_id": CREATE_EVENT_ID})
    );
    assert_eq!(ok_mutates.calls.load(Ordering::SeqCst), 2);
    assert_eq!(terminal_persist_failures(start).len(), 1);
    assert_eq!(unknown_persist_failures(start).len(), 0);
    db.close().await.unwrap();
}

#[tokio::test]
async fn event_key_failure_after_confirmed_write_is_logged_as_reconciliation() {
    let Some(db) = database().await else { return };
    // Union of main (#941) and PR locks: same global capture, same order.
    let _capture = PERSIST_DIAGNOSTIC_LOCK.lock().await;
    let _tracing = TRACING_DIAGNOSTIC_LOCK.lock().await;
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
    set_event_cancel_flag(false);
    let start = crate::tracing_capture::global().text().len();
    let mutates: Arc<CountedScriptedMutate> = Arc::new(CountedScriptedMutate {
        calls: AtomicUsize::new(0),
        replies: Mutex::new(vec![Ok(json!({"event_id": CREATE_EVENT_ID}))]),
        break_release: None,
        break_event_keys: Some(db.pool().clone()),
    });
    let app = router(Arc::new(ReceiverState::new(
        config(),
        db.pool().clone(),
        Arc::new(MockEffect::new(MockOutcome::Success)),
        Arc::new(MockEffect::new(MockOutcome::Success)),
        Arc::new(MockEventRead::default()),
        mutates.clone(),
        Arc::new(MockModeration::default()),
        Arc::new(MockChannel::default()),
    )));
    // The effect confirms the write, then the follow-up key registration
    // fails: the response stays reconciliation and one terminal diagnostic
    // names the `event_key` op. Previously this path was silent.
    let raw = upsert_payload("key-fail");
    let (status, _, first) = answer(app.clone(), signed(&raw, "old", "intent-key-fail")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(first["error"]["code"], "needs_reconciliation");
    let terminal = terminal_persist_failures(start);
    assert_eq!(terminal.len(), 1, "{terminal:?}");
    assert!(terminal[0].contains(r#"persist_op="event_key""#));
    assert!(terminal[0].contains(r#"persist_class="storage_unavailable""#));
    assert!(terminal[0].contains(r#"action="event.upsert""#));
    // The key write failed but the unknown-state mark succeeded, so no
    // unknown-state diagnostic is emitted.
    assert_eq!(unknown_persist_failures(start).len(), 0);
    for secret in [
        first["request_id"].as_str().unwrap(),
        "intent-key-fail",
        "key-fail",
        CREATE_EVENT_ID,
    ] {
        assert!(!terminal[0].contains(secret));
    }
    assert_eq!(mutates.calls.load(Ordering::SeqCst), 1);
    sqlx::query("ALTER TABLE receiver_hidden_event_keys RENAME TO internal_event_keys")
        .execute(db.pool())
        .await
        .unwrap();
    // The claim is retained as unknown: the same key replays
    // `needs_reconciliation` with no second dispatch and no new diagnostic.
    let (status, _, repeat) = answer(app.clone(), signed(&raw, "old", "intent-key-fail")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(repeat["error"]["code"], "needs_reconciliation");
    assert_eq!(mutates.calls.load(Ordering::SeqCst), 1);
    assert_eq!(terminal_persist_failures(start).len(), 1);
    assert_eq!(unknown_persist_failures(start).len(), 0);
    let state: String =
        sqlx::query_scalar("SELECT state FROM internal_idempotency WHERE action = 'event.upsert'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(state, "unknown");
    db.close().await.unwrap();
}

#[tokio::test]
async fn member_preclaim_malformed_emits_no_terminal_diagnostic() {
    let Some(db) = database().await else { return };
    // Union of main (#941) and PR locks: same global capture, same order.
    let _capture = PERSIST_DIAGNOSTIC_LOCK.lock().await;
    let _tracing = TRACING_DIAGNOSTIC_LOCK.lock().await;
    let _flag = ADD_MEMBER_FLAG_LOCK.lock().await;
    set_add_member_flag(true);
    let start = crate::tracing_capture::global().text().len();
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let app = router(state(db.pool().clone(), effect.clone()));
    // Pre-claim `Malformed` refusals take no claim and perform no store
    // write, so they must emit no terminal-persist diagnostic. The post-claim
    // `Malformed` revalidations are defensive (same deterministic inputs as
    // the pre-claim checks, proven at the helper level above); these cases
    // pin the no-false-positive side over HTTP.
    for (raw, intent) in [
        (
            r#"{"action":"role.assign","discord_id":"bad","role_key":"member"}"#,
            "intent-member-malformed-id",
        ),
        (
            r#"{"action":"guild.add_member","discord_id":"bad","access_token":"fixture-oauth"}"#,
            "intent-member-malformed-add",
        ),
    ] {
        let (status, _, body) = answer(app.clone(), signed(raw, "old", intent)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "malformed");
        assert!(!body.to_string().contains("fixture-oauth"));
    }
    assert_eq!(effect.calls(), 0);
    assert_eq!(terminal_persist_failures(start).len(), 0);
    assert_eq!(unknown_persist_failures(start).len(), 0);
    let claims: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM internal_idempotency")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(claims, 0);
    set_add_member_flag(false);
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
    // Non-vacuous control: the raw signed bytes DO carry the token pre-persist,
    // so the stored-state scan below would catch a leak instead of passing
    // on empty input.
    assert!(
        add_ok.contains("fixture-oauth"),
        "control: raw guild.add_member bytes must carry the token"
    );
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
    // Wire responses never echo the transient token, including the replay.
    assert!(
        !add_first.to_string().contains("fixture-oauth"),
        "first guild.add_member response must not echo the token"
    );
    assert!(
        !add_replay.to_string().contains("fixture-oauth"),
        "replayed guild.add_member response must not echo the token"
    );
    // Stored-state evidence: the OAuth token is absent from every persisted
    // record for the happy path — the idempotency claim plus its
    // `store.finish` receipt (same row, now completed) and the audit rows.
    let claim_rows: Vec<String> = sqlx::query_scalar(
        "SELECT row_to_json(i)::text FROM internal_idempotency i \
         WHERE action = 'guild.add_member'",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(claim_rows.len(), 1, "exactly one guild.add_member claim");
    for row in &claim_rows {
        assert!(
            !row.contains("fixture-oauth"),
            "OAuth token persisted in idempotency claim/receipt"
        );
    }
    // The receipt is durable: completed success with the stored-member
    // `affected = 1`, no resource, and still no token bytes.
    let state: String = sqlx::query_scalar(
        "SELECT state FROM internal_idempotency WHERE action = 'guild.add_member'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(state, "completed");
    let response_code: String = sqlx::query_scalar(
        "SELECT response_code FROM internal_idempotency WHERE action = 'guild.add_member'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(response_code, "success");
    let affected: i64 = sqlx::query_scalar(
        "SELECT affected FROM internal_idempotency WHERE action = 'guild.add_member'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(affected, 1);
    let resource_id: Option<String> = sqlx::query_scalar(
        "SELECT resource_id FROM internal_idempotency WHERE action = 'guild.add_member'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(resource_id, None);
    let audit_rows: Vec<String> = sqlx::query_scalar(
        "SELECT row_to_json(a)::text FROM internal_action_log a \
         WHERE intent_id IN (SELECT intent_id FROM internal_idempotency \
         WHERE action = 'guild.add_member') ORDER BY audit_id",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(audit_rows.len(), 2, "intent + terminal audit rows");
    for row in &audit_rows {
        assert!(
            !row.contains("fixture-oauth"),
            "OAuth token persisted in audit row"
        );
    }
    // Sweep every persisted internal-action record so a cross-row leak
    // cannot hide outside the happy-path intent.
    let persisted: Vec<String> = sqlx::query_scalar(
        "SELECT row_to_json(i)::text FROM internal_idempotency i \
         UNION ALL SELECT row_to_json(a)::text FROM internal_action_log a",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    for row in &persisted {
        assert!(
            !row.contains("fixture-oauth"),
            "OAuth token persisted in stored internal-action record"
        );
    }

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
    // stored-member `None` + `affected` contract. The stored rows carry the
    // executed outcome (`affected` 1 renders as the verb's applied outcome)
    // with the request linkage, so a receipt completing with the wrong
    // outcome or linkage fails here, not just on the envelope.
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM internal_idempotency")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(total, 2, "replays and refusals create no extra intent rows");
    let completed: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM internal_idempotency WHERE state = 'completed'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(completed, 2, "both membership intents complete");
    // Stored receipt shape per verb, ordered by action so the linkage pins
    // without echoing request bytes: `guild.add_member` sorts before
    // `role.assign`. Only scalar linkage columns are selected; no hashes,
    // tokens or raw bytes enter the assertion output.
    let rows: Vec<StoredReceiptRow> = sqlx::query_as(
        "SELECT action, state, response_code, http_status, resource_id, affected, \
         guild_id, actor_id, target_id, resolved_role_id \
         FROM internal_idempotency ORDER BY action",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(rows.len(), 2, "two stored membership receipts");
    let guild = staging_guild();
    // `guild.add_member`: applied `added` is `affected` 1 with member linkage
    // and no resolved role.
    assert_eq!(rows[0].0, "guild.add_member", "add receipt action");
    assert_eq!(rows[0].1, "completed", "add receipt state");
    assert_eq!(rows[0].2.as_deref(), Some("success"), "add receipt code");
    assert_eq!(rows[0].3, Some(200), "add receipt status");
    assert_eq!(rows[0].4, None, "add receipt has no message resource");
    assert_eq!(rows[0].5, Some(1), "add receipt carries the applied effect");
    assert_eq!(
        rows[0].6.as_deref(),
        Some(guild),
        "add receipt guild linkage"
    );
    assert_eq!(rows[0].7, None, "add receipt has no actor linkage");
    assert_eq!(
        rows[0].8.as_deref(),
        Some("111111111111111111"),
        "add receipt target linkage"
    );
    assert_eq!(rows[0].9, None, "add receipt has no resolved role");
    // `role.assign`: applied `assigned` is `affected` 1 with the allowlisted
    // role pinned at claim time.
    assert_eq!(rows[1].0, "role.assign", "assign receipt action");
    assert_eq!(rows[1].1, "completed", "assign receipt state");
    assert_eq!(rows[1].2.as_deref(), Some("success"), "assign receipt code");
    assert_eq!(rows[1].3, Some(200), "assign receipt status");
    assert_eq!(rows[1].4, None, "assign receipt has no message resource");
    assert_eq!(
        rows[1].5,
        Some(1),
        "assign receipt carries the applied effect"
    );
    assert_eq!(
        rows[1].6.as_deref(),
        Some(guild),
        "assign receipt guild linkage"
    );
    assert_eq!(rows[1].7, None, "assign receipt has no actor linkage");
    assert_eq!(
        rows[1].8.as_deref(),
        Some("111111111111111111"),
        "assign receipt target linkage"
    );
    assert_eq!(
        rows[1].9.as_deref(),
        Some("222222222222222222"),
        "assign receipt resolved-role linkage"
    );
    // The replayed envelopes above (`assigned` / `added` with
    // `idempotent-replay`) return these stored rows: the stored `affected` 1
    // is what renders each applied outcome, and the effect count proves no
    // second dispatch ran.
    assert_eq!(role_replay["result"], json!({"outcome": "assigned"}));
    assert_eq!(add_replay["result"], json!({"outcome": "added"}));
    // Terminal audit rows mirror the same outcome linkage with executor
    // evidence, proving `store.finish` carried it to the ledger as well.
    let logs: Vec<TerminalAuditRow> = sqlx::query_as(
        "SELECT action, phase, response_code, evidence_code, http_status, \
         guild_id, target_id, resolved_role_id \
         FROM internal_action_log WHERE phase = 'terminal' ORDER BY action",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(logs.len(), 2, "two terminal audit rows");
    assert_eq!(logs[0].0, "guild.add_member", "add audit action");
    assert_eq!(logs[0].1, "terminal", "add audit phase");
    assert_eq!(logs[0].2.as_deref(), Some("success"), "add audit code");
    assert_eq!(logs[0].3.as_deref(), Some("executor"), "add audit evidence");
    assert_eq!(logs[0].4, Some(200), "add audit status");
    assert_eq!(logs[0].5.as_deref(), Some(guild), "add audit guild linkage");
    assert_eq!(
        logs[0].6.as_deref(),
        Some("111111111111111111"),
        "add audit target linkage"
    );
    assert_eq!(logs[0].7, None, "add audit has no resolved role");
    assert_eq!(logs[1].0, "role.assign", "assign audit action");
    assert_eq!(logs[1].1, "terminal", "assign audit phase");
    assert_eq!(logs[1].2.as_deref(), Some("success"), "assign audit code");
    assert_eq!(
        logs[1].3.as_deref(),
        Some("executor"),
        "assign audit evidence"
    );
    assert_eq!(logs[1].4, Some(200), "assign audit status");
    assert_eq!(
        logs[1].5.as_deref(),
        Some(guild),
        "assign audit guild linkage"
    );
    assert_eq!(
        logs[1].6.as_deref(),
        Some("111111111111111111"),
        "assign audit target linkage"
    );
    assert_eq!(
        logs[1].7.as_deref(),
        Some("222222222222222222"),
        "assign audit resolved-role linkage"
    );
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
const CREATE_EVENT_ID: &str = "100000000000000011";

fn staging_guild() -> &'static str {
    two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID
}

fn discord_event() -> Value {
    discord_event_for(READ_EVENT_ID, 1)
}

fn discord_event_for(event_id: &str, status: i64) -> Value {
    json!({
        "id": event_id,
        "guild_id": staging_guild(),
        "name": "Launch Night",
        "scheduled_start_time": "2026-09-01T20:00:00.000Z",
        "channel_id": Value::Null,
        "description": Value::Null,
        "entity_metadata": {"location": "The Hall"},
        "status": status,
    })
}

/// One scripted Discord answer, consumed in request order. Exhaustion falls
/// back to 500 so an unexpected second effect fails loudly, never silently.
struct ScriptedEvent {
    status: StatusCode,
    body: Value,
}

impl MockEventApi {
    async fn start() -> Self {
        Self::start_scripted(vec![ScriptedEvent {
            status: StatusCode::OK,
            body: discord_event(),
        }])
        .await
    }

    async fn start_scripted(scripts: Vec<ScriptedEvent>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let replies = Arc::new(Mutex::new(scripts));
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().fallback(|request: Request| async move {
                    let (parts, body) = request.into_parts();
                    let raw = to_bytes(body, 8192).await.unwrap_or_default();
                    let sent: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
                    seen.lock().unwrap().push(json!({
                        "method": parts.method.as_str(),
                        "path": parts.uri.path(),
                        "body": sent,
                    }));
                    let mut replies = replies.lock().unwrap();
                    if replies.is_empty() {
                        return (StatusCode::INTERNAL_SERVER_ERROR, Json(Value::Null))
                            .into_response();
                    }
                    let next = replies.remove(0);
                    (next.status, Json(next.body)).into_response()
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
        Arc::new(MockEventMutate),
        Arc::new(MockModeration::default()),
        Arc::new(MockChannel::default()),
    )))
}

fn mutate_app(pool: sqlx::PgPool, api: &MockEventApi) -> Router {
    let executor =
        ActionExecutor::with_proxy("not-a-credential".to_owned(), Some(api.origin.clone()))
            .unwrap();
    let reads: Arc<dyn EventReadEffect> = Arc::new(MockEventRead::default());
    let effect: Arc<dyn ActionEffect> = Arc::new(MockEffect::new(MockOutcome::Success));
    let mutates: Arc<dyn EventMutateEffect> =
        Arc::new(EventMutationExecutor::new(executor, pool.clone()));
    router(Arc::new(ReceiverState::new(
        config(),
        pool,
        effect,
        Arc::new(MockEffect::new(MockOutcome::Success)),
        reads,
        mutates,
        Arc::new(MockModeration::default()),
        Arc::new(MockChannel::default()),
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
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
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
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
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
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
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
        Arc::new(MockEventMutate),
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

#[tokio::test]
async fn moderation_ledger_ids_are_not_caller_request_ids() {
    let Some(db) = database().await else { return };
    let _flag = MODERATION_FLAG_LOCK.lock().await;
    set_moderation_flags(true);
    let moderation = Arc::new(MockModeration::default());
    let app = moderation_app(db.pool().clone(), moderation.clone());
    let caller_id = two_bot_core::internal_actions::new_request_id(now_ms(), &[7; 16]);

    for intent in ["intent-warn-one", "intent-warn-two"] {
        let mut request = signed(&moderation_payload("moderation.warn"), "old", intent);
        request
            .headers_mut()
            .insert("x-request-id", HeaderValue::from_str(&caller_id).unwrap());
        let (status, _, body) = answer(app.clone(), request).await;
        assert_eq!(status, StatusCode::OK, "{intent}: {body}");
        assert_eq!(body["request_id"], caller_id);
    }

    set_moderation_flags(false);
    assert_eq!(moderation.calls(), 2);
    let ledger_ids = moderation.request_ids();
    assert_eq!(ledger_ids.len(), 2);
    assert_ne!(ledger_ids[0], caller_id);
    assert_ne!(ledger_ids[1], caller_id);
    assert_ne!(ledger_ids[0], ledger_ids[1]);
    db.close().await.unwrap();
}

#[tokio::test]
async fn moderation_channel_flag_off_is_refused_before_any_effect() {
    let Some(db) = database().await else { return };
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let moderation = Arc::new(MockModeration::default());
    let channel = Arc::new(MockChannel::default());
    let app = router(state_full_with_channel(
        db.pool().clone(),
        effect.clone(),
        Arc::new(MockEventRead::default()),
        Arc::new(MockEventMutate),
        moderation.clone(),
        channel.clone(),
    ));
    let _flag = MODERATION_FLAG_LOCK.lock().await;
    set_moderation_flags(false);
    for action in [
        "moderation.purge",
        "moderation.slowmode",
        "moderation.lockdown",
        "moderation.unlock",
    ] {
        let (status, _, body) = answer(
            app.clone(),
            signed(&channel_payload(action), "old", "intent-channel-flag-off"),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{action}");
        assert_eq!(body["error"]["code"], "action_not_allowed");
        assert_eq!(body["error"]["retryable"], false);
    }
    assert_eq!(effect.calls(), 0);
    assert_eq!(moderation.calls(), 0);
    assert_eq!(channel.calls(), 0);
    db.close().await.unwrap();
}

#[tokio::test]
async fn moderation_channel_happy_paths_emit_planner_outcomes() {
    let Some(db) = database().await else { return };
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let moderation = Arc::new(MockModeration::default());
    let channel = Arc::new(MockChannel::default());
    let app = router(state_full_with_channel(
        db.pool().clone(),
        effect.clone(),
        Arc::new(MockEventRead::default()),
        Arc::new(MockEventMutate),
        moderation.clone(),
        channel.clone(),
    ));
    let _flag = MODERATION_FLAG_LOCK.lock().await;
    set_moderation_flags(true);
    for (action, intent, expected, expected_affected) in [
        (
            "moderation.purge",
            "intent-channel-purge",
            serde_json::json!({"outcome": "purged", "affected": 3}),
            3_i64,
        ),
        (
            "moderation.slowmode",
            "intent-channel-slowmode",
            serde_json::json!({"outcome": "slowmode_updated"}),
            1_i64,
        ),
        (
            "moderation.lockdown",
            "intent-channel-lockdown",
            serde_json::json!({"outcome": "locked_down"}),
            1_i64,
        ),
        (
            "moderation.unlock",
            "intent-channel-unlock",
            serde_json::json!({"outcome": "unlocked"}),
            1_i64,
        ),
    ] {
        let (status, headers, body) =
            answer(app.clone(), signed(&channel_payload(action), "old", intent)).await;
        assert_eq!(status, StatusCode::OK, "{action}");
        assert!(!headers.contains_key("idempotent-replay"), "{action}");
        assert_eq!(body["result"], expected, "{action}");
        assert_eq!(body["request_id"].as_str().unwrap().len(), 26, "{action}");
        // The planner outcome renders on the wire (asserted above) while the
        // persisted `store.finish` receipt carries it as the stored
        // `affected` count plus the request linkage: moderated channel as both
        // `resource_id` and `target_id`, actor and staging guild scalars.
        // Reuses the canonical `StoredReceiptRow` shape so clippy's
        // `type_complexity` stays quiet.
        let row: StoredReceiptRow = sqlx::query_as(
            "SELECT action, state, response_code, http_status, resource_id, affected, \
             guild_id, actor_id, target_id, resolved_role_id FROM internal_idempotency \
             WHERE action = $1",
        )
        .bind(action)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(row.0, action, "{action}");
        assert_eq!(row.1, "completed", "{action}");
        assert_eq!(row.2.as_deref(), Some("success"), "{action}");
        assert_eq!(row.3, Some(200), "{action}");
        assert_eq!(
            row.4.as_deref(),
            Some("222222222222222222"),
            "{action}: stored channel linkage"
        );
        assert_eq!(
            row.5,
            Some(expected_affected),
            "{action}: stored planner count"
        );
        assert_eq!(row.6.as_deref(), Some(staging_guild()), "{action}");
        assert_eq!(row.7.as_deref(), Some("111111111111111111"), "{action}");
        assert_eq!(row.8.as_deref(), Some("222222222222222222"), "{action}");
        assert_eq!(row.9, None, "{action}: no resolved role");
    }
    set_moderation_flags(false);
    assert_eq!(channel.calls(), 4);
    assert_eq!(effect.calls(), 0);
    assert_eq!(moderation.calls(), 0);
    db.close().await.unwrap();
}

#[tokio::test]
async fn moderation_channel_bad_channel_keys_are_refused_before_any_effect() {
    let Some(db) = database().await else { return };
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let moderation = Arc::new(MockModeration::default());
    let channel = Arc::new(MockChannel::default());
    let app = router(state_full_with_channel(
        db.pool().clone(),
        effect.clone(),
        Arc::new(MockEventRead::default()),
        Arc::new(MockEventMutate),
        moderation.clone(),
        channel.clone(),
    ));
    let _flag = MODERATION_FLAG_LOCK.lock().await;
    set_moderation_flags(true);
    // Malformed actor/channel identities, out-of-range numbers and missing
    // fields must refuse before the channel executor runs: the mock counts
    // every entry, so any call here is a refusal-before-effect failure.
    for (action, extra) in [
        (
            "moderation.purge",
            serde_json::json!({"channel_id": "not-a-snowflake"}),
        ),
        (
            "moderation.slowmode",
            serde_json::json!({"channel_id": "00000000000000000"}),
        ),
        (
            "moderation.lockdown",
            serde_json::json!({"channel_id": "99999999999999999999"}),
        ),
        (
            "moderation.unlock",
            serde_json::json!({"channel_id": "022222222222222222"}),
        ),
        (
            "moderation.purge",
            serde_json::json!({"actor_id": "not-a-snowflake"}),
        ),
        ("moderation.purge", serde_json::json!({"count": 0})),
        ("moderation.purge", serde_json::json!({"count": 101})),
        ("moderation.purge", serde_json::json!({"count": "10"})),
        ("moderation.slowmode", serde_json::json!({"seconds": 21601})),
        ("moderation.slowmode", serde_json::json!({"seconds": "30"})),
        ("moderation.lockdown", serde_json::json!({"reason": ""})),
        (
            "moderation.unlock",
            serde_json::json!({"reason": "x".repeat(513)}),
        ),
    ] {
        let raw = channel_payload_with(action, extra);
        let (status, _, body) =
            answer(app.clone(), signed(&raw, "old", "intent-channel-bad-key")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{action}: {raw}");
        assert_eq!(body["error"]["code"], "malformed", "{action}: {raw}");
    }
    // A well-formed purge without its required count is malformed, not a
    // default: the existing 1–100 path requires the field.
    let missing_count = serde_json::json!({
        "action": "moderation.purge",
        "actor_id": "111111111111111111",
        "channel_id": "222222222222222222",
        "reason": "spam",
    })
    .to_string();
    let (status, _, body) = answer(
        app.clone(),
        signed(&missing_count, "old", "intent-channel-bad-key"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "malformed");
    set_moderation_flags(false);
    assert_eq!(channel.calls(), 0);
    assert_eq!(effect.calls(), 0);
    assert_eq!(moderation.calls(), 0);
    db.close().await.unwrap();
}

#[tokio::test]
async fn moderation_channel_success_replays_and_mismatches_like_announcements() {
    let Some(db) = database().await else { return };
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let moderation = Arc::new(MockModeration::default());
    let channel = Arc::new(MockChannel::default());
    let app = router(state_full_with_channel(
        db.pool().clone(),
        effect.clone(),
        Arc::new(MockEventRead::default()),
        Arc::new(MockEventMutate),
        moderation.clone(),
        channel.clone(),
    ));
    let _flag = MODERATION_FLAG_LOCK.lock().await;
    set_moderation_flags(true);
    let raw = channel_payload("moderation.lockdown");
    let (status, headers, first) =
        answer(app.clone(), signed(&raw, "old", "intent-channel-fixture")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key("idempotent-replay"));
    assert_eq!(
        first["result"],
        serde_json::json!({"outcome": "locked_down"})
    );
    assert_eq!(
        channel.calls(),
        1,
        "first lockdown reaches the channel effect once"
    );
    let restarted = router(state_full_with_channel(
        db.independent_pool().await.unwrap(),
        effect.clone(),
        Arc::new(MockEventRead::default()),
        Arc::new(MockEventMutate),
        moderation.clone(),
        channel.clone(),
    ));
    let (status, headers, replay) = answer(
        restarted.clone(),
        signed(&raw, "new", "intent-channel-fixture"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["idempotent-replay"], "true");
    assert_eq!(first["result"], replay["result"]);
    assert_eq!(
        channel.calls(),
        1,
        "idempotent replay returns the stored record without a second Discord call"
    );
    // The stored receipt keeps the planner linkage: completed success for the
    // lockdown verb, moderated channel as resource and target, actor and guild.
    let row: StoredReceiptRow = sqlx::query_as(
        "SELECT action, state, response_code, http_status, resource_id, affected, \
         guild_id, actor_id, target_id, resolved_role_id FROM internal_idempotency \
         WHERE state = 'completed'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(row.0, "moderation.lockdown");
    assert_eq!(row.1, "completed");
    assert_eq!(row.2.as_deref(), Some("success"));
    assert_eq!(row.3, Some(200));
    assert_eq!(row.4.as_deref(), Some("222222222222222222"));
    assert_eq!(row.5, Some(1));
    assert_eq!(row.6.as_deref(), Some(staging_guild()));
    assert_eq!(row.7.as_deref(), Some("111111111111111111"));
    assert_eq!(row.8.as_deref(), Some("222222222222222222"));
    assert_eq!(row.9, None, "lockdown receipt has no resolved role");
    let changed = format!("{raw} ");
    let (status, _, mismatch) =
        answer(restarted, signed(&changed, "new", "intent-channel-fixture")).await;
    set_moderation_flags(false);
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(mismatch["error"]["retryable"], false);
    assert_eq!(
        channel.calls(),
        1,
        "payload mismatch still performs no second Discord call"
    );
    assert_eq!(effect.calls(), 0);
    assert_eq!(moderation.calls(), 0);
    db.close().await.unwrap();
}

/// Dropping the executor's reason on either HTTP success or persisted outer
/// replay loses information. A completed purge must keep its original bytes.
#[tokio::test]
async fn purge_stop_reason_http_receipt_survives_outer_replay() {
    use two_bot_core::PurgeStopReason::{RateLimited, Refused};

    let Some(db) = database().await else { return };
    let _flag = MODERATION_FLAG_LOCK.lock().await;
    for (stop_reason, intent, expected_result) in [
        (
            Some(Refused),
            "purge-http-refused",
            r#"{"outcome":"purged","affected":1,"stop_reason":"refused"}"#,
        ),
        (
            Some(RateLimited),
            "purge-http-rate-limited",
            r#"{"outcome":"purged","affected":1,"stop_reason":"rate_limited"}"#,
        ),
        (
            None,
            "purge-http-completed",
            r#"{"outcome":"purged","affected":3}"#,
        ),
    ] {
        let effect = Arc::new(MockEffect::new(MockOutcome::Success));
        let moderation = Arc::new(MockModeration::default());
        let channel = Arc::new(MockChannel {
            stop_reason,
            ..MockChannel::default()
        });
        let app = router(state_full_with_channel(
            db.pool().clone(),
            effect.clone(),
            Arc::new(MockEventRead::default()),
            Arc::new(MockEventMutate),
            moderation.clone(),
            channel.clone(),
        ));
        let raw = channel_payload("moderation.purge");
        let mut fresh = signed(&raw, "old", intent);
        fresh.headers_mut().insert(
            "x-request-id",
            HeaderValue::from_static("01ARZ3NDEKTSV4RRFFQ69G5FAV"),
        );
        set_moderation_flags(true);
        let response = app.oneshot(fresh).await.unwrap();
        let fresh_status = response.status();
        let fresh_headers = response.headers().clone();
        let fresh_bytes = to_bytes(response.into_body(), 8192).await.unwrap();
        let calls_after_fresh = channel.calls();
        // Rebuild the receiver over an independent connection: the repeated
        // result must come from the durable outer receipt, not a memory cache.
        let replay_pool = db.independent_pool().await.unwrap();
        let restarted = router(state_full_with_channel(
            replay_pool.clone(),
            effect.clone(),
            Arc::new(MockEventRead::default()),
            Arc::new(MockEventMutate),
            moderation.clone(),
            channel.clone(),
        ));
        let mut repeat = signed(&raw, "new", intent);
        repeat.headers_mut().insert(
            "x-request-id",
            HeaderValue::from_static("01ARZ3NDEKTSV4RRFFQ69G5FAV"),
        );
        let response = restarted.oneshot(repeat).await.unwrap();
        let replay_status = response.status();
        let replay_headers = response.headers().clone();
        let replay_bytes = to_bytes(response.into_body(), 8192).await.unwrap();
        set_moderation_flags(false);
        replay_pool.close().await;

        let expected = format!(
            r#"{{"ok":true,"result":{expected_result},"request_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV"}}"#,
        );
        assert_eq!(fresh_status, StatusCode::OK, "{intent}");
        assert!(!fresh_headers.contains_key("idempotent-replay"), "{intent}");
        assert_eq!(fresh_bytes.as_ref(), expected.as_bytes(), "{intent}: fresh");
        assert_eq!(calls_after_fresh, 1, "{intent}: first effect");
        assert_eq!(replay_status, StatusCode::OK, "{intent}");
        assert_eq!(replay_headers["idempotent-replay"], "true", "{intent}");
        assert_eq!(
            replay_bytes.as_ref(),
            expected.as_bytes(),
            "{intent}: replay"
        );
        assert_eq!(channel.calls(), 1, "{intent}: no repeated effect");
        assert_eq!(effect.calls(), 0);
        assert_eq!(moderation.calls(), 0);
    }
    db.close().await.unwrap();
}

/// Channel permission gates pin the live-snapshot check itself: purge needs
/// Manage Messages, slowmode/lockdown/unlock need Manage Channels. Deleting
/// the per-verb check turns these refusals into resolution successes.
#[test]
fn channel_required_permission_covers_purge_and_manage_channels_verbs() {
    assert_eq!(
        channel_required_permission(ModerationAction::Purge),
        PERM_MANAGE_MESSAGES
    );
    for action in [
        ModerationAction::Slowmode,
        ModerationAction::Lockdown,
        ModerationAction::Unlock,
    ] {
        assert_eq!(
            channel_required_permission(action),
            PERM_MANAGE_CHANNELS,
            "{}",
            action.action_name()
        );
    }
}

/// A disabled channel executor must bind despite invalid moderation gates:
/// a bad `TWO_MODERATION_PROTECTED_ROLE_IDS` (or a stray `TWO_MODERATION=1`
/// without `TWO_OWEN_USER_ID`) must not stop the receiver from starting while
/// the channel verbs are off. Enabled misconfiguration stays fatal (next test).
#[tokio::test]
async fn channel_executor_disabled_tolerates_invalid_gates() {
    let _flag = MODERATION_FLAG_LOCK.lock().await;
    let prev_allow = std::env::var("TWO_INTERNAL_ALLOW_MODERATION").ok();
    let prev_mod = std::env::var("TWO_MODERATION").ok();
    let prev_owen = std::env::var("TWO_OWEN_USER_ID").ok();
    let prev_protected = std::env::var("TWO_MODERATION_PROTECTED_ROLE_IDS").ok();
    // Stray moderation publish gate without the internal allowlist: the
    // channel verbs are off, with both invalid-gate shapes present at once.
    std::env::remove_var("TWO_INTERNAL_ALLOW_MODERATION");
    std::env::set_var("TWO_MODERATION", "1");
    std::env::remove_var("TWO_OWEN_USER_ID");
    std::env::set_var("TWO_MODERATION_PROTECTED_ROLE_IDS", "not-a-snowflake");
    let api = MockEventApi::start().await;
    let discord =
        ActionExecutor::with_proxy("not-a-credential".to_owned(), Some(api.origin.clone()))
            .unwrap();
    let result = channel_executor_from_env(lazy_pool(), discord);
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
        "disabled channel executor must bind despite invalid gates"
    );
}

/// Enabled misconfiguration stays fatal: `TWO_MODERATION=1` with the channel
/// verbs on but no valid `TWO_OWEN_USER_ID` must refuse the receiver bind.
#[tokio::test]
async fn channel_executor_enabled_rejects_invalid_gates() {
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
    let result = channel_executor_from_env(lazy_pool(), discord);
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
        "enabled channel executor must refuse invalid gates"
    );
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
async fn all_nine_moderation_verbs_pass_admission_with_flags_on() {
    let Some(db) = database().await else { return };
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let moderation = Arc::new(MockModeration::default());
    let channel = Arc::new(MockChannel::default());
    let app = router(state_full_with_channel(
        db.pool().clone(),
        effect.clone(),
        Arc::new(MockEventRead::default()),
        Arc::new(MockEventMutate),
        moderation.clone(),
        channel.clone(),
    ));
    let _flag = MODERATION_FLAG_LOCK.lock().await;
    set_moderation_flags(true);
    // Both families are wired now: the five member verbs and the four
    // channel verbs all pass the action_not_allowed gate. Per-verb outcomes
    // live in the family happy-path tests; this pins the dispatch union so a
    // future merge cannot silently drop a verb back to refused.
    for action in [
        "moderation.ban",
        "moderation.tempban",
        "moderation.kick",
        "moderation.warn",
        "moderation.timeout",
    ] {
        let (status, _, body) = answer(
            app.clone(),
            signed(
                &moderation_payload(action),
                "old",
                &format!("intent-all-wired-{action}"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{action}");
        assert_ne!(body["error"]["code"], "action_not_allowed", "{action}");
    }
    for action in [
        "moderation.purge",
        "moderation.slowmode",
        "moderation.lockdown",
        "moderation.unlock",
    ] {
        let (status, _, body) = answer(
            app.clone(),
            signed(
                &channel_payload(action),
                "old",
                &format!("intent-all-wired-{action}"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{action}");
        assert_ne!(body["error"]["code"], "action_not_allowed", "{action}");
    }
    set_moderation_flags(false);
    assert_eq!(
        moderation.calls(),
        5,
        "member verbs reach the member effect"
    );
    assert_eq!(channel.calls(), 4, "channel verbs reach the channel effect");
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
    // The membership double shares the announcement mock: deny-path tests refuse
    // before any member effect runs, matching `state_full`.
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    router(Arc::new(ReceiverState::new(
        config(),
        pool,
        effect.clone(),
        effect,
        Arc::new(MockEventRead::default()),
        Arc::new(MockEventMutate),
        moderation,
        Arc::new(MockChannel::default()),
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

/// Read one `two_bot_moderation_enforcements_total{verb,outcome}` sample count
/// from the process-global registry. Callers must hold
/// `super::MODERATION_METRICS_LOCK` across the before/after reads: the
/// DB-backed resolver tests in this module and the slash preflight test in
/// `command_runtime_tests` record the same `refused` series, and
/// `cargo test --workspace --lib --bins` runs them as threads of one
/// process. Acquire the metrics lock before `MODERATION_FLAG_LOCK`.
fn moderation_samples(verb: &str, outcome: &str) -> u64 {
    let prefix =
        format!("two_bot_moderation_enforcements_total{{verb=\"{verb}\",outcome=\"{outcome}\"}} ");
    two_bot_core::metrics::global()
        .render(None)
        .lines()
        .filter_map(|line| line.strip_prefix(prefix.as_str()))
        .filter_map(|value| value.trim().parse::<u64>().ok())
        .sum()
}

#[test]
fn resolver_refusal_counts_only_action_not_allowed() {
    // Fresh recorder per case, never the process-global registry: parallel
    // tests must not observe each other's samples.
    for (name, code, expected) in [
        ("not_allowed", ErrorCode::ActionNotAllowed, 1),
        ("unavailable", ErrorCode::DiscordUnavailable, 0),
        ("malformed", ErrorCode::Malformed, 0),
        ("in_progress", ErrorCode::InProgress, 0),
    ] {
        let recorder = metrics::Metrics::default();
        record_resolver_refusal(
            &recorder,
            "kick",
            &ActionError::new(code, "test refusal", "test"),
        );
        let line = format!(
            "two_bot_moderation_enforcements_total{{verb=\"kick\",outcome=\"refused\"}} {expected}\n"
        );
        assert!(
            recorder.render(None).contains(&line),
            "{name} must record {expected} refused sample(s)"
        );
    }
}

/// The blocking review scenario: a website kick for a departed target is
/// refused by `moderation_resolve` before the executor runs, and must still
/// record exactly one `refused` sample. Offline: the resolver refuses before
/// any store use, so the lazy pool never connects and no database is needed.
#[tokio::test]
async fn website_kick_departed_target_records_one_refused_sample() {
    let _metrics = super::MODERATION_METRICS_LOCK.lock().await;
    let _flag = MODERATION_FLAG_LOCK.lock().await;
    let mock = MockMembers::start(200, false).await;
    let discord =
        ActionExecutor::with_proxy("not-a-credential".to_owned(), Some(mock.origin.clone()))
            .unwrap();
    let store = PgMemberModerationStore::new(lazy_pool(), staging_guild().to_owned());
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
    let effect = ModerationExecutor::new(inner, discord);
    let body = serde_json::json!({
        "actor_id": RESOLVE_ACTOR_ID,
        "discord_id": RESOLVE_TARGET_ID,
        "reason": "spam",
    });
    let request =
        InternalMemberRequest::from_body("moderation.kick", body.as_object().unwrap()).unwrap();
    let before = moderation_samples("kick", "refused");
    let err = effect
        .execute_moderation(
            &request,
            "req-refused-kick",
            "website-refused-001",
            1_760_000_000_000,
        )
        .await
        .expect_err("departed kick target must refuse");
    assert_eq!(err.code, ErrorCode::ActionNotAllowed);
    assert_eq!(
        moderation_samples("kick", "refused") - before,
        1,
        "one terminal website refusal records one sample"
    );
    assert!(
        mock.methods().iter().all(|method| method == "GET"),
        "a refusal performs no Discord mutation: {:?}",
        mock.paths()
    );
}

/// Channel mirror: the fixture actor holds Kick + Moderate Members, never
/// Manage Messages, so a website purge is refused by `channel_resolve` before
/// the executor runs — and must still record exactly one `refused` sample.
#[tokio::test]
async fn website_purge_forbidden_actor_records_one_refused_sample() {
    let _metrics = super::MODERATION_METRICS_LOCK.lock().await;
    let _flag = MODERATION_FLAG_LOCK.lock().await;
    let mock = MockMembers::start(200, true).await;
    let discord =
        ActionExecutor::with_proxy("not-a-credential".to_owned(), Some(mock.origin.clone()))
            .unwrap();
    let store = ChannelModerationStore::from_pool(lazy_pool());
    let inner = InternalChannelExecutor::new(
        store,
        discord.clone(),
        InternalChannelConfig {
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
    let effect = ChannelModerationExecutor::new(inner, discord);
    let body = serde_json::json!({
        "actor_id": RESOLVE_ACTOR_ID,
        "channel_id": "222222222222222222",
        "count": 10,
        "reason": "cleanup",
    });
    let request =
        InternalChannelRequest::from_body("moderation.purge", body.as_object().unwrap()).unwrap();
    let before = moderation_samples("purge", "refused");
    let err = effect
        .execute_channel(
            &request,
            "req-refused-purge",
            "website-refused-002",
            "2026-10-17T00:00:00+00:00",
        )
        .await
        .expect_err("actor without Manage Messages must refuse");
    assert_eq!(err.code, ErrorCode::ActionNotAllowed);
    assert_eq!(
        moderation_samples("purge", "refused") - before,
        1,
        "one terminal website refusal records one sample"
    );
    assert!(
        mock.methods().iter().all(|method| method == "GET"),
        "a refusal performs no Discord mutation: {:?}",
        mock.paths()
    );
}

/// The inner member executor records its own disabled refusal exactly once,
/// without touching Discord or the store.
#[tokio::test]
async fn website_member_executor_disabled_records_one_refused_sample() {
    let _metrics = super::MODERATION_METRICS_LOCK.lock().await;
    let _flag = MODERATION_FLAG_LOCK.lock().await;
    let mock = MockMembers::start(200, true).await;
    let discord =
        ActionExecutor::with_proxy("not-a-credential".to_owned(), Some(mock.origin.clone()))
            .unwrap();
    let executor = InternalMemberExecutor::new(
        PgMemberModerationStore::new(lazy_pool(), staging_guild().to_owned()),
        discord,
        InternalMemberConfig {
            guild_id: staging_guild().to_owned(),
            enabled: false,
            policy: ModerationPolicy {
                owen_user_id: RESOLVE_OWNER_ID.to_owned(),
                protected_role_ids: HashSet::new(),
                bot_user_id: None,
            },
            audit_secret: None,
        },
    )
    .unwrap();
    let actor = ModerationActor {
        user_id: RESOLVE_ACTOR_ID.to_owned(),
        highest_role_position: 5,
        role_ids: vec![staging_guild().to_owned()],
        permissions: 0,
    };
    let target = ModerationTarget {
        user_id: RESOLVE_TARGET_ID.to_owned(),
        highest_role_position: 0,
        role_ids: vec![staging_guild().to_owned()],
        is_bot: false,
        is_guild_owner: false,
    };
    let body = serde_json::json!({
        "actor_id": RESOLVE_ACTOR_ID,
        "discord_id": RESOLVE_TARGET_ID,
        "reason": "spam",
    });
    let request =
        InternalMemberRequest::from_body("moderation.kick", body.as_object().unwrap()).unwrap();
    let before = moderation_samples("kick", "refused");
    let err = executor
        .execute(
            &request,
            &actor,
            &target,
            50,
            "req-disabled-member",
            "website-disabled-001",
            1_760_000_000_000,
        )
        .await
        .expect_err("disabled member executor must refuse");
    assert_eq!(err.code, ErrorCode::ActionNotAllowed);
    assert_eq!(
        moderation_samples("kick", "refused") - before,
        1,
        "one terminal executor refusal records one sample"
    );
    assert!(
        mock.paths().is_empty(),
        "a disabled executor touches neither Discord nor the store: {:?}",
        mock.paths()
    );
}

/// The inner channel executor records its own disabled refusal exactly once,
/// without touching Discord or the store.
#[tokio::test]
async fn website_channel_executor_disabled_records_one_refused_sample() {
    let _metrics = super::MODERATION_METRICS_LOCK.lock().await;
    let _flag = MODERATION_FLAG_LOCK.lock().await;
    let mock = MockMembers::start(200, true).await;
    let discord =
        ActionExecutor::with_proxy("not-a-credential".to_owned(), Some(mock.origin.clone()))
            .unwrap();
    let executor = InternalChannelExecutor::new(
        ChannelModerationStore::from_pool(lazy_pool()),
        discord,
        InternalChannelConfig {
            guild_id: staging_guild().to_owned(),
            enabled: false,
            policy: ModerationPolicy {
                owen_user_id: RESOLVE_OWNER_ID.to_owned(),
                protected_role_ids: HashSet::new(),
                bot_user_id: None,
            },
            audit_secret: None,
        },
    )
    .unwrap();
    let actor = ModerationActor {
        user_id: RESOLVE_ACTOR_ID.to_owned(),
        highest_role_position: 5,
        role_ids: vec![staging_guild().to_owned()],
        permissions: 0,
    };
    let body = serde_json::json!({
        "actor_id": RESOLVE_ACTOR_ID,
        "channel_id": "222222222222222222",
        "count": 10,
        "reason": "cleanup",
    });
    let request =
        InternalChannelRequest::from_body("moderation.purge", body.as_object().unwrap()).unwrap();
    let before = moderation_samples("purge", "refused");
    let err = executor
        .execute(
            &request,
            &actor,
            "req-disabled-channel",
            "website-disabled-002",
            "2026-10-17T00:00:00+00:00",
        )
        .await
        .expect_err("disabled channel executor must refuse");
    assert_eq!(err.code, ErrorCode::ActionNotAllowed);
    assert_eq!(
        moderation_samples("purge", "refused") - before,
        1,
        "one terminal executor refusal records one sample"
    );
    assert!(
        mock.paths().is_empty(),
        "a disabled executor touches neither Discord nor the store: {:?}",
        mock.paths()
    );
}

#[tokio::test]
async fn event_read_flag_off_is_refused_before_any_discord_call() {
    let Some(db) = database().await else { return };
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
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
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
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
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
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

/// Authorize refusals recover the verb for both `ActionNotAllowed` and
/// `RateLimited`: a rate-limited `guild.add_member` keeps the `membership`
/// family instead of collapsing to `other`, so the counter series
/// `membership`/`rate_limit` can increment. Earlier refusals stay unknown.
#[test]
fn authorize_refusals_recover_the_verb_for_disabled_and_rate_limited_actions() {
    let member_body = br#"{"action":"guild.add_member"}"#;
    let unknown_body = br#"{"action":"website.nonexistent"}"#;
    let malformed_body = b"not json";
    for (code, raw, expected) in [
        (
            ErrorCode::ActionNotAllowed,
            member_body.as_slice(),
            ActionLabel::Known("guild.add_member"),
        ),
        (
            ErrorCode::RateLimited,
            member_body.as_slice(),
            ActionLabel::Known("guild.add_member"),
        ),
        (
            ErrorCode::RateLimited,
            unknown_body.as_slice(),
            ActionLabel::Unknown,
        ),
        (
            ErrorCode::RateLimited,
            malformed_body.as_slice(),
            ActionLabel::Unknown,
        ),
        (
            ErrorCode::Malformed,
            member_body.as_slice(),
            ActionLabel::Unknown,
        ),
    ] {
        let label = authorize_action_label(code, raw);
        assert_eq!(label, expected, "code {code:?}");
        assert_eq!(
            internal_family(label),
            match expected {
                ActionLabel::Known("guild.add_member") => "membership",
                _ => "other",
            },
            "code {code:?}"
        );
    }
}

#[tokio::test]
async fn event_upsert_create_registers_key_and_replays_created() {
    let Some(db) = database().await else { return };
    // Upsert is a phase-1 default: no event flag is needed for the create.
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
    set_event_cancel_flag(false);
    let api = MockEventApi::start_scripted(vec![ScriptedEvent {
        status: StatusCode::CREATED,
        body: discord_event_for(CREATE_EVENT_ID, 1),
    }])
    .await;
    let app = mutate_app(db.pool().clone(), &api);
    let raw = upsert_payload("fresh");
    let (status, headers, first) = answer(app.clone(), signed(&raw, "old", "intent-create")).await;
    set_event_cancel_flag(false);
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key("idempotent-replay"));
    assert_eq!(
        first["result"],
        json!({"outcome": "created", "event_id": CREATE_EVENT_ID})
    );
    assert_eq!(api.count(), 1, "one Discord POST for one create");
    {
        let seen = api.requests.lock().unwrap();
        assert_eq!(seen[0]["method"], "POST");
        assert_eq!(
            seen[0]["path"],
            format!("/api/v10/guilds/{}/scheduled-events", staging_guild())
        );
        assert_eq!(seen[0]["body"]["name"], "Launch Night");
    }
    let store = InternalActionStore::new(db.pool().clone());
    assert_eq!(
        store
            .event_id_for_key(staging_guild(), "fresh")
            .await
            .unwrap()
            .as_deref(),
        Some(CREATE_EVENT_ID)
    );
    let mirrored: String = sqlx::query_scalar(
        "SELECT name FROM scheduled_events WHERE guild_id = $1 AND event_id = $2",
    )
    .bind(staging_guild())
    .bind(CREATE_EVENT_ID)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(mirrored, "Launch Night");
    // Same key and bytes replay the stored receipt without a second effect —
    // still `created` even though the key is now registered.
    let (status, headers, replay) = answer(app, signed(&raw, "old", "intent-create")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["idempotent-replay"], "true");
    assert_eq!(first["result"], replay["result"]);
    assert_eq!(replay["result"]["outcome"], "created");
    assert_eq!(api.count(), 1, "the replay must not reach Discord again");
    db.close().await.unwrap();
}

#[tokio::test]
async fn event_upsert_update_patches_the_mapped_event_and_replays_updated() {
    let Some(db) = database().await else { return };
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
    set_event_cancel_flag(false);
    InternalActionStore::new(db.pool().clone())
        .put_event_key(staging_guild(), "launch", READ_EVENT_ID)
        .await
        .unwrap();
    let api = MockEventApi::start_scripted(vec![ScriptedEvent {
        status: StatusCode::OK,
        body: discord_event(),
    }])
    .await;
    let app = mutate_app(db.pool().clone(), &api);
    let raw = upsert_payload("launch");
    let (status, _, first) = answer(app.clone(), signed(&raw, "old", "intent-update")).await;
    set_event_cancel_flag(false);
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        first["result"],
        json!({"outcome": "updated", "event_id": READ_EVENT_ID})
    );
    {
        let seen = api.requests.lock().unwrap();
        assert_eq!(seen[0]["method"], "PATCH");
        assert_eq!(
            seen[0]["path"],
            format!(
                "/api/v10/guilds/{}/scheduled-events/{READ_EVENT_ID}",
                staging_guild()
            )
        );
    }
    let (status, headers, replay) = answer(app, signed(&raw, "old", "intent-update")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["idempotent-replay"], "true");
    assert_eq!(first["result"], replay["result"]);
    assert_eq!(api.count(), 1, "the replay must not reach Discord again");
    db.close().await.unwrap();
}

#[tokio::test]
async fn event_cancel_cancels_retains_mapping_and_replays() {
    let Some(db) = database().await else { return };
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
    set_event_cancel_flag(true);
    map_launch(db.pool()).await;
    let api = MockEventApi::start_scripted(vec![ScriptedEvent {
        status: StatusCode::OK,
        body: discord_event_for(READ_EVENT_ID, 4),
    }])
    .await;
    let app = mutate_app(db.pool().clone(), &api);
    let raw = cancel_payload("launch");
    let (status, _, first) = answer(app.clone(), signed(&raw, "old", "intent-cancel")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        first["result"],
        json!({"outcome": "cancelled", "event_id": READ_EVENT_ID})
    );
    {
        let seen = api.requests.lock().unwrap();
        assert_eq!(seen[0]["method"], "PATCH");
        assert_eq!(
            seen[0]["path"],
            format!(
                "/api/v10/guilds/{}/scheduled-events/{READ_EVENT_ID}",
                staging_guild()
            )
        );
        assert_eq!(seen[0]["body"], json!({"status": 4}));
    }
    // Cancel retains the mapping so a later edit cannot silently recreate.
    let store = InternalActionStore::new(db.pool().clone());
    assert_eq!(
        store
            .event_id_for_key(staging_guild(), "launch")
            .await
            .unwrap()
            .as_deref(),
        Some(READ_EVENT_ID)
    );
    let mirrored: String = sqlx::query_scalar(
        "SELECT status FROM scheduled_events WHERE guild_id = $1 AND event_id = $2",
    )
    .bind(staging_guild())
    .bind(READ_EVENT_ID)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(mirrored, "cancelled");
    let (status, headers, replay) = answer(app, signed(&raw, "old", "intent-cancel")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["idempotent-replay"], "true");
    assert_eq!(first["result"], replay["result"]);
    assert_eq!(api.count(), 1, "the replay must not reach Discord again");
    // The flag stays on through the replay: replays pass `authorize` too.
    set_event_cancel_flag(false);
    db.close().await.unwrap();
}

#[tokio::test]
async fn event_cancel_unmapped_key_refused_before_claim_or_discord() {
    let Some(db) = database().await else { return };
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
    set_event_cancel_flag(true);
    let api = MockEventApi::start_scripted(vec![]).await;
    let (status, _, body) = answer(
        mutate_app(db.pool().clone(), &api),
        signed(&cancel_payload("ghost"), "old", "intent-cancel"),
    )
    .await;
    set_event_cancel_flag(false);
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "action_not_allowed");
    assert_eq!(body["error"]["retryable"], false);
    assert_eq!(api.count(), 0, "unmapped cancels never reach Discord");
    let intents: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM internal_idempotency")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(intents, 0, "unmapped cancels take no claim");
    db.close().await.unwrap();
}

#[tokio::test]
async fn event_cancel_flag_off_refused_before_any_discord_call() {
    let Some(db) = database().await else { return };
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
    set_event_cancel_flag(false);
    map_launch(db.pool()).await;
    let api = MockEventApi::start_scripted(vec![]).await;
    let (status, _, body) = answer(
        mutate_app(db.pool().clone(), &api),
        signed(&cancel_payload("launch"), "old", "intent-cancel"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "action_not_allowed");
    assert_eq!(api.count(), 0, "disabled cancels never reach Discord");
    db.close().await.unwrap();
}

#[tokio::test]
async fn moderation_timeout_flag_off_is_refused_before_any_effect() {
    let Some(db) = database().await else { return };
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let moderation = Arc::new(MockModeration::default());
    let app = router(state_full(
        db.pool().clone(),
        effect.clone(),
        Arc::new(MockEventRead::default()),
        Arc::new(MockEventMutate),
        moderation.clone(),
    ));
    let _flag = MODERATION_FLAG_LOCK.lock().await;
    set_moderation_flags(false);
    let (status, _, body) = answer(
        app.clone(),
        signed(
            &moderation_payload("moderation.timeout"),
            "old",
            "intent-timeout-flag-off",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "action_not_allowed");
    assert_eq!(body["error"]["retryable"], false);
    assert_eq!(effect.calls(), 0);
    assert_eq!(moderation.calls(), 0);
    db.close().await.unwrap();
}

#[tokio::test]
async fn event_mutation_malformed_bodies_refused_before_any_discord_call() {
    let Some(db) = database().await else { return };
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
    set_event_cancel_flag(true);
    let api = MockEventApi::start_scripted(vec![]).await;
    let app = mutate_app(db.pool().clone(), &api);
    let base = upsert_payload("launch");
    let mut base_value: Value = serde_json::from_str(&base).unwrap();
    let mut cases = vec![
        // Missing and misshapen event keys.
        serde_json::json!({"action": "event.upsert"}),
        serde_json::json!({"action": "event.cancel", "event_key": "has space"}),
    ];
    for raw in [
        // Ends before it starts.
        upsert_payload("launch").replace("2026-09-01T22:00:00Z", "2026-09-01T18:00:00Z"),
        // Both placements at once.
        upsert_payload("launch").replace("\"location\"", "\"channel_key\":\"ann\",\"location\""),
        // Unknown channel key.
        upsert_payload("launch").replace("\"location\":\"The Hall\"", "\"channel_key\":\"nope\""),
    ] {
        cases.push(serde_json::from_str(&raw).unwrap());
    }
    // Location is 1-100 characters on Discord; one more is refused locally.
    cases.push(
        serde_json::from_str(
            &upsert_payload("launch").replace("\"The Hall\"", &format!("\"{}\"", "x".repeat(101))),
        )
        .unwrap(),
    );
    // Empty description is a missing field, not an omitted one.
    base_value["description"] = json!("");
    cases.push(base_value);
    for (index, case) in cases.into_iter().enumerate() {
        let raw = case.to_string();
        let expected = if index == 4 {
            // Unknown channel keys are a policy refusal, not a shape refusal.
            StatusCode::FORBIDDEN
        } else {
            StatusCode::BAD_REQUEST
        };
        let (status, _, body) = answer(
            app.clone(),
            signed(&raw, "old", &format!("intent-malformed-{index}")),
        )
        .await;
        assert_eq!(status, expected, "{raw}");
        let code = if index == 4 {
            "action_not_allowed"
        } else {
            "malformed"
        };
        assert_eq!(body["error"]["code"], code, "{raw}");
    }
    // Mutations require an Idempotency-Key header; a keyless signed request is malformed.
    let (status, _, body) =
        answer(app.clone(), signed_read(&upsert_payload("launch"), "old")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "malformed");
    let mut bad_key = signed(&upsert_payload("launch"), "old", "intent-ok");
    bad_key
        .headers_mut()
        .insert("idempotency-key", HeaderValue::from_static("has space!"));
    let (status, _, body) = answer(app, bad_key).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "malformed");
    set_event_cancel_flag(false);
    assert_eq!(api.count(), 0, "malformed mutations never reach Discord");
    db.close().await.unwrap();
}

#[tokio::test]
async fn unwired_membership_verb_stays_refused_with_moderation_flags_on() {
    let Some(db) = database().await else { return };
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let moderation = Arc::new(MockModeration::default());
    let app = router(state_full(
        db.pool().clone(),
        effect.clone(),
        Arc::new(MockEventRead::default()),
        Arc::new(MockEventMutate),
        moderation.clone(),
    ));
    let _flag = MODERATION_FLAG_LOCK.lock().await;
    set_moderation_flags(true);
    // The moderation union wired ban, tempban, kick, warn and timeout: only a
    // verb from another family (membership) still refuses here.
    for raw in [
        r#"{"action":"role.assign","discord_id":"111111111111111111","role_key":"fixture"}"#
            .to_owned(),
    ] {
        let (status, _, body) = answer(app.clone(), signed(&raw, "old", "intent-no-widen")).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{raw}");
        assert_eq!(body["error"]["code"], "action_not_allowed");
        assert!(!body.to_string().contains("111111111111111111"));
    }
    set_moderation_flags(false);
    assert_eq!(effect.calls(), 0);
    assert_eq!(moderation.calls(), 0);
    db.close().await.unwrap();
}

#[tokio::test]
async fn event_cancel_already_cancelled_is_discord_rejected() {
    let Some(db) = database().await else { return };
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
    set_event_cancel_flag(true);
    map_launch(db.pool()).await;
    // Legacy does not swallow an already-cancelled 400: idempotency is durable
    // replay of the first result, not a new PATCH.
    let api = MockEventApi::start_scripted(vec![ScriptedEvent {
        status: StatusCode::BAD_REQUEST,
        body: discord_event_for(READ_EVENT_ID, 4),
    }])
    .await;
    let app = mutate_app(db.pool().clone(), &api);
    let raw = cancel_payload("launch");
    let (status, _, first) = answer(app.clone(), signed(&raw, "old", "intent-cancel")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(first["error"]["code"], "discord_rejected");
    assert_eq!(first["error"]["retryable"], false);
    let (_, headers, replay) = answer(app, signed(&raw, "old", "intent-cancel")).await;
    assert_eq!(replay["error"]["code"], "discord_rejected");
    assert_eq!(headers["idempotent-replay"], "true");
    assert_eq!(api.count(), 1, "the replay must not reach Discord again");
    // The flag stays on through the replay: replays pass `authorize` too.
    set_event_cancel_flag(false);
    db.close().await.unwrap();
}

#[tokio::test]
async fn event_upsert_discord_500_needs_reconciliation_without_receipt() {
    let Some(db) = database().await else { return };
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
    set_event_cancel_flag(true);
    let api = MockEventApi::start_scripted(vec![ScriptedEvent {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        body: Value::Null,
    }])
    .await;
    let app = mutate_app(db.pool().clone(), &api);
    let raw = upsert_payload("fresh");
    let (status, _, first) = answer(app.clone(), signed(&raw, "old", "intent-upsert")).await;
    set_event_cancel_flag(false);
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(first["error"]["code"], "needs_reconciliation");
    // No mapping and no receipt: the fence is retained, nothing is replayable.
    let store = InternalActionStore::new(db.pool().clone());
    assert_eq!(
        store
            .event_id_for_key(staging_guild(), "fresh")
            .await
            .unwrap(),
        None
    );
    let (_, _, second) = answer(app, signed(&raw, "old", "intent-upsert")).await;
    assert_eq!(second["error"]["code"], "needs_reconciliation");
    assert_eq!(api.count(), 1, "uncertainty never resends the intent");
    db.close().await.unwrap();
}

#[tokio::test]
async fn concurrent_creates_for_one_key_serialize_into_a_create_then_an_update() {
    let Some(db) = database().await else { return };
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
    set_event_cancel_flag(false);
    let api = MockEventApi::start_scripted(vec![
        ScriptedEvent {
            status: StatusCode::CREATED,
            body: discord_event_for(CREATE_EVENT_ID, 1),
        },
        ScriptedEvent {
            status: StatusCode::OK,
            body: discord_event_for(CREATE_EVENT_ID, 1),
        },
    ])
    .await;
    let app = mutate_app(db.pool().clone(), &api);
    let raw = upsert_payload("race");
    let (first, second) = tokio::join!(
        answer(app.clone(), signed(&raw, "old", "intent-race-a")),
        answer(app, signed(&raw, "old", "intent-race-b")),
    );
    let (first_status, _, first) = first;
    let (second_status, _, second) = second;
    assert_eq!(first_status, StatusCode::OK);
    assert_eq!(second_status, StatusCode::OK);
    let mut outcomes = [
        first["result"]["outcome"].as_str().unwrap().to_owned(),
        second["result"]["outcome"].as_str().unwrap().to_owned(),
    ];
    outcomes.sort();
    assert_eq!(outcomes, ["created", "updated"]);
    assert_eq!(
        api.count(),
        2,
        "one create and one update, never two creates"
    );
    {
        let seen = api.requests.lock().unwrap();
        assert_eq!(seen[0]["method"], "POST");
        assert_eq!(seen[1]["method"], "PATCH");
    }
    let store = InternalActionStore::new(db.pool().clone());
    assert_eq!(
        store
            .event_id_for_key(staging_guild(), "race")
            .await
            .unwrap()
            .as_deref(),
        Some(CREATE_EVENT_ID)
    );
    db.close().await.unwrap();
}

struct ScriptedEventMutate {
    replies: Mutex<Vec<Result<Value, EventActionError>>>,
}

impl EventMutateEffect for ScriptedEventMutate {
    fn execute_mutation<'a>(
        &'a self,
        _: &'a str,
        _: &'a EventCall,
    ) -> BoxFuture<'a, Result<Value, EventActionError>> {
        Box::pin(async move { self.replies.lock().unwrap().remove(0) })
    }
}

#[tokio::test]
async fn event_create_admission_refusal_releases_claim_for_same_key_retry() {
    let Some(db) = database().await else { return };
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
    set_event_cancel_flag(false);
    let blocked = two_bot_core::send_admission::AdmissionError::Blocked.to_string();
    let mutates: Arc<dyn EventMutateEffect> = Arc::new(ScriptedEventMutate {
        replies: Mutex::new(vec![
            Err(EventActionError::Discord(DiscordError::Unavailable(
                blocked,
            ))),
            Ok(json!({"event_id": CREATE_EVENT_ID})),
        ]),
    });
    let reads: Arc<dyn EventReadEffect> = Arc::new(MockEventRead::default());
    let effect: Arc<dyn ActionEffect> = Arc::new(MockEffect::new(MockOutcome::Success));
    let app = router(Arc::new(ReceiverState::new(
        config(),
        db.pool().clone(),
        effect,
        Arc::new(MockEffect::new(MockOutcome::Success)),
        reads,
        mutates,
        Arc::new(MockModeration::default()),
        Arc::new(MockChannel::default()),
    )));
    let raw = upsert_payload("lane");
    let (status, _, refused) = answer(app.clone(), signed(&raw, "old", "intent-lane")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(refused["error"]["code"], "discord_unavailable");
    // The lane refusal proved nothing was sent, so the same key may retry.
    let (status, headers, created) = answer(app, signed(&raw, "old", "intent-lane")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key("idempotent-replay"));
    assert_eq!(
        created["result"],
        json!({"outcome": "created", "event_id": CREATE_EVENT_ID})
    );
    let store = InternalActionStore::new(db.pool().clone());
    assert_eq!(
        store
            .event_id_for_key(staging_guild(), "lane")
            .await
            .unwrap()
            .as_deref(),
        Some(CREATE_EVENT_ID)
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn event_cancel_admission_refusal_releases_claim_for_same_key_retry() {
    let Some(db) = database().await else { return };
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
    set_event_cancel_flag(true);
    map_launch(db.pool()).await;
    let blocked = two_bot_core::send_admission::AdmissionError::Blocked.to_string();
    let mutates: Arc<dyn EventMutateEffect> = Arc::new(ScriptedEventMutate {
        replies: Mutex::new(vec![
            Err(EventActionError::Discord(DiscordError::Unavailable(
                blocked,
            ))),
            Ok(json!({"event_id": READ_EVENT_ID})),
        ]),
    });
    let reads: Arc<dyn EventReadEffect> = Arc::new(MockEventRead::default());
    let effect: Arc<dyn ActionEffect> = Arc::new(MockEffect::new(MockOutcome::Success));
    let app = router(Arc::new(ReceiverState::new(
        config(),
        db.pool().clone(),
        effect,
        Arc::new(MockEffect::new(MockOutcome::Success)),
        reads,
        mutates,
        Arc::new(MockModeration::default()),
        Arc::new(MockChannel::default()),
    )));
    let raw = cancel_payload("launch");
    let (status, _, refused) = answer(app.clone(), signed(&raw, "old", "intent-cancel")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(refused["error"]["code"], "discord_unavailable");
    let (status, headers, cancelled) = answer(app, signed(&raw, "old", "intent-cancel")).await;
    set_event_cancel_flag(false);
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key("idempotent-replay"));
    assert_eq!(
        cancelled["result"],
        json!({"outcome": "cancelled", "event_id": READ_EVENT_ID})
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn event_create_admission_storage_is_typed_internal_and_releases_claim() {
    let Some(db) = database().await else { return };
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
    set_event_cancel_flag(false);
    let storage = two_bot_core::send_admission::AdmissionError::Storage.to_string();
    let error = EventActionError::Discord(DiscordError::Unavailable(storage));
    assert!(
        error.is_admission_storage(),
        "admission storage must be typed, not generic unavailable"
    );
    assert!(!error.is_admission_blocked());
    let wire = error.action_error();
    assert_eq!(wire.code, ErrorCode::Internal);
    assert_eq!(wire.log_reason, "admission_storage_unavailable");
    let mutates: Arc<dyn EventMutateEffect> = Arc::new(ScriptedEventMutate {
        replies: Mutex::new(vec![
            Err(EventActionError::Discord(DiscordError::Unavailable(
                two_bot_core::send_admission::AdmissionError::Storage.to_string(),
            ))),
            Ok(json!({"event_id": CREATE_EVENT_ID})),
        ]),
    });
    let reads: Arc<dyn EventReadEffect> = Arc::new(MockEventRead::default());
    let effect: Arc<dyn ActionEffect> = Arc::new(MockEffect::new(MockOutcome::Success));
    let app = router(Arc::new(ReceiverState::new(
        config(),
        db.pool().clone(),
        effect,
        Arc::new(MockEffect::new(MockOutcome::Success)),
        reads,
        mutates,
        Arc::new(MockModeration::default()),
        Arc::new(MockChannel::default()),
    )));
    let raw = upsert_payload("lane-storage");
    let (status, _, refused) =
        answer(app.clone(), signed(&raw, "old", "intent-lane-storage")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(refused["error"]["code"], "internal");
    assert_eq!(refused["error"]["retryable"], true);
    // The storage refusal proved nothing was sent, so the same key may retry.
    let (status, headers, created) = answer(app, signed(&raw, "old", "intent-lane-storage")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key("idempotent-replay"));
    assert_eq!(
        created["result"],
        json!({"outcome": "created", "event_id": CREATE_EVENT_ID})
    );
    let store = InternalActionStore::new(db.pool().clone());
    assert_eq!(
        store
            .event_id_for_key(staging_guild(), "lane-storage")
            .await
            .unwrap()
            .as_deref(),
        Some(CREATE_EVENT_ID)
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn event_mutation_queued_behind_the_gate_is_refused_before_any_claim() {
    let Some(db) = database().await else { return };
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
    set_event_cancel_flag(false);
    let api = MockEventApi::start_scripted(vec![]).await;
    let executor =
        ActionExecutor::with_proxy("not-a-credential".to_owned(), Some(api.origin.clone()))
            .unwrap();
    let reads: Arc<dyn EventReadEffect> = Arc::new(MockEventRead::default());
    let effect: Arc<dyn ActionEffect> = Arc::new(MockEffect::new(MockOutcome::Success));
    let mutates: Arc<dyn EventMutateEffect> =
        Arc::new(EventMutationExecutor::new(executor, db.pool().clone()));
    let mut receiver = ReceiverState::new(
        config(),
        db.pool().clone(),
        effect,
        Arc::new(MockEffect::new(MockOutcome::Success)),
        reads,
        mutates,
        Arc::new(MockModeration::default()),
        Arc::new(MockChannel::default()),
    );
    receiver.event_gate_wait = Duration::from_millis(50);
    let state = Arc::new(receiver);
    let held = state.event_gate.lock().await;
    let app = router(state.clone());
    let (status, _, body) = answer(
        app,
        signed(&upsert_payload("queued"), "old", "intent-queued"),
    )
    .await;
    drop(held);
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "in_progress");
    assert_eq!(api.count(), 0, "a queued mutation never reaches Discord");
    let claims: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM internal_idempotency")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(claims, 0, "a queued mutation takes no claim");
    let store = InternalActionStore::new(db.pool().clone());
    assert_eq!(
        store
            .event_id_for_key(staging_guild(), "queued")
            .await
            .unwrap(),
        None
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn event_create_mirror_failure_leaves_the_key_unmapped_and_unknown() {
    let Some(db) = database().await else { return };
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
    set_event_cancel_flag(false);
    let mutates: Arc<dyn EventMutateEffect> = Arc::new(ScriptedEventMutate {
        replies: Mutex::new(vec![Err(EventActionError::Mirror)]),
    });
    let reads: Arc<dyn EventReadEffect> = Arc::new(MockEventRead::default());
    let effect: Arc<dyn ActionEffect> = Arc::new(MockEffect::new(MockOutcome::Success));
    let app = router(Arc::new(ReceiverState::new(
        config(),
        db.pool().clone(),
        effect,
        Arc::new(MockEffect::new(MockOutcome::Success)),
        reads,
        mutates,
        Arc::new(MockModeration::default()),
        Arc::new(MockChannel::default()),
    )));
    let raw = upsert_payload("mirror");
    let (status, _, first) = answer(app.clone(), signed(&raw, "old", "intent-mirror")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(first["error"]["code"], "needs_reconciliation");
    // The retry finds the unknown intent, so the one-reply mutator is never called again.
    let (status, _, second) = answer(app, signed(&raw, "old", "intent-mirror")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(second["error"]["code"], "needs_reconciliation");
    let store = InternalActionStore::new(db.pool().clone());
    assert_eq!(
        store
            .event_id_for_key(staging_guild(), "mirror")
            .await
            .unwrap(),
        None
    );
    db.close().await.unwrap();
}

struct DelayedEventMutate {
    delay: Duration,
    event_id: &'static str,
    entered: Arc<tokio::sync::Notify>,
}

impl EventMutateEffect for DelayedEventMutate {
    fn execute_mutation<'a>(
        &'a self,
        _: &'a str,
        _: &'a EventCall,
    ) -> BoxFuture<'a, Result<Value, EventActionError>> {
        Box::pin(async move {
            self.entered.notify_one();
            tokio::time::sleep(self.delay).await;
            Ok(json!({"event_id": self.event_id}))
        })
    }
}

#[tokio::test]
async fn event_mutation_completes_its_receipt_after_the_client_is_dropped() {
    let Some(db) = database().await else { return };
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
    set_event_cancel_flag(false);
    let entered = Arc::new(tokio::sync::Notify::new());
    let mutates: Arc<dyn EventMutateEffect> = Arc::new(DelayedEventMutate {
        delay: Duration::from_millis(300),
        event_id: CREATE_EVENT_ID,
        entered: Arc::clone(&entered),
    });
    let reads: Arc<dyn EventReadEffect> = Arc::new(MockEventRead::default());
    let effect: Arc<dyn ActionEffect> = Arc::new(MockEffect::new(MockOutcome::Success));
    let app = router(Arc::new(ReceiverState::new(
        config(),
        db.pool().clone(),
        effect,
        Arc::new(MockEffect::new(MockOutcome::Success)),
        reads,
        mutates,
        Arc::new(MockModeration::default()),
        Arc::new(MockChannel::default()),
    )));
    let raw = upsert_payload("dropped");
    let client = tokio::spawn(answer(app.clone(), signed(&raw, "old", "intent-dropped")));
    entered.notified().await;
    client.abort();
    let mut completed: i64 = 0;
    for _ in 0..200 {
        completed = sqlx::query_scalar(
            "SELECT COUNT(*) FROM internal_idempotency WHERE state = 'completed'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        if completed == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        completed, 1,
        "the claimed create finishes without its client"
    );
    let store = InternalActionStore::new(db.pool().clone());
    assert_eq!(
        store
            .event_id_for_key(staging_guild(), "dropped")
            .await
            .unwrap()
            .as_deref(),
        Some(CREATE_EVENT_ID)
    );
    let (status, headers, replay) = answer(app, signed(&raw, "old", "intent-dropped")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["idempotent-replay"], "true");
    assert_eq!(
        replay["result"],
        json!({"outcome": "created", "event_id": CREATE_EVENT_ID})
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn event_read_queued_behind_the_gate_is_refused_before_any_discord_call() {
    let Some(db) = database().await else { return };
    let _flag = INTERNAL_FLAG_LOCK.lock().await;
    set_event_read_flag(true);
    map_launch(db.pool()).await;
    let api = MockEventApi::start_scripted(vec![]).await;
    let executor =
        ActionExecutor::with_proxy("not-a-credential".to_owned(), Some(api.origin.clone()))
            .unwrap();
    let reads: Arc<dyn EventReadEffect> =
        Arc::new(EventReadExecutor::new(executor, db.pool().clone()));
    let effect: Arc<dyn ActionEffect> = Arc::new(MockEffect::new(MockOutcome::Success));
    let mutates: Arc<dyn EventMutateEffect> = Arc::new(MockEventMutate);
    let mut receiver = ReceiverState::new(
        config(),
        db.pool().clone(),
        effect,
        Arc::new(MockEffect::new(MockOutcome::Success)),
        reads,
        mutates,
        Arc::new(MockModeration::default()),
        Arc::new(MockChannel::default()),
    );
    receiver.event_gate_wait = Duration::from_millis(50);
    let state = Arc::new(receiver);
    let held = state.event_gate.lock().await;
    let app = router(state.clone());
    let (status, _, body) = answer(app, signed_read(&read_payload("launch"), "old")).await;
    drop(held);
    set_event_read_flag(false);
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "in_progress");
    assert_eq!(api.count(), 0, "a queued read never reaches Discord");
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

#[test]
fn only_registry_bearer_refusals_receive_a_bearer_challenge() {
    let make_error =
        || ActionError::new(ErrorCode::Unauthorized, "refused", "admin_bearer_refused");
    let registry = Failure::from_admin(make_error()).response("fixture");
    assert_eq!(registry.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        registry.headers().get(header::WWW_AUTHENTICATE).unwrap(),
        "Bearer"
    );
    let transport = Failure::from_action(make_error()).response("fixture");
    assert_eq!(transport.status(), StatusCode::UNAUTHORIZED);
    assert!(!transport.headers().contains_key(header::WWW_AUTHENTICATE));
    for code in [ErrorCode::ActionNotAllowed, ErrorCode::Internal] {
        let response =
            Failure::from_admin(ActionError::new(code, "refused", "fixture")).response("fixture");
        assert!(!response.headers().contains_key(header::WWW_AUTHENTICATE));
    }
}

#[derive(Default)]
struct CountingScopedEventMutate {
    calls: AtomicUsize,
}

impl EventMutateEffect for CountingScopedEventMutate {
    fn execute_mutation<'a>(
        &'a self,
        _: &'a str,
        _: &'a EventCall,
    ) -> BoxFuture<'a, Result<Value, EventActionError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"event_id": CREATE_EVENT_ID}))
        })
    }
}

#[tokio::test]
async fn scoped_receiver_challenges_registry_refusals_but_not_service_faults() {
    let Some(db) = database().await else { return };
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let mutates = Arc::new(CountingScopedEventMutate::default());
    let app = router(Arc::new(ReceiverState::new(
        config(),
        db.pool().clone(),
        effect.clone(),
        effect.clone(),
        Arc::new(MockEventRead::default()),
        mutates.clone(),
        Arc::new(MockModeration::default()),
        Arc::new(MockChannel::default()),
    )));
    // Use the same valid event input for scoped refusals and the legacy
    // positive control: a malformed event must not hide a misplaced fence.
    let bearer_secret = hex::encode(rand::random::<[u8; 32]>());
    let mut payload: Value = serde_json::from_str(&upsert_payload("scoped-refusal")).unwrap();
    payload["guild_id"] = json!(staging_guild());
    payload["admin_bearer"] = json!(format!("adm_0123456789abcdef.{bearer_secret}"));
    let raw = payload.to_string();
    let make = |key_id: &str, signing_secret: Vec<u8>, seconds_ago: u64| {
        let timestamp = (now_ms() / 1000 - seconds_ago).to_string();
        let nonce = nonce();
        let key = two_bot_core::internal_actions::SigningKey {
            id: key_id.to_owned(),
            secret: two_bot_core::Secret::new(signing_secret),
        };
        let signature = key.sign_scoped(
            &timestamp,
            &nonce,
            raw.as_bytes(),
            "moderate",
            staging_guild(),
        );
        Request::builder()
            .method(Method::POST)
            .uri(ACTIONS_PATH)
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-two-key-id", key_id)
            .header("x-two-timestamp", timestamp)
            .header("x-two-nonce", nonce)
            .header("x-two-signature", signature)
            .header("x-two-scope", "moderate")
            .header("x-two-guild", staging_guild())
            .header("idempotency-key", "intent-scoped-refusal")
            .body(Body::from(raw.clone()))
            .unwrap()
    };
    let (status, headers, body) = answer(app.clone(), make("old", secret(0).into_bytes(), 0)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(headers.get(header::WWW_AUTHENTICATE).unwrap(), "Bearer");
    assert_eq!(body["error"]["code"], "unauthorized");
    // Fresh incorrect key material exercises the bad-MAC path without
    // introducing a hard-coded cryptographic key into the test source.
    let wrong_secret = rand::random::<[u8; 32]>().to_vec();
    assert_ne!(wrong_secret, secret(0).as_bytes());
    for (key_id, signing_secret, seconds_ago, code) in [
        ("unknown", secret(0).into_bytes(), 0, "unauthorized"),
        ("old", wrong_secret, 0, "unauthorized"),
        ("old", secret(0).into_bytes(), 10000, "stale_request"),
    ] {
        let (status, headers, body) =
            answer(app.clone(), make(key_id, signing_secret, seconds_ago)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], code);
        assert!(!headers.contains_key(header::WWW_AUTHENTICATE));
    }
    // The same bearer becomes live authority, but the scoped surface must
    // still refuse before an execution intent or event effect can exist.
    sqlx::query(
        "INSERT INTO admin_api_tokens
         (token_id, secret_hash, scopes, guild_ids, holder_id, holder_kind, issued_at, expires_at)
         VALUES ($1, $2, ARRAY['moderate'], $3, 'scope-test-holder', 'human',
                 now(), now() + interval '1 hour')",
    )
    .bind("adm_0123456789abcdef")
    .bind(InternalFlags::hash_admin_token_secret(bearer_secret.as_bytes()).unwrap())
    .bind(vec![staging_guild().parse::<i64>().unwrap()])
    .execute(db.pool())
    .await
    .unwrap();
    let (status, headers, body) = answer(app.clone(), make("old", secret(0).into_bytes(), 0)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "action_not_allowed");
    assert!(!headers.contains_key(header::WWW_AUTHENTICATE));
    assert_eq!(effect.calls(), 0);
    assert_eq!(mutates.calls.load(Ordering::SeqCst), 0);
    let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM internal_idempotency")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(
        receipts, 0,
        "scoped refusals must precede execution intents"
    );

    // Bearer-bearing input cannot fall back to legacy authority either.
    let (status, _, body) = answer(app.clone(), signed(&raw, "old", "intent-scoped-refusal")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "unauthorized");
    assert_eq!(mutates.calls.load(Ordering::SeqCst), 0);

    // Prove the event fields and counting seam reach the legacy executor,
    // rather than failing validation before it. Remove only the bearer:
    // legacy deliberately refuses bearer-bearing input before dispatch.
    // This is a disposable database and offline effect.
    payload.as_object_mut().unwrap().remove("admin_bearer");
    let legacy_raw = payload.to_string();
    let (status, _, body) = answer(app, signed(&legacy_raw, "old", "intent-scoped-refusal")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["result"]["event_id"], CREATE_EVENT_ID);
    assert_eq!(mutates.calls.load(Ordering::SeqCst), 1);
    assert_eq!(effect.calls(), 0);
    db.close().await.unwrap();
}
