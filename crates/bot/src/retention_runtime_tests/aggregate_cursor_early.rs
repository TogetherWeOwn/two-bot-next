#![cfg(test)]

//! Regression: per-guild deletes carry `lock_timeout`/`statement_timeout`, so
//! a row lock held by a stuck session fails that guild fast as an isolated
//! retryable error instead of blocking the pass until the supervisor cancels
//! it and pins its pooled connection (sqlx 0.9 pings the dropped connection
//! with no timeout). Separate from the acceptance paging contract (which this
//! file never edits): it holds a row lock on the first guild across three
//! passes on a production-shaped 2-connection pool and asserts later guilds
//! are still reached and the pool still hands out connections.

use super::database::{database, RUNTIME_LOCK};
use super::*;
use crate::retention_jobs::{
    aggregate_expiry_cursor_for_tests, pool_from_url, reset_aggregate_expiry_cursor_for_tests,
};

fn guild_id(index: u64) -> String {
    format!("8{index:017}")
}

async fn seed_guild(pool: &sqlx::PgPool, guild: &str, stale: u64) {
    for j in 0..stale {
        sqlx::query(
            "INSERT INTO game_activity_guild_aggregates \
             (guild_id, game, total_play_ms, in_voice_ms, session_count, updated_at) \
             VALUES ($1, $2, 600000, 0, 1, now() - make_interval(days => 100))",
        )
        .bind(guild)
        .bind(format!("Stale-{j}"))
        .execute(pool)
        .await
        .unwrap();
    }
}

async fn stale_rows(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM game_activity_guild_aggregates \
         WHERE updated_at < now() - make_interval(days => 90)",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn stale_rows_in(pool: &sqlx::PgPool, guild: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM game_activity_guild_aggregates \
         WHERE guild_id = $1 AND updated_at < now() - make_interval(days => 90)",
    )
    .bind(guild)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn aggregate_expiry_cursor_advances_before_deletes_on_cancelled_pass() {
    let _serial = RUNTIME_LOCK.lock().await;
    let Some(db) = database().await else { return };
    let pool = db.pool();
    reset_aggregate_expiry_cursor_for_tests();
    // Six guilds: test pages at 4, so the first page is guilds 1-4.
    for i in 1..=6 {
        seed_guild(pool, &guild_id(i), 1).await;
    }
    let first = guild_id(1);
    let second = guild_id(2);
    // Hold a row lock on the first guild's stale row: without per-guild
    // timeouts the batched ctid DELETE for that guild would block until the
    // supervisor cancels the pass and pins its pooled connection.
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT ctid FROM game_activity_guild_aggregates WHERE guild_id = $1 FOR UPDATE")
        .bind(&first)
        .fetch_all(&mut *holder)
        .await
        .unwrap();

    // Production-shaped pool: 2 connections and a 10 s acquire timeout, like
    // `pool_from_url`. The fixture's 5-connection pool would hide the pinning
    // this test guards (two cancelled passes exhaust 2 connections). The
    // trailing `statement_timeout=0` strips the fixture's 5 s server-side
    // timeout (a later `-c` wins in sqlx 0.9), so the only bound on a blocked
    // DELETE is the fix's own per-guild `SET LOCAL`: deleting those lines
    // makes the first pass hang past the 30 s deadline and fail this test.
    let limited = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(
            pool.connect_options()
                .as_ref()
                .clone()
                .options([("statement_timeout", "0")]),
        )
        .await
        .unwrap();
    let limited_timeout: String = sqlx::query_scalar("SHOW statement_timeout")
        .fetch_one(&limited)
        .await
        .unwrap();
    assert_eq!(
        limited_timeout, "0",
        "test pool must carry no server-side timeout of its own, so only the fix bounds a blocked delete"
    );
    // Pin the other half of the fix: the production pool constructor must set
    // the 15 s pool-level statement timeout. `pool_from_url` refuses CI-service
    // hosts under the default required TLS policy, so opt this test process
    // into local-only first (CI already sets it; this only ever touches
    // disposable local databases, serialized by RUNTIME_LOCK).
    std::env::set_var("TWO_DATABASE_TLS", "local-only");
    let bootstrap_url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("retention runtime fixture needs TWO_TEST_DATABASE_URL");
    let prod_pool = pool_from_url(&bootstrap_url).expect("production pool builds from test URL");
    let prod_timeout: String = sqlx::query_scalar("SHOW statement_timeout")
        .fetch_one(&prod_pool)
        .await
        .unwrap();
    assert_eq!(
        prod_timeout, "15s",
        "pool_from_url must set the 15 s pool-level statement timeout"
    );
    prod_pool.close().await;

    async fn pool_still_usable(pool: &sqlx::PgPool) {
        // The exact failing point from the review: `run_retention` starts with
        // `pool.begin()`, which timed out once both pooled connections were
        // pinned by cancelled blocked deletes.
        tokio::time::timeout(Duration::from_secs(10), pool.acquire())
            .await
            .expect("pool must still hand out connections while the lock is held")
            .expect("acquire must succeed while the lock is held");
        sqlx::query("SELECT 1")
            .fetch_one(pool)
            .await
            .expect("trivial query must answer while the lock is held");
    }

    // Pass 1 visits guilds 1-4: guild 1's delete fails fast on `lock_timeout`
    // as an isolated per-guild error while guilds 2-4 still drain in the same
    // pass. The pass reports the failure (Err) but completes instead of
    // hanging until cancellation.
    let action = job(limited.clone(), RetentionMode::Execute).action;
    tokio::time::timeout(Duration::from_secs(30), (action)())
        .await
        .expect("blocked-guild pass must complete via lock timeout, not hang")
        .expect_err("blocked-guild pass must report the isolated per-guild failure");
    assert_eq!(
        stale_rows_in(&limited, &second).await,
        0,
        "the guild after a still-locked guild must be expired by the same pass"
    );
    assert_eq!(
        stale_rows_in(&limited, &first).await,
        1,
        "the locked guild keeps its row for a later retry"
    );
    pool_still_usable(&limited).await;

    // Pass 2 resumes after the first page and drains guilds 5-6 with no locked
    // guild on its page.
    let retry = job(limited.clone(), RetentionMode::Execute).action;
    tokio::time::timeout(Duration::from_secs(30), (retry)())
        .await
        .expect("second pass must complete while the lock is held")
        .expect("second pass with no locked guild on its page must succeed");
    pool_still_usable(&limited).await;

    // Pass 3 wraps back to the still-locked first guild: it must still
    // complete (isolated failure) and the pool must still hand out
    // connections. Before the timeout fix this third pass pinned the second
    // pooled connection, so every later tick failed at `pool.begin()`.
    let third = job(limited.clone(), RetentionMode::Execute).action;
    tokio::time::timeout(Duration::from_secs(30), (third)())
        .await
        .expect("third pass with the lock still held must complete")
        .expect_err("third pass must report the isolated per-guild failure");
    assert_eq!(
        stale_rows_in(&limited, &first).await,
        1,
        "the locked guild still keeps its row after three passes"
    );
    pool_still_usable(&limited).await;
    // The cursor kept moving across all three passes instead of sticking on
    // the blocked guild: the last pass attempted the locked guild again.
    assert_eq!(
        aggregate_expiry_cursor_for_tests(&limited).as_deref(),
        Some(first.as_str()),
        "three passes with a held lock must still advance the cursor"
    );
    holder.rollback().await.unwrap();

    // With the lock released, later passes drain everything, proving no work
    // was skipped.
    let drain = job(limited.clone(), RetentionMode::Execute).action;
    let mut passes = 0u64;
    loop {
        passes += 1;
        assert!(
            passes <= 10,
            "the fleet must drain after the cancelled pass advances"
        );
        let result = tokio::time::timeout(Duration::from_secs(30), (drain)())
            .await
            .expect("drain pass must complete within fixture deadline");
        let _ = result;
        if stale_rows(&limited).await == 0 {
            break;
        }
    }
    limited.close().await;
    reset_aggregate_expiry_cursor_for_tests();
    db.close().await.unwrap();
}
