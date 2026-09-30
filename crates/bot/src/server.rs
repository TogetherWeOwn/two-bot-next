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
use two_bot_core::{ComponentStatus, HealthReport};

use crate::gateway::GatewayState;

/// Readiness reflects the live database, not only successful boot.
#[derive(Clone)]
pub struct SharedState {
    pub gateway: Arc<RwLock<GatewayState>>,
    pub database: Option<sqlx::PgPool>,
}

/// Build the router (split out for tests: no socket needed).
pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/readyz", get(readyz))
        .with_state(state)
        .layer(TraceLayer::new_for_http())
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn readyz(
    axum::extract::State(state): axum::extract::State<SharedState>,
) -> (StatusCode, Json<HealthReport>) {
    let gateway = *state.gateway.read().await;
    let database_ready = match &state.database {
        Some(pool) => {
            tokio::time::timeout(std::time::Duration::from_secs(2), two_bot_store::ping(pool))
                .await
                .unwrap_or(false)
        }
        None => false,
    };
    let report = readiness_report(gateway, database_ready);
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

/// Serve until SIGTERM/SIGINT (Container stop) or a bind failure.
pub async fn serve(addr: &str, state: SharedState) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(addr, "listening");
    axum::serve(listener, router(state).into_make_service())
        .with_graceful_shutdown(shutdown_signal())
        .await
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");

    tokio::select! {
        _ = term.recv() => tracing::info!("SIGTERM received; draining"),
        _ = int.recv() => tracing::info!("SIGINT received; draining"),
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
        SharedState {
            gateway: Arc::new(RwLock::new(s)),
            database: None,
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

    #[test]
    fn report_is_ready_only_with_gateway_and_database() {
        assert!(readiness_report(GatewayState::Connected, true).ready());
        assert!(!readiness_report(GatewayState::Connected, false).ready());
        assert!(!readiness_report(GatewayState::Unconfigured, true).ready());
        assert!(!readiness_report(GatewayState::Draining, true).ready());
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
