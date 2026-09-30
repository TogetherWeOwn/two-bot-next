//! Opt-in integration proof. Never reads DATABASE_URL or Discord credentials.
#![cfg(feature = "db")]

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection, PgPool};
use two_bot_core::onboarding::*;
use two_bot_core::onboarding_store::*;

const TEST_DATABASE: &str = "postgres://agent_test@agent-testdb:5432/postgres";
const AT: &str = "2026-08-24T00:00:00.000Z";
const LATER: &str = "2026-08-25T00:00:00.000Z";
const BASE: &str = include_str!("../../cutover/migrations/0001_funnel.sql");
const MIGRATION: &str = include_str!("../../cutover/migrations/0190_onboarding.sql");

async fn isolated_pool(schema: &str) -> PgPool {
    assert!(schema
        .bytes()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_'));
    let search_path = schema.to_owned();
    PgPoolOptions::new()
        .max_connections(4)
        .after_connect(move |connection, _| {
            let statement = search_path.clone();
            Box::pin(async move {
                sqlx::query("SELECT set_config('search_path', $1, false)")
                    .bind(statement)
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect(TEST_DATABASE)
        .await
        .expect("agent-testdb: expected agent_test with empty password")
}

// No transport: records only the plain-data effects handed to the shared
// executor. Tests cannot send a DM, touch a live guild or change real roles.
#[derive(Clone, Default)]
struct MockDiscord(Arc<Mutex<Vec<WelcomeEffect>>>);

impl MockDiscord {
    fn send(&self, effect: WelcomeEffect) {
        assert!(matches!(effect, WelcomeEffect::Post { .. }));
        self.0.lock().expect("mock").push(effect);
    }

    fn sent(&self) -> usize {
        self.0.lock().expect("mock").len()
    }
}

#[tokio::test]
#[ignore = "requires agent-testdb; explicitly run with --features db --ignored"]
async fn onboarding_migration_store_and_mock_delivery() {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let schema_a = format!("tog10086_onboarding_{stamp}_a");
    let schema_b = format!("tog10086_onboarding_{stamp}_b");
    let mut admin = PgConnection::connect(TEST_DATABASE)
        .await
        .expect("agent-testdb");
    for schema in [&schema_a, &schema_b] {
        // Identifiers contain only this fixed prefix, numeric clock and a/b.
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&mut admin)
            .await
            .unwrap();
    }
    let a = isolated_pool(&schema_a).await;
    let b = isolated_pool(&schema_b).await;

    // Two simultaneously-present schemas expose database-wide constraint-name
    // guards. Apply twice too: migration is safe to replay in either schema.
    for pool in [&a, &b] {
        sqlx::raw_sql(BASE).execute(pool).await.unwrap();
        sqlx::raw_sql(MIGRATION).execute(pool).await.unwrap();
        sqlx::raw_sql(MIGRATION).execute(pool).await.unwrap();
        let (constraint_count,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM pg_constraint WHERE conrelid = 'events'::regclass
             AND conname = 'chk_events_onboarding_vocabulary'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(constraint_count, 1);
        let (index_count,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM pg_indexes WHERE schemaname = current_schema()
             AND indexname IN ('idx_events_onboarding_prompt', 'idx_events_route_time')",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(index_count, 2);
    }

    assert!(!has_onboarding_prompt(&a, "1", "2").await.unwrap());
    assert!(record_prompted(&a, "1", "2", "3", AT).await.unwrap());
    assert!(!record_prompted(&a, "1", "2", "9", LATER).await.unwrap());
    assert!(has_onboarding_prompt(&a, "1", "2").await.unwrap());
    assert!(
        !has_onboarding_prompt(&b, "1", "2").await.unwrap(),
        "no pool leakage"
    );
    assert!(
        record_prompted(&a, "9", "2", "3", AT).await.unwrap(),
        "guild scoped"
    );
    let (source, key): (String, String) = sqlx::query_as(
        "SELECT source, idempotency_key FROM events WHERE guild_id = '1' AND member_id = '2'",
    )
    .fetch_one(&a)
    .await
    .unwrap();
    assert_eq!(
        (source.as_str(), key.as_str()),
        ("channel:3", "1:2:onboarding_prompted")
    );

    let selected = plan_game_selection(&["shooters"], &|_| true);
    let keys: Vec<String> = selected
        .destinations
        .iter()
        .map(|d| d.key.clone())
        .collect();
    assert!(record_game_selected(&a, "1", "2", &keys, AT).await.unwrap());
    assert!(!record_game_selected(&a, "1", "2", &keys, AT).await.unwrap());
    assert!(record_game_selected(&a, "1", "2", &keys, LATER)
        .await
        .unwrap());
    assert!(record_channel_routed(&a, "1", "2", &selected, AT)
        .await
        .unwrap());
    let picks = build_session_picks("10", "11");
    let session = plan_session(&["find-players"], &|_| true, &picks);
    assert!(record_session_routed(&a, "1", "2", &session, LATER)
        .await
        .unwrap());
    let (metadata, source): (String, String) =
        sqlx::query_as("SELECT metadata, source FROM events WHERE idempotency_key = $1")
            .bind(format!("1:2:channel_routed:{LATER}"))
            .fetch_one(&a)
            .await
            .unwrap();
    assert_eq!(source, "session-picker");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&metadata).unwrap(),
        serde_json::json!({"picks": ["find-players"], "channels": ["10"], "unavailable": []})
    );

    // Independent pools stand in for two gateway handlers/processes. The
    // check/send/record interval must be serialized, not just the row insert.
    let second_process = isolated_pool(&schema_a).await;
    for mode in [
        OnboardingMode::Legacy,
        OnboardingMode::Session,
        OnboardingMode::Anchor,
    ] {
        let member = format!("member-{}", mode.as_str());
        let mock = MockDiscord::default();
        let mut tasks = Vec::new();
        for i in 0..12 {
            let pool = if i % 2 == 0 {
                a.clone()
            } else {
                second_process.clone()
            };
            let member = member.clone();
            let mock = mock.clone();
            tasks.push(tokio::spawn(async move {
                if let Some(guard) = begin_prompt(&pool, "mock", &member).await.unwrap() {
                    mock.send(adjudicate_welcome(
                        mode,
                        42,
                        Some("3"),
                        "4",
                        SUNDAY_SQUAD.series_start_epoch,
                        SUNDAY_SQUAD,
                        false,
                    ));
                    // Widen the race: another handler can run before recording.
                    tokio::task::yield_now().await;
                    assert!(guard.record_sent("3", AT).await.unwrap());
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(mock.sent(), 1, "one prompt per member in {mode:?}");
        assert!(begin_prompt(&a, "mock", &member).await.unwrap().is_none());
    }

    // A rejected send never writes a successful-prompt marker, and the lock
    // rolls back when its guard is dropped; a subsequent event can retry.
    let failed_send = begin_prompt(&a, "mock", "retry").await.unwrap().unwrap();
    drop(failed_send);
    let retry = begin_prompt(&second_process, "mock", "retry")
        .await
        .unwrap()
        .unwrap();
    assert!(retry.record_sent("3", AT).await.unwrap());

    // Session welcomes run under dry run; game/anchor welcomes do not consume
    // the durable guard when their plain-data outcome is a skip.
    let dry_mock = MockDiscord::default();
    for mode in [
        OnboardingMode::Legacy,
        OnboardingMode::Session,
        OnboardingMode::Anchor,
    ] {
        let member = format!("dry-{}", mode.as_str());
        let effect = adjudicate_welcome(
            mode,
            42,
            Some("3"),
            "4",
            SUNDAY_SQUAD.series_start_epoch,
            SUNDAY_SQUAD,
            true,
        );
        if matches!(effect, WelcomeEffect::Post { .. }) {
            let guard = begin_prompt(&a, "mock", &member).await.unwrap().unwrap();
            dry_mock.send(effect);
            guard.record_sent("3", AT).await.unwrap();
        }
        assert_eq!(
            has_onboarding_prompt(&a, "mock", &member).await.unwrap(),
            mode == OnboardingMode::Session
        );
    }
    assert_eq!(dry_mock.sent(), 1);

    second_process.close().await;
    a.close().await;
    b.close().await;
    // Only this test's uniquely named schemas. No objects in public are used.
    for schema in [&schema_a, &schema_b] {
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&mut admin)
            .await
            .unwrap();
    }
    let (remaining,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM pg_namespace WHERE nspname IN ($1, $2)")
            .bind(&schema_a)
            .bind(&schema_b)
            .fetch_one(&mut admin)
            .await
            .unwrap();
    assert_eq!(remaining, 0, "test schemas cleaned");
}
