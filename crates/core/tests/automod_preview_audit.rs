#![cfg(feature = "db")]

//! Database proofs for the preview audit transaction (spec §6, T17 core half).
//! Both tests use the shared disposable database fixture and run in the
//! standard CI integration sweep with its guarded `TWO_TEST_DATABASE_URL`.
//! Neither test consults DATABASE_URL or inherited application credentials.
//!
//! The lock test checks exclusion and rollback using two transactions. The row
//! test uses a rolled-back transaction over `pg_temp` to check insertion and
//! same-transaction reload. Neither proves cross-connection committed replay or
//! that a real handler commits before replying.

use sqlx::{migrate::Migrator, PgPool, Postgres, Transaction};
use two_bot_core::automod_preview_audit as preview_audit;
use two_bot_testsupport::TestDatabase;

const MIGRATION: &str = include_str!("../../cutover/migrations/0130_custom_commands.sql");
const AT: &str = "2026-11-02T08:00:00.123456Z";

fn matched_record() -> two_bot_core::custom_commands::AuditRecord {
    preview_audit::preview_audit_record(
        "test-guild-one",
        "actor-one",
        "channel-one",
        "matched",
        "filter=bad_words;exemption=none;mode=dry_run;len=0_50".to_owned(),
    )
}

const INTERACTION_ID: &str = "lock-test-999";
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

#[tokio::test]
async fn preview_claim_insert_replay_and_rollback() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required");
    let db = TestDatabase::create(&url, &Migrator::DEFAULT).await?;
    // Capture assertion panics until verified teardown has completed.
    let proof = tokio::spawn(verify_same_transaction_row(db.pool().clone())).await;
    db.close().await?;
    proof??;
    Ok(())
}

async fn verify_same_transaction_row(pool: PgPool) -> Result<(), sqlx::Error> {
    // pg_temp shadows all persistent tables. Everything, including DDL, rolls
    // back, even on an assertion failure when the transaction is dropped.
    let mut tx = pool.begin().await?;
    sqlx::query("CREATE TEMP TABLE preview_audit_test_scope (id INTEGER)")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL search_path = pg_temp")
        .execute(&mut *tx)
        .await?;
    sqlx::raw_sql(MIGRATION).execute(&mut *tx).await?;
    sqlx::raw_sql(MIGRATION).execute(&mut *tx).await?;

    let id = preview_audit::preview_audit_id("999");

    // No committed row: a first delivery may evaluate.
    preview_audit::lock_preview_attempt(&mut tx, "999").await?;
    assert_eq!(
        preview_audit::load_preview_audit(&mut *tx, &id).await?,
        None
    );

    // The winner inserts with its receipt time and reads back the stored
    // timestamp; the first reply and every replay share that text.
    sqlx::query("SET LOCAL TIME ZONE 'America/New_York'")
        .execute(&mut *tx)
        .await?;
    let record = matched_record();
    let stored = preview_audit::insert_preview_audit(&mut *tx, &record, &id, AT).await?;
    assert_eq!(stored, "2026-11-02 08:00:00.123456+00");
    // Neither insert nor replay may inherit the database session's timezone.
    sqlx::query("SET LOCAL TIME ZONE 'Asia/Tokyo'")
        .execute(&mut *tx)
        .await?;
    let row = preview_audit::load_preview_audit(&mut *tx, &id)
        .await?
        .expect("inserted row reloads on the same connection");
    assert_eq!(row.outcome, "matched");
    assert_eq!(
        row.reason.as_deref(),
        Some("filter=bad_words;exemption=none;mode=dry_run;len=0_50")
    );
    assert_eq!(row.created_at, stored);
    let first = two_bot_core::automod_preview_report::render_preview_reply(
        &two_bot_core::automod_preview::PreviewVerdict {
            result: two_bot_core::automod_preview::PreviewResult::Matched,
            filter: Some(two_bot_core::automod::AutomodFilter::BadWords),
            knobs: two_bot_core::automod_preview::deciding_knobs(
                two_bot_core::automod::AutomodFilter::BadWords,
            ),
            exemption: None,
            dry_run: true,
        },
        &stored,
    );
    assert_eq!(
        preview_audit::replay_preview_reply(&row.outcome, row.reason.as_deref(), &row.created_at),
        Some(first),
        "same-transaction replay renders byte-identical to the first reply"
    );

    tx.rollback().await?;
    Ok(())
}
