#![cfg(feature = "db")]

//! Disposable-Postgres proof of preview advisory claim exclusion and rollback.

use sqlx::{migrate::Migrator, PgPool, Postgres, Transaction};
use two_bot_core::automod_preview_audit as preview_audit;
use two_bot_testsupport::TestDatabase;

const INTERACTION_ID: &str = "999";
const OTHER_INTERACTION_ID: &str = "1000";

async fn try_claim(tx: &mut Transaction<'_, Postgres>, id: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(preview_audit::preview_audit_id(id))
        .fetch_one(&mut **tx)
        .await
}

async fn observe_claims(pool: &PgPool) -> Result<(bool, bool, bool), sqlx::Error> {
    let mut holder = pool.begin().await?;
    preview_audit::lock_preview_attempt(&mut holder, INTERACTION_ID).await?;

    // Both transactions stay open, so the contender uses a separate connection.
    let mut contender = pool.begin().await?;
    let same = try_claim(&mut contender, INTERACTION_ID).await?;
    let other = try_claim(&mut contender, OTHER_INTERACTION_ID).await?;

    holder.rollback().await?;
    let released = try_claim(&mut contender, INTERACTION_ID).await?;
    contender.rollback().await?;
    Ok((same, other, released))
}

#[tokio::test]
async fn preview_claim_excludes_contender_and_rollback_releases_it(
) -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required");
    let db = TestDatabase::create(&url, &Migrator::DEFAULT).await?;
    let observations = observe_claims(db.pool()).await;
    db.close().await?;

    // Assert after teardown so a broken lock cannot strand the fixture.
    let (same, other, released) = observations?;
    assert!(!same, "claim lock excludes the same interaction");
    assert!(other, "unrelated interaction remains independent");
    assert!(released, "rollback releases the claim");
    Ok(())
}
