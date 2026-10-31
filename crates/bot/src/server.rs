//! HTTP surface: liveness `/health`, readiness `/readyz`, identity `/version`.
//!
//! The Container supervisor polls these; the DO keepalive (wrangler/)
//! forwards them so an idle container still renews its activity timeout.
//! `/health` is always 200 when the process answers. `/readyz` is 200 only
//! when every component reports ready, else 503 with the per-component
//! breakdown — never a bare error string. `/version` is always 200 with the
//! stamped build identity, so a deploy records what it serves without
//! judging readiness.

use std::sync::{atomic::AtomicBool, Arc};

use axum::{http::StatusCode, routing::get, Json, Router};
use tokio::{net::TcpListener, sync::RwLock};
use tower_http::trace::{MakeSpan, TraceLayer};
use tracing::{instrument::WithSubscriber, Instrument};
use two_bot_core::{ComponentStatus, HealthReport};

use crate::gateway::GatewayState;
use crate::gateway_failure::{FailureSlot, GatewayFailure};

/// Staging rollback-drill injection (docs/staging-rollout-gate.md): when
/// active, readiness reports the gateway `down` through the real `/readyz`
/// computation so the deploy gate fails and auto-rolls back. It activates
/// only under both exact values below — the deployment-environment marker
/// the Worker carries only in its staging vars, and the injection switch
/// that reaches the container only from a generated staging deploy config.
/// Every other combination (unset, `production`, a typo, a truthy spelling
/// other than `1`) is off, so production refuses it by construction.
pub const DEPLOY_ENV_VAR: &str = "TWO_DEPLOY_ENV";
pub const GATEWAY_FAIL_INJECTION_VAR: &str = "TWO_GATEWAY_READINESS_FAIL_INJECTION";

/// Pure fence: staging marker exactly `staging` AND injection exactly `1`.
#[must_use]
pub fn gateway_failure_injection_active(deploy_env: Option<&str>, injection: Option<&str>) -> bool {
    deploy_env == Some("staging") && injection == Some("1")
}

/// The fence read once from the process environment at boot.
#[must_use]
pub fn gateway_failure_injection_from_env() -> bool {
    let deploy_env = std::env::var(DEPLOY_ENV_VAR).ok();
    let injection = std::env::var(GATEWAY_FAIL_INJECTION_VAR).ok();
    gateway_failure_injection_active(deploy_env.as_deref(), injection.as_deref())
}

/// Readiness reflects the live database, not only successful boot.
#[derive(Clone)]
pub struct SharedState {
    pub gateway: Arc<RwLock<GatewayState>>,
    pub database: Option<sqlx::PgPool>,
    /// Why the gateway task last stopped (TOG-13044); empty until it fails.
    pub failure: FailureSlot,
    /// Verifier for the private backfill progress route: the same
    /// `TWO_INTERNAL_KEYS` ring as the internal-action receiver. Empty by
    /// default (every read refuses); production attaches `from_env()`, tests
    /// attach an explicit spec. Lives in listener state — not an `Extension`
    /// layer — so the handler stays a `State + Request` pair like the
    /// receiver's `handle`.
    pub backfill_ring: crate::ops_backfill::SharedRing,
    /// Resolved staging-drill injection (see
    /// [`gateway_failure_injection_active`]); false unless production wiring
    /// attaches the fenced env read. The gateway itself keeps running —
    /// only readiness reports it down, which is what the deploy gate reads.
    pub gateway_fail_injected: bool,
    /// Transition latch for the `/ops` event-store snapshot probe: false
    /// while the last probe succeeded, true while it fails. The `/ops`
    /// handlers log `ops_snapshot_failed` on the ok→failed edge only, so a
    /// steady outage emits one line; recovery resets it silently and re-arms
    /// the next edge. Shared by `/ops/summary` and `/ops/backfill` through
    /// the cloned state, so the two endpoints never double-log one outage.
    pub ops_snapshot_failed: Arc<AtomicBool>,
}

impl SharedState {
    pub fn new(gateway: Arc<RwLock<GatewayState>>, database: Option<sqlx::PgPool>) -> Self {
        Self {
            gateway,
            database,
            failure: FailureSlot::default(),
            backfill_ring: crate::ops_backfill::SharedRing::empty(),
            gateway_fail_injected: false,
            ops_snapshot_failed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Attach the backfill verifier without disturbing the other fields.
    pub fn with_backfill_ring(mut self, ring: crate::ops_backfill::SharedRing) -> Self {
        self.backfill_ring = ring;
        self
    }

    /// Attach the resolved drill-injection fence without disturbing the
    /// other fields. Callers pass [`gateway_failure_injection_from_env`],
    /// never a raw env value, so the staging marker cannot be skipped.
    pub fn with_gateway_fail_injection(mut self, active: bool) -> Self {
        self.gateway_fail_injected = active;
        self
    }
}

/// Build the router (split out for tests: no socket needed).
#[cfg(test)]
pub fn router(state: SharedState) -> Router {
    router_with_jobs(state, crate::jobs::statuses(&[], true))
}

pub fn router_with_jobs(state: SharedState, jobs: crate::jobs::SharedStatus) -> Router {
    router_with_guard(
        state,
        jobs,
        two_bot_discord::ratelimit_guard::process_guard(),
    )
}

pub(crate) fn router_with_guard(
    state: SharedState,
    jobs: crate::jobs::SharedStatus,
    guard: Arc<two_bot_discord::ratelimit_guard::RateLimitGuard>,
) -> Router {
    // Attach the backfill verifier before `with_state`: the handler reads
    // both pool and ring from listener state (a `State + Request` pair like
    // the internal-action receiver's `handle`), never an `Extension` layer.
    let state = state.with_backfill_ring(crate::ops_backfill::SharedRing::from_env());
    Router::new()
        .route("/health", get(health))
        .route("/healthz", get(health))
        .route("/readyz", get(readyz))
        .route("/version", get(version))
        // Backfill progress reads the same TWO_INTERNAL_KEYS ring as the
        // internal-action receiver, over its own path-bound signature. Merged
        // before `with_state` (unlike the stateless metrics route below): the
        // handler reads pool and ring from listener state.
        .merge(crate::ops_backfill::router())
        .with_state((state.clone(), jobs.clone()))
        .layer(axum::Extension(Arc::clone(&guard)))
        // Metrics and readiness sample the same guard as production executors.
        .merge(crate::metrics_http::router_with_guard(Arc::clone(&guard)))
        // Read-only operator roll-ups; the Worker gates off-container reads
        // behind the scrape bearer, as with `/metrics`.
        .merge(crate::ops_http::router_with_state(state, jobs, guard))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(RedactedHttpMakeSpan)
                .on_failure(
                    |classification: tower_http::classify::ServerErrorsFailureClass,
                     latency: std::time::Duration,
                     span: &tracing::Span| {
                        // A 503 from /readyz is routine (parked, connecting or
                        // draining, including the keepalive probes), not an
                        // error. Anything else keeps the default ERROR level.
                        // (`tracing::event!` needs a constant level, so one
                        // fixed-level event per branch.)
                        use tower_http::classify::ServerErrorsFailureClass as Class;
                        match &classification {
                            Class::StatusCode(code) if *code == StatusCode::SERVICE_UNAVAILABLE => {
                                tracing::debug!(
                                    parent: span,
                                    classification = %classification,
                                    latency_ms = latency.as_millis(),
                                    "response failed"
                                );
                            }
                            _ => {
                                tracing::error!(
                                    parent: span,
                                    classification = %classification,
                                    latency_ms = latency.as_millis(),
                                    "response failed"
                                );
                            }
                        }
                    },
                ),
        )
}

/// Routes the public listener serves; the only paths a span may record.
/// Keep in sync with `router_with_guard`, `metrics_http::router_with_guard`
/// `ops_backfill::router` and `ops_http::router_with_state`.
const TRACED_PATHS: [&str; 10] = [
    "/health",
    "/healthz",
    "/readyz",
    "/version",
    "/metrics",
    "/ops/summary",
    "/ops/gateway",
    "/ops/backfill",
    "/ops/backfill/progress",
    "/ops/audit",
];

/// Recorded for every path that is not an exact `TRACED_PATHS` entry.
const REDACTED_PATH: &str = "[REDACTED]";

/// Span factory for the public listener: method plus an allowlisted path only.
///
/// tower-http's default span records the full request URI, query string
/// included, so a misrouted `webhooks/{app}/{token}` path or a `?token=`
/// query would copy a credential into trace storage (the same `url.full`
/// finding that keeps Worker traces off). Query strings are dropped outright
/// and any path that is not one of the listener's own routes is replaced by a
/// constant marker, so encoded, mixed-case, parameterized and unknown paths
/// never reach a span. Headers are never recorded.
#[derive(Clone, Copy, Debug, Default)]
struct RedactedHttpMakeSpan;

impl<B> MakeSpan<B> for RedactedHttpMakeSpan {
    fn make_span(&mut self, request: &axum::http::Request<B>) -> tracing::Span {
        tracing::debug_span!(
            "request",
            http.request.method = %request.method(),
            http.request.path = %redacted_path(request.uri()),
            request_id = %crate::logging::incoming_request_id(request.headers()),
        )
    }
}

/// Drop the query string and return the path only when it exactly matches a
/// served route (a static string, never the supplied bytes), else the marker.
fn redacted_path(uri: &axum::http::Uri) -> &'static str {
    let path = uri.path();
    TRACED_PATHS
        .iter()
        .copied()
        .find(|route| *route == path)
        .unwrap_or(REDACTED_PATH)
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

#[derive(serde::Serialize)]
pub(crate) struct ReadinessReport {
    #[serde(flatten)]
    pub(crate) health: HealthReport,
    // Informational component: not included in HealthReport::ready().
    pub(crate) jobs: std::collections::BTreeMap<String, crate::jobs::JobStatus>,
    /// Fixed-vocabulary phase/class plus the failing step and SQLSTATE code of
    /// the last gateway task failure; absent while none has happened. Never
    /// carries error text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) gateway_failure: Option<GatewayFailure>,
    pub(crate) build_revision: &'static str,
    pub(crate) build_id: &'static str,
    pub(crate) deploy_id: &'static str,
}

/// Shared readiness computation for `/readyz` and the read-only `/ops`
/// roll-ups: same ping budget, same component breakdown, same jobs sample.
pub(crate) async fn readiness(
    state: &SharedState,
    jobs: &crate::jobs::SharedStatus,
    token_invalid: bool,
) -> (StatusCode, ReadinessReport) {
    // Sample informational jobs before the ping/gateway fence too: awaiting
    // their lock afterward could publish a readiness snapshot from before stop.
    let jobs = jobs.read().await.clone();
    let (code, Json(health)) = readiness_after_ping(&state.gateway, async {
        match &state.database {
            Some(pool) => {
                tokio::time::timeout(std::time::Duration::from_secs(2), two_bot_store::ping(pool))
                    .await
                    .unwrap_or(false)
            }
            None => false,
        }
    })
    .await;
    let (code, health) = with_injected_gateway_failure(code, health, state.gateway_fail_injected);
    let (code, health) = with_token_state(code, health, token_invalid);
    (
        code,
        ReadinessReport {
            health,
            jobs,
            gateway_failure: state.failure.get(),
            build_revision: option_env!("BOT_BUILD_REVISION").unwrap_or("unknown"),
            build_id: option_env!("BOT_BUILD_ID").unwrap_or("unknown"),
            deploy_id: option_env!("BOT_DEPLOY_ID").unwrap_or("unknown"),
        },
    )
}

async fn readyz(
    axum::extract::State((state, jobs)): axum::extract::State<(
        SharedState,
        crate::jobs::SharedStatus,
    )>,
    axum::Extension(guard): axum::Extension<Arc<two_bot_discord::ratelimit_guard::RateLimitGuard>>,
) -> (StatusCode, Json<ReadinessReport>) {
    let (code, report) = readiness(&state, &jobs, guard.snapshot().token_invalid).await;
    (code, Json(report))
}

/// Stamped build identity, always 200: deploy records read what serves
/// without judging readiness. Field names match the deploy record (`sha` is
/// the full commit, the same value `/readyz` calls `build_revision`).
#[derive(serde::Serialize)]
pub(crate) struct VersionReport {
    pub(crate) deploy_id: &'static str,
    pub(crate) sha: &'static str,
    pub(crate) build_id: &'static str,
}

async fn version() -> Json<VersionReport> {
    Json(VersionReport {
        deploy_id: option_env!("BOT_DEPLOY_ID").unwrap_or("unknown"),
        sha: option_env!("BOT_BUILD_REVISION").unwrap_or("unknown"),
        build_id: option_env!("BOT_BUILD_ID").unwrap_or("unknown"),
    })
}

/// The staging drill injection reports the real `gateway` component down:
/// same component name and status vocabulary as a parked shard, so the
/// deploy gate sees exactly the not-ready shape it already rolls back on.
/// Off (the default everywhere but a fenced staging drill deploy) the
/// report is returned untouched.
fn with_injected_gateway_failure(
    code: StatusCode,
    mut health: HealthReport,
    injected: bool,
) -> (StatusCode, HealthReport) {
    if !injected {
        return (code, health);
    }
    for (name, status) in &mut health.components {
        if name == "gateway" {
            *status = ComponentStatus::Down;
        }
    }
    (StatusCode::SERVICE_UNAVAILABLE, health)
}

/// A rejected bot token is fatal: surface it as its own readiness component.
fn with_token_state(
    code: StatusCode,
    mut health: HealthReport,
    token_invalid: bool,
) -> (StatusCode, HealthReport) {
    health.components.push((
        "token_invalid".to_owned(),
        if token_invalid {
            ComponentStatus::Down
        } else {
            ComponentStatus::Ready
        },
    ));
    let code = if token_invalid {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        code
    };
    (code, health)
}

async fn readiness_after_ping(
    gateway: &RwLock<GatewayState>,
    ping: impl std::future::Future<Output = bool>,
) -> (StatusCode, Json<HealthReport>) {
    let database_ready = ping.await;
    // Reception can stop while the database probe waits. Never publish the
    // pre-probe Connected snapshot after a drain or disconnect.
    let report = readiness_report(*gateway.read().await, database_ready);
    let code = if report.ready() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(report))
}

fn readiness_report(gateway: GatewayState, database_ready: bool) -> HealthReport {
    HealthReport::new(vec![
        ("process".to_owned(), ComponentStatus::Ready),
        ("gateway".to_owned(), gateway.status()),
        (
            "database".to_owned(),
            if database_ready {
                ComponentStatus::Ready
            } else {
                ComponentStatus::Down
            },
        ),
    ])
}

/// Bind before starting the gateway so liveness never waits for Discord.
pub async fn bind(addr: &str) -> std::io::Result<TcpListener> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(msg = "http_listening", addr, "listening");
    Ok(listener)
}

/// Serve until externally stopped or SIGTERM/SIGINT, notifying jobs before draining.
pub async fn serve(
    listener: TcpListener,
    state: SharedState,
    jobs: crate::jobs::SharedStatus,
    shutdown: tokio::sync::watch::Sender<bool>,
) -> std::io::Result<()> {
    serve_with_shutdown(listener, state, jobs, shutdown, shutdown_signal()).await
}

pub(super) async fn serve_with_shutdown(
    listener: TcpListener,
    state: SharedState,
    jobs: crate::jobs::SharedStatus,
    shutdown: tokio::sync::watch::Sender<bool>,
    signal: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let gateway = Arc::clone(&state.gateway);
    // Axum spawns the signal future: preserve both the run span and dispatcher.
    // https://docs.rs/axum/0.8.9/src/axum/serve/mod.rs.html
    let router = crate::logging::with_http_context(router_with_jobs(state, jobs));
    axum::serve(listener, router.into_make_service())
        .with_graceful_shutdown(
            async move {
                tokio::select! {
                    biased;
                    _ = shutdown_requested(shutdown.subscribe()) => {},
                    _ = signal => {},
                }
                *gateway.write().await = GatewayState::Draining;
                shutdown.send_replace(true);
                crate::shutdown::exit_on_second_signal();
            }
            .in_current_span()
            .with_current_subscriber(),
        )
        .await?;
    tracing::info!(msg = "shutdown_completed");
    Ok(())
}

/// Observe sticky cancellation, including a stop sent before subscribing or closure.
pub(crate) async fn shutdown_requested(mut shutdown: tokio::sync::watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow_and_update() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");

    tokio::select! {
        _ = term.recv() => tracing::info!(msg = "shutdown_started", signal = "SIGTERM", "SIGTERM received; draining"),
        _ = int.recv() => tracing::info!(msg = "shutdown_started", signal = "SIGINT", "SIGINT received; draining"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt as _;

    fn state(s: GatewayState) -> SharedState {
        SharedState::new(Arc::new(RwLock::new(s)), None)
    }

    /// Span-and-event recorder: the shared tracing `Capture` helper keeps
    /// events only, but the HTTP layer carries the URI in span attributes,
    /// so this regression must observe spans too.
    #[derive(Clone, Default)]
    struct SpanRecorder {
        text: Arc<std::sync::Mutex<String>>,
        next_id: Arc<std::sync::atomic::AtomicU64>,
    }

    impl SpanRecorder {
        fn text(&self) -> String {
            self.text.lock().unwrap().clone()
        }

        fn push(&self, line: String) {
            let mut text = self.text.lock().unwrap();
            text.push_str(&line);
            text.push('\n');
        }
    }

    struct RecorderVisit<'a>(&'a mut String);

    impl RecorderVisit<'_> {
        fn push(&mut self, field: &tracing::field::Field, value: String) {
            use std::fmt::Write as _;
            write!(self.0, " {}={value}", field.name()).unwrap();
        }
    }

    impl tracing::field::Visit for RecorderVisit<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.push(field, format!("{value:?}"));
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.push(field, format!("{value:?}"));
        }

        fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
            self.push(field, value.to_string());
        }

        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            self.push(field, value.to_string());
        }

        fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
            self.push(field, value.to_string());
        }

        fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
            self.push(field, value.to_string());
        }

        fn record_error(
            &mut self,
            field: &tracing::field::Field,
            value: &(dyn std::error::Error + 'static),
        ) {
            self.push(field, format!("{value}"));
        }
    }

    impl tracing::Subscriber for SpanRecorder {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, attrs: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            let id = self
                .next_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            let mut line = format!("span {}:{id}", attrs.metadata().name());
            attrs.record(&mut RecorderVisit(&mut line));
            self.push(line);
            tracing::span::Id::from_u64(id)
        }

        fn record(&self, _: &tracing::span::Id, values: &tracing::span::Record<'_>) {
            let mut line = String::from("record");
            values.record(&mut RecorderVisit(&mut line));
            self.push(line);
        }

        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let mut line = format!(
                "event {} {}",
                event.metadata().level(),
                event.metadata().target()
            );
            event.record(&mut RecorderVisit(&mut line));
            self.push(line);
        }

        fn enter(&self, _: &tracing::span::Id) {}

        fn exit(&self, _: &tracing::span::Id) {}
    }

    #[test]
    fn redacted_path_allows_only_served_routes() {
        use axum::http::Uri;
        for (raw, expected) in [
            ("/health", "/health"),
            ("/healthz", "/healthz"),
            ("/readyz", "/readyz"),
            ("/version", "/version"),
            ("/metrics", "/metrics"),
            ("/ops/summary", "/ops/summary"),
            ("/ops/gateway", "/ops/gateway"),
            ("/ops/backfill", "/ops/backfill"),
            ("/ops/backfill/progress", "/ops/backfill/progress"),
            ("/ops/summary?token=fixture-query-secret", "/ops/summary"),
            ("/readyz?token=fixture-query-secret", "/readyz"),
            ("/version?token=fixture-query-secret", "/version"),
            ("/health?key=fixture-query-secret", "/health"),
            ("/api/webhooks/1/fixture-webhook-token", "[REDACTED]"),
            ("/api/WebHooks/1/fixture-webhook-token", "[REDACTED]"),
            ("/api/%77ebhooks/1/fixture-webhook-token", "[REDACTED]"),
            ("/api/%57EBHOOKS/1/fixture-webhook-token", "[REDACTED]"),
            ("/api/webhooks%2F1%2Ffixture-webhook-token", "[REDACTED]"),
            ("/api/interactions/fixture-interaction-token", "[REDACTED]"),
            ("/Health", "[REDACTED]"),
            ("/health/", "[REDACTED]"),
            ("/health/fixture-unknown-segment", "[REDACTED]"),
            ("/%68ealth", "[REDACTED]"),
            ("/", "[REDACTED]"),
            // EventSub deliveries land on the private receiver, never the
            // public listener: the callback path must stay out of spans.
            ("/hooks/twitch", "[REDACTED]"),
            ("/hooks/twitch?token=fixture-query-secret", "[REDACTED]"),
            (
                "/fixture-unknown-path?token=fixture-query-secret",
                "[REDACTED]",
            ),
        ] {
            let uri: Uri = raw.parse().unwrap();
            assert!(
                redacted_path(&uri) == expected,
                "request path redaction failed"
            );
        }
    }

    /// Credential-shaped request paths and queries must never reach trace
    /// spans or log events through the public listener, at any level, and
    /// the listener's routing and status codes must be unchanged.
    #[test]
    fn http_trace_spans_never_carry_request_paths() {
        const SENTINELS: [&str; 6] = [
            "fixture-webhook-token",
            "fixture-interaction-token",
            "fixture-unknown-segment",
            "fixture-query-secret",
            "fixture-header-secret",
            "fixture-encoded-token",
        ];
        let cases: [(&str, StatusCode); 17] = [
            (
                "/readyz?token=fixture-query-secret",
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            ("/health?key=fixture-query-secret", StatusCode::OK),
            ("/healthz", StatusCode::OK),
            ("/version?token=fixture-query-secret", StatusCode::OK),
            // Private EventSub callback: the public listener 404s and the
            // span carries the marker, never the path.
            ("/hooks/twitch", StatusCode::NOT_FOUND),
            (
                "/api/webhooks/1/fixture-webhook-token",
                StatusCode::NOT_FOUND,
            ),
            (
                "/api/WebHooks/1/fixture-webhook-token",
                StatusCode::NOT_FOUND,
            ),
            (
                "/api/%77ebhooks/1/fixture-webhook-token",
                StatusCode::NOT_FOUND,
            ),
            (
                "/api/webhooks%2F1%2Ffixture-encoded-token",
                StatusCode::NOT_FOUND,
            ),
            (
                "/api/interactions/fixture-interaction-token/callback",
                StatusCode::NOT_FOUND,
            ),
            ("/fixture-unknown-segment", StatusCode::NOT_FOUND),
            ("/health/fixture-unknown-segment", StatusCode::NOT_FOUND),
            (
                "/nope?token=fixture-query-secret&k=fixture-encoded-token",
                StatusCode::NOT_FOUND,
            ),
            ("/Health", StatusCode::NOT_FOUND),
            ("/metrics?token=fixture-query-secret", StatusCode::OK),
            // The backfill progress route is signed: a bare read refuses without
            // leaking whether any key exists. The open `/ops/backfill`
            // roll-up stays unauthenticated (see `ops_http`).
            ("/ops/backfill/progress", StatusCode::UNAUTHORIZED),
            ("/ops/summary?token=fixture-query-secret", StatusCode::OK),
        ];
        let recorder = SpanRecorder::default();
        tracing::subscriber::with_default(recorder.clone(), || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let app = router(state(GatewayState::Unconfigured));
                for (uri, expected) in cases {
                    let response = app
                        .clone()
                        .oneshot(
                            Request::builder()
                                .uri(uri)
                                .header("authorization", "Bearer fixture-header-secret")
                                .body(Body::empty())
                                .unwrap(),
                        )
                        .await
                        .unwrap();
                    assert_eq!(response.status(), expected, "request status changed");
                    let _ = axum::body::to_bytes(response.into_body(), 65536)
                        .await
                        .unwrap();
                }
                tracing::debug!("capture remains active");
            });
        });
        let text = recorder.text();
        // A failed redaction assertion must not dump the captured data itself.
        assert!(
            text.contains("capture remains active"),
            "recorder saw no events"
        );
        assert!(text.contains("span request"), "http trace span missing");
        assert!(text.contains("http.request.method=GET"), "method missing");
        for known in [
            "/health",
            "/healthz",
            "/readyz",
            "/version",
            "/metrics",
            "/ops/summary",
        ] {
            assert!(
                text.lines().any(|line| {
                    line.split_whitespace()
                        .any(|field| field == format!("http.request.path={known}"))
                }),
                // Naming the expected route is safe: it comes from this
                // static list, never from the captured trace data.
                "known route missing: {known}"
            );
        }
        let spans = text
            .lines()
            .filter(|l| l.starts_with("span request"))
            .count();
        let marked = text
            .lines()
            .filter(|line| {
                line.split_whitespace()
                    .any(|field| field == "http.request.path=[REDACTED]")
            })
            .count();
        assert_eq!(spans, cases.len(), "one span per request");
        assert_eq!(marked, 10, "unknown paths must carry the marker");
        for secret in SENTINELS {
            assert!(!text.contains(secret), "credential reached traces");
        }
    }

    #[tokio::test]
    async fn readyz_reports_fatal_bot_token_while_health_stays_live() {
        let guard = Arc::new(
            two_bot_discord::ratelimit_guard::RateLimitGuard::new(Default::default()).unwrap(),
        );
        guard.observe_status(401, true);
        let app = router_with_guard(
            state(GatewayState::Connected),
            crate::jobs::statuses(&[], true),
            guard,
        );
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(value["components"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(["token_invalid", "down"])));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn failing_jobs_are_visible_but_do_not_change_component_readiness() {
        for gateway in [GatewayState::Connected, GatewayState::Unconfigured] {
            // This offline router fixture has no database, so both responses
            // must be 503 regardless of informational job failures.
            let baseline = router(state(gateway))
                .oneshot(
                    Request::builder()
                        .uri("/readyz")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let expected = baseline.status();
            assert_eq!(expected, StatusCode::SERVICE_UNAVAILABLE);
            let jobs = crate::jobs::statuses(&["counter"], false);
            {
                let mut statuses = jobs.write().await;
                let job = statuses.get_mut("counter").unwrap();
                job.last_start = Some(100);
                job.last_success = Some(50);
                job.last_error_class = Some(crate::jobs::ErrorClass::Timeout);
                job.consecutive_failures = 2;
            }
            let response = router_with_jobs(state(gateway), jobs)
                .oneshot(
                    Request::builder()
                        .uri("/readyz")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
            let bytes = axum::body::to_bytes(response.into_body(), 8192)
                .await
                .unwrap();
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(json["jobs"]["counter"]["last_start"], 100);
            assert_eq!(json["jobs"]["counter"]["last_success"], 50);
            assert_eq!(json["jobs"]["counter"]["last_error_class"], "timeout");
            assert_eq!(json["jobs"]["counter"]["consecutive_failures"], 2);
            assert_eq!(json["components"][0][0], "process");
            assert_eq!(
                json["build_revision"],
                option_env!("BOT_BUILD_REVISION").unwrap_or("unknown")
            );
            assert_eq!(
                json["build_id"],
                option_env!("BOT_BUILD_ID").unwrap_or("unknown")
            );
            assert_eq!(
                json["deploy_id"],
                option_env!("BOT_DEPLOY_ID").unwrap_or("unknown")
            );
            assert_eq!(
                json["components"][1],
                serde_json::json!(["gateway", gateway.status()])
            );
            assert_eq!(
                json["components"][2],
                serde_json::json!(["database", "down"])
            );
        }
    }

    #[tokio::test]
    async fn health_is_always_ok() {
        let response = router(state(GatewayState::Unconfigured))
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn version_reports_the_stamped_identity_without_judging_readiness() {
        // Unconfigured gateway, no database: /readyz would be 503, but the
        // identity endpoint still answers what this build is.
        let response = router(state(GatewayState::Unconfigured))
            .oneshot(
                Request::builder()
                    .uri("/version")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            json["deploy_id"],
            option_env!("BOT_DEPLOY_ID").unwrap_or("unknown")
        );
        assert_eq!(
            json["sha"],
            option_env!("BOT_BUILD_REVISION").unwrap_or("unknown")
        );
        assert_eq!(
            json["build_id"],
            option_env!("BOT_BUILD_ID").unwrap_or("unknown")
        );
    }

    #[tokio::test]
    async fn readyz_is_503_while_gateway_down() {
        let response = router(state(GatewayState::Unconfigured))
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = axum::body::to_bytes(response.into_body(), 8192)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            json["components"],
            serde_json::json!([
                ["process", "ready"],
                ["gateway", "down"],
                ["database", "down"],
                ["token_invalid", "ready"]
            ])
        );
    }

    #[tokio::test]
    async fn readyz_reports_the_gateway_failure_only_once_one_is_recorded() {
        use crate::gateway_failure::FailureClass;
        let shared = state(GatewayState::Armed);
        let fetch = |shared: SharedState| async move {
            let response = router(shared)
                .oneshot(
                    Request::builder()
                        .uri("/readyz")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            let bytes = axum::body::to_bytes(response.into_body(), 8192)
                .await
                .unwrap();
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
        };
        assert!(fetch(shared.clone()).await.get("gateway_failure").is_none());
        for class in FailureClass::ALL {
            shared.failure.record(GatewayFailure::durable_gateway(
                class,
                "checkpoint_load",
                "42P01",
            ));
            let json = fetch(shared.clone()).await;
            assert_eq!(
                json["gateway_failure"],
                serde_json::json!({"phase": "durable_gateway", "class": class.as_str(), "failed_step": "checkpoint_load", "error_code": "42P01"})
            );
            // The component breakdown is untouched by the new field.
            assert_eq!(
                json["components"][1],
                serde_json::json!(["gateway", "starting"])
            );
        }
    }

    #[test]
    fn report_is_ready_only_with_gateway_and_database() {
        assert!(readiness_report(GatewayState::Connected, true).ready());
        assert!(!readiness_report(GatewayState::Connected, false).ready());
        assert!(!readiness_report(GatewayState::Unconfigured, true).ready());
        assert!(!readiness_report(GatewayState::Draining, true).ready());
    }

    #[tokio::test]
    async fn readiness_probe_cannot_publish_a_pre_drain_snapshot() {
        for stopped in [GatewayState::Draining, GatewayState::Armed] {
            let gateway = RwLock::new(GatewayState::Connected);
            let (release, wait) = tokio::sync::oneshot::channel();
            let response = readiness_after_ping(&gateway, async { wait.await.unwrap() });
            futures_util::pin_mut!(response);
            assert!(futures_util::poll!(&mut response).is_pending());
            *gateway.write().await = stopped;
            release.send(true).unwrap();
            let (code, report) = response.await;
            assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
            assert!(!report.0.ready());
        }
    }

    #[test]
    fn gateway_failure_injection_fence_needs_staging_marker_and_exact_one() {
        // Off by default and on every production-shaped combination; the
        // only active row is the staging marker with the exact switch.
        for (deploy_env, injection, active) in [
            (None, None, false),
            (Some("staging"), None, false),
            (None, Some("1"), false),
            (Some("production"), Some("1"), false),
            (Some("Staging"), Some("1"), false),
            (Some("staging "), Some("1"), false),
            (Some(""), Some("1"), false),
            (Some("staging"), Some("0"), false),
            (Some("staging"), Some("true"), false),
            (Some("staging"), Some(""), false),
            (Some("staging"), Some("1 "), false),
            (Some("staging"), Some("1"), true),
        ] {
            assert_eq!(
                gateway_failure_injection_active(deploy_env, injection),
                active,
                "deploy_env={deploy_env:?} injection={injection:?}"
            );
        }
    }

    #[tokio::test]
    async fn injected_gateway_failure_reports_gateway_down_through_readyz() {
        let fetch = |shared: SharedState| async move {
            let response = router(shared)
                .oneshot(
                    Request::builder()
                        .uri("/readyz")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            let bytes = axum::body::to_bytes(response.into_body(), 8192)
                .await
                .unwrap();
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
        };
        // Connected + no injection (the default `new`): gateway ready. The
        // offline fixture has no database, so both fetches stay 503; only
        // the gateway component may move.
        let plain = fetch(state(GatewayState::Connected)).await;
        assert_eq!(
            plain["components"][1],
            serde_json::json!(["gateway", "ready"])
        );
        let injected =
            fetch(state(GatewayState::Connected).with_gateway_fail_injection(true)).await;
        assert_eq!(
            injected["components"][1],
            serde_json::json!(["gateway", "down"])
        );
        // Liveness is not readiness: /health stays 200 while injected.
        let response = router(state(GatewayState::Connected).with_gateway_fail_injection(true))
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn readyz_is_503_when_gateway_connected_without_database() {
        let response = router(state(GatewayState::Connected))
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
