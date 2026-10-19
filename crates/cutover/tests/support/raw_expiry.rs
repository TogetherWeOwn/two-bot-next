use std::future::Future;

use sqlx::PgPool;
use time::OffsetDateTime;
use twilight_model::gateway::event::EventType;
use two_bot_testsupport::TestDatabase;

pub const GUILD_A: &str = "111111111111111111";
pub const GUILD_B: &str = "222222222222222222";
pub const MEMBER: &str = "333333333333333333";

/// Mirror the reviewed capture schema with the actual Discord dispatch enum.
/// There is no public raw-capture writer at the pinned main baseline; this is
/// fixture setup, not evidence that a producer or ingest-role ACL is installed.
/// Audit IDs encode event_time using Discord's timestamp epoch, not receipt time.
pub async fn insert_capture(
    pool: &PgPool,
    kind: EventType,
    guild: &str,
    key: &str,
    event_time: OffsetDateTime,
    received_at: OffsetDateTime,
) -> u64 {
    let presence = kind == EventType::PresenceUpdate;
    let voice = kind == EventType::VoiceStateUpdate;
    let audit = kind == EventType::GuildAuditLogEntryCreate;
    let entry_id = audit.then(|| {
        let millis = u64::try_from(event_time.unix_timestamp_nanos() / 1_000_000)
            .expect("fixture event timestamp is after Discord's epoch");
        ((millis - 1_420_070_400_000) << 22).to_string()
    });
    sqlx::query(
        "INSERT INTO event_store
         (event_type, guild_id, user_id, event_time, received_at, dedup_key,
          presence_status, activity_type, channel_id, session_id,
          audit_log_entry_id, action_type, actor_id, target_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
         ON CONFLICT (dedup_key) DO NOTHING",
    )
    .bind(kind.name().expect("fixture is a named Discord dispatch"))
    .bind(guild)
    .bind((presence || voice).then_some(MEMBER))
    .bind(event_time)
    .bind(received_at)
    .bind(key)
    .bind(presence.then_some("online"))
    .bind(presence.then_some(0_i32))
    .bind(voice.then_some("444444444444444444"))
    .bind(voice.then_some("raw-expiry-session"))
    .bind(entry_id)
    .bind(audit.then_some(20_i32))
    .bind(audit.then_some(MEMBER))
    .bind(audit.then_some("555555555555555555"))
    .execute(pool)
    .await
    .expect("insert capture fixture")
    .rows_affected()
}

/// A configured setup failure is an error, never a passing/skipped acceptance.
/// Keep the runtime alive for explicit fixture teardown even when RED panics.
pub async fn with_database<F, Fut>(check: F)
where
    F: FnOnce(PgPool) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("raw-expiry acceptance requires the disposable test bootstrap");
    let db = TestDatabase::create(&url, &sqlx::migrate!("./migrations"))
        .await
        .expect("create migrated disposable raw-expiry fixture");
    let result = tokio::spawn(check(db.pool().clone())).await;
    db.close()
        .await
        .expect("verify disposable fixture teardown");
    if let Err(error) = result {
        if error.is_panic() {
            std::panic::resume_unwind(error.into_panic());
        }
        panic!("raw-expiry acceptance task was cancelled");
    }
}

/// Snapshot every public table, not just rows expected to expire: dry-run must
/// not advance a queue, change a receipt, mutate an aggregate or clear a fence.
/// Relation names come from this disposable database's catalog, quoted as SQL
/// identifiers; no external input is interpolated.
pub async fn snapshot(pool: &PgPool) -> Vec<(String, String)> {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename::text FROM pg_tables WHERE schemaname = 'public' ORDER BY tablename",
    )
    .fetch_all(pool)
    .await
    .expect("list disposable fixture tables");
    let mut rows = Vec::new();
    for table in tables {
        let quoted = table.replace('"', "\"\"");
        let data: String = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text), '[]'::jsonb)::text \
             FROM public.\"{quoted}\" t"
        )))
        .fetch_one(pool)
        .await
        .expect("snapshot fixture table");
        rows.push((table, data));
    }
    let sequences: String = sqlx::query_scalar(
        "SELECT COALESCE(jsonb_agg(jsonb_build_array(sequencename, last_value) \
         ORDER BY sequencename), '[]'::jsonb)::text FROM pg_sequences WHERE schemaname = 'public'",
    )
    .fetch_one(pool)
    .await
    .expect("snapshot fixture sequences");
    rows.push(("sequences".to_owned(), sequences));
    rows
}
