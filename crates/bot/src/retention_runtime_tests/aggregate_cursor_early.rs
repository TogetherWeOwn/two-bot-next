#![cfg(test)]

//! Regression: the aggregate-expiry cursor advances before the per-guild
//! deletes, so a hung delete or a supervisor timeout that cancels the pass
//! still moves on to later guilds instead of re-blocking on the same first
//! page forever. Separate from the acceptance paging contract (which this
//! file never edits): it holds a row lock on the first guild, cancels the
//! blocked pass, and asserts the cursor already moved past the visited page.

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

#[tokio::test]
async fn aggregate_expiry_cursor_advances_before_deletes_on_cancelled_pass() {
    let _serial = RUNTIME_LOCK.lock().await;
    let Some(db) = database().await else { return };
    let pool = db.pool();
    reset_aggregate_expiry_cursor_for_tests();
    // Six guilds: test pages at 4, so the first page is guilds 1-4 and the
    // cursor must land on guild 4 even when guild 1's delete never finishes.
    for i in 1..=6 {
        seed_guild(pool, &guild_id(i), 1).await;
    }
    let first = guild_id(1);
    let expected_cursor = guild_id(4);
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
        Some(expected_cursor.as_str()),
        "a cancelled pass must still advance past the visited page"
    );
    holder.rollback().await.unwrap();

    // With the lock released, later passes visit the deferred guilds first
    // and eventually drain everything, proving no work was skipped.
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
