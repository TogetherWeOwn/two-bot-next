//! Attendance retention acceptance: raw event-window voice sessions roll off
//! 90 days after `joined_at` through the retention job.
//!
//! Seeds one stale closed row (91 days), one fresh open row and one mapped
//! window. The dry run counts the stale row and deletes nothing; Execute
//! deletes it while the fresh row and the window survive (windows are
//! control-plane records, never time-purged); a repeat Execute deletes
//! nothing. The logger half (inside/outside windows, dual-tag overlap) and
//! the read projection pin without a database in
//! `crates/bot/src/event_attendance.rs`.

#[path = "support/raw_expiry.rs"]
mod support;

use sqlx::PgPool;
use support::{with_database, GUILD_A, MEMBER};
use two_bot_cutover::retention::{run_retention, RetentionMode};

const CHANNEL: &str = "444444444444444444";

async fn seed_attendance(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO event_attendance_windows \
         (guild_id, event_key, event_id, instance_starts_at, starts_at, ends_at) \
         VALUES ($1, 'game-night', '2001', now() - make_interval(days => 100), \
                 now() - make_interval(days => 100), now() - make_interval(days => 99))",
    )
    .bind(GUILD_A)
    .execute(pool)
    .await
    .expect("seed mapped window");
    // Stale closed session: joined 91 days ago, two hours long.
    sqlx::query(
        "INSERT INTO event_voice_sessions \
         (guild_id, member_id, channel_id, channel_name, is_bot, afk, joined_at, left_at, event_keys) \
         VALUES ($1, $2, $3, 'lobby', FALSE, FALSE, \
                 now() - make_interval(days => 91), \
                 now() - make_interval(days => 91) + make_interval(hours => 2), \
                 ARRAY['game-night'])",
    )
    .bind(GUILD_A)
    .bind(MEMBER)
    .bind(CHANNEL)
    .execute(pool)
    .await
    .expect("seed stale session");
    // Fresh open session: joined an hour ago, still open.
    sqlx::query(
        "INSERT INTO event_voice_sessions \
         (guild_id, member_id, channel_id, channel_name, is_bot, afk, joined_at, left_at, event_keys) \
         VALUES ($1, $2, $3, 'lobby', FALSE, FALSE, now() - make_interval(hours => 1), NULL, ARRAY['game-night'])",
    )
    .bind(GUILD_A)
    .bind(MEMBER)
    .bind(CHANNEL)
    .execute(pool)
    .await
    .expect("seed fresh session");
}

async fn session_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM event_voice_sessions")
        .fetch_one(pool)
        .await
        .expect("count sessions")
}

async fn window_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM event_attendance_windows")
        .fetch_one(pool)
        .await
        .expect("count windows")
}

#[tokio::test]
async fn dry_run_counts_stale_attendance_without_deleting() {
    with_database(|pool| async move {
        seed_attendance(&pool).await;
        let report = run_retention(&pool, RetentionMode::DryRun)
            .await
            .expect("dry-run retention");
        assert_eq!(
            report.attendance_raw_deleted, 1,
            "the 91-day session must count as expired"
        );
        assert_eq!(session_count(&pool).await, 2, "dry run deletes nothing");
        assert_eq!(window_count(&pool).await, 1, "dry run keeps the window");
    })
    .await;
}

#[tokio::test]
async fn execute_expires_stale_attendance_and_keeps_window() {
    with_database(|pool| async move {
        seed_attendance(&pool).await;
        let report = run_retention(&pool, RetentionMode::Execute)
            .await
            .expect("execute retention");
        assert_eq!(
            report.attendance_raw_deleted, 1,
            "the 91-day session must expire"
        );
        assert_eq!(
            session_count(&pool).await,
            1,
            "only the fresh open session survives"
        );
        assert_eq!(
            window_count(&pool).await,
            1,
            "windows are control-plane records, never time-purged"
        );
        let repeated = run_retention(&pool, RetentionMode::Execute)
            .await
            .expect("repeat execute retention");
        assert_eq!(
            repeated.attendance_raw_deleted, 0,
            "a repeat execute must be stable"
        );
    })
    .await;
}
