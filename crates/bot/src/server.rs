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

/// Shared handle the /readyz handler reads.
pub type SharedState = Arc<RwLock<GatewayState>>;

/// Build the router (split out for tests: no socket needed).
pub fn router(state: SharedState) -> Router {
    router_with_guard(state, two_bot_discord::ratelimit_guard::process_guard())
}

fn router_with_guard(
    state: SharedState,
    guard: Arc<two_bot_discord::ratelimit_guard::RateLimitGuard>,
) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/readyz", get(readyz))
        .with_state(state)
        .layer(axum::Extension(guard))
        .layer(TraceLayer::new_for_http())
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn readyz(
    axum::extract::State(state): axum::extract::State<SharedState>,
    axum::Extension(guard): axum::Extension<Arc<two_bot_discord::ratelimit_guard::RateLimitGuard>>,
) -> (StatusCode, Json<HealthReport>) {
    let gateway = *state.read().await;
    let report = HealthReport::new(vec![
        ("process".to_owned(), ComponentStatus::Ready),
        ("gateway".to_owned(), gateway.status()),
        (
            "token_invalid".to_owned(),
            if guard.snapshot().token_invalid {
                ComponentStatus::Down
            } else {
                ComponentStatus::Ready
            },
        ),
    ]);
    let code = if report.ready() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(report))
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
        Arc::new(RwLock::new(s))
    }

    #[tokio::test]
    async fn readyz_reports_fatal_bot_token_while_health_stays_live() {
        let guard = Arc::new(
            two_bot_discord::ratelimit_guard::RateLimitGuard::new(Default::default()).unwrap(),
        );
        guard.observe_status(401, true);
        let app = router_with_guard(state(GatewayState::Connected), guard);
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
