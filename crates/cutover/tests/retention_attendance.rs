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

use std::time::Duration;

use sqlx::{postgres::PgPoolOptions, PgPool};
use support::{with_database, GUILD_A, GUILD_B, MEMBER};
use time::OffsetDateTime;
use two_bot_core::{
    event_instance_store::confirm_deleted,
    internal_action_store::InternalActionStore,
    scheduled_events::{EventStatus, ScheduledEvent},
    website_store::{replace_events, upsert_event},
};
use two_bot_cutover::{
    event_attendance::{insert_attendance_session, NewAttendanceSession},
    retention::{handle_guild_delete, run_retention, RetentionMode},
};

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

async fn seed_guild_history(pool: &PgPool, guild: &str, event_id: &str) {
    sqlx::query(
        "INSERT INTO event_attendance_windows \
         (guild_id, event_key, event_id, instance_starts_at, starts_at, ends_at) \
         VALUES ($1, 'game-night', $2, now() - make_interval(hours => 2), \
                 now() - make_interval(hours => 2), now() - make_interval(hours => 1))",
    )
    .bind(guild)
    .bind(event_id)
    .execute(pool)
    .await
    .expect("seed window history");
    sqlx::query(
        "INSERT INTO event_voice_sessions \
         (guild_id, member_id, channel_id, channel_name, is_bot, afk, joined_at, left_at, event_keys) \
         VALUES ($1, $2, $3, 'lobby', FALSE, FALSE, now() - make_interval(hours => 2), \
                 now() - make_interval(hours => 1), ARRAY['game-night'])",
    )
    .bind(guild)
    .bind(MEMBER)
    .bind(CHANNEL)
    .execute(pool)
    .await
    .expect("seed session");
}

async fn window_count_for(pool: &PgPool, guild: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM event_attendance_windows WHERE guild_id = $1")
        .bind(guild)
        .fetch_one(pool)
        .await
        .expect("count guild windows")
}

#[tokio::test]
async fn guild_purge_removes_attendance_window_history_so_the_key_can_be_reused() {
    with_database(|pool| async move {
        seed_guild_history(&pool, GUILD_A, "2001").await;
        // The same key in another guild is unrelated and must survive.
        seed_guild_history(&pool, GUILD_B, "2002").await;
        handle_guild_delete(&pool, GUILD_A, None)
            .await
            .expect("queue guild purge");
        let dry = run_retention(&pool, RetentionMode::DryRun)
            .await
            .expect("dry-run retention");
        assert_eq!(dry.guild_rows_purged, 2, "one window plus one session");
        assert_eq!(
            window_count_for(&pool, GUILD_A).await,
            1,
            "dry run keeps it"
        );
        assert_eq!(session_count(&pool).await, 2, "dry run deletes nothing");
        let live = run_retention(&pool, RetentionMode::Execute)
            .await
            .expect("execute retention");
        assert_eq!(live.guild_rows_purged, dry.guild_rows_purged);
        assert_eq!(window_count_for(&pool, GUILD_A).await, 0, "history is gone");
        assert_eq!(
            window_count_for(&pool, GUILD_B).await,
            1,
            "other guild kept"
        );
        assert_eq!(
            session_count(&pool).await,
            1,
            "only the other guild's session"
        );
        // A rejoined guild can capture the same key for a new accepted instance
        // through the real mapping/lifecycle/capture writers, not a direct insert.
        let now = OffsetDateTime::now_utc();
        let iso = |at: OffsetDateTime| {
            two_bot_core::funnel::format_iso_millis((at.unix_timestamp_nanos() / 1_000_000) as i64)
        };
        let event = ScheduledEvent {
            id: "20000000000000003".to_owned(),
            name: "New game night".to_owned(),
            starts_at: iso(now),
            ends_at: Some(iso(now + time::Duration::hours(1))),
            channel_id: None,
            description: None,
            status: EventStatus::Active,
        };
        upsert_event(&pool, GUILD_A, &iso(now), &event)
            .await
            .unwrap();
        InternalActionStore::new(pool.clone())
            .put_event_key(GUILD_A, "game-night", &event.id)
            .await
            .unwrap();
        let id = insert_attendance_session(
            &pool,
            &NewAttendanceSession {
                guild_id: GUILD_A.to_owned(),
                member_id: MEMBER.to_owned(),
                channel_id: CHANNEL.to_owned(),
                channel_name: "Lobby".to_owned(),
                is_bot: false,
                afk: false,
                joined_at: now,
                event_keys: vec!["game-night".to_owned()],
            },
        )
        .await
        .expect("recapture the purged key");
        assert!(id > 0);
        assert_eq!(window_count_for(&pool, GUILD_A).await, 1);
        assert_eq!(session_count(&pool).await, 2);
    })
    .await;
}

// ---------------------------------------------------------------------------
// Retention versus the real lifecycle writers (observe, omit, delete).
//
// Each case drives the actual `run_retention(Execute)` future against one
// actual writer future. Test-only statement triggers (installed after seeding,
// in the disposable database) block on a test-held advisory lock, which parks a
// side at a known point without sleeping or touching production code:
//   * writer-first: the writer is parked after its instance-evidence write,
//     before it touches attendance history, holding the shared retention gate.
//   * retention-first: retention is parked after the gate and the sorted table
//     locks, at its attendance-session delete.
// The other side is then started and the test polls `pg_locks` (bounded, with
// `yield_now`) for it waiting on the retention gate. Without the gate, retention
// instead takes the attendance table lock and waits for the instance table
// (writer-first, a lock-order deadlock once released), or the writer waits on a
// relation lock (retention-first); neither ever waits on the gate, so the gate
// assertion fails. Both sides must then commit and the history stop must exist.

const BARRIER_CLASS: i32 = 7001;
const WRITER_BARRIER: i32 = 1;
const RETENTION_BARRIER: i32 = 2;
const POLL_BOUND: Duration = Duration::from_secs(3);
const JOIN_BOUND: Duration = Duration::from_secs(15);
const LIFECYCLE_EVENT: &str = "20000000000000010";

#[derive(Clone, Copy, Debug)]
enum Writer {
    Observe,
    Omit,
    Delete,
}

impl Writer {
    fn status(self) -> &'static str {
        match self {
            Writer::Observe => "cancelled",
            Writer::Omit => "missing",
            Writer::Delete => "deleted",
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum First {
    Writer,
    Retention,
}

fn iso_at(at: OffsetDateTime) -> String {
    two_bot_core::funnel::format_iso_millis((at.unix_timestamp_nanos() / 1_000_000) as i64)
}

/// Poll until `sql` (one scalar count, bound with `arg`) is positive.
async fn poll_positive(control: &PgPool, sql: &str, arg: &str) -> bool {
    let deadline = tokio::time::Instant::now() + POLL_BOUND;
    loop {
        let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
            .bind(arg)
            .fetch_one(control)
            .await
            .expect("poll pg_locks");
        if count > 0 {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::task::yield_now().await;
    }
}

/// A side is parked on the test-held barrier advisory lock.
async fn barrier_parked(control: &PgPool, key: i32) -> bool {
    let sql = format!(
        "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND NOT granted \
         AND database = (SELECT oid FROM pg_database WHERE datname = current_database()) \
         AND objsubid = 2 AND classid::int = {BARRIER_CLASS} AND objid::int = $1::text::int"
    );
    poll_positive(control, &sql, &key.to_string()).await
}

/// A side waits on the retention gate in `mode` (`ExclusiveLock` = retention,
/// `ShareLock` = a lifecycle writer).
async fn gate_waiter(control: &PgPool, mode: &str) -> bool {
    poll_positive(
        control,
        "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND NOT granted \
         AND database = (SELECT oid FROM pg_database WHERE datname = current_database()) \
         AND mode = $1 \
         AND ((classid::bigint << 32) | objid::bigint) = hashtextextended('event_instance_retention', 0)",
        mode,
    )
    .await
}

fn lifecycle_event(status: EventStatus, now: OffsetDateTime) -> ScheduledEvent {
    ScheduledEvent {
        id: LIFECYCLE_EVENT.to_owned(),
        name: "Lifecycle night".to_owned(),
        starts_at: iso_at(now - time::Duration::minutes(30)),
        ends_at: Some(iso_at(now + time::Duration::minutes(30))),
        channel_id: None,
        description: None,
        status,
    }
}

fn spawn_writer(
    writer: Writer,
    pool: &PgPool,
    observed_at: String,
    now: OffsetDateTime,
) -> tokio::task::JoinHandle<Result<(), String>> {
    let pool = pool.clone();
    tokio::spawn(async move {
        let result = match writer {
            Writer::Observe => {
                let event = lifecycle_event(EventStatus::Cancelled, now);
                upsert_event(&pool, GUILD_A, &observed_at, &event).await
            }
            Writer::Omit => replace_events(&pool, GUILD_A, &observed_at, &[])
                .await
                .map(|skip| assert!(skip.is_none(), "the omission snapshot must be accepted")),
            Writer::Delete => confirm_deleted(&pool, GUILD_A, LIFECYCLE_EVENT, &observed_at).await,
        };
        result.map_err(|error| error.to_string())
    })
}

async fn join_bounded<T>(handle: tokio::task::JoinHandle<T>, what: &str) -> Option<T> {
    match tokio::time::timeout(JOIN_BOUND, handle).await {
        Ok(joined) => Some(joined.unwrap_or_else(|error| panic!("{what} task failed: {error}"))),
        Err(_) => None,
    }
}

async fn lifecycle_race(writer: Writer, first: First) {
    with_database(move |pool| async move {
        let now = OffsetDateTime::now_utc();
        // Establish live evidence and a captured history row through the real
        // writer, then arm the barriers for the racing pair only.
        upsert_event(
            &pool,
            GUILD_A,
            &iso_at(now - time::Duration::minutes(10)),
            &lifecycle_event(EventStatus::Active, now),
        )
        .await
        .expect("seed live instance evidence");
        sqlx::query(
            "INSERT INTO event_attendance_windows \
             (guild_id, event_key, event_id, instance_starts_at, starts_at, ends_at) \
             VALUES ($1, 'game-night', $2, $3::text::timestamptz, \
                     $3::text::timestamptz, $4::text::timestamptz)",
        )
        .bind(GUILD_A)
        .bind(LIFECYCLE_EVENT)
        .bind(iso_at(now - time::Duration::minutes(30)))
        .bind(iso_at(now + time::Duration::minutes(30)))
        .execute(&pool)
        .await
        .expect("seed captured history");
        sqlx::query(
            "CREATE FUNCTION test_retention_barrier() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN PERFORM pg_advisory_xact_lock(7001, TG_ARGV[0]::int); RETURN NULL; END $$",
        )
        .execute(&pool)
        .await
        .expect("create barrier function");
        for (name, table, key) in [
            ("test_writer_barrier", "event_instance_windows", WRITER_BARRIER),
            ("test_retention_barrier", "event_voice_sessions", RETENTION_BARRIER),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "CREATE TRIGGER {name} AFTER INSERT OR UPDATE OR DELETE ON {table} \
                 FOR EACH STATEMENT EXECUTE FUNCTION test_retention_barrier('{key}')"
            )))
            .execute(&pool)
            .await
            .expect("create barrier trigger");
        }

        // Separate pool: barrier holder and poller never compete with the pair.
        let control = PgPoolOptions::new()
            .max_connections(2)
            .connect_with((*pool.connect_options()).clone())
            .await
            .expect("control pool");
        let observed_at = iso_at(now);
        let (parked_key, second_mode) = match first {
            First::Writer => (WRITER_BARRIER, "ExclusiveLock"),
            First::Retention => (RETENTION_BARRIER, "ShareLock"),
        };
        let mut barrier = control.begin().await.expect("barrier tx");
        sqlx::query("SELECT pg_advisory_xact_lock(7001, $1)")
            .bind(parked_key)
            .execute(&mut *barrier)
            .await
            .expect("hold barrier");

        let start_retention = |pool: &PgPool| {
            let pool = pool.clone();
            tokio::spawn(async move {
                run_retention(&pool, RetentionMode::Execute)
                    .await
                    .map(|_| ())
                    .map_err(|error| format!("{error:?}"))
            })
        };
        let (writer_task, retention_task, first_parked, second_waiting);
        match first {
            First::Writer => {
                writer_task = spawn_writer(writer, &pool, observed_at.clone(), now);
                first_parked = barrier_parked(&control, parked_key).await;
                retention_task = start_retention(&pool);
                second_waiting = gate_waiter(&control, second_mode).await;
            }
            First::Retention => {
                retention_task = start_retention(&pool);
                first_parked = barrier_parked(&control, parked_key).await;
                writer_task = spawn_writer(writer, &pool, observed_at.clone(), now);
                second_waiting = gate_waiter(&control, second_mode).await;
            }
        }
        // Release before asserting so both futures finish and teardown is clean.
        barrier.rollback().await.expect("release barrier");
        let writer_result = join_bounded(writer_task, "writer").await;
        let retention_result = join_bounded(retention_task, "retention").await;

        assert!(first_parked, "{writer:?}/{first:?}: first side never parked");
        assert!(
            second_waiting,
            "{writer:?}/{first:?}: the second side must wait on the retention gate ({second_mode}), \
             not on table locks"
        );
        assert_eq!(writer_result, Some(Ok(())), "{writer:?}/{first:?}: writer must commit");
        assert_eq!(
            retention_result,
            Some(Ok(())),
            "{writer:?}/{first:?}: retention must commit"
        );
        let committed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM event_instance_windows WHERE guild_id = $1 AND event_id = $2 \
             AND status = $3",
        )
        .bind(GUILD_A)
        .bind(LIFECYCLE_EVENT)
        .bind(writer.status())
        .fetch_one(&pool)
        .await
        .expect("read instance evidence");
        assert_eq!(committed, 1, "{writer:?}/{first:?}: lifecycle write committed");
        let stopped: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM event_attendance_windows WHERE guild_id = $1 \
             AND event_key = 'game-night' AND cardinality(capture_stops) = 1 \
             AND capture_stops[1] = $2::text::timestamptz",
        )
        .bind(GUILD_A)
        .bind(&observed_at)
        .fetch_one(&pool)
        .await
        .expect("read history stop");
        assert_eq!(stopped, 1, "{writer:?}/{first:?}: history stop at the observation");
        control.close().await;
    })
    .await;
}

#[tokio::test]
async fn retention_gate_writer_first_observe() {
    lifecycle_race(Writer::Observe, First::Writer).await;
}

#[tokio::test]
async fn retention_gate_writer_first_omit() {
    lifecycle_race(Writer::Omit, First::Writer).await;
}

#[tokio::test]
async fn retention_gate_writer_first_delete() {
    lifecycle_race(Writer::Delete, First::Writer).await;
}

#[tokio::test]
async fn retention_gate_retention_first_observe() {
    lifecycle_race(Writer::Observe, First::Retention).await;
}

#[tokio::test]
async fn retention_gate_retention_first_omit() {
    lifecycle_race(Writer::Omit, First::Retention).await;
}

#[tokio::test]
async fn retention_gate_retention_first_delete() {
    lifecycle_race(Writer::Delete, First::Retention).await;
}
