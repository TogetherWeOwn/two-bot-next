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
pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/healthz", get(health))
        .route("/readyz", get(readyz))
        .with_state(state)
        .layer(TraceLayer::new_for_http().make_span_with(crate::logging::http_span))
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn readyz(
    axum::extract::State(state): axum::extract::State<SharedState>,
) -> (StatusCode, Json<HealthReport>) {
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
    (code, Json(report))
}

/// Bind before starting the gateway so liveness never waits for Discord.
pub async fn bind(addr: &str) -> std::io::Result<TcpListener> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(msg = "http_listening", addr, "listening");
    Ok(listener)
}

/// Serve until SIGTERM/SIGINT (Container stop).
pub async fn serve(listener: TcpListener, state: SharedState) -> std::io::Result<()> {
    serve_with_shutdown(listener, state, shutdown_signal()).await
}

pub(super) async fn serve_with_shutdown(
    listener: TcpListener,
    state: SharedState,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    // Axum spawns the signal future: preserve both the run span and dispatcher.
    // https://docs.rs/axum/0.8.9/src/axum/serve/mod.rs.html
    // https://docs.rs/tracing/0.1.44/tracing/trait.Instrument.html#method.in_current_span
    // https://docs.rs/tracing/0.1.44/tracing/instrument/trait.WithSubscriber.html#method.with_current_subscriber
    axum::serve(listener, router(state).into_make_service())
        .with_graceful_shutdown(shutdown.in_current_span().with_current_subscriber())
        .await?;
    tracing::info!(msg = "shutdown_completed");
    Ok(())
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
