#![cfg(test)]

//! Regression: the aggregate-expiry cursor advances per guild immediately
//! before that guild's delete, so a hung delete or a supervisor timeout that
//! cancels the pass still moves on to later guilds on the same page instead
//! of re-blocking on the same first guild forever. Separate from the
//! acceptance paging contract (which this file never edits): it holds a row
//! lock on the first guild, cancels the blocked pass, runs a second pass
//! without releasing the lock, and asserts the later guilds are reached.

use super::database::{database, RUNTIME_LOCK};
use super::*;
use crate::retention_jobs::{
    aggregate_expiry_cursor_for_tests, reset_aggregate_expiry_cursor_for_tests,
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
    // Six guilds: test pages at 4, so the first page is guilds 1-4. The
    // cursor must land on guild 1 (the last attempted guild) even when guild
    // 1's delete never finishes, so the next pass resumes after it.
    for i in 1..=6 {
        seed_guild(pool, &guild_id(i), 1).await;
    }
    let first = guild_id(1);
    let second = guild_id(2);
    // Hold a row lock on the first guild's stale row: the pass's batched
    // ctid DELETE for that guild blocks until this transaction rolls back.
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT ctid FROM game_activity_guild_aggregates WHERE guild_id = $1 FOR UPDATE")
        .bind(&first)
        .fetch_all(&mut *holder)
        .await
        .unwrap();

    let action = job(pool.clone(), RetentionMode::Execute).action;
    let cancelled = tokio::time::timeout(Duration::from_secs(2), (action)())
        .await
        .is_err();
    assert!(
        cancelled,
        "the blocked delete must still be pending at the fixture deadline"
    );
    assert_eq!(
        aggregate_expiry_cursor_for_tests(pool).as_deref(),
        Some(first.as_str()),
        "a cancelled pass must advance past the last attempted guild"
    );

    // Without releasing the lock, the next pass must resume after guild 1
    // and reach guild 2: per-guild cursor advancement means a blocked delete
    // cannot starve the other guilds on its page, even for a single-page
    // fleet.
    let retry = job(pool.clone(), RetentionMode::Execute).action;
    tokio::time::timeout(Duration::from_secs(30), (retry)())
        .await
        .expect("the pass after a blocked guild must complete without the lock")
        .unwrap();
    assert_eq!(
        stale_rows_in(pool, &second).await,
        0,
        "the guild after a still-locked guild must be expired by the next pass"
    );
    assert_eq!(
        stale_rows_in(pool, &first).await,
        1,
        "the locked guild keeps its row for a later retry"
    );
    holder.rollback().await.unwrap();

    // With the lock released, later passes drain everything, proving no work
    // was skipped.
    let drain = job(pool.clone(), RetentionMode::Execute).action;
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
        result.unwrap();
        if stale_rows(pool).await == 0 {
            break;
        }
    }
    reset_aggregate_expiry_cursor_for_tests();
    db.close().await.unwrap();
}
