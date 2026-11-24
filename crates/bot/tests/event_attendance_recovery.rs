//! Bounded attendance actor turns against a disposable PostgreSQL database.

use sqlx::{PgExecutor, PgPool};
use time::{Duration, OffsetDateTime};
use two_bot::event_attendance::recovery::{AttendanceRecovery, MemberLossFence, TurnError};
use two_bot_core::Snowflake;
use two_bot_testsupport::TestDatabase;

const GUILD: Snowflake = 100000000000000001;
const LOST: Snowflake = 300000000000000001;
const NEIGHBOR: Snowflake = 300000000000000002;
const ROOM: Snowflake = 400000000000000001;
const JOIN: OffsetDateTime = OffsetDateTime::UNIX_EPOCH;

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test database required");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .unwrap()
}

async fn seed<'e>(connection: impl PgExecutor<'e>, member: Snowflake, rows: i32) -> i64 {
    sqlx::query_scalar(
        "WITH rows AS (
            INSERT INTO event_voice_sessions (guild_id,member_id,channel_id,joined_at)
            SELECT $1,$2,$3,$4 FROM generate_series(1,$5::integer) RETURNING id
         ) SELECT min(id) FROM rows",
    )
    .bind(GUILD.to_string())
    .bind(member.to_string())
    .bind(ROOM.to_string())
    .bind(JOIN)
    .bind(rows)
    .fetch_one(connection)
    .await
    .unwrap()
}

async fn counts(pool: &PgPool, member: Snowflake, departure: OffsetDateTime) -> (i64, i64, i64) {
    sqlx::query_as(
        "SELECT count(*) FILTER (WHERE left_at IS NULL),
                count(*) FILTER (WHERE left_at = joined_at),
                count(*) FILTER (WHERE left_at = $3)
         FROM event_voice_sessions WHERE guild_id = $1 AND member_id = $2",
    )
    .bind(GUILD.to_string())
    .bind(member.to_string())
    .bind(departure)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn retained_departure_finishes_before_new_member_loss() {
    let db = database().await;
    let pool = db.pool();
    let mut actor = AttendanceRecovery::new(GUILD, MemberLossFence::default());
    seed(pool, NEIGHBOR, 10).await;
    seed(pool, LOST, 1).await;
    let departure = JOIN + Duration::minutes(20);
    assert!(actor.capture_allowed());
    actor.retain_departure(NEIGHBOR, ROOM, departure);
    assert!(!actor.capture_allowed());
    actor.turn(pool).await.unwrap();
    assert_eq!(counts(pool, NEIGHBOR, departure).await, (2, 0, 8));
    actor.retain_departure(LOST, ROOM, departure + Duration::minutes(1));
    actor.turn(pool).await.unwrap();
    assert_eq!(counts(pool, NEIGHBOR, departure).await, (0, 0, 10));
    assert_eq!(counts(pool, LOST, departure).await, (1, 0, 0));
    assert!(!actor.capture_allowed());
    actor.turn(pool).await.unwrap();
    assert_eq!(counts(pool, LOST, departure).await, (0, 1, 0));
    assert_eq!(counts(pool, NEIGHBOR, departure).await, (0, 0, 10));
    assert!(actor.capture_allowed());
    db.close().await.unwrap();
}

#[tokio::test]
async fn newer_loss_restarts_the_member_cursor() {
    let db = database().await;
    let pool = db.pool();
    let fence = MemberLossFence::default();
    let mut actor = AttendanceRecovery::new(GUILD, fence.clone());
    let mut earlier = pool.begin().await.unwrap();
    let hidden_id = seed(&mut *earlier, LOST, 1).await;
    let later_id = seed(pool, LOST, 9).await;
    assert!(hidden_id < later_id);
    let departure = JOIN + Duration::minutes(20);
    fence.record_loss(LOST);
    actor.turn(pool).await.unwrap();
    assert_eq!(counts(pool, LOST, departure).await, (1, 8, 0));
    fence.record_loss(LOST);
    earlier.commit().await.unwrap();
    actor.turn(pool).await.unwrap();
    assert_eq!(counts(pool, LOST, departure).await, (0, 10, 0));
    assert!(actor.capture_allowed());
    db.close().await.unwrap();
}

#[tokio::test]
async fn loss_recorded_during_close_survives_old_acknowledgement() {
    let db = database().await;
    let pool = db.pool().clone();
    let id = seed(&pool, LOST, 1).await;
    let mut writer = pool.begin().await.unwrap();
    let blocker: i32 = sqlx::query_scalar(
        "SELECT pg_backend_pid() FROM event_voice_sessions WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_one(&mut *writer)
    .await
    .unwrap();
    let fence = MemberLossFence::default();
    let mut actor = AttendanceRecovery::new(GUILD, fence.clone());
    fence.record_loss(LOST);
    let work_pool = pool.clone();
    let turn = tokio::spawn(async move {
        actor.turn(&work_pool).await.unwrap();
        actor
    });
    tokio::time::timeout(std::time::Duration::from_millis(750), async {
        let probe = "SELECT EXISTS (SELECT 1 FROM pg_stat_activity
                     WHERE datname = current_database() AND $1 = ANY(pg_blocking_pids(pid)))";
        while !sqlx::query_scalar::<_, bool>(probe)
            .bind(blocker)
            .fetch_one(&pool)
            .await
            .unwrap()
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("recovery close must wait behind the row lock");
    fence.record_loss(LOST);
    writer.commit().await.unwrap();
    let mut actor = turn.await.unwrap();
    assert!(!actor.capture_allowed());
    actor.turn(&pool).await.unwrap();
    assert!(actor.capture_allowed());
    db.close().await.unwrap();
}

#[tokio::test]
async fn turn_deadline_includes_pool_wait_and_keeps_original_evidence() {
    let db = database().await;
    let pool = db.pool();
    seed(pool, LOST, 1).await;
    let mut held = Vec::new();
    for _ in 0..pool.options().get_max_connections() {
        held.push(pool.acquire().await.unwrap());
    }
    let mut actor = AttendanceRecovery::new(GUILD, MemberLossFence::default());
    let departure = JOIN + Duration::minutes(20);
    actor.retain_departure(LOST, ROOM, departure);
    tokio::time::pause();
    let started = tokio::time::Instant::now();
    assert!(matches!(actor.turn(pool).await, Err(TurnError::Deadline)));
    let elapsed = started.elapsed();
    // Tokio rounds deadlines up to the next millisecond tick.
    assert!(elapsed >= std::time::Duration::from_millis(1_000));
    assert!(elapsed <= std::time::Duration::from_millis(1_001));
    tokio::time::resume();
    drop(held);

    actor.retain_departure(NEIGHBOR, ROOM, departure + Duration::minutes(1));
    actor.turn(pool).await.unwrap();
    assert_eq!(counts(pool, LOST, departure).await, (0, 0, 1));
    db.close().await.unwrap();
}

#[tokio::test]
async fn thirty_third_loss_latches_capture_stop_without_sweeping() {
    let db = database().await;
    let pool = db.pool();
    let fence = MemberLossFence::default();
    let mut actor = AttendanceRecovery::new(GUILD, fence.clone());
    let unknown = LOST + 32;
    let healthy = unknown + 1;
    let departure = JOIN + Duration::minutes(20);
    for member in LOST..=healthy {
        seed(pool, member, 1).await;
    }

    for member in LOST..unknown {
        fence.record_loss(member);
    }
    // Updating an existing loss at capacity must not latch the stop.
    fence.record_loss(LOST);
    for _ in LOST..unknown {
        actor.turn(pool).await.unwrap();
    }
    assert!(actor.capture_allowed());

    for member in LOST..unknown {
        seed(pool, member, 1).await;
        fence.record_loss(member);
    }
    fence.record_loss(unknown);
    for _ in LOST..unknown {
        actor.turn(pool).await.unwrap();
    }
    actor.turn(pool).await.unwrap();
    assert!(!actor.capture_allowed());
    for member in LOST..unknown {
        assert_eq!(counts(pool, member, departure).await, (0, 2, 0));
    }
    for member in [unknown, healthy] {
        assert_eq!(counts(pool, member, departure).await, (1, 0, 0));
    }
    db.close().await.unwrap();
}
