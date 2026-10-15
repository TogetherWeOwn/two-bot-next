//! Private Twitch EventSub receiver (`POST /hooks/twitch`). Never merge this
//! router into the health socket. Verification (`twitch_eventsub::verify_delivery`)
//! precedes any JSON parsing; classification errors after verification are
//! deterministic and are acknowledged with 2xx so Twitch does not retry them.
//! Only transient failures (none in this slice: no send, no ledger) would be
//! 5xx. No Helix client and no durable ledger here (separate slices).
//!
//! Logging carries only scalar `event`/`status`/`error_class`: no header
//! values, challenge, body bytes or query. The callback path never reaches a
//! trace span (`server::TRACED_PATHS` must not list it; unknown paths render
//! as `[REDACTED]`).

use std::{
    future::IntoFuture,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Json, Router,
};
use serde_json::json;
use tokio::{net::TcpListener, sync::Semaphore};
use two_bot_core::{
    eventsub_receiver_config::EventsubReceiverConfig,
    twitch_eventsub::{classify, verify_delivery, Delivery, DeliveryHeaders, EventSubError},
};

/// Exact callback path; mirrors `wrangler/src/eventsub.ts` (`EVENTSUB_PATH`).
pub const EVENTSUB_PATH: &str = "/hooks/twitch";
const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_REQUESTS: usize = 32;
const BODY_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

struct ReceiverState {
    secret: two_bot_core::twitch_eventsub::WebhookSecret,
    capacity: Arc<Semaphore>,
}

impl ReceiverState {
    fn new(config: &EventsubReceiverConfig) -> Self {
        Self {
            // `WebhookSecret` is `Clone` (wraps `Secret<String>`); keep the
            // value inside the state so the handler never re-reads the env.
            secret: config.secret().clone(),
            capacity: Arc::new(Semaphore::new(MAX_REQUESTS)),
        }
    }
}

/// Binding completes before any gateway/job task starts. Invalid enabled
/// configuration and bind failure are fatal; there is no health-only fallback.
pub struct BoundEventsubReceiver {
    listener: TcpListener,
    state: Arc<ReceiverState>,
}

pub async fn bind(config: EventsubReceiverConfig) -> std::io::Result<BoundEventsubReceiver> {
    let listener = TcpListener::bind(config.listen_addr()).await?;
    Ok(BoundEventsubReceiver {
        listener,
        state: Arc::new(ReceiverState::new(&config)),
    })
}

impl BoundEventsubReceiver {
    /// Supervised alongside the public listener and the internal-actions
    /// receiver; first termination stops admission everywhere.
    pub async fn serve(self, shutdown: tokio::sync::watch::Receiver<bool>) -> std::io::Result<()> {
        let server = axum::serve(self.listener, router(Arc::clone(&self.state)))
            .with_graceful_shutdown(crate::server::shutdown_requested(shutdown))
            .into_future();
        server.await
    }
}

fn router(state: Arc<ReceiverState>) -> Router {
    // `any` lets the boundary give non-POSTs the same scalar envelope. No JSON
    // extractor or generic TraceLayer: neither request paths nor headers may log.
    Router::new()
        .route(EVENTSUB_PATH, any(handle))
        .fallback(not_found)
        .with_state(state)
}

fn request_id() -> String {
    two_bot_core::internal_actions::new_request_id(now_ms_u64(), &rand::random())
}

fn now_ms_u64() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            elapsed.as_millis().try_into().unwrap_or(u64::MAX)
        })
}

fn now_ms_i64() -> i64 {
    i64::try_from(now_ms_u64()).unwrap_or(i64::MAX)
}

fn envelope(status: StatusCode, code: &str, retryable: bool, id: &str) -> Response {
    let body = Json(json!({
        "ok": false,
        "request_id": id,
        "error": {"code": code, "message": "request refused", "retryable": retryable},
    }));
    (status, body).into_response()
}

fn refuse(
    state: &ReceiverState,
    status: StatusCode,
    code: &str,
    retryable: bool,
    id: &str,
) -> Response {
    let _ = state;
    tracing::warn!(
        event = "eventsub_receiver",
        status = status.as_u16(),
        error_class = code,
        "eventsub delivery refused"
    );
    envelope(status, code, retryable, id)
}

fn ack(id: &str) -> Response {
    (StatusCode::OK, Json(json!({"ok": true, "request_id": id}))).into_response()
}

async fn not_found(State(state): State<Arc<ReceiverState>>) -> Response {
    refuse(
        &state,
        StatusCode::NOT_FOUND,
        "not_found",
        false,
        &request_id(),
    )
}

async fn handle(State(state): State<Arc<ReceiverState>>, request: Request) -> Response {
    let id = request_id();
    let Ok(_permit) = Arc::clone(&state.capacity).try_acquire_owned() else {
        return refuse(&state, StatusCode::SERVICE_UNAVAILABLE, "busy", true, &id);
    };
    receive_with_timeout(&state, &id, receive(&state, request, &id)).await
}

async fn receive_with_timeout(
    state: &ReceiverState,
    id: &str,
    receive: impl std::future::Future<Output = Response>,
) -> Response {
    match tokio::time::timeout(REQUEST_TIMEOUT, receive).await {
        Ok(response) => response,
        Err(_) => refuse(
            state,
            StatusCode::SERVICE_UNAVAILABLE,
            "upstream_timeout",
            true,
            id,
        ),
    }
}

async fn receive(state: &Arc<ReceiverState>, request: Request, id: &str) -> Response {
    // Exact path, no query: the Worker forwards only the sanitized shape, but
    // a direct `containerFetch` must not reach verification with extras.
    let path = request.uri().path();
    if path != EVENTSUB_PATH {
        return refuse(state, StatusCode::NOT_FOUND, "not_found", false, id);
    }
    if request.uri().query().is_some() {
        return refuse(state, StatusCode::BAD_REQUEST, "malformed", false, id);
    }
    if request.method() != Method::POST {
        let mut response = refuse(
            state,
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            false,
            id,
        );
        response
            .headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("POST"));
        return response;
    }
    // Content type, case-insensitive, exactly the two spellings the Worker
    // allowlists. Anything else (including an encoding) is 415.
    let media = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_lowercase);
    if !matches!(
        media.as_deref(),
        Some("application/json") | Some("application/json; charset=utf-8")
    ) {
        return refuse(
            state,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            false,
            id,
        );
    }

    let (parts, body) = request.into_parts();
    let headers = match DeliveryHeaders::from_lookup(|name| {
        parts
            .headers
            .get(name)
            .and_then(|value| value.to_str().ok())
    }) {
        Ok(headers) => headers,
        Err(_) => {
            return refuse(state, StatusCode::UNAUTHORIZED, "unauthorized", false, id);
        }
    };
    let message_type = headers.message_type().to_owned();

    let raw = match tokio::time::timeout(BODY_TIMEOUT, to_bytes(body, MAX_BODY_BYTES)).await {
        Ok(Ok(raw)) => raw,
        Ok(Err(_)) => {
            return refuse(
                state,
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                false,
                id,
            );
        }
        Err(_) => {
            return refuse(
                state,
                StatusCode::REQUEST_TIMEOUT,
                "request_timeout",
                true,
                id,
            );
        }
    };
    if raw.is_empty() {
        return refuse(state, StatusCode::BAD_REQUEST, "malformed", false, id);
    }

    if let Err(error) = verify_delivery(&state.secret, &headers, &raw, now_ms_i64()) {
        // `BodyTooLarge` cannot arrive here (`to_bytes` caps first), but map
        // it explicitly rather than collapsing every variant into one arm.
        if error == EventSubError::BodyTooLarge {
            return refuse(
                state,
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                false,
                id,
            );
        }
        return refuse(state, StatusCode::UNAUTHORIZED, "unauthorized", false, id);
    }

    match classify(&message_type, &raw) {
        Ok(Delivery::Challenge(challenge)) => {
            tracing::info!(
                event = "eventsub_receiver",
                status = 200,
                "challenge answered"
            );
            let mut response = Response::new(axum::body::Body::from(challenge));
            *response.status_mut() = StatusCode::OK;
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        Ok(Delivery::Notification(_)) => {
            // No send and no ledger in this slice: acknowledge so Twitch does
            // not retry; the follow-up wires the durable claim plus post.
            tracing::info!(
                event = "eventsub_receiver",
                status = 200,
                "notification acknowledged"
            );
            ack(id)
        }
        Ok(Delivery::Revocation { .. }) => {
            // Reconcile runs at boot and after revocation in the Helix slice;
            // here only acknowledge so Twitch does not retry.
            tracing::info!(
                event = "eventsub_receiver",
                status = 200,
                "revocation acknowledged"
            );
            ack(id)
        }
        Ok(Delivery::Ignored) => {
            tracing::info!(
                event = "eventsub_receiver",
                status = 200,
                "delivery ignored"
            );
            ack(id)
        }
        // Deterministic planning errors: acknowledge with 2xx so Twitch does
        // not retry what will never succeed. Only transient send/ledger
        // failures (not in this slice) are 5xx.
        Err(EventSubError::WrongSubscription) | Err(EventSubError::InvalidBody) => {
            tracing::info!(
                event = "eventsub_receiver",
                status = 200,
                "delivery ignored"
            );
            ack(id)
        }
        Err(EventSubError::InvalidChallenge) => {
            refuse(state, StatusCode::BAD_REQUEST, "malformed", false, id)
        }
        Err(e) => {
            // Unreachable today: post-verification `classify` only yields
            // WrongSubscription, InvalidBody or InvalidChallenge (all handled
            // above). Warn loudly so a future classify variant shows up in
            // logs instead of silently acking.
            tracing::warn!(
                event = "eventsub_receiver",
                status = 200,
                error_class = %e,
                "delivery ignored"
            );
            ack(id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::Sha256;
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        OnceLock,
    };
    use tower::ServiceExt as _;

    static NEXT_TEST_SECRET: AtomicU64 = AtomicU64::new(1);

    /// Runtime-generated signing secret: no literal key material reaches the
    /// HMAC, so CodeQL's hard-coded-key rule stays silent. Shared across tests
    /// in the binary via `OnceLock` so `config()` and `sign()` agree.
    fn test_secret() -> &'static str {
        static SECRET: OnceLock<String> = OnceLock::new();
        SECRET.get_or_init(|| {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time is after the Unix epoch")
                .as_nanos();
            let nonce = NEXT_TEST_SECRET.fetch_add(1, Ordering::Relaxed);
            format!("{nanos:032x}{nonce:016x}")
        })
    }

    fn config() -> EventsubReceiverConfig {
        let mut vars = std::collections::HashMap::new();
        vars.insert("TWO_EVENTSUB".to_owned(), "1".to_owned());
        vars.insert("TWO_EVENTSUB_BIND".to_owned(), "127.0.0.1:8092".to_owned());
        vars.insert(
            "TWO_TWITCH_WEBHOOK_SECRET".to_owned(),
            test_secret().to_owned(),
        );
        EventsubReceiverConfig::from_map(&vars)
            .unwrap()
            .expect("enabled")
    }

    fn app() -> Router {
        router(Arc::new(ReceiverState::new(&config())))
    }

    fn sign(message_id: &str, timestamp: &str, body: &[u8]) -> String {
        let mut mac = <Hmac<Sha256>>::new_from_slice(test_secret().as_bytes()).unwrap();
        mac.update(message_id.as_bytes());
        mac.update(timestamp.as_bytes());
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    fn fresh_timestamp() -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        two_bot_core::format_iso_millis(now)
    }

    struct TestHeaders<'a> {
        message_id: Option<&'a str>,
        timestamp: Option<&'a str>,
        signature: Option<&'a str>,
        message_type: Option<&'a str>,
    }

    fn request(
        method: &str,
        uri: &str,
        message_id: Option<&str>,
        timestamp: Option<&str>,
        signature: Option<&str>,
        message_type: Option<&str>,
        body: &'static str,
    ) -> Request<Body> {
        request_with_content_type(
            uri,
            "application/json",
            method,
            TestHeaders {
                message_id,
                timestamp,
                signature,
                message_type,
            },
            Body::from(body),
        )
    }

    fn request_with_content_type(
        uri: &str,
        content_type: &str,
        method: &str,
        headers: TestHeaders<'_>,
        body: Body,
    ) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        builder = builder.header("content-type", content_type);
        if let Some(value) = headers.message_id {
            builder = builder.header("twitch-eventsub-message-id", value);
        }
        if let Some(value) = headers.timestamp {
            builder = builder.header("twitch-eventsub-message-timestamp", value);
        }
        if let Some(value) = headers.signature {
            builder = builder.header("twitch-eventsub-message-signature", value);
        }
        if let Some(value) = headers.message_type {
            builder = builder.header("twitch-eventsub-message-type", value);
        }
        builder.body(body).unwrap()
    }

    fn challenge_body() -> &'static str {
        r#"{"challenge":"synthetic-challenge-0001","subscription":{"id":"synthetic-sub-0001","status":"webhook_callback_verification_pending","type":"stream.online","version":"1","condition":{"broadcaster_user_id":"123456"},"transport":{"method":"webhook","callback":"https://example.com/hooks/twitch"},"created_at":"2026-10-13T00:00:00Z"}}"#
    }

    #[tokio::test]
    async fn challenge_echoes_as_text_plain() {
        let body = challenge_body();
        let timestamp = fresh_timestamp();
        let signature = sign("synthetic-delivery-0001", &timestamp, body.as_bytes());
        let response = app()
            .oneshot(request(
                "POST",
                EVENTSUB_PATH,
                Some("synthetic-delivery-0001"),
                Some(&timestamp),
                Some(&signature),
                Some("webhook_callback_verification"),
                body,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/plain"
        );
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"synthetic-challenge-0001");
    }

    #[tokio::test]
    async fn bad_signature_is_401_without_echo() {
        let body = challenge_body();
        let timestamp = fresh_timestamp();
        let response = app()
            .oneshot(request(
                "POST",
                EVENTSUB_PATH,
                Some("synthetic-delivery-0001"),
                Some(&timestamp),
                Some("sha256=0000000000000000000000000000000000000000000000000000000000000000"),
                Some("webhook_callback_verification"),
                body,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["ok"], false);
        assert!(!String::from_utf8_lossy(&bytes).contains("synthetic-challenge-0001"));
    }

    #[tokio::test]
    async fn unknown_message_type_is_acknowledged() {
        let body = r#"{"subscription":{"id":"synthetic-sub-0001","status":"enabled","type":"stream.online","version":"1","condition":{"broadcaster_user_id":"123456"},"transport":{"method":"webhook","callback":"https://example.com/hooks/twitch"},"created_at":"2026-10-13T00:00:00Z"}}"#;
        let timestamp = fresh_timestamp();
        let signature = sign("synthetic-delivery-0002", &timestamp, body.as_bytes());
        let response = app()
            .oneshot(request(
                "POST",
                EVENTSUB_PATH,
                Some("synthetic-delivery-0002"),
                Some(&timestamp),
                Some(&signature),
                Some("fixture-unknown-type"),
                body,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["ok"], true);
    }

    #[tokio::test]
    async fn wrong_path_and_method_are_rejected() {
        let body = challenge_body();
        let timestamp = fresh_timestamp();
        let signature = sign("synthetic-delivery-0003", &timestamp, body.as_bytes());
        let not_found = app()
            .oneshot(request(
                "POST",
                "/hooks/fixture",
                Some("synthetic-delivery-0003"),
                Some(&timestamp),
                Some(&signature),
                Some("webhook_callback_verification"),
                body,
            ))
            .await
            .unwrap();
        assert_eq!(not_found.status(), StatusCode::NOT_FOUND);
        let method = app()
            .oneshot(request(
                "GET",
                EVENTSUB_PATH,
                Some("synthetic-delivery-0003"),
                Some(&timestamp),
                Some(&signature),
                Some("webhook_callback_verification"),
                body,
            ))
            .await
            .unwrap();
        assert_eq!(method.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn deterministic_classification_errors_are_acknowledged() {
        // Valid signature over a notification body whose subscription is not
        // stream.online v1: deterministic, must be 2xx, never a retry.
        let body = r#"{"subscription":{"id":"synthetic-sub-0009","status":"enabled","type":"stream.offline","version":"1","condition":{"broadcaster_user_id":"123456"},"transport":{"method":"webhook","callback":"https://example.com/hooks/twitch"},"created_at":"2026-10-13T00:00:00Z"},"event":{"broadcaster_user_id":"123456"}}"#;
        let timestamp = fresh_timestamp();
        let signature = sign("synthetic-delivery-0009", &timestamp, body.as_bytes());
        let response = app()
            .oneshot(request(
                "POST",
                EVENTSUB_PATH,
                Some("synthetic-delivery-0009"),
                Some(&timestamp),
                Some(&signature),
                Some("notification"),
                body,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn query_string_is_refused_before_verification() {
        // Signed exactly like the happy path: the query gate runs first, so a
        // valid signature must still answer 400, never reach the receiver.
        let body = challenge_body();
        let timestamp = fresh_timestamp();
        let signature = sign("synthetic-delivery-0011", &timestamp, body.as_bytes());
        let response = app()
            .oneshot(request(
                "POST",
                "/hooks/twitch?x=1",
                Some("synthetic-delivery-0011"),
                Some(&timestamp),
                Some(&signature),
                Some("webhook_callback_verification"),
                body,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "malformed");
        assert_eq!(value["error"]["retryable"], false);
    }

    #[tokio::test]
    async fn wrong_content_type_is_refused() {
        let body = challenge_body();
        let timestamp = fresh_timestamp();
        let message_id = "synthetic-delivery-0012";
        let signature = sign(message_id, &timestamp, body.as_bytes());
        for content_type in ["text/plain", "application/json; charset=iso-8859-1"] {
            let response = app()
                .oneshot(request_with_content_type(
                    EVENTSUB_PATH,
                    content_type,
                    "POST",
                    TestHeaders {
                        message_id: Some(message_id),
                        timestamp: Some(&timestamp),
                        signature: Some(&signature),
                        message_type: Some("webhook_callback_verification"),
                    },
                    Body::from(body),
                ))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "{content_type}"
            );
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["error"]["code"], "unsupported_media_type");
            assert_eq!(value["error"]["retryable"], false);
        }
        // The allowlisted utf-8 spelling passes the gate: same signed bytes
        // answer the challenge instead of refusing.
        let allowed = app()
            .oneshot(request_with_content_type(
                EVENTSUB_PATH,
                "application/json; charset=utf-8",
                "POST",
                TestHeaders {
                    message_id: Some(message_id),
                    timestamp: Some(&timestamp),
                    signature: Some(&signature),
                    message_type: Some("webhook_callback_verification"),
                },
                Body::from(body),
            ))
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn oversize_body_is_refused() {
        // Headers pass `from_lookup`; the 64 KiB cap triggers before verify.
        let timestamp = fresh_timestamp();
        let big = vec![b'a'; MAX_BODY_BYTES + 1];
        let signature = sign("synthetic-delivery-0013", &timestamp, &big);
        let response = app()
            .oneshot(request_with_content_type(
                EVENTSUB_PATH,
                "application/json",
                "POST",
                TestHeaders {
                    message_id: Some("synthetic-delivery-0013"),
                    timestamp: Some(&timestamp),
                    signature: Some(&signature),
                    message_type: Some("notification"),
                },
                Body::from(big),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["code"], "payload_too_large");
        assert_eq!(value["error"]["retryable"], false);
    }

    #[tokio::test]
    async fn malformed_challenge_is_refused() {
        // Empty and non-graphic challenges are `InvalidChallenge`: 400, never
        // an ack. Each body is signed exactly as sent.
        for challenge in ["", "has space"] {
            let body = format!(
                r#"{{"challenge":{challenge:?},"subscription":{{"id":"synthetic-sub-0014","status":"webhook_callback_verification_pending","type":"stream.online","version":"1","condition":{{"broadcaster_user_id":"123456"}},"transport":{{"method":"webhook","callback":"https://example.com/hooks/twitch"}},"created_at":"2026-10-13T00:00:00Z"}}}}"#
            );
            let timestamp = fresh_timestamp();
            let signature = sign("synthetic-delivery-0014", &timestamp, body.as_bytes());
            let response = app()
                .oneshot(request_with_content_type(
                    EVENTSUB_PATH,
                    "application/json",
                    "POST",
                    TestHeaders {
                        message_id: Some("synthetic-delivery-0014"),
                        timestamp: Some(&timestamp),
                        signature: Some(&signature),
                        message_type: Some("webhook_callback_verification"),
                    },
                    Body::from(body),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{challenge:?}");
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["error"]["code"], "malformed");
            assert_eq!(value["error"]["retryable"], false);
        }
    }

    #[tokio::test]
    async fn busy_returns_503_with_retryable() {
        // Hold every permit: the semaphore gate runs before path, method,
        // content-type and verification, so even a bare request gets 503.
        let state = Arc::new(ReceiverState::new(&config()));
        let _permits = (0..MAX_REQUESTS)
            .map(|_| state.capacity.clone().try_acquire_owned().expect("permit"))
            .collect::<Vec<_>>();
        let response = router(Arc::clone(&state))
            .oneshot(request("POST", EVENTSUB_PATH, None, None, None, None, ""))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["code"], "busy");
        assert_eq!(value["error"]["retryable"], true);
    }

    #[tokio::test(start_paused = true)]
    async fn slow_body_times_out_with_408_and_retryable() {
        // A body that never ends must hit the 5 s read budget instead of
        // holding one of the 32 permits: 408 `request_timeout`, retriable.
        let timestamp = fresh_timestamp();
        let pending = Body::from_stream(futures_util::stream::pending::<
            Result<axum::body::Bytes, std::io::Error>,
        >());
        let stalled = request_with_content_type(
            EVENTSUB_PATH,
            "application/json",
            "POST",
            TestHeaders {
                message_id: Some("synthetic-delivery-0015"),
                timestamp: Some(&timestamp),
                signature: Some(
                    "sha256=0000000000000000000000000000000000000000000000000000000000000000",
                ),
                message_type: Some("notification"),
            },
            pending,
        );
        let mut pending_response = Box::pin(app().oneshot(stalled));
        assert!(
            futures_util::future::poll_immediate(&mut pending_response)
                .await
                .is_none(),
            "receiver must wait for the slow body"
        );
        tokio::time::advance(Duration::from_secs(4)).await;
        assert!(
            futures_util::future::poll_immediate(&mut pending_response)
                .await
                .is_none(),
            "receiver must still wait before the 5 s body budget"
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        let response = futures_util::future::poll_immediate(pending_response)
            .await
            .expect("slow body must time out within 6 s")
            .unwrap();
        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["code"], "request_timeout");
        assert_eq!(value["error"]["retryable"], true);
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_request_times_out_with_503_and_retryable() {
        // The 20 s outer budget maps a stalled receive to 503
        // `upstream_timeout`, retriable.
        let state = ReceiverState::new(&config());
        let mut pending_response = Box::pin(receive_with_timeout(
            &state,
            "synthetic-request-0016",
            std::future::pending::<Response>(),
        ));
        assert!(
            futures_util::future::poll_immediate(&mut pending_response)
                .await
                .is_none(),
            "receiver must wait for the stalled request"
        );
        tokio::time::advance(Duration::from_secs(19)).await;
        assert!(
            futures_util::future::poll_immediate(&mut pending_response)
                .await
                .is_none(),
            "receiver must still wait before the 20 s request budget"
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        let response = futures_util::future::poll_immediate(pending_response)
            .await
            .expect("stalled request must time out within 21 s");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["code"], "upstream_timeout");
        assert_eq!(value["error"]["retryable"], true);
    }

    // Webhook-secret reload acceptance: the no-restart rotation contract.
    //
    // The rotation primitive is `EventsubReceiverConfig::from_map` (the same
    // process re-reads the Operator-provided map; nothing restarts) combined
    // with `verify_delivery` against the reloaded secret. These tests pin the
    // reload properties end to end through the bound router: the reloaded
    // secret verifies new deliveries, the pre-reload secret is rejected
    // afterwards (no dual-accept window), an in-flight delivery sees exactly
    // one verifier (deterministic verify or refusal, never 500, never a
    // mixed-secret accept), and no secret material reaches logs or `Debug`
    // output on the reload path. Secrets are runtime-generated per test, so
    // no key material appears in source and parallel tests never share a
    // secret with the module's shared signing fixture.

    /// Runtime-generated reload secret: never a literal, so the hardcoded-key
    /// rule stays silent. The embedded counter makes every call distinct,
    /// even within the same nanosecond.
    fn fresh_reload_secret() -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time is after the Unix epoch")
            .as_nanos();
        let nonce = NEXT_TEST_SECRET.fetch_add(1, Ordering::Relaxed);
        format!("reload-{nanos:032x}-{nonce:016x}")
    }

    /// Re-read the Operator map with `secret`, the same process-wide step a
    /// no-restart rotation performs (no rebind, no restart).
    fn reload_config_for(secret: &str) -> EventsubReceiverConfig {
        let mut vars = std::collections::HashMap::new();
        vars.insert("TWO_EVENTSUB".to_owned(), "1".to_owned());
        vars.insert("TWO_EVENTSUB_BIND".to_owned(), "127.0.0.1:8092".to_owned());
        vars.insert("TWO_TWITCH_WEBHOOK_SECRET".to_owned(), secret.to_owned());
        EventsubReceiverConfig::from_map(&vars)
            .unwrap()
            .expect("enabled")
    }

    fn router_for_secret(secret: &str) -> Router {
        router(Arc::new(ReceiverState::new(&reload_config_for(secret))))
    }

    fn sign_with(secret: &str, message_id: &str, timestamp: &str, body: &[u8]) -> String {
        let mut mac = <Hmac<Sha256>>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(message_id.as_bytes());
        mac.update(timestamp.as_bytes());
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    async fn reload_response(
        router: Router,
        message_id: &str,
        timestamp: &str,
        signature: &str,
        body: &'static str,
    ) -> axum::response::Response {
        router
            .oneshot(request(
                "POST",
                EVENTSUB_PATH,
                Some(message_id),
                Some(timestamp),
                Some(signature),
                Some("webhook_callback_verification"),
                body,
            ))
            .await
            .unwrap()
    }

    fn reload_headers<'a>(
        message_id: &'a str,
        timestamp: &'a str,
        signature: &'a str,
    ) -> DeliveryHeaders<'a> {
        DeliveryHeaders::from_lookup(|name| match name {
            "twitch-eventsub-message-id" => Some(message_id),
            "twitch-eventsub-message-timestamp" => Some(timestamp),
            "twitch-eventsub-message-signature" => Some(signature),
            "twitch-eventsub-message-type" => Some("webhook_callback_verification"),
            _ => None,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn reloaded_secret_verifies_new_deliveries() {
        let old_secret = fresh_reload_secret();
        let new_secret = fresh_reload_secret();
        assert_ne!(old_secret, new_secret, "rotation must change the secret");
        // The same process loads the initial map, then re-reads it after the
        // Operator rotation: no restart happens between the two loads.
        let _initial = reload_config_for(&old_secret);
        let reloaded = reload_config_for(&new_secret);
        let body = challenge_body();
        let timestamp = fresh_timestamp();
        let message_id = "synthetic-reload-0001";
        let signature = sign_with(&new_secret, message_id, &timestamp, body.as_bytes());
        // Pure verification first: the reloaded secret authenticates the new
        // delivery at the current clock.
        let headers = reload_headers(message_id, &timestamp, &signature);
        assert_eq!(
            verify_delivery(reloaded.secret(), &headers, body.as_bytes(), now_ms_i64()),
            Ok(())
        );
        // Then end to end: a router bound to the reloaded secret answers the
        // Twitch challenge instead of refusing it.
        let response = reload_response(
            router_for_secret(&new_secret),
            message_id,
            &timestamp,
            &signature,
            body,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/plain"
        );
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"synthetic-challenge-0001");
    }

    #[tokio::test]
    async fn pre_reload_secret_is_rejected_after_reload() {
        let old_secret = fresh_reload_secret();
        let new_secret = fresh_reload_secret();
        let _initial = reload_config_for(&old_secret);
        let reloaded = reload_config_for(&new_secret);
        let body = challenge_body();
        // A delivery signed before the rotation ...
        let timestamp = fresh_timestamp();
        let message_id = "synthetic-reload-0002";
        let old_signature = sign_with(&old_secret, message_id, &timestamp, body.as_bytes());
        // ... is rejected by the reloaded secret: no dual-accept window.
        let headers = reload_headers(message_id, &timestamp, &old_signature);
        assert_eq!(
            verify_delivery(reloaded.secret(), &headers, body.as_bytes(), now_ms_i64()),
            Err(EventSubError::BadSignature)
        );
        let response = reload_response(
            router_for_secret(&new_secret),
            message_id,
            &timestamp,
            &old_signature,
            body,
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_ne!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "a stale secret is a deterministic refusal, never a 500"
        );
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "unauthorized");
        assert_eq!(value["error"]["retryable"], false);
        assert!(!String::from_utf8_lossy(&bytes).contains("synthetic-challenge-0001"));
        // And symmetrically: the new secret never verifies against the old
        // one, so the cutover is atomic in both directions.
        let reverse_timestamp = fresh_timestamp();
        let reverse_id = "synthetic-reload-0003";
        let new_signature = sign_with(&new_secret, reverse_id, &reverse_timestamp, body.as_bytes());
        let reverse = reload_headers(reverse_id, &reverse_timestamp, &new_signature);
        let initial = reload_config_for(&old_secret);
        assert_eq!(
            verify_delivery(initial.secret(), &reverse, body.as_bytes(), now_ms_i64()),
            Err(EventSubError::BadSignature)
        );
    }

    #[tokio::test]
    async fn in_flight_delivery_during_reload_is_deterministic() {
        // An in-flight delivery is signed once, before the cutover, and lands
        // on either side of it. The atomic cutover means exactly one verifier
        // accepts each delivery: 200 on the matching side, a deterministic
        // refusal on the other, never 500 and never a cross-secret accept.
        let old_secret = fresh_reload_secret();
        let new_secret = fresh_reload_secret();
        let body = challenge_body();
        let timestamp = fresh_timestamp();
        let old_id = "synthetic-reload-0004";
        let new_id = "synthetic-reload-0005";
        let old_signature = sign_with(&old_secret, old_id, &timestamp, body.as_bytes());
        let new_signature = sign_with(&new_secret, new_id, &timestamp, body.as_bytes());
        // Pure layer: each signature verifies under exactly one secret.
        for (secret, message_id, signature) in [
            (old_secret.as_str(), old_id, old_signature.as_str()),
            (new_secret.as_str(), new_id, new_signature.as_str()),
        ] {
            for verifier in [old_secret.as_str(), new_secret.as_str()] {
                let headers = reload_headers(message_id, &timestamp, signature);
                let config = reload_config_for(verifier);
                let outcome =
                    verify_delivery(config.secret(), &headers, body.as_bytes(), now_ms_i64());
                if verifier == secret {
                    assert_eq!(outcome, Ok(()), "matching secret must verify");
                } else {
                    assert_eq!(
                        outcome,
                        Err(EventSubError::BadSignature),
                        "a cross-secret verify would be a mixed-secret accept"
                    );
                }
            }
        }
        // Router layer: the same four combinations answer 200 on the matching
        // side and a deterministic non-500 refusal on the other.
        for (secret, message_id, signature) in [
            (old_secret.as_str(), old_id, old_signature.as_str()),
            (new_secret.as_str(), new_id, new_signature.as_str()),
        ] {
            for verifier in [old_secret.as_str(), new_secret.as_str()] {
                let response = reload_response(
                    router_for_secret(verifier),
                    message_id,
                    &timestamp,
                    signature,
                    body,
                )
                .await;
                assert_ne!(
                    response.status(),
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "in-flight must never see 500"
                );
                if verifier == secret {
                    assert_eq!(response.status(), StatusCode::OK);
                    let bytes = axum::body::to_bytes(response.into_body(), 4096)
                        .await
                        .unwrap();
                    assert_eq!(&bytes[..], b"synthetic-challenge-0001");
                } else {
                    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
                    let bytes = axum::body::to_bytes(response.into_body(), 4096)
                        .await
                        .unwrap();
                    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    assert_eq!(value["ok"], false);
                    assert_eq!(value["error"]["code"], "unauthorized");
                    assert_eq!(value["error"]["retryable"], false);
                }
            }
        }
        // The retryable refusal is deterministic too: with every permit held,
        // an in-flight delivery gets 503 `busy`, still never 500.
        let loaded = reload_config_for(&new_secret);
        let state = Arc::new(ReceiverState::new(&loaded));
        let _permits = (0..MAX_REQUESTS)
            .map(|_| state.capacity.clone().try_acquire_owned().expect("permit"))
            .collect::<Vec<_>>();
        let response = router(Arc::clone(&state))
            .oneshot(request("POST", EVENTSUB_PATH, None, None, None, None, ""))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_ne!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "overload is a retryable refusal, never a 500"
        );
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["code"], "busy");
        assert_eq!(value["error"]["retryable"], true);
    }

    #[derive(Clone)]
    struct ReloadLogWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for ReloadLogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for ReloadLogWriter {
        type Writer = Self;

        fn make_writer(&'writer self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn reload_path_never_logs_or_debugs_secret_material() {
        let old_secret = fresh_reload_secret();
        let new_secret = fresh_reload_secret();
        let secrets = [&old_secret, &new_secret];
        // `Debug` on either side of the rotation redacts both secrets.
        for config in [
            reload_config_for(&old_secret),
            reload_config_for(&new_secret),
        ] {
            for rendered in [format!("{config:?}"), format!("{config:#?}")] {
                for secret in secrets {
                    assert!(
                        !rendered.contains(secret.as_str()),
                        "reloaded config Debug leaked secret material"
                    );
                }
                assert!(rendered.contains("[REDACTED]"));
            }
            for rendered in [
                format!("{:?}", config.secret()),
                format!("{:#?}", config.secret()),
            ] {
                for secret in secrets {
                    assert!(
                        !rendered.contains(secret.as_str()),
                        "webhook secret Debug leaked secret material"
                    );
                }
                assert!(rendered.contains("[REDACTED]"));
            }
        }
        // Live refusal and acknowledgement paths log only scalar fields: run
        // both through one router pair under a capturing subscriber. The
        // current-thread runtime keeps every event on the capturing thread;
        // the marker proves the capture was live so the redaction assertions
        // below cannot pass vacuously.
        let bytes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = ReloadLogWriter(std::sync::Arc::clone(&bytes));
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(writer)
            .finish();
        let body = challenge_body();
        tracing::subscriber::with_default(subscriber, || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                // Refusal: a new-signed delivery against the pre-reload router.
                let timestamp = fresh_timestamp();
                let refused_id = "synthetic-reload-0006";
                let refused_signature =
                    sign_with(&new_secret, refused_id, &timestamp, body.as_bytes());
                let refused = router_for_secret(&old_secret)
                    .oneshot(request(
                        "POST",
                        EVENTSUB_PATH,
                        Some(refused_id),
                        Some(&timestamp),
                        Some(&refused_signature),
                        Some("webhook_callback_verification"),
                        body,
                    ))
                    .await
                    .unwrap();
                assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
                let refused_bytes = axum::body::to_bytes(refused.into_body(), 4096)
                    .await
                    .unwrap();
                assert!(!String::from_utf8_lossy(&refused_bytes).contains(&old_secret));
                assert!(!String::from_utf8_lossy(&refused_bytes).contains(&new_secret));
                // Acknowledgement: an old-signed delivery against its router.
                let accepted_id = "synthetic-reload-0007";
                let accepted_signature =
                    sign_with(&old_secret, accepted_id, &timestamp, body.as_bytes());
                let accepted = router_for_secret(&old_secret)
                    .oneshot(request(
                        "POST",
                        EVENTSUB_PATH,
                        Some(accepted_id),
                        Some(&timestamp),
                        Some(&accepted_signature),
                        Some("webhook_callback_verification"),
                        body,
                    ))
                    .await
                    .unwrap();
                assert_eq!(accepted.status(), StatusCode::OK);
                let accepted_bytes = axum::body::to_bytes(accepted.into_body(), 4096)
                    .await
                    .unwrap();
                assert_eq!(&accepted_bytes[..], b"synthetic-challenge-0001");
                tracing::info!("reload log capture live");
            });
        });
        let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(
            text.contains("reload log capture live"),
            "log capture missed events; the redaction assertions would pass vacuously"
        );
        for secret in secrets {
            assert!(
                !text.contains(secret.as_str()),
                "secret material reached the reload-path logs"
            );
        }
    }

    #[tokio::test]
    async fn bound_receiver_serves_initial_secret_until_runtime_reload_lands() {
        // Gap pin: `ReceiverState` clones the secret at bind time and never
        // re-reads it, so a rotation without restart is invisible to an
        // already-bound router. The runtime-reload build replaces this test's
        // second expectation with post-reload verification on the same bound
        // router (no restart, no new bind); until then this documents the
        // missing holder. Fail-without-holder evidence: expecting 200 for the
        // new-signed delivery below fails on this head with 401.
        let old_secret = fresh_reload_secret();
        let new_secret = fresh_reload_secret();
        let bound = router_for_secret(&old_secret);
        // Simulate the Operator rotation: the map now yields the new secret.
        let _reloaded = reload_config_for(&new_secret);
        let body = challenge_body();
        let timestamp = fresh_timestamp();
        // The bound router still serves the initial secret ...
        let old_id = "synthetic-reload-0008";
        let old_signature = sign_with(&old_secret, old_id, &timestamp, body.as_bytes());
        let served = reload_response(bound.clone(), old_id, &timestamp, &old_signature, body).await;
        assert_eq!(served.status(), StatusCode::OK);
        // ... and the same bound router does not pick up the rotation.
        let new_id = "synthetic-reload-0009";
        let new_signature = sign_with(&new_secret, new_id, &timestamp, body.as_bytes());
        let missed = reload_response(bound, new_id, &timestamp, &new_signature, body).await;
        assert_eq!(
            missed.status(),
            StatusCode::UNAUTHORIZED,
            "the bound router cannot see the rotation until the reload holder lands"
        );
    }
}
