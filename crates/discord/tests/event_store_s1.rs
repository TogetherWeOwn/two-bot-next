//! Event-store S1 acceptance tests for the append-only core slice
//! (0437 migration on the shared append-only `event_store` table).
//!
//! Contract under test (event-store P1 slice 1: append-only core +
//! message-create metadata + idempotent keys + lag metric):
//!
//! - `event_store` table, append-only, with at least these columns: `id`,
//!   `event_type`, `guild_id`, `channel_id`, `message_id`, `user_id`,
//!   `event_time`, `received_at`, `dedup_key`. A column outside the shared
//!   reviewed allowlist (a raw payload or free-text column included) fails
//!   the schema check; sibling slices' reviewed columns pass.
//!   `dedup_key` is UNIQUE.
//! - Resume-safe idempotent inserts: `dedup_key = "MESSAGE_CREATE:<id>"`,
//!   `ON CONFLICT (dedup_key) DO NOTHING`, first-wins on replay. The insert
//!   helpers bind ids only; live gateway dispatch wiring stays with the
//!   parent event-store scope.
//! - Out-of-order / replayed MESSAGE_CREATE fixtures never add a row.
//! - Consumer lag gauge `two_bot_event_store_consumer_lag_seconds` in the
//!   `/ops/metrics` exposition with fixed cardinality (no unbounded labels).
//! - Privacy tripwire: a fixture carrying a body / custom-status text leaves
//!   no such text persisted anywhere in the table.
//!
//! Routing: these tests run in the `ignored db (stores)` CI lane and are
//! skipped in the nightly broad sweep to avoid a double run. DB tests run
//! only against the disposable test database (`TestDatabase` refuses
//! anything but agent-testdb), never staging or production.
#![cfg(feature = "db")]

use serde_json::{json, Value};
use sqlx::PgPool;
use two_bot_testsupport::{assert_event_store_columns, TestDatabase};

const GUILD: &str = "2222";
const CHANNEL_ID: &str = "4444";
const AUTHOR_ID: &str = "77";

/// Sentinel body text: real gateway payload words that must never be stored.
/// If this string ever appears in a persisted row, the privacy fence broke.
const BODY_SENTINEL: &str = "s1-privacy-sentinel-body-must-never-persist";

/// Sentinel custom-status text: same rule as the body, other payload shape.
const STATUS_SENTINEL: &str = "s1-privacy-sentinel-status-must-never-persist";

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

fn dedup_key(message_id: &str) -> String {
    format!("MESSAGE_CREATE:{message_id}")
}

/// Minimal MESSAGE_CREATE gateway payload: ids + timestamps + enums only,
/// except the two sentinel free-text fields, which model what the real
/// gateway delivers and what the store must refuse to persist.
fn message_create_fixture(message_id: &str, clock: &str) -> Value {
    json!({
        "id": message_id,
        "guild_id": GUILD,
        "channel_id": CHANNEL_ID,
        "author": {"id": AUTHOR_ID, "username": "fixture-member", "discriminator": "0", "bot": false},
        "timestamp": format!("{clock}+00:00"),
        "type": 0,
        "content": BODY_SENTINEL,
        "custom_status_text": STATUS_SENTINEL,
    })
}

/// S1 append-only insert: metadata columns only, idempotent on `dedup_key`.
/// Returns rows inserted (1) or deduplicated (0).
async fn insert_message_metadata(
    pool: &PgPool,
    fixture: &Value,
    event_time: &str,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO event_store
            (event_type, guild_id, channel_id, message_id, user_id,
             event_time, received_at, dedup_key)
         VALUES
            ('MESSAGE_CREATE', $1, $2, $3, $4, $5::timestamptz, NOW(), $6)
         ON CONFLICT (dedup_key) DO NOTHING",
    )
    .bind(fixture["guild_id"].as_str().expect("fixture guild_id"))
    .bind(fixture["channel_id"].as_str().expect("fixture channel_id"))
    .bind(fixture["id"].as_str().expect("fixture message id"))
    .bind(fixture["author"]["id"].as_str().expect("fixture author id"))
    .bind(event_time)
    .bind(dedup_key(
        fixture["id"].as_str().expect("fixture message id"),
    ))
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

/// Required S1 columns: `id` plus the eight metadata columns. The table is
/// shared with the other capture slices, so the check also requires every
/// column to be on the reviewed allowlist in
/// `two_bot_testsupport::EVENT_STORE_ALLOWED_COLUMNS`: a raw-payload or
/// free-text column fails, while a sibling slice's reviewed columns pass.
const EXPECTED_COLUMNS: &[&str] = &[
    "id",
    "event_type",
    "guild_id",
    "channel_id",
    "message_id",
    "user_id",
    "event_time",
    "received_at",
    "dedup_key",
];

const FORBIDDEN_COLUMNS: &[&str] = &[
    "body",
    "content",
    "message_body",
    "message_content",
    "custom_status_text",
    "status_text",
];

fn assert_minimal_schema(columns: &[String]) {
    assert!(
        !columns.is_empty(),
        "S1 creates the append-only event_store table"
    );
    assert_event_store_columns(columns, EXPECTED_COLUMNS);
    for forbidden in FORBIDDEN_COLUMNS {
        assert!(
            !columns.iter().any(|column| column == forbidden),
            "event_store must not gain a {forbidden} column; has {columns:?}"
        );
    }
}

/// No `json`/`jsonb` column may exist except the reviewed `roles` role-id
/// array (S2): free-text payloads (message bodies, raw payloads) have no
/// schema home whatever the column name.
async fn assert_no_json_columns(pool: &PgPool) {
    for (column, data_type) in table_column_types(pool).await {
        if column == "roles" {
            continue;
        }
        assert!(
            data_type != "json" && data_type != "jsonb",
            "event_store column {column} has free-text type {data_type}; \
             message capture persists ids+timestamps only"
        );
    }
}

/// S1 AC 1: append-only insert with minimal fields (ids + timestamps +
/// enums), and no body / custom-text column or field anywhere.
///
/// Runs in the `ignored db (stores)` CI lane against the disposable test database.
#[tokio::test]
async fn event_store_s1_append_only_insert_uses_minimal_fields() {
    let db = TestDb::new().await;
    assert_minimal_schema(&table_columns(&db.pool).await);
    let unique: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM pg_indexes
         WHERE tablename = 'event_store'
           AND indexdef LIKE '%UNIQUE%'
           AND indexdef LIKE '%dedup_key%'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("dedup_key index readable");
    assert_eq!(unique.0, 1, "dedup_key carries the resume-safe UNIQUE key");

    let fixture = message_create_fixture("3333", "2026-10-12T12:00:00.000Z");
    let inserted = insert_message_metadata(&db.pool, &fixture, "2026-10-12T12:00:00.000Z")
        .await
        .expect("S1 append-only insert");
    assert_eq!(inserted, 1, "first delivery of a new message inserts");
    let stored: Vec<String> = sqlx::query_scalar(
        "SELECT to_jsonb(store)::text FROM event_store AS store WHERE dedup_key = $1",
    )
    .bind(dedup_key("3333"))
    .fetch_all(&db.pool)
    .await
    .expect("stored row readable");
    assert_eq!(stored.len(), 1, "exactly one row for the message");
    assert!(
        !stored[0].contains(BODY_SENTINEL) && !stored[0].contains(STATUS_SENTINEL),
        "persisted row carries ids+timestamps only: {}",
        stored[0]
    );
    db.cleanup().await;
}

/// S1 AC 2: duplicate delivery with the same `dedup_key` inserts once
/// (resume-safe, `ON CONFLICT DO NOTHING`).
///
/// Runs in the `ignored db (stores)` CI lane against the disposable test database.
#[tokio::test]
async fn event_store_s1_duplicate_delivery_inserts_once() {
    let db = TestDb::new().await;
    let fixture = message_create_fixture("dup-1", "2026-10-12T12:00:00.000Z");
    let first = insert_message_metadata(&db.pool, &fixture, "2026-10-12T12:00:00.000Z")
        .await
        .expect("first delivery inserts");
    assert_eq!(first, 1, "first delivery inserts");
    // Gateway resume redelivery of the same dispatch: same dedup_key.
    let retry = insert_message_metadata(&db.pool, &fixture, "2026-10-12T12:00:00.000Z")
        .await
        .expect("duplicate delivery is resume-safe");
    assert_eq!(retry, 0, "same dedup_key must insert nothing on redelivery");
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM event_store WHERE dedup_key = $1")
        .bind(dedup_key("dup-1"))
        .fetch_one(&db.pool)
        .await
        .expect("row count readable");
    assert_eq!(count.0, 1, "no double row after resume redelivery");
    db.cleanup().await;
}

/// S1 AC 3: an out-of-order / replayed MESSAGE_CREATE fixture does not
/// duplicate (first-wins on the idempotent key).
///
/// Runs in the `ignored db (stores)` CI lane against the disposable test database.
#[tokio::test]
async fn event_store_s1_replayed_message_create_does_not_duplicate() {
    let db = TestDb::new().await;
    // Original delivery wins; a late replay carrying an earlier event_time
    // (out-of-order arrival) must not add or move the row.
    let original = message_create_fixture("replay-1", "2026-10-12T12:00:00.000Z");
    let first = insert_message_metadata(&db.pool, &original, "2026-10-12T12:00:00.000Z")
        .await
        .expect("original delivery inserts");
    assert_eq!(first, 1, "original delivery inserts");
    let replay = message_create_fixture("replay-1", "2026-10-12T11:59:00.000Z");
    let second = insert_message_metadata(&db.pool, &replay, "2026-10-12T11:59:00.000Z")
        .await
        .expect("replay is idempotent");
    assert_eq!(second, 0, "replayed MESSAGE_CREATE inserts nothing");
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT to_char(event_time AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')
         FROM event_store WHERE dedup_key = $1",
    )
    .bind(dedup_key("replay-1"))
    .fetch_all(&db.pool)
    .await
    .expect("surviving row readable");
    assert_eq!(rows.len(), 1, "exactly one row survives the replay");
    assert_eq!(
        rows[0].0, "2026-10-12T12:00:00.000Z",
        "first-wins: the replay must not move the original timestamp"
    );
    db.cleanup().await;
}

/// S1 AC 4: the consumer lag gauge appears in the `/ops/metrics` exposition
/// with fixed cardinality (no unbounded labels).
///
/// Runs in the `ignored db (stores)` CI lane against the disposable test database.
#[test]
fn event_store_s1_consumer_lag_gauge_has_fixed_cardinality() {
    let name = "two_bot_event_store_consumer_lag_seconds";
    let text = two_bot_core::metrics::Metrics::default().render(None);
    assert!(
        text.contains(&format!("# TYPE {name} gauge\n")),
        "S1 exports the consumer lag gauge in the /ops/metrics exposition"
    );
    let samples: Vec<&str> = text
        .lines()
        .filter(|line| line.starts_with(name) && !line.starts_with('#'))
        .collect();
    assert!(
        !samples.is_empty(),
        "lag gauge exposes at least one sample series"
    );
    assert!(
        samples.len() <= 8,
        "lag gauge stays fixed-cardinality, got {} series",
        samples.len()
    );
    for sample in &samples {
        for unbounded in [
            "message_id",
            "channel_id",
            "user_id",
            "dedup_key",
            "content",
            "body",
        ] {
            assert!(
                !sample.contains(unbounded),
                "lag gauge must not label by {unbounded}: {sample}"
            );
        }
        let value: f64 = sample
            .rsplit(' ')
            .next()
            .expect("gauge sample carries a value")
            .parse()
            .expect("gauge value parses");
        assert!(
            value.is_finite() && value >= 0.0,
            "lag is a finite non-negative duration: {sample}"
        );
    }
}

/// S1 AC 5 (privacy tripwire): a fixture containing a message body /
/// custom-status text is rejected or never persisted.
///
/// Runs in the `ignored db (stores)` CI lane against the disposable test database.
#[tokio::test]
async fn event_store_s1_privacy_tripwire_rejects_body_text() {
    let db = TestDb::new().await;
    // Tripwire part 1: the table holds the S1 columns and only reviewed
    // allowlist columns, so no free-text or raw-payload column exists to hold the sentinels.
    let columns = table_columns(&db.pool).await;
    assert!(
        !columns.is_empty(),
        "S1 creates the append-only event_store table"
    );
    assert_minimal_schema(&columns);
    assert_no_json_columns(&db.pool).await;
    // Tripwire part 2: a fixture carrying body + custom-status text persists
    // its ids only; the sentinels appear in no stored row.
    let fixture = message_create_fixture("trip-1", "2026-10-12T12:00:00.000Z");
    assert!(
        fixture["content"] == json!(BODY_SENTINEL),
        "fixture models the real gateway payload"
    );
    insert_message_metadata(&db.pool, &fixture, "2026-10-12T12:00:00.000Z")
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
            !row.contains(BODY_SENTINEL) && !row.contains(STATUS_SENTINEL),
            "privacy fence: free text never persisted: {row}"
        );
    }
    db.cleanup().await;
}
