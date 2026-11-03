#![cfg(feature = "db")]

//! Opt-in database proof for the preview audit transaction (spec §6, T17
//! core half). Restricted to agent-testdb or the CI service container.
//! Run: cargo test -p two-bot-core --features db --test automod_preview_audit -- --ignored
//! No DATABASE_URL or inherited application credentials are consulted.
//!
//! Proves, inside one rolled-back transaction over `pg_temp`: the advisory
//! lock serializes attempts by interaction id, the insert returns the stored
//! timestamp the first reply and every replay share, a committed-shape row
//! reloads verbatim, and a rollback releases the claim so a later delivery
//! may retry with no stranded row.

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use two_bot_core::automod_preview_audit as preview_audit;

const MIGRATION: &str = include_str!("../../cutover/migrations/0130_custom_commands.sql");
const AT: &str = "2026-11-02T08:00:00Z";

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

fn matched_record() -> two_bot_core::custom_commands::AuditRecord {
    preview_audit::preview_audit_record(
        "test-guild-one",
        "actor-one",
        "channel-one",
        "matched",
        "filter=bad_words;exemption=none;mode=dry_run;len=0_50".to_owned(),
    )
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn preview_claim_insert_replay_and_rollback() -> Result<(), sqlx::Error> {
    let pool = test_pool().await?;

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
    let record = matched_record();
    let stored = preview_audit::insert_preview_audit(&mut *tx, &record, &id, AT).await?;
    assert!(
        stored.starts_with("2026-11-02 08:00:00"),
        "stored timestamp keeps the receipt instant: {stored}"
    );
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
        "replay renders the committed row byte-identical to the first reply"
    );

    // The same lock key excludes a contender while held; an unrelated
    // interaction id remains independent.
    let mut contender = pool.begin().await?;
    let same: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(preview_audit::preview_audit_id("999"))
            .fetch_one(&mut *contender)
            .await?;
    assert!(!same, "claim lock excludes the same interaction");
    let other: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(preview_audit::preview_audit_id("1000"))
            .fetch_one(&mut *contender)
            .await?;
    assert!(other, "unrelated interaction remains independent");
    contender.rollback().await?;

    // Rollback releases the claim with no stranded row: a later delivery
    // finds nothing and may retry, disclosing nothing uncommitted. The
    // rolled-back DDL is gone on that session, so the retry re-establishes
    // its own temp schema first — whichever pooled session it lands on.
    tx.rollback().await?;
    let mut retry = pool.begin().await?;
    sqlx::query("CREATE TEMP TABLE preview_audit_retry_scope (id INTEGER)")
        .execute(&mut *retry)
        .await?;
    sqlx::query("SET LOCAL search_path = pg_temp")
        .execute(&mut *retry)
        .await?;
    sqlx::raw_sql(MIGRATION).execute(&mut *retry).await?;
    preview_audit::lock_preview_attempt(&mut retry, "999").await?;
    assert_eq!(
        preview_audit::load_preview_audit(&mut *retry, &id).await?,
        None,
        "rolled-back attempt leaves no row to replay"
    );
    retry.rollback().await?;
    Ok(())
}
