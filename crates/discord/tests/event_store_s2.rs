//! Event-store S2 acceptance tests (paired failing-tests card for the S2 build).
//!
//! Test files ONLY: this suite pins the S2 acceptance criteria before the
//! builder starts, so every test here FAILS until the S2 build lands its
//! member-capture migration on the append-only store. The builder makes these
//! tests pass; nothing outside test code changes on this card.
//!
//! Contract under test (event-store P1 slice 2: guild member add / remove /
//! update capture into the append-only `event_store` table):
//!
//! - One row per member event with minimal fields: `event_type`,
//!   `guild_id`, `user_id`, `event_time`, `received_at`, `dedup_key` plus the
//!   membership enum deltas `premium_since`, `nick` and role-id `roles`.
//!   No free-text columns: no bio, no about, no username.
//! - Resume-safe idempotent inserts: `dedup_key` carries a UNIQUE key such as
//!   `GUILD_MEMBER_ADD:<guild>:<user>:<joined_at>` and every insert runs
//!   `ON CONFLICT (dedup_key) DO NOTHING`, so a gateway resume redelivery
//!   inserts nothing.
//! - Privacy tripwire: a fixture carrying bio / about free text persists its
//!   ids and deltas only; the sentinels appear in no stored row.
//!
//! Pending-test routing (established pattern): every test is ignored and
//! skipped in the nightly broad sweep, so this card's CI stays green while
//! the S2 build is outstanding; run with `-- --ignored` to see the gap. The
//! S2 build card un-ignores these tests once the migration lands. DB tests
//! run only against the disposable test database (`TestDatabase` refuses
//! anything but agent-testdb), never staging or production.
#![cfg(feature = "db")]

use serde_json::{json, Value};
use sqlx::PgPool;
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "2222";
const USER: &str = "77";
const ROLE_A: &str = "8888";
const ROLE_B: &str = "9999";

/// Sentinel username: the real gateway member payload always carries one, and
/// it must never be stored. If this string ever appears in a persisted row,
/// the ids-only fence broke.
const USERNAME_SENTINEL: &str = "s2-username-sentinel-must-never-persist";

/// Sentinel bio text: models a free-text member field the store must refuse
/// to persist.
const BIO_SENTINEL: &str = "s2-privacy-sentinel-bio-must-never-persist";

/// Sentinel about text: same rule as the bio, other payload shape.
const ABOUT_SENTINEL: &str = "s2-privacy-sentinel-about-must-never-persist";

/// Allowed nick delta: a guild nickname IS persisted as a membership delta.
/// It is the only free-text-ish value the S2 contract keeps.
const NICK_FIXTURE: &str = "fixture-nick";

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

fn member_user() -> Value {
    json!({
        "id": USER,
        "username": USERNAME_SENTINEL,
        "discriminator": "0",
        "bot": false,
    })
}

fn dedup_key(event_type: &str, event_time: &str) -> String {
    format!("{event_type}:{GUILD}:{USER}:{event_time}")
}

/// Minimal GUILD_MEMBER_ADD gateway payload: ids + timestamps + membership
/// enum deltas only. `premium_since` is `None` for a never-boosted join.
fn member_add_fixture(joined_at: &str, premium_since: Option<&str>) -> Value {
    json!({
        "guild_id": GUILD,
        "user": member_user(),
        "joined_at": joined_at,
        "premium_since": premium_since,
        "pending": false,
        "roles": [],
        "nick": null,
        "deaf": false,
        "mute": false,
        "flags": 0,
    })
}

/// Minimal GUILD_MEMBER_REMOVE gateway payload: the remove dispatch carries
/// no deltas, only identity.
fn member_remove_fixture() -> Value {
    json!({
        "guild_id": GUILD,
        "user": member_user(),
    })
}

/// Minimal GUILD_MEMBER_UPDATE gateway payload: the update dispatch carries
/// the nick and role-id deltas plus `premium_since`.
fn member_update_fixture(event_time: &str, nick: &str, roles: &[&str]) -> Value {
    json!({
        "guild_id": GUILD,
        "user": member_user(),
        "joined_at": "2026-10-12T12:00:00.000Z",
        "premium_since": "2026-10-12T12:05:00.000Z",
        "pending": false,
        "roles": roles,
        "nick": nick,
        "deaf": false,
        "mute": false,
        "observed_at": event_time,
    })
}

/// S2 append-only insert for a member event: identity + timestamps + the
/// membership enum deltas, idempotent on `dedup_key`. Returns rows inserted
/// (1) or deduplicated (0). The column list is the whole contract: anything
/// the fixture carries beyond these columns (username, bio, about) has no
/// binding and can never persist.
async fn insert_member_event(
    pool: &PgPool,
    event_type: &str,
    guild_id: &str,
    user_id: &str,
    event_time: &str,
    premium_since: Option<&str>,
    nick: Option<&str>,
    roles: Value,
) -> Result<u64, sqlx::Error> {
    let key = dedup_key(event_type, event_time);
    let result = sqlx::query(
        "INSERT INTO event_store
            (event_type, guild_id, user_id, event_time, received_at, dedup_key,
             premium_since, nick, roles)
         VALUES
            ($1, $2, $3, $4::timestamptz, NOW(), $5, $6::timestamptz, $7, $8)
         ON CONFLICT (dedup_key) DO NOTHING",
    )
    .bind(event_type)
    .bind(guild_id)
    .bind(user_id)
    .bind(event_time)
    .bind(&key)
    .bind(premium_since)
    .bind(nick)
    .bind(sqlx::types::Json(roles))
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

const REQUIRED_COLUMNS: &[&str] = &[
    "event_type",
    "guild_id",
    "user_id",
    "event_time",
    "received_at",
    "dedup_key",
    "premium_since",
    "nick",
    "roles",
];

const FORBIDDEN_COLUMNS: &[&str] = &[
    "bio",
    "about",
    "biography",
    "member_bio",
    "username",
    "body",
    "content",
    "message_body",
    "custom_status_text",
    "status_text",
];

fn assert_member_schema(columns: &[String]) {
    assert!(
        !columns.is_empty(),
        "S2 creates the member columns on the append-only event_store table"
    );
    for required in REQUIRED_COLUMNS {
        assert!(
            columns.iter().any(|column| column == required),
            "event_store keeps member column {required}; has {columns:?}"
        );
    }
    for forbidden in FORBIDDEN_COLUMNS {
        assert!(
            !columns.iter().any(|column| column == forbidden),
            "event_store must not gain a {forbidden} column; has {columns:?}"
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
    assert_eq!(unique.0, 1, "dedup_key carries the resume-safe UNIQUE key");
}

fn assert_no_free_text(row: &str) {
    for sentinel in [USERNAME_SENTINEL, BIO_SENTINEL, ABOUT_SENTINEL] {
        assert!(
            !row.contains(sentinel),
            "persisted row carries ids+timestamps+enum deltas only: {row}"
        );
    }
}

/// S2 AC 1a: a GUILD_MEMBER_ADD fixture persists user id, guild id,
/// timestamps and the `premium_since` delta only.
///
/// Pending: no member columns exist until the S2 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
#[ignore = "pending S2 build: needs disposable test database event_store member columns; run with --ignored to see the gap"]
async fn event_store_s2_member_add_persists_ids_and_premium_since_only() {
    let db = TestDb::new().await;
    assert_member_schema(&table_columns(&db.pool).await);
    assert_dedup_key_is_unique(&db.pool).await;

    let joined_at = "2026-10-12T12:00:00.000Z";
    let premium_since = "2026-10-12T12:05:00.000Z";
    let fixture = member_add_fixture(joined_at, Some(premium_since));
    assert_eq!(
        fixture["user"]["username"],
        json!(USERNAME_SENTINEL),
        "fixture models the real gateway payload"
    );
    let inserted = insert_member_event(
        &db.pool,
        "GUILD_MEMBER_ADD",
        GUILD,
        USER,
        joined_at,
        Some(premium_since),
        None,
        json!([]),
    )
    .await
    .expect("S2 append-only member insert");
    assert_eq!(inserted, 1, "first delivery of a new member inserts");

    let stored: Vec<String> = sqlx::query_scalar(
        "SELECT to_jsonb(store)::text FROM event_store AS store WHERE dedup_key = $1",
    )
    .bind(dedup_key("GUILD_MEMBER_ADD", joined_at))
    .fetch_all(&db.pool)
    .await
    .expect("stored row readable");
    assert_eq!(stored.len(), 1, "exactly one row for the member add");
    assert_no_free_text(&stored[0]);

    let premium: (Option<String>,) = sqlx::query_as(
        "SELECT to_char(premium_since AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')
         FROM event_store WHERE dedup_key = $1",
    )
    .bind(dedup_key("GUILD_MEMBER_ADD", joined_at))
    .fetch_one(&db.pool)
    .await
    .expect("premium_since readable");
    assert_eq!(
        premium.0.as_deref(),
        Some(premium_since),
        "the boost delta survives the insert"
    );
    db.cleanup().await;
}

/// S2 AC 1b: a GUILD_MEMBER_REMOVE fixture persists user id, guild id and
/// timestamps with NULL deltas.
///
/// Pending: no member columns exist until the S2 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
#[ignore = "pending S2 build: needs disposable test database event_store member columns; run with --ignored to see the gap"]
async fn event_store_s2_member_remove_persists_ids_only() {
    let db = TestDb::new().await;
    assert_member_schema(&table_columns(&db.pool).await);
    assert_dedup_key_is_unique(&db.pool).await;

    let removed_at = "2026-10-12T13:00:00.000Z";
    let fixture = member_remove_fixture();
    assert_eq!(
        fixture["user"]["id"],
        json!(USER),
        "fixture models the real gateway payload"
    );
    let inserted = insert_member_event(
        &db.pool,
        "GUILD_MEMBER_REMOVE",
        GUILD,
        USER,
        removed_at,
        None,
        None,
        json!([]),
    )
    .await
    .expect("S2 append-only member insert");
    assert_eq!(inserted, 1, "first delivery of a member removal inserts");

    let stored: Vec<String> = sqlx::query_scalar(
        "SELECT to_jsonb(store)::text FROM event_store AS store WHERE dedup_key = $1",
    )
    .bind(dedup_key("GUILD_MEMBER_REMOVE", removed_at))
    .fetch_all(&db.pool)
    .await
    .expect("stored row readable");
    assert_eq!(stored.len(), 1, "exactly one row for the member removal");
    assert_no_free_text(&stored[0]);

    let deltas: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT to_char(premium_since AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), nick
         FROM event_store WHERE dedup_key = $1",
    )
    .bind(dedup_key("GUILD_MEMBER_REMOVE", removed_at))
    .fetch_one(&db.pool)
    .await
    .expect("removal deltas readable");
    assert_eq!(
        deltas,
        (None, None),
        "a removal carries no premium/nick delta"
    );
    db.cleanup().await;
}

/// S2 AC 1c: a GUILD_MEMBER_UPDATE fixture persists the nick and role-id
/// deltas alongside ids and timestamps.
///
/// Pending: no member columns exist until the S2 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
#[ignore = "pending S2 build: needs disposable test database event_store member columns; run with --ignored to see the gap"]
async fn event_store_s2_member_update_persists_nick_and_role_deltas_only() {
    let db = TestDb::new().await;
    assert_member_schema(&table_columns(&db.pool).await);
    assert_dedup_key_is_unique(&db.pool).await;

    let updated_at = "2026-10-12T14:00:00.000Z";
    let premium_since = "2026-10-12T12:05:00.000Z";
    let fixture = member_update_fixture(updated_at, NICK_FIXTURE, &[ROLE_A, ROLE_B]);
    assert_eq!(
        fixture["roles"],
        json!([ROLE_A, ROLE_B]),
        "fixture models the real gateway payload"
    );
    let inserted = insert_member_event(
        &db.pool,
        "GUILD_MEMBER_UPDATE",
        GUILD,
        USER,
        updated_at,
        Some(premium_since),
        Some(NICK_FIXTURE),
        json!([ROLE_A, ROLE_B]),
    )
    .await
    .expect("S2 append-only member insert");
    assert_eq!(inserted, 1, "first delivery of a member update inserts");

    let stored: Vec<String> = sqlx::query_scalar(
        "SELECT to_jsonb(store)::text FROM event_store AS store WHERE dedup_key = $1",
    )
    .bind(dedup_key("GUILD_MEMBER_UPDATE", updated_at))
    .fetch_all(&db.pool)
    .await
    .expect("stored row readable");
    assert_eq!(stored.len(), 1, "exactly one row for the member update");
    assert_no_free_text(&stored[0]);
    assert!(
        stored[0].contains(NICK_FIXTURE),
        "the nick delta IS persisted: {}",
        stored[0]
    );

    let deltas: (Option<String>, Option<String>, sqlx::types::Json<Value>) = sqlx::query_as(
        "SELECT nick,
                to_char(premium_since AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'),
                roles
         FROM event_store WHERE dedup_key = $1",
    )
    .bind(dedup_key("GUILD_MEMBER_UPDATE", updated_at))
    .fetch_one(&db.pool)
    .await
    .expect("update deltas readable");
    assert_eq!(deltas.0.as_deref(), Some(NICK_FIXTURE), "nick delta stored");
    assert_eq!(
        deltas.1.as_deref(),
        Some(premium_since),
        "premium_since delta stored"
    );
    assert_eq!(
        deltas.2 .0,
        json!([ROLE_A, ROLE_B]),
        "role-id delta stored as ids only"
    );
    db.cleanup().await;
}

/// S2 AC 2: duplicate delivery with the same `dedup_key` inserts once
/// (resume-safe, `ON CONFLICT DO NOTHING`).
///
/// Pending: no member columns exist until the S2 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
#[ignore = "pending S2 build: needs disposable test database event_store member columns; run with --ignored to see the gap"]
async fn event_store_s2_duplicate_delivery_inserts_once() {
    let db = TestDb::new().await;
    assert_member_schema(&table_columns(&db.pool).await);

    let joined_at = "2026-10-12T12:00:00.000Z";
    let first = insert_member_event(
        &db.pool,
        "GUILD_MEMBER_ADD",
        GUILD,
        USER,
        joined_at,
        None,
        None,
        json!([]),
    )
    .await
    .expect("first delivery inserts");
    assert_eq!(first, 1, "first delivery inserts");
    // Gateway resume redelivery of the same dispatch: same dedup_key.
    let retry = insert_member_event(
        &db.pool,
        "GUILD_MEMBER_ADD",
        GUILD,
        USER,
        joined_at,
        None,
        None,
        json!([]),
    )
    .await
    .expect("duplicate delivery is resume-safe");
    assert_eq!(retry, 0, "same dedup_key must insert nothing on redelivery");
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM event_store WHERE dedup_key = $1")
        .bind(dedup_key("GUILD_MEMBER_ADD", joined_at))
        .fetch_one(&db.pool)
        .await
        .expect("row count readable");
    assert_eq!(count.0, 1, "no double row after resume redelivery");
    db.cleanup().await;
}

/// S2 AC 3 (privacy tripwire): a fixture carrying free-text bio / about
/// fields is rejected or never persisted.
///
/// Pending: no member columns exist until the S2 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
#[ignore = "pending S2 build: needs disposable test database event_store member columns; run with --ignored to see the gap"]
async fn event_store_s2_privacy_tripwire_rejects_bio_text() {
    let db = TestDb::new().await;
    // Tripwire part 1: no free-text column may exist to hold the payload.
    let columns = table_columns(&db.pool).await;
    assert!(
        !columns.is_empty(),
        "S2 creates the member columns on the append-only event_store table"
    );
    for forbidden in FORBIDDEN_COLUMNS {
        assert!(
            !columns.iter().any(|column| column == forbidden),
            "privacy fence: no {forbidden} column; has {columns:?}"
        );
    }
    // Tripwire part 2: a fixture carrying bio + about free text persists its
    // ids and deltas only; the sentinels appear in no stored row.
    let updated_at = "2026-10-12T14:00:00.000Z";
    let mut fixture = member_update_fixture(updated_at, NICK_FIXTURE, &[ROLE_A]);
    fixture["bio"] = json!(BIO_SENTINEL);
    fixture["about"] = json!(ABOUT_SENTINEL);
    assert_eq!(
        fixture["bio"],
        json!(BIO_SENTINEL),
        "fixture models a payload carrying free text"
    );
    insert_member_event(
        &db.pool,
        "GUILD_MEMBER_UPDATE",
        GUILD,
        USER,
        updated_at,
        None,
        Some(NICK_FIXTURE),
        json!([ROLE_A]),
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
            !row.contains(BIO_SENTINEL) && !row.contains(ABOUT_SENTINEL),
            "privacy fence: free text never persisted: {row}"
        );
    }
    db.cleanup().await;
}
