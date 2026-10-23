//! Private route on the existing listener; the Worker gates off-container scrapes.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Once, OnceLock, RwLock,
};

use axum::{http::header, routing::get, Router};
use sqlx::PgPool;
use two_bot_core::metrics;

fn pool_slot() -> &'static RwLock<Option<PgPool>> {
    static POOL: OnceLock<RwLock<Option<PgPool>>> = OnceLock::new();
    POOL.get_or_init(|| RwLock::new(None))
}

/// Rejected duplicate `register_pool` calls observed since boot. In-memory
/// only and never rendered into `/metrics` output, so repeated
/// misconfiguration costs O(1) memory and O(1) log lines.
static DUPLICATE_REGISTRATIONS: AtomicU64 = AtomicU64::new(0);
static DUPLICATE_WARNED: Once = Once::new();

fn try_install(slot: &RwLock<Option<PgPool>>, pool: PgPool) -> bool {
    let mut guard = slot
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.is_some() {
        return false;
    }
    *guard = Some(pool);
    true
}

fn install_replace(slot: &RwLock<Option<PgPool>>, pool: PgPool) {
    *slot
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(pool);
}

/// First registration wins. A second call drops its handle without
/// re-pointing scrapes, counts the rejection in memory, and emits at most
/// one `tracing::warn` per process. Returns whether the pool was installed.
/// The rejected handle is dropped, never closed: `PgPool` clones share the
/// underlying pool, so closing could kill the live pool.
fn register_into(slot: &RwLock<Option<PgPool>>, pool: PgPool) -> bool {
    if try_install(slot, pool) {
        return true;
    }
    DUPLICATE_REGISTRATIONS.fetch_add(1, Ordering::Relaxed);
    DUPLICATE_WARNED.call_once(|| {
        tracing::warn!("metrics pool already registered; keeping first pool");
    });
    false
}

pub(crate) fn register_pool(pool: PgPool) -> bool {
    register_into(pool_slot(), pool)
}

/// Explicit escape hatch for deliberate replacement (tests, future reconnect
/// flows). The sole production caller (`main.rs`) stays on `register_pool`.
/// No production caller yet; kept as the documented replacement path.
#[allow(dead_code)]
pub(crate) fn replace_pool(pool: PgPool) {
    install_replace(pool_slot(), pool);
}

/// In-memory rejection count for introspection, surfaced as the
/// top-level `duplicate_pool_registrations` field on `/ops/summary`.
/// Always present (zero on unconfigured boots), never null or absent.
pub(crate) fn duplicate_registration_count() -> u64 {
    DUPLICATE_REGISTRATIONS.load(Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) fn router() -> Router {
    router_with_guard(two_bot_discord::ratelimit_guard::process_guard())
}

pub(crate) fn router_with_guard(
    guard: Arc<two_bot_discord::ratelimit_guard::RateLimitGuard>,
) -> Router {
    Router::new()
        .route("/metrics", get(scrape))
        .layer(axum::Extension(guard))
}

/// Pool bookkeeping for scrapes and the read-only `/ops` roll-ups.
/// SQLx exposes this without SQL or acquiring a connection.
/// https://docs.rs/sqlx/0.9.0/sqlx/struct.Pool.html#method.size
/// `None` while no pool is registered (unconfigured boot).
pub(crate) fn pool_sample() -> Option<(u32, usize, u32)> {
    pool_slot()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .map(|pool| {
            (
                pool.size(),
                pool.num_idle(),
                pool.options().get_max_connections(),
            )
        })
}

async fn scrape(
    axum::Extension(guard): axum::Extension<Arc<two_bot_discord::ratelimit_guard::RateLimitGuard>>,
) -> ([(header::HeaderName, &'static str); 2], String) {
    let sample = pool_sample();
    (
        [
            (header::CONTENT_TYPE, metrics::CONTENT_TYPE),
            (header::CACHE_CONTROL, "no-store"),
        ],
        metrics::global().render_with_rest_guard(sample, guard.snapshot().into()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use std::time::Duration;
    use tower::ServiceExt as _;
    use two_bot_discord::ratelimit_guard::{GuardConfig, GuardError, RateLimitGuard};

    async fn scrape_text(app: &Router) -> String {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            metrics::CONTENT_TYPE
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        String::from_utf8(
            to_bytes(response.into_body(), 65_536)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn scrape_samples_the_injected_shared_guard_without_changing_readiness_policy() {
        let guard = Arc::new(
            RateLimitGuard::new(GuardConfig {
                invalid_request_threshold: 2,
                window: Duration::from_secs(10),
            })
            .unwrap(),
        );
        let state = crate::server::SharedState::new(
            Arc::new(tokio::sync::RwLock::new(
                crate::gateway::GatewayState::Unconfigured,
            )),
            None,
        );
        let app = crate::server::router_with_guard(
            state,
            crate::jobs::statuses(&[], true),
            Arc::clone(&guard),
        );
        let fresh = scrape_text(&app).await;
        for line in fresh
            .lines()
            .filter(|line| line.starts_with("two_bot_rest_guard_"))
        {
            assert!(line.ends_with(" 0"), "nonzero fresh guard: {line}");
        }
        guard.observe_status(403, true);
        guard.observe_status(429, true);
        assert_eq!(guard.admit(false).await, Err(GuardError::CircuitOpen));
        let open = scrape_text(&app).await;
        for sample in [
            "two_bot_rest_guard_invalid_requests_in_window 2\n",
            "two_bot_rest_guard_invalid_requests_total 2\n",
            "two_bot_rest_guard_rejected_requests_total 1\n",
            "two_bot_rest_guard_breaker_opens_total 1\n",
            "two_bot_rest_guard_breaker_open 1\n",
            "two_bot_rest_guard_token_invalid 0\n",
        ] {
            assert!(open.contains(sample), "missing {sample}");
        }
        tokio::time::advance(Duration::from_secs(10)).await;
        guard.observe_global(Some(1.0));
        let paused = scrape_text(&app).await;
        for sample in [
            "two_bot_rest_guard_invalid_requests_in_window 0\n",
            "two_bot_rest_guard_breaker_closes_total 1\n",
            "two_bot_rest_guard_breaker_open 0\n",
            "two_bot_rest_guard_global_pauses_total 1\n",
            "two_bot_rest_guard_global_paused 1\n",
            "two_bot_rest_guard_global_pause_remaining_seconds 1.25\n",
        ] {
            assert!(paused.contains(sample), "missing {sample}");
        }
        tokio::time::advance(Duration::from_millis(1250)).await;
        let cooled = scrape_text(&app).await;
        assert!(cooled.contains("two_bot_rest_guard_global_paused 0\n"));
        assert!(cooled.contains("two_bot_rest_guard_global_pause_remaining_seconds 0\n"));
        guard.observe_status(401, true);
        assert_eq!(guard.admit(true).await, Err(GuardError::TokenInvalid));
        let invalid = scrape_text(&app).await;
        assert!(invalid.contains("two_bot_rest_guard_token_invalid 1\n"));
        assert!(invalid.contains("two_bot_rest_guard_invalid_requests_total 3\n"));
        assert!(invalid.contains("two_bot_rest_guard_rejected_requests_total 2\n"));
        let ready = app
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ready.status(), StatusCode::SERVICE_UNAVAILABLE);
        let report: serde_json::Value =
            serde_json::from_slice(&to_bytes(ready.into_body(), 65_536).await.unwrap()).unwrap();
        assert!(report["components"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(["token_invalid", "down"])));
    }

    #[tokio::test]
    async fn existing_server_exposes_metrics_without_gateway_or_database() {
        let state = crate::server::SharedState::new(
            Arc::new(tokio::sync::RwLock::new(
                crate::gateway::GatewayState::Unconfigured,
            )),
            None,
        );
        let response = crate::server::router(state)
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            metrics::CONTENT_TYPE
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let text = String::from_utf8(
            to_bytes(response.into_body(), 65_536)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        for name in [
            "two_bot_gateway_latency_seconds",
            "two_bot_gateway_reconnects_total",
            "two_bot_gateway_resumes_total",
            "two_bot_gateway_disconnects_total",
            "two_bot_gateway_missed_events_total",
            "two_bot_gateway_rsvp_refused_total",
            "two_bot_gateway_events_total",
            "two_bot_handler_duration_seconds",
            "two_bot_rest_requests_total",
            "two_bot_db_pool_connections",
            "two_bot_job_last_success_timestamp_seconds",
            "two_bot_audit_delivery_halt",
        ] {
            assert!(text.contains(&format!("# TYPE {name} ")), "missing {name}");
        }
    }

    #[tokio::test]
    async fn scrape_contract_pins_wire_content_type_and_stable_labels() {
        // Pin the literal wire value, not just self-equality with the
        // constant: Prometheus scrapers match this exact exposition version.
        assert_eq!(
            metrics::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8"
        );
        let state = crate::server::SharedState::new(
            Arc::new(tokio::sync::RwLock::new(
                crate::gateway::GatewayState::Unconfigured,
            )),
            None,
        );
        let response = crate::server::router(state)
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/plain; version=0.0.4; charset=utf-8"
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let text = String::from_utf8(
            to_bytes(response.into_body(), 65_536)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        // Stable label names already emitted by crates/core/src/metrics.rs.
        // Unknown values collapse to `other`; renaming a label breaks scrapers.
        for sample in [
            "two_bot_gateway_events_total{event=\"RESUMED\"} ",
            "two_bot_gateway_events_total{event=\"other\"} ",
            "two_bot_rest_requests_total{route=\"other\",result=\"429\"} ",
            "two_bot_job_last_success_timestamp_seconds{job=\"session_checkpoint\"} ",
            "two_bot_job_last_success_timestamp_seconds{job=\"invite_snapshot\"} ",
            "two_bot_job_last_success_timestamp_seconds{job=\"other\"} ",
            "two_bot_db_pool_configured ",
            "two_bot_db_pool_idle_connections ",
            "two_bot_db_pool_max_connections ",
            "two_bot_db_errors_total{op=\"admission\"} ",
            "two_bot_db_errors_total{op=\"other\"} ",
            "two_bot_send_admissions_total{outcome=\"admitted\"} ",
            "two_bot_send_admissions_total{outcome=\"blocked\"} ",
            "two_bot_dispatch_drops_total{lane=\"messages\"} ",
            "two_bot_dispatch_drops_total{lane=\"reactions\"} ",
            "two_bot_voice_vote_kick_total{outcome=\"started\"} ",
            "two_bot_voice_vote_kick_total{outcome=\"cooldown\"} ",
            "two_bot_voice_vote_kick_total{outcome=\"initiator_limited\"} ",
            "two_bot_voice_vote_kick_total{outcome=\"connect_denied_and_disconnected\"} ",
            "two_bot_voice_vote_kick_total{outcome=\"other\"} ",
            "two_bot_voice_inbox_overflow_dropped_total ",
            "two_bot_voice_queue_overflow_dropped_total ",
            "two_bot_gateway_checkpoint_failures_total{stage=\"pre_commit\"} ",
            "two_bot_gateway_checkpoint_failures_total{stage=\"commit\"} ",
            "two_bot_drop_poison_total{guard=\"command_dispatch_guard\"} ",
            "two_bot_drop_poison_total{guard=\"schedule_run\"} ",
            "two_bot_drop_poison_total{guard=\"user_slot\"} ",
            "two_bot_drop_poison_total{guard=\"in_flight_guard\"} ",
            "two_bot_drop_poison_total{guard=\"ticket_supervisor\"} ",
            "two_bot_drop_poison_total{guard=\"response_accounting\"} ",
            "two_bot_drop_poison_total{guard=\"voice_connection_guard\"} ",
            "two_bot_eventsub_reconcile_passes_total{outcome=\"healthy\"} ",
            "two_bot_eventsub_reconcile_actions_total{action=\"create\"} ",
            "two_bot_eventsub_reconcile_errors_total{error_class=\"reconcile_unhealthy\"} ",
            "two_bot_audit_delivery_halt ",
        ] {
            assert!(text.contains(sample), "missing sample {sample}");
        }
        assert!(text.ends_with('\n'));
    }

    #[tokio::test]
    async fn double_registration_keeps_first_pool() {
        // Exercises the `register_pool` keep-first wiring through
        // `register_into` on a local slot so the global scrape slot stays
        // untouched for other tests. Distinct max-connections values identify
        // which handle survived; the rejection counter pins the duplicate
        // path that a revert to always-overwrite would drop.
        let slot = RwLock::new(None);
        let first = sqlx::postgres::PgPoolOptions::new()
            .max_connections(3)
            .connect_lazy("postgres://agent_test@agent-testdb/agent_test")
            .unwrap();
        let second = sqlx::postgres::PgPoolOptions::new()
            .max_connections(7)
            .connect_lazy("postgres://agent_test@agent-testdb/agent_test")
            .unwrap();
        let before = duplicate_registration_count();
        assert!(register_into(&slot, first));
        assert!(!register_into(&slot, second));
        assert_eq!(duplicate_registration_count(), before + 1);
        let guard = slot.read().unwrap();
        assert_eq!(guard.as_ref().unwrap().options().get_max_connections(), 3);
    }

    #[tokio::test]
    async fn explicit_replace_repoints_slot() {
        // The deliberate replacement path used by `replace_pool`.
        let slot = RwLock::new(None);
        let first = sqlx::postgres::PgPoolOptions::new()
            .max_connections(3)
            .connect_lazy("postgres://agent_test@agent-testdb/agent_test")
            .unwrap();
        let second = sqlx::postgres::PgPoolOptions::new()
            .max_connections(7)
            .connect_lazy("postgres://agent_test@agent-testdb/agent_test")
            .unwrap();
        assert!(try_install(&slot, first));
        install_replace(&slot, second);
        let guard = slot.read().unwrap();
        assert_eq!(guard.as_ref().unwrap().options().get_max_connections(), 7);
    }

    #[tokio::test]
    async fn lazy_pool_sampling_does_not_connect() {
        // No credentials or external service: connect_lazy never does I/O here.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(3)
            .connect_lazy("postgres://agent_test@agent-testdb/agent_test")
            .unwrap();
        let text = metrics::Metrics::default().render(Some((
            pool.size(),
            pool.num_idle(),
            pool.options().get_max_connections(),
        )));
        assert!(text.contains("two_bot_db_pool_connections 0\n"));
        assert!(text.contains("two_bot_db_pool_max_connections 3\n"));
        pool.close().await;
    }
}
