//! Read-only operator roll-ups: `/ops/summary`, `/ops/gateway`, `/ops/backfill`.
//!
//! Served openly on the container listener like `/metrics`; the Worker gates
//! off-container reads behind the scrape bearer (404 when unconfigured), so
//! no per-request auth lives here. Every section degrades to an explicit
//! unavailable marker instead of failing the roll-up: a database outage must
//! not hide gateway and job health, and no error text ever reaches the body
//! (fixed `available: false`, never a message, class or row content).

use std::{collections::BTreeMap, sync::Arc};

use axum::{extract::State, http::header, routing::get, Extension, Json, Router};
use two_bot_discord::ratelimit_guard::RateLimitGuard;

use crate::gateway::GatewayState;
use crate::gateway_failure::GatewayFailure;
use crate::jobs::{JobStatus, SharedStatus};
use crate::server::{readiness, SharedState};

/// Bound for the event-store reads; mirrors the `/readyz` probe budget.
const EVENT_STORE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

pub(crate) fn router_with_state(
    state: SharedState,
    jobs: SharedStatus,
    guard: Arc<RateLimitGuard>,
) -> Router {
    Router::new()
        .route("/ops/summary", get(summary))
        .route("/ops/gateway", get(gateway))
        .route("/ops/backfill", get(backfill))
        .with_state((state, jobs))
        .layer(Extension(guard))
}

/// Fixed-vocabulary gateway phase for JSON; never the Debug string by
/// accident of a future variant rename (exhaustive match fails to compile).
fn gateway_state_name(state: GatewayState) -> &'static str {
    match state {
        GatewayState::Unconfigured => "unconfigured",
        GatewayState::Armed => "armed",
        GatewayState::Connected => "connected",
        GatewayState::Draining => "draining",
    }
}

/// Signed per-table journal lag: the table's own newest journal id minus
/// its committed watermark. `None` while the table has no journal rows or
/// no committed watermark: reconcile from the baseline, never assume zero
/// writes. Zero means fully journaled. Negative only means the watermark
/// was advanced past the table's own tip via `advance_watermark`:
/// [`record`] commits the tip and the watermark together, and tips and
/// watermarks are read in one snapshot, so concurrent writers alone cannot
/// produce a negative lag.
fn cursor_lag(tip: Option<i64>, watermark: Option<i64>) -> Option<i64> {
    tip.zip(watermark).map(|(tip, mark)| tip - mark)
}

#[derive(serde::Serialize)]
struct PoolGauges {
    size: u32,
    idle: usize,
    max: u32,
}

#[derive(serde::Serialize)]
struct TableCursor {
    table: &'static str,
    watermark: Option<i64>,
    lag: Option<i64>,
}

#[derive(serde::Serialize)]
struct EventStoreRollup {
    available: bool,
    restorable_point: Option<i64>,
    max_lag: Option<i64>,
}

#[derive(serde::Serialize)]
struct BackfillSummary {
    available: bool,
    restorable_point: Option<i64>,
    tables_total: usize,
    tables_at_point: usize,
    max_lag: Option<i64>,
}

#[derive(serde::Serialize)]
struct BackfillDetail {
    available: bool,
    restorable_point: Option<i64>,
    max_lag: Option<i64>,
    tables: Vec<TableCursor>,
}

/// Bounded event-store read behind every database-backed ops section: the
/// global restorable point, every committed per-table cursor, and every
/// table's newest journal id, all from one statement and one snapshot (see
/// [`two_bot_store::journal::snapshot`]). `None` when the database is
/// absent, unreadable, or slower than the probe budget — never an error
/// string.
struct StoreSnapshot {
    point: i64,
    cursors: BTreeMap<String, i64>,
    tips: BTreeMap<String, i64>,
}

async fn store_snapshot(pool: Option<&sqlx::PgPool>) -> Option<StoreSnapshot> {
    let pool = pool?;
    let fetch = async {
        let snapshot = two_bot_store::journal::snapshot(pool).await.ok()?;
        Some(StoreSnapshot {
            point: snapshot.point,
            cursors: snapshot.cursors,
            tips: snapshot.tips,
        })
    };
    tokio::time::timeout(EVENT_STORE_TIMEOUT, fetch)
        .await
        .unwrap_or(None)
}

fn backfill_tables(snapshot: &StoreSnapshot) -> Vec<TableCursor> {
    two_bot_store::journal::COVERED_TABLES
        .iter()
        .map(|table| {
            let watermark = snapshot.cursors.get(*table).copied();
            let tip = snapshot.tips.get(*table).copied();
            TableCursor {
                table,
                watermark,
                lag: cursor_lag(tip, watermark),
            }
        })
        .collect()
}

fn backfill_detail(snapshot: Option<StoreSnapshot>) -> BackfillDetail {
    match snapshot {
        Some(ref snapshot) => {
            let tables = backfill_tables(snapshot);
            let max_lag = tables.iter().filter_map(|row| row.lag).max();
            BackfillDetail {
                available: true,
                restorable_point: Some(snapshot.point),
                max_lag,
                tables,
            }
        }
        None => BackfillDetail {
            available: false,
            restorable_point: None,
            max_lag: None,
            tables: Vec::new(),
        },
    }
}

#[derive(serde::Serialize)]
struct Summary {
    ready: bool,
    components: Vec<(String, two_bot_core::ComponentStatus)>,
    gateway_state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    gateway_failure: Option<GatewayFailure>,
    jobs: BTreeMap<String, JobStatus>,
    pool: Option<PoolGauges>,
    event_store: EventStoreRollup,
    backfill: BackfillSummary,
    build_revision: &'static str,
    build_id: &'static str,
}

fn no_store_json<T: serde::Serialize>(
    value: T,
) -> ([(header::HeaderName, &'static str); 1], Json<T>) {
    ([(header::CACHE_CONTROL, "no-store")], Json(value))
}

async fn summary(
    State((state, jobs)): State<(SharedState, SharedStatus)>,
    Extension(guard): Extension<Arc<RateLimitGuard>>,
) -> ([(header::HeaderName, &'static str); 1], Json<Summary>) {
    let (_, report) = readiness(&state, &jobs, guard.snapshot().token_invalid).await;
    let gateway_state = gateway_state_name(*state.gateway.read().await);
    let pool =
        crate::metrics_http::pool_sample().map(|(size, idle, max)| PoolGauges { size, idle, max });
    let snapshot = store_snapshot(state.database.as_ref()).await;
    let tables = snapshot.as_ref().map(backfill_tables);
    let max_lag = tables
        .as_deref()
        .and_then(|rows| rows.iter().filter_map(|row| row.lag).max());
    let (event_store, backfill) = match (&snapshot, &tables) {
        (Some(snapshot), Some(rows)) => {
            let tables_at_point = rows.iter().filter(|row| row.lag == Some(0)).count();
            (
                EventStoreRollup {
                    available: true,
                    restorable_point: Some(snapshot.point),
                    max_lag,
                },
                BackfillSummary {
                    available: true,
                    restorable_point: Some(snapshot.point),
                    tables_total: two_bot_store::journal::COVERED_TABLES.len(),
                    tables_at_point,
                    max_lag,
                },
            )
        }
        _ => (
            EventStoreRollup {
                available: false,
                restorable_point: None,
                max_lag: None,
            },
            BackfillSummary {
                available: false,
                restorable_point: None,
                tables_total: two_bot_store::journal::COVERED_TABLES.len(),
                tables_at_point: 0,
                max_lag: None,
            },
        ),
    };
    no_store_json(Summary {
        ready: report.health.ready(),
        components: report.health.components,
        gateway_state,
        gateway_failure: report.gateway_failure,
        jobs: report.jobs,
        pool,
        event_store,
        backfill,
        build_revision: report.build_revision,
        build_id: report.build_id,
    })
}

#[derive(serde::Serialize)]
struct GatewayReport {
    state: &'static str,
    status: two_bot_core::ComponentStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure: Option<GatewayFailure>,
    reconnects: u64,
    resumes: u64,
    disconnects: u64,
    missed_events: u64,
    last_ready_unix: Option<u64>,
    last_resume_unix: Option<u64>,
    events: BTreeMap<String, u64>,
    checkpoint_failures: BTreeMap<String, u64>,
    session_checkpoint_last_success: Option<u64>,
}

async fn gateway(
    State((state, jobs)): State<(SharedState, SharedStatus)>,
) -> ([(header::HeaderName, &'static str); 1], Json<GatewayReport>) {
    let current = *state.gateway.read().await;
    let snapshot = two_bot_core::metrics::global().gateway_snapshot();
    let session_checkpoint_last_success = jobs
        .read()
        .await
        .get("session_checkpoint")
        .and_then(|job| job.last_success);
    no_store_json(GatewayReport {
        state: gateway_state_name(current),
        status: current.status(),
        failure: state.failure.get(),
        reconnects: snapshot.reconnects,
        resumes: snapshot.resumes,
        disconnects: snapshot.disconnects,
        missed_events: snapshot.missed_events,
        last_ready_unix: snapshot.last_ready_unix,
        last_resume_unix: snapshot.last_resume_unix,
        events: snapshot.events.into_iter().collect(),
        checkpoint_failures: snapshot.checkpoint_failures.into_iter().collect(),
        session_checkpoint_last_success,
    })
}

async fn backfill(
    State((state, _)): State<(SharedState, SharedStatus)>,
) -> (
    [(header::HeaderName, &'static str); 1],
    Json<BackfillDetail>,
) {
    no_store_json(backfill_detail(
        store_snapshot(state.database.as_ref()).await,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tokio::sync::RwLock;
    use tower::ServiceExt as _;

    fn state_with(gateway: GatewayState, database: Option<sqlx::PgPool>) -> SharedState {
        SharedState::new(Arc::new(RwLock::new(gateway)), database)
    }

    fn lazy_pool() -> sqlx::PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://agent_test@agent-testdb/agent_test")
            .unwrap_or_else(|_| panic!("lazy pool must build without I/O"))
    }

    async fn get(app: Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1_048_576)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    fn app(gateway: GatewayState, database: Option<sqlx::PgPool>, jobs: SharedStatus) -> Router {
        crate::server::router_with_guard(
            state_with(gateway, database),
            jobs,
            two_bot_discord::ratelimit_guard::process_guard(),
        )
    }

    #[test]
    fn gateway_state_names_are_fixed_vocabulary() {
        assert_eq!(
            gateway_state_name(GatewayState::Unconfigured),
            "unconfigured"
        );
        assert_eq!(gateway_state_name(GatewayState::Armed), "armed");
        assert_eq!(gateway_state_name(GatewayState::Connected), "connected");
        assert_eq!(gateway_state_name(GatewayState::Draining), "draining");
    }

    #[test]
    fn cursor_lag_reports_unknown_as_none_and_keeps_sign() {
        assert_eq!(cursor_lag(None, None), None);
        assert_eq!(cursor_lag(Some(10), None), None);
        assert_eq!(cursor_lag(None, Some(10)), None);
        assert_eq!(cursor_lag(Some(10), Some(10)), Some(0));
        assert_eq!(cursor_lag(Some(10), Some(4)), Some(6));
        // A watermark past the table tip is data, not an error.
        assert_eq!(cursor_lag(Some(10), Some(12)), Some(-2));
    }

    fn snapshot(point: i64, cursors: &[(&str, i64)], tips: &[(&str, i64)]) -> StoreSnapshot {
        StoreSnapshot {
            point,
            cursors: cursors
                .iter()
                .map(|(table, id)| ((*table).to_owned(), *id))
                .collect(),
            tips: tips
                .iter()
                .map(|(table, id)| ((*table).to_owned(), *id))
                .collect(),
        }
    }

    #[test]
    fn backfill_detail_covers_every_table_with_max_lag() {
        let detail = backfill_detail(Some(snapshot(
            10,
            &[("events", 7), ("members", 10)],
            &[("events", 10), ("members", 10)],
        )));
        assert!(detail.available);
        assert_eq!(detail.restorable_point, Some(10));
        assert_eq!(
            detail.tables.len(),
            two_bot_store::journal::COVERED_TABLES.len()
        );
        let events = detail
            .tables
            .iter()
            .find(|row| row.table == "events")
            .unwrap();
        assert_eq!(events.watermark, Some(7));
        assert_eq!(events.lag, Some(3));
        let unknown = detail
            .tables
            .iter()
            .find(|row| row.table == "rank_ladder")
            .unwrap();
        assert_eq!(unknown.watermark, None);
        assert_eq!(unknown.lag, None);
        assert_eq!(detail.max_lag, Some(3));
    }

    #[test]
    fn backfill_lag_ignores_other_tables_writes() {
        // `events` journaled id 1, then `members` journaled ids 2..=1001.
        // Every table is fully journaled, so every lag reads 0 even though
        // the global point is 1001: lag is tip minus watermark, never the
        // point minus the watermark.
        let detail = backfill_detail(Some(snapshot(
            1001,
            &[("events", 1), ("members", 1001)],
            &[("events", 1), ("members", 1001)],
        )));
        assert!(detail.available);
        assert_eq!(detail.restorable_point, Some(1001));
        assert_eq!(detail.max_lag, Some(0));
        assert_eq!(
            detail
                .tables
                .iter()
                .filter(|row| row.lag == Some(0))
                .count(),
            2
        );
        let events = detail
            .tables
            .iter()
            .find(|row| row.table == "events")
            .unwrap();
        assert_eq!(events.watermark, Some(1));
        assert_eq!(events.lag, Some(0));
    }

    #[test]
    fn backfill_detail_unavailable_has_no_tables_or_point() {
        let detail = backfill_detail(None);
        assert!(!detail.available);
        assert_eq!(detail.restorable_point, None);
        assert_eq!(detail.max_lag, None);
        assert!(detail.tables.is_empty());
    }

    #[tokio::test]
    async fn summary_without_database_reports_readyz_and_unavailable_sections() {
        let (status, json) = get(
            app(
                GatewayState::Unconfigured,
                None,
                crate::jobs::statuses(&[], true),
            ),
            "/ops/summary",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["ready"], false);
        assert_eq!(
            json["components"],
            serde_json::json!([
                ["process", "ready"],
                ["gateway", "down"],
                ["database", "down"],
                ["token_invalid", "ready"]
            ])
        );
        assert_eq!(json["gateway_state"], "unconfigured");
        assert!(json.get("gateway_failure").is_none());
        assert_eq!(json["pool"], serde_json::Value::Null);
        assert_eq!(json["event_store"]["available"], false);
        assert_eq!(
            json["event_store"]["restorable_point"],
            serde_json::Value::Null
        );
        assert_eq!(json["backfill"]["available"], false);
        assert_eq!(
            json["backfill"]["tables_total"],
            two_bot_store::journal::COVERED_TABLES.len() as u64
        );
        assert_eq!(json["backfill"]["tables_at_point"], 0);
        assert_eq!(
            json["build_revision"],
            option_env!("BOT_BUILD_REVISION").unwrap_or("unknown")
        );
    }

    #[tokio::test]
    async fn gateway_reports_state_counters_and_checkpoint_job() {
        let jobs = crate::jobs::statuses(&["session_checkpoint"], true);
        {
            let mut statuses = jobs.write().await;
            statuses.get_mut("session_checkpoint").unwrap().last_success = Some(1_700_000_000);
        }
        let (status, json) = get(app(GatewayState::Connected, None, jobs), "/ops/gateway").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["state"], "connected");
        assert_eq!(json["status"], "ready");
        assert!(json.get("failure").is_none());
        for key in ["reconnects", "resumes", "disconnects", "missed_events"] {
            assert!(json[key].is_u64(), "counter missing: {key}");
        }
        assert!(json["last_ready_unix"].is_null() || json["last_ready_unix"].is_u64());
        assert!(json["last_resume_unix"].is_null() || json["last_resume_unix"].is_u64());
        assert!(json["events"]["READY"].is_u64());
        assert!(json["events"]["RESUMED"].is_u64());
        assert!(json["checkpoint_failures"]["pre_commit"].is_u64());
        assert!(json["checkpoint_failures"]["commit"].is_u64());
        assert_eq!(json["session_checkpoint_last_success"], 1_700_000_000);
    }

    #[tokio::test]
    async fn gateway_reports_recorded_failure_without_error_text() {
        use crate::gateway_failure::{FailureClass, GatewayFailure};
        let shared = state_with(GatewayState::Armed, None);
        shared.failure.record(GatewayFailure::durable_gateway(
            FailureClass::GatewayRuntimeFailed,
        ));
        let (status, json) = get(
            crate::server::router_with_guard(
                shared,
                crate::jobs::statuses(&[], true),
                two_bot_discord::ratelimit_guard::process_guard(),
            ),
            "/ops/gateway",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["state"], "armed");
        assert_eq!(json["status"], "starting");
        assert_eq!(json["failure"]["phase"], "durable_gateway");
    }

    #[tokio::test]
    async fn backfill_without_database_is_unavailable() {
        let (status, json) = get(
            app(
                GatewayState::Connected,
                None,
                crate::jobs::statuses(&[], true),
            ),
            "/ops/backfill",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["available"], false);
        assert_eq!(json["tables"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn unreachable_database_degrades_to_unavailable_not_500() {
        // `connect_lazy` performs no I/O, so the queries fail and the
        // endpoints must report unavailable sections with 200, never 500.
        let pool = lazy_pool();
        for uri in ["/ops/summary", "/ops/backfill"] {
            let (status, json) = get(
                app(
                    GatewayState::Connected,
                    Some(pool.clone()),
                    crate::jobs::statuses(&[], true),
                ),
                uri,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "endpoint failed: {uri}");
            let section = if uri == "/ops/summary" {
                json["event_store"].clone()
            } else {
                json.clone()
            };
            assert_eq!(section["available"], false, "endpoint leaked: {uri}");
        }
        pool.close().await;
    }

    #[tokio::test]
    async fn ops_responses_carry_no_store_cache_control() {
        let response = app(
            GatewayState::Connected,
            None,
            crate::jobs::statuses(&[], true),
        )
        .oneshot(
            Request::builder()
                .uri("/ops/gateway")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    }
}
