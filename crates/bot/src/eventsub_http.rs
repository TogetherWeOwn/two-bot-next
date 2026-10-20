//! Private Twitch EventSub receiver (`POST /hooks/twitch`). Never merge this
//! router into the health socket. Verification (`twitch_eventsub::verify_delivery`)
//! precedes any JSON parsing; when `TWO_EVENTSUB_RELOAD_KEY` arms rotation, an
//! authenticated per-delivery refresh (`eventsub_refresh`) swaps the secret
//! before that verification, and any refresh failure refuses with retryable
//! 503. Known deterministic classification errors are
//! acknowledged with 2xx so Twitch does not retry them. Unknown/future errors
//! fail closed with retryable 503. A verified `stream.online` runs the
//! durable claim → send → settle flow (`stream_live_store::deliver_go_live`)
//! only when delivery dependencies were injected at bind time
//! (`eventsub_delivery`); with none, it is acknowledged without side
//! effects, so the route stays dark for posting until provisioning lands.
//!
//! Logging carries only scalar `event`/`status`/`error_class` plus the bounded
//! `message_type` (refusals) and `reason` (revocation ack) scalars: no header
//! values, challenge, body bytes or query. The callback path never reaches a
//! trace span (`server::TRACED_PATHS` must not list it; unknown paths render
//! as `[REDACTED]`).

use std::{
    future::IntoFuture,
    sync::{Arc, RwLock},
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
use tracing::Instrument;
use two_bot_core::{
    eventsub_receiver_config::EventsubReceiverConfig,
    eventsub_refresh::{refresh_before_verify, RefreshVerifier},
    stream_live_store::deliver_go_live,
    twitch_eventsub::{classify, verify_delivery, Delivery, DeliveryHeaders, EventSubError},
};

use crate::eventsub_delivery::{post_metric_counts, DeliveryDeps};

/// Exact callback path; mirrors `wrangler/src/eventsub.ts` (`EVENTSUB_PATH`).
pub const EVENTSUB_PATH: &str = "/hooks/twitch";
const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_REQUESTS: usize = 32;
const BODY_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

struct ReceiverState {
    secret: RwLock<two_bot_core::twitch_eventsub::WebhookSecret>,
    /// No-restart rotation, armed only when the Operator provisioned
    /// `TWO_EVENTSUB_RELOAD_KEY`. `None` keeps the legacy startup-secret
    /// behavior: the snapshot below never changes after bind.
    reload: Option<RefreshVerifier>,
    capacity: Arc<Semaphore>,
    /// Go-live delivery dependencies, injected at bind time. `None` keeps
    /// verified notifications side-effect free (acknowledge only).
    delivery: Option<Arc<DeliveryDeps>>,
}

impl ReceiverState {
    fn new(config: &EventsubReceiverConfig) -> Self {
        Self::with_delivery(config, None)
    }

    fn with_delivery(config: &EventsubReceiverConfig, delivery: Option<DeliveryDeps>) -> Self {
        Self {
            // Keep one atomic secret snapshot in the state; handlers never
            // re-read the environment or observe partially replaced material.
            secret: RwLock::new(config.secret().clone()),
            reload: config.reload_key().cloned().map(RefreshVerifier::new),
            capacity: Arc::new(Semaphore::new(MAX_REQUESTS)),
            delivery: delivery.map(Arc::new),
        }
    }
}

/// Binding completes before any gateway/job task starts. Invalid enabled
/// configuration and bind failure are fatal; there is no health-only fallback.
pub struct BoundEventsubReceiver {
    listener: TcpListener,
    state: Arc<ReceiverState>,
}

/// Bind with optional go-live delivery dependencies: verified
/// notifications then run the durable ledger flow instead of ack-only.
/// Boot passes the production dependencies while delivery is enabled and
/// `None` while it stays dark. Binding completes before any gateway/job
/// task starts; invalid enabled configuration and bind failure are fatal.
pub async fn bind_with_delivery(
    config: EventsubReceiverConfig,
    delivery: Option<DeliveryDeps>,
) -> std::io::Result<BoundEventsubReceiver> {
    let listener = TcpListener::bind(config.listen_addr()).await?;
    let state = match delivery {
        Some(deps) => ReceiverState::with_delivery(&config, Some(deps)),
        None => ReceiverState::new(&config),
    };
    Ok(BoundEventsubReceiver {
        listener,
        state: Arc::new(state),
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

/// Bounded `message_type` scalar for refuse lines: the three Twitch types the
/// receiver handles, else `unknown`. Raw header values never reach a log
/// line, so a failed challenge separates from a malformed notification or
/// an edge probe in logs alone.
fn bounded_message_type(value: &str) -> &'static str {
    match value {
        "webhook_callback_verification" => "webhook_callback_verification",
        "notification" => "notification",
        "revocation" => "revocation",
        _ => "unknown",
    }
}

fn refuse(
    state: &ReceiverState,
    status: StatusCode,
    code: &str,
    retryable: bool,
    id: &str,
    message_type: &'static str,
) -> Response {
    let _ = state;
    tracing::warn!(
        event = "eventsub_receiver",
        status = status.as_u16(),
        error_class = code,
        message_type = message_type,
        "eventsub delivery refused"
    );
    envelope(status, code, retryable, id)
}

fn ack(id: &str) -> Response {
    (StatusCode::OK, Json(json!({"ok": true, "request_id": id}))).into_response()
}

async fn not_found(State(state): State<Arc<ReceiverState>>, request: Request) -> Response {
    // Honor the Worker's `x-request-id` when it has a generated shape, so one
    // id joins the Worker, Durable Object and container logs.
    let id = crate::logging::incoming_request_id(request.headers());
    let span = crate::logging::request_span(&id);
    async move {
        refuse(
            &state,
            StatusCode::NOT_FOUND,
            "not_found",
            false,
            &id,
            "unknown",
        )
    }
    .instrument(span)
    .await
}

async fn handle(State(state): State<Arc<ReceiverState>>, request: Request) -> Response {
    let id = crate::logging::incoming_request_id(request.headers());
    let span = crate::logging::request_span(&id);
    async move {
        let Ok(_permit) = Arc::clone(&state.capacity).try_acquire_owned() else {
            return refuse(
                &state,
                StatusCode::SERVICE_UNAVAILABLE,
                "busy",
                true,
                &id,
                "unknown",
            );
        };
        receive_with_timeout(&state, &id, receive(&state, request, &id)).await
    }
    .instrument(span)
    .await
}

async fn receive_with_timeout(
    state: &ReceiverState,
    id: &str,
    receive: impl std::future::Future<Output = Response>,
) -> Response {
    match tokio::time::timeout(REQUEST_TIMEOUT, receive).await {
        Ok(response) => response,
        // The stalled future is opaque on timeout: the message type may never
        // have been established, so `unknown` is the honest scalar.
        Err(_) => refuse(
            state,
            StatusCode::SERVICE_UNAVAILABLE,
            "upstream_timeout",
            true,
            id,
            "unknown",
        ),
    }
}

/// One verified `stream.online`: with no delivery dependencies this is the
/// historical ack-only behavior; with them, run the durable claim → send →
/// settle flow and answer its disposition — 200 acknowledges (posted,
/// duplicate, or not ours), a retryable 503 asks Twitch to redeliver after
/// a transient ledger/send failure. The claim fences make that redelivery
/// safe: it can never double-post (see `deliver_go_live`). Delivery runs in
/// a spawned task so the 20 s request timeout cannot cancel it after
/// `mark_started` commits but before the send settles; the task keeps
/// running to settle even if the request times out, and the Twitch retry is
/// then answered as a duplicate instead of losing the announcement.
async fn handle_notification(
    state: &Arc<ReceiverState>,
    stream: &two_bot_core::stream_funnel::StreamOnline,
    id: &str,
) -> Response {
    let Some(deps) = &state.delivery else {
        tracing::info!(
            event = "eventsub_receiver",
            status = 200,
            "notification acknowledged"
        );
        return ack(id);
    };
    let task_deps = Arc::clone(deps);
    let task_stream = (*stream).clone();
    let delivery = match tokio::spawn(async move {
        deliver_go_live(
            task_deps.ledger.as_ref(),
            task_deps.sender.as_ref(),
            &task_deps.config,
            &task_stream,
        )
        .await
    })
    .await
    {
        Ok(delivery) => delivery,
        Err(_) => {
            return refuse(
                state,
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                true,
                id,
                bounded_message_type("notification"),
            );
        }
    };
    let metrics = two_bot_core::metrics::global();
    let (attempted, succeeded, failed) = post_metric_counts(&delivery.outcome);
    if attempted {
        metrics.eventsub_post("attempted");
    }
    if succeeded {
        metrics.eventsub_post("succeeded");
    }
    if failed {
        metrics.eventsub_post("failed");
    }
    if delivery.disposition.status == 200 {
        tracing::info!(
            event = "eventsub_receiver",
            status = 200,
            "notification acknowledged"
        );
        ack(id)
    } else {
        refuse(
            state,
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            true,
            id,
            bounded_message_type("notification"),
        )
    }
}

fn record_verification_failure(metrics: &two_bot_core::metrics::Metrics, error: &EventSubError) {
    if *error == EventSubError::BadSignature {
        metrics.eventsub_classified("tampered");
    } else {
        metrics.eventsub_classified("ignored");
    }
}

async fn receive(state: &Arc<ReceiverState>, request: Request, id: &str) -> Response {
    // Exact path, no query: the Worker forwards only the sanitized shape, but
    // a direct `containerFetch` must not reach verification with extras.
    let path = request.uri().path();
    if path != EVENTSUB_PATH {
        return refuse(
            state,
            StatusCode::NOT_FOUND,
            "not_found",
            false,
            id,
            "unknown",
        );
    }
    if request.uri().query().is_some() {
        return refuse(
            state,
            StatusCode::BAD_REQUEST,
            "malformed",
            false,
            id,
            "unknown",
        );
    }
    if request.method() != Method::POST {
        let mut response = refuse(
            state,
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            false,
            id,
            "unknown",
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
            "unknown",
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
            // Headers missing or malformed: the message type is untrusted.
            return refuse(
                state,
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                false,
                id,
                "unknown",
            );
        }
    };
    let message_type = headers.message_type().to_owned();

    // No-restart rotation, when armed: the fenced DO attached the Worker's
    // current secret with its own HMAC. Swap before verifying this delivery;
    // any refresh failure refuses retryably without touching the stale
    // snapshot or the Twitch verification below.
    if let Some(reload) = state.reload.as_ref() {
        let refreshed = refresh_before_verify(
            reload,
            &state.secret,
            |name| {
                parts
                    .headers
                    .get(name)
                    .and_then(|value| value.to_str().ok())
            },
            id,
            now_ms_u64(),
        );
        if refreshed.is_err() {
            return refuse(
                state,
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                true,
                id,
                bounded_message_type(&message_type),
            );
        }
    }

    let raw = match tokio::time::timeout(BODY_TIMEOUT, to_bytes(body, MAX_BODY_BYTES)).await {
        Ok(Ok(raw)) => raw,
        Ok(Err(_)) => {
            return refuse(
                state,
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                false,
                id,
                bounded_message_type(&message_type),
            );
        }
        Err(_) => {
            return refuse(
                state,
                StatusCode::REQUEST_TIMEOUT,
                "request_timeout",
                true,
                id,
                bounded_message_type(&message_type),
            );
        }
    };
    if raw.is_empty() {
        return refuse(
            state,
            StatusCode::BAD_REQUEST,
            "malformed",
            false,
            id,
            bounded_message_type(&message_type),
        );
    }

    two_bot_core::metrics::global().eventsub_received();
    let secret = state
        .secret
        .read()
        .expect("EventSub secret lock poisoned")
        .clone();
    if let Err(error) = verify_delivery(&secret, &headers, &raw, now_ms_i64()) {
        // Timestamp failures happen only after a valid HMAC. Only a signature
        // mismatch is evidence of a tampered delivery. `BodyTooLarge` cannot
        // arrive here (`to_bytes` caps first), but map it explicitly rather
        // than collapsing every variant into one arm.
        record_verification_failure(two_bot_core::metrics::global(), &error);
        if error == EventSubError::BodyTooLarge {
            return refuse(
                state,
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                false,
                id,
                bounded_message_type(&message_type),
            );
        }
        return refuse(
            state,
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            false,
            id,
            bounded_message_type(&message_type),
        );
    }

    match classify(&message_type, &raw) {
        Ok(Delivery::Challenge(challenge)) => {
            two_bot_core::metrics::global().eventsub_classified("challenge");
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
        Ok(Delivery::Notification(stream)) => {
            two_bot_core::metrics::global().eventsub_classified("notification");
            handle_notification(state, &stream, id).await
        }
        Ok(Delivery::Revocation { reason, .. }) => {
            two_bot_core::metrics::global().eventsub_classified("revocation");
            // The bounded reconcile pass re-subscribes; here only acknowledge
            // so Twitch does not retry. The bounded `reason` scalar (never the
            // subscription id) answers revocation counts by reason from logs alone.
            tracing::info!(
                event = "eventsub_receiver",
                status = 200,
                reason = reason.as_str(),
                "revocation acknowledged"
            );
            ack(id)
        }
        Ok(Delivery::Ignored) => {
            two_bot_core::metrics::global().eventsub_classified("ignored");
            tracing::info!(
                event = "eventsub_receiver",
                status = 200,
                "delivery ignored"
            );
            ack(id)
        }
        // Deterministic planning errors: acknowledge with 2xx so Twitch does
        // not retry what will never succeed. Unknown/future classification
        // errors remain retryable through the fallback below.
        Err(EventSubError::WrongSubscription) | Err(EventSubError::InvalidBody) => {
            two_bot_core::metrics::global().eventsub_classified("ignored");
            tracing::info!(
                event = "eventsub_receiver",
                status = 200,
                "delivery ignored"
            );
            ack(id)
        }
        Err(EventSubError::InvalidChallenge) => {
            two_bot_core::metrics::global().eventsub_classified("ignored");
            // Same `malformed` error class as a bad notification body, but
            // `message_type = "webhook_callback_verification"` separates
            // failed challenges in logs alone.
            refuse(
                state,
                StatusCode::BAD_REQUEST,
                "malformed",
                false,
                id,
                bounded_message_type(&message_type),
            )
        }
        Err(e) => {
            let _ = e;
            two_bot_core::metrics::global().eventsub_classified("other");
            // A future classify error may be transient. Keep it retryable
            // rather than acknowledging it as a permanent delivery outcome.
            refuse(
                state,
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                true,
                id,
                bounded_message_type(&message_type),
            )
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
        Mutex, OnceLock,
    };
    use tower::ServiceExt as _;

    impl ReceiverState {
        fn reload_secret(&self, config: &EventsubReceiverConfig) {
            *self.secret.write().expect("EventSub secret lock poisoned") = config.secret().clone();
        }
    }

    static NEXT_TEST_SECRET: AtomicU64 = AtomicU64::new(1);

    /// Receiver tests emit through the shared `refuse`/challenge callsites.
    /// `tracing-core` resolves a callsite's first registration against the
    /// registering thread's default, so a scoped capture can miss events if a
    /// sibling test registers those callsites first; the log-hygiene test
    /// therefore reads the process-wide `crate::tracing_capture::global()`.
    /// That capture is shared with every test in this binary, so each reader
    /// keeps only the text appended after its own start offset — and every
    /// router-driving test in this module serializes here, so a sibling's
    /// `eventsub delivery refused` / `challenge answered` lines cannot land in
    /// (or satisfy) another test's slice. Acquire it first, before anything
    /// that can emit through those callsites.
    static RECEIVER_LOG_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

    #[derive(Clone)]
    struct EventSubLogWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for EventSubLogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn capture_eventsub_logs() -> (Arc<Mutex<Vec<u8>>>, tracing::subscriber::DefaultGuard) {
        // Share the global capture subscriber first so callsite interest stays
        // stable when peer tests hit the same log site on other threads;
        // the local JSON writer still captures only this test's events.
        let _global = crate::tracing_capture::global();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let output = Arc::clone(&bytes);
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(move || EventSubLogWriter(Arc::clone(&output)))
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        (bytes, guard)
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
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
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
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
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
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
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
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
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
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
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

    // --- Injected go-live delivery (durable ledger glue) ---

    use std::collections::{HashMap, VecDeque};
    use two_bot_core::stream_live_store::{
        ClaimOutcome, LedgerFuture, LiveClaim, LiveDeliveryConfig, LiveLedger, LiveSendError,
        LiveSender, SendFuture,
    };

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum FakeRow {
        Started,
        Delivered,
    }

    /// Scripted in-memory ledger: first claim wins, a started/delivered row
    /// dedupes, release frees only a started row. SQL fences are pinned by
    /// the core store integration tests; here only the receiver's use of
    /// the trait outcomes matters.
    #[derive(Default)]
    struct FakeLedger {
        rows: Mutex<HashMap<String, FakeRow>>,
    }

    impl LiveLedger for FakeLedger {
        fn claim<'a>(&'a self, claim: &'a LiveClaim) -> LedgerFuture<'a, ClaimOutcome> {
            Box::pin(async move {
                let mut rows = self.rows.lock().unwrap();
                Ok(if rows.contains_key(&claim.dedup_key) {
                    ClaimOutcome::Duplicate
                } else {
                    rows.insert(claim.dedup_key.clone(), FakeRow::Started);
                    ClaimOutcome::Claimed(two_bot_core::Secret::new("fake-token".to_owned()))
                })
            })
        }

        fn mark_started<'a>(
            &'a self,
            _dedup_key: &'a str,
            _claim_token: &'a str,
        ) -> LedgerFuture<'a, bool> {
            Box::pin(async move { Ok(true) })
        }

        fn mark_delivered<'a>(
            &'a self,
            dedup_key: &'a str,
            _claim_token: &'a str,
            _message_id: &'a str,
        ) -> LedgerFuture<'a, bool> {
            Box::pin(async move {
                self.rows
                    .lock()
                    .unwrap()
                    .insert(dedup_key.to_owned(), FakeRow::Delivered);
                Ok(true)
            })
        }

        fn release_started<'a>(
            &'a self,
            dedup_key: &'a str,
            _claim_token: &'a str,
        ) -> LedgerFuture<'a, bool> {
            Box::pin(async move {
                let mut rows = self.rows.lock().unwrap();
                Ok(rows.remove(dedup_key) == Some(FakeRow::Started))
            })
        }
    }

    #[derive(Default)]
    struct FakeSender {
        calls: Mutex<Vec<String>>,
        script: Mutex<VecDeque<Result<String, LiveSendError>>>,
    }

    impl FakeSender {
        fn scripted(answers: Vec<Result<String, LiveSendError>>) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                script: Mutex::new(answers.into()),
            })
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl LiveSender for FakeSender {
        fn send_live<'a>(&'a self, _channel_id: &'a str, content: &'a str) -> SendFuture<'a> {
            Box::pin(async move {
                self.calls.lock().unwrap().push(content.to_owned());
                self.script
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("script must answer every send")
            })
        }
    }

    fn notification_body() -> &'static str {
        r#"{"subscription":{"id":"synthetic-sub-0020","status":"enabled","type":"stream.online","version":"1","condition":{"broadcaster_user_id":"123456"},"transport":{"method":"webhook","callback":"https://example.com/hooks/twitch"},"created_at":"2026-10-13T00:00:00Z"},"event":{"id":"synthetic-stream-0020","broadcaster_user_id":"123456","broadcaster_user_login":"ourgamenight","broadcaster_user_name":"OurGameNight","type":"live","started_at":"2026-10-13T20:00:00Z"}}"#
    }

    fn delivery_app(sender: Arc<FakeSender>, broadcasters: Vec<String>) -> Router {
        let deps = DeliveryDeps {
            ledger: Arc::new(FakeLedger::default()),
            sender,
            config: LiveDeliveryConfig {
                broadcasters,
                invite_url: "https://discord.gg/example".to_owned(),
                channel_id: "777".to_owned(),
            },
        };
        router(Arc::new(ReceiverState::with_delivery(
            &config(),
            Some(deps),
        )))
    }

    async fn post_notification(app: Router, message_id: &str) -> axum::response::Response {
        let body = notification_body();
        let timestamp = fresh_timestamp();
        let signature = sign(message_id, &timestamp, body.as_bytes());
        app.oneshot(request(
            "POST",
            EVENTSUB_PATH,
            Some(message_id),
            Some(&timestamp),
            Some(&signature),
            Some("notification"),
            body,
        ))
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn notification_posts_once_and_dedupes_the_retry() {
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
        let sender = FakeSender::scripted(vec![Ok("msg-1".to_owned())]);
        let app = delivery_app(Arc::clone(&sender), vec!["123456".to_owned()]);
        let first = post_notification(app.clone(), "synthetic-delivery-0020").await;
        assert_eq!(first.status(), StatusCode::OK);
        let retry = post_notification(app, "synthetic-delivery-0021").await;
        assert_eq!(retry.status(), StatusCode::OK);
        let calls = sender.calls();
        assert_eq!(calls.len(), 1, "the Twitch retry must not double-post");
        assert!(calls[0].contains("https://www.twitch.tv/ourgamenight"));
    }

    #[tokio::test]
    async fn notification_proven_no_send_is_retryable_then_posts() {
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
        let sender = FakeSender::scripted(vec![
            Err(LiveSendError::ProvenNoSend),
            Ok("msg-2".to_owned()),
        ]);
        let app = delivery_app(Arc::clone(&sender), vec!["123456".to_owned()]);
        let first = post_notification(app.clone(), "synthetic-delivery-0022").await;
        assert_eq!(first.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = axum::body::to_bytes(first.into_body(), 4096).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["retryable"], true);
        let retry = post_notification(app, "synthetic-delivery-0023").await;
        assert_eq!(retry.status(), StatusCode::OK);
        assert_eq!(sender.calls().len(), 2);
    }

    #[tokio::test]
    async fn notification_for_unlisted_broadcaster_never_sends() {
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
        let sender = FakeSender::scripted(vec![]);
        let app = delivery_app(Arc::clone(&sender), vec!["999".to_owned()]);
        let response = post_notification(app, "synthetic-delivery-0024").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(sender.calls().is_empty());
    }

    #[tokio::test]
    async fn notification_without_delivery_deps_still_acknowledges() {
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
        // Default bind (no injected delivery): ack-only, as before this slice.
        let response = post_notification(app(), "synthetic-delivery-0025").await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn query_string_is_refused_before_verification() {
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
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
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
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
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
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

    #[test]
    fn bounded_message_type_accepts_only_known_eventsub_types() {
        for (input, expected) in [
            (
                "webhook_callback_verification",
                "webhook_callback_verification",
            ),
            ("notification", "notification"),
            ("revocation", "revocation"),
            ("untrusted-header-value", "unknown"),
        ] {
            assert_eq!(bounded_message_type(input), expected);
        }
    }

    #[tokio::test]
    async fn malformed_challenge_is_refused() {
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
        // Empty and non-graphic challenges are `InvalidChallenge`: 400, never
        // an ack. Each body is signed exactly as sent.
        let (logs, _subscriber) = capture_eventsub_logs();
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

        let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        assert_eq!(logs.matches("\"event\":\"eventsub_receiver\"").count(), 2);
        assert_eq!(
            logs.matches("\"message_type\":\"webhook_callback_verification\"")
                .count(),
            2
        );
        assert_eq!(logs.matches("\"error_class\":\"malformed\"").count(), 2);
        assert!(!logs.contains("has space"));
        assert!(!logs.contains("synthetic-delivery-0014"));
        assert!(!logs.contains("https://example.com/hooks/twitch"));
    }

    /// Child: the real redaction probe, alone in its own process (see the
    /// parent below). Scoped JSON captures lose events when sibling tests
    /// share the binary (tracing-core resolves a callsite's first
    /// registration against the registering thread's dispatcher), so the log
    /// assertions run here and the parent only relays the exit status.
    ///
    /// Redaction proof for the receiver path: secrets riding the query
    /// string, authorization/cookie headers or the body never reach the JSON
    /// logs (which carry only scalar event/status/error_class plus the
    /// correlation id), while the honored `x-request-id` is both logged and
    /// echoed in the envelope — the join a staging curl can verify per hop.
    #[tokio::test]
    async fn refusal_logs_carry_request_id_child() {
        if std::env::var_os("EVENTSUB_REDACTION_PROBE").is_none() {
            return;
        }
        use tracing::instrument::WithSubscriber;

        let sent = two_bot_core::internal_actions::new_request_id(1_720_000_000_000, &[9u8; 16]);
        let capture = crate::logging::Capture::default();
        let request = Request::builder()
            .method("POST")
            .uri("/hooks/twitch?token=synthetic-query-secret")
            .header("content-type", "application/json")
            .header("authorization", "Bearer synthetic-auth-secret")
            .header("cookie", "session=synthetic-cookie-secret")
            .header("x-request-id", &sent)
            .header("twitch-eventsub-message-id", "synthetic-delivery-9001")
            .header("twitch-eventsub-message-timestamp", fresh_timestamp())
            .header(
                "twitch-eventsub-message-signature",
                "sha256=0000000000000000000000000000000000000000000000000000000000000000",
            )
            .header("twitch-eventsub-message-type", "notification")
            .body(Body::from(challenge_body()))
            .unwrap();
        let response = app()
            .oneshot(request)
            .with_subscriber(crate::logging::capture_subscriber(capture.clone()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["request_id"], sent);
        let text = capture.text();
        assert!(
            text.contains(sent.as_str()),
            "honored id missing from logs: {text}"
        );
        for secret in [
            "synthetic-query-secret",
            "synthetic-auth-secret",
            "synthetic-cookie-secret",
            "synthetic-challenge-0001",
        ] {
            assert!(!text.contains(secret), "sensitive value reached logs");
        }
    }

    /// Parent: re-run only the probe above in an env-cleared child. A
    /// separate process avoids process-global tracing-callsite races with
    /// the parallel suite.
    #[test]
    fn refusal_logs_carry_request_id_without_secrets() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .env_clear()
            .args([
                "eventsub_http::tests::refusal_logs_carry_request_id_child",
                "--exact",
                "--nocapture",
            ])
            .env("EVENTSUB_REDACTION_PROBE", "1")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "isolated redaction probe failed: {stderr}"
        );
        assert!(stdout.contains("running 1 test"));
        assert!(stdout.contains("refusal_logs_carry_request_id_child ... ok"));
        for marker in [
            "synthetic-query-secret",
            "synthetic-auth-secret",
            "synthetic-cookie-secret",
            "synthetic-challenge-0001",
        ] {
            assert!(!stdout.contains(marker) && !stderr.contains(marker));
        }
    }

    #[tokio::test]
    async fn busy_returns_503_with_retryable() {
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
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
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
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

    #[test]
    fn eventsub_verification_failures_only_mark_bad_signatures_tampered() {
        let metrics = two_bot_core::metrics::Metrics::default();
        record_verification_failure(&metrics, &EventSubError::BadSignature);
        for error in [
            EventSubError::InvalidTimestamp,
            EventSubError::StaleTimestamp,
            EventSubError::BodyTooLarge,
        ] {
            record_verification_failure(&metrics, &error);
        }

        let exposition = metrics.render(None);
        assert!(exposition.contains("two_bot_eventsub_classified_total{kind=\"tampered\"} 1\n"));
        assert!(exposition.contains("two_bot_eventsub_classified_total{kind=\"ignored\"} 3\n"));
    }

    #[test]
    fn eventsub_received_is_counted_once_before_verification() {
        // `two_bot_eventsub_received_total` includes tampered deliveries only
        // because `eventsub_received()` runs after the empty-body gate and
        // before `verify_delivery`. A behavioral delta test would race on the
        // process-global registry under parallel test threads, so pin the
        // position in source instead.
        let source = include_str!("eventsub_http.rs");
        let non_test = source.split_once("#[cfg(test)]").expect("test module").0;
        assert_eq!(
            non_test.matches("eventsub_received()").count(),
            1,
            "receiver must count each accepted delivery exactly once via eventsub_received()"
        );
        let empty_pos = non_test.find("if raw.is_empty()").expect("empty-body gate");
        let received_pos = non_test.find("eventsub_received()").expect("receipt call");
        let verify_pos = non_test
            .find("if let Err(error) = verify_delivery")
            .expect("verification branch");
        assert!(
            empty_pos < received_pos && received_pos < verify_pos,
            "eventsub_received() must run after the empty-body gate and before verify_delivery so tampered deliveries still count toward received_total"
        );
        assert!(
            non_test[empty_pos..verify_pos].contains("eventsub_received()"),
            "receipt call must sit in the slice between the empty-body gate and verify_delivery"
        );
    }

    #[test]
    fn eventsub_classification_literals_stay_with_their_delivery_arms() {
        let source = include_str!("eventsub_http.rs");
        let verify_failure = source
            .split_once("if let Err(error) = verify_delivery")
            .expect("verify failure branch")
            .1
            .split_once("match classify(")
            .expect("classification match")
            .0;
        assert!(verify_failure
            .contains("record_verification_failure(two_bot_core::metrics::global(), &error)"));

        let arm = |start: &str, end: &str| {
            let body = source.split_once(start).expect("delivery arm start").1;
            body.split_once(end).expect("next delivery arm").0
        };
        let assert_classification = |body: &str, expected: &str| {
            let calls = body.matches("eventsub_classified(").count();
            let expected_call = format!("eventsub_classified(\"{expected}\")");
            assert_eq!(
                calls, 1,
                "delivery arm must emit exactly one classification, found {calls}"
            );
            assert!(
                body.contains(&expected_call),
                "delivery arm must classify as {expected}"
            );
        };

        let catch_all = arm("Err(e) => {", "\n    }\n}\n\n#[cfg(test)]");
        assert!(catch_all.contains("StatusCode::SERVICE_UNAVAILABLE"));
        assert!(!catch_all.contains("ack("));
        assert!(catch_all.contains("\"unavailable\""));
        assert!(catch_all.contains("                true,"));

        for (start, end, kind) in [
            (
                "Ok(Delivery::Challenge(",
                "Ok(Delivery::Notification(",
                "challenge",
            ),
            (
                "Ok(Delivery::Notification(",
                "Ok(Delivery::Revocation",
                "notification",
            ),
            (
                "Ok(Delivery::Revocation",
                "Ok(Delivery::Ignored)",
                "revocation",
            ),
            (
                "Ok(Delivery::Ignored)",
                "Err(EventSubError::WrongSubscription) | Err(EventSubError::InvalidBody)",
                "ignored",
            ),
            (
                "Err(EventSubError::WrongSubscription) | Err(EventSubError::InvalidBody)",
                "Err(EventSubError::InvalidChallenge)",
                "ignored",
            ),
            (
                "Err(EventSubError::InvalidChallenge)",
                "Err(e) => {",
                "ignored",
            ),
            ("Err(e) => {", "\n    }\n}\n\n#[cfg(test)]", "other"),
        ] {
            assert_classification(arm(start, end), kind);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_request_times_out_with_503_and_retryable() {
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
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

    // Webhook-secret reload coverage: the no-restart rotation contract.
    //
    // Tests rotate one bound ReceiverState through its test-visible reload
    // seam, then verify deliveries through the same router. Production binds
    // the initial secret at startup; runtime configuration wiring remains a
    // separate follow-up. Secrets are runtime-generated per test, so no key
    // material appears in source or overlaps the module's signing fixture.

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

    /// Build a parsed test config carrying `secret`; this does not mutate a
    /// bound receiver. Runtime config reload wiring is covered separately.
    fn reload_config_for(secret: &str) -> EventsubReceiverConfig {
        let mut vars = std::collections::HashMap::new();
        vars.insert("TWO_EVENTSUB".to_owned(), "1".to_owned());
        vars.insert("TWO_EVENTSUB_BIND".to_owned(), "127.0.0.1:8092".to_owned());
        vars.insert("TWO_TWITCH_WEBHOOK_SECRET".to_owned(), secret.to_owned());
        EventsubReceiverConfig::from_map(&vars)
            .unwrap()
            .expect("enabled")
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

    #[tokio::test]
    async fn reloaded_secret_verifies_new_deliveries() {
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
        let old_secret = fresh_reload_secret();
        let new_secret = fresh_reload_secret();
        assert_ne!(old_secret, new_secret, "rotation must change the secret");
        let state = Arc::new(ReceiverState::new(&reload_config_for(&old_secret)));
        let bound = router(Arc::clone(&state));
        state.reload_secret(&reload_config_for(&new_secret));

        let body = challenge_body();
        let timestamp = fresh_timestamp();
        let message_id = "synthetic-reload-0001";
        let signature = sign_with(&new_secret, message_id, &timestamp, body.as_bytes());
        let response = reload_response(bound, message_id, &timestamp, &signature, body).await;
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
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
        let old_secret = fresh_reload_secret();
        let new_secret = fresh_reload_secret();
        let state = Arc::new(ReceiverState::new(&reload_config_for(&old_secret)));
        let bound = router(Arc::clone(&state));
        let body = challenge_body();
        let timestamp = fresh_timestamp();

        let before_id = "synthetic-reload-0002";
        let before_signature = sign_with(&old_secret, before_id, &timestamp, body.as_bytes());
        let before = reload_response(
            bound.clone(),
            before_id,
            &timestamp,
            &before_signature,
            body,
        )
        .await;
        assert_eq!(before.status(), StatusCode::OK);

        state.reload_secret(&reload_config_for(&new_secret));
        let old_id = "synthetic-reload-0003";
        let old_signature = sign_with(&old_secret, old_id, &timestamp, body.as_bytes());
        let stale = reload_response(bound.clone(), old_id, &timestamp, &old_signature, body).await;
        assert_eq!(stale.status(), StatusCode::UNAUTHORIZED);
        assert_ne!(
            stale.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "a stale secret is a deterministic refusal, never a 500"
        );
        let bytes = axum::body::to_bytes(stale.into_body(), 4096).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "unauthorized");
        assert_eq!(value["error"]["retryable"], false);
        assert!(!String::from_utf8_lossy(&bytes).contains("synthetic-challenge-0001"));

        let new_id = "synthetic-reload-0004";
        let new_signature = sign_with(&new_secret, new_id, &timestamp, body.as_bytes());
        let fresh = reload_response(bound, new_id, &timestamp, &new_signature, body).await;
        assert_eq!(fresh.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn in_flight_delivery_during_reload_is_deterministic() {
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
        let old_secret = fresh_reload_secret();
        let new_secret = fresh_reload_secret();
        let state = Arc::new(ReceiverState::new(&reload_config_for(&old_secret)));
        let bound = router(Arc::clone(&state));
        let body = challenge_body();
        let timestamp = fresh_timestamp();
        let old_id = "synthetic-reload-0005";
        let old_signature = sign_with(&old_secret, old_id, &timestamp, body.as_bytes());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let held_body = Body::from_stream(futures_util::stream::once(async move {
            started_tx.send(()).expect("test is waiting for body read");
            release_rx.await.expect("test releases the request body")
        }));
        let request = request_with_content_type(
            EVENTSUB_PATH,
            "application/json",
            "POST",
            TestHeaders {
                message_id: Some(old_id),
                timestamp: Some(&timestamp),
                signature: Some(&old_signature),
                message_type: Some("webhook_callback_verification"),
            },
            held_body,
        );
        let pending = tokio::spawn(bound.clone().oneshot(request));

        // The request is waiting for its body while the shared secret changes.
        started_rx
            .await
            .expect("receiver began reading request body");
        state.reload_secret(&reload_config_for(&new_secret));
        release_tx
            .send(Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(
                body.as_bytes().to_vec(),
            )))
            .expect("receiver is waiting for request body");
        let in_flight = pending.await.unwrap().unwrap();
        assert_eq!(in_flight.status(), StatusCode::UNAUTHORIZED);
        assert_ne!(
            in_flight.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "one atomic secret snapshot yields a deterministic refusal, never a 500"
        );

        let new_id = "synthetic-reload-0006";
        let new_signature = sign_with(&new_secret, new_id, &timestamp, body.as_bytes());
        let reloaded = reload_response(bound, new_id, &timestamp, &new_signature, body).await;
        assert_eq!(reloaded.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn reload_path_never_logs_or_debugs_secret_material() {
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
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
        // Exercise the reload on one bound state, and read its subsequent
        // refusal and acknowledgement events from the process-wide capture
        // (see `RECEIVER_LOG_SERIAL`). Calling `global()` here ensures the
        // process-wide dispatch is installed before the start offset; that
        // one-time install (`Dispatch::new`) rebuilds every callsite's
        // cached interest against the live dispatches, so a sibling that
        // registered these callsites before the install cannot starve this
        // test of events.
        let capture = crate::tracing_capture::global();
        let start = capture.text().len();
        let state = Arc::new(ReceiverState::new(&reload_config_for(&old_secret)));
        let bound = router(Arc::clone(&state));
        let body = challenge_body();
        state.reload_secret(&reload_config_for(&new_secret));
        let timestamp = fresh_timestamp();
        let old_id = "synthetic-reload-0006";
        let old_signature = sign_with(&old_secret, old_id, &timestamp, body.as_bytes());
        let refused =
            reload_response(bound.clone(), old_id, &timestamp, &old_signature, body).await;
        assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
        let refused_bytes = axum::body::to_bytes(refused.into_body(), 4096)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&refused_bytes).contains(&old_secret));
        assert!(!String::from_utf8_lossy(&refused_bytes).contains(&new_secret));

        let new_id = "synthetic-reload-0007";
        let new_signature = sign_with(&new_secret, new_id, &timestamp, body.as_bytes());
        let accepted = reload_response(bound, new_id, &timestamp, &new_signature, body).await;
        assert_eq!(accepted.status(), StatusCode::OK);
        let accepted_bytes = axum::body::to_bytes(accepted.into_body(), 4096)
            .await
            .unwrap();
        assert_eq!(&accepted_bytes[..], b"synthetic-challenge-0001");
        tracing::info!("reload log capture live");

        let text = capture.text().split_off(start);
        assert!(
            text.contains("reload log capture live"),
            "log capture missed events; the redaction assertions would pass vacuously"
        );
        assert!(text.contains("eventsub delivery refused"));
        assert!(text.contains("challenge answered"));
        for secret in secrets {
            assert!(
                !text.contains(secret.as_str()),
                "secret material reached the reload-path logs"
            );
        }
    }
}
