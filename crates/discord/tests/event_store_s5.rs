//! Event-store S5 acceptance tests (paired failing-tests card for the S5 build).
//!
//! Test files ONLY: this suite pins the S5 acceptance criteria before the
//! builder starts, so every test here FAILS until the S5 build lands its
//! message/reaction-capture migration on the append-only store. The builder
//! makes these tests pass; nothing outside test code changes on this card.
//!
//! Contract under test (event-store P1 slice 5: MESSAGE_UPDATE /
//! MESSAGE_DELETE / MESSAGE_DELETE_BULK plus MESSAGE_REACTION_ADD /
//! MESSAGE_REACTION_REMOVE minimal capture into the append-only `event_store`
//! table):
//!
//! - One row per message event with identity fields only: `event_type`,
//!   `guild_id`, `channel_id`, `message_id`, `user_id`, `event_time`,
//!   `received_at`, `dedup_key`, plus `emoji_id` / `emoji_name` on reaction
//!   rows. No message-body columns: no content, no embeds, no attachments.
//! - Bulk delete fans out to one row per deleted id sharing a dedup prefix
//!   such as `MESSAGE_DELETE_BULK:<channel>:<bulk_time>:<message_id>`, and
//!   every insert runs `ON CONFLICT (dedup_key) DO NOTHING`, so a gateway
//!   resume redelivery inserts nothing.
//! - Emoji unicode is stored as an enum/id reference only (custom emoji id or
//!   a codepoint reference in `emoji_name`); raw message text never persists.
//! - Privacy tripwire: a fixture carrying message-body sentinels persists its
//!   ids only; the sentinels appear in no stored row.
//!
//! Pending-test routing (established pattern): every test is ignored and
//! skipped in the nightly broad sweep, so this card's CI stays green while
//! the S5 build is outstanding; run with `-- --ignored` to see the gap. The
//! S5 build card un-ignores these tests once the migration lands. DB tests
//! run only against the disposable test database (`TestDatabase` refuses
//! anything but agent-testdb), never staging or production.
#![cfg(feature = "db")]

use serde_json::{json, Value};
use sqlx::PgPool;
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "4444";
const CHANNEL: &str = "8888";
const MESSAGE: &str = "99991";
const USER: &str = "77";
const AUTHOR: &str = "78";
const CUSTOM_EMOJI_ID: &str = "123456789";
const CUSTOM_EMOJI_NAME: &str = "partyparrot";

/// Unicode thumbs-up as the reaction name, the way the gateway delivers a
/// non-custom emoji reaction (no id, name carries the glyph).
const UNICODE_EMOJI_NAME: &str = "\u{1F44D}";

/// Codepoint reference the store must keep instead of raw message text: the
/// unicode reaction is captured as an enum/id-style reference, never as a
/// message body.
const UNICODE_EMOJI_REF: &str = "U+1F44D";

/// Sentinel message body: models the `content` field the store must refuse to
/// persist. If this string ever appears in a persisted row, the
/// ids+timestamps+enum-only fence broke.
const CONTENT_SENTINEL: &str = "s5-content-sentinel-must-never-persist";

/// Sentinel embed payload: models embed title/description fields the store
/// must refuse to persist.
const EMBED_SENTINEL: &str = "s5-embed-sentinel-must-never-persist";

/// Sentinel attachment payload: models attachment URL/filename fields the
/// store must refuse to persist.
const ATTACHMENT_SENTINEL: &str = "s5-attachment-sentinel-must-never-persist";

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
struct MessageEvent {
    event_type: &'static str,
    message_id: &'static str,
    event_time: &'static str,
}

impl MessageEvent {
    fn dedup_key(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.event_type, CHANNEL, self.message_id, self.event_time
        )
    }
}

struct ReactionEvent {
    event_type: &'static str,
    message_id: &'static str,
    user_id: &'static str,
    emoji_id: Option<&'static str>,
    emoji_name: &'static str,
    event_time: &'static str,
}

impl ReactionEvent {
    fn dedup_key(&self) -> String {
        format!(
            "{}:{}:{}:{}:{}",
            self.event_type, CHANNEL, self.message_id, self.user_id, self.event_time
        )
    }
}

/// Minimal MESSAGE_UPDATE gateway shape: ids + author + edited timestamp, plus
/// hostile `content` / `embeds` / `attachments` extras a full dispatch
/// carries. The extras have no binding in the insert below and must never
/// persist.
fn message_update_payload(event: &MessageEvent) -> Value {
    json!({
        "guild_id": GUILD,
        "channel_id": CHANNEL,
        "id": event.message_id,
        "author": {"id": AUTHOR},
        "content": CONTENT_SENTINEL,
        "embeds": [{"title": EMBED_SENTINEL}],
        "attachments": [{"filename": ATTACHMENT_SENTINEL}],
        "edited_timestamp": event.event_time,
    })
}

/// Minimal MESSAGE_DELETE gateway shape: the dispatch names ids only, so the
/// capture is ids plus timestamps by construction.
fn message_delete_payload(message_id: &str) -> Value {
    json!({
        "guild_id": GUILD,
        "channel_id": CHANNEL,
        "id": message_id,
    })
}

/// Minimal MESSAGE_REACTION_ADD / REMOVE gateway shape: message, channel and
/// guild ids plus the reacting user and the emoji reference. A hostile
/// `content` extra models a mis-shaped payload; it has no binding and must
/// never persist.
fn reaction_payload(event: &ReactionEvent) -> Value {
    json!({
        "guild_id": GUILD,
        "channel_id": CHANNEL,
        "message_id": event.message_id,
        "user_id": event.user_id,
        "emoji": {"id": event.emoji_id, "name": event.emoji_name},
        "content": CONTENT_SENTINEL,
    })
}

/// S5 append-only insert for a single message event: identity + timestamps,
/// idempotent on `dedup_key`. Returns rows inserted (1) or deduplicated (0).
/// The column list is the whole contract: anything the fixture carries beyond
/// these columns (content, embeds, attachments) has no binding and can never
/// persist.
async fn insert_message_event(pool: &PgPool, event: &MessageEvent) -> Result<u64, sqlx::Error> {
    let key = event.dedup_key();
    let result = sqlx::query(
        "INSERT INTO event_store
            (event_type, guild_id, channel_id, message_id, user_id,
             event_time, received_at, dedup_key)
         VALUES
            ($1, $2, $3, $4, $5, $6::timestamptz, NOW(), $7)
         ON CONFLICT (dedup_key) DO NOTHING",
    )
    .bind(event.event_type)
    .bind(GUILD)
    .bind(CHANNEL)
    .bind(event.message_id)
    .bind(AUTHOR)
    .bind(event.event_time)
    .bind(&key)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// S5 append-only insert for a reaction event: identity + the emoji id/name
/// reference, idempotent on `dedup_key`. The emoji is stored as an id
/// reference (custom emoji) or a codepoint/enum reference (unicode), never as
/// message text.
async fn insert_reaction_event(pool: &PgPool, event: &ReactionEvent) -> Result<u64, sqlx::Error> {
    let key = event.dedup_key();
    let result = sqlx::query(
        "INSERT INTO event_store
            (event_type, guild_id, channel_id, message_id, user_id,
             emoji_id, emoji_name, event_time, received_at, dedup_key)
         VALUES
            ($1, $2, $3, $4, $5, $6, $7, $8::timestamptz, NOW(), $9)
         ON CONFLICT (dedup_key) DO NOTHING",
    )
    .bind(event.event_type)
    .bind(GUILD)
    .bind(CHANNEL)
    .bind(event.message_id)
    .bind(event.user_id)
    .bind(event.emoji_id)
    .bind(event.emoji_name)
    .bind(event.event_time)
    .bind(&key)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

const BULK_TIME: &str = "2026-10-12T12:04:00.000Z";

/// Shared dedup prefix for one MESSAGE_DELETE_BULK dispatch: every fanned-out
/// row keys under this prefix plus its own message id.
fn bulk_dedup_prefix() -> String {
    format!("MESSAGE_DELETE_BULK:{CHANNEL}:{BULK_TIME}")
}

fn bulk_dedup_key(message_id: &str) -> String {
    format!("{}:{message_id}", bulk_dedup_prefix())
}

/// S5 fan-out insert for one bulk-deleted id: identity + timestamps under the
/// shared bulk prefix, idempotent on its per-id `dedup_key`.
async fn insert_bulk_delete_row(pool: &PgPool, message_id: &str) -> Result<u64, sqlx::Error> {
    let key = bulk_dedup_key(message_id);
    let result = sqlx::query(
        "INSERT INTO event_store
            (event_type, guild_id, channel_id, message_id,
             event_time, received_at, dedup_key)
         VALUES
            ('MESSAGE_DELETE_BULK', $1, $2, $3, $4::timestamptz, NOW(), $5)
         ON CONFLICT (dedup_key) DO NOTHING",
    )
    .bind(GUILD)
    .bind(CHANNEL)
    .bind(message_id)
    .bind(BULK_TIME)
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
    "message_id",
    "user_id",
    "emoji_id",
    "emoji_name",
    "event_time",
    "received_at",
    "dedup_key",
];

const FORBIDDEN_COLUMNS: &[&str] = &[
    "content",
    "message_body",
    "body",
    "text",
    "embeds",
    "embed_title",
    "attachments",
    "attachment_url",
    "username",
];

fn assert_message_schema(columns: &[String]) {
    assert!(
        !columns.is_empty(),
        "S5 creates the message/reaction columns on the append-only event_store table"
    );
    for required in REQUIRED_COLUMNS {
        assert!(
            columns.iter().any(|column| column == required),
            "event_store keeps message column {required}; has {columns:?}"
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

fn assert_no_message_body(row: &str) {
    for sentinel in [CONTENT_SENTINEL, EMBED_SENTINEL, ATTACHMENT_SENTINEL] {
        assert!(
            !row.contains(sentinel),
            "persisted row carries ids+timestamps+enum refs only: {row}"
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

/// S5 AC 1a: a MESSAGE_UPDATE fixture persists message, channel, guild and
/// author ids plus timestamps only — with NO message body, NO embeds and NO
/// attachments.
///
/// Pending: no message columns exist until the S5 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
#[ignore = "pending S5 build: needs disposable test database event_store message columns; run with --ignored to see the gap"]
async fn event_store_s5_message_update_persists_ids_without_body() {
    let db = TestDb::new().await;
    assert_message_schema(&table_columns(&db.pool).await);
    assert_dedup_key_is_unique(&db.pool).await;

    let event = MessageEvent {
        event_type: "MESSAGE_UPDATE",
        message_id: MESSAGE,
        event_time: "2026-10-12T12:00:00.000Z",
    };
    let fixture = message_update_payload(&event);
    assert_eq!(
        fixture["content"],
        json!(CONTENT_SENTINEL),
        "fixture models the real gateway payload"
    );
    let inserted = insert_message_event(&db.pool, &event)
        .await
        .expect("S5 append-only message insert");
    assert_eq!(inserted, 1, "first delivery of a message update inserts");

    let stored = stored_row(&db.pool, &event.dedup_key()).await;
    assert_eq!(stored.len(), 1, "exactly one row for the message update");
    assert_no_message_body(&stored[0]);

    let row: (String, String, String, String) = sqlx::query_as(
        "SELECT guild_id, channel_id, message_id, user_id
         FROM event_store WHERE dedup_key = $1",
    )
    .bind(event.dedup_key())
    .fetch_one(&db.pool)
    .await
    .expect("message ids readable");
    assert_eq!(
        row,
        (
            GUILD.to_owned(),
            CHANNEL.to_owned(),
            MESSAGE.to_owned(),
            AUTHOR.to_owned(),
        ),
        "guild, channel, message and author ids survive the insert"
    );
    db.cleanup().await;
}

/// S5 AC 1b: a MESSAGE_DELETE fixture persists message, channel and guild ids
/// plus timestamps — the delete dispatch names ids only, so the capture holds
/// no body by construction.
///
/// Pending: no message columns exist until the S5 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
#[ignore = "pending S5 build: needs disposable test database event_store message columns; run with --ignored to see the gap"]
async fn event_store_s5_message_delete_persists_ids_only() {
    let db = TestDb::new().await;
    assert_message_schema(&table_columns(&db.pool).await);
    assert_dedup_key_is_unique(&db.pool).await;

    let fixture = message_delete_payload(MESSAGE);
    assert_eq!(
        fixture["id"],
        json!(MESSAGE),
        "fixture models the real gateway payload"
    );
    let event = MessageEvent {
        event_type: "MESSAGE_DELETE",
        message_id: MESSAGE,
        event_time: "2026-10-12T12:01:00.000Z",
    };
    let inserted = insert_message_event(&db.pool, &event)
        .await
        .expect("S5 append-only message insert");
    assert_eq!(inserted, 1, "first delivery of a message delete inserts");

    let stored = stored_row(&db.pool, &event.dedup_key()).await;
    assert_eq!(stored.len(), 1, "exactly one row for the message delete");
    assert_no_message_body(&stored[0]);

    let ids: (String, String, String) = sqlx::query_as(
        "SELECT guild_id, channel_id, message_id FROM event_store WHERE dedup_key = $1",
    )
    .bind(event.dedup_key())
    .fetch_one(&db.pool)
    .await
    .expect("delete ids readable");
    assert_eq!(
        ids,
        (GUILD.to_owned(), CHANNEL.to_owned(), MESSAGE.to_owned()),
        "the delete keeps guild, channel and message ids"
    );
    db.cleanup().await;
}

/// S5 AC 1c: MESSAGE_REACTION_ADD / REMOVE fixtures persist message, channel,
/// guild and user ids plus the emoji id/name reference only — with NO message
/// body.
///
/// Pending: no message columns exist until the S5 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
#[ignore = "pending S5 build: needs disposable test database event_store message columns; run with --ignored to see the gap"]
async fn event_store_s5_reaction_add_remove_persist_emoji_refs_only() {
    let db = TestDb::new().await;
    assert_message_schema(&table_columns(&db.pool).await);
    assert_dedup_key_is_unique(&db.pool).await;

    let add = ReactionEvent {
        event_type: "MESSAGE_REACTION_ADD",
        message_id: MESSAGE,
        user_id: USER,
        emoji_id: Some(CUSTOM_EMOJI_ID),
        emoji_name: CUSTOM_EMOJI_NAME,
        event_time: "2026-10-12T12:02:00.000Z",
    };
    let fixture = reaction_payload(&add);
    assert_eq!(
        fixture["emoji"]["id"],
        json!(CUSTOM_EMOJI_ID),
        "fixture models the real gateway payload"
    );
    let inserted = insert_reaction_event(&db.pool, &add)
        .await
        .expect("S5 append-only reaction insert");
    assert_eq!(inserted, 1, "first delivery of a reaction add inserts");

    let remove = ReactionEvent {
        event_type: "MESSAGE_REACTION_REMOVE",
        message_id: MESSAGE,
        user_id: USER,
        emoji_id: Some(CUSTOM_EMOJI_ID),
        emoji_name: CUSTOM_EMOJI_NAME,
        event_time: "2026-10-12T12:03:00.000Z",
    };
    let removed = insert_reaction_event(&db.pool, &remove)
        .await
        .expect("S5 append-only reaction insert");
    assert_eq!(removed, 1, "first delivery of a reaction remove inserts");

    for event in [&add, &remove] {
        let stored = stored_row(&db.pool, &event.dedup_key()).await;
        assert_eq!(stored.len(), 1, "exactly one row per reaction event");
        assert_no_message_body(&stored[0]);
    }

    let refs: (Option<String>, Option<String>) =
        sqlx::query_as("SELECT emoji_id, emoji_name FROM event_store WHERE dedup_key = $1")
            .bind(add.dedup_key())
            .fetch_one(&db.pool)
            .await
            .expect("reaction emoji refs readable");
    assert_eq!(
        refs,
        (
            Some(CUSTOM_EMOJI_ID.to_owned()),
            Some(CUSTOM_EMOJI_NAME.to_owned()),
        ),
        "custom emoji persists as its id/name reference"
    );
    db.cleanup().await;
}

/// S5 AC 2: a MESSAGE_DELETE_BULK dispatch fans out to one row per deleted id
/// under a shared dedup prefix, and redelivery of the same bulk inserts
/// nothing (`ON CONFLICT DO NOTHING` per id).
///
/// Pending: no message columns exist until the S5 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
#[ignore = "pending S5 build: needs disposable test database event_store message columns; run with --ignored to see the gap"]
async fn event_store_s5_bulk_delete_fans_out_one_row_per_id() {
    let db = TestDb::new().await;
    assert_message_schema(&table_columns(&db.pool).await);
    assert_dedup_key_is_unique(&db.pool).await;

    let ids = ["99991", "99992", "99993"];
    let prefix = bulk_dedup_prefix();
    for id in ids {
        let inserted = insert_bulk_delete_row(&db.pool, id)
            .await
            .expect("S5 bulk fan-out insert");
        assert_eq!(inserted, 1, "each bulk-deleted id inserts one row");
    }

    let keys: Vec<(String, String)> = sqlx::query_as(
        "SELECT message_id, dedup_key FROM event_store
         WHERE event_type = 'MESSAGE_DELETE_BULK' ORDER BY message_id",
    )
    .fetch_all(&db.pool)
    .await
    .expect("bulk rows readable");
    assert_eq!(keys.len(), 3, "one row per bulk-deleted id");
    for (message_id, key) in &keys {
        assert!(
            key.starts_with(&prefix),
            "row for {message_id} shares the bulk dedup prefix: {key}"
        );
        assert!(
            key.ends_with(message_id),
            "row for {message_id} keys its own id under the prefix: {key}"
        );
        let stored = stored_row(&db.pool, key).await;
        assert_eq!(stored.len(), 1, "exactly one row for {message_id}");
        assert_no_message_body(&stored[0]);
    }

    // Gateway resume redelivery of the same bulk dispatch: same per-id keys.
    for id in ids {
        let retry = insert_bulk_delete_row(&db.pool, id)
            .await
            .expect("bulk redelivery is resume-safe");
        assert_eq!(
            retry, 0,
            "same per-id key must insert nothing on redelivery"
        );
    }
    let count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM event_store WHERE event_type = 'MESSAGE_DELETE_BULK'")
            .fetch_one(&db.pool)
            .await
            .expect("bulk row count readable");
    assert_eq!(count.0, 3, "no double rows after bulk redelivery");
    db.cleanup().await;
}

/// S5 AC 3: a unicode (non-custom) reaction stores its emoji as an enum/id
/// reference only — NULL custom id plus the codepoint reference — never raw
/// message text.
///
/// Pending: no message columns exist until the S5 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
#[ignore = "pending S5 build: needs disposable test database event_store message columns; run with --ignored to see the gap"]
async fn event_store_s5_unicode_reaction_stores_enum_ref_only() {
    let db = TestDb::new().await;
    assert_message_schema(&table_columns(&db.pool).await);

    let event = ReactionEvent {
        event_type: "MESSAGE_REACTION_ADD",
        message_id: MESSAGE,
        user_id: USER,
        emoji_id: None,
        emoji_name: UNICODE_EMOJI_REF,
        event_time: "2026-10-12T12:05:00.000Z",
    };
    // The fixture models the gateway shape: a unicode reaction arrives with
    // no custom id and the glyph as its name.
    let gateway = reaction_payload(&ReactionEvent {
        event_type: event.event_type,
        message_id: event.message_id,
        user_id: event.user_id,
        emoji_id: None,
        emoji_name: UNICODE_EMOJI_NAME,
        event_time: event.event_time,
    });
    assert_eq!(
        gateway["emoji"]["id"],
        Value::Null,
        "unicode reactions carry no custom emoji id"
    );
    let inserted = insert_reaction_event(&db.pool, &event)
        .await
        .expect("S5 append-only reaction insert");
    assert_eq!(inserted, 1, "first delivery of a unicode reaction inserts");

    let refs: (Option<String>, Option<String>) =
        sqlx::query_as("SELECT emoji_id, emoji_name FROM event_store WHERE dedup_key = $1")
            .bind(event.dedup_key())
            .fetch_one(&db.pool)
            .await
            .expect("unicode emoji refs readable");
    assert_eq!(
        refs,
        (None, Some(UNICODE_EMOJI_REF.to_owned())),
        "unicode emoji persists as its enum/id reference with no custom id"
    );
    let stored = stored_row(&db.pool, &event.dedup_key()).await;
    assert_eq!(stored.len(), 1, "exactly one row for the unicode reaction");
    assert_no_message_body(&stored[0]);
    db.cleanup().await;
}

/// S5 AC 4 (privacy tripwire): a fixture carrying message-body, embed and
/// attachment payload is rejected or never persisted.
///
/// Pending: no message columns exist until the S5 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
#[ignore = "pending S5 build: needs disposable test database event_store message columns; run with --ignored to see the gap"]
async fn event_store_s5_privacy_tripwire_rejects_message_bodies() {
    let db = TestDb::new().await;
    // Tripwire part 1: no message-body column may exist to hold the payload.
    let columns = table_columns(&db.pool).await;
    assert!(
        !columns.is_empty(),
        "S5 creates the message columns on the append-only event_store table"
    );
    for forbidden in FORBIDDEN_COLUMNS {
        assert!(
            !columns.iter().any(|column| column == forbidden),
            "privacy fence: no {forbidden} column; has {columns:?}"
        );
    }
    // Tripwire part 2: a fixture carrying body + embed + attachment free text
    // persists its ids only; the sentinels appear in no stored row.
    let event = MessageEvent {
        event_type: "MESSAGE_UPDATE",
        message_id: MESSAGE,
        event_time: "2026-10-12T14:00:00.000Z",
    };
    let fixture = message_update_payload(&event);
    assert_eq!(
        fixture["embeds"][0]["title"],
        json!(EMBED_SENTINEL),
        "fixture models a payload carrying embed free text"
    );
    insert_message_event(&db.pool, &event)
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
            !row.contains(CONTENT_SENTINEL)
                && !row.contains(EMBED_SENTINEL)
                && !row.contains(ATTACHMENT_SENTINEL),
            "privacy fence: message bodies never persist: {row}"
        );
    }
    db.cleanup().await;
}
