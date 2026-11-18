#![cfg(feature = "db")]

//! Disposable-Postgres proof of preview advisory claim exclusion and rollback.
//! No DATABASE_URL or inherited application credentials are consulted.

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use two_bot_core::automod_preview_audit as preview_audit;

async fn test_pool() -> Result<sqlx::PgPool, sqlx::Error> {
    let ci_service = std::env::var("CI").as_deref() == Ok("true")
        && std::env::var("TWO_CUSTOM_COMMAND_TEST_CI").as_deref() == Ok("1");
    let options = PgConnectOptions::new()
        .host(if ci_service {
            "127.0.0.1"
        } else {
            "agent-testdb"
        })
        .port(5432)
        .username("agent_test")
        .password("")
        .database("agent_test")
        .options([("statement_timeout", "5000ms")]);
    PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect_with(options)
        .await
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn preview_claim_excludes_contender_and_rollback_releases_it() -> Result<(), sqlx::Error> {
    let pool = test_pool().await?;
    let mut tx = pool.begin().await?;
    preview_audit::lock_preview_attempt(&mut tx, "lock-test-999").await?;

    // The same lock key excludes a contender; an unrelated id is independent.
    let mut contender = pool.begin().await?;
    let same: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(preview_audit::preview_audit_id("lock-test-999"))
            .fetch_one(&mut *contender)
            .await?;
    assert!(!same, "claim lock excludes the same interaction");
    let other: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(preview_audit::preview_audit_id("1000"))
            .fetch_one(&mut *contender)
            .await?;
    assert!(other, "unrelated interaction remains independent");

    tx.rollback().await?;
    let released: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(preview_audit::preview_audit_id("lock-test-999"))
            .fetch_one(&mut *contender)
            .await?;
    assert!(released, "rollback releases the claim");
    contender.rollback().await?;
    Ok(())
}
