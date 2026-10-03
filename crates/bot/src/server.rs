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
use crate::gateway_failure::{FailureSlot, GatewayFailure};

/// Readiness reflects the live database, not only successful boot.
#[derive(Clone)]
pub struct SharedState {
    pub gateway: Arc<RwLock<GatewayState>>,
    pub database: Option<sqlx::PgPool>,
    /// Why the gateway task last stopped (TOG-13044); empty until it fails.
    pub failure: FailureSlot,
}

impl SharedState {
    pub fn new(gateway: Arc<RwLock<GatewayState>>, database: Option<sqlx::PgPool>) -> Self {
        Self {
            gateway,
            database,
            failure: FailureSlot::default(),
        }
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

fn router_with_guard(
    state: SharedState,
    jobs: crate::jobs::SharedStatus,
    guard: Arc<two_bot_discord::ratelimit_guard::RateLimitGuard>,
) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/healthz", get(health))
        .route("/readyz", get(readyz))
        .with_state((state, jobs))
        .layer(axum::Extension(guard))
        // Internal metrics live on the same listener (Worker never proxies it).
        .merge(crate::metrics_http::router())
        .layer(TraceLayer::new_for_http())
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
    /// Fixed-vocabulary phase/class of the last gateway task failure; absent
    /// while none has happened. Never carries error text.
    #[serde(skip_serializing_if = "Option::is_none")]
    gateway_failure: Option<GatewayFailure>,
    build_revision: &'static str,
    build_id: &'static str,
}

async fn readyz(
    axum::extract::State((state, jobs)): axum::extract::State<(
        SharedState,
        crate::jobs::SharedStatus,
    )>,
    axum::Extension(guard): axum::Extension<Arc<two_bot_discord::ratelimit_guard::RateLimitGuard>>,
) -> (StatusCode, Json<ReadinessReport>) {
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
    let (code, health) = with_token_state(code, health, guard.snapshot().token_invalid);
    (
        code,
        Json(ReadinessReport {
            health,
            jobs,
            gateway_failure: state.failure.get(),
            build_revision: option_env!("BOT_BUILD_REVISION").unwrap_or("unknown"),
            build_id: option_env!("BOT_BUILD_ID").unwrap_or("unknown"),
        }),
    )
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
    tracing::info!(addr, "listening");
    Ok(listener)
}

/// Serve until externally stopped or SIGTERM/SIGINT, notifying jobs before draining.
pub async fn serve(
    listener: TcpListener,
    state: SharedState,
    jobs: crate::jobs::SharedStatus,
    shutdown: tokio::sync::watch::Sender<bool>,
) -> std::io::Result<()> {
    let gateway = Arc::clone(&state.gateway);
    axum::serve(listener, router_with_jobs(state, jobs).into_make_service())
        .with_graceful_shutdown(async move {
            tokio::select! {
                biased;
                _ = shutdown_requested(shutdown.subscribe()) => {},
                _ = shutdown_signal() => {},
            }
            *gateway.write().await = GatewayState::Draining;
            shutdown.send_replace(true);
            crate::shutdown::exit_on_second_signal();
        })
        .await
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
        SharedState::new(Arc::new(RwLock::new(s)), None)
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
            shared
                .failure
                .record(GatewayFailure::durable_gateway(class));
            let json = fetch(shared.clone()).await;
            assert_eq!(
                json["gateway_failure"],
                serde_json::json!({"phase": "durable_gateway", "class": class.as_str()})
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
