//! Read-only operator roll-ups: `/ops/summary`, `/ops/gateway`, `/ops/backfill`.
//!
//! Served openly on the container listener like `/metrics`; the Worker gates
//! off-container reads behind the scrape bearer (404 when unconfigured), so
//! no per-request auth lives here. Every section degrades to an explicit
//! unavailable marker instead of failing the roll-up: a database outage must
//! not hide gateway and job health, and no error text ever reaches the body
//! (fixed `available: false`, never a message, class or row content).

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use axum::{extract::State, http::header, routing::get, Extension, Json, Router};
use two_bot_discord::ratelimit_guard::RateLimitGuard;

use crate::db_log::db_class;
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
        .route(
            "/ops/audit",
            get(audit).head(|| async { axum::http::StatusCode::METHOD_NOT_ALLOWED }),
        )
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

async fn store_snapshot(pool: Option<&sqlx::PgPool>, failed: &AtomicBool) -> Option<StoreSnapshot> {
    store_snapshot_with_budget(pool, failed, EVENT_STORE_TIMEOUT).await
}

/// [`store_snapshot`] with an injectable probe budget. Production passes
/// [`EVENT_STORE_TIMEOUT`]; tests pass zero to force the timeout edge
/// without waiting out the probe.
async fn store_snapshot_with_budget(
    pool: Option<&sqlx::PgPool>,
    failed: &AtomicBool,
    budget: std::time::Duration,
) -> Option<StoreSnapshot> {
    // No database is configured state (parked boot), not a failure: stay
    // silent and leave the latch alone, like the backfill route's own
    // no-database baseline.
    let pool = pool?;
    snapshot_probe_result(
        tokio::time::timeout(budget, two_bot_store::journal::snapshot(pool)).await,
        failed,
    )
}

/// Map the completed probe at one boundary so success, store failure and
/// timeout all update the latch before returning the unchanged roll-up data.
fn snapshot_probe_result(
    result: Result<
        Result<two_bot_store::journal::Snapshot, two_bot_store::journal::JournalError>,
        tokio::time::error::Elapsed,
    >,
    failed: &AtomicBool,
) -> Option<StoreSnapshot> {
    match result {
        Ok(Ok(snapshot)) => {
            note_snapshot_outcome(failed, SnapshotOutcome::Ok);
            Some(StoreSnapshot {
                point: snapshot.point,
                cursors: snapshot.cursors,
                tips: snapshot.tips,
            })
        }
        Ok(Err(error)) => {
            note_snapshot_outcome(
                failed,
                SnapshotOutcome::StoreFailed(snapshot_failure_class(&error)),
            );
            None
        }
        Err(_) => {
            note_snapshot_outcome(failed, SnapshotOutcome::TimedOut);
            None
        }
    }
}

/// One probe result for the transition log below: success resets the latch
/// silently, either failure kind warns on the ok→failed edge only.
#[derive(Clone, Copy)]
enum SnapshotOutcome {
    Ok,
    StoreFailed(&'static str),
    TimedOut,
}

/// Fixed token for a failed event-store snapshot read, never the error
/// value: the `db_log` class for database errors, `invalid` for a journal
/// entry that fails validation (unreachable from the store `snapshot()`
/// read, which never validates — the arm keeps the match exhaustive
/// without a panic path).
fn snapshot_failure_class(error: &two_bot_store::journal::JournalError) -> &'static str {
    match error {
        two_bot_store::journal::JournalError::Db(inner) => db_class(inner),
        two_bot_store::journal::JournalError::Invalid(_) => "invalid",
    }
}

/// Record one probe result on the shared latch: the first failure after a
/// success warns with its fixed token, steady-state failures stay silent,
/// and recovery resets the latch silently so the next edge re-arms. The
/// ordering is relaxed: the latch guards no other state, so a lost race at
/// worst duplicates one line.
fn note_snapshot_outcome(failed: &AtomicBool, outcome: SnapshotOutcome) {
    match outcome {
        SnapshotOutcome::Ok => failed.store(false, Ordering::Relaxed),
        SnapshotOutcome::StoreFailed(class) => {
            if !failed.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    msg = "ops_snapshot_failed",
                    db_class = class,
                    "event-store snapshot unreadable"
                );
            }
        }
        SnapshotOutcome::TimedOut => {
            if !failed.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    msg = "ops_snapshot_failed",
                    timed_out = true,
                    "event-store snapshot unreadable"
                );
            }
        }
    }
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
    /// Rejected duplicate `register_pool` calls since boot. Top-level (not
    /// inside `pool`) so the zero state still renders `0` on unconfigured
    /// boots where `pool` is `null`, never absent-vs-zero ambiguity.
    duplicate_pool_registrations: u64,
    event_store: EventStoreRollup,
    backfill: BackfillSummary,
    build_revision: &'static str,
    build_id: &'static str,
    deploy_id: &'static str,
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
    let snapshot = store_snapshot(state.database.as_ref(), &state.ops_snapshot_failed).await;
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
        duplicate_pool_registrations: crate::metrics_http::duplicate_registration_count(),
        event_store,
        backfill,
        build_revision: report.build_revision,
        build_id: report.build_id,
        deploy_id: report.deploy_id,
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
        store_snapshot(state.database.as_ref(), &state.ops_snapshot_failed).await,
    ))
}

const AUDIT_MAX_ROWS: usize = 50;
const AUDIT_MAX_BYTES: usize = 64 * 1024;

/// Deliberately separate from StoredAudit: private metadata and delivery
/// capabilities must never even be loaded by an operator excerpt read.
#[derive(serde::Serialize, sqlx::FromRow)]
struct AuditRow {
    entry_id: String,
    kind: String,
    guild_id: String,
    occurred_at: String,
    actor_id: Option<String>,
    target_id: Option<String>,
    source_channel_id: Option<String>,
    destination_channel_id: Option<String>,
    message_id: Option<String>,
    action: Option<String>,
    delivery_state: String,
}

impl AuditRow {
    fn valid(&self) -> bool {
        let id = |s: &str| !s.is_empty() && s.len() <= 20 && s.bytes().all(|b| b.is_ascii_digit());
        let optional_ids = [
            &self.actor_id,
            &self.target_id,
            &self.source_channel_id,
            &self.destination_channel_id,
            &self.message_id,
        ];
        !self.entry_id.is_empty()
            && self.entry_id.len() <= 128
            && self
                .entry_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'.'))
            && id(&self.guild_id)
            && optional_ids
                .into_iter()
                .all(|v| v.as_deref().is_none_or(id))
            && matches!(
                self.kind.as_str(),
                "message_edit"
                    | "message_delete"
                    | "member_update"
                    | "voice_join"
                    | "voice_leave"
                    | "voice_move"
                    | "moderation_action"
            )
            && matches!(
                self.delivery_state.as_str(),
                "none" | "pending" | "delivering" | "delivered" | "quarantined"
            )
            && self.action.as_deref().is_none_or(|action| {
                two_bot_core::classify::ModerationAuditAction::ALL
                    .iter()
                    .any(|value| value.as_str() == action)
                    || two_bot_core::mac::is_auditable_action(action)
            })
    }
}

#[derive(serde::Serialize)]
struct AuditExcerpt {
    available: bool,
    rows: Vec<AuditRow>,
    truncated: bool,
}

impl AuditExcerpt {
    fn unavailable() -> Self {
        Self {
            available: false,
            rows: Vec::new(),
            truncated: false,
        }
    }

    fn bounded(mut rows: Vec<AuditRow>) -> Self {
        if !rows.iter().all(AuditRow::valid) {
            return Self::unavailable();
        }
        let truncated = rows.len() > AUDIT_MAX_ROWS;
        rows.truncate(AUDIT_MAX_ROWS);
        let excerpt = Self {
            available: true,
            rows,
            truncated,
        };
        // Field bounds keep valid excerpts below this budget; pin the actual
        // serialized size too, so future schema growth cannot lift the cap.
        if serde_json::to_vec(&excerpt).is_ok_and(|bytes| bytes.len() <= AUDIT_MAX_BYTES) {
            excerpt
        } else {
            Self::unavailable()
        }
    }
}

async fn audit(
    State((state, _)): State<(SharedState, SharedStatus)>,
) -> ([(header::HeaderName, &'static str); 1], Json<AuditExcerpt>) {
    let Some(pool) = state.database.as_ref() else {
        return no_store_json(AuditExcerpt::unavailable());
    };
    // Fixed SELECT only. Bound TEXT at the database boundary before allocating;
    // 129-byte/character sentinels fail validation, never become truncated facts.
    // One extra row detects truncation in the same snapshot as the excerpt.
    let fetch = sqlx::query_as::<_, AuditRow>(
        "SELECT left(entry_id, 129) AS entry_id, left(event_kind, 129) AS kind,
                left(guild_id, 129) AS guild_id,
                to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS occurred_at,
                left(actor_id, 129) AS actor_id, left(target_id, 129) AS target_id,
                left(source_channel_id, 129) AS source_channel_id,
                left(destination_channel_id, 129) AS destination_channel_id,
                left(message_id, 129) AS message_id, left(action, 129) AS action,
                left(delivery_state, 129) AS delivery_state
         FROM operational_audit_log ORDER BY created_at DESC, entry_id DESC LIMIT 51",
    ).fetch_all(pool);
    let excerpt = match tokio::time::timeout(EVENT_STORE_TIMEOUT, fetch).await {
        Ok(Ok(rows)) => AuditExcerpt::bounded(rows),
        _ => AuditExcerpt::unavailable(),
    };
    no_store_json(excerpt)
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

    fn audit_row() -> AuditRow {
        AuditRow {
            entry_id: "synthetic-entry".to_owned(),
            kind: "message_delete".to_owned(),
            guild_id: "424242".to_owned(),
            occurred_at: "2026-10-19T00:00:00.000Z".to_owned(),
            actor_id: None,
            target_id: None,
            source_channel_id: None,
            destination_channel_id: None,
            message_id: None,
            action: None,
            delivery_state: "none".to_owned(),
        }
    }

    #[test]
    fn audit_excerpt_empty_and_exact_row_cap_are_not_truncated() {
        for count in [0, 1, AUDIT_MAX_ROWS] {
            let excerpt = AuditExcerpt::bounded((0..count).map(|_| audit_row()).collect());
            assert!(excerpt.available);
            assert!(!excerpt.truncated);
            assert_eq!(excerpt.rows.len(), count);
        }
    }

    #[test]
    fn audit_excerpt_maximum_fields_and_row_overflow_stay_within_byte_cap() {
        let excerpt = AuditExcerpt::bounded(
            (0..=AUDIT_MAX_ROWS)
                .map(|_| {
                    let mut row = audit_row();
                    row.entry_id = "e".repeat(128);
                    row.guild_id = "9".repeat(20);
                    row.actor_id = Some("9".repeat(20));
                    row.target_id = Some("9".repeat(20));
                    row.source_channel_id = Some("9".repeat(20));
                    row.destination_channel_id = Some("9".repeat(20));
                    row.message_id = Some("9".repeat(20));
                    row.action = Some("channel_overwrite_delete".to_owned());
                    row
                })
                .collect(),
        );
        assert!(excerpt.available);
        assert!(excerpt.truncated);
        assert_eq!(excerpt.rows.len(), AUDIT_MAX_ROWS);
        assert!(serde_json::to_vec(&excerpt).unwrap().len() <= AUDIT_MAX_BYTES);
    }

    #[test]
    fn audit_excerpt_rejects_oversize_and_non_identifier_facts_without_echo() {
        for field in [
            "entry", "guild", "actor", "kind", "state", "action", "bytes",
        ] {
            let mut row = audit_row();
            match field {
                "entry" => row.entry_id = "synthetic-private-marker".repeat(10),
                "guild" => row.guild_id = "synthetic-private-marker".to_owned(),
                "actor" => row.actor_id = Some("synthetic-private-marker".to_owned()),
                "kind" => row.kind = "synthetic-private-marker".to_owned(),
                "state" => row.delivery_state = "synthetic-private-marker".to_owned(),
                "action" => row.action = Some("synthetic-private-marker".to_owned()),
                _ => row.occurred_at = "synthetic-private-marker".repeat(AUDIT_MAX_BYTES),
            }
            let excerpt = AuditExcerpt::bounded(vec![row]);
            assert!(!excerpt.available, "invalid {field} must fail closed");
            assert!(excerpt.rows.is_empty());
            assert!(!excerpt.truncated);
            assert!(!serde_json::to_string(&excerpt)
                .unwrap()
                .contains("synthetic-private-marker"));
        }
    }

    #[tokio::test]
    async fn audit_excerpt_timestamp_matches_worker_millisecond_utc_contract() {
        let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
            assert!(
                std::env::var("GITHUB_ACTIONS").is_err(),
                "CI must supply the guarded test database"
            );
            eprintln!("SKIP audit_excerpt_timestamp: TWO_TEST_DATABASE_URL is not set");
            return;
        };
        let db = two_bot_testsupport::TestDatabase::create(
            &url,
            &sqlx::migrate!("../cutover/migrations"),
        )
        .await
        .expect("create migrated agent-testdb fixture");
        sqlx::query(
            "INSERT INTO operational_audit_log
                (entry_id, event_kind, guild_id, occurred_at, metadata_json, created_at)
             VALUES ('synthetic-whole-second', 'message_delete', '424242',
                     TIMESTAMPTZ '2026-10-19T00:00:02Z', '{}',
                     TIMESTAMPTZ '2026-10-19T00:00:02Z'),
                    ('synthetic-fractional-second', 'message_delete', '424242',
                     TIMESTAMPTZ '2026-10-19T02:00:01.123+02:00', '{}',
                     TIMESTAMPTZ '2026-10-19T00:00:01Z')",
        )
        .execute(db.pool())
        .await
        .expect("seed exact timestamp fixtures");
        let (status, json) = get(
            app(
                GatewayState::Connected,
                Some(db.pool().clone()),
                crate::jobs::statuses(&[], true),
            ),
            "/ops/audit",
        )
        .await;
        // Close the disposable fixture even if the contract assertions fail.
        db.close().await.expect("close fixture database");
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["available"], true);
        assert_eq!(json["rows"].as_array().unwrap().len(), 2);
        assert_eq!(json["rows"][0]["entry_id"], "synthetic-whole-second");
        assert_eq!(json["rows"][0]["occurred_at"], "2026-10-19T00:00:02.000Z");
        assert_eq!(json["rows"][1]["entry_id"], "synthetic-fractional-second");
        assert_eq!(json["rows"][1]["occurred_at"], "2026-10-19T00:00:01.123Z");
    }

    #[tokio::test]
    async fn audit_head_is_refused_without_reading_database() {
        let response = app(
            GatewayState::Connected,
            None,
            crate::jobs::statuses(&[], true),
        )
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri("/ops/audit")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

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

    /// Serializes the tests that assert exact `ops_snapshot_failed` counts
    /// on the process-global capture, plus the tests that emit those lines:
    /// without it a concurrent emitter lands inside another test's window
    /// and its absolute count assertion fails. Hold for the whole test in
    /// every test that reads or writes these diagnostics (same pattern as
    /// the internal-action receiver's `TRACING_DIAGNOSTIC_LOCK`).
    static OPS_SNAPSHOT_DIAGNOSTIC_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Process-wide log capture shared with every log-hygiene test in this
    /// binary (see `tracing_capture::global`): one install per process, each
    /// reader keeps only the text appended after its own start offset.
    fn ops_snapshot_capture() -> &'static crate::tracing_capture::Capture {
        crate::tracing_capture::global()
    }

    /// A pool whose every query fails at once with `PoolClosed`: the
    /// deterministic store-failure probe (no network, no waits).
    async fn closed_pool() -> sqlx::PgPool {
        let pool = lazy_pool();
        pool.close().await;
        pool
    }

    async fn get_bytes(app: Router, uri: &str) -> (StatusCode, Vec<u8>) {
        let response = app
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .expect("test request"),
            )
            .await
            .expect("oneshot");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1_048_576)
            .await
            .expect("body bytes");
        (status, bytes.to_vec())
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
        let _diagnostics = OPS_SNAPSHOT_DIAGNOSTIC_LOCK.lock().await;
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
        // Presence only: the counter is process-global and other tests in
        // this binary reject into it, so the exact value is not stable here.
        // Exact values (1 after a reject, 0 on a fresh boot) are pinned by
        // the isolated child-process acceptance tests.
        assert!(json["duplicate_pool_registrations"].is_u64());
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
        assert_eq!(
            json["deploy_id"],
            option_env!("BOT_DEPLOY_ID").unwrap_or("unknown")
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
            "gateway_run",
            "none",
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
        let _diagnostics = OPS_SNAPSHOT_DIAGNOSTIC_LOCK.lock().await;
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
        // Its failing probes emit `ops_snapshot_failed`: hold the
        // diagnostic lock so the counting tests keep absolute windows.
        let _diagnostics = OPS_SNAPSHOT_DIAGNOSTIC_LOCK.lock().await;
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

    #[test]
    fn snapshot_failure_classes_are_fixed_tokens() {
        assert_eq!(
            snapshot_failure_class(&two_bot_store::journal::JournalError::Db(
                sqlx::Error::PoolClosed
            )),
            "pool_closed"
        );
        assert_eq!(
            snapshot_failure_class(&two_bot_store::journal::JournalError::Db(
                sqlx::Error::RowNotFound
            )),
            "row_not_found"
        );
        // The store read never validates, so this arm is unreachable in
        // practice; the token keeps the match exhaustive without a panic.
        assert_eq!(
            snapshot_failure_class(&two_bot_store::journal::JournalError::Invalid("test")),
            "invalid"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn snapshot_store_failure_warns_once_per_transition() {
        let _diagnostics = OPS_SNAPSHOT_DIAGNOSTIC_LOCK.lock().await;
        let failed = AtomicBool::new(false);
        let pool = closed_pool().await;
        let capture = ops_snapshot_capture();
        let start = capture.text().len();
        // Two failing probes: the transition logs, the steady state stays
        // silent.
        assert!(store_snapshot(Some(&pool), &failed).await.is_none());
        assert!(store_snapshot(Some(&pool), &failed).await.is_none());
        assert!(failed.load(Ordering::Relaxed));
        let logs = capture.text().split_off(start);
        assert_eq!(
            logs.matches("ops_snapshot_failed").count(),
            1,
            "one warn per transition, got: {logs:?}"
        );
        assert!(
            logs.contains("db_class=\"pool_closed\""),
            "warn must carry the fixed class token, got: {logs:?}"
        );
        assert!(
            logs.contains("WARN two_bot::ops_http"),
            "snapshot failure must warn on this module, got: {logs:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn snapshot_probe_timeout_warns_with_timed_out_once() {
        let _diagnostics = OPS_SNAPSHOT_DIAGNOSTIC_LOCK.lock().await;
        let failed = AtomicBool::new(false);
        // Open but unconnected: every query pends on connect, so a zero
        // budget always elapses instead of answering.
        let pool = lazy_pool();
        let capture = ops_snapshot_capture();
        let start = capture.text().len();
        assert!(
            store_snapshot_with_budget(Some(&pool), &failed, std::time::Duration::ZERO)
                .await
                .is_none()
        );
        assert!(
            store_snapshot_with_budget(Some(&pool), &failed, std::time::Duration::ZERO)
                .await
                .is_none()
        );
        pool.close().await;
        let logs = capture.text().split_off(start);
        assert_eq!(
            logs.matches("ops_snapshot_failed").count(),
            1,
            "one warn per transition, got: {logs:?}"
        );
        assert!(
            logs.contains("timed_out=true"),
            "timeout warn must carry timed_out, got: {logs:?}"
        );
        assert!(
            !logs.contains("db_class"),
            "timeout warn must not carry a store class, got: {logs:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn snapshot_recovery_is_silent_and_rearms() {
        let _diagnostics = OPS_SNAPSHOT_DIAGNOSTIC_LOCK.lock().await;
        let failed = AtomicBool::new(false);
        let capture = ops_snapshot_capture();
        let start = capture.text().len();
        note_snapshot_outcome(&failed, SnapshotOutcome::StoreFailed("pool_closed"));
        assert!(failed.load(Ordering::Relaxed));
        // Steady-state failure: silent.
        note_snapshot_outcome(&failed, SnapshotOutcome::StoreFailed("pool_closed"));
        // Recovery: silent, and the latch drops.
        note_snapshot_outcome(&failed, SnapshotOutcome::Ok);
        assert!(!failed.load(Ordering::Relaxed));
        // The next edge re-arms, whatever the failure kind.
        note_snapshot_outcome(&failed, SnapshotOutcome::TimedOut);
        note_snapshot_outcome(&failed, SnapshotOutcome::TimedOut);
        let logs = capture.text().split_off(start);
        assert_eq!(
            logs.matches("ops_snapshot_failed").count(),
            2,
            "edges log, steady states and recovery stay silent, got: {logs:?}"
        );
        assert!(
            logs.contains("db_class=\"pool_closed\""),
            "store edge must carry its class, got: {logs:?}"
        );
        assert!(
            logs.contains("timed_out=true"),
            "timeout edge must carry timed_out, got: {logs:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn snapshot_success_result_silently_rearms_store_failure() {
        let _diagnostics = OPS_SNAPSHOT_DIAGNOSTIC_LOCK.lock().await;
        let failed = AtomicBool::new(false);
        let pool = closed_pool().await;
        let capture = ops_snapshot_capture();
        let start = capture.text().len();
        assert!(store_snapshot(Some(&pool), &failed).await.is_none());
        assert!(failed.load(Ordering::Relaxed));
        let recovery_start = capture.text().len();

        // Exercise the production probe-result mapping, not the latch helper:
        // omitting the success arm's reset must leave the latch stuck true.
        let recovered = snapshot_probe_result(
            Ok(Ok(two_bot_store::journal::Snapshot {
                point: 10,
                cursors: BTreeMap::from([("events".to_owned(), 7)]),
                tips: BTreeMap::from([("events".to_owned(), 10)]),
            })),
            &failed,
        )
        .expect("successful probe retains its snapshot");
        assert_eq!(recovered.point, 10);
        assert_eq!(
            recovered.cursors,
            BTreeMap::from([("events".to_owned(), 7)])
        );
        assert_eq!(recovered.tips, BTreeMap::from([("events".to_owned(), 10)]));
        assert!(!failed.load(Ordering::Relaxed));
        let recovery_logs = capture.text().split_off(recovery_start);
        assert_eq!(
            recovery_logs.matches("ops_snapshot_failed").count(),
            0,
            "recovery must be silent: {recovery_logs:?}"
        );

        assert!(store_snapshot(Some(&pool), &failed).await.is_none());
        assert!(failed.load(Ordering::Relaxed));
        let logs = capture.text().split_off(start);
        assert_eq!(
            logs.matches("ops_snapshot_failed").count(),
            2,
            "each outage must warn after a successful probe: {logs:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ops_handlers_share_snapshot_failure_latch() {
        let _diagnostics = OPS_SNAPSHOT_DIAGNOSTIC_LOCK.lock().await;
        let state = state_with(GatewayState::Connected, Some(closed_pool().await));
        assert!(!state.ops_snapshot_failed.load(Ordering::Relaxed));
        let app = crate::server::router_with_guard(
            state.clone(),
            crate::jobs::statuses(&[], true),
            two_bot_discord::ratelimit_guard::process_guard(),
        );
        let capture = ops_snapshot_capture();
        let start = capture.text().len();
        for uri in [
            "/ops/summary",
            "/ops/backfill",
            "/ops/summary",
            "/ops/backfill",
        ] {
            let (status, json) = get(app.clone(), uri).await;
            assert_eq!(status, StatusCode::OK);
            let section = if uri == "/ops/summary" {
                &json["event_store"]
            } else {
                &json
            };
            assert_eq!(section["available"], false);
            assert!(state.ops_snapshot_failed.load(Ordering::Relaxed));
            let logs = capture.text().split_off(start);
            assert_eq!(
                logs.matches("ops_snapshot_failed").count(),
                1,
                "both handlers must share one outage transition after {uri}: {logs:?}"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn snapshot_without_database_is_silent_and_preserves_latch() {
        let _diagnostics = OPS_SNAPSHOT_DIAGNOSTIC_LOCK.lock().await;
        let capture = ops_snapshot_capture();
        let start = capture.text().len();
        // Neither a spurious failure nor an implicit recovery is allowed.
        for initial in [false, true] {
            let failed = AtomicBool::new(initial);
            assert!(store_snapshot(None, &failed).await.is_none());
            assert_eq!(failed.load(Ordering::Relaxed), initial);
        }
        let logs = capture.text().split_off(start);
        assert_eq!(
            logs.matches("ops_snapshot_failed").count(),
            0,
            "an absent database must not warn: {logs:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ops_bodies_are_byte_identical_across_snapshot_transition() {
        // Emits (the first failing probe trips the transition), so it holds
        // the diagnostic lock even though it never reads the capture.
        let _diagnostics = OPS_SNAPSHOT_DIAGNOSTIC_LOCK.lock().await;
        let pool = closed_pool().await;
        // One app per endpoint, so the first request trips the transition
        // (and logs) while the second rides the steady state.
        let backfill_app = app(
            GatewayState::Connected,
            Some(pool.clone()),
            crate::jobs::statuses(&[], true),
        );
        let (first_status, first) = get_bytes(backfill_app.clone(), "/ops/backfill").await;
        let (second_status, second) = get_bytes(backfill_app, "/ops/backfill").await;
        assert_eq!(first_status, StatusCode::OK);
        assert_eq!(second_status, StatusCode::OK);
        assert_eq!(first, second, "log transition must not alter body bytes");
        let backfill: serde_json::Value =
            serde_json::from_slice(&first).expect("backfill JSON body");
        assert_eq!(backfill["available"], false);
        // `/ops/summary` also renders process-global gauges, so only the
        // snapshot-derived sections compare; both must stay degraded.
        let summary_app = app(
            GatewayState::Connected,
            Some(pool.clone()),
            crate::jobs::statuses(&[], true),
        );
        let (_, summary_first) = get_bytes(summary_app.clone(), "/ops/summary").await;
        let (_, summary_second) = get_bytes(summary_app, "/ops/summary").await;
        let first_json: serde_json::Value =
            serde_json::from_slice(&summary_first).expect("summary JSON body");
        let second_json: serde_json::Value =
            serde_json::from_slice(&summary_second).expect("summary JSON body");
        assert_eq!(first_json["event_store"], second_json["event_store"]);
        assert_eq!(first_json["backfill"], second_json["backfill"]);
        assert_eq!(first_json["event_store"]["available"], false);
        assert_eq!(first_json["backfill"]["available"], false);
        // No log token may reach the wire on either endpoint.
        for (name, bytes) in [("backfill", &first), ("summary", &summary_first)] {
            let text = String::from_utf8_lossy(bytes);
            for leaked in [
                "ops_snapshot_failed",
                "pool_closed",
                "timed_out",
                "db_class",
            ] {
                assert!(!text.contains(leaked), "{name} body leaked {leaked:?}");
            }
        }
    }
}
