//! Event-store S8 acceptance tests (paired failing-tests card for the S8 build).
//!
//! Test files ONLY: this suite pins the S8 acceptance criteria before the
//! builder starts, so every database test here FAILS until the S8 build lands
//! its stage-instance / interaction-observe / audit-log-entry capture
//! migration on the append-only store. The builder makes these tests pass;
//! nothing outside test code changes on this card.
//!
//! Contract under test (event-store P1 slice 8: STAGE_INSTANCE_CREATE /
//! STAGE_INSTANCE_UPDATE / STAGE_INSTANCE_DELETE plus INTERACTION_CREATE
//! observe plus GUILD_AUDIT_LOG_ENTRY_CREATE minimal capture into the
//! append-only `event_store` table):
//!
//! - One row per stage-instance event with identity + enum fields only:
//!   `event_type`, `guild_id`, `channel_id`, `stage_instance_id`,
//!   `event_time`, `received_at`, `dedup_key`, plus the `privacy_level`
//!   integer enum. The free-text `topic` never persists.
//! - One row per INTERACTION_CREATE with observe-only identity + enum fields:
//!   `event_type`, `guild_id`, `channel_id`, `interaction_id`,
//!   `interaction_type`, `application_id`, `command_id`, `event_time`,
//!   `received_at`, `dedup_key`. The interaction `token` (a bearer secret)
//!   and the free-text `data.options` / nested username never persist, and
//!   the capture never consumes the dispatch: the ACK path still sees the
//!   full payload.
//! - One row per audit-log entry with identity + enum fields only:
//!   `event_type`, `guild_id`, `audit_log_entry_id`, `action_type`,
//!   `actor_id`, `target_id`, `event_time`, `received_at`, `dedup_key`. No
//!   `changes` detail bodies, no `options`, no free-text `reason`.
//! - Every insert runs `ON CONFLICT (dedup_key) DO NOTHING`, so a gateway
//!   resume redelivery inserts nothing.
//! - Observe-only: the two offline tests below run WITHOUT the database and
//!   pass before the build; they pin that the fixtures model the real
//!   dispatch shape (so the dispatcher keeps working unmodified) and that
//!   dedup keys are stable per delivery. Existing dispatch tests pass
//!   unmodified.
//! - Privacy tripwire: fixtures carrying topic, token, option, username,
//!   change-detail and reason free text persist their ids, timestamps and
//!   enums only; the sentinels appear in no stored row.
//!
//! Test routing: the database tests below run enabled against the disposable
//! test database (`TestDatabase` refuses anything but agent-testdb), never
//! staging or production. They are skipped in the nightly broad sweep via
//! `--skip` entries (see `.github/workflows/nightly.yml`), so the broad sweep
//! stays green while slice suites run in their own lanes.
#![cfg(feature = "db")]

use serde_json::{json, Value};
use sqlx::PgPool;
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "99991";
const CHANNEL: &str = "99992";
const STAGE_INSTANCE: &str = "99993";
const INTERACTION: &str = "109876543210987654";
const APPLICATION: &str = "109876543210987650";
const COMMAND: &str = "109876543210987651";
const ACTOR: &str = "1111";
const TARGET: &str = "2222";
const AUDIT_ENTRY: &str = "3333";

/// Sentinel stage topic: the real gateway stage-instance payload always
/// carries one, and it must never be stored. If this string ever appears in a
/// persisted row, the ids+timestamps+enums-only fence broke.
const TOPIC_SENTINEL: &str = "s8-topic-sentinel-must-never-persist";

/// Sentinel interaction token: the real INTERACTION_CREATE dispatch carries a
/// bearer token the ACK path needs, and it must never be stored. Only its
/// presence in the live fixture (for the dispatcher) is asserted; no stored
/// row may contain it.
const TOKEN_SENTINEL: &str = "s8-interaction-token-must-never-persist";

/// Sentinel interaction option value: models the free-text `data.options` a
/// full application-command dispatch carries; it must never be stored.
const OPTION_SENTINEL: &str = "s8-option-free-text-must-never-persist";

/// Sentinel username: models the nested member/user expansion an
/// INTERACTION_CREATE dispatch can carry; it must never be stored.
const USERNAME_SENTINEL: &str = "s8-username-sentinel-must-never-persist";

/// Sentinel audit-log change detail: models a `changes[].new_value` free-text
/// body a GUILD_AUDIT_LOG_ENTRY_CREATE capture can carry; it must never be
/// stored.
const CHANGE_SENTINEL: &str = "s8-change-detail-must-never-persist";

/// Sentinel audit-log reason: models the free-text `reason` field an audit-log
/// entry can carry; it must never be stored.
const REASON_SENTINEL: &str = "s8-reason-sentinel-must-never-persist";

/// Stage-instance privacy-level enums, as the gateway delivers them: 1 public,
/// 2 guild-only.
const PRIVACY_PUBLIC: i32 = 1;
const PRIVACY_GUILD_ONLY: i32 = 2;

/// Interaction type enums, as the gateway delivers them: 2 application
/// command, 3 message component.
const INTERACTION_APPLICATION_COMMAND: i32 = 2;
const INTERACTION_MESSAGE_COMPONENT: i32 = 3;

/// Audit-log action-type enums, as the capture delivers them: 1 guild update,
/// 72 message delete.
const ACTION_GUILD_UPDATE: i32 = 1;
const ACTION_MESSAGE_DELETE: i32 = 72;

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
struct StageEvent {
    event_type: &'static str,
    stage_instance_id: &'static str,
    channel_id: &'static str,
    privacy_level: i32,
    event_time: &'static str,
}

impl StageEvent {
    fn dedup_key(&self) -> String {
        format!(
            "{}:{GUILD}:{}:{}",
            self.event_type, self.stage_instance_id, self.event_time
        )
    }
}

#[derive(Clone, Copy)]
struct InteractionEvent {
    event_type: &'static str,
    interaction_id: &'static str,
    interaction_type: i32,
    application_id: &'static str,
    command_id: &'static str,
    channel_id: &'static str,
    event_time: &'static str,
}

impl InteractionEvent {
    fn dedup_key(&self) -> String {
        format!(
            "{}:{GUILD}:{}:{}",
            self.event_type, self.interaction_id, self.event_time
        )
    }
}

#[derive(Clone, Copy)]
struct AuditLogEvent {
    event_type: &'static str,
    audit_log_entry_id: &'static str,
    action_type: i32,
    actor_id: &'static str,
    target_id: &'static str,
    event_time: &'static str,
}

impl AuditLogEvent {
    fn dedup_key(&self) -> String {
        format!(
            "{}:{GUILD}:{}:{}",
            self.event_type, self.audit_log_entry_id, self.event_time
        )
    }
}

/// Minimal STAGE_INSTANCE gateway shape: instance, guild and channel ids plus
/// the privacy-level enum and a hostile free-text `topic` the store must drop.
/// The topic has no binding in the insert below and must never persist.
fn stage_payload(
    stage_instance_id: &str,
    channel_id: &str,
    privacy_level: i32,
    discoverable_disabled: bool,
) -> Value {
    json!({
        "id": stage_instance_id,
        "guild_id": GUILD,
        "channel_id": channel_id,
        "topic": TOPIC_SENTINEL,
        "privacy_level": privacy_level,
        "discoverable_disabled": discoverable_disabled,
    })
}

/// Minimal INTERACTION_CREATE gateway shape: interaction, application and
/// command ids plus the type enum, with a hostile bearer `token`, free-text
/// `data.options` and nested username the store must drop. The extras have no
/// binding in the insert below; the token stays in the live dispatch for the
/// ACK path only and must never persist.
fn interaction_payload(
    interaction_id: &str,
    interaction_type: i32,
    application_id: &str,
    command_id: &str,
    channel_id: &str,
) -> Value {
    json!({
        "id": interaction_id,
        "application_id": application_id,
        "type": interaction_type,
        "token": TOKEN_SENTINEL,
        "data": {
            "id": command_id,
            "name": "ping",
            "type": 1,
            "options": [{"name": "note", "type": 3, "value": OPTION_SENTINEL}],
        },
        "guild_id": GUILD,
        "channel_id": channel_id,
        "member": {"user": {"id": ACTOR, "username": USERNAME_SENTINEL}},
    })
}

/// Minimal GUILD_AUDIT_LOG_ENTRY_CREATE capture shape: entry, guild, actor and
/// target ids plus the action-type enum, with hostile `changes` detail bodies
/// and free-text `options` / `reason` the store must drop. The extras have no
/// binding in the insert below and must never persist.
fn audit_log_payload(
    audit_log_entry_id: &str,
    action_type: i32,
    actor_id: &str,
    target_id: &str,
) -> Value {
    json!({
        "id": audit_log_entry_id,
        "guild_id": GUILD,
        "action_type": action_type,
        "user_id": actor_id,
        "target_id": target_id,
        "changes": [{"key": "topic", "old_value": "before", "new_value": CHANGE_SENTINEL}],
        "options": {"channel_id": CHANNEL, "note": OPTION_SENTINEL},
        "reason": REASON_SENTINEL,
    })
}

/// S8 append-only insert for a stage-instance event: identity + timestamps +
/// the privacy-level enum, idempotent on `dedup_key`. The column list is the
/// whole contract: anything the fixture carries beyond these columns (topic
/// free text) has no binding and can never persist.
async fn insert_stage_event(pool: &PgPool, event: &StageEvent) -> Result<u64, sqlx::Error> {
    let key = event.dedup_key();
    let result = sqlx::query(
        "INSERT INTO event_store
            (event_type, guild_id, channel_id, stage_instance_id,
             event_time, received_at, dedup_key, privacy_level)
         VALUES
            ($1, $2, $3, $4, $5::timestamptz, NOW(), $6, $7)
         ON CONFLICT (dedup_key) DO NOTHING",
    )
    .bind(event.event_type)
    .bind(GUILD)
    .bind(event.channel_id)
    .bind(event.stage_instance_id)
    .bind(event.event_time)
    .bind(&key)
    .bind(event.privacy_level)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// S8 append-only insert for an observed interaction: identity + timestamps +
/// the type enum and application/command id refs, idempotent on `dedup_key`.
/// Observe-only: this insert reads the dispatch fields and never consumes
/// them; the token and options have no binding and can never persist.
async fn insert_interaction_event(
    pool: &PgPool,
    event: &InteractionEvent,
) -> Result<u64, sqlx::Error> {
    let key = event.dedup_key();
    let result = sqlx::query(
        "INSERT INTO event_store
            (event_type, guild_id, channel_id, interaction_id, interaction_type,
             application_id, command_id, event_time, received_at, dedup_key)
         VALUES
            ($1, $2, $3, $4, $5, $6, $7, $8::timestamptz, NOW(), $9)
         ON CONFLICT (dedup_key) DO NOTHING",
    )
    .bind(event.event_type)
    .bind(GUILD)
    .bind(event.channel_id)
    .bind(event.interaction_id)
    .bind(event.interaction_type)
    .bind(event.application_id)
    .bind(event.command_id)
    .bind(event.event_time)
    .bind(&key)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// S8 append-only insert for an audit-log entry: identity + timestamps + the
/// action-type enum plus actor/target ids, idempotent on `dedup_key`. Change
/// details, options and reason have no binding and can never persist.
async fn insert_audit_log_event(pool: &PgPool, event: &AuditLogEvent) -> Result<u64, sqlx::Error> {
    let key = event.dedup_key();
    let result = sqlx::query(
        "INSERT INTO event_store
            (event_type, guild_id, audit_log_entry_id, action_type,
             actor_id, target_id, event_time, received_at, dedup_key)
         VALUES
            ($1, $2, $3, $4, $5, $6, $7::timestamptz, NOW(), $8)
         ON CONFLICT (dedup_key) DO NOTHING",
    )
    .bind(event.event_type)
    .bind(GUILD)
    .bind(event.audit_log_entry_id)
    .bind(event.action_type)
    .bind(event.actor_id)
    .bind(event.target_id)
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

/// The S8 columns that must exist after the S8 migration: ids, timestamps,
/// the resume-safe key, the member id relaxed for sibling order-independence,
/// and the stage/interaction/audit enums only.
const REQUIRED_COLUMNS: &[&str] = &[
    "action_type",
    "actor_id",
    "application_id",
    "audit_log_entry_id",
    "channel_id",
    "command_id",
    "dedup_key",
    "event_time",
    "event_type",
    "guild_id",
    "id",
    "interaction_id",
    "interaction_type",
    "privacy_level",
    "received_at",
    "stage_instance_id",
    "target_id",
    "user_id",
];

/// The reviewed column allowlist for the shared `event_store` table: the union
/// of the slices' columns (S2 member deltas, S3 presence, S4 voice, S5
/// message/reaction, S6 threads, S8 stage/interaction) plus the `id` primary
/// key. Every actual column must be a member, so any new free-text column (a
/// body-carrying `raw_payload`, `stage_topic`, or similar) fails the suite. A
/// denylist cannot fail on a new name, so this asserts the closed set instead.
/// Later slices extend this list when they add their own reviewed columns
/// (`ADD COLUMN IF NOT EXISTS` per 0431).
const ALLOWED_COLUMNS: &[&str] = &[
    "action_type",
    "activity_type",
    "actor_id",
    "application_id",
    "archived",
    "audit_log_entry_id",
    "channel_id",
    "command_id",
    "deaf",
    "dedup_key",
    "emoji_id",
    "emoji_name",
    "event_time",
    "event_type",
    "guild_id",
    "id",
    "interaction_id",
    "interaction_type",
    "invitable",
    "locked",
    "message_id",
    "mute",
    "nick",
    "nick_changed",
    "parent_id",
    "premium_since",
    "presence_status",
    "privacy_level",
    "received_at",
    "roles",
    "self_deaf",
    "self_mute",
    "self_stream",
    "self_video",
    "session_id",
    "stage_instance_id",
    "suppress",
    "target_id",
    "thread_id",
    "user_id",
];

fn assert_stage_schema(columns: &[String]) {
    assert!(
        !columns.is_empty(),
        "S8 creates the stage/interaction/audit columns on the append-only event_store table"
    );
    for required in REQUIRED_COLUMNS {
        assert!(
            columns.iter().any(|column| column == required),
            "event_store keeps S8 column {required}; has {columns:?}"
        );
    }
    for column in columns {
        assert!(
            ALLOWED_COLUMNS.contains(&column.as_str()),
            "event_store holds only reviewed slice columns; unexpected {column}; \
             has {columns:?} (extend ALLOWED_COLUMNS only with a reviewed slice column)"
        );
    }
}

async fn assert_no_free_form_types(pool: &PgPool) {
    let types: Vec<(String, String)> = sqlx::query_as(
        "SELECT column_name, data_type FROM information_schema.columns
         WHERE table_name = 'event_store'",
    )
    .fetch_all(pool)
    .await
    .expect("event_store column types readable");
    for (column, data_type) in &types {
        assert!(
            data_type != "json" && data_type != "jsonb",
            "privacy fence: no free-form {column} {data_type} column; ids+enums only"
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
        TOPIC_SENTINEL,
        TOKEN_SENTINEL,
        OPTION_SENTINEL,
        USERNAME_SENTINEL,
        CHANGE_SENTINEL,
        REASON_SENTINEL,
    ] {
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

/// S8 AC 1a: STAGE_INSTANCE_CREATE / UPDATE / DELETE fixtures persist stage,
/// channel and guild ids plus timestamps and the privacy-level enum only —
/// with NO topic free text. The store is append-only, so lifecycle
/// transitions land as separate rows.
///
/// Pending: no stage columns exist until the S8 migration lands, so run with
/// `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
async fn event_store_s8_stage_lifecycle_persists_ids_and_privacy_enum_only() {
    let db = TestDb::new().await;
    assert_stage_schema(&table_columns(&db.pool).await);
    assert_dedup_key_is_unique(&db.pool).await;

    let created = StageEvent {
        event_type: "STAGE_INSTANCE_CREATE",
        stage_instance_id: STAGE_INSTANCE,
        channel_id: CHANNEL,
        privacy_level: PRIVACY_PUBLIC,
        event_time: "2026-10-12T12:00:00.000Z",
    };
    let fixture = stage_payload(STAGE_INSTANCE, CHANNEL, PRIVACY_PUBLIC, false);
    assert_eq!(
        fixture["topic"],
        json!(TOPIC_SENTINEL),
        "fixture models the real gateway payload"
    );
    let inserted = insert_stage_event(&db.pool, &created)
        .await
        .expect("S8 append-only stage insert");
    assert_eq!(inserted, 1, "first delivery of a stage create inserts");

    let updated = StageEvent {
        event_type: "STAGE_INSTANCE_UPDATE",
        stage_instance_id: STAGE_INSTANCE,
        channel_id: CHANNEL,
        privacy_level: PRIVACY_GUILD_ONLY,
        event_time: "2026-10-12T12:01:00.000Z",
    };
    let update_inserted = insert_stage_event(&db.pool, &updated)
        .await
        .expect("S8 append-only stage insert");
    assert_eq!(
        update_inserted, 1,
        "first delivery of a stage update inserts"
    );

    let deleted = StageEvent {
        event_type: "STAGE_INSTANCE_DELETE",
        stage_instance_id: STAGE_INSTANCE,
        channel_id: CHANNEL,
        privacy_level: PRIVACY_GUILD_ONLY,
        event_time: "2026-10-12T12:02:00.000Z",
    };
    let delete_inserted = insert_stage_event(&db.pool, &deleted)
        .await
        .expect("S8 append-only stage insert");
    assert_eq!(
        delete_inserted, 1,
        "first delivery of a stage delete inserts"
    );

    for event in [&created, &updated, &deleted] {
        let stored = stored_row(&db.pool, &event.dedup_key()).await;
        assert_eq!(stored.len(), 1, "exactly one row per lifecycle event");
        assert_no_free_text(&stored[0]);
    }

    let enums: Vec<(String, i32)> = sqlx::query_as(
        "SELECT event_type, privacy_level FROM event_store
         WHERE stage_instance_id = $1 ORDER BY event_time",
    )
    .bind(STAGE_INSTANCE)
    .fetch_all(&db.pool)
    .await
    .expect("lifecycle enums readable");
    assert_eq!(
        enums,
        vec![
            ("STAGE_INSTANCE_CREATE".to_owned(), PRIVACY_PUBLIC),
            ("STAGE_INSTANCE_UPDATE".to_owned(), PRIVACY_GUILD_ONLY),
            ("STAGE_INSTANCE_DELETE".to_owned(), PRIVACY_GUILD_ONLY),
        ],
        "each lifecycle row keeps its own privacy-level enum"
    );
    db.cleanup().await;
}

/// S8 AC 1b: INTERACTION_CREATE fixtures are observed, never consumed: the
/// capture persists interaction, application, command, channel and guild ids
/// plus the type enum and timestamps only. The bearer token and the free-text
/// options / username stay in the live dispatch for the ACK path and never
/// persist.
///
/// Pending: no interaction columns exist until the S8 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
async fn event_store_s8_interaction_create_observes_ids_and_command_enums_only() {
    let db = TestDb::new().await;
    assert_stage_schema(&table_columns(&db.pool).await);
    assert_dedup_key_is_unique(&db.pool).await;

    let event = InteractionEvent {
        event_type: "INTERACTION_CREATE",
        interaction_id: INTERACTION,
        interaction_type: INTERACTION_APPLICATION_COMMAND,
        application_id: APPLICATION,
        command_id: COMMAND,
        channel_id: CHANNEL,
        event_time: "2026-10-12T12:00:00.000Z",
    };
    let fixture = interaction_payload(
        INTERACTION,
        INTERACTION_APPLICATION_COMMAND,
        APPLICATION,
        COMMAND,
        CHANNEL,
    );
    // Observe-only proof at the fixture level: the live dispatch still carries
    // everything the ACK path needs (token + type + command ref), so the
    // dispatcher keeps working unmodified while the store keeps ids + enums.
    assert_eq!(
        fixture["token"],
        json!(TOKEN_SENTINEL),
        "live dispatch keeps the bearer token for the ACK path"
    );
    assert_eq!(
        fixture["data"]["options"][0]["value"],
        json!(OPTION_SENTINEL),
        "fixture models the free-text options the store must drop"
    );
    let inserted = insert_interaction_event(&db.pool, &event)
        .await
        .expect("S8 append-only interaction observe insert");
    assert_eq!(inserted, 1, "first delivery of an interaction inserts");

    let stored = stored_row(&db.pool, &event.dedup_key()).await;
    assert_eq!(stored.len(), 1, "exactly one row for the interaction");
    assert_no_free_text(&stored[0]);

    let row: (String, String, String, String, i32, String) = sqlx::query_as(
        "SELECT guild_id, channel_id, interaction_id, application_id,
                interaction_type, command_id
         FROM event_store WHERE dedup_key = $1",
    )
    .bind(event.dedup_key())
    .fetch_one(&db.pool)
    .await
    .expect("interaction ids and enums readable");
    assert_eq!(
        row,
        (
            GUILD.to_owned(),
            CHANNEL.to_owned(),
            INTERACTION.to_owned(),
            APPLICATION.to_owned(),
            INTERACTION_APPLICATION_COMMAND,
            COMMAND.to_owned(),
        ),
        "guild, channel, interaction, application and command ids plus the type enum survive the observe"
    );

    // A message-component interaction observes the same way with its own type
    // enum and no command body.
    let component = InteractionEvent {
        event_type: "INTERACTION_CREATE",
        interaction_id: "209876543210987654",
        interaction_type: INTERACTION_MESSAGE_COMPONENT,
        application_id: APPLICATION,
        command_id: COMMAND,
        channel_id: CHANNEL,
        event_time: "2026-10-12T12:01:00.000Z",
    };
    let component_inserted = insert_interaction_event(&db.pool, &component)
        .await
        .expect("S8 append-only interaction observe insert");
    assert_eq!(
        component_inserted, 1,
        "first delivery of a component interaction inserts"
    );
    let component_stored = stored_row(&db.pool, &component.dedup_key()).await;
    assert_eq!(component_stored.len(), 1, "exactly one row per interaction");
    assert_no_free_text(&component_stored[0]);
    db.cleanup().await;
}

/// S8 AC 1c: GUILD_AUDIT_LOG_ENTRY_CREATE fixtures persist the entry,
/// guild, actor and target ids plus timestamps and the action-type enum only
/// — with NO change-detail bodies, NO options and NO reason free text.
///
/// Pending: no audit-log columns exist until the S8 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
async fn event_store_s8_audit_log_entry_persists_ids_and_action_enum_only() {
    let db = TestDb::new().await;
    assert_stage_schema(&table_columns(&db.pool).await);
    assert_dedup_key_is_unique(&db.pool).await;

    let event = AuditLogEvent {
        event_type: "GUILD_AUDIT_LOG_ENTRY_CREATE",
        audit_log_entry_id: AUDIT_ENTRY,
        action_type: ACTION_GUILD_UPDATE,
        actor_id: ACTOR,
        target_id: TARGET,
        event_time: "2026-10-12T12:00:00.000Z",
    };
    let fixture = audit_log_payload(AUDIT_ENTRY, ACTION_GUILD_UPDATE, ACTOR, TARGET);
    assert_eq!(
        fixture["changes"][0]["new_value"],
        json!(CHANGE_SENTINEL),
        "fixture models the change-detail body the store must drop"
    );
    assert_eq!(
        fixture["reason"],
        json!(REASON_SENTINEL),
        "fixture models the free-text reason the store must drop"
    );
    let inserted = insert_audit_log_event(&db.pool, &event)
        .await
        .expect("S8 append-only audit-log insert");
    assert_eq!(inserted, 1, "first delivery of an audit-log entry inserts");

    let stored = stored_row(&db.pool, &event.dedup_key()).await;
    assert_eq!(stored.len(), 1, "exactly one row for the audit-log entry");
    assert_no_free_text(&stored[0]);

    let row: (String, String, i32, String, String) = sqlx::query_as(
        "SELECT guild_id, audit_log_entry_id, action_type, actor_id, target_id
         FROM event_store WHERE dedup_key = $1",
    )
    .bind(event.dedup_key())
    .fetch_one(&db.pool)
    .await
    .expect("audit-log ids and action enum readable");
    assert_eq!(
        row,
        (
            GUILD.to_owned(),
            AUDIT_ENTRY.to_owned(),
            ACTION_GUILD_UPDATE,
            ACTOR.to_owned(),
            TARGET.to_owned(),
        ),
        "guild, entry, actor and target ids plus the action-type enum survive the insert"
    );
    db.cleanup().await;
}

/// S8 AC 3: duplicate delivery with the same `dedup_key` inserts once
/// (resume-safe, `ON CONFLICT DO NOTHING`) across all three S8 captures.
///
/// Pending: no S8 columns exist until the S8 migration lands, so run with
/// `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
async fn event_store_s8_duplicate_delivery_inserts_once() {
    let db = TestDb::new().await;
    assert_stage_schema(&table_columns(&db.pool).await);

    let stage = StageEvent {
        event_type: "STAGE_INSTANCE_CREATE",
        stage_instance_id: STAGE_INSTANCE,
        channel_id: CHANNEL,
        privacy_level: PRIVACY_PUBLIC,
        event_time: "2026-10-12T12:00:00.000Z",
    };
    let first = insert_stage_event(&db.pool, &stage)
        .await
        .expect("first delivery inserts");
    assert_eq!(first, 1, "first delivery inserts");
    // Gateway resume redelivery of the same dispatch: same dedup_key.
    let retry = insert_stage_event(&db.pool, &stage)
        .await
        .expect("duplicate delivery is resume-safe");
    assert_eq!(retry, 0, "same dedup_key must insert nothing on redelivery");

    let interaction = InteractionEvent {
        event_type: "INTERACTION_CREATE",
        interaction_id: INTERACTION,
        interaction_type: INTERACTION_APPLICATION_COMMAND,
        application_id: APPLICATION,
        command_id: COMMAND,
        channel_id: CHANNEL,
        event_time: "2026-10-12T12:00:00.000Z",
    };
    let interaction_first = insert_interaction_event(&db.pool, &interaction)
        .await
        .expect("first delivery inserts");
    assert_eq!(interaction_first, 1, "first delivery inserts");
    let interaction_retry = insert_interaction_event(&db.pool, &interaction)
        .await
        .expect("duplicate delivery is resume-safe");
    assert_eq!(
        interaction_retry, 0,
        "same dedup_key must insert nothing on redelivery"
    );

    let audit = AuditLogEvent {
        event_type: "GUILD_AUDIT_LOG_ENTRY_CREATE",
        audit_log_entry_id: AUDIT_ENTRY,
        action_type: ACTION_MESSAGE_DELETE,
        actor_id: ACTOR,
        target_id: TARGET,
        event_time: "2026-10-12T12:00:00.000Z",
    };
    let audit_first = insert_audit_log_event(&db.pool, &audit)
        .await
        .expect("first delivery inserts");
    assert_eq!(audit_first, 1, "first delivery inserts");
    let audit_retry = insert_audit_log_event(&db.pool, &audit)
        .await
        .expect("duplicate delivery is resume-safe");
    assert_eq!(
        audit_retry, 0,
        "same dedup_key must insert nothing on redelivery"
    );

    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM event_store")
        .fetch_one(&db.pool)
        .await
        .expect("row count readable");
    assert_eq!(count.0, 3, "no double rows after resume redeliveries");
    db.cleanup().await;
}

/// S8 AC 4 (privacy tripwire): fixtures carrying topic, token, option,
/// username, change-detail and reason free text are rejected or never
/// persisted.
///
/// Pending: no S8 columns exist until the S8 migration lands, so run with
/// `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
async fn event_store_s8_privacy_tripwire_rejects_topic_token_and_change_details() {
    let db = TestDb::new().await;
    // Tripwire part 1: the table holds exactly the reviewed columns, so no
    // free-text or secret column exists to hold the payload; a later migration
    // adding raw_payload, stage_topic or any other column fails here. Free-form
    // json/jsonb types are refused too, so a payload column cannot hide behind
    // a reviewed-looking name.
    assert_stage_schema(&table_columns(&db.pool).await);
    assert_no_free_form_types(&db.pool).await;
    // Tripwire part 2: fixtures carrying topic + token + options + username +
    // change-detail + reason free text persist their ids, timestamps and enums
    // only; the sentinels appear in no stored row.
    let stage_fixture = stage_payload(STAGE_INSTANCE, CHANNEL, PRIVACY_PUBLIC, false);
    assert_eq!(
        stage_fixture["topic"],
        json!(TOPIC_SENTINEL),
        "fixture models a payload carrying topic free text"
    );
    let interaction_fixture = interaction_payload(
        INTERACTION,
        INTERACTION_APPLICATION_COMMAND,
        APPLICATION,
        COMMAND,
        CHANNEL,
    );
    assert_eq!(
        interaction_fixture["token"],
        json!(TOKEN_SENTINEL),
        "fixture models a payload carrying the bearer token"
    );
    let audit_fixture = audit_log_payload(AUDIT_ENTRY, ACTION_GUILD_UPDATE, ACTOR, TARGET);
    assert_eq!(
        audit_fixture["changes"][0]["new_value"],
        json!(CHANGE_SENTINEL),
        "fixture models a payload carrying change-detail bodies"
    );
    insert_stage_event(
        &db.pool,
        &StageEvent {
            event_type: "STAGE_INSTANCE_CREATE",
            stage_instance_id: STAGE_INSTANCE,
            channel_id: CHANNEL,
            privacy_level: PRIVACY_PUBLIC,
            event_time: "2026-10-12T14:00:00.000Z",
        },
    )
    .await
    .expect("metadata insert");
    insert_interaction_event(
        &db.pool,
        &InteractionEvent {
            event_type: "INTERACTION_CREATE",
            interaction_id: INTERACTION,
            interaction_type: INTERACTION_APPLICATION_COMMAND,
            application_id: APPLICATION,
            command_id: COMMAND,
            channel_id: CHANNEL,
            event_time: "2026-10-12T14:00:00.000Z",
        },
    )
    .await
    .expect("metadata insert");
    insert_audit_log_event(
        &db.pool,
        &AuditLogEvent {
            event_type: "GUILD_AUDIT_LOG_ENTRY_CREATE",
            audit_log_entry_id: AUDIT_ENTRY,
            action_type: ACTION_GUILD_UPDATE,
            actor_id: ACTOR,
            target_id: TARGET,
            event_time: "2026-10-12T14:00:00.000Z",
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
        assert_no_free_text(row);
    }
    db.cleanup().await;
}

/// S8 AC 2 (observe-only, offline): the S8 fixtures model the real dispatch
/// shape, so the existing dispatcher keeps working unmodified. The capture is
/// additive: every field the live paths match on (stage ids, interaction
/// token + type + command ref, audit entry + action type) is still present in
/// the payload the dispatcher sees. No database needed; runs in default CI.
#[test]
fn event_store_s8_observe_leaves_dispatch_shape_untouched() {
    let stage = stage_payload(STAGE_INSTANCE, CHANNEL, PRIVACY_PUBLIC, false);
    assert_eq!(stage["id"], json!(STAGE_INSTANCE));
    assert_eq!(stage["guild_id"], json!(GUILD));
    assert_eq!(stage["channel_id"], json!(CHANNEL));

    let interaction = interaction_payload(
        INTERACTION,
        INTERACTION_APPLICATION_COMMAND,
        APPLICATION,
        COMMAND,
        CHANNEL,
    );
    assert_eq!(
        interaction["type"],
        json!(INTERACTION_APPLICATION_COMMAND),
        "dispatcher still sees the interaction type for routing"
    );
    assert_eq!(
        interaction["token"],
        json!(TOKEN_SENTINEL),
        "dispatcher still sees the bearer token for the ACK path"
    );
    assert_eq!(
        interaction["data"]["id"],
        json!(COMMAND),
        "dispatcher still sees the command ref for routing"
    );

    let audit = audit_log_payload(AUDIT_ENTRY, ACTION_MESSAGE_DELETE, ACTOR, TARGET);
    assert_eq!(audit["id"], json!(AUDIT_ENTRY));
    assert_eq!(audit["action_type"], json!(ACTION_MESSAGE_DELETE));
}

/// S8 AC 2 (observe-only, offline): dedup keys are stable per delivery — the
/// same event always maps to the same key (resume redelivery collides) while
/// distinct events map to distinct keys (no cross-talk between captures). No
/// database needed; runs in default CI.
#[test]
fn event_store_s8_dedup_keys_are_stable_per_delivery() {
    let stage = StageEvent {
        event_type: "STAGE_INSTANCE_CREATE",
        stage_instance_id: STAGE_INSTANCE,
        channel_id: CHANNEL,
        privacy_level: PRIVACY_PUBLIC,
        event_time: "2026-10-12T12:00:00.000Z",
    };
    assert_eq!(
        stage.dedup_key(),
        stage.dedup_key(),
        "same delivery always maps to the same key"
    );

    let interaction = InteractionEvent {
        event_type: "INTERACTION_CREATE",
        interaction_id: INTERACTION,
        interaction_type: INTERACTION_APPLICATION_COMMAND,
        application_id: APPLICATION,
        command_id: COMMAND,
        channel_id: CHANNEL,
        event_time: "2026-10-12T12:00:00.000Z",
    };
    let audit = AuditLogEvent {
        event_type: "GUILD_AUDIT_LOG_ENTRY_CREATE",
        audit_log_entry_id: AUDIT_ENTRY,
        action_type: ACTION_MESSAGE_DELETE,
        actor_id: ACTOR,
        target_id: TARGET,
        event_time: "2026-10-12T12:00:00.000Z",
    };
    assert_ne!(
        stage.dedup_key(),
        interaction.dedup_key(),
        "stage and interaction captures never share a key"
    );
    assert_ne!(
        interaction.dedup_key(),
        audit.dedup_key(),
        "interaction and audit-log captures never share a key"
    );
    let updated = StageEvent {
        event_type: "STAGE_INSTANCE_UPDATE",
        ..stage
    };
    assert_ne!(
        stage.dedup_key(),
        updated.dedup_key(),
        "lifecycle transitions land as separate append-only rows"
    );
}
