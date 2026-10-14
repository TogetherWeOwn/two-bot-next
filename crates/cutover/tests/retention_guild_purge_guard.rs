//! Guild-purge replay-guard regression (Builder-owned, TOG-24247).
//!
//! `purge_guild` must check unresolved replay guards across the entire guild
//! before deleting any guild rows; a guarded guild stays queued with rows
//! untouched until the guards settle. This file is owned by the purge-guard
//! fix, not the Test Author's `retention_windows` acceptance suite, which is
//! left unmodified.
//!
//! Fixed fake snowflakes only. Disposable test databases via
//! `two-bot-testsupport`; never staging or production bindings, never real
//! user data, no secrets in code or comments.

use sqlx::PgPool;
use two_bot_cutover::retention::{
    handle_guild_delete, run_retention, RetentionMode, RetentionReport,
};
use two_bot_testsupport::TestDatabase;

const GONE_GUILD: &str = "333333333333333333";
const MEMBER: &str = "444444444444444444";

async fn database() -> Option<TestDatabase> {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure the retention database suite"
            );
            eprintln!("SKIP retention integration: TWO_TEST_DATABASE_URL not set");
            return None;
        }
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("./migrations"))
            .await
            .unwrap(),
    )
}

async fn count(pool: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

/// A queued guild purge with an unresolved replay guard deletes nothing and
/// stays queued; once the guard settles the purge proceeds.
#[tokio::test]
async fn guild_purge_with_unresolved_guard_keeps_rows_queued() {
    let Some(db) = database().await else { return };
    let pool = db.pool();
    sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2)")
        .bind(GONE_GUILD)
        .bind(MEMBER)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO member_levels (guild_id, member_id, xp, message_xp, updated_at)
         VALUES ($1, $2, 150, 150, now())",
    )
    .bind(GONE_GUILD)
    .bind(MEMBER)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO presence_probe (guild_id, observed_at, approximate_presence_count, bot_floor)
         VALUES ($1, to_char(now() - interval '1 day', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), 10, NULL)",
    )
    .bind(GONE_GUILD)
    .execute(pool)
    .await
    .unwrap();
    // In-flight idempotency intent for the departed guild.
    sqlx::query(
        "INSERT INTO internal_idempotency
           (caller_hash, key_hash, action, payload_hash, state, guild_id, actor_id, target_id)
         VALUES (repeat('a', 64), repeat('b', 64), 'role.assign', repeat('c', 64), 'unknown', $1, $2, $2)",
    )
    .bind(GONE_GUILD)
    .bind(MEMBER)
    .execute(pool)
    .await
    .unwrap();
    assert!(
        handle_guild_delete(pool, GONE_GUILD, None).await.unwrap(),
        "a real removal enqueues a purge"
    );

    let report: RetentionReport = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert_eq!(
        report.guild_rows_purged, 0,
        "a guarded purge must delete nothing, got {report:?}"
    );
    assert_eq!(
        report.guild_purges_failed, 1,
        "a guarded purge must stay queued, got {report:?}"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM internal_idempotency WHERE guild_id = '333333333333333333'"
        )
        .await,
        1,
        "the in-flight intent must survive the guarded purge"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM member_levels WHERE guild_id = '333333333333333333'"
        )
        .await,
        1,
        "the guild's rows must survive the guarded purge"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM presence_probe WHERE guild_id = '333333333333333333'"
        )
        .await,
        1,
        "the guild's probe rows must survive the guarded purge"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM retention_guild_purge_queue WHERE guild_id = '333333333333333333'"
        )
        .await,
        1,
        "the guarded guild must stay queued"
    );

    // Settle the fence; the next run must purge the guild.
    sqlx::query(
        "UPDATE internal_idempotency SET state = 'completed', response_code = 'success',
         http_status = 200, resource_id = actor_id, affected = 1 WHERE guild_id = $1 AND actor_id = $2",
    )
    .bind(GONE_GUILD)
    .bind(MEMBER)
    .execute(pool)
    .await
    .unwrap();
    let settled: RetentionReport = run_retention(pool, RetentionMode::Execute).await.unwrap();
    assert!(
        settled.guild_rows_purged > 0,
        "the settled purge must delete rows, got {settled:?}"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM retention_guild_purge_queue WHERE guild_id = '333333333333333333'"
        )
        .await,
        0,
        "the settled guild must dequeue"
    );
    assert_eq!(
        count(
            pool,
            "SELECT count(*) FROM member_levels WHERE guild_id = '333333333333333333'"
        )
        .await,
        0,
        "the settled guild's rows must go"
    );
    db.close().await.unwrap();
}
