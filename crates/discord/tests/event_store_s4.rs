//! Event-store S4 acceptance tests (paired failing-tests card for the S4 build).
//!
//! Test files ONLY: this suite pins the S4 acceptance criteria before the
//! builder starts, so every test here FAILS until the S4 build lands its
//! voice-capture migration on the append-only store. The builder makes these
//! tests pass; nothing outside test code changes on this card.
//!
//! Contract under test (event-store P1 slice 4: VOICE_STATE_UPDATE plus
//! voice-server-update / voice-effect / voice-status minimal capture into the
//! append-only `event_store` table):
//!
//! - One row per voice event with minimal fields: `event_type`,
//!   `guild_id`, `user_id`, `channel_id`, `session_id`, `event_time`,
//!   `received_at`, `dedup_key` plus the voice enum flags `mute`, `deaf`,
//!   `self_mute`, `self_deaf`, `self_video`, `self_stream` and `suppress`.
//!   No audio columns, no transcript columns: no audio bytes, no transcript
//!   text, no voice token, no endpoint.
//! - Resume-safe idempotent inserts: `dedup_key` carries a UNIQUE key such as
//!   `VOICE_STATE_UPDATE:<guild>:<user>:<event_time>` and every insert runs
//!   `ON CONFLICT (dedup_key) DO NOTHING`, so a gateway resume redelivery
//!   inserts nothing.
//! - Privacy tripwire: a fixture carrying free-text effect or channel-status
//!   payload persists its ids and flags only; the sentinels appear in no
//!   stored row.
//!
//! Pending-test routing (established pattern): every test is ignored and
//! skipped in the nightly broad sweep, so this card's CI stays green while
//! the S4 build is outstanding; run with `-- --ignored` to see the gap. The
//! S4 build card un-ignores these tests once the migration lands. DB tests
//! run only against the disposable test database (`TestDatabase` refuses
//! anything but agent-testdb), never staging or production.
#![cfg(feature = "db")]

use serde_json::{json, Value};
use sqlx::PgPool;
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "4444";
const USER: &str = "77";
const CHANNEL: &str = "8888";
const SESSION: &str = "sess-s4-fixture-01";

/// Sentinel audio payload: models an audio-bytes field the store must refuse
/// to persist. If this string ever appears in a persisted row, the
/// ids+timestamps+flags-only fence broke.
const AUDIO_SENTINEL: &str = "s4-audio-sentinel-must-never-persist";

/// Sentinel transcript text: models a transcript field the store must refuse
/// to persist.
const TRANSCRIPT_SENTINEL: &str = "s4-transcript-sentinel-must-never-persist";

/// Sentinel voice-server token: the VOICE_SERVER_UPDATE dispatch carries a
/// short-lived token that must never be stored.
const TOKEN_SENTINEL: &str = "s4-voice-token-sentinel-must-never-persist";

/// Sentinel voice-server endpoint: same rule as the token, URL payload shape.
const ENDPOINT_SENTINEL: &str = "s4-voice-endpoint-must-never-persist.example.invalid:443";

/// Sentinel voice-effect payload: models free-text effect content (emoji /
/// animation metadata) the store must refuse to persist.
const EFFECT_SENTINEL: &str = "s4-effect-sentinel-must-never-persist";

/// Sentinel voice-channel status text: models the free-text channel status
/// the store must refuse to persist.
const STATUS_SENTINEL: &str = "s4-status-sentinel-must-never-persist";

/// Voice identity plus session and mute/deaf/video/stream flags read back
/// from `event_store`: kept as one alias so clippy's `type_complexity`
/// lint stays quiet.
type VoiceStateRow = (
    String,
    String,
    Option<String>,
    String,
    bool,
    bool,
    bool,
    bool,
    bool,
    bool,
    bool,
);

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
struct VoiceFlags {
    mute: bool,
    deaf: bool,
    self_mute: bool,
    self_deaf: bool,
    self_video: bool,
    self_stream: bool,
    suppress: bool,
}

struct VoiceEvent {
    event_type: &'static str,
    guild_id: &'static str,
    user_id: &'static str,
    channel_id: Option<&'static str>,
    session_id: &'static str,
    event_time: &'static str,
    flags: VoiceFlags,
}

impl VoiceEvent {
    fn dedup_key(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.event_type, self.guild_id, self.user_id, self.event_time
        )
    }
}

const JOIN_FLAGS: VoiceFlags = VoiceFlags {
    mute: false,
    deaf: false,
    self_mute: true,
    self_deaf: false,
    self_video: true,
    self_stream: false,
    suppress: false,
};

const QUIET_FLAGS: VoiceFlags = VoiceFlags {
    mute: false,
    deaf: false,
    self_mute: false,
    self_deaf: false,
    self_video: false,
    self_stream: false,
    suppress: false,
};

/// Minimal VOICE_STATE_UPDATE gateway shape: ids + session id + the voice
/// enum flags, plus hostile `audio` / `transcript` extras a mis-shaped
/// payload could carry. The extras have no binding in the insert below and
/// must never persist.
fn voice_state_payload(event: &VoiceEvent) -> Value {
    json!({
        "guild_id": event.guild_id,
        "user_id": event.user_id,
        "channel_id": event.channel_id,
        "session_id": event.session_id,
        "mute": event.flags.mute,
        "deaf": event.flags.deaf,
        "self_mute": event.flags.self_mute,
        "self_deaf": event.flags.self_deaf,
        "self_video": event.flags.self_video,
        "self_stream": event.flags.self_stream,
        "suppress": event.flags.suppress,
        "request_to_speak_timestamp": null,
        "audio": AUDIO_SENTINEL,
        "transcript": TRANSCRIPT_SENTINEL,
        "observed_at": event.event_time,
    })
}

/// Minimal VOICE_SERVER_UPDATE gateway shape: the dispatch carries a
/// short-lived token and endpoint for the voice websocket handshake. Neither
/// may persist; only the guild (plus the joining user's ids) is captured.
fn voice_server_payload() -> Value {
    json!({
        "guild_id": GUILD,
        "token": TOKEN_SENTINEL,
        "endpoint": ENDPOINT_SENTINEL,
    })
}

/// S4 append-only insert for a voice event: identity + session + timestamps +
/// the voice enum flags, idempotent on `dedup_key`. Returns rows inserted (1)
/// or deduplicated (0). The column list is the whole contract: anything the
/// fixture carries beyond these columns (audio, transcript, token, endpoint,
/// effect text, status text) has no binding and can never persist.
async fn insert_voice_event(pool: &PgPool, event: &VoiceEvent) -> Result<u64, sqlx::Error> {
    let key = event.dedup_key();
    let flags = event.flags;
    let result = sqlx::query(
        "INSERT INTO event_store
            (event_type, guild_id, user_id, channel_id, session_id,
             event_time, received_at, dedup_key,
             mute, deaf, self_mute, self_deaf, self_video, self_stream, suppress)
         VALUES
            ($1, $2, $3, $4, $5, $6::timestamptz, NOW(), $7,
             $8, $9, $10, $11, $12, $13, $14)
         ON CONFLICT (dedup_key) DO NOTHING",
    )
    .bind(event.event_type)
    .bind(event.guild_id)
    .bind(event.user_id)
    .bind(event.channel_id)
    .bind(event.session_id)
    .bind(event.event_time)
    .bind(&key)
    .bind(flags.mute)
    .bind(flags.deaf)
    .bind(flags.self_mute)
    .bind(flags.self_deaf)
    .bind(flags.self_video)
    .bind(flags.self_stream)
    .bind(flags.suppress)
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
    "channel_id",
    "session_id",
    "event_time",
    "received_at",
    "dedup_key",
    "mute",
    "deaf",
    "self_mute",
    "self_deaf",
    "self_video",
    "self_stream",
    "suppress",
];

const FORBIDDEN_COLUMNS: &[&str] = &[
    "audio",
    "audio_data",
    "audio_bytes",
    "transcript",
    "transcript_text",
    "transcription",
    "token",
    "endpoint",
    "effect",
    "effect_text",
    "status_text",
    "username",
];

fn assert_voice_schema(columns: &[String]) {
    assert!(
        !columns.is_empty(),
        "S4 creates the voice columns on the append-only event_store table"
    );
    for required in REQUIRED_COLUMNS {
        assert!(
            columns.iter().any(|column| column == required),
            "event_store keeps voice column {required}; has {columns:?}"
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

fn assert_no_voice_free_text(row: &str) {
    for sentinel in [
        AUDIO_SENTINEL,
        TRANSCRIPT_SENTINEL,
        TOKEN_SENTINEL,
        ENDPOINT_SENTINEL,
        EFFECT_SENTINEL,
        STATUS_SENTINEL,
    ] {
        assert!(
            !row.contains(sentinel),
            "persisted row carries ids+timestamps+flag enums only: {row}"
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

/// S4 AC 1a: a VOICE_STATE_UPDATE join fixture persists user, guild, channel
/// and session ids, timestamps and the mute/deaf/video/stream enum flags
/// only — with NO audio and NO transcript fields.
///
/// Pending: no voice columns exist until the S4 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
#[ignore = "pending S4 build: needs disposable test database event_store voice columns; run with --ignored to see the gap"]
async fn event_store_s4_voice_state_join_persists_ids_session_and_flags_only() {
    let db = TestDb::new().await;
    assert_voice_schema(&table_columns(&db.pool).await);
    assert_dedup_key_is_unique(&db.pool).await;

    let event = VoiceEvent {
        event_type: "VOICE_STATE_UPDATE",
        guild_id: GUILD,
        user_id: USER,
        channel_id: Some(CHANNEL),
        session_id: SESSION,
        event_time: "2026-10-12T12:00:00.000Z",
        flags: JOIN_FLAGS,
    };
    let fixture = voice_state_payload(&event);
    assert_eq!(
        fixture["session_id"],
        json!(SESSION),
        "fixture models the real gateway payload"
    );
    assert_eq!(
        fixture["audio"],
        json!(AUDIO_SENTINEL),
        "fixture models a payload carrying audio bytes"
    );
    let inserted = insert_voice_event(&db.pool, &event)
        .await
        .expect("S4 append-only voice insert");
    assert_eq!(inserted, 1, "first delivery of a voice join inserts");

    let stored = stored_row(&db.pool, &event.dedup_key()).await;
    assert_eq!(stored.len(), 1, "exactly one row for the voice join");
    assert_no_voice_free_text(&stored[0]);

    let row: VoiceStateRow = sqlx::query_as(
        "SELECT guild_id, user_id, channel_id, session_id,
                mute, deaf, self_mute, self_deaf, self_video, self_stream, suppress
         FROM event_store WHERE dedup_key = $1",
    )
    .bind(event.dedup_key())
    .fetch_one(&db.pool)
    .await
    .expect("voice ids and flags readable");
    assert_eq!(
        row,
        (
            GUILD.to_owned(),
            USER.to_owned(),
            Some(CHANNEL.to_owned()),
            SESSION.to_owned(),
            false,
            false,
            true,
            false,
            true,
            false,
            false,
        ),
        "ids, session and the mute/deaf/video/stream flags survive the insert"
    );
    db.cleanup().await;
}

/// S4 AC 1b: a VOICE_SERVER_UPDATE fixture persists guild (and joining-user)
/// ids and timestamps only — the handshake token and endpoint never persist.
///
/// Pending: no voice columns exist until the S4 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
#[ignore = "pending S4 build: needs disposable test database event_store voice columns; run with --ignored to see the gap"]
async fn event_store_s4_voice_server_update_persists_ids_without_token_or_endpoint() {
    let db = TestDb::new().await;
    assert_voice_schema(&table_columns(&db.pool).await);
    assert_dedup_key_is_unique(&db.pool).await;

    let fixture = voice_server_payload();
    assert_eq!(
        fixture["token"],
        json!(TOKEN_SENTINEL),
        "fixture models the real gateway handshake payload"
    );
    // A server update names no channel of its own: the capture binds the
    // joining user's ids with a NULL channel and the session under
    // negotiation. The token and endpoint have no binding.
    let event = VoiceEvent {
        event_type: "VOICE_SERVER_UPDATE",
        guild_id: GUILD,
        user_id: USER,
        channel_id: None,
        session_id: SESSION,
        event_time: "2026-10-12T12:01:00.000Z",
        flags: QUIET_FLAGS,
    };
    let inserted = insert_voice_event(&db.pool, &event)
        .await
        .expect("S4 append-only voice insert");
    assert_eq!(
        inserted, 1,
        "first delivery of a voice server update inserts"
    );

    let stored = stored_row(&db.pool, &event.dedup_key()).await;
    assert_eq!(stored.len(), 1, "exactly one row for the server update");
    assert_no_voice_free_text(&stored[0]);

    let ids: (String, Option<String>) =
        sqlx::query_as("SELECT guild_id, channel_id FROM event_store WHERE dedup_key = $1")
            .bind(event.dedup_key())
            .fetch_one(&db.pool)
            .await
            .expect("server-update ids readable");
    assert_eq!(
        ids,
        (GUILD.to_owned(), None),
        "the server update keeps guild ids with a NULL channel"
    );
    db.cleanup().await;
}

/// S4 AC 2: duplicate delivery with the same `dedup_key` inserts once
/// (resume-safe, `ON CONFLICT DO NOTHING`).
///
/// Pending: no voice columns exist until the S4 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
#[ignore = "pending S4 build: needs disposable test database event_store voice columns; run with --ignored to see the gap"]
async fn event_store_s4_duplicate_delivery_inserts_once() {
    let db = TestDb::new().await;
    assert_voice_schema(&table_columns(&db.pool).await);

    let event = VoiceEvent {
        event_type: "VOICE_STATE_UPDATE",
        guild_id: GUILD,
        user_id: USER,
        channel_id: Some(CHANNEL),
        session_id: SESSION,
        event_time: "2026-10-12T12:00:00.000Z",
        flags: JOIN_FLAGS,
    };
    let first = insert_voice_event(&db.pool, &event)
        .await
        .expect("first delivery inserts");
    assert_eq!(first, 1, "first delivery inserts");
    // Gateway resume redelivery of the same dispatch: same dedup_key.
    let retry = insert_voice_event(&db.pool, &event)
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

/// S4 AC 3 (privacy tripwire): a fixture carrying free-text voice-effect and
/// channel-status payload is rejected or never persisted.
///
/// Pending: no voice columns exist until the S4 migration lands, so run
/// with `-- --ignored` against the disposable test database to see the gap.
#[tokio::test]
#[ignore = "pending S4 build: needs disposable test database event_store voice columns; run with --ignored to see the gap"]
async fn event_store_s4_privacy_tripwire_rejects_effect_and_status_text() {
    let db = TestDb::new().await;
    // Tripwire part 1: no free-text column may exist to hold the payload.
    let columns = table_columns(&db.pool).await;
    assert!(
        !columns.is_empty(),
        "S4 creates the voice columns on the append-only event_store table"
    );
    for forbidden in FORBIDDEN_COLUMNS {
        assert!(
            !columns.iter().any(|column| column == forbidden),
            "privacy fence: no {forbidden} column; has {columns:?}"
        );
    }
    // Tripwire part 2: a fixture carrying effect + status free text persists
    // its ids and flags only; the sentinels appear in no stored row.
    let event = VoiceEvent {
        event_type: "VOICE_STATE_UPDATE",
        guild_id: GUILD,
        user_id: USER,
        channel_id: Some(CHANNEL),
        session_id: SESSION,
        event_time: "2026-10-12T14:00:00.000Z",
        flags: QUIET_FLAGS,
    };
    let mut fixture = voice_state_payload(&event);
    fixture["effect"] = json!({"emoji": EFFECT_SENTINEL});
    fixture["status"] = json!(STATUS_SENTINEL);
    assert_eq!(
        fixture["effect"]["emoji"],
        json!(EFFECT_SENTINEL),
        "fixture models a payload carrying effect free text"
    );
    insert_voice_event(&db.pool, &event)
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
            !row.contains(EFFECT_SENTINEL) && !row.contains(STATUS_SENTINEL),
            "privacy fence: effect and status text never persist: {row}"
        );
    }
    db.cleanup().await;
}
