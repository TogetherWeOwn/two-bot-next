//! Event-store S7 acceptance tests (paired failing-tests card for the S7 build).
//!
//! Test files ONLY: this suite pins the S7 acceptance criteria before the
//! builder starts, so every test here FAILS until the S7 build lands its
//! invite/scheduled-event-capture migration on the append-only store. The
//! builder makes these tests pass; nothing outside test code changes on this
//! card.
//!
//! Contract under test (event-store P1 slice 7: INVITE_CREATE / INVITE_DELETE
//! plus GUILD_SCHEDULED_EVENT_CREATE / UPDATE / DELETE plus
//! GUILD_SCHEDULED_EVENT_USER_ADD / REMOVE minimal capture into the
//! append-only `event_store` table):
//!
//! - One row per invite event with identity fields only: `event_type`,
//!   `guild_id`, `channel_id`, `invite_code_hash`, `inviter_id`,
//!   `event_time`, `received_at`, `dedup_key`. The raw invite code never
//!   persists: only its hex sha256 hash is stored, and the dedup key is built
//!   from the hash so the code appears in no column.
//! - One row per scheduled-event lifecycle event with identity + enum fields
//!   only: `event_type`, `guild_id`, `channel_id`, `scheduled_event_id`,
//!   `event_time`, `received_at`, `dedup_key`, plus the `event_status` /
//!   `entity_type` integer enums. No description, no name, no free-text
//!   location. User add/remove rows carry `scheduled_event_id` plus the
//!   member-id delta `user_id`.
//! - Every insert runs `ON CONFLICT (dedup_key) DO NOTHING`, so a gateway
//!   resume redelivery inserts nothing.
//! - Privacy tripwire: fixtures carrying invite-code, description, name,
//!   location and username free text persist their ids, code hash, timestamps
//!   and enums only; the sentinels appear in no stored row.
//!
//! Pending-test routing (established pattern): every test is ignored and
//! skipped in the nightly broad sweep, so this card's CI stays green while
//! the S7 build is outstanding; run with `-- --ignored` to see the gap. The
//! S7 build card un-ignores these tests once the migration lands. DB tests
//! run only against the disposable test database (`TestDatabase` refuses
//! anything but agent-testdb), never staging or production.
#![cfg(feature = "db")]

use serde_json::{json, Value};
use sha2::Digest;
use sqlx::PgPool;
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "7777";
const CHANNEL: &str = "88881";
const INVITER: &str = "111";
const USER: &str = "112";
const REMOVED_USER: &str = "113";
const SCHEDULED_EVENT: &str = "99991";

/// Raw invite code: the real gateway INVITE_CREATE dispatch carries it in the
/// clear, and it must never be stored. Only its hash may persist; if this
/// string ever appears in a persisted row, the ids+timestamps+enums-only fence
/// broke.
const CODE_SENTINEL: &str = "s7-raw-invite-code-must-never-persist";

/// Sentinel scheduled-event description: models the free-text description field
/// the store must refuse to persist.
const DESCRIPTION_SENTINEL: &str = "s7-description-sentinel-must-never-persist";

/// Sentinel scheduled-event name: models the free-text name field the store
/// must refuse to persist.
const NAME_SENTINEL: &str = "s7-name-sentinel-must-never-persist";

/// Sentinel external location: models the free-text
/// `entity_metadata.location` field the store must refuse to persist.
const LOCATION_SENTINEL: &str = "s7-location-sentinel-must-never-persist";

/// Sentinel inviter username: models the nested inviter user expansion an
/// INVITE_CREATE dispatch can carry; it must never be stored.
const USERNAME_SENTINEL: &str = "s7-username-sentinel-must-never-persist";

/// Scheduled-event status enums, as the gateway delivers them: 1 scheduled,
/// 2 active, 3 completed, 4 canceled.
const STATUS_SCHEDULED: i32 = 1;
const STATUS_ACTIVE: i32 = 2;

/// Scheduled-event entity-type enums, as the gateway delivers them: 1 stage
/// instance, 2 voice, 3 external.
const ENTITY_VOICE: i32 = 2;
const ENTITY_EXTERNAL: i32 = 3;

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

/// Hex sha256 of an invite code: the only code-derived value the store may
/// keep. The raw code appears in no column, including `dedup_key`.
fn invite_code_hash(code: &str) -> String {
    hex::encode(sha2::Sha256::digest(code.as_bytes()))
}

#[derive(Clone, Copy)]
struct InviteEvent {
    event_type: &'static str,
    channel_id: &'static str,
    code: &'static str,
    inviter_id: &'static str,
    event_time: &'static str,
}

impl InviteEvent {
    fn code_hash(&self) -> String {
        invite_code_hash(self.code)
    }

    fn dedup_key(&self) -> String {
        format!(
            "{}:{GUILD}:{}:{}:{}",
            self.event_type,
            self.channel_id,
            self.code_hash(),
            self.event_time
        )
    }
}

#[derive(Clone, Copy)]
struct ScheduledEvent {
    event_type: &'static str,
    scheduled_event_id: &'static str,
    channel_id: &'static str,
    status: i32,
    entity_type: i32,
    event_time: &'static str,
}

impl ScheduledEvent {
    fn dedup_key(&self) -> String {
        format!(
            "{}:{GUILD}:{}:{}",
            self.event_type, self.scheduled_event_id, self.event_time
        )
    }
}

struct ScheduledUserEvent {
    event_type: &'static str,
    scheduled_event_id: &'static str,
    user_id: &'static str,
    event_time: &'static str,
}

impl ScheduledUserEvent {
    fn dedup_key(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.event_type, self.scheduled_event_id, self.user_id, self.event_time
        )
    }
}

/// Minimal INVITE_CREATE gateway shape: code plus channel, guild and inviter
/// ids, with a hostile nested inviter expansion carrying a username. The
/// username has no binding in the insert below and must never persist.
fn invite_payload(code: &str, channel_id: &str, inviter_id: &str, created_at: &str) -> Value {
    json!({
        "code": code,
        "guild_id": GUILD,
        "channel_id": channel_id,
        "inviter": {
            "id": inviter_id,
            "username": USERNAME_SENTINEL,
        },
        "created_at": created_at,
        "max_age": 86400,
        "max_uses": 5,
        "temporary": false,
        "uses": 0,
    })
}

/// Minimal INVITE_DELETE gateway shape: the dispatch names the code plus
/// channel and guild ids only.
fn invite_delete_payload(code: &str, channel_id: &str) -> Value {
    json!({
        "code": code,
        "guild_id": GUILD,
        "channel_id": channel_id,
    })
}

/// Minimal GUILD_SCHEDULED_EVENT gateway shape: event, guild and channel ids
/// plus timestamps and the status / entity-type enums, plus hostile `name` /
/// `description` / `entity_metadata.location` extras a full dispatch carries.
/// The extras have no binding in the insert below and must never persist.
fn scheduled_event_payload(
    scheduled_event_id: &str,
    channel_id: &str,
    status: i32,
    entity_type: i32,
    start_time: &str,
    end_time: &str,
) -> Value {
    json!({
        "id": scheduled_event_id,
        "guild_id": GUILD,
        "channel_id": channel_id,
        "creator_id": INVITER,
        "name": NAME_SENTINEL,
        "description": DESCRIPTION_SENTINEL,
        "scheduled_start_time": start_time,
        "scheduled_end_time": end_time,
        "privacy_level": 2,
        "status": status,
        "entity_type": entity_type,
        "entity_id": null,
        "entity_metadata": {"location": LOCATION_SENTINEL},
        "user_count": 3,
    })
}

/// Minimal GUILD_SCHEDULED_EVENT_USER_ADD / REMOVE gateway shape: the dispatch
/// names the event, guild and user ids only.
fn scheduled_user_payload(scheduled_event_id: &str, user_id: &str) -> Value {
    json!({
        "guild_scheduled_event_id": scheduled_event_id,
        "guild_id": GUILD,
        "user_id": user_id,
    })
}

/// S7 append-only insert for an invite event: identity + code hash + timestamps,
/// idempotent on `dedup_key`. The column list is the whole contract: anything
/// the fixture carries beyond these columns (raw code, inviter username) has
/// no binding and can never persist.
async fn insert_invite_event(pool: &PgPool, event: &InviteEvent) -> Result<u64, sqlx::Error> {
    let key = event.dedup_key();
    let hash = event.code_hash();
    let result = sqlx::query(
        "INSERT INTO event_store
            (event_type, guild_id, channel_id, invite_code_hash, inviter_id,
             event_time, received_at, dedup_key)
         VALUES
            ($1, $2, $3, $4, $5, $6::timestamptz, NOW(), $7)
         ON CONFLICT (dedup_key) DO NOTHING",
    )
    .bind(event.event_type)
    .bind(GUILD)
    .bind(event.channel_id)
    .bind(&hash)
    .bind(event.inviter_id)
    .bind(event.event_time)
    .bind(&key)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// S7 append-only insert for a scheduled-event lifecycle event: identity +
/// timestamps + the status / entity-type enums, idempotent on `dedup_key`.
async fn insert_scheduled_event(pool: &PgPool, event: &ScheduledEvent) -> Result<u64, sqlx::Error> {
    let key = event.dedup_key();
    let result = sqlx::query(
        "INSERT INTO event_store
            (event_type, guild_id, channel_id, scheduled_event_id,
             event_time, received_at, dedup_key,
             event_status, entity_type)
         VALUES
            ($1, $2, $3, $4, $5::timestamptz, NOW(), $6, $7, $8)
         ON CONFLICT (dedup_key) DO NOTHING",
    )
    .bind(event.event_type)
    .bind(GUILD)
    .bind(event.channel_id)
    .bind(event.scheduled_event_id)
    .bind(event.event_time)
    .bind(&key)
    .bind(event.status)
    .bind(event.entity_type)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// S7 append-only insert for a scheduled-event user delta: event id plus the
/// member-id delta, idempotent on `dedup_key`.
async fn insert_scheduled_user_event(
    pool: &PgPool,
    event: &ScheduledUserEvent,
) -> Result<u64, sqlx::Error> {
    let key = event.dedup_key();
    let result = sqlx::query(
        "INSERT INTO event_store
            (event_type, guild_id, scheduled_event_id, user_id,
             event_time, received_at, dedup_key)
         VALUES
            ($1, $2, $3, $4, $5::timestamptz, NOW(), $6)
         ON CONFLICT (dedup_key) DO NOTHING",
    )
    .bind(event.event_type)
    .bind(GUILD)
    .bind(event.scheduled_event_id)
    .bind(event.user_id)
    .bind(event.event_time)
    .bind(&key)
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
    "channel_id",
    "invite_code_hash",
    "inviter_id",
    "scheduled_event_id",
    "user_id",
    "event_time",
    "received_at",
    "dedup_key",
    "event_status",
    "entity_type",
];

const FORBIDDEN_COLUMNS: &[&str] = &[
    "code",
    "invite_code",
    "vanity_code",
    "name",
    "description",
    "location",
    "entity_metadata",
    "content",
    "message_body",
    "body",
    "text",
    "embeds",
    "username",
    "bio",
    "about",
];

fn assert_invite_schema(columns: &[String]) {
    assert!(
        !columns.is_empty(),
        "S7 creates the invite and scheduled-event columns on the append-only event_store table"
    );
    for required in REQUIRED_COLUMNS {
        assert!(
            columns.iter().any(|column| column == required),
            "event_store keeps invite/scheduled-event column {required}; has {columns:?}"
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
    for sentinel in [
        CODE_SENTINEL,
        DESCRIPTION_SENTINEL,
        NAME_SENTINEL,
        LOCATION_SENTINEL,
        USERNAME_SENTINEL,
    ] {
        assert!(
            !row.contains(sentinel),
            "persisted row carries ids+hash+timestamps+enum fields only: {row}"
        );
    }
}

async fn stored_row(pool: &PgPool, key: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT to_jsonb(store)::text FROM event_store AS store WHERE dedup_key = $1",
    )
    .bind(key.to_owned())
    .fetch_all(pool)
    .await
    .expect("stored row readable")
}

/// S7 AC 1a: an INVITE_CREATE fixture persists the code hash plus inviter,
/// channel and guild ids and timestamps only — the raw code never persists.
///
/// Pending: no invite columns exist until the S7 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
async fn event_store_s7_invite_create_persists_code_hash_and_ids_only() {
    let db = TestDb::new().await;
    assert_invite_schema(&table_columns(&db.pool).await);
    assert_dedup_key_is_unique(&db.pool).await;

    let event = InviteEvent {
        event_type: "INVITE_CREATE",
        channel_id: CHANNEL,
        code: CODE_SENTINEL,
        inviter_id: INVITER,
        event_time: "2026-10-12T12:00:00.000Z",
    };
    let fixture = invite_payload(CODE_SENTINEL, CHANNEL, INVITER, event.event_time);
    assert_eq!(
        fixture["inviter"]["username"],
        json!(USERNAME_SENTINEL),
        "fixture models the nested expansion the store must drop"
    );
    let inserted = insert_invite_event(&db.pool, &event)
        .await
        .expect("S7 append-only invite insert");
    assert_eq!(inserted, 1, "first delivery of an invite create inserts");

    let stored = stored_row(&db.pool, &event.dedup_key()).await;
    assert_eq!(stored.len(), 1, "exactly one row for the invite create");
    assert_no_free_text(&stored[0]);

    let row: (String, String, String, String, String) = sqlx::query_as(
        "SELECT guild_id, channel_id, invite_code_hash, inviter_id, event_type
         FROM event_store WHERE dedup_key = $1",
    )
    .bind(event.dedup_key())
    .fetch_one(&db.pool)
    .await
    .expect("invite ids and code hash readable");
    assert_eq!(
        row,
        (
            GUILD.to_owned(),
            CHANNEL.to_owned(),
            invite_code_hash(CODE_SENTINEL),
            INVITER.to_owned(),
            "INVITE_CREATE".to_owned(),
        ),
        "guild, channel and inviter ids plus the code hash survive the insert"
    );
    db.cleanup().await;
}

/// S7 AC 1b: an INVITE_DELETE fixture persists the code hash plus channel and
/// guild ids and timestamps only — the delete dispatch names the code and ids,
/// so the capture holds no inviter or free text by construction.
///
/// Pending: no invite columns exist until the S7 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
async fn event_store_s7_invite_delete_persists_code_hash_and_ids_only() {
    let db = TestDb::new().await;
    assert_invite_schema(&table_columns(&db.pool).await);
    assert_dedup_key_is_unique(&db.pool).await;

    let fixture = invite_delete_payload(CODE_SENTINEL, CHANNEL);
    assert_eq!(
        fixture["code"],
        json!(CODE_SENTINEL),
        "fixture models the real gateway payload"
    );
    let event = InviteEvent {
        event_type: "INVITE_DELETE",
        channel_id: CHANNEL,
        code: CODE_SENTINEL,
        inviter_id: INVITER,
        event_time: "2026-10-12T12:01:00.000Z",
    };
    let inserted = insert_invite_event(&db.pool, &event)
        .await
        .expect("S7 append-only invite insert");
    assert_eq!(inserted, 1, "first delivery of an invite delete inserts");

    let stored = stored_row(&db.pool, &event.dedup_key()).await;
    assert_eq!(stored.len(), 1, "exactly one row for the invite delete");
    assert_no_free_text(&stored[0]);

    let hash: (String,) =
        sqlx::query_as("SELECT invite_code_hash FROM event_store WHERE dedup_key = $1")
            .bind(event.dedup_key())
            .fetch_one(&db.pool)
            .await
            .expect("delete code hash readable");
    assert_eq!(
        hash.0,
        invite_code_hash(CODE_SENTINEL),
        "the delete keeps the code hash with no raw code"
    );
    db.cleanup().await;
}

/// S7 AC 1c: scheduled-event CREATE / UPDATE / DELETE fixtures persist event,
/// guild and channel ids plus timestamps and the status / entity-type enums
/// only — with NO description and NO free-text location. The store is
/// append-only, so lifecycle transitions land as separate rows.
///
/// Pending: no scheduled-event columns exist until the S7 migration lands, so
/// run with `-- --ignored` against the disposable test database to see the
/// gap.
#[tokio::test]
async fn event_store_s7_scheduled_event_lifecycle_persists_ids_and_status_enums_only() {
    let db = TestDb::new().await;
    assert_invite_schema(&table_columns(&db.pool).await);
    assert_dedup_key_is_unique(&db.pool).await;

    let created = ScheduledEvent {
        event_type: "GUILD_SCHEDULED_EVENT_CREATE",
        scheduled_event_id: SCHEDULED_EVENT,
        channel_id: CHANNEL,
        status: STATUS_SCHEDULED,
        entity_type: ENTITY_VOICE,
        event_time: "2026-10-12T12:00:00.000Z",
    };
    let fixture = scheduled_event_payload(
        SCHEDULED_EVENT,
        CHANNEL,
        STATUS_SCHEDULED,
        ENTITY_VOICE,
        "2026-10-20T18:00:00.000Z",
        "2026-10-20T20:00:00.000Z",
    );
    assert_eq!(
        fixture["description"],
        json!(DESCRIPTION_SENTINEL),
        "fixture models the real gateway payload"
    );
    assert_eq!(
        fixture["entity_metadata"]["location"],
        json!(LOCATION_SENTINEL),
        "fixture models the free-text location the store must drop"
    );
    let inserted = insert_scheduled_event(&db.pool, &created)
        .await
        .expect("S7 append-only scheduled-event insert");
    assert_eq!(
        inserted, 1,
        "first delivery of a scheduled-event create inserts"
    );

    let updated = ScheduledEvent {
        event_type: "GUILD_SCHEDULED_EVENT_UPDATE",
        scheduled_event_id: SCHEDULED_EVENT,
        channel_id: CHANNEL,
        status: STATUS_ACTIVE,
        entity_type: ENTITY_VOICE,
        event_time: "2026-10-12T12:01:00.000Z",
    };
    let update_inserted = insert_scheduled_event(&db.pool, &updated)
        .await
        .expect("S7 append-only scheduled-event insert");
    assert_eq!(
        update_inserted, 1,
        "first delivery of a scheduled-event update inserts"
    );

    let deleted = ScheduledEvent {
        event_type: "GUILD_SCHEDULED_EVENT_DELETE",
        scheduled_event_id: SCHEDULED_EVENT,
        channel_id: CHANNEL,
        status: STATUS_ACTIVE,
        entity_type: ENTITY_EXTERNAL,
        event_time: "2026-10-12T12:02:00.000Z",
    };
    let delete_inserted = insert_scheduled_event(&db.pool, &deleted)
        .await
        .expect("S7 append-only scheduled-event insert");
    assert_eq!(
        delete_inserted, 1,
        "first delivery of a scheduled-event delete inserts"
    );

    for event in [&created, &updated, &deleted] {
        let stored = stored_row(&db.pool, &event.dedup_key()).await;
        assert_eq!(stored.len(), 1, "exactly one row per lifecycle event");
        assert_no_free_text(&stored[0]);
    }

    let enums: Vec<(String, i32, i32)> = sqlx::query_as(
        "SELECT event_type, event_status, entity_type FROM event_store
         WHERE scheduled_event_id = $1 ORDER BY event_time",
    )
    .bind(SCHEDULED_EVENT)
    .fetch_all(&db.pool)
    .await
    .expect("lifecycle enums readable");
    assert_eq!(
        enums,
        vec![
            (
                "GUILD_SCHEDULED_EVENT_CREATE".to_owned(),
                STATUS_SCHEDULED,
                ENTITY_VOICE
            ),
            (
                "GUILD_SCHEDULED_EVENT_UPDATE".to_owned(),
                STATUS_ACTIVE,
                ENTITY_VOICE
            ),
            (
                "GUILD_SCHEDULED_EVENT_DELETE".to_owned(),
                STATUS_ACTIVE,
                ENTITY_EXTERNAL
            ),
        ],
        "each lifecycle row keeps its own status and entity-type enums"
    );
    db.cleanup().await;
}

/// S7 AC 1d: GUILD_SCHEDULED_EVENT_USER_ADD / REMOVE fixtures persist the
/// scheduled-event id plus the user-id delta only.
///
/// Pending: no scheduled-event columns exist until the S7 migration lands, so
/// run with `-- --ignored` against the disposable test database to see the
/// gap.
#[tokio::test]
async fn event_store_s7_scheduled_event_user_add_remove_persist_user_id_deltas_only() {
    let db = TestDb::new().await;
    assert_invite_schema(&table_columns(&db.pool).await);
    assert_dedup_key_is_unique(&db.pool).await;

    let added = ScheduledUserEvent {
        event_type: "GUILD_SCHEDULED_EVENT_USER_ADD",
        scheduled_event_id: SCHEDULED_EVENT,
        user_id: USER,
        event_time: "2026-10-12T12:03:00.000Z",
    };
    let fixture = scheduled_user_payload(SCHEDULED_EVENT, USER);
    assert_eq!(
        fixture["user_id"],
        json!(USER),
        "fixture models the real gateway payload"
    );
    let inserted = insert_scheduled_user_event(&db.pool, &added)
        .await
        .expect("S7 append-only user delta insert");
    assert_eq!(inserted, 1, "first delivery of a user add inserts");

    let removed = ScheduledUserEvent {
        event_type: "GUILD_SCHEDULED_EVENT_USER_REMOVE",
        scheduled_event_id: SCHEDULED_EVENT,
        user_id: REMOVED_USER,
        event_time: "2026-10-12T12:04:00.000Z",
    };
    let removed_inserted = insert_scheduled_user_event(&db.pool, &removed)
        .await
        .expect("S7 append-only user delta insert");
    assert_eq!(
        removed_inserted, 1,
        "first delivery of a user remove inserts"
    );

    for event in [&added, &removed] {
        let stored = stored_row(&db.pool, &event.dedup_key()).await;
        assert_eq!(stored.len(), 1, "exactly one row per user event");
        assert_no_free_text(&stored[0]);
    }

    let ids: (String, String, String) = sqlx::query_as(
        "SELECT guild_id, scheduled_event_id, user_id FROM event_store WHERE dedup_key = $1",
    )
    .bind(added.dedup_key())
    .fetch_one(&db.pool)
    .await
    .expect("user add ids readable");
    assert_eq!(
        ids,
        (
            GUILD.to_owned(),
            SCHEDULED_EVENT.to_owned(),
            USER.to_owned()
        ),
        "the user add keeps guild, event and member ids"
    );
    let removed_ids: (String, String, String) = sqlx::query_as(
        "SELECT guild_id, scheduled_event_id, user_id FROM event_store WHERE dedup_key = $1",
    )
    .bind(removed.dedup_key())
    .fetch_one(&db.pool)
    .await
    .expect("user remove ids readable");
    assert_eq!(
        removed_ids,
        (
            GUILD.to_owned(),
            SCHEDULED_EVENT.to_owned(),
            REMOVED_USER.to_owned()
        ),
        "the user remove keeps guild, event and removed-member ids"
    );
    db.cleanup().await;
}

/// S7 AC 2: duplicate delivery with the same `dedup_key` inserts once
/// (resume-safe, `ON CONFLICT DO NOTHING`).
///
/// Pending: no invite columns exist until the S7 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
async fn event_store_s7_duplicate_delivery_inserts_once() {
    let db = TestDb::new().await;
    assert_invite_schema(&table_columns(&db.pool).await);

    let event = InviteEvent {
        event_type: "INVITE_CREATE",
        channel_id: CHANNEL,
        code: CODE_SENTINEL,
        inviter_id: INVITER,
        event_time: "2026-10-12T12:00:00.000Z",
    };
    let first = insert_invite_event(&db.pool, &event)
        .await
        .expect("first delivery inserts");
    assert_eq!(first, 1, "first delivery inserts");
    // Gateway resume redelivery of the same dispatch: same dedup_key.
    let retry = insert_invite_event(&db.pool, &event)
        .await
        .expect("duplicate delivery is resume-safe");
    assert_eq!(retry, 0, "same dedup_key must insert nothing on redelivery");
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM event_store WHERE dedup_key = $1")
        .bind(event.dedup_key())
        .fetch_one(&db.pool)
        .await
        .expect("row count readable");
    assert_eq!(count.0, 1, "no double row after resume redelivery");

    let scheduled = ScheduledEvent {
        event_type: "GUILD_SCHEDULED_EVENT_CREATE",
        scheduled_event_id: SCHEDULED_EVENT,
        channel_id: CHANNEL,
        status: STATUS_SCHEDULED,
        entity_type: ENTITY_VOICE,
        event_time: "2026-10-12T12:00:00.000Z",
    };
    let scheduled_first = insert_scheduled_event(&db.pool, &scheduled)
        .await
        .expect("first delivery inserts");
    assert_eq!(scheduled_first, 1, "first delivery inserts");
    let scheduled_retry = insert_scheduled_event(&db.pool, &scheduled)
        .await
        .expect("duplicate delivery is resume-safe");
    assert_eq!(
        scheduled_retry, 0,
        "same dedup_key must insert nothing on redelivery"
    );
    db.cleanup().await;
}

/// S7 AC 3 (privacy tripwire): fixtures carrying invite-code, description,
/// name, location and username free text are rejected or never persisted.
///
/// Pending: no invite columns exist until the S7 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
async fn event_store_s7_privacy_tripwire_rejects_descriptions_and_locations() {
    let db = TestDb::new().await;
    // Tripwire part 1: no free-text column may exist to hold the payload.
    let columns = table_columns(&db.pool).await;
    assert!(
        !columns.is_empty(),
        "S7 creates the invite and scheduled-event columns on the append-only event_store table"
    );
    for forbidden in FORBIDDEN_COLUMNS {
        assert!(
            !columns.iter().any(|column| column == forbidden),
            "privacy fence: no {forbidden} column; has {columns:?}"
        );
    }
    // Tripwire part 2: fixtures carrying code + description + name + location +
    // username free text persist their ids, code hash, timestamps and enums
    // only; the sentinels appear in no stored row.
    let invite_event = InviteEvent {
        event_type: "INVITE_CREATE",
        channel_id: CHANNEL,
        code: CODE_SENTINEL,
        inviter_id: INVITER,
        event_time: "2026-10-12T14:00:00.000Z",
    };
    let invite_fixture = invite_payload(CODE_SENTINEL, CHANNEL, INVITER, invite_event.event_time);
    assert_eq!(
        invite_fixture["inviter"]["username"],
        json!(USERNAME_SENTINEL),
        "fixture models a payload carrying username free text"
    );
    let scheduled_fixture = scheduled_event_payload(
        SCHEDULED_EVENT,
        CHANNEL,
        STATUS_SCHEDULED,
        ENTITY_EXTERNAL,
        "2026-10-20T18:00:00.000Z",
        "2026-10-20T20:00:00.000Z",
    );
    assert_eq!(
        scheduled_fixture["description"],
        json!(DESCRIPTION_SENTINEL),
        "fixture models a payload carrying description free text"
    );
    assert_eq!(
        scheduled_fixture["entity_metadata"]["location"],
        json!(LOCATION_SENTINEL),
        "fixture models a payload carrying location free text"
    );
    insert_invite_event(&db.pool, &invite_event)
        .await
        .expect("metadata insert");
    insert_scheduled_event(
        &db.pool,
        &ScheduledEvent {
            event_type: "GUILD_SCHEDULED_EVENT_CREATE",
            scheduled_event_id: SCHEDULED_EVENT,
            channel_id: CHANNEL,
            status: STATUS_SCHEDULED,
            entity_type: ENTITY_EXTERNAL,
            event_time: invite_event.event_time,
        },
    )
    .await
    .expect("metadata insert");
    let dumped: Vec<String> =
        sqlx::query_scalar("SELECT to_jsonb(store)::text FROM event_store AS store")
            .fetch_all(&db.pool)
            .await
            .expect("table dump readable");
    assert!(!dumped.is_empty(), "the metadata rows persisted");
    for row in &dumped {
        assert!(
            !row.contains(CODE_SENTINEL)
                && !row.contains(DESCRIPTION_SENTINEL)
                && !row.contains(NAME_SENTINEL)
                && !row.contains(LOCATION_SENTINEL)
                && !row.contains(USERNAME_SENTINEL),
            "privacy fence: invite and scheduled-event free text never persists: {row}"
        );
    }
    db.cleanup().await;
}
