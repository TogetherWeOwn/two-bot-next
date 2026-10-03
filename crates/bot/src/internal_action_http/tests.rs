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
    Arc::new(ReceiverState::new(config(), pool, effect))
}

fn payload() -> &'static str {
    r#"{"action":"announcement.post","channel_key":"ann","body":"fixture announcement"}"#
}

fn signed(raw: &str, key: &str, nonce: u32, intent: &str) -> Request {
    let timestamp = (now_ms() / 1000).to_string();
    let nonce = format!("{nonce:032x}");
    let signature = sign(
        secret(usize::from(key == "new")).as_bytes(),
        &timestamp,
        &nonce,
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
        let mut request = signed(payload(), "old", 1, "intent-fixture");
        let value = request.headers()[name].clone();
        request.headers_mut().append(name, value);
        let (status, _, _) = answer(router(state.clone()), request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name}");
    }
    for (media, encoding) in [("text/plain", None), ("application/json", Some("gzip"))] {
        let mut request = signed(payload(), "old", 2, "intent-fixture");
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
    let mut request = signed(payload(), "old", 3, "intent-fixture");
    request.headers_mut().insert(
        "x-padding",
        HeaderValue::from_str(&"p".repeat(MAX_HEADER_BYTES)).unwrap(),
    );
    assert_eq!(
        answer(router(state.clone()), request).await.0,
        StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
    );
    let mut request = signed(payload(), "old", 4, "intent-fixture");
    request.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&(MAX_BODY_BYTES + 1).to_string()).unwrap(),
    );
    assert_eq!(
        answer(router(state.clone()), request).await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let mut request = signed(payload(), "old", 5, "intent-fixture");
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
        signed(payload(), "old", 7, "intent-fixture"),
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
    let mut request = signed(payload(), "old", 8, "intent-fixture");
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
        let mut request = signed(payload(), "old", 6, "intent-fixture");
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
    for (nonce, raw) in [
        (10, "{"),
        (
            11,
            r#"{"action":"announcement.post","action":"announcement.post"}"#,
        ),
    ] {
        assert_eq!(
            answer(app.clone(), signed(raw, "old", nonce, "intent-fixture"))
                .await
                .2["error"]["code"],
            "malformed"
        );
        assert_eq!(
            answer(app.clone(), signed(raw, "old", nonce, "intent-fixture"))
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
        answer(app.clone(), signed(payload(), "old", 20, "intent-fixture")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key("idempotent-replay"));
    assert_eq!(first["result"], json!({"message_id":"444444444444444444"}));
    assert_eq!(first["request_id"].as_str().unwrap().len(), 26);
    let restarted_pool = db.independent_pool().await.unwrap();
    let restarted = router(state(restarted_pool.clone(), effect.clone()));
    let (status, headers, replay) = answer(
        restarted.clone(),
        signed(payload(), "new", 21, "intent-fixture"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["idempotent-replay"], "true");
    assert_eq!(first["result"], replay["result"]);
    // Same JSON intent with changed exact signed representation is a mismatch.
    let changed = format!("{} ", payload());
    let (status, _, refusal) =
        answer(restarted, signed(&changed, "new", 22, "intent-fixture")).await;
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
        signed(payload(), "old", 30, "intent-fixture"),
    ));
    tokio::time::timeout(Duration::from_secs(5), effect.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let (status, _, duplicate) =
        answer(app.clone(), signed(payload(), "old", 31, "intent-fixture")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        duplicate["error"],
        json!({"code":"in_progress", "message":"Internal action refused", "retryable":true})
    );
    assert_eq!(effect.calls(), 1);
    effect.release.add_permits(1);
    assert_eq!(first.await.unwrap().0, StatusCode::OK);
    assert_eq!(
        answer(app, signed(payload(), "old", 32, "intent-fixture"))
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
    for (nonce, intent, outcome, code, replayed) in [
        (
            40,
            "intent-no-effect",
            MockOutcome::NoEffect,
            "no_effect",
            true,
        ),
        (
            50,
            "intent-unknown",
            MockOutcome::Unknown,
            "needs_reconciliation",
            false,
        ),
    ] {
        let effect = Arc::new(MockEffect::new(outcome));
        let app = router(state(db.pool().clone(), effect.clone()));
        let (_, _, first) = answer(app.clone(), signed(payload(), "old", nonce, intent)).await;
        assert_eq!(first["error"]["code"], code);
        assert_eq!(first["error"]["retryable"], false);
        let (_, headers, second) = answer(app, signed(payload(), "old", nonce + 1, intent)).await;
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
        signed(payload(), "old", 60, "intent-fixture"),
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
    let (_, _, next) = answer(app, signed(payload(), "old", 61, "intent-fixture")).await;
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
        answer(app.clone(), signed(payload(), "old", 70, "intent-fixture"))
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
        answer(app, signed(payload(), "old", 71, "intent-fixture"))
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
    let (_, _, first) = answer(app.clone(), signed(payload(), "old", 80, "intent-fixture")).await;
    assert_eq!(first["error"]["code"], "needs_reconciliation");
    sqlx::query("ALTER TABLE receiver_hidden_audit RENAME TO internal_action_log")
        .execute(db.pool())
        .await
        .unwrap();
    let (_, _, second) = answer(app, signed(payload(), "old", 81, "intent-fixture")).await;
    assert_eq!(second["error"]["code"], "in_progress");
    assert_eq!(effect.calls(), 1);
    db.close().await.unwrap();
}

#[tokio::test]
async fn authenticated_unsupported_actions_and_bad_channel_keys_stay_redacted() {
    let Some(db) = database().await else { return };
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let app = router(state(db.pool().clone(), effect.clone()));
    for (nonce, raw) in [
        (
            90,
            r#"{"action":"role.assign","discord_id":"111111111111111111","role_key":"fixture"}"#,
        ),
        (
            91,
            r#"{"action":"announcement.post","channel_key":"secret-sentinel","body":"secret-sentinel"}"#,
        ),
    ] {
        let (_, _, body) = answer(app.clone(), signed(raw, "old", nonce, "intent-fixture")).await;
        assert_eq!(body["error"]["code"], "action_not_allowed");
        assert!(!body.to_string().contains("secret-sentinel"));
    }
    assert_eq!(effect.calls(), 0);
    db.close().await.unwrap();
}

#[tokio::test]
async fn health_router_has_no_action_route() {
    let state = crate::server::SharedState {
        gateway: Arc::new(tokio::sync::RwLock::new(
            crate::gateway::GatewayState::Unconfigured,
        )),
        database: None,
    };
    let response = crate::server::router(state)
        .oneshot(signed(payload(), "old", 100, "intent-fixture"))
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
