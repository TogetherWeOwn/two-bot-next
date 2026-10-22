//! Builder-owned pin: the production Postgres store suppresses a redundant
//! `:in_progress` audit write once the accepted-effect row exists (the
//! `WHERE NOT EXISTS` guard in `PgMemberModerationStore::record_audit`).
//! The memory-store twin is pinned offline; this is the SQL-durability twin,
//! and it must go red if the guard is reverted to a plain insert.
#![cfg(feature = "db")]

use std::collections::HashSet;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Postgres, QueryBuilder};
use two_bot_core::member_moderation::{
    MemberExecution, MemberModerationService, MockMemberDiscord,
};
use two_bot_core::member_moderation_store::PgMemberModerationStore;
use two_bot_core::{ModerationAction, ModerationActor, ModerationPolicy, ModerationTarget};

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn postgres_completion_loss_suppresses_redundant_in_progress_audit() {
    let (admin, pool, schema) = database().await;
    let store = PgMemberModerationStore::new(pool.clone(), "100000000000000001");
    let discord = MockMemberDiscord::new();
    let svc = MemberModerationService::new(discord.clone(), store, policy(), || 1_700_000_000_000);
    sqlx::query("ALTER TABLE moderation_idempotency ADD CONSTRAINT refuse_completion CHECK (state <> 'done') NOT VALID")
        .execute(&pool).await.expect("scratch completion failure");
    let kick = execution(ModerationAction::Kick, "suppression-probe");
    assert!(svc.execute(&kick).await.is_err());
    let accepted: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM moderation_audit WHERE request_id = 'suppression-probe' AND outcome = 'kicked'")
        .fetch_one(&pool).await.expect("accepted kick audited before failing completion");
    assert_eq!(accepted, 1);
    // Accepted-effect precedence: the suppressed `in_progress` write must not
    // leave a redundant row. Reverting the NOT EXISTS guard turns this red.
    let redundant: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moderation_audit WHERE request_id = 'suppression-probe:in_progress'",
    )
    .fetch_one(&pool)
    .await
    .expect("suppressed row stays absent");
    assert_eq!(redundant, 0);
    assert_eq!(discord.call_count("kick"), 1);
    cleanup(admin, pool, schema).await;
}

// No DATABASE_URL or inherited credentials: tests accept only the approved
// agent container or the ephemeral Postgres service in GitHub Actions.
async fn database_schema() -> (PgPool, PgPool, String) {
    let host = match std::env::var("MEMBER_TESTDB").as_deref() {
        Ok("agent-testdb") => "agent-testdb",
        Ok("ci") if std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true") => "localhost",
        _ => panic!("set MEMBER_TESTDB=agent-testdb (or ci inside GitHub Actions)"),
    };
    let options = PgConnectOptions::new()
        .host(host)
        .port(5432)
        .username("agent_test")
        .password("")
        .database("postgres");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(options.clone())
        .await
        .expect("approved test DB must connect; no credential fallback");
    let schema = format!(
        "member_audit_suppression_test_{:032x}",
        rand::random::<u128>()
    );
    // Identifier is a fixed prefix + generated hexadecimal, never user input.
    QueryBuilder::<Postgres>::new("CREATE SCHEMA ")
        .push(&schema)
        .build()
        .execute(&admin)
        .await
        .expect("test schema");
    let search_path = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(move |connection, _| {
            let query = search_path.clone();
            Box::pin(async move {
                sqlx::query("SELECT set_config('search_path', $1, false)")
                    .bind(query)
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with(options)
        .await
        .expect("schema pool");
    (admin, pool, schema)
}

async fn database() -> (PgPool, PgPool, String) {
    let (admin, pool, schema) = database_schema().await;
    for _ in 0..2 {
        for migration in [
            include_str!("../../cutover/migrations/0110_moderation_member.sql"),
            include_str!("../../cutover/migrations/0111_moderation_ban_ownership.sql"),
            include_str!("../../cutover/migrations/0112_moderation_legacy_timestamps.sql"),
            include_str!("../../cutover/migrations/0113_moderation_unban_retry_order.sql"),
        ] {
            sqlx::raw_sql(migration)
                .execute(&pool)
                .await
                .expect("idempotent member migrations");
        }
    }
    (admin, pool, schema)
}

async fn cleanup(admin: PgPool, pool: PgPool, schema: String) {
    pool.close().await;
    // Only this test's generated scratch schema is removed.
    QueryBuilder::<Postgres>::new("DROP SCHEMA ")
        .push(&schema)
        .push(" CASCADE")
        .build()
        .execute(&admin)
        .await
        .expect("test cleanup");
    admin.close().await;
}

fn policy() -> ModerationPolicy {
    ModerationPolicy {
        owen_user_id: "123456789012345678".into(),
        bot_user_id: Some("555555555555555555".into()),
        protected_role_ids: HashSet::new(),
    }
}

fn execution(action: ModerationAction, id: &str) -> MemberExecution {
    MemberExecution {
        action,
        guild_id: "100000000000000001".into(),
        actor: ModerationActor {
            user_id: "111111111111111111".into(),
            role_ids: vec![],
            highest_role_position: 50,
            permissions: u64::MAX,
        },
        target: Some(ModerationTarget {
            user_id: "333333333333333333".into(),
            role_ids: vec![],
            highest_role_position: 10,
            is_bot: false,
            is_guild_owner: false,
        }),
        bot_highest_role_position: 100,
        reason: "spam".into(),
        duration_seconds: Some(3600),
        request_id: id.into(),
        idempotency_key: id.into(),
    }
}
