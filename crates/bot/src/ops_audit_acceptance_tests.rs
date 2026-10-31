//! Failing-first acceptance for the container `GET /ops/audit` excerpt
//! behind the MCP `get_audit` tool. Test files only — no production
//! changes. The Builder starts from this head and must not edit this file.
//!
//! Contract under test (builder: implement exactly this, joining the
//! existing `/ops/summary` + `/ops/gateway` + `/ops/backfill` roll-ups on
//! the same container listener):
//! - `GET /ops/audit` answers `200` JSON (`application/json`,
//!   `cache-control: no-store`) with body
//!   `{"available": bool, "rows": [...], "truncated": bool}`.
//! - Rows are the newest `operational_audit_log` entries first (by
//!   `created_at`), capped at 50 rows; `truncated` is true exactly when
//!   more rows exist than were returned. The whole body fits the MCP
//!   layer's 64 KiB container-read cap.
//! - Each row carries exactly the allowlisted fields `entry_id`, `kind`
//!   (the stored `event_kind`), `guild_id`, `occurred_at` (ISO-8601 UTC
//!   string), `actor_id`, `target_id`, `source_channel_id`,
//!   `destination_channel_id`, `message_id`, `action` (each a string or
//!   null) and `delivery_state`. Nothing else: no `metadata_json`
//!   payload, no delivery nonce/claim token, no last-error text — never
//!   secrets, tokens or message bodies, even when stored rows contain
//!   such material in their metadata.
//! - The route is read-only: non-GET methods are refused with `405` and
//!   repeated reads mutate no audit row (delivery state, attempts and
//!   timestamps are untouched).
//! - With no database, or an unreachable one, the route degrades like
//!   the other ops roll-ups: `available: false`, empty rows,
//!   `truncated: false`, still `200`, never an error body or a 500.
//!
//! Database cases run only against the guarded disposable fixture
//! (`TWO_TEST_DATABASE_URL`, agent-testdb / CI Postgres), mirroring
//! `audit_runtime_tests::database`: skipped locally when unset, a hard
//! failure in CI without it. No Discord endpoint is contacted.

use std::sync::Arc;

use axum::{
    body::Body,
    http::{header, Method, Request, StatusCode},
    Router,
};
use tokio::sync::RwLock;
use tower::ServiceExt as _;
use two_bot_testsupport::TestDatabase;

use crate::gateway::GatewayState;
use crate::server::SharedState;

/// Row keys the excerpt may carry, sorted for exact-set comparison.
const ALLOWED_ROW_KEYS: [&str; 11] = [
    "action",
    "actor_id",
    "delivery_state",
    "destination_channel_id",
    "entry_id",
    "guild_id",
    "kind",
    "message_id",
    "occurred_at",
    "source_channel_id",
    "target_id",
];

/// The bounded row cap the Builder and the MCP proxy share.
const MAX_ROWS: usize = 50;

/// The MCP outbound container-read cap (wrangler `MCP_MAX_BODY_BYTES`).
const MAX_BODY_BYTES: usize = 64 * 1024;

const PATH: &str = "/ops/audit";

fn app(database: Option<sqlx::PgPool>) -> Router {
    crate::server::router_with_guard(
        SharedState::new(Arc::new(RwLock::new(GatewayState::Connected)), database),
        crate::jobs::statuses(&[], true),
        two_bot_discord::ratelimit_guard::process_guard(),
    )
}

fn lazy_pool() -> sqlx::PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy("postgres://agent_test@agent-testdb/agent_test")
        .unwrap_or_else(|_| panic!("lazy pool must build without I/O"))
}

/// Mirrors `audit_runtime_tests::database`: disposable migrated fixture,
/// skipped off-CI when the guarded URL is absent.
async fn database(test: &str) -> Option<TestDatabase> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP {test}: TWO_TEST_DATABASE_URL is not set");
        return None;
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated agent-testdb fixture"),
    )
}

async fn raw_request(app: Router, method: Method) -> (StatusCode, String, axum::http::HeaderMap) {
    let response = app
        .oneshot(
            Request::builder()
                .method(method)
                .uri(PATH)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1_048_576)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap(), headers)
}

async fn get_json(app: Router) -> (StatusCode, serde_json::Value, String) {
    let (status, text, headers) = raw_request(app, Method::GET).await;
    assert_eq!(
        headers[header::CONTENT_TYPE],
        "application/json",
        "excerpt must be JSON, body: {text}"
    );
    assert_eq!(
        headers[header::CACHE_CONTROL],
        "no-store",
        "excerpt must never be cached"
    );
    assert!(
        text.len() <= MAX_BODY_BYTES,
        "excerpt body {} bytes exceeds the MCP container-read cap",
        text.len()
    );
    let json: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|_| panic!("excerpt must parse: {text}"));
    (status, json, text)
}

fn rows_of(json: &serde_json::Value) -> &Vec<serde_json::Value> {
    json["rows"]
        .as_array()
        .expect("excerpt must carry a rows array")
}

/// Exact key-set pin: an allowlisted excerpt cannot quietly grow a
/// metadata, nonce, token or body field.
fn assert_row_shape(row: &serde_json::Value) {
    let object = row.as_object().expect("each excerpt row is an object");
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ALLOWED_ROW_KEYS.to_vec(),
        "excerpt row keys must be exactly the allowlist"
    );
    for key in [
        "entry_id",
        "kind",
        "guild_id",
        "occurred_at",
        "delivery_state",
    ] {
        assert!(
            row[key].as_str().is_some_and(|value| !value.is_empty()),
            "row field {key} must be a non-empty string, row: {row}"
        );
    }
    for key in [
        "actor_id",
        "target_id",
        "source_channel_id",
        "destination_channel_id",
        "message_id",
        "action",
    ] {
        assert!(
            row[key].is_null() || row[key].is_string(),
            "row field {key} must be a string or null, row: {row}"
        );
    }
}

#[tokio::test]
async fn acceptance_audit_without_database_is_unavailable_and_empty() {
    let (status, json, _) = get_json(app(None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["available"], false);
    assert_eq!(json["rows"], serde_json::json!([]));
    assert_eq!(json["truncated"], false);
}

#[tokio::test]
async fn acceptance_audit_unreachable_database_degrades_not_500() {
    // `connect_lazy` performs no I/O; the read fails and the route must
    // report unavailable with 200, never an error text or a 500.
    let pool = lazy_pool();
    let (status, json, text) = get_json(app(Some(pool.clone()))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["available"], false, "body: {text}");
    assert_eq!(json["rows"], serde_json::json!([]));
    assert_eq!(json["truncated"], false);
    pool.close().await;
}

#[tokio::test]
async fn acceptance_audit_is_get_only() {
    for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
        let (status, text, _) = raw_request(app(None), method.clone()).await;
        assert_eq!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} {PATH} must be refused, body: {text}"
        );
    }
    let (status, _, _) = get_json(app(None)).await;
    assert_eq!(status, StatusCode::OK, "GET stays served after refusals");
}

/// Seed one audit row. `at_second` orders both `occurred_at` and
/// `created_at` (the excerpt's newest-first key) deterministically.
async fn seed_row(pool: &sqlx::PgPool, entry_id: &str, at_second: u32, metadata: &str) {
    let at = format!("2026-10-19T00:00:{at_second:02}Z");
    sqlx::query(
        "INSERT INTO operational_audit_log
           (entry_id, event_kind, guild_id, occurred_at, actor_id, target_id,
            source_channel_id, destination_channel_id, message_id, action,
            metadata_json, created_at, delivery_state, delivery_nonce,
            delivery_claim_token, delivery_last_error)
         VALUES ($1, 'message_delete', '424242', $2::timestamptz, '111111',
                 '333333', '444444', NULL, '555555', NULL, $3,
                 $2::timestamptz, 'none',
                 'synthetic-delivery-nonce-must-not-leak',
                 'synthetic-claim-token-must-not-leak',
                 'synthetic-last-error-must-not-leak')",
    )
    .bind(entry_id)
    .bind(&at)
    .bind(metadata)
    .execute(pool)
    .await
    .expect("seed audit row");
}

#[tokio::test]
async fn acceptance_audit_returns_real_rows_newest_first_allowlisted() {
    let Some(db) = database("acceptance_audit_returns_real_rows").await else {
        return;
    };
    let pool = db.pool().clone();
    seed_row(&pool, "synthetic-entry-older", 1, "{}").await;
    seed_row(&pool, "synthetic-entry-newer", 2, "{}").await;

    let (status, json, _) = get_json(app(Some(pool))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["available"], true);
    assert_eq!(json["truncated"], false);
    let rows = rows_of(&json);
    assert_eq!(rows.len(), 2, "both seeded rows are excerpted: {json}");
    assert_eq!(
        rows[0]["entry_id"], "synthetic-entry-newer",
        "newest created_at first"
    );
    assert_eq!(rows[1]["entry_id"], "synthetic-entry-older");
    let newest = &rows[0];
    assert_row_shape(newest);
    assert_eq!(newest["kind"], "message_delete");
    assert_eq!(newest["guild_id"], "424242");
    assert_eq!(newest["actor_id"], "111111");
    assert_eq!(newest["target_id"], "333333");
    assert_eq!(newest["source_channel_id"], "444444");
    assert_eq!(newest["destination_channel_id"], serde_json::Value::Null);
    assert_eq!(newest["message_id"], "555555");
    assert_eq!(newest["delivery_state"], "none");
    assert!(
        newest["occurred_at"]
            .as_str()
            .is_some_and(|at| at.starts_with("2026-10-19T00:00:02")),
        "occurred_at is the stored ISO instant, got: {newest}"
    );
    db.close().await.expect("close fixture database");
}

#[tokio::test]
async fn acceptance_audit_caps_rows_and_marks_truncated_within_byte_bound() {
    let Some(db) = database("acceptance_audit_caps_rows").await else {
        return;
    };
    let pool = db.pool().clone();
    for index in 0..(MAX_ROWS + 5) {
        seed_row(&pool, &format!("synthetic-entry-{index:02}"), 1, "{}").await;
        // Distinct created_at per row, rising with the index: the seeded
        // `at_second` above is only ever 1, so spread rows explicitly.
        sqlx::query(
            "UPDATE operational_audit_log
             SET created_at = TIMESTAMPTZ '2026-10-19T00:00:00Z' + make_interval(secs => $2::float8)
             WHERE entry_id = $1",
        )
        .bind(format!("synthetic-entry-{index:02}"))
        .bind(index as f64)
        .execute(&pool)
        .await
        .expect("spread created_at");
    }

    let (status, json, _) = get_json(app(Some(pool))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["available"], true);
    assert_eq!(json["truncated"], true, "more rows exist than returned");
    let rows = rows_of(&json);
    assert_eq!(rows.len(), MAX_ROWS, "excerpt is capped at {MAX_ROWS} rows");
    assert_eq!(
        rows[0]["entry_id"],
        format!("synthetic-entry-{:02}", MAX_ROWS + 4),
        "the cap keeps the newest rows, not the oldest"
    );
    for row in rows {
        assert_row_shape(row);
    }
    db.close().await.expect("close fixture database");
}

#[tokio::test]
async fn acceptance_audit_redacts_metadata_and_delivery_secrets() {
    let Some(db) = database("acceptance_audit_redacts").await else {
        return;
    };
    let pool = db.pool().clone();
    // Hostile stored material: a message-body-shaped value, a token-shaped
    // value and free text all inside metadata_json, plus the delivery
    // nonce/claim-token/last-error seeded on every row by seed_row. None
    // of it may appear — as a key or as a value — while the row itself
    // still shows up in the excerpt.
    let hostile_metadata = serde_json::json!({
        "content": "synthetic-message-body-must-not-leak",
        "token": "synthetic-token-value-must-not-leak",
        "reason": "synthetic free-form reason must not leak",
    })
    .to_string();
    seed_row(&pool, "synthetic-entry-hostile", 7, &hostile_metadata).await;

    let (status, json, text) = get_json(app(Some(pool))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["available"], true);
    assert_eq!(
        rows_of(&json).len(),
        1,
        "the hostile row is still excerpted"
    );
    assert_eq!(rows_of(&json)[0]["entry_id"], "synthetic-entry-hostile");
    assert_row_shape(&rows_of(&json)[0]);
    for sentinel in [
        "synthetic-message-body-must-not-leak",
        "synthetic-token-value-must-not-leak",
        "synthetic free-form reason must not leak",
        "synthetic-delivery-nonce-must-not-leak",
        "synthetic-claim-token-must-not-leak",
        "synthetic-last-error-must-not-leak",
        "metadata_json",
        "metadata",
        "delivery_nonce",
        "delivery_claim_token",
        "last_error",
        "content",
        "token",
        "reason",
    ] {
        assert!(
            !text.contains(sentinel),
            "excerpt leaks {sentinel:?}: {text}"
        );
    }
    db.close().await.expect("close fixture database");
}

#[tokio::test]
async fn acceptance_audit_repeat_reads_mutate_nothing() {
    let Some(db) = database("acceptance_audit_repeat_reads").await else {
        return;
    };
    let pool = db.pool().clone();
    seed_row(&pool, "synthetic-entry-stable", 3, "{}").await;

    let before: Vec<(String, String, i32, Option<String>)> = sqlx::query_as(
        "SELECT entry_id, delivery_state, delivery_attempts,
                to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')
         FROM operational_audit_log ORDER BY entry_id",
    )
    .fetch_all(&pool)
    .await
    .expect("snapshot audit rows before reads");

    let (status_a, json_a, _) = get_json(app(Some(pool.clone()))).await;
    let (status_b, json_b, _) = get_json(app(Some(pool.clone()))).await;
    assert_eq!(status_a, StatusCode::OK);
    assert_eq!(status_b, StatusCode::OK);
    assert_eq!(json_a, json_b, "repeat reads are stable");

    let after: Vec<(String, String, i32, Option<String>)> = sqlx::query_as(
        "SELECT entry_id, delivery_state, delivery_attempts,
                to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')
         FROM operational_audit_log ORDER BY entry_id",
    )
    .fetch_all(&pool)
    .await
    .expect("snapshot audit rows after reads");
    assert_eq!(before, after, "reads must not mutate audit rows");
    db.close().await.expect("close fixture database");
}
