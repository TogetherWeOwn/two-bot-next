//! Event-store S6 acceptance tests (paired failing-tests card for the S6 build).
//!
//! Test files ONLY: this suite pins the S6 acceptance criteria before the
//! builder starts, so every test here FAILS until the S6 build lands its
//! thread-lifecycle/member-delta migration on the append-only store. The
//! builder makes these tests pass; nothing outside test code changes on this
//! card.
//!
//! Contract under test (event-store P1 slice 6: THREAD_CREATE / THREAD_UPDATE /
//! THREAD_DELETE plus THREAD_LIST_SYNC fan-out plus THREAD_MEMBER_UPDATE /
//! THREAD_MEMBER_REMOVE minimal capture into the append-only `event_store`
//! table):
//!
//! - One row per thread event with identity fields only: `event_type`,
//!   `guild_id`, `thread_id`, `parent_id`, `event_time`, `received_at`,
//!   `dedup_key`, plus the thread-state enums `archived` / `locked` /
//!   `invitable`. Member rows carry `thread_id` plus the member-id delta
//!   `user_id`. No free-text columns: no thread name, no topic, no body.
//! - THREAD_LIST_SYNC fans out to one row per listed thread sharing a dedup
//!   prefix such as `THREAD_LIST_SYNC:<guild>:<sync_time>:<thread_id>`, and
//!   every insert runs `ON CONFLICT (dedup_key) DO NOTHING`, so a gateway
//!   resume redelivery inserts nothing.
//! - Privacy tripwire: fixtures carrying thread-name / topic / username free
//!   text persist their ids, timestamps and enums only; the sentinels appear
//!   in no stored row.
//!
//! Pending-test routing (established pattern): every test is ignored and
//! skipped in the nightly broad sweep, so this card's CI stays green while
//! the S6 build is outstanding; run with `-- --ignored` to see the gap. The
//! S6 build card un-ignores these tests once the migration lands. DB tests
//! run only against the disposable test database (`TestDatabase` refuses
//! anything but agent-testdb), never staging or production.
#![cfg(feature = "db")]

use serde_json::{json, Value};
use sqlx::PgPool;
use two_bot_testsupport::{assert_event_store_columns, TestDatabase};

const GUILD: &str = "5555";
const PARENT: &str = "6666";
const THREAD: &str = "77771";
const USER: &str = "77";
const REMOVED_USER: &str = "79";

/// Sentinel thread name: the real gateway thread payload always carries one,
/// and it must never be stored. If this string ever appears in a persisted
/// row, the ids+timestamps+enums-only fence broke.
const NAME_SENTINEL: &str = "s6-name-sentinel-must-never-persist";

/// Sentinel thread topic: models the free-text topic field the store must
/// refuse to persist.
const TOPIC_SENTINEL: &str = "s6-topic-sentinel-must-never-persist";

/// Sentinel username: models the nested guild-member expansion a
/// THREAD_MEMBER_UPDATE dispatch can carry; it must never be stored.
const USERNAME_SENTINEL: &str = "s6-username-sentinel-must-never-persist";

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

#[derive(Clone, Copy)]
struct ThreadEvent {
    event_type: &'static str,
    thread_id: &'static str,
    parent_id: &'static str,
    event_time: &'static str,
}

impl ThreadEvent {
    fn dedup_key(&self) -> String {
        format!(
            "{}:{GUILD}:{}:{}",
            self.event_type, self.thread_id, self.event_time
        )
    }
}

struct ThreadMemberEvent {
    event_type: &'static str,
    thread_id: &'static str,
    user_id: &'static str,
    event_time: &'static str,
}

impl ThreadMemberEvent {
    fn dedup_key(&self) -> String {
        // Member deltas key guild-first like every other S6 row
        // (`<TYPE>:<guild>:<thread>:<time>:<user>`), matching the 0429 header
        // and the backup seed; keys in one shape deduplicate against each other.
        format!(
            "{}:{GUILD}:{}:{}:{}",
            self.event_type, self.thread_id, self.event_time, self.user_id
        )
    }
}

/// Minimal THREAD_CREATE / THREAD_UPDATE gateway shape: thread, parent and
/// guild ids plus the `thread_metadata` state enums, plus hostile `name` /
/// `topic` extras a full dispatch carries. The extras have no binding in the
/// insert below and must never persist.
fn thread_payload(
    thread_id: &str,
    parent_id: &str,
    archived: bool,
    locked: bool,
    invitable: Option<bool>,
    archive_timestamp: &str,
) -> Value {
    json!({
        "id": thread_id,
        "guild_id": GUILD,
        "parent_id": parent_id,
        "owner_id": USER,
        "type": 11,
        "name": NAME_SENTINEL,
        "topic": TOPIC_SENTINEL,
        "member_count": 2,
        "message_count": 5,
        "rate_limit_per_user": 0,
        "thread_metadata": {
            "archived": archived,
            "auto_archive_duration": 1440,
            "archive_timestamp": archive_timestamp,
            "locked": locked,
            "invitable": invitable,
        },
    })
}

/// Minimal THREAD_DELETE gateway shape: the dispatch names ids only, so the
/// capture holds no flags or free text by construction.
fn thread_delete_payload(thread_id: &str, parent_id: &str) -> Value {
    json!({
        "id": thread_id,
        "guild_id": GUILD,
        "parent_id": parent_id,
        "type": 11,
    })
}

/// Minimal THREAD_MEMBER_UPDATE gateway shape: thread and guild ids plus the
/// member-id delta, with a hostile nested guild-member expansion carrying a
/// username. The expansion has no binding in the insert below and must never
/// persist.
fn member_update_payload(thread_id: &str, user_id: &str, join_timestamp: &str) -> Value {
    json!({
        "id": thread_id,
        "guild_id": GUILD,
        "user_id": user_id,
        "join_timestamp": join_timestamp,
        "flags": 0,
        "member": {
            "user": {"id": user_id, "username": USERNAME_SENTINEL},
            "joined_at": join_timestamp,
        },
    })
}

/// Minimal THREAD_MEMBERS_UPDATE removal shape: the dispatch names the thread
/// plus the removed member ids only.
fn members_remove_payload(thread_id: &str, removed_user_ids: &[&str]) -> Value {
    json!({
        "id": thread_id,
        "guild_id": GUILD,
        "removed_member_ids": removed_user_ids,
    })
}

/// S6 append-only insert for a thread lifecycle event: identity + timestamps +
/// the archived/locked/invitable state enums, idempotent on `dedup_key`.
/// Returns rows inserted (1) or deduplicated (0). The column list is the whole
/// contract: anything the fixture carries beyond these columns (name, topic)
/// has no binding and can never persist.
async fn insert_thread_event(
    pool: &PgPool,
    event: &ThreadEvent,
    archived: Option<bool>,
    locked: Option<bool>,
    invitable: Option<bool>,
) -> Result<u64, sqlx::Error> {
    let key = event.dedup_key();
    let result = sqlx::query(
        "INSERT INTO event_store
            (event_type, guild_id, thread_id, parent_id,
             event_time, received_at, dedup_key,
             archived, locked, invitable)
         VALUES
            ($1, $2, $3, $4, $5::timestamptz, NOW(), $6, $7, $8, $9)
         ON CONFLICT (dedup_key) DO NOTHING",
    )
    .bind(event.event_type)
    .bind(GUILD)
    .bind(event.thread_id)
    .bind(event.parent_id)
    .bind(event.event_time)
    .bind(&key)
    .bind(archived)
    .bind(locked)
    .bind(invitable)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// S6 append-only insert for a thread member-delta event: thread id plus the
/// member-id delta, idempotent on `dedup_key`.
async fn insert_thread_member_event(
    pool: &PgPool,
    event: &ThreadMemberEvent,
) -> Result<u64, sqlx::Error> {
    let key = event.dedup_key();
    let result = sqlx::query(
        "INSERT INTO event_store
            (event_type, guild_id, thread_id, user_id,
             event_time, received_at, dedup_key)
         VALUES
            ($1, $2, $3, $4, $5::timestamptz, NOW(), $6)
         ON CONFLICT (dedup_key) DO NOTHING",
    )
    .bind(event.event_type)
    .bind(GUILD)
    .bind(event.thread_id)
    .bind(event.user_id)
    .bind(event.event_time)
    .bind(&key)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

const SYNC_TIME: &str = "2026-10-12T12:04:00.000Z";

/// Shared dedup prefix for one THREAD_LIST_SYNC dispatch: every fanned-out
/// row keys under this prefix plus its own thread id.
fn sync_dedup_prefix() -> String {
    format!("THREAD_LIST_SYNC:{GUILD}:{SYNC_TIME}")
}

fn sync_dedup_key(thread_id: &str) -> String {
    format!("{}:{thread_id}", sync_dedup_prefix())
}

/// S6 fan-out insert for one listed thread: identity + timestamps + the
/// archived/locked/invitable state enums under the shared sync prefix,
/// idempotent on its per-thread `dedup_key`.
async fn insert_sync_row(
    pool: &PgPool,
    thread_id: &str,
    parent_id: &str,
    archived: bool,
    locked: bool,
    invitable: Option<bool>,
) -> Result<u64, sqlx::Error> {
    let key = sync_dedup_key(thread_id);
    let result = sqlx::query(
        "INSERT INTO event_store
            (event_type, guild_id, thread_id, parent_id,
             event_time, received_at, dedup_key,
             archived, locked, invitable)
         VALUES
            ('THREAD_LIST_SYNC', $1, $2, $3, $4::timestamptz, NOW(), $5, $6, $7, $8)
         ON CONFLICT (dedup_key) DO NOTHING",
    )
    .bind(GUILD)
    .bind(thread_id)
    .bind(parent_id)
    .bind(SYNC_TIME)
    .bind(&key)
    .bind(archived)
    .bind(locked)
    .bind(invitable)
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

/// S6 required columns: the thread-lifecycle/member-delta contract. Every one
/// of the 7 tests calls `assert_thread_schema`, so these must be present
/// whatever sibling slices have landed.
const REQUIRED_COLUMNS: &[&str] = &[
    "event_type",
    "guild_id",
    "thread_id",
    "parent_id",
    "user_id",
    "event_time",
    "received_at",
    "dedup_key",
    "archived",
    "locked",
    "invitable",
];

/// Shared-table schema check: the S6 thread-lifecycle/member-delta columns must
/// be present and every column must sit on the reviewed allowlist in
/// `two_bot_testsupport::EVENT_STORE_ALLOWED_COLUMNS` (the fence table in
/// `docs/event-store.md`). Sibling slices share `event_store` via
/// `ADD COLUMN IF NOT EXISTS`, so an exact S6-only set would break when a
/// sibling lands (S7 invite/scheduled-event columns); a raw-payload or
/// free-text column is in no slice and fails here.
fn assert_thread_schema(columns: &[String]) {
    assert!(
        !columns.is_empty(),
        "S6 creates the thread columns on the append-only event_store table"
    );
    assert_event_store_columns(columns, REQUIRED_COLUMNS);
}

/// No `json`/`jsonb` column may exist except the reviewed `roles` role-id
/// array (S2): free-text payloads (raw thread payloads, member blobs) have no
/// schema home whatever the column name. The writers below bind ids/enums
/// only, so this type fence plus the name allowlist above is what makes the
/// privacy tripwire fail on a scratch `raw_payload JSONB` migration.
async fn assert_no_json_columns(pool: &PgPool) {
    for (column, data_type) in table_column_types(pool).await {
        if column == "roles" {
            continue;
        }
        assert!(
            data_type != "json" && data_type != "jsonb",
            "event_store column {column} has free-text type {data_type}; \
             thread capture persists ids+enums only"
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
    for sentinel in [NAME_SENTINEL, TOPIC_SENTINEL, USERNAME_SENTINEL] {
        assert!(
            !row.contains(sentinel),
            "persisted row carries ids+timestamps+enum fields only: {row}"
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

/// S6 AC 1a: a THREAD_CREATE fixture persists thread, parent and guild ids
/// plus timestamps and the archived/locked/invitable enums only — with NO
/// thread name and NO topic.
///
/// Pending: no thread columns exist until the S6 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
async fn event_store_s6_thread_create_persists_ids_and_flag_enums_only() {
    let db = TestDb::new().await;
    assert_thread_schema(&table_columns(&db.pool).await);
    assert_dedup_key_is_unique(&db.pool).await;

    let event = ThreadEvent {
        event_type: "THREAD_CREATE",
        thread_id: THREAD,
        parent_id: PARENT,
        event_time: "2026-10-12T12:00:00.000Z",
    };
    let fixture = thread_payload(THREAD, PARENT, false, false, Some(true), event.event_time);
    assert_eq!(
        fixture["name"],
        json!(NAME_SENTINEL),
        "fixture models the real gateway payload"
    );
    let inserted = insert_thread_event(&db.pool, &event, Some(false), Some(false), Some(true))
        .await
        .expect("S6 append-only thread insert");
    assert_eq!(inserted, 1, "first delivery of a thread create inserts");

    let stored = stored_row(&db.pool, &event.dedup_key()).await;
    assert_eq!(stored.len(), 1, "exactly one row for the thread create");
    assert_no_free_text(&stored[0]);

    let row: (
        String,
        String,
        String,
        Option<bool>,
        Option<bool>,
        Option<bool>,
    ) = sqlx::query_as(
        "SELECT guild_id, thread_id, parent_id, archived, locked, invitable
         FROM event_store WHERE dedup_key = $1",
    )
    .bind(event.dedup_key())
    .fetch_one(&db.pool)
    .await
    .expect("thread ids and enums readable");
    assert_eq!(
        row,
        (
            GUILD.to_owned(),
            THREAD.to_owned(),
            PARENT.to_owned(),
            Some(false),
            Some(false),
            Some(true),
        ),
        "guild, thread and parent ids plus the state enums survive the insert"
    );
    db.cleanup().await;
}

/// S6 AC 1b: a THREAD_UPDATE fixture persists the changed archived/locked /
/// invitable enums alongside ids and timestamps — the store is append-only, so
/// the update lands as a second row and the latest row carries the new flags.
///
/// Pending: no thread columns exist until the S6 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
async fn event_store_s6_thread_update_persists_changed_flags_only() {
    let db = TestDb::new().await;
    assert_thread_schema(&table_columns(&db.pool).await);
    assert_dedup_key_is_unique(&db.pool).await;

    let created = ThreadEvent {
        event_type: "THREAD_CREATE",
        thread_id: THREAD,
        parent_id: PARENT,
        event_time: "2026-10-12T12:00:00.000Z",
    };
    insert_thread_event(&db.pool, &created, Some(false), Some(false), Some(true))
        .await
        .expect("S6 append-only thread insert");
    let updated = ThreadEvent {
        event_type: "THREAD_UPDATE",
        thread_id: THREAD,
        parent_id: PARENT,
        event_time: "2026-10-12T12:01:00.000Z",
    };
    let fixture = thread_payload(THREAD, PARENT, true, false, Some(false), updated.event_time);
    assert_eq!(
        fixture["thread_metadata"]["archived"],
        json!(true),
        "fixture models the real gateway payload"
    );
    let inserted = insert_thread_event(&db.pool, &updated, Some(true), Some(false), Some(false))
        .await
        .expect("S6 append-only thread insert");
    assert_eq!(inserted, 1, "first delivery of a thread update inserts");

    let stored = stored_row(&db.pool, &updated.dedup_key()).await;
    assert_eq!(stored.len(), 1, "exactly one row for the thread update");
    assert_no_free_text(&stored[0]);

    let flags: (Option<bool>, Option<bool>, Option<bool>) =
        sqlx::query_as("SELECT archived, locked, invitable FROM event_store WHERE dedup_key = $1")
            .bind(updated.dedup_key())
            .fetch_one(&db.pool)
            .await
            .expect("updated enums readable");
    assert_eq!(
        flags,
        (Some(true), Some(false), Some(false)),
        "the update row carries the changed state enums"
    );
    let count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM event_store WHERE thread_id = $1 AND guild_id = $2")
            .bind(THREAD)
            .bind(GUILD)
            .fetch_one(&db.pool)
            .await
            .expect("append-only row count readable");
    assert_eq!(count.0, 2, "create + update stay as two append-only rows");
    db.cleanup().await;
}

/// S6 AC 1c: a THREAD_DELETE fixture persists thread, parent and guild ids
/// plus timestamps with NULL state enums — the delete dispatch names ids
/// only, so the capture holds no flags or free text by construction.
///
/// Pending: no thread columns exist until the S6 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
async fn event_store_s6_thread_delete_persists_ids_only() {
    let db = TestDb::new().await;
    assert_thread_schema(&table_columns(&db.pool).await);
    assert_dedup_key_is_unique(&db.pool).await;

    let fixture = thread_delete_payload(THREAD, PARENT);
    assert_eq!(
        fixture["id"],
        json!(THREAD),
        "fixture models the real gateway payload"
    );
    let event = ThreadEvent {
        event_type: "THREAD_DELETE",
        thread_id: THREAD,
        parent_id: PARENT,
        event_time: "2026-10-12T12:02:00.000Z",
    };
    let inserted = insert_thread_event(&db.pool, &event, None, None, None)
        .await
        .expect("S6 append-only thread insert");
    assert_eq!(inserted, 1, "first delivery of a thread delete inserts");

    let stored = stored_row(&db.pool, &event.dedup_key()).await;
    assert_eq!(stored.len(), 1, "exactly one row for the thread delete");
    assert_no_free_text(&stored[0]);

    let row: (
        String,
        String,
        String,
        Option<bool>,
        Option<bool>,
        Option<bool>,
    ) = sqlx::query_as(
        "SELECT guild_id, thread_id, parent_id, archived, locked, invitable
         FROM event_store WHERE dedup_key = $1",
    )
    .bind(event.dedup_key())
    .fetch_one(&db.pool)
    .await
    .expect("delete ids readable");
    assert_eq!(
        row,
        (
            GUILD.to_owned(),
            THREAD.to_owned(),
            PARENT.to_owned(),
            None,
            None,
            None,
        ),
        "the delete keeps ids with no state enums"
    );
    db.cleanup().await;
}

/// S6 AC 2: a THREAD_LIST_SYNC dispatch fans out to one row per listed thread
/// under a shared dedup prefix, and redelivery of the same sync inserts
/// nothing (`ON CONFLICT DO NOTHING` per thread).
///
/// Pending: no thread columns exist until the S6 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
async fn event_store_s6_list_sync_fans_out_one_row_per_thread() {
    let db = TestDb::new().await;
    assert_thread_schema(&table_columns(&db.pool).await);
    assert_dedup_key_is_unique(&db.pool).await;

    let threads = [("77771", false), ("77772", true), ("77773", false)];
    let prefix = sync_dedup_prefix();
    // The fixture models the real sync shape: the dispatch carries full thread
    // objects (with hostile name/topic extras) plus per-thread member stubs.
    let fixture = json!({
        "guild_id": GUILD,
        "channel_ids": [PARENT],
        "threads": threads.iter().map(|t| thread_payload(
            t.0, PARENT, t.1, false, Some(true), SYNC_TIME,
        )).collect::<Vec<_>>(),
        "members": threads.iter().map(|t| json!({
            "id": t.0,
            "user_id": USER,
            "join_timestamp": SYNC_TIME,
            "flags": 0,
        })).collect::<Vec<_>>(),
    });
    assert_eq!(
        fixture["threads"][0]["name"],
        json!(NAME_SENTINEL),
        "fixture models the real gateway payload"
    );
    for (id, archived) in threads {
        let inserted = insert_sync_row(&db.pool, id, PARENT, archived, false, Some(true))
            .await
            .expect("S6 sync fan-out insert");
        assert_eq!(inserted, 1, "each synced thread inserts one row");
    }

    let keys: Vec<(String, String, Option<bool>)> = sqlx::query_as(
        "SELECT thread_id, dedup_key, archived FROM event_store
         WHERE event_type = 'THREAD_LIST_SYNC' ORDER BY thread_id",
    )
    .fetch_all(&db.pool)
    .await
    .expect("sync rows readable");
    assert_eq!(keys.len(), 3, "one row per synced thread");
    for (thread_id, key, _) in &keys {
        assert!(
            key.starts_with(&prefix),
            "row for {thread_id} shares the sync dedup prefix: {key}"
        );
        assert!(
            key.ends_with(thread_id),
            "row for {thread_id} keys its own id under the prefix: {key}"
        );
        let stored = stored_row(&db.pool, key).await;
        assert_eq!(stored.len(), 1, "exactly one row for {thread_id}");
        assert_no_free_text(&stored[0]);
    }
    assert_eq!(
        keys.iter()
            .map(|(_, _, archived)| *archived)
            .collect::<Vec<_>>(),
        vec![Some(false), Some(true), Some(false)],
        "each synced row keeps its own archived enum"
    );

    // Gateway resume redelivery of the same sync dispatch: same per-thread keys.
    for (id, archived) in threads {
        let retry = insert_sync_row(&db.pool, id, PARENT, archived, false, Some(true))
            .await
            .expect("sync redelivery is resume-safe");
        assert_eq!(
            retry, 0,
            "same per-thread key must insert nothing on redelivery"
        );
    }
    let count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM event_store WHERE event_type = 'THREAD_LIST_SYNC'")
            .fetch_one(&db.pool)
            .await
            .expect("sync row count readable");
    assert_eq!(count.0, 3, "no double rows after sync redelivery");
    db.cleanup().await;
}

/// S6 AC 3: THREAD_MEMBER_UPDATE / THREAD_MEMBER_REMOVE fixtures persist the
/// thread id plus the member-id delta only — with NO nested username or other
/// free text.
///
/// Pending: no thread columns exist until the S6 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
async fn event_store_s6_member_update_remove_persist_member_id_deltas_only() {
    let db = TestDb::new().await;
    assert_thread_schema(&table_columns(&db.pool).await);
    assert_dedup_key_is_unique(&db.pool).await;

    let update = ThreadMemberEvent {
        event_type: "THREAD_MEMBER_UPDATE",
        thread_id: THREAD,
        user_id: USER,
        event_time: "2026-10-12T12:05:00.000Z",
    };
    let fixture = member_update_payload(THREAD, USER, update.event_time);
    assert_eq!(
        fixture["member"]["user"]["username"],
        json!(USERNAME_SENTINEL),
        "fixture models the nested expansion the store must drop"
    );
    let inserted = insert_thread_member_event(&db.pool, &update)
        .await
        .expect("S6 append-only member insert");
    assert_eq!(inserted, 1, "first delivery of a member update inserts");

    let remove = ThreadMemberEvent {
        event_type: "THREAD_MEMBER_REMOVE",
        thread_id: THREAD,
        user_id: REMOVED_USER,
        event_time: "2026-10-12T12:06:00.000Z",
    };
    let remove_fixture = members_remove_payload(THREAD, &[REMOVED_USER]);
    assert_eq!(
        remove_fixture["removed_member_ids"],
        json!([REMOVED_USER]),
        "fixture models the real gateway payload"
    );
    let removed = insert_thread_member_event(&db.pool, &remove)
        .await
        .expect("S6 append-only member insert");
    assert_eq!(removed, 1, "first delivery of a member removal inserts");

    for event in [&update, &remove] {
        let stored = stored_row(&db.pool, &event.dedup_key()).await;
        assert_eq!(stored.len(), 1, "exactly one row per member event");
        assert_no_free_text(&stored[0]);
    }

    let ids: (String, String, String) =
        sqlx::query_as("SELECT guild_id, thread_id, user_id FROM event_store WHERE dedup_key = $1")
            .bind(update.dedup_key())
            .fetch_one(&db.pool)
            .await
            .expect("member update ids readable");
    assert_eq!(
        ids,
        (GUILD.to_owned(), THREAD.to_owned(), USER.to_owned()),
        "the member update keeps guild, thread and member ids"
    );
    let removed_ids: (String, String, String) =
        sqlx::query_as("SELECT guild_id, thread_id, user_id FROM event_store WHERE dedup_key = $1")
            .bind(remove.dedup_key())
            .fetch_one(&db.pool)
            .await
            .expect("member removal ids readable");
    assert_eq!(
        removed_ids,
        (GUILD.to_owned(), THREAD.to_owned(), REMOVED_USER.to_owned()),
        "the member removal keeps guild, thread and removed-member ids"
    );
    db.cleanup().await;
}

/// S6 AC 4: duplicate delivery with the same `dedup_key` inserts once
/// (resume-safe, `ON CONFLICT DO NOTHING`).
///
/// Pending: no thread columns exist until the S6 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
async fn event_store_s6_duplicate_delivery_inserts_once() {
    let db = TestDb::new().await;
    assert_thread_schema(&table_columns(&db.pool).await);

    let event = ThreadEvent {
        event_type: "THREAD_CREATE",
        thread_id: THREAD,
        parent_id: PARENT,
        event_time: "2026-10-12T12:00:00.000Z",
    };
    let first = insert_thread_event(&db.pool, &event, Some(false), Some(false), Some(true))
        .await
        .expect("first delivery inserts");
    assert_eq!(first, 1, "first delivery inserts");
    // Gateway resume redelivery of the same dispatch: same dedup_key.
    let retry = insert_thread_event(&db.pool, &event, Some(false), Some(false), Some(true))
        .await
        .expect("duplicate delivery is resume-safe");
    assert_eq!(retry, 0, "same dedup_key must insert nothing on redelivery");
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM event_store WHERE dedup_key = $1")
        .bind(event.dedup_key())
        .fetch_one(&db.pool)
        .await
        .expect("row count readable");
    assert_eq!(count.0, 1, "no double row after resume redelivery");
    db.cleanup().await;
}

/// S6 AC 5 (privacy tripwire): fixtures carrying thread-name, topic and
/// username free text are rejected or never persisted.
///
/// Pending: no thread columns exist until the S6 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
async fn event_store_s6_privacy_tripwire_rejects_thread_names_and_bodies() {
    let db = TestDb::new().await;
    // Tripwire part 1: the table holds only reviewed slice columns, so no
    // free-text column exists to hold the payload.
    assert_thread_schema(&table_columns(&db.pool).await);
    assert_no_json_columns(&db.pool).await;
    // Tripwire part 2: fixtures carrying name + topic + username free text
    // persist their ids, timestamps and enums only; the sentinels appear in no
    // stored row.
    let event = ThreadEvent {
        event_type: "THREAD_CREATE",
        thread_id: THREAD,
        parent_id: PARENT,
        event_time: "2026-10-12T14:00:00.000Z",
    };
    let fixture = thread_payload(THREAD, PARENT, false, false, Some(true), event.event_time);
    assert_eq!(
        fixture["topic"],
        json!(TOPIC_SENTINEL),
        "fixture models a payload carrying topic free text"
    );
    let member_fixture = member_update_payload(THREAD, USER, event.event_time);
    assert_eq!(
        member_fixture["member"]["user"]["username"],
        json!(USERNAME_SENTINEL),
        "fixture models a payload carrying username free text"
    );
    insert_thread_event(&db.pool, &event, Some(false), Some(false), Some(true))
        .await
        .expect("metadata insert");
    insert_thread_member_event(
        &db.pool,
        &ThreadMemberEvent {
            event_type: "THREAD_MEMBER_UPDATE",
            thread_id: THREAD,
            user_id: USER,
            event_time: event.event_time,
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
            !row.contains(NAME_SENTINEL)
                && !row.contains(TOPIC_SENTINEL)
                && !row.contains(USERNAME_SENTINEL),
            "privacy fence: thread free text never persists: {row}"
        );
    }
    db.cleanup().await;
}
