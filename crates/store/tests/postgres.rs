//! Opt-in database tests. Only agent-testdb or a CI service container is
//! admitted, with the agent_test identity. DATABASE_URL is never consulted.
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Pool, Postgres};
use two_bot_core::{EventType, FunnelEvent, FunnelStore, InviteSnapshotStore, InviteState};
use two_bot_store::{migrate, PgFunnelStore, PgInviteSnapshots, DB_POOL_MAX};

static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    admin: Pool<Postgres>,
    pool: Pool<Postgres>,
    schema: String,
}

impl Fixture {
    async fn new() -> Self {
        let url = std::env::var("TEST_DATABASE_URL").expect("set TEST_DATABASE_URL for DB tests");
        let options = PgConnectOptions::from_str(&url).expect("invalid TEST_DATABASE_URL");
        let ci_container = std::env::var("CI").as_deref() == Ok("true")
            && matches!(options.get_host(), "localhost" | "127.0.0.1" | "postgres");
        assert!(
            options.get_host() == "agent-testdb" || ci_container,
            "test containers only"
        );
        assert_eq!(options.get_username(), "agent_test", "test identity only");
        let admin = two_bot_store::connect_pool(&url)
            .await
            .expect("test DB connect failed");
        let schema = format!(
            "s6_test_{}_{}",
            std::process::id(),
            NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed)
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(DB_POOL_MAX)
            .connect_with(options.options([
                ("search_path", schema.clone()),
                ("statement_timeout", "15000ms".to_owned()),
            ]))
            .await
            .unwrap();
        Self {
            admin,
            pool,
            schema,
        }
    }

    async fn finish(self) {
        self.pool.close().await;
        // Only schemas created by this fixture; never shared tables or roles.
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA IF EXISTS {}_web_v1 CASCADE; DROP SCHEMA {} CASCADE",
            self.schema, self.schema
        )))
        .execute(&self.admin)
        .await
        .unwrap();
        self.admin.close().await;
    }
}

fn event(t: EventType, at: &str) -> FunnelEvent {
    FunnelEvent {
        guild_id: 123,
        member_id: Some(456),
        event_type: t,
        occurred_at: at.to_owned(),
        source: "unknown".to_owned(),
        metadata: None,
        dedupe_token: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn migrations_contract_and_checksum_guard() {
    let f = Fixture::new().await;
    migrate(&f.pool).await.unwrap();
    migrate(&f.pool).await.unwrap();
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _two_bot_migrations ORDER BY version")
            .fetch_all(&f.pool)
            .await
            .unwrap();
    for v in [1, 3, 5, 7, 8, 9] {
        assert!(versions.contains(&v));
    }
    assert!(versions.iter().all(|v| (1..=999).contains(v)));
    let timeout: String = sqlx::query_scalar("SHOW statement_timeout")
        .fetch_one(&f.admin)
        .await
        .unwrap();
    assert_eq!(timeout, "15s");
    assert_eq!(f.admin.options().get_max_connections(), 5);
    two_bot_store::apply_web_contract(&f.pool).await.unwrap();
    two_bot_store::apply_web_contract(&f.pool).await.unwrap();
    let web = format!("{}_web_v1", f.schema);
    let version: String = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT contract_version FROM {web}.contract_meta"
    )))
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(version, "1.0");
    let counts: (Option<i32>, Option<i32>) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT human_member_count, online_count FROM {web}.live_counts"
    )))
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(counts, (None, None));
    let ranks: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM {web}.rank_counts"
    )))
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(ranks, 5);
    // Corrupt only this test's ledger: a deployed checksum mismatch must fail.
    sqlx::query("UPDATE _two_bot_migrations SET checksum = decode('00', 'hex') WHERE version = 1")
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(
        matches!(migrate(&f.pool).await, Err(sqlx::Error::Migrate(e))
        if matches!(*e, sqlx::migrate::MigrateError::VersionMismatch(1)))
    );
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn existing_cutover_schema_is_preserved_in_both_orders() {
    for cutover_first in [true, false] {
        let f = Fixture::new().await;
        let cutover = sqlx::migrate::Migrator::new(std::path::Path::new("../cutover/migrations"))
            .await
            .unwrap();
        if cutover_first {
            cutover.run(&f.pool).await.unwrap();
        }
        migrate(&f.pool).await.unwrap();
        let store = PgFunnelStore::new(f.pool.clone());
        let e = event(EventType::MemberJoin, "2026-09-29T12:00:00.000Z");
        assert!(store.record(e.clone()).inserted);
        if !cutover_first {
            cutover.run(&f.pool).await.unwrap();
        }
        migrate(&f.pool).await.unwrap();
        assert!(!store.record(e).inserted);
        let ledgers: (i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM _two_bot_migrations), (SELECT count(*) FROM _sqlx_migrations)")
            .fetch_one(&f.pool).await.unwrap();
        assert!(ledgers.0 >= 6);
        assert_eq!(ledgers.1, cutover.iter().count() as i64);
        f.finish().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn insert_first_projection_rungs_and_rollback() {
    let f = Fixture::new().await;
    migrate(&f.pool).await.unwrap();
    let store = PgFunnelStore::new(f.pool.clone());
    let at = "2026-09-29T12:00:00.000Z";
    let e = event(EventType::MemberJoin, at);
    let mut writers = Vec::new();
    for _ in 0..10 {
        let store = store.clone();
        let e = e.clone();
        writers.push(tokio::spawn(async move {
            store.try_record(&e).await.unwrap().inserted
        }));
    }
    let mut winners = 0;
    for w in writers {
        winners += usize::from(w.await.unwrap());
    }
    assert_eq!(winners, 1);
    assert!(store.has_event(123, 456, EventType::MemberJoin));
    assert!(!store.has_event(123, 456, EventType::GateCleared));
    for (t, stamp) in [
        (EventType::FirstMessage, at),
        (EventType::SecondMessage, "2026-09-29T12:00:01.000Z"),
        (EventType::ThirdMessage, "2026-09-29T12:00:02.000Z"),
    ] {
        assert_eq!(store.next_message_rung(123, 456, stamp), Some(t));
        assert!(store.record(event(t, stamp)).inserted);
        assert_eq!(store.next_message_rung(123, 456, stamp), None);
    }
    assert_eq!(
        store.next_message_rung(123, 456, "2026-09-29T12:00:03.000Z"),
        None
    );
    store.touch_activity(123, 456, "2026-09-29T11:00:00.000Z");
    let recency: String = sqlx::query_scalar("SELECT to_char(last_active_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') FROM members WHERE guild_id = '123' AND member_id = '456'")
        .fetch_one(&f.pool).await.unwrap();
    assert_eq!(recency, "2026-09-29T12:00:02.000Z");
    store.record(event(EventType::MemberLeave, "2026-09-29T13:00:00.000Z"));
    store.record(event(EventType::MemberJoin, "2026-09-29T14:00:00.000Z"));
    let left: bool = sqlx::query_scalar(
        "SELECT left_at IS NULL FROM members WHERE guild_id = '123' AND member_id = '456'",
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert!(left);
    // Projection failure cannot leave an orphan event behind.
    sqlx::raw_sql("ALTER TABLE members ADD CONSTRAINT test_reject CHECK (member_id <> '999')")
        .execute(&f.pool)
        .await
        .unwrap();
    let mut rejected = event(EventType::MemberJoin, at);
    rejected.member_id = Some(999);
    assert!(store.try_record(&rejected).await.is_err());
    assert!(!store.has_event(123, 999, EventType::MemberJoin));
    let mut voice = event(EventType::VoiceSessionEnd, at);
    voice.metadata =
        Some(serde_json::json!({ "startKnown": true, "startedAt": at, "durationSeconds": 60 }));
    assert!(store.record(voice.clone()).inserted);
    let metadata: String =
        sqlx::query_scalar("SELECT metadata FROM events WHERE event_type = 'voice_session_end'")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(metadata, voice.metadata.unwrap().to_string());
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn invite_snapshots_survive_reconstruction_and_delete_is_guild_scoped() {
    let f = Fixture::new().await;
    migrate(&f.pool).await.unwrap();
    let snapshots = PgInviteSnapshots::new(f.pool.clone());
    let mut invite = InviteState {
        code: "fixture".to_owned(),
        uses: 5,
        inviter_id: Some(789),
        channel_id: Some(321),
    };
    snapshots.store_all(123, &[invite.clone()]);
    snapshots.store_all(124, &[invite.clone()]);
    invite.uses = 6;
    snapshots.store_all(123, &[invite.clone()]);
    let restarted = PgInviteSnapshots::new(f.pool.clone());
    assert_eq!(restarted.load(123), vec![invite]);
    two_bot_core::InviteTracker::new(restarted.clone()).seed(
        123,
        InviteState {
            code: "new-code".to_owned(),
            uses: 0,
            inviter_id: None,
            channel_id: None,
        },
    );
    assert_eq!(
        restarted.load(123).len(),
        2,
        "seeding must not prune the guild baseline"
    );
    restarted.delete_missing(123, &HashSet::new());
    assert!(restarted.load(123).is_empty());
    assert_eq!(restarted.load(124).len(), 1);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn mock_gateway_join_records_funnel_and_persisted_attribution() {
    use twilight_model::gateway::{event::Event, payload::incoming::MemberAdd};
    use two_bot_core::{NoopFacts, NoopLeveling};
    use two_bot_discord::{NoClassification, Pipeline, ScriptedInvites};

    let f = Fixture::new().await;
    migrate(&f.pool).await.unwrap();
    let pipeline = Pipeline::with_snapshots(
        PgFunnelStore::new(f.pool.clone()),
        Some(NoopLeveling),
        Some(NoopFacts),
        ScriptedInvites::new(),
        NoClassification,
        PgInviteSnapshots::new(f.pool.clone()),
    );
    let invite = InviteState {
        code: "fixture".to_owned(),
        uses: 5,
        inviter_id: Some(789),
        channel_id: None,
    };
    pipeline.invite_source().push(123, vec![invite.clone()]);
    pipeline.prime_invite_snapshot(123);
    pipeline
        .invite_source()
        .push(123, vec![InviteState { uses: 6, ..invite }]);
    let member: MemberAdd = serde_json::from_value(serde_json::json!({
        "guild_id": "123", "user": {"id":"456", "username":"fixture", "discriminator":"0", "avatar":null, "bot":false},
        "roles":[], "joined_at":"2026-09-29T12:00:00.000+00:00", "deaf":false, "mute":false, "flags":0, "pending":false
    })).unwrap();
    let event = Event::MemberAdd(Box::new(member));
    pipeline.handle(&event);
    pipeline.handle(&event); // No second REST fixture: failure retains baseline.
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT event_type, source FROM events ORDER BY id")
            .fetch_all(&f.pool)
            .await
            .unwrap();
    assert_eq!(
        rows,
        vec![
            ("member_join".to_owned(), "invite:fixture".to_owned()),
            ("gate_cleared".to_owned(), "gateway".to_owned())
        ]
    );
    assert_eq!(PgInviteSnapshots::new(f.pool.clone()).load(123)[0].uses, 6);
    f.finish().await;
}
