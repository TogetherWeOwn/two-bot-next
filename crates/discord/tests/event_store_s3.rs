//! Event-store S3 acceptance tests (paired failing-tests card for the S3 build).
//!
//! Test files ONLY: this suite pins the S3 acceptance criteria before the
//! builder starts, so every test here FAILS until the S3 build lands its
//! presence-capture migration on the append-only store. The builder makes these
//! tests pass; nothing outside test code changes on this card.
//!
//! Contract under test (event-store P1 slice 3: PRESENCE_UPDATE minimal
//! capture into the append-only `event_store` table):
//!
//! - One row per presence update with minimal fields: `event_type`,
//!   `guild_id`, `user_id`, `event_time`, `received_at`, `dedup_key` plus the
//!   presence enums `presence_status` (online/idle/dnd/invisible/offline), the
//!   activity TYPE enum `activity_type` (0 Game, 1 Streaming, 2 Listening,
//!   3 Watching, 4 Custom, 5 Competing) and the nullable `application_id`.
//!   No free-text columns: no custom-status text, no activity name, details,
//!   state, URL or emoji.
//! - Best-effort idempotent inserts: `dedup_key` carries a UNIQUE key such as
//!   `PRESENCE_UPDATE:<guild>:<user>:<event_time>` (with `event_time` set at
//!   receive time, since Discord's PRESENCE_UPDATE payload carries no event
//!   timestamp) and every insert runs `ON CONFLICT (dedup_key) DO NOTHING`,
//!   so a same-key retry inserts nothing. A resume redelivery received at a
//!   different instant carries a different key and is NOT absorbed.
//! - Privacy tripwire: a fixture carrying custom-status text or activity
//!   details/URLs persists its ids and enums only; the sentinels appear in
//!   no stored row.
//! - Metrics use fixed cardinality only: the dedicated presence-capture
//!   outcome counter exposes exactly three series
//!   (`outcome="inserted|duplicate|rejected"`) with no per-user or
//!   per-activity labels. Presence is the highest-volume family, so the
//!   insert path stays lean.
//!
//! Pending-test routing (established pattern): every test is ignored and
//! skipped in the nightly broad sweep, so this card's CI stays green while
//! the S3 build is outstanding; run with `-- --ignored` to see the gap. The
//! S3 build card un-ignores these tests once the migration lands. DB tests
//! run only against the disposable test database (`TestDatabase` refuses
//! anything but agent-testdb), never staging or production.
#![cfg(feature = "db")]

use serde_json::{json, Value};
use sqlx::PgPool;
use two_bot_core::metrics::Metrics;
use two_bot_testsupport::{assert_event_store_columns, TestDatabase};

const GUILD: &str = "3333";
const USER: &str = "42";
const APP_ID: &str = "5555";

/// Sentinel custom-status text: models the activity `state` free text the
/// store must refuse to persist. If this string ever appears in a persisted
/// row, the ids-only fence broke.
const CUSTOM_STATUS_SENTINEL: &str = "s3-custom-status-sentinel-must-never-persist";

/// Sentinel activity details text: same rule as the custom status, other
/// payload shape.
const DETAILS_SENTINEL: &str = "s3-privacy-sentinel-details-must-never-persist";

/// Sentinel activity URL: same rule as the custom status, URL payload shape.
const URL_SENTINEL: &str = "https://example.invalid/s3-privacy-sentinel-url-must-never-persist";

/// Allowed presence status enum value persisted by the S3 contract.
const STATUS_ONLINE: &str = "online";

/// Allowed presence status enum value for the no-activity (offline) case.
const STATUS_OFFLINE: &str = "offline";

/// Allowed activity TYPE enum value (0 = Game) persisted by the S3 contract.
const ACTIVITY_GAME: i32 = 0;

/// Tripwire activity TYPE enum value (4 = Custom): even when the gateway
/// reports a custom status, only the TYPE enum persists, never its text.
const ACTIVITY_CUSTOM: i32 = 4;

struct TestDb {
    fixture: TestDatabase,
    pool: PgPool,
}

impl TestDb {
    async fn new() -> Self {
        let url =
            std::env::var("TWO_TEST_DATABASE_URL").expect("explicit test-container URL required");
        let fixture = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("disposable test database");
        let pool = fixture.pool().clone();
        Self { fixture, pool }
    }

    async fn cleanup(self) {
        self.fixture.close().await.expect("drop test database");
    }
}

/// Minimal PRESENCE_UPDATE gateway payload: ids + timestamp + the status
/// enum + one activity carrying the TYPE enum and application id only.
fn presence_fixture(event_time: &str, status: &str, activities: Value) -> Value {
    json!({
        "user": { "id": USER },
        "guild_id": GUILD,
        "status": status,
        "activities": activities,
        "client_status": { "desktop": status },
        "observed_at": event_time,
    })
}

/// One game activity: TYPE enum + application id, no name/details/state/URL.
fn game_activity() -> Value {
    json!([{ "name": null, "type": ACTIVITY_GAME, "application_id": APP_ID }])
}

/// No activities: the offline/invisible shape carries no activity enums.
fn no_activities() -> Value {
    json!([])
}

/// Custom-status activity carrying free text in every text field the privacy
/// fence forbids: `state`, `details` and `url`.
fn custom_status_activity() -> Value {
    json!([{
        "name": "Custom Status",
        "type": ACTIVITY_CUSTOM,
        "state": CUSTOM_STATUS_SENTINEL,
        "details": DETAILS_SENTINEL,
        "url": URL_SENTINEL,
        "application_id": null,
    }])
}

fn dedup_key(event_time: &str) -> String {
    format!("PRESENCE_UPDATE:{GUILD}:{USER}:{event_time}")
}

/// S3 append-only insert for a presence update: identity + timestamps + the
/// presence status and activity TYPE enums, idempotent on `dedup_key`.
/// Returns rows inserted (1) or deduplicated (0). This helper binds
/// ids/enums only, so on its own it cannot prove free text never persists
/// (an extra column would simply stay NULL here); the schema allowlist plus
/// the no-json fence in the privacy tripwire is what refuses that column.
async fn insert_presence_event(
    pool: &PgPool,
    guild_id: &str,
    user_id: &str,
    event_time: &str,
    presence_status: &str,
    activity_type: Option<i32>,
    application_id: Option<&str>,
) -> Result<u64, sqlx::Error> {
    let key = dedup_key(event_time);
    let result = sqlx::query(
        "INSERT INTO event_store
            (event_type, guild_id, user_id, event_time, received_at, dedup_key,
             presence_status, activity_type, application_id)
         VALUES
            ('PRESENCE_UPDATE', $1, $2, $3::timestamptz, NOW(), $4, $5, $6, $7)
         ON CONFLICT (dedup_key) DO NOTHING",
    )
    .bind(guild_id)
    .bind(user_id)
    .bind(event_time)
    .bind(&key)
    .bind(presence_status)
    .bind(activity_type)
    .bind(application_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

async fn table_columns(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns
         WHERE table_name = 'event_store' ORDER BY column_name",
    )
    .fetch_all(pool)
    .await
    .expect("event_store columns readable")
}

async fn table_column_types(pool: &PgPool) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT column_name, data_type FROM information_schema.columns
         WHERE table_name = 'event_store' ORDER BY column_name",
    )
    .fetch_all(pool)
    .await
    .expect("event_store column types readable")
}

const REQUIRED_COLUMNS: &[&str] = &[
    "event_type",
    "guild_id",
    "user_id",
    "event_time",
    "received_at",
    "dedup_key",
    "presence_status",
    "activity_type",
    "application_id",
];

/// Shared-table schema check: the S3 presence columns must be present and every
/// column must sit on the reviewed allowlist in
/// `two_bot_testsupport::EVENT_STORE_ALLOWED_COLUMNS` (the fence table in
/// `docs/event-store.md`). Sibling slices share `event_store` via
/// `ADD COLUMN IF NOT EXISTS`, so an exact S3-only set would break when S1/S2
/// land; a raw-payload or free-text column is in no slice and fails here.
fn assert_presence_schema(columns: &[String]) {
    assert!(
        !columns.is_empty(),
        "S3 creates the presence columns on the append-only event_store table"
    );
    assert_event_store_columns(columns, REQUIRED_COLUMNS);
}

/// Only the reviewed `roles` role-id array may use `jsonb`: free-text payloads
/// (raw presence payloads, activity blobs) have no other schema home whatever
/// the column name. The writer below binds ids/enums only, so this type fence
/// plus the shared allowlist above is what makes the privacy tripwire fail on
/// a scratch `raw_payload JSONB` migration while the sibling S2 `roles`
/// column passes.
async fn assert_no_json_columns(pool: &PgPool) {
    for (column, data_type) in table_column_types(pool).await {
        if column == "roles" {
            assert_eq!(
                data_type, "jsonb",
                "reviewed roles column keeps its jsonb role-id array type"
            );
            continue;
        }
        assert!(
            data_type != "json" && data_type != "jsonb",
            "event_store column {column} has free-text type {data_type}; \
             presence capture persists ids+enums only"
        );
    }
}

async fn assert_dedup_key_is_unique(pool: &PgPool) {
    let unique: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM pg_indexes
         WHERE tablename = 'event_store'
           AND indexdef LIKE '%UNIQUE%'
           AND indexdef LIKE '%dedup_key%'",
    )
    .fetch_one(pool)
    .await
    .expect("dedup_key index readable");
    assert_eq!(unique.0, 1, "dedup_key carries the best-effort UNIQUE key");
}

fn assert_no_presence_free_text(row: &str) {
    for sentinel in [CUSTOM_STATUS_SENTINEL, DETAILS_SENTINEL, URL_SENTINEL] {
        assert!(
            !row.contains(sentinel),
            "persisted row carries ids+timestamps+enum fields only: {row}"
        );
    }
}

/// S3 AC 1a: a PRESENCE_UPDATE fixture persists user id, guild id, timestamp,
/// the status enum and the activity TYPE enum + application id only.
///
/// Pending: no presence columns exist until the S3 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
async fn event_store_s3_presence_update_persists_ids_status_and_activity_enums_only() {
    let db = TestDb::new().await;
    assert_presence_schema(&table_columns(&db.pool).await);
    assert_dedup_key_is_unique(&db.pool).await;

    let event_time = "2026-10-12T12:00:00.000Z";
    let fixture = presence_fixture(event_time, STATUS_ONLINE, game_activity());
    assert_eq!(
        fixture["status"],
        json!(STATUS_ONLINE),
        "fixture models the real gateway payload"
    );
    assert_eq!(
        fixture["activities"][0]["type"],
        json!(ACTIVITY_GAME),
        "fixture carries the activity TYPE enum"
    );
    let inserted = insert_presence_event(
        &db.pool,
        GUILD,
        USER,
        event_time,
        STATUS_ONLINE,
        Some(ACTIVITY_GAME),
        Some(APP_ID),
    )
    .await
    .expect("S3 append-only presence insert");
    assert_eq!(inserted, 1, "first delivery of a presence update inserts");

    let stored: Vec<String> = sqlx::query_scalar(
        "SELECT to_jsonb(store)::text FROM event_store AS store WHERE dedup_key = $1",
    )
    .bind(dedup_key(event_time))
    .fetch_all(&db.pool)
    .await
    .expect("stored row readable");
    assert_eq!(stored.len(), 1, "exactly one row for the presence update");
    assert_no_presence_free_text(&stored[0]);

    let enums: (String, Option<i32>, Option<String>) = sqlx::query_as(
        "SELECT presence_status, activity_type, application_id
         FROM event_store WHERE dedup_key = $1",
    )
    .bind(dedup_key(event_time))
    .fetch_one(&db.pool)
    .await
    .expect("presence enums readable");
    assert_eq!(
        enums.0, STATUS_ONLINE,
        "the status enum survives the insert"
    );
    assert_eq!(
        enums.1,
        Some(ACTIVITY_GAME),
        "the activity TYPE enum survives the insert"
    );
    assert_eq!(
        enums.2.as_deref(),
        Some(APP_ID),
        "the application id survives the insert"
    );
    db.cleanup().await;
}

/// S3 AC 1b: a PRESENCE_UPDATE fixture with no activities (offline) persists
/// ids, timestamp and the status enum with NULL activity enums.
///
/// Pending: no presence columns exist until the S3 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
async fn event_store_s3_presence_update_without_activity_persists_null_activity() {
    let db = TestDb::new().await;
    assert_presence_schema(&table_columns(&db.pool).await);
    assert_dedup_key_is_unique(&db.pool).await;

    let event_time = "2026-10-12T13:00:00.000Z";
    let fixture = presence_fixture(event_time, STATUS_OFFLINE, no_activities());
    assert_eq!(
        fixture["activities"],
        json!([]),
        "fixture models the real gateway payload"
    );
    let inserted = insert_presence_event(
        &db.pool,
        GUILD,
        USER,
        event_time,
        STATUS_OFFLINE,
        None,
        None,
    )
    .await
    .expect("S3 append-only presence insert");
    assert_eq!(inserted, 1, "first delivery of an offline presence inserts");

    let stored: Vec<String> = sqlx::query_scalar(
        "SELECT to_jsonb(store)::text FROM event_store AS store WHERE dedup_key = $1",
    )
    .bind(dedup_key(event_time))
    .fetch_all(&db.pool)
    .await
    .expect("stored row readable");
    assert_eq!(stored.len(), 1, "exactly one row for the offline presence");
    assert_no_presence_free_text(&stored[0]);

    let enums: (String, Option<i32>, Option<String>) = sqlx::query_as(
        "SELECT presence_status, activity_type, application_id
         FROM event_store WHERE dedup_key = $1",
    )
    .bind(dedup_key(event_time))
    .fetch_one(&db.pool)
    .await
    .expect("offline enums readable");
    assert_eq!(
        (enums.0.as_str(), enums.1, enums.2.as_deref()),
        (STATUS_OFFLINE, None, None),
        "an offline presence carries no activity enums"
    );
    db.cleanup().await;
}

/// S3 AC 3: duplicate delivery with the same `dedup_key` inserts once
/// (best-effort, `ON CONFLICT DO NOTHING`). Same-key retries insert nothing;
/// a resume redelivery received at a different instant carries a different
/// key (Discord's PRESENCE_UPDATE has no event timestamp) and is not
/// absorbed.
///
/// Pending: no presence columns exist until the S3 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
async fn event_store_s3_duplicate_delivery_inserts_once() {
    let db = TestDb::new().await;
    assert_presence_schema(&table_columns(&db.pool).await);

    let event_time = "2026-10-12T12:00:00.000Z";
    let first = insert_presence_event(
        &db.pool,
        GUILD,
        USER,
        event_time,
        STATUS_ONLINE,
        Some(ACTIVITY_GAME),
        Some(APP_ID),
    )
    .await
    .expect("first delivery inserts");
    assert_eq!(first, 1, "first delivery inserts");
    // Same-key retry of the same dispatch: same dedup_key.
    let retry = insert_presence_event(
        &db.pool,
        GUILD,
        USER,
        event_time,
        STATUS_ONLINE,
        Some(ACTIVITY_GAME),
        Some(APP_ID),
    )
    .await
    .expect("duplicate delivery inserts nothing");
    assert_eq!(retry, 0, "same dedup_key must insert nothing on retry");
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM event_store WHERE dedup_key = $1")
        .bind(dedup_key(event_time))
        .fetch_one(&db.pool)
        .await
        .expect("row count readable");
    assert_eq!(count.0, 1, "no double row after same-key retry");
    db.cleanup().await;
}

/// S3 AC 2 (privacy tripwire): a fixture carrying custom-status text and
/// activity details/URLs is rejected or never persisted.
///
/// Pending: no presence columns exist until the S3 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
async fn event_store_s3_privacy_tripwire_rejects_custom_status_text() {
    let db = TestDb::new().await;
    // Tripwire part 1: the schema holds the S3 ids/enums plus only reviewed
    // sibling columns, with no unreviewed json/jsonb home for a free-text
    // payload (the reviewed S2 `roles` array is the sole jsonb exception). A
    // scratch migration adding `raw_payload JSONB` or `custom_status TEXT`
    // fails here even though the lean writer below would leave those columns
    // NULL.
    assert_presence_schema(&table_columns(&db.pool).await);
    assert_no_json_columns(&db.pool).await;
    // Tripwire part 2: a fixture carrying custom-status text plus activity
    // details/URLs persists its ids and enums only; the sentinels appear in
    // no stored row.
    let event_time = "2026-10-12T14:00:00.000Z";
    let fixture = presence_fixture(event_time, STATUS_ONLINE, custom_status_activity());
    assert_eq!(
        fixture["activities"][0]["state"],
        json!(CUSTOM_STATUS_SENTINEL),
        "fixture models a payload carrying custom-status text"
    );
    insert_presence_event(
        &db.pool,
        GUILD,
        USER,
        event_time,
        STATUS_ONLINE,
        Some(ACTIVITY_CUSTOM),
        None,
    )
    .await
    .expect("metadata insert");
    let dumped: Vec<String> =
        sqlx::query_scalar("SELECT to_jsonb(store)::text FROM event_store AS store")
            .fetch_all(&db.pool)
            .await
            .expect("table dump readable");
    assert!(!dumped.is_empty(), "the metadata row persisted");
    for row in &dumped {
        assert!(
            !row.contains(CUSTOM_STATUS_SENTINEL)
                && !row.contains(DETAILS_SENTINEL)
                && !row.contains(URL_SENTINEL),
            "privacy fence: custom text, details and URLs never persist: {row}"
        );
    }
    db.cleanup().await;
}

/// S3 AC 4: presence metrics use fixed cardinality only. Presence is the
/// highest-volume family, so the capture outcome counter exposes exactly
/// three series (`outcome="inserted|duplicate|rejected"`) with no per-user
/// or per-activity labels.
///
/// Pending: the counter lands with the S3 build; run with `-- --ignored`
/// against the disposable test database lane to see the gap. This test needs
/// no database: it pins the process-local exposition contract so the insert
/// path stays lean.
#[test]
fn event_store_s3_metrics_use_fixed_cardinality_only() {
    let metrics = Metrics::default();
    metrics.gateway_event("PRESENCE_UPDATE");
    let text = metrics.render(None);
    // The S3 outcome counter exists with exactly the three fixed outcomes.
    for outcome in ["inserted", "duplicate", "rejected"] {
        let series = format!("two_bot_presence_capture_total{{outcome=\"{outcome}\"}}");
        assert!(
            text.contains(series.as_str()),
            "presence capture exposes a fixed inserted/duplicate/rejected outcome series"
        );
    }
    // Fixed cardinality: exactly three presence-capture series, one per
    // outcome, and none carries per-user or per-activity labels.
    let capture: Vec<&str> = text
        .lines()
        .filter(|line| line.starts_with("two_bot_presence_capture_total{"))
        .collect();
    assert_eq!(
        capture.len(),
        3,
        "presence capture stays at three fixed series, however many users update: {capture:?}"
    );
    for line in &capture {
        for label in ["user_id", "user=", "activity", "application"] {
            assert!(
                !line.contains(label),
                "no per-user/per-activity label on the hot path: {line}"
            );
        }
    }
    // Free text never reaches exposition, however hostile the payload.
    for sentinel in [CUSTOM_STATUS_SENTINEL, DETAILS_SENTINEL, URL_SENTINEL] {
        assert!(
            !text.contains(sentinel),
            "presence free text never reaches exposition"
        );
    }
}
