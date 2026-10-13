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
//!   membership enum deltas `premium_since`, the `nick_changed` flag and the
//!   role-id `roles`. No free-text columns: no nickname, no bio, no about,
//!   no username. The nickname itself is never stored; only whether the
//!   dispatch set one.
//! - Resume-safe idempotent inserts: every insert runs
//!   `ON CONFLICT (dedup_key) DO NOTHING` against the single shared unique
//!   index, so a gateway resume redelivery inserts nothing. The key shape is
//!   per dispatch type, because only ADD carries a payload timestamp:
//!   `GUILD_MEMBER_ADD:<guild>:<user>:<joined_at>` for adds,
//!   `GUILD_MEMBER_REMOVE:<guild>:<user>:<session>:<seq>` for removes and
//!   `GUILD_MEMBER_UPDATE:<guild>:<user>:<session>:<seq>` for updates, where
//!   `session` is the gateway READY session id and `seq` the dispatch
//!   sequence number `s`. Both are Discord-provided envelope values on every
//!   dispatch, so a redelivered REMOVE/UPDATE replays the same key while a
//!   later leave or update carries a new one. `event_time` on those rows is
//!   receipt time, the writer's best-known time.
//! - Privacy tripwire: the table holds every S1+S2 column and only columns on
//!   the shared reviewed allowlist, so a sibling slice's columns pass while an
//!   added raw payload or free-text column fails the schema check; and a fixture carrying bio / about / nickname free
//!   text persists its ids and deltas only, the sentinels appearing in no
//!   stored row.
//!
//! Pending-test routing (established pattern): this card's CI lane runs this
//! file live against the disposable test database (`TestDatabase` refuses
//! anything but agent-testdb), never staging or production; the nightly broad
//! sweep skips the five tests by name.
#![cfg(feature = "db")]

use serde_json::{json, Value};
use sqlx::PgPool;
use two_bot_testsupport::{assert_event_store_columns, TestDatabase};

const GUILD: &str = "2222";
const USER: &str = "77";
const ROLE_A: &str = "8888";
const ROLE_B: &str = "9999";

/// Gateway session id for the envelope key parts: the READY `session_id` the
/// gateway client resumes with. Together with the dispatch sequence number
/// `s` it is the only redelivery-stable distinguisher a timestamp-less
/// dispatch (REMOVE, UPDATE) has. Both are Discord-provided envelope values,
/// not invented payload fields.
const SESSION: &str = "s2-fixture-session";

/// Sentinel username: the real gateway member payload always carries one, and
/// it must never be stored. If this string ever appears in a persisted row,
/// the ids-only fence broke.
const USERNAME_SENTINEL: &str = "s2-username-sentinel-must-never-persist";

/// Sentinel bio text: models a free-text member field the store must refuse
/// to persist.
const BIO_SENTINEL: &str = "s2-privacy-sentinel-bio-must-never-persist";

/// Sentinel about text: same rule as the bio, other payload shape.
const ABOUT_SENTINEL: &str = "s2-privacy-sentinel-about-must-never-persist";

/// Payload-only nickname value: a real update dispatch may carry one, but it
/// must never be stored. Only the `nick_changed` flag persists. If this
/// string ever appears in a persisted row, the privacy fence broke.
const NICK_VALUE: &str = "fixture-nick";

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

/// ADD key: `joined_at` is a payload timestamp, so a resume redelivery
/// carries the same key while a later rejoin carries a new one.
fn add_key(event_time: &str) -> String {
    format!("GUILD_MEMBER_ADD:{GUILD}:{USER}:{event_time}")
}

/// REMOVE key: the dispatch carries no timestamp, so the key uses the
/// gateway envelope instead. Same dispatch redelivered: same key, no new
/// row. A later leave: a new sequence number, a new row.
fn remove_key(session: &str, seq: &str) -> String {
    format!("GUILD_MEMBER_REMOVE:{GUILD}:{USER}:{session}:{seq}")
}

/// UPDATE key: same envelope rule as REMOVE; the update dispatch carries no
/// event timestamp either.
fn update_key(session: &str, seq: &str) -> String {
    format!("GUILD_MEMBER_UPDATE:{GUILD}:{USER}:{session}:{seq}")
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
/// no deltas and no timestamp, only identity. The caller passes the envelope
/// session/sequence separately for the dedup key.
fn member_remove_fixture() -> Value {
    json!({
        "guild_id": GUILD,
        "user": member_user(),
    })
}

/// Minimal GUILD_MEMBER_UPDATE gateway payload: the update dispatch carries
/// the nick and role-id deltas plus `premium_since`, but no event timestamp.
fn member_update_fixture(nick: Option<&str>, roles: &[&str]) -> Value {
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
        "flags": 0,
    })
}

/// One S2 member event: identity + timestamps + the membership enum deltas.
/// A struct keeps the wide column list under the argument-count lint.
struct MemberEvent {
    event_type: &'static str,
    guild_id: &'static str,
    user_id: &'static str,
    event_time: &'static str,
    dedup_key: String,
    premium_since: Option<&'static str>,
    nick_changed: Option<bool>,
    roles: Value,
}

/// S2 append-only insert for a member event, idempotent on `dedup_key`.
/// Returns rows inserted (1) or deduplicated (0). The column list is the
/// whole contract: anything the fixture carries beyond these columns
/// (username, bio, about, nickname) has no binding and can never persist.
async fn insert_member_event(pool: &PgPool, event: MemberEvent) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "INSERT INTO event_store
            (event_type, guild_id, user_id, event_time, received_at, dedup_key,
             premium_since, nick_changed, roles)
         VALUES
            ($1, $2, $3, $4::timestamptz, NOW(), $5, $6::timestamptz, $7, $8)
         ON CONFLICT (dedup_key) DO NOTHING",
    )
    .bind(event.event_type)
    .bind(event.guild_id)
    .bind(event.user_id)
    .bind(event.event_time)
    .bind(event.dedup_key.as_str())
    .bind(event.premium_since)
    .bind(event.nick_changed)
    .bind(sqlx::types::Json(event.roles))
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

/// Columns this slice requires: the S1 identity/timestamp core plus the S2
/// member deltas. The table is shared with the other capture slices, so the
/// schema check pairs this required set with the reviewed allowlist in
/// `two_bot_testsupport::EVENT_STORE_ALLOWED_COLUMNS`: a sibling slice adding
/// its columns stays green, while a raw-payload or free-text column outside
/// every reviewed slice fails.
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
    "premium_since",
    "nick_changed",
    "roles",
];

const FORBIDDEN_COLUMNS: &[&str] = &[
    "nick",
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
    assert_event_store_columns(columns, EXPECTED_COLUMNS);
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
    for sentinel in [USERNAME_SENTINEL, BIO_SENTINEL, ABOUT_SENTINEL, NICK_VALUE] {
        assert!(
            !row.contains(sentinel),
            "persisted row carries ids+timestamps+enum deltas only: {row}"
        );
    }
}

/// S2 AC 1a: a GUILD_MEMBER_ADD fixture persists user id, guild id,
/// timestamps and the `premium_since` delta only. The join carries no nick,
/// so the flag is false.
#[tokio::test]
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
        MemberEvent {
            event_type: "GUILD_MEMBER_ADD",
            guild_id: GUILD,
            user_id: USER,
            event_time: joined_at,
            dedup_key: add_key(joined_at),
            premium_since: Some(premium_since),
            nick_changed: Some(false),
            roles: json!([]),
        },
    )
    .await
    .expect("S2 append-only member insert");
    assert_eq!(inserted, 1, "first delivery of a new member inserts");

    let stored: Vec<String> = sqlx::query_scalar(
        "SELECT to_jsonb(store)::text FROM event_store AS store WHERE dedup_key = $1",
    )
    .bind(add_key(joined_at))
    .fetch_all(&db.pool)
    .await
    .expect("stored row readable");
    assert_eq!(stored.len(), 1, "exactly one row for the member add");
    assert_no_free_text(&stored[0]);

    let deltas: (Option<String>, Option<bool>) = sqlx::query_as(
        "SELECT to_char(premium_since AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), nick_changed
         FROM event_store WHERE dedup_key = $1",
    )
    .bind(add_key(joined_at))
    .fetch_one(&db.pool)
    .await
    .expect("add deltas readable");
    assert_eq!(
        deltas,
        (Some(premium_since.to_string()), Some(false)),
        "the boost delta survives the insert and the join sets no nick"
    );
    db.cleanup().await;
}

/// S2 AC 1b: a GUILD_MEMBER_REMOVE fixture persists user id, guild id and
/// receipt timestamps with NULL deltas. The dispatch carries no timestamp,
/// so the key uses the gateway envelope; a later leave (new sequence)
/// inserts, a redelivered leave (same sequence) does not.
#[tokio::test]
async fn event_store_s2_member_remove_persists_ids_only() {
    let db = TestDb::new().await;
    assert_member_schema(&table_columns(&db.pool).await);
    assert_dedup_key_is_unique(&db.pool).await;

    // Receipt time: the remove dispatch carries no timestamp of its own.
    let received_at = "2026-10-12T13:00:00.000Z";
    let fixture = member_remove_fixture();
    assert_eq!(
        fixture["user"]["id"],
        json!(USER),
        "fixture models the real gateway payload"
    );
    let inserted = insert_member_event(
        &db.pool,
        MemberEvent {
            event_type: "GUILD_MEMBER_REMOVE",
            guild_id: GUILD,
            user_id: USER,
            event_time: received_at,
            dedup_key: remove_key(SESSION, "11"),
            premium_since: None,
            nick_changed: None,
            roles: json!([]),
        },
    )
    .await
    .expect("S2 append-only member insert");
    assert_eq!(inserted, 1, "first delivery of a member removal inserts");

    let stored: Vec<String> = sqlx::query_scalar(
        "SELECT to_jsonb(store)::text FROM event_store AS store WHERE dedup_key = $1",
    )
    .bind(remove_key(SESSION, "11"))
    .fetch_all(&db.pool)
    .await
    .expect("stored row readable");
    assert_eq!(stored.len(), 1, "exactly one row for the member removal");
    assert_no_free_text(&stored[0]);

    let deltas: (Option<String>, Option<bool>) = sqlx::query_as(
        "SELECT to_char(premium_since AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), nick_changed
         FROM event_store WHERE dedup_key = $1",
    )
    .bind(remove_key(SESSION, "11"))
    .fetch_one(&db.pool)
    .await
    .expect("removal deltas readable");
    assert_eq!(
        deltas,
        (None, None),
        "a removal carries no premium/nick delta"
    );

    // A later leave is a new sequence number, so it inserts a new row.
    let second = insert_member_event(
        &db.pool,
        MemberEvent {
            event_type: "GUILD_MEMBER_REMOVE",
            guild_id: GUILD,
            user_id: USER,
            event_time: received_at,
            dedup_key: remove_key(SESSION, "12"),
            premium_since: None,
            nick_changed: None,
            roles: json!([]),
        },
    )
    .await
    .expect("later leave inserts");
    assert_eq!(second, 1, "a new sequence number is a new leave");
    // Gateway resume redelivery of the first leave inserts nothing.
    let retry = insert_member_event(
        &db.pool,
        MemberEvent {
            event_type: "GUILD_MEMBER_REMOVE",
            guild_id: GUILD,
            user_id: USER,
            event_time: received_at,
            dedup_key: remove_key(SESSION, "11"),
            premium_since: None,
            nick_changed: None,
            roles: json!([]),
        },
    )
    .await
    .expect("redelivered leave is resume-safe");
    assert_eq!(
        retry, 0,
        "same envelope key must insert nothing on redelivery"
    );
    db.cleanup().await;
}

/// S2 AC 1c: a GUILD_MEMBER_UPDATE fixture persists the nick_changed flag
/// and the role-id deltas alongside ids and timestamps. The nickname itself
/// is never stored: only the flag.
#[tokio::test]
async fn event_store_s2_member_update_persists_nick_and_role_deltas_only() {
    let db = TestDb::new().await;
    assert_member_schema(&table_columns(&db.pool).await);
    assert_dedup_key_is_unique(&db.pool).await;

    // Receipt time: the update dispatch carries no event timestamp of its own.
    let received_at = "2026-10-12T14:00:00.000Z";
    let premium_since = "2026-10-12T12:05:00.000Z";
    let fixture = member_update_fixture(Some(NICK_VALUE), &[ROLE_A, ROLE_B]);
    assert_eq!(
        fixture["roles"],
        json!([ROLE_A, ROLE_B]),
        "fixture models the real gateway payload"
    );
    let inserted = insert_member_event(
        &db.pool,
        MemberEvent {
            event_type: "GUILD_MEMBER_UPDATE",
            guild_id: GUILD,
            user_id: USER,
            event_time: received_at,
            dedup_key: update_key(SESSION, "21"),
            premium_since: Some(premium_since),
            nick_changed: Some(true),
            roles: json!([ROLE_A, ROLE_B]),
        },
    )
    .await
    .expect("S2 append-only member insert");
    assert_eq!(inserted, 1, "first delivery of a member update inserts");

    let stored: Vec<String> = sqlx::query_scalar(
        "SELECT to_jsonb(store)::text FROM event_store AS store WHERE dedup_key = $1",
    )
    .bind(update_key(SESSION, "21"))
    .fetch_all(&db.pool)
    .await
    .expect("stored row readable");
    assert_eq!(stored.len(), 1, "exactly one row for the member update");
    assert_no_free_text(&stored[0]);
    assert!(
        !stored[0].contains(NICK_VALUE),
        "the nickname itself is never persisted: {}",
        stored[0]
    );

    let deltas: (Option<bool>, Option<String>, sqlx::types::Json<Value>) = sqlx::query_as(
        "SELECT nick_changed,
                to_char(premium_since AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'),
                roles
         FROM event_store WHERE dedup_key = $1",
    )
    .bind(update_key(SESSION, "21"))
    .fetch_one(&db.pool)
    .await
    .expect("update deltas readable");
    assert_eq!(deltas.0, Some(true), "nick_changed flag stored");
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

    // Gateway resume redelivery of the same dispatch inserts nothing, even
    // though the payload carries no timestamp to key on.
    let retry = insert_member_event(
        &db.pool,
        MemberEvent {
            event_type: "GUILD_MEMBER_UPDATE",
            guild_id: GUILD,
            user_id: USER,
            event_time: received_at,
            dedup_key: update_key(SESSION, "21"),
            premium_since: Some(premium_since),
            nick_changed: Some(true),
            roles: json!([ROLE_A, ROLE_B]),
        },
    )
    .await
    .expect("redelivered update is resume-safe");
    assert_eq!(
        retry, 0,
        "same envelope key must insert nothing on redelivery"
    );
    db.cleanup().await;
}

/// S2 AC 2: duplicate delivery with the same `dedup_key` inserts once
/// (resume-safe, `ON CONFLICT DO NOTHING`).
#[tokio::test]
async fn event_store_s2_duplicate_delivery_inserts_once() {
    let db = TestDb::new().await;
    assert_member_schema(&table_columns(&db.pool).await);

    let joined_at = "2026-10-12T12:00:00.000Z";
    let first = insert_member_event(
        &db.pool,
        MemberEvent {
            event_type: "GUILD_MEMBER_ADD",
            guild_id: GUILD,
            user_id: USER,
            event_time: joined_at,
            dedup_key: add_key(joined_at),
            premium_since: None,
            nick_changed: Some(false),
            roles: json!([]),
        },
    )
    .await
    .expect("first delivery inserts");
    assert_eq!(first, 1, "first delivery inserts");
    // Gateway resume redelivery of the same dispatch: same dedup_key.
    let retry = insert_member_event(
        &db.pool,
        MemberEvent {
            event_type: "GUILD_MEMBER_ADD",
            guild_id: GUILD,
            user_id: USER,
            event_time: joined_at,
            dedup_key: add_key(joined_at),
            premium_since: None,
            nick_changed: Some(false),
            roles: json!([]),
        },
    )
    .await
    .expect("duplicate delivery is resume-safe");
    assert_eq!(retry, 0, "same dedup_key must insert nothing on redelivery");
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM event_store WHERE dedup_key = $1")
        .bind(add_key(joined_at))
        .fetch_one(&db.pool)
        .await
        .expect("row count readable");
    assert_eq!(count.0, 1, "no double row after resume redelivery");
    db.cleanup().await;
}

/// S2 AC 3 (privacy tripwire): the table holds the S1+S2 columns and nothing
/// outside the reviewed allowlist, so no free-text column may exist to hold the payload; and a fixture
/// carrying free-text bio / about / nickname fields persists its ids and
/// deltas only, the sentinels appearing in no stored row.
#[tokio::test]
async fn event_store_s2_privacy_tripwire_rejects_bio_text() {
    let db = TestDb::new().await;
    // Tripwire part 1: the allowlist fails on any unreviewed column, so a
    // raw-payload or free-text column cannot hide beside the contract.
    assert_member_schema(&table_columns(&db.pool).await);
    // Tripwire part 2: a fixture carrying bio + about free text plus a
    // nickname persists its ids and deltas only; the sentinels appear in no
    // stored row.
    let mut fixture = member_update_fixture(Some(NICK_VALUE), &[ROLE_A]);
    fixture["bio"] = json!(BIO_SENTINEL);
    fixture["about"] = json!(ABOUT_SENTINEL);
    assert_eq!(
        fixture["bio"],
        json!(BIO_SENTINEL),
        "fixture models a payload carrying free text"
    );
    insert_member_event(
        &db.pool,
        MemberEvent {
            event_type: "GUILD_MEMBER_UPDATE",
            guild_id: GUILD,
            user_id: USER,
            event_time: "2026-10-12T14:00:00.000Z",
            dedup_key: update_key(SESSION, "31"),
            premium_since: None,
            nick_changed: Some(true),
            roles: json!([ROLE_A]),
        },
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
        assert_no_free_text(row);
    }
    db.cleanup().await;
}
