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
//! `message_type` (refusals), `reason` (revocation ack) and `error_variant`
//! (classify errors) scalars: no header values, challenge, body bytes or
//! query. Refusal and classify-error warns are sampled (first, then every
//! 64th process-wide) so hostile bursts count in counters, never per-request
//! logs. The callback path never reaches a
//! trace span (`server::TRACED_PATHS` must not list it; unknown paths render
//! as `[REDACTED]`).

use std::{
    future::IntoFuture,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, OnceLock, RwLock,
    },
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
use tokio::{
    net::TcpListener,
    sync::{oneshot, watch, Mutex, Semaphore},
    task::JoinSet,
};
use tracing::Instrument;
use two_bot_core::{
    eventsub_receiver_config::EventsubReceiverConfig,
    eventsub_refresh::{refresh_before_verify, RefreshVerifier},
    stream_live_store::deliver_go_live,
    twitch_eventsub::{
        classify, eventsub_error_label, verify_delivery, Delivery, DeliveryHeaders, EventSubError,
    },
};

use crate::eventsub_delivery::{post_metric_counts, DeliveryDeps};

/// Exact callback path; mirrors `wrangler/src/eventsub.ts` (`EVENTSUB_PATH`).
pub const EVENTSUB_PATH: &str = "/hooks/twitch";
const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_REQUESTS: usize = 32;
const BODY_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Headroom inside the supervisor's HTTP drain budget: the receiver reports
/// its own deadline exhaustion before `drain_http` fires. Mirrors the
/// internal-actions receiver's `SUPERVISOR_HEADROOM`.
const SUPERVISOR_HEADROOM: Duration = Duration::from_millis(500);

struct ReceiverState {
    /// The one live signing snapshot: seeded from the boot secret, swapped
    /// by armed refresh, and shared (by [`Arc`] clone) with the reconcile
    /// loop so creates always carry the secret verified here.
    secret: Arc<RwLock<two_bot_core::twitch_eventsub::WebhookSecret>>,
    /// No-restart rotation, armed only when the Operator provisioned
    /// `TWO_EVENTSUB_RELOAD_KEY`. `None` keeps the legacy startup-secret
    /// behavior: the snapshot below never changes after bind.
    reload: Option<RefreshVerifier>,
    capacity: Arc<Semaphore>,
    /// Receiver-owned delivery children. Completed joins are reaped on the
    /// next admission; active children remain joinable through shutdown.
    children: Mutex<JoinSet<()>>,
    /// Set under the child lock at shutdown so no child can race admission.
    draining: AtomicBool,
    /// The service shutdown watch, installed when `serve` starts. Admission
    /// reads the latch itself, so a request that finishes verifying after
    /// shutdown latches is refused even before `serve` closes admission.
    shutdown: OnceLock<watch::Receiver<bool>>,
    /// Go-live delivery dependencies, injected at bind time. `None` keeps
    /// verified notifications side-effect free (acknowledge only).
    delivery: Option<Arc<DeliveryDeps>>,
}

impl Drop for ReceiverState {
    fn drop(&mut self) {
        // If the enclosing service future is cancelled at its own deadline,
        // dropping JoinSet would abort uncertain sends. Keep children detached
        // instead; their started ledger claims remain the durable fence.
        self.children.get_mut().detach_all();
    }
}

impl ReceiverState {
    fn new(config: &EventsubReceiverConfig) -> Self {
        Self::with_delivery(config, None)
    }

    fn with_delivery(config: &EventsubReceiverConfig, delivery: Option<DeliveryDeps>) -> Self {
        Self {
            // Keep one atomic secret snapshot in the state; handlers never
            // re-read the environment or observe partially replaced material.
            secret: Arc::new(RwLock::new(config.secret().clone())),
            reload: config.reload_key().cloned().map(RefreshVerifier::new),
            capacity: Arc::new(Semaphore::new(MAX_REQUESTS)),
            children: Mutex::new(JoinSet::new()),
            draining: AtomicBool::new(false),
            shutdown: OnceLock::new(),
            delivery: delivery.map(Arc::new),
        }
    }

    /// Admission is closed once `serve` drains or the shared shutdown watch
    /// has latched, whichever this request observes first.
    fn admission_closed(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
            || self
                .shutdown
                .get()
                .is_some_and(|shutdown| *shutdown.borrow())
    }
}

/// Reports a delivery child that ends without completing (panic or
/// cancellation). It drops inside the child's own instrumented span, so the
/// warning carries that delivery's `request_id` and is emitted when the
/// child fails, not when a later admission or shutdown reaps its join.
struct ChildFailureReport {
    armed: bool,
}

impl Drop for ChildFailureReport {
    fn drop(&mut self) {
        if self.armed {
            let panicked = std::thread::panicking();
            tracing::warn!(
                event = "eventsub_receiver",
                msg = "eventsub_delivery_child_failed",
                panicked = panicked,
                cancelled = !panicked,
                "delivery child failed; durable claim fences retain uncertainty"
            );
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
    /// The live signing snapshot this receiver verifies against (and swaps
    /// on armed refresh). The reconcile loop shares this handle so its
    /// creates always carry the current secret: one snapshot for both
    /// sides, never a split-brain copy.
    pub fn secret_snapshot(&self) -> Arc<RwLock<two_bot_core::twitch_eventsub::WebhookSecret>> {
        Arc::clone(&self.state.secret)
    }

    /// Shutdown budget shared by the Axum drain and the child join, armed
    /// when shutdown latches. It sits inside the supervisor's HTTP drain
    /// budget by [`SUPERVISOR_HEADROOM`], so the receiver reports (and runs
    /// its child-detach path) before the `drain_http` backstop fires —
    /// the same contract as the internal-actions receiver. Never the full
    /// service deadline: this future already runs inside the supervisor's
    /// HTTP drain, so a longer budget would be unreachable.
    fn shutdown_budget() -> Duration {
        crate::website_jobs::HTTP_DRAIN_TIMEOUT.saturating_sub(SUPERVISOR_HEADROOM)
    }

    /// Supervised alongside the public listener and the internal-actions
    /// receiver; first termination stops admission everywhere. Shutdown
    /// joins admitted children against [`Self::shutdown_budget`].
    pub async fn serve(self, shutdown: tokio::sync::watch::Receiver<bool>) -> std::io::Result<()> {
        // `serve` consumes the bound receiver, so this is the only install.
        let _ = self.state.shutdown.set(shutdown.clone());
        let server = axum::serve(self.listener, router(Arc::clone(&self.state)))
            .with_graceful_shutdown(crate::server::shutdown_requested(shutdown.clone()))
            .into_future();
        tokio::pin!(server);
        let deadline = tokio::select! {
            biased;
            _ = crate::server::shutdown_requested(shutdown.clone()) => {
                let deadline = tokio::time::Instant::now() + Self::shutdown_budget();
                Self::close_admission(&self.state).await;
                deadline
            }
            result = &mut server => {
                Self::close_admission(&self.state).await;
                let deadline = tokio::time::Instant::now() + Self::shutdown_budget();
                let joined = Self::join_children(&self.state, deadline).await;
                return match (result, joined) {
                    (Err(error), _) => Err(error),
                    (Ok(()), true) => Ok(()),
                    (Ok(()), false) => Err(std::io::Error::other("EventSub receiver shutdown deadline exceeded")),
                };
            }
        };

        let server_result = tokio::time::timeout_at(deadline, &mut server).await;
        let joined = Self::join_children(&self.state, deadline).await;
        match (server_result, joined) {
            (Ok(Ok(())), true) => Ok(()),
            (Ok(Err(error)), _) => Err(error),
            _ => {
                tracing::warn!(
                    event = "eventsub_receiver",
                    msg = "eventsub_receiver_drain_deadline",
                    "receiver shutdown deadline exhausted; unfinished delivery claims remain fenced"
                );
                Err(std::io::Error::other(
                    "EventSub receiver shutdown deadline exceeded",
                ))
            }
        }
    }

    async fn close_admission(state: &ReceiverState) {
        let _children = state.children.lock().await;
        state.draining.store(true, Ordering::SeqCst);
    }

    async fn join_children(state: &ReceiverState, deadline: tokio::time::Instant) -> bool {
        let mut children = state.children.lock().await;
        match tokio::time::timeout_at(deadline, async {
            // Each child reports its own failure (`ChildFailureReport`).
            while children.join_next().await.is_some() {}
        })
        .await
        {
            Ok(()) => true,
            Err(_) => {
                let pending = children.len();
                // Started sends have unknown external effects. Detach rather
                // than abort them; their durable started claims remain fenced.
                children.detach_all();
                tracing::warn!(
                    event = "eventsub_receiver",
                    msg = "eventsub_receiver_drain_deadline",
                    pending = pending,
                    "receiver shutdown deadline exhausted with delivery children unfinished"
                );
                false
            }
        }
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

/// Refusal and classify-error warns are hostile-traffic signal, not
/// per-request logs: the process emits the first warn, then every 64th, so a
/// forgery burst is counted in counters while the sampled warn keeps the
/// bounded scalars visible. One process-wide counter covers both `refuse`
/// and `sampled_classify_warn` so concurrent hostile paths share one budget.
static REFUSAL_WARN_SAMPLE: AtomicU64 = AtomicU64::new(0);
const REFUSAL_WARN_SAMPLE_EVERY: u64 = 64;

fn sample_every_n(counter: u64) -> bool {
    counter.is_multiple_of(REFUSAL_WARN_SAMPLE_EVERY)
}

fn sampled_refusal_warn_emits() -> bool {
    sample_every_n(REFUSAL_WARN_SAMPLE.fetch_add(1, Ordering::Relaxed))
}

/// Sampled warn for a classify error: carries the bounded `error_variant`
/// label ([`eventsub_error_label`]) plus the outcome's `error_class`, so a
/// `BadSignature` rotation mismatch separates from clock-skew/misconfig
/// signal in logs alone. Classify-error arms answer through `envelope`/`ack`
/// directly and warn only here, so each error emits at most one sampled warn.
fn sampled_classify_warn(
    status: StatusCode,
    error_class: &'static str,
    error_variant: &'static str,
    message_type: &'static str,
) {
    if sampled_refusal_warn_emits() {
        tracing::warn!(
            event = "eventsub_receiver",
            status = status.as_u16(),
            error_class = error_class,
            error_variant = error_variant,
            message_type = message_type,
            "eventsub delivery classification failed"
        );
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
    if sampled_refusal_warn_emits() {
        tracing::warn!(
            event = "eventsub_receiver",
            status = status.as_u16(),
            error_class = code,
            message_type = message_type,
            "eventsub delivery refused"
        );
    }
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
        let Ok(permit) = Arc::clone(&state.capacity).try_acquire_owned() else {
            return refuse(
                &state,
                StatusCode::SERVICE_UNAVAILABLE,
                "busy",
                true,
                &id,
                "unknown",
            );
        };
        receive_with_timeout(&state, &id, receive(&state, request, &id, permit)).await
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
    permit: tokio::sync::OwnedSemaphorePermit,
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
    let (complete_tx, complete_rx) = oneshot::channel();
    {
        let mut children = state.children.lock().await;
        if state.admission_closed() {
            return refuse(
                state,
                StatusCode::SERVICE_UNAVAILABLE,
                "busy",
                true,
                id,
                bounded_message_type("notification"),
            );
        }
        // Reap finished joins; each child reports its own failure.
        while children.try_join_next().is_some() {}
        let child_span = tracing::Span::current();
        // Armed before the block so a child dropped before its first poll
        // still reports its cancellation.
        let failure = ChildFailureReport { armed: true };
        children.spawn(
            async move {
                let mut failure = failure;
                let _permit = permit;
                let delivery = deliver_go_live(
                    task_deps.ledger.as_ref(),
                    task_deps.sender.as_ref(),
                    &task_deps.config,
                    &task_stream,
                )
                .await;
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
                let _ = complete_tx.send(delivery);
                failure.armed = false;
            }
            .instrument(child_span),
        );
    }
    let delivery = match complete_rx.await {
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

async fn receive(
    state: &Arc<ReceiverState>,
    request: Request,
    id: &str,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Response {
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
            handle_notification(state, &stream, id, permit).await
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
        // not retry what will never succeed. A refused duplicate-key body is
        // permanent too: the retry carries the same bytes. Unknown/future
        // classification errors remain retryable through the fallback below.
        // Every error arm records its variant and shares the sampled warn budget.
        Err(e @ (EventSubError::WrongSubscription | EventSubError::InvalidBody))
        | Err(e @ EventSubError::DuplicateKeys) => {
            let variant = eventsub_error_label(&e);
            two_bot_core::metrics::global().eventsub_classified(variant);
            sampled_classify_warn(
                StatusCode::OK,
                "acknowledged",
                variant,
                bounded_message_type(&message_type),
            );
            ack(id)
        }
        Err(e @ EventSubError::InvalidChallenge) => {
            let variant = eventsub_error_label(&e);
            two_bot_core::metrics::global().eventsub_classified(variant);
            // Same `malformed` error class as a bad notification body, but
            // `message_type = "webhook_callback_verification"` separates
            // failed challenges in logs alone.
            sampled_classify_warn(
                StatusCode::BAD_REQUEST,
                "malformed",
                variant,
                bounded_message_type(&message_type),
            );
            envelope(StatusCode::BAD_REQUEST, "malformed", false, id)
        }
        Err(e) => {
            let variant = eventsub_error_label(&e);
            two_bot_core::metrics::global().eventsub_classified(variant);
            // A future classify error may be transient. Keep it retryable
            // rather than acknowledging it as a permanent delivery outcome.
            sampled_classify_warn(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                variant,
                bounded_message_type(&message_type),
            );
            envelope(StatusCode::SERVICE_UNAVAILABLE, "unavailable", true, id)
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
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Mutex, OnceLock,
    };
    use tower::ServiceExt as _;

    /// Protected delivery-capacity/shutdown acceptance (separate file; the
    /// Builder must not edit it). A descendant of this module, so it reuses
    /// the signing/request helpers and serializes on `RECEIVER_LOG_SERIAL`.
    mod delivery_shutdown_tests;

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

    #[tokio::test]
    async fn duplicate_key_body_is_acknowledged_without_retry() {
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
        // Start a sampling window while holding the shared receiver-test lock
        // so the redaction assertions inspect an emitted production warning.
        REFUSAL_WARN_SAMPLE.store(0, Ordering::Relaxed);
        let capture = crate::tracing_capture::global();
        let start = capture.text().len();
        // Valid signature over a notification body that repeats a key: the
        // refusal is deterministic (a retry carries the same bytes), so the
        // receiver answers 2xx and plans nothing from it.
        let body = r#"{"subscription":{"id":"synthetic-sub-0031","status":"enabled","type":"stream.online","version":"1","condition":{"broadcaster_user_id":"123456"}},"subscription":{"id":"synthetic-sub-0031","status":"enabled","type":"stream.online","version":"1","condition":{"broadcaster_user_id":"123456"}},"event":{"id":"9001","broadcaster_user_id":"123456","broadcaster_user_login":"synthetic_user","broadcaster_user_name":"Synthetic_User","type":"live","started_at":"2026-10-13T00:00:00Z"}}"#;
        let timestamp = fresh_timestamp();
        let signature = sign("synthetic-delivery-0031", &timestamp, body.as_bytes());
        let response = app()
            .oneshot(request(
                "POST",
                EVENTSUB_PATH,
                Some("synthetic-delivery-0031"),
                Some(&timestamp),
                Some(&signature),
                Some("notification"),
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
        assert!(
            value.get("error").is_none(),
            "duplicate keys must not retry"
        );
        let text = capture.text().split_off(start);
        assert_eq!(
            text.matches("eventsub delivery classification failed")
                .count(),
            1
        );
        assert!(text.contains("error_variant=\"duplicate_keys\""), "{text}");
        assert!(text.contains("error_class=\"acknowledged\""), "{text}");
        assert!(!text.contains(body));
        assert!(!text.contains(test_secret()));
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

    // --- Shutdown-path regressions (review pins; builder-owned) ---

    /// Send gate for shutdown-path regressions: every send blocks on
    /// `release` before answering, so a test can hold a real delivery child
    /// mid-send, drop or drain its owner, then release and assert settlement.
    struct BlockingSender {
        entered: AtomicUsize,
        release: Semaphore,
        calls: Mutex<Vec<String>>,
    }

    impl BlockingSender {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                entered: AtomicUsize::new(0),
                release: Semaphore::new(0),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl LiveSender for BlockingSender {
        fn send_live<'a>(&'a self, _channel_id: &'a str, content: &'a str) -> SendFuture<'a> {
            Box::pin(async move {
                self.calls.lock().unwrap().push(content.to_owned());
                self.entered.fetch_add(1, Ordering::SeqCst);
                self.release
                    .acquire()
                    .await
                    .expect("release gate stays open")
                    .forget();
                Ok("gated-msg-1".to_owned())
            })
        }
    }

    fn delivery_deps_for(ledger: Arc<FakeLedger>, sender: Arc<dyn LiveSender>) -> DeliveryDeps {
        DeliveryDeps {
            ledger,
            sender,
            config: LiveDeliveryConfig {
                broadcasters: vec!["123456".to_owned()],
                invite_url: "https://discord.gg/example".to_owned(),
                channel_id: "777".to_owned(),
            },
        }
    }

    /// One `two_bot_eventsub_posts_total` counter from the process-global
    /// registry. Callers hold `RECEIVER_LOG_SERIAL` so the delta cannot race
    /// a sibling emission.
    fn post_count(outcome: &str) -> u64 {
        let exposition = two_bot_core::metrics::global().render(None);
        let prefix = format!("two_bot_eventsub_posts_total{{outcome=\"{outcome}\"}} ");
        exposition
            .lines()
            .find_map(|line| line.strip_prefix(&prefix)?.parse::<u64>().ok())
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn draining_receiver_refuses_notification_without_starting_delivery() {
        // The shutdown admission guard: after `close_admission`, an
        // already-accepted request (e.g. a slow body that finished verifying
        // after the latch) must refuse with retryable 503 `busy` instead of
        // spawning a delivery child during the drain.
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
        let ledger = Arc::new(FakeLedger::default());
        let sender = FakeSender::scripted(vec![Ok("msg-guard-1".to_owned())]);
        let state = Arc::new(ReceiverState::with_delivery(
            &config(),
            Some(delivery_deps_for(Arc::clone(&ledger), sender.clone())),
        ));
        BoundEventsubReceiver::close_admission(&state).await;
        let response =
            post_notification(router(Arc::clone(&state)), "synthetic-drain-guard-1").await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["code"], "busy");
        assert_eq!(value["error"]["retryable"], true);
        assert!(
            ledger.rows.lock().unwrap().is_empty(),
            "a refused notification must not claim"
        );
        assert!(
            sender.calls().is_empty(),
            "a refused notification must not send"
        );
        assert_eq!(
            state.children.lock().await.len(),
            0,
            "a refused notification must not spawn a delivery child"
        );
    }

    #[tokio::test]
    async fn dropped_receiver_state_detaches_blocked_child_until_it_settles() {
        // The cancelled-serve safeguard: if the owner drops while a delivery
        // child is blocked in its send (e.g. the supervisor cancels `serve`),
        // the child must detach — not abort — so releasing the send still
        // settles exactly once with one terminal metric.
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
        let attempted_before = post_count("attempted");
        let succeeded_before = post_count("succeeded");
        let failed_before = post_count("failed");
        let ledger = Arc::new(FakeLedger::default());
        let sender = BlockingSender::new();
        let state = Arc::new(ReceiverState::with_delivery(
            &config(),
            Some(delivery_deps_for(Arc::clone(&ledger), sender.clone())),
        ));
        let in_flight = tokio::spawn(post_notification(
            router(Arc::clone(&state)),
            "synthetic-drop-detach-1",
        ));
        for _ in 0..10_000 {
            if sender.entered.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sender.entered.load(Ordering::SeqCst),
            1,
            "the delivery must block in its send before the owner drops"
        );
        // Cancel the request and wait for the cancellation to land, so the
        // request future has dropped its owner handles; the test's `Arc` is
        // then the last one and dropping it runs `ReceiverState::drop` now,
        // while the child is still blocked.
        in_flight.abort();
        let _ = in_flight.await;
        assert_eq!(
            Arc::strong_count(&state),
            1,
            "the test must hold the last owner handle so this drop runs ReceiverState::drop"
        );
        drop(state);
        sender.release.add_permits(1);
        for _ in 0..10_000 {
            if ledger
                .rows
                .lock()
                .unwrap()
                .values()
                .any(|row| *row == FakeRow::Delivered)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            ledger
                .rows
                .lock()
                .unwrap()
                .values()
                .any(|row| *row == FakeRow::Delivered),
            "the detached child must settle after release"
        );
        assert_eq!(sender.calls().len(), 1, "exactly one send attempt");
        assert_eq!(post_count("attempted"), attempted_before + 1);
        assert_eq!(post_count("succeeded"), succeeded_before + 1);
        assert_eq!(post_count("failed"), failed_before);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_budget_reports_before_the_http_drain_backstop() {
        // Composition pin: the eventsub `serve` future runs inside
        // `serve_jobs`/`drain_http`, which cancels it after
        // `HTTP_DRAIN_TIMEOUT`. The receiver's own shutdown budget sits
        // inside that drain by headroom, so a blocked child yields the
        // receiver's deadline error — never the generic drain timeout.
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
        let ledger = Arc::new(FakeLedger::default());
        let sender = BlockingSender::new();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral bind must succeed");
        let state = Arc::new(ReceiverState::with_delivery(
            &config(),
            Some(delivery_deps_for(Arc::clone(&ledger), sender.clone())),
        ));
        let bound = BoundEventsubReceiver {
            listener,
            state: Arc::clone(&state),
        };
        // One delivery through the router on the same state, held blocked in
        // its send before shutdown latches.
        let in_flight = tokio::spawn(post_notification(
            router(Arc::clone(&state)),
            "synthetic-budget-1",
        ));
        for _ in 0..10_000 {
            if sender.entered.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sender.entered.load(Ordering::SeqCst),
            1,
            "the delivery must block in its send before shutdown latches"
        );
        // Compose exactly as production does: `serve` inside `serve_jobs`.
        let (shutdown_tx, _) = tokio::sync::watch::channel(false);
        let status = crate::jobs::statuses(&[], true);
        let serve = tokio::spawn(crate::website_jobs::serve_jobs(
            Vec::new(),
            status,
            shutdown_tx.clone(),
            bound.serve(shutdown_tx.subscribe()),
        ));
        shutdown_tx.send_replace(true);
        // Advance past the receiver budget (4.5 s) but not the drain backstop
        // (5 s): the composition must already report the receiver's deadline.
        for _ in 0..48 {
            if serve.is_finished() {
                break;
            }
            tokio::time::advance(Duration::from_millis(100)).await;
            tokio::task::yield_now().await;
        }
        assert!(
            serve.is_finished(),
            "serve_jobs must report within the receiver budget, before the HTTP drain backstop"
        );
        let result = serve.await.expect("serve task");
        let error = result.expect_err("deadline exhaustion must be reported, not success");
        assert_eq!(
            error.to_string(),
            "EventSub receiver shutdown deadline exceeded"
        );
        assert_eq!(sender.calls().len(), 1, "exactly one send attempt");
        assert!(
            ledger
                .rows
                .lock()
                .unwrap()
                .values()
                .all(|row| *row == FakeRow::Started),
            "deadline exhaustion must leave the started claim fenced"
        );
        in_flight.abort();
    }

    #[tokio::test]
    async fn latched_shutdown_refuses_notification_before_admission_closes() {
        // The shared watch latch, not only `close_admission`, closes
        // admission: a request that finishes verifying after shutdown latches
        // but before `serve` drains must not start a delivery child.
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
        let ledger = Arc::new(FakeLedger::default());
        let sender = FakeSender::scripted(vec![Ok("msg-latch-1".to_owned())]);
        let state = Arc::new(ReceiverState::with_delivery(
            &config(),
            Some(delivery_deps_for(Arc::clone(&ledger), sender.clone())),
        ));
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        assert!(state.shutdown.set(shutdown_rx).is_ok(), "first install");
        shutdown_tx.send_replace(true);
        assert!(
            !state.draining.load(Ordering::SeqCst),
            "serve has not closed admission; only the latch is set"
        );
        let response =
            post_notification(router(Arc::clone(&state)), "synthetic-latch-guard-1").await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["code"], "busy");
        assert_eq!(value["error"]["retryable"], true);
        assert!(
            ledger.rows.lock().unwrap().is_empty(),
            "a latched refusal must not claim"
        );
        assert!(sender.calls().is_empty(), "a latched refusal must not send");
        assert_eq!(state.children.lock().await.len(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_exhaustion_detaches_child_so_it_still_settles_once() {
        // The `join_children` deadline branch must detach, not abort: after
        // `serve` reports exhaustion, releasing the send still settles the
        // same child once and answers its request with one terminal metric.
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
        let attempted_before = post_count("attempted");
        let succeeded_before = post_count("succeeded");
        let failed_before = post_count("failed");
        let ledger = Arc::new(FakeLedger::default());
        let sender = BlockingSender::new();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral bind must succeed");
        let state = Arc::new(ReceiverState::with_delivery(
            &config(),
            Some(delivery_deps_for(Arc::clone(&ledger), sender.clone())),
        ));
        let bound = BoundEventsubReceiver {
            listener,
            state: Arc::clone(&state),
        };
        let in_flight = tokio::spawn(post_notification(
            router(Arc::clone(&state)),
            "synthetic-detach-1",
        ));
        for _ in 0..10_000 {
            if sender.entered.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sender.entered.load(Ordering::SeqCst),
            1,
            "the delivery must block in its send before shutdown latches"
        );
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        shutdown_tx.send_replace(true);
        let error = bound
            .serve(shutdown_rx)
            .await
            .expect_err("deadline exhaustion must be reported, not success");
        assert_eq!(
            error.to_string(),
            "EventSub receiver shutdown deadline exceeded"
        );
        assert_eq!(
            state.children.lock().await.len(),
            0,
            "the unfinished child leaves the receiver's join set detached"
        );

        sender.release.add_permits(1);
        let response = in_flight.await.expect("request task");
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "the detached child must settle and answer, not be cancelled"
        );
        assert!(ledger
            .rows
            .lock()
            .unwrap()
            .values()
            .all(|row| *row == FakeRow::Delivered));
        assert_eq!(sender.calls().len(), 1, "exactly one send attempt");
        assert_eq!(post_count("attempted"), attempted_before + 1);
        assert_eq!(post_count("succeeded"), succeeded_before + 1);
        assert_eq!(post_count("failed"), failed_before);
    }

    fn raw_notification(message_id: &str) -> String {
        let body = notification_body();
        let timestamp = fresh_timestamp();
        let signature = sign(message_id, &timestamp, body.as_bytes());
        format!(
            "POST {EVENTSUB_PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\ntwitch-eventsub-message-id: {message_id}\r\ntwitch-eventsub-message-timestamp: {timestamp}\r\ntwitch-eventsub-message-signature: {signature}\r\ntwitch-eventsub-message-type: notification\r\n\r\n{body}",
            body.len(),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn aged_http_delivery_reports_receiver_deadline_before_drain_backstop() {
        // Composition oracle through the real listener: the request is 19 s
        // old when shutdown latches, so its handler times out 1 s into the
        // drain while its child stays blocked. The child join must reuse the
        // deadline armed at the latch; a deadline renewed after the HTTP
        // drain would outlast `drain_http` and surface its generic timeout.
        use tokio::io::AsyncWriteExt as _;

        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
        let ledger = Arc::new(FakeLedger::default());
        let sender = BlockingSender::new();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral bind must succeed");
        let addr = listener.local_addr().expect("bound port");
        let state = Arc::new(ReceiverState::with_delivery(
            &config(),
            Some(delivery_deps_for(Arc::clone(&ledger), sender.clone())),
        ));
        let bound = BoundEventsubReceiver {
            listener,
            state: Arc::clone(&state),
        };
        let (shutdown_tx, _) = tokio::sync::watch::channel(false);
        let status = crate::jobs::statuses(&[], true);
        let serve = tokio::spawn(crate::website_jobs::serve_jobs(
            Vec::new(),
            status,
            shutdown_tx.clone(),
            bound.serve(shutdown_tx.subscribe()),
        ));
        let mut client = tokio::net::TcpStream::connect(addr)
            .await
            .expect("loopback connect");
        client
            .write_all(raw_notification("synthetic-aged-1").as_bytes())
            .await
            .expect("loopback write");
        for _ in 0..10_000 {
            if sender.entered.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sender.entered.load(Ordering::SeqCst),
            1,
            "the HTTP delivery must block in its send before shutdown latches"
        );

        tokio::time::advance(REQUEST_TIMEOUT - Duration::from_secs(1)).await;
        shutdown_tx.send_replace(true);
        // Past the receiver budget (4.5 s) but not the drain backstop (5 s).
        for _ in 0..48 {
            if serve.is_finished() {
                break;
            }
            tokio::time::advance(Duration::from_millis(100)).await;
            for _ in 0..50 {
                tokio::task::yield_now().await;
            }
        }
        assert!(
            serve.is_finished(),
            "serve_jobs must report within the receiver budget, before the HTTP drain backstop"
        );
        let error = serve
            .await
            .expect("serve task")
            .expect_err("deadline exhaustion must be reported, not success");
        assert_eq!(
            error.to_string(),
            "EventSub receiver shutdown deadline exceeded"
        );
        assert_eq!(sender.calls().len(), 1, "exactly one send attempt");
        assert!(
            ledger
                .rows
                .lock()
                .unwrap()
                .values()
                .all(|row| *row == FakeRow::Started),
            "deadline exhaustion must leave the started claim fenced"
        );
        drop(client);
    }

    /// Panics inside the send with a marker payload, so a test can prove
    /// the child-failure warning never carries the panic message.
    struct PanickingSender;

    const CHILD_PANIC_MARKER: &str = "synthetic-child-panic-marker";

    impl LiveSender for PanickingSender {
        fn send_live<'a>(&'a self, _channel_id: &'a str, content: &'a str) -> SendFuture<'a> {
            Box::pin(async move {
                if content.contains("twitch.tv") {
                    panic!("{CHILD_PANIC_MARKER}");
                }
                Ok("unreachable-msg".to_owned())
            })
        }
    }

    /// Child: a delivery child that panics reports one bounded warning in
    /// its own request span, before any later request or shutdown reaps it.
    #[tokio::test]
    async fn child_failure_report_carries_its_own_request_id_child() {
        if std::env::var_os("EVENTSUB_CHILD_PANIC_PROBE").is_none() {
            return;
        }
        let capture = crate::logging::Capture::default();
        let _subscriber =
            tracing::subscriber::set_default(crate::logging::capture_subscriber(capture.clone()));
        let sent = two_bot_core::internal_actions::new_request_id(1_720_000_000_000, &[7u8; 16]);
        let state = Arc::new(ReceiverState::with_delivery(
            &config(),
            Some(delivery_deps_for(
                Arc::new(FakeLedger::default()),
                Arc::new(PanickingSender),
            )),
        ));
        let message_id = "synthetic-child-panic-1";
        let body = notification_body();
        let timestamp = fresh_timestamp();
        let signature = sign(message_id, &timestamp, body.as_bytes());
        let mut panicking = request(
            "POST",
            EVENTSUB_PATH,
            Some(message_id),
            Some(&timestamp),
            Some(&signature),
            Some("notification"),
            body,
        );
        panicking
            .headers_mut()
            .insert("x-request-id", HeaderValue::from_str(&sent).unwrap());
        let response = router(Arc::clone(&state)).oneshot(panicking).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        // No second request and no shutdown: the warning is already emitted.
        let failures: Vec<serde_json::Value> = capture
            .lines()
            .into_iter()
            .filter(|line| line.to_string().contains("eventsub_delivery_child_failed"))
            .collect();
        assert_eq!(failures.len(), 1, "exactly one child-failure warning");
        let failure = &failures[0];
        assert_eq!(failure["request_id"], sent.as_str());
        assert_eq!(failure["panicked"], true);
        assert_eq!(failure["cancelled"], false);
        assert!(
            !capture.text().contains(CHILD_PANIC_MARKER),
            "the panic payload must never reach structured logs"
        );
    }

    /// Parent: re-run only the probe above in an env-cleared child process,
    /// so process-global tracing callsites cannot race the parallel suite.
    #[test]
    fn child_failure_report_carries_its_own_request_id() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .env_clear()
            .args([
                "eventsub_http::tests::child_failure_report_carries_its_own_request_id_child",
                "--exact",
                "--nocapture",
            ])
            .env("EVENTSUB_CHILD_PANIC_PROBE", "1")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "isolated child-panic probe failed: {stdout}{stderr}"
        );
        assert!(stdout.contains("running 1 test"));
        assert!(stdout.contains("child_failure_report_carries_its_own_request_id_child ... ok"));
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
            // Pin each malformed payload's log fields at the start of a
            // sampling window; the receiver-test lock excludes peer requests.
            REFUSAL_WARN_SAMPLE.store(0, Ordering::Relaxed);
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
        // Error arms record the bound variant through the total mapping (never
        // a collapsed literal) and warn once through the sampled helper.
        let assert_variant_classification = |body: &str| {
            let calls = body.matches("eventsub_classified(").count();
            assert_eq!(
                calls, 1,
                "error arm must emit exactly one classification, found {calls}"
            );
            assert!(
                body.contains("eventsub_classified(variant)"),
                "error arm must classify with its bound variant label"
            );
            assert!(
                body.contains("eventsub_error_label(&e)"),
                "error arm must map the bound error through eventsub_error_label"
            );
            assert_eq!(
                body.matches("sampled_classify_warn(").count(),
                1,
                "error arm must warn once through the sampled classify helper"
            );
        };

        let catch_all = arm("Err(e) => {", "\n    }\n}\n\n#[cfg(test)]");
        assert!(catch_all.contains("StatusCode::SERVICE_UNAVAILABLE"));
        assert!(!catch_all.contains("ack("));
        assert!(catch_all.contains("\"unavailable\""));
        assert!(
            catch_all
                .contains("envelope(StatusCode::SERVICE_UNAVAILABLE, \"unavailable\", true, id)"),
            "catch-all must answer retryable 503 unavailable"
        );

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
                "Err(e @ (EventSubError::WrongSubscription",
                "ignored",
            ),
        ] {
            assert_classification(arm(start, end), kind);
        }
        for (start, end) in [
            (
                "Err(e @ (EventSubError::WrongSubscription",
                "Err(e @ EventSubError::InvalidChallenge)",
            ),
            ("Err(e @ EventSubError::InvalidChallenge)", "Err(e) => {"),
            ("Err(e) => {", "\n    }\n}\n\n#[cfg(test)]"),
        ] {
            assert_variant_classification(arm(start, end));
        }
    }

    #[test]
    fn refusal_warn_sampling_emits_first_then_every_nth() {
        assert!(sample_every_n(0), "the first hostile request always warns");
        assert!(!sample_every_n(1));
        assert!(!sample_every_n(REFUSAL_WARN_SAMPLE_EVERY - 1));
        assert!(sample_every_n(REFUSAL_WARN_SAMPLE_EVERY));
        assert!(sample_every_n(2 * REFUSAL_WARN_SAMPLE_EVERY));
        assert_eq!(
            (0..REFUSAL_WARN_SAMPLE_EVERY * 3)
                .filter(|count| sample_every_n(*count))
                .count(),
            3,
            "a burst of 3 windows emits exactly 3 sampled warns"
        );
    }

    /// Run alone so both the production counter and tracing callsites start
    /// fresh. No sampler reset or test-only sampling path is used in the child.
    #[tokio::test]
    async fn warning_sampling_shares_one_budget_across_receiver_error_paths() {
        const TEST: &str =
            "eventsub_http::tests::warning_sampling_shares_one_budget_across_receiver_error_paths";
        const PROBE: &str = "EVENTSUB_WARNING_SAMPLING_PROBE";
        if std::env::var_os(PROBE).is_none() {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child.env_clear().args([TEST, "--exact", "--nocapture"]);
            // Keep the child's executed lines in the CI coverage report.
            if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
                child.env("LLVM_PROFILE_FILE", profile);
            }
            let output = child.env(PROBE, "1").output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "isolated warning-sampling probe failed:\n{stdout}\n{stderr}"
            );
            assert!(stdout.contains("running 1 test"));
            assert!(stdout.contains(&format!("{TEST} ... ok")));
            return;
        }

        let (logs, _subscriber) = capture_eventsub_logs();
        let bound = app();
        let timestamp = fresh_timestamp();
        let body = r#"{"subscription":null}"#;
        let mut emitted = [0_usize; 2];
        for index in 0..1000_u64 {
            // Alternate paths within a window and at each emission boundary.
            // Separate refusal/classification counters would warn too often.
            let classification = (index + index / 64) % 2 == 1;
            let message_id = format!("synthetic-sampling-{index:04}");
            let signature = if classification {
                sign(&message_id, &timestamp, body.as_bytes())
            } else {
                format!("sha256={}", "0".repeat(64))
            };
            let before = logs.lock().unwrap().len();
            let response = bound
                .clone()
                .oneshot(request(
                    "POST",
                    EVENTSUB_PATH,
                    Some(&message_id),
                    Some(&timestamp),
                    Some(&signature),
                    Some("notification"),
                    body,
                ))
                .await
                .unwrap();
            let status = if classification {
                StatusCode::OK
            } else {
                StatusCode::UNAUTHORIZED
            };
            assert_eq!(response.status(), status, "delivery {index}");
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let response: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(response["ok"], classification);
            if !classification {
                assert_eq!(response["error"]["code"], "unauthorized");
                assert_eq!(response["error"]["retryable"], false);
            }

            let output = logs.lock().unwrap()[before..].to_vec();
            if index.is_multiple_of(64) {
                let event: serde_json::Value =
                    serde_json::from_slice(&output).expect("expected exactly one sampled warning");
                assert_eq!(event["level"], "WARN");
                let fields = &event["fields"];
                assert_eq!(fields["event"], "eventsub_receiver");
                assert_eq!(fields["status"], status.as_u16());
                assert_eq!(fields["message_type"], "notification");
                if classification {
                    assert_eq!(fields["error_class"], "acknowledged");
                    assert_eq!(fields["error_variant"], "invalid_body");
                } else {
                    assert_eq!(fields["error_class"], "unauthorized");
                    assert!(fields.get("error_variant").is_none());
                }
                emitted[usize::from(classification)] += 1;
            } else {
                assert!(
                    output.is_empty(),
                    "delivery {index} must suppress its warning"
                );
            }
        }
        assert_eq!(emitted, [8, 8], "1000 mixed errors emit only 16 warnings");
        assert_eq!(REFUSAL_WARN_SAMPLE.load(Ordering::Relaxed), 1000);
        let text = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        for private in [test_secret(), body, "synthetic-sampling-"] {
            assert!(!text.contains(private), "per-delivery value reached logs");
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

    #[tokio::test]
    async fn secret_snapshot_shares_the_live_handle() {
        // The reconciler signs creates with the receiver's live snapshot, not
        // a copy: the initial value is the boot secret, and a write through
        // one handle is visible through the other. The listener binds an
        // ephemeral loopback port and is never served.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral bind must succeed");
        let config = config();
        let bound = BoundEventsubReceiver {
            listener,
            state: Arc::new(ReceiverState::new(&config)),
        };
        let first = bound.secret_snapshot();
        assert_eq!(
            *first.read().expect("snapshot must be readable"),
            *config.secret(),
            "snapshot seeds from the boot secret"
        );
        let second = bound.secret_snapshot();
        let rotated = two_bot_core::twitch_eventsub::WebhookSecret::new(fresh_reload_secret())
            .expect("rotated secret must parse");
        *first.write().expect("snapshot must be writable") = rotated.clone();
        assert_eq!(
            *second.read().expect("snapshot must be readable"),
            rotated,
            "the snapshot is shared, not copied"
        );
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
        // The receiver-test lock isolates this sampling window for the
        // refusal redaction assertions below; sampling still runs normally.
        REFUSAL_WARN_SAMPLE.store(0, Ordering::Relaxed);
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

    // --- Armed no-restart rotation: the HTTP-boundary contract ---
    //
    // Builder-owned integration coverage for the armed refresh gate in
    // `receive()`: the DO-attached refresh headers swap the snapshot before
    // Twitch verification, and any refresh failure refuses retryably without
    // verifying against the stale secret. Disabling the gate must turn these
    // red: the rotated-secret challenge would 401 against the stale snapshot,
    // and the missing-refresh delivery (valid under the current secret) would
    // answer 200. All key material is runtime-generated per test, as above;
    // request ids use the generated Crockford shape so the receiver honors
    // the id the refresh was signed against.

    /// Armed config: startup secret plus the Operator reload key.
    fn armed_config_for(secret: &str, reload_key: &str) -> EventsubReceiverConfig {
        let mut vars = std::collections::HashMap::new();
        vars.insert("TWO_EVENTSUB".to_owned(), "1".to_owned());
        vars.insert("TWO_EVENTSUB_BIND".to_owned(), "127.0.0.1:8092".to_owned());
        vars.insert("TWO_TWITCH_WEBHOOK_SECRET".to_owned(), secret.to_owned());
        vars.insert("TWO_EVENTSUB_RELOAD_KEY".to_owned(), reload_key.to_owned());
        EventsubReceiverConfig::from_map(&vars)
            .unwrap()
            .expect("enabled")
    }

    /// Generated-shape request id, so `incoming_request_id` honors the id
    /// the refresh headers were signed against.
    fn fresh_armed_request_id() -> String {
        let now_ms: u64 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time is after the Unix epoch")
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        two_bot_core::internal_actions::new_request_id(now_ms, &rand::random())
    }

    fn fresh_refresh_nonce() -> String {
        hex::encode(rand::random::<[u8; 16]>())
    }

    fn refresh_timestamp_now() -> String {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time is after the Unix epoch")
            .as_secs()
            .to_string()
    }

    /// Validly signed refresh headers binding `candidate` to `request_id`
    /// under `reload_key`: the (secret, timestamp, nonce, signature) tuple
    /// the fenced DO would attach to the delivery.
    fn signed_refresh(
        reload_key: &str,
        candidate: &str,
        timestamp: &str,
        nonce: &str,
        request_id: &str,
    ) -> (String, String, String, String) {
        let signature = two_bot_core::eventsub_refresh::sign(
            reload_key.as_bytes(),
            timestamp,
            nonce,
            request_id,
            candidate.as_bytes(),
        );
        (
            candidate.to_owned(),
            timestamp.to_owned(),
            nonce.to_owned(),
            signature,
        )
    }

    /// Challenge delivery through the armed router: Twitch headers plus the
    /// DO-attached refresh headers. `twitch` is (message id, timestamp,
    /// signature); `refresh` is `None` for the missing-refresh case.
    fn armed_challenge_request(
        request_id: &str,
        refresh: Option<(String, String, String, String)>,
        twitch: (&str, &str, &str),
    ) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri(EVENTSUB_PATH)
            .header("content-type", "application/json")
            .header("x-request-id", request_id)
            .header("twitch-eventsub-message-id", twitch.0)
            .header("twitch-eventsub-message-timestamp", twitch.1)
            .header("twitch-eventsub-message-signature", twitch.2)
            .header(
                "twitch-eventsub-message-type",
                "webhook_callback_verification",
            );
        if let Some((secret, refresh_ts, nonce, refresh_sig)) = refresh {
            builder = builder
                .header("x-two-eventsub-secret", secret)
                .header("x-two-eventsub-timestamp", refresh_ts)
                .header("x-two-eventsub-nonce", nonce)
                .header("x-two-eventsub-signature", refresh_sig);
        }
        builder.body(Body::from(challenge_body())).unwrap()
    }

    #[tokio::test]
    async fn armed_refresh_swaps_before_twitch_verification() {
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
        let old_secret = fresh_reload_secret();
        let new_secret = fresh_reload_secret();
        let reload_key = fresh_reload_secret();
        assert_ne!(old_secret, new_secret, "rotation must change the secret");
        let state = Arc::new(ReceiverState::new(&armed_config_for(
            &old_secret,
            &reload_key,
        )));
        let bound = router(Arc::clone(&state));

        // The DO attached the current secret under this delivery's request
        // id; the receiver swaps before verifying, so a challenge signed
        // with the ROTATED secret answers 200 on the first armed request.
        // With the gate disabled this would 401 against the stale snapshot.
        let request_id = fresh_armed_request_id();
        let refresh_ts = refresh_timestamp_now();
        let nonce = fresh_refresh_nonce();
        let refresh = signed_refresh(&reload_key, &new_secret, &refresh_ts, &nonce, &request_id);
        let body = challenge_body();
        let timestamp = fresh_timestamp();
        let message_id = "synthetic-armed-0001";
        let twitch_sig = sign_with(&new_secret, message_id, &timestamp, body.as_bytes());
        let response = bound
            .oneshot(armed_challenge_request(
                &request_id,
                Some(refresh),
                (message_id, &timestamp, &twitch_sig),
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
    async fn armed_refresh_failure_refuses_retryable_without_verifying() {
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
        let old_secret = fresh_reload_secret();
        let reload_key = fresh_reload_secret();
        let state = Arc::new(ReceiverState::new(&armed_config_for(
            &old_secret,
            &reload_key,
        )));
        let bound = router(Arc::clone(&state));
        let body = challenge_body();

        // Missing refresh headers, but a Twitch signature valid under the
        // current secret: armed, the gate refuses before verification. With
        // the gate disabled this would answer the challenge.
        let request_id = fresh_armed_request_id();
        let timestamp = fresh_timestamp();
        let message_id = "synthetic-armed-0002";
        let twitch_sig = sign_with(&old_secret, message_id, &timestamp, body.as_bytes());
        let missing = bound
            .clone()
            .oneshot(armed_challenge_request(
                &request_id,
                None,
                (message_id, &timestamp, &twitch_sig),
            ))
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = axum::body::to_bytes(missing.into_body(), 4096)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "unavailable");
        assert_eq!(value["error"]["retryable"], true);

        // Forged refresh (signed under the wrong key) with an otherwise
        // valid delivery: the same retryable refusal, never 200 or 401.
        let forged_id = fresh_armed_request_id();
        let forged_ts = refresh_timestamp_now();
        let wrong_key = fresh_reload_secret();
        let wrong_candidate = fresh_reload_secret();
        let forged_nonce = fresh_refresh_nonce();
        let forged = signed_refresh(
            &wrong_key,
            &wrong_candidate,
            &forged_ts,
            &forged_nonce,
            &forged_id,
        );
        let forged_message_id = "synthetic-armed-0003";
        let forged_sig = sign_with(&old_secret, forged_message_id, &timestamp, body.as_bytes());
        let forged_response = bound
            .oneshot(armed_challenge_request(
                &forged_id,
                Some(forged),
                (forged_message_id, &timestamp, &forged_sig),
            ))
            .await
            .unwrap();
        assert_eq!(forged_response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = axum::body::to_bytes(forged_response.into_body(), 4096)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "unavailable");
        assert_eq!(value["error"]["retryable"], true);
    }

    #[tokio::test]
    async fn armed_pre_refresh_secret_rejected_after_swap() {
        let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
        let old_secret = fresh_reload_secret();
        let new_secret = fresh_reload_secret();
        let reload_key = fresh_reload_secret();
        let state = Arc::new(ReceiverState::new(&armed_config_for(
            &old_secret,
            &reload_key,
        )));
        let bound = router(Arc::clone(&state));
        let body = challenge_body();

        // The first armed delivery rotates the snapshot, answering the
        // challenge signed with the new secret.
        let first_id = fresh_armed_request_id();
        let first_ts = refresh_timestamp_now();
        let first_nonce = fresh_refresh_nonce();
        let first = signed_refresh(&reload_key, &new_secret, &first_ts, &first_nonce, &first_id);
        let timestamp = fresh_timestamp();
        let first_sig = sign_with(
            &new_secret,
            "synthetic-armed-0004",
            &timestamp,
            body.as_bytes(),
        );
        let rotated = bound
            .clone()
            .oneshot(armed_challenge_request(
                &first_id,
                Some(first),
                ("synthetic-armed-0004", &timestamp, &first_sig),
            ))
            .await
            .unwrap();
        assert_eq!(rotated.status(), StatusCode::OK);

        // A later delivery signed with the PRE-refresh secret is rejected
        // after the swap — even with its own valid refresh — proving HMAC
        // verification uses the current secret only.
        let retry_id = fresh_armed_request_id();
        let retry_ts = refresh_timestamp_now();
        let retry_nonce = fresh_refresh_nonce();
        let retry = signed_refresh(&reload_key, &new_secret, &retry_ts, &retry_nonce, &retry_id);
        let retry_sig = sign_with(
            &old_secret,
            "synthetic-armed-0005",
            &timestamp,
            body.as_bytes(),
        );
        let stale = bound
            .oneshot(armed_challenge_request(
                &retry_id,
                Some(retry),
                ("synthetic-armed-0005", &timestamp, &retry_sig),
            ))
            .await
            .unwrap();
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
    }
}
