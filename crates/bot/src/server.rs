//! HTTP surface: liveness `/health`, readiness `/readyz`.
//!
//! The Container supervisor polls these; the DO keepalive (wrangler/)
//! forwards them so an idle container still renews its activity timeout.
//! `/health` is always 200 when the process answers. `/readyz` is 200 only
//! when every component reports ready, else 503 with the per-component
//! breakdown — never a bare error string.

use std::sync::Arc;

use axum::{http::StatusCode, routing::get, Json, Router};
use tokio::{net::TcpListener, sync::RwLock};
use tower_http::trace::TraceLayer;
use tracing::{instrument::WithSubscriber, Instrument};
use two_bot_core::{ComponentStatus, HealthReport};

use crate::gateway::GatewayState;

/// Shared handle the /readyz handler reads.
pub type SharedState = Arc<RwLock<GatewayState>>;

/// Build the router (split out for tests: no socket needed).
#[cfg(test)]
pub fn router(state: SharedState) -> Router {
    router_with_jobs(state, crate::jobs::statuses(&[], true))
}

pub fn router_with_jobs(state: SharedState, jobs: crate::jobs::SharedStatus) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/healthz", get(health))
        .route("/readyz", get(readyz))
        .with_state((state, jobs))
        // Internal metrics live on the same listener (Worker never proxies it).
        .merge(crate::metrics_http::router())
        .layer(TraceLayer::new_for_http().make_span_with(crate::logging::http_span))
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

#[derive(serde::Serialize)]
struct ReadinessReport {
    #[serde(flatten)]
    health: HealthReport,
    // Informational component: not included in HealthReport::ready().
    jobs: std::collections::BTreeMap<String, crate::jobs::JobStatus>,
}

async fn readyz(
    axum::extract::State((state, jobs)): axum::extract::State<(
        SharedState,
        crate::jobs::SharedStatus,
    )>,
) -> (StatusCode, Json<ReadinessReport>) {
    let gateway = *state.read().await;
    let report = HealthReport::new(vec![
        ("process".to_owned(), ComponentStatus::Ready),
        ("gateway".to_owned(), gateway.status()),
    ]);
    let code = if report.ready() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        code,
        Json(ReadinessReport {
            health: report,
            jobs: jobs.read().await.clone(),
        }),
    )
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
                shutdown.send_replace(true);
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
        Arc::new(RwLock::new(s))
    }

    #[tokio::test]
    async fn failing_jobs_are_visible_but_do_not_change_gateway_readiness() {
        for (gateway, expected) in [
            (GatewayState::Connected, StatusCode::OK),
            (GatewayState::Unconfigured, StatusCode::SERVICE_UNAVAILABLE),
        ] {
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
    }

    #[tokio::test]
    async fn readyz_is_200_when_gateway_connected() {
        let response = router(state(GatewayState::Connected))
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
