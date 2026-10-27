#![cfg(feature = "db")]

//! Store pins for `/moderation-recent` (`read_recent_moderation`).
//!
//! Builder-owned companion to the protected runtime acceptance suite: it pins
//! the read shape the runtime relies on (guild scope, the 100-row window,
//! newest-first order with identity tie-break, warning/audit dedupe and the
//! warnings-only projection) directly against the ledger tables.
//!
//! Runs on the disposable test database only:
//! TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_local \
//! python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db \
//!   --test moderation_recent_store

use sqlx::PgPool;
use two_bot_core::{
    moderation_recent::{RecentRow, RECENT_WINDOW},
    moderation_recent_store::read_recent_moderation,
};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "111111111111111111";
const OTHER_GUILD: &str = "999999999999999999";

async fn database() -> TestDatabase {
    let url =
        std::env::var("TWO_TEST_DATABASE_URL").expect("explicit isolated test service required");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated disposable fixture")
}

async fn audit(pool: &PgPool, guild: &str, request: &str, outcome: &str, at: &str) {
    sqlx::query(
        "INSERT INTO moderation_audit
         (request_id, guild_id, actor_id, action, target_id, channel_id, reason,
          outcome, idempotency_key, metadata_json, created_at)
         VALUES ($1, $2, '88', 'moderation.kick', '3333', NULL, 'reason',
                 $3, $4, '{\"claim_token\":\"hidden\"}', $5::text::timestamptz)",
    )
    .bind(request)
    .bind(guild)
    .bind(outcome)
    .bind(format!("idem-{request}"))
    .bind(at)
    .execute(pool)
    .await
    .expect("seed audit row");
}

async fn warning(pool: &PgPool, guild: &str, request: &str, at: &str) {
    sqlx::query(
        "INSERT INTO moderation_warnings
         (id, guild_id, user_id, actor_id, reason, request_id, created_at)
         VALUES ($1, $2, '3333', '88', 'warned reason', $3, $4::text::timestamptz)",
    )
    .bind(format!("warn-{request}"))
    .bind(guild)
    .bind(request)
    .bind(at)
    .execute(pool)
    .await
    .expect("seed warning row");
}

fn ids(rows: &[RecentRow]) -> Vec<&str> {
    rows.iter().map(|row| row.request_id.as_str()).collect()
}

#[tokio::test]
async fn reads_are_guild_scoped_and_order_ties_by_request_identity() {
    let db = database().await;
    let pool = db.pool().clone();
    for request in ["req-b", "req-c", "req-a"] {
        audit(&pool, GUILD, request, "kicked", "2026-10-11T00:00:00+00:00").await;
    }
    audit(
        &pool,
        GUILD,
        "req-newest",
        "kicked",
        "2026-10-12T00:00:00+00:00",
    )
    .await;
    audit(
        &pool,
        OTHER_GUILD,
        "req-foreign",
        "banned",
        "2026-10-13T00:00:00+00:00",
    )
    .await;
    warning(
        &pool,
        OTHER_GUILD,
        "req-foreign-warn",
        "2026-10-14T00:00:00+00:00",
    )
    .await;
    let rows = read_recent_moderation(&pool, GUILD).await.expect("read");
    db.close().await.expect("drop owned fixture");
    assert_eq!(
        ids(&rows),
        ["req-newest", "req-a", "req-b", "req-c"],
        "newest first, ties by request id, no foreign rows"
    );
    assert_eq!(rows[0].created_at, "2026-10-12T00:00:00Z");
}

#[tokio::test]
async fn warnings_dedupe_against_audit_and_project_alone_when_uncorroborated() {
    let db = database().await;
    let pool = db.pool().clone();
    audit(
        &pool,
        GUILD,
        "req-both",
        "warned",
        "2026-10-10T00:00:01+00:00",
    )
    .await;
    warning(&pool, GUILD, "req-both", "2026-10-10T00:00:01+00:00").await;
    warning(&pool, GUILD, "req-only", "2026-10-10T00:00:02+00:00").await;
    let rows = read_recent_moderation(&pool, GUILD).await.expect("read");
    db.close().await.expect("drop owned fixture");
    assert_eq!(ids(&rows), ["req-only", "req-both"]);
    let only = &rows[0];
    assert_eq!(
        (
            only.action.as_str(),
            only.outcome.as_str(),
            only.target_id.as_deref()
        ),
        ("moderation.warn", "warned", Some("3333"))
    );
    assert_eq!(only.actor_id, "88");
    assert!(only.channel_id.is_none());
    assert_eq!(rows[1].action, "moderation.kick", "the audit row wins");
}

#[tokio::test]
async fn the_read_is_bounded_to_the_newest_window() {
    let db = database().await;
    let pool = db.pool().clone();
    for index in 0..RECENT_WINDOW + 5 {
        let at = format!("2026-10-01T00:{:02}:{:02}+00:00", index / 60, index % 60);
        audit(&pool, GUILD, &format!("req-{index:03}"), "kicked", &at).await;
    }
    let rows = read_recent_moderation(&pool, GUILD).await.expect("read");
    db.close().await.expect("drop owned fixture");
    assert_eq!(rows.len(), RECENT_WINDOW);
    assert_eq!(rows[0].request_id, format!("req-{:03}", RECENT_WINDOW + 4));
    assert_eq!(rows[RECENT_WINDOW - 1].request_id, "req-005");
}
