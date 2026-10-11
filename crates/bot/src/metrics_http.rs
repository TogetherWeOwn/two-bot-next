//! Internal-only route on the existing listener; the Worker never proxies it.

use std::sync::{OnceLock, RwLock};

use axum::{http::header, routing::get, Router};
use sqlx::PgPool;
use two_bot_core::metrics;

fn pool_slot() -> &'static RwLock<Option<PgPool>> {
    static POOL: OnceLock<RwLock<Option<PgPool>>> = OnceLock::new();
    POOL.get_or_init(|| RwLock::new(None))
}

pub(crate) fn register_pool(pool: PgPool) {
    *pool_slot()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(pool);
}

pub(crate) fn router() -> Router {
    Router::new().route("/metrics", get(scrape))
}

async fn scrape() -> ([(header::HeaderName, &'static str); 2], String) {
    // SQLx exposes pool bookkeeping without SQL or acquiring a connection.
    // https://docs.rs/sqlx/0.9.0/sqlx/struct.Pool.html#method.size
    let pool = pool_slot()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sample = pool.as_ref().map(|pool| {
        (
            pool.size(),
            pool.num_idle(),
            pool.options().get_max_connections(),
        )
    });
    (
        [
            (header::CONTENT_TYPE, metrics::CONTENT_TYPE),
            (header::CACHE_CONTROL, "no-store"),
        ],
        metrics::global().render(sample),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use std::sync::Arc;
    use tower::ServiceExt as _;

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
            "two_bot_gateway_checkpoint_failures_total{stage=\"pre_commit\"} ",
            "two_bot_gateway_checkpoint_failures_total{stage=\"commit\"} ",
            "two_bot_audit_delivery_halt ",
        ] {
            assert!(text.contains(sample), "missing sample {sample}");
        }
        assert!(text.ends_with('\n'));
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
