//! Attendance capture consumes accepted lifecycle evidence, never key registration
//! or its own historical windows. Runs against the disposable CI test database.

#[path = "support/raw_expiry.rs"]
mod support;

use std::{future::Future, time::Duration as StdDuration};

use sqlx::PgPool;
use support::{with_database, GUILD_A, GUILD_B, MEMBER};
use time::{Duration, OffsetDateTime};
use two_bot_core::{
    internal_action_store::InternalActionStore,
    scheduled_events::{EventStatus, ScheduledEvent},
    website_store::{replace_events, upsert_event},
};
use two_bot_cutover::event_attendance::{
    attendance_sessions_for_event_key, attendance_window_for_key, attendance_windows_for_guild,
    close_attendance_session, insert_attendance_session, is_attendance_suppressed,
    is_attendance_voice_enabled, open_attendance_sessions, NewAttendanceSession,
};
use two_bot_cutover::{
    privacy::{opt_out_member, set_analytics_settings},
    CutoverDb,
};

// Exercise the public key writer with valid synthetic snowflakes, not the
// short numeric IDs accepted only by the underlying TEXT columns.
const EVENT_A: &str = "20000000000000001";
const EVENT_B: &str = "20000000000000002";

fn iso(at: OffsetDateTime) -> String {
    two_bot_core::funnel::format_iso_millis((at.unix_timestamp_nanos() / 1_000_000) as i64)
}

fn event(now: OffsetDateTime) -> ScheduledEvent {
    ScheduledEvent {
        id: EVENT_A.to_owned(),
        name: "Game night".to_owned(),
        starts_at: iso(now - Duration::minutes(20)),
        ends_at: Some(iso(now + Duration::minutes(20))),
        channel_id: None,
        description: None,
        status: EventStatus::Active,
    }
}

fn session(now: OffsetDateTime, keys: &[&str]) -> NewAttendanceSession {
    NewAttendanceSession {
        guild_id: GUILD_A.to_owned(),
        member_id: MEMBER.to_owned(),
        channel_id: "4001".to_owned(),
        channel_name: "Lobby".to_owned(),
        is_bot: false,
        afk: false,
        joined_at: now - Duration::minutes(10),
        event_keys: keys.iter().map(|key| (*key).to_owned()).collect(),
    }
}

async fn map(pool: &PgPool, guild: &str, key: &str, event_id: &str) {
    InternalActionStore::new(pool.clone())
        .put_event_key(guild, key, event_id)
        .await
        .expect("map key without asserting liveness");
}

#[tokio::test]
async fn mappings_and_historical_windows_never_authorize_capture() {
    with_database(|pool| async move {
        let now = OffsetDateTime::now_utc();
        map(&pool, GUILD_A, "game-night", EVENT_A).await;
        assert!(attendance_windows_for_guild(&pool, GUILD_A)
            .await
            .unwrap()
            .is_empty());
        assert!(
            insert_attendance_session(&pool, &session(now, &["game-night"]))
                .await
                .is_err()
        );
        assert!(attendance_window_for_key(&pool, GUILD_A, "game-night")
            .await
            .unwrap()
            .is_none());
        assert!(attendance_sessions_for_event_key(&pool, GUILD_A, "unknown")
            .await
            .unwrap()
            .is_empty());

        let mut observed = event(now);
        // A confirmed window already closed at enable time never gets history.
        observed.starts_at = iso(now - Duration::days(2));
        observed.ends_at = Some(iso(now - Duration::days(1)));
        upsert_event(&pool, GUILD_A, &iso(now - Duration::minutes(2)), &observed)
            .await
            .unwrap();
        assert!(attendance_windows_for_guild(&pool, GUILD_A)
            .await
            .unwrap()
            .is_empty());
        assert!(
            insert_attendance_session(&pool, &session(now, &["game-night"]))
                .await
                .is_err()
        );
        assert!(attendance_window_for_key(&pool, GUILD_A, "game-night")
            .await
            .unwrap()
            .is_none());

        observed = event(now);
        observed.ends_at = None;
        upsert_event(&pool, GUILD_A, &iso(now - Duration::minutes(1)), &observed)
            .await
            .unwrap();
        assert!(
            attendance_windows_for_guild(&pool, GUILD_A)
                .await
                .unwrap()
                .is_empty(),
            "unknown end cannot prove a finite attendance window"
        );
        assert!(attendance_windows_for_guild(&pool, GUILD_B)
            .await
            .unwrap()
            .is_empty());
    })
    .await;
}

#[tokio::test]
async fn capture_rechecks_privacy_and_commits_overlap_history_with_the_session() {
    with_database(|pool| async move {
        let now = OffsetDateTime::now_utc();
        let observed = event(now);
        upsert_event(&pool, GUILD_A, &iso(now - Duration::minutes(2)), &observed)
            .await
            .unwrap();
        map(&pool, GUILD_A, "game-night", EVENT_A).await;
        let mut other = observed.clone();
        other.id = EVENT_B.to_owned();
        upsert_event(&pool, GUILD_A, &iso(now - Duration::minutes(2)), &other)
            .await
            .unwrap();
        map(&pool, GUILD_A, "overlap", EVENT_B).await;
        assert_eq!(
            attendance_windows_for_guild(&pool, GUILD_A)
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(!is_attendance_suppressed(&pool, GUILD_A, MEMBER)
            .await
            .unwrap());
        assert!(is_attendance_voice_enabled(&pool, GUILD_A).await.unwrap());
        sqlx::query("INSERT INTO privacy_opt_outs (guild_id, member_id) VALUES ($1, $2)")
            .bind(GUILD_A)
            .bind(MEMBER)
            .execute(&pool)
            .await
            .unwrap();
        assert!(is_attendance_suppressed(&pool, GUILD_A, MEMBER)
            .await
            .unwrap());
        assert!(
            insert_attendance_session(&pool, &session(now, &["game-night", "overlap"]))
                .await
                .is_err()
        );
        assert!(attendance_window_for_key(&pool, GUILD_A, "game-night")
            .await
            .unwrap()
            .is_none());
        sqlx::query("DELETE FROM privacy_opt_outs WHERE guild_id = $1")
            .bind(GUILD_A)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO guild_analytics_settings (guild_id, voice_enabled) VALUES ($1, FALSE)",
        )
        .bind(GUILD_A)
        .execute(&pool)
        .await
        .unwrap();
        assert!(!is_attendance_voice_enabled(&pool, GUILD_A).await.unwrap());
        assert!(
            insert_attendance_session(&pool, &session(now, &["game-night"]))
                .await
                .is_err()
        );
        sqlx::query("UPDATE guild_analytics_settings SET voice_enabled = TRUE WHERE guild_id = $1")
            .bind(GUILD_A)
            .execute(&pool)
            .await
            .unwrap();
        let id =
            insert_attendance_session(&pool, &session(now, &["game-night", "overlap", "unknown"]))
                .await
                .unwrap();
        let rows = open_attendance_sessions(&pool, GUILD_A, MEMBER)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert_eq!(rows[0].event_keys, vec!["game-night", "overlap"]);
        assert!(open_attendance_sessions(&pool, GUILD_B, MEMBER)
            .await
            .unwrap()
            .is_empty());
        let history = InternalActionStore::new(pool.clone())
            .attendance_window_for_key(GUILD_A, "game-night")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(iso(history.starts_at), observed.starts_at);
        // Suppression stops new tags, but must not prevent finalization.
        sqlx::query("INSERT INTO privacy_opt_outs (guild_id, member_id) VALUES ($1, $2)")
            .bind(GUILD_A)
            .bind(MEMBER)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            close_attendance_session(&pool, id, now, "Renamed", &["unknown".to_owned()])
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            close_attendance_session(&pool, id, now, "Again", &[])
                .await
                .unwrap(),
            0
        );
        let rows = attendance_sessions_for_event_key(&pool, GUILD_A, "game-night")
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].channel_name, "Renamed");
        assert_eq!(rows[0].event_keys, vec!["game-night", "overlap"]);
        assert!(rows[0].left_at.is_some());
    })
    .await;
}

#[tokio::test]
async fn reschedules_omission_and_revocation_use_the_real_accepted_writers() {
    with_database(|pool| async move {
        let now = OffsetDateTime::now_utc();
        let mut observed = event(now);
        let stamp = |seconds| iso(now - Duration::seconds(seconds));
        upsert_event(&pool, GUILD_A, &stamp(120), &observed)
            .await
            .unwrap();
        map(&pool, GUILD_A, "game-night", EVENT_A).await;
        let id = insert_attendance_session(&pool, &session(now, &["game-night"]))
            .await
            .unwrap();
        let identity: OffsetDateTime = sqlx::query_scalar(
            "SELECT instance_starts_at FROM event_attendance_windows WHERE guild_id = $1",
        )
        .bind(GUILD_A)
        .fetch_one(&pool)
        .await
        .unwrap();
        observed.starts_at = iso(now + Duration::hours(2));
        observed.ends_at = Some(iso(now + Duration::hours(3)));
        upsert_event(&pool, GUILD_A, &stamp(110), &observed)
            .await
            .unwrap();
        assert!(attendance_windows_for_guild(&pool, GUILD_A)
            .await
            .unwrap()
            .is_empty());
        observed = event(now);
        upsert_event(&pool, GUILD_A, &stamp(100), &observed)
            .await
            .unwrap();
        assert_eq!(
            attendance_windows_for_guild(&pool, GUILD_A)
                .await
                .unwrap()
                .len(),
            1
        );
        replace_events(&pool, GUILD_A, &stamp(90), &[])
            .await
            .unwrap();
        assert!(attendance_windows_for_guild(&pool, GUILD_A)
            .await
            .unwrap()
            .is_empty());
        assert!(
            insert_attendance_session(&pool, &session(now, &["game-night"]))
                .await
                .is_err()
        );
        let clipped = InternalActionStore::new(pool.clone())
            .attendance_sessions_for_event_key(GUILD_A, "game-night")
            .await
            .unwrap();
        assert_eq!(clipped.len(), 1);
        assert_eq!(iso(clipped[0].left_at.unwrap()), stamp(90));
        // Positive completion can prove finite grace, unlike omission alone.
        observed.status = EventStatus::Completed;
        upsert_event(&pool, GUILD_A, &stamp(80), &observed)
            .await
            .unwrap();
        replace_events(&pool, GUILD_A, &stamp(70), &[])
            .await
            .unwrap();
        assert_eq!(
            attendance_windows_for_guild(&pool, GUILD_A)
                .await
                .unwrap()
                .len(),
            1
        );
        observed.status = EventStatus::Cancelled;
        upsert_event(&pool, GUILD_A, &stamp(60), &observed)
            .await
            .unwrap();
        observed.status = EventStatus::Active;
        upsert_event(&pool, GUILD_A, &stamp(60), &observed)
            .await
            .unwrap();
        assert!(
            attendance_windows_for_guild(&pool, GUILD_A)
                .await
                .unwrap()
                .is_empty(),
            "equal replay cannot revive cancellation"
        );
        assert!(
            insert_attendance_session(&pool, &session(now, &["game-night"]))
                .await
                .is_err()
        );
        assert_eq!(
            close_attendance_session(&pool, id, now, "Lobby", &[])
                .await
                .unwrap(),
            1
        );
        let rows = attendance_sessions_for_event_key(&pool, GUILD_A, "game-night")
            .await
            .unwrap();
        assert_eq!(
            iso(rows[0].left_at.unwrap()),
            stamp(90),
            "completion and cancellation must not fill an earlier unconfirmed gap"
        );
        let retained: OffsetDateTime = sqlx::query_scalar(
            "SELECT instance_starts_at FROM event_attendance_windows WHERE guild_id = $1",
        )
        .bind(GUILD_A)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            identity, retained,
            "rescheduling does not replace occurrence identity"
        );
    })
    .await;
}

#[tokio::test]
async fn repointed_keys_do_not_mix_instances_and_deletion_revokes_capture() {
    with_database(|pool| async move {
        let now = OffsetDateTime::now_utc();
        let observed = event(now);
        upsert_event(&pool, GUILD_A, &iso(now - Duration::minutes(2)), &observed)
            .await
            .unwrap();
        map(&pool, GUILD_A, "game-night", EVENT_A).await;
        insert_attendance_session(&pool, &session(now, &["game-night"]))
            .await
            .unwrap();
        let mut other = observed.clone();
        other.id = EVENT_B.to_owned();
        upsert_event(&pool, GUILD_A, &iso(now - Duration::minutes(1)), &other)
            .await
            .unwrap();
        map(&pool, GUILD_A, "game-night", EVENT_B).await;
        assert!(attendance_windows_for_guild(&pool, GUILD_A)
            .await
            .unwrap()
            .is_empty());
        assert!(
            insert_attendance_session(&pool, &session(now, &["game-night"]))
                .await
                .is_err()
        );
        map(&pool, GUILD_A, "new-key", EVENT_B).await;
        assert_eq!(
            attendance_windows_for_guild(&pool, GUILD_A).await.unwrap()[0].event_key,
            "new-key"
        );
        let id = insert_attendance_session(&pool, &session(now, &["new-key"]))
            .await
            .unwrap();
        two_bot_core::event_instance_store::confirm_deleted(&pool, GUILD_A, EVENT_B, &iso(now))
            .await
            .unwrap();
        assert!(attendance_windows_for_guild(&pool, GUILD_A)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            close_attendance_session(&pool, id, now, "Lobby", &[])
                .await
                .unwrap(),
            1
        );
        assert!(attendance_window_for_key(&pool, GUILD_A, "new-key")
            .await
            .unwrap()
            .is_some());
        assert_eq!(
            attendance_sessions_for_event_key(&pool, GUILD_A, "game-night")
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            attendance_sessions_for_event_key(&pool, GUILD_B, "game-night")
                .await
                .unwrap()
                .is_empty()
        );
    })
    .await;
}

#[tokio::test]
async fn failed_session_write_rolls_back_history_and_new_close_tags_are_revalidated() {
    with_database(|pool| async move {
        let now = OffsetDateTime::now_utc();
        let observed = event(now);
        upsert_event(&pool, GUILD_A, &iso(now - Duration::minutes(2)), &observed).await.unwrap();
        map(&pool, GUILD_A, "game-night", EVENT_A).await;
        sqlx::query("ALTER TABLE event_voice_sessions ADD CONSTRAINT reject_test_capture CHECK (channel_id <> '4001')")
            .execute(&pool).await.unwrap();
        assert!(insert_attendance_session(&pool, &session(now, &["game-night"])).await.is_err());
        assert!(attendance_window_for_key(&pool, GUILD_A, "game-night").await.unwrap().is_none());
        sqlx::query("ALTER TABLE event_voice_sessions DROP CONSTRAINT reject_test_capture").execute(&pool).await.unwrap();
        let id = insert_attendance_session(&pool, &session(now, &["game-night"])).await.unwrap();
        let mut overlap = observed.clone();
        overlap.id = EVENT_B.to_owned();
        upsert_event(&pool, GUILD_A, &iso(now - Duration::minutes(1)), &overlap).await.unwrap();
        map(&pool, GUILD_A, "overlap", EVENT_B).await;
        assert_eq!(close_attendance_session(&pool, id, now, "Lobby", &["game-night".to_owned(), "overlap".to_owned(), "unknown".to_owned()]).await.unwrap(), 1);
        assert!(attendance_window_for_key(&pool, GUILD_A, "overlap").await.unwrap().is_some());
        let rows = attendance_sessions_for_event_key(&pool, GUILD_A, "overlap").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event_keys, vec!["game-night", "overlap"]);
        assert_eq!(close_attendance_session(&pool, i64::MAX, now, "Lobby", &[]).await.unwrap(), 0);
    }).await;
}

fn lock_key(kind: &str, member: bool) -> String {
    if member {
        format!("{kind}:{GUILD_A}:{MEMBER}")
    } else {
        format!("{kind}:{GUILD_A}")
    }
}

async fn advisory_waiters(pool: &PgPool, key: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND NOT granted
           AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
           AND objsubid = 1
           AND ((classid::bigint << 32) | objid::bigint) = hashtextextended($1, 0)",
    )
    .bind(key)
    .fetch_one(pool)
    .await
    .unwrap()
}

// Observe real lock waiters in pg_locks instead of sleeping: a blocker holds
// `key`, `first` queues, then `second` queues behind it, and only then is the
// blocker released. Advisory lock waiters are granted in queue order.
async fn queued_race<A, B>(pool: &PgPool, key: &str, first: A, second: B) -> (A::Output, B::Output)
where
    A: Future + Send + 'static,
    A::Output: Send + 'static,
    B: Future + Send + 'static,
    B::Output: Send + 'static,
{
    async fn wait_for(pool: &PgPool, key: &str, want: i64) {
        tokio::time::timeout(StdDuration::from_secs(4), async {
            while advisory_waiters(pool, key).await < want {
                tokio::time::sleep(StdDuration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("expected {want} queued waiters on {key}"));
    }
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(key)
        .execute(&mut *blocker)
        .await
        .unwrap();
    let first = tokio::spawn(first);
    wait_for(pool, key, 1).await;
    let second = tokio::spawn(second);
    wait_for(pool, key, 2).await;
    blocker.commit().await.unwrap();
    (first.await.unwrap(), second.await.unwrap())
}

async fn count(pool: &PgPool, table: &str) -> i64 {
    assert!(matches!(
        table,
        "privacy_opt_outs"
            | "guild_analytics_settings"
            | "event_voice_sessions"
            | "event_attendance_windows"
    ));
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn two_windows(pool: &PgPool, now: OffsetDateTime) {
    let observed = event(now);
    let stamp = iso(now - Duration::minutes(2));
    upsert_event(pool, GUILD_A, &stamp, &observed)
        .await
        .unwrap();
    map(pool, GUILD_A, "game-night", EVENT_A).await;
    let mut other = observed;
    other.id = EVENT_B.to_owned();
    upsert_event(pool, GUILD_A, &stamp, &other).await.unwrap();
    map(pool, GUILD_A, "overlap", EVENT_B).await;
}

#[tokio::test]
async fn queued_opt_out_commits_before_capture_rechecks_with_no_suppression_row() {
    with_database(|pool| async move {
        let now = OffsetDateTime::now_utc();
        two_windows(&pool, now).await;
        assert_eq!(count(&pool, "privacy_opt_outs").await, 0);
        let db = CutoverDb::from_pool(pool.clone());
        let (opt, insert) = queued_race(
            &pool,
            &lock_key("privacy", true),
            async move { opt_out_member(&db, GUILD_A, MEMBER, "operator").await },
            {
                let (pool, new) = (pool.clone(), session(now, &["game-night"]));
                async move { insert_attendance_session(&pool, &new).await }
            },
        )
        .await;
        opt.unwrap();
        assert!(
            insert.is_err(),
            "capture must observe the committed opt-out"
        );
        assert!(is_attendance_suppressed(&pool, GUILD_A, MEMBER)
            .await
            .unwrap());
        assert_eq!(count(&pool, "event_voice_sessions").await, 0);
        assert_eq!(count(&pool, "event_attendance_windows").await, 0);
    })
    .await;
}

#[tokio::test]
async fn queued_voice_disable_commits_before_capture_rechecks_with_no_settings_row() {
    with_database(|pool| async move {
        let now = OffsetDateTime::now_utc();
        two_windows(&pool, now).await;
        assert_eq!(count(&pool, "guild_analytics_settings").await, 0);
        let db = CutoverDb::from_pool(pool.clone());
        let (set, insert) = queued_race(
            &pool,
            &lock_key("analytics_settings", false),
            async move { set_analytics_settings(&db, GUILD_A, true, true, false).await },
            {
                let (pool, new) = (pool.clone(), session(now, &["game-night"]));
                async move { insert_attendance_session(&pool, &new).await }
            },
        )
        .await;
        set.unwrap();
        assert!(
            insert.is_err(),
            "capture must observe the committed disable"
        );
        assert!(!is_attendance_voice_enabled(&pool, GUILD_A).await.unwrap());
        assert_eq!(count(&pool, "event_voice_sessions").await, 0);
        assert_eq!(count(&pool, "event_attendance_windows").await, 0);
    })
    .await;
}

async fn assert_close_adds_no_new_tag(pool: &PgPool, id: i64, closed: u64) {
    assert_eq!(closed, 1, "suppression never blocks finalization");
    let rows = attendance_sessions_for_event_key(pool, GUILD_A, "game-night")
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, id);
    assert_eq!(rows[0].event_keys, vec!["game-night"]);
    assert!(rows[0].left_at.is_some());
    assert!(attendance_sessions_for_event_key(pool, GUILD_A, "overlap")
        .await
        .unwrap()
        .is_empty());
    assert!(attendance_window_for_key(pool, GUILD_A, "overlap")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn queued_opt_out_or_voice_disable_blocks_new_close_time_tag() {
    with_database(|pool| async move {
        let now = OffsetDateTime::now_utc();
        let both = vec!["game-night".to_owned(), "overlap".to_owned()];
        // Opt-out first, then a close that requests an extra tag.
        let observed = event(now);
        upsert_event(&pool, GUILD_A, &iso(now - Duration::minutes(2)), &observed)
            .await
            .unwrap();
        map(&pool, GUILD_A, "game-night", EVENT_A).await;
        let id = insert_attendance_session(&pool, &session(now, &["game-night"]))
            .await
            .unwrap();
        two_windows(&pool, now).await;
        let db = CutoverDb::from_pool(pool.clone());
        let (opt, closed) = queued_race(
            &pool,
            &lock_key("privacy", true),
            async move { opt_out_member(&db, GUILD_A, MEMBER, "operator").await },
            {
                let (pool, both) = (pool.clone(), both.clone());
                async move { close_attendance_session(&pool, id, now, "Lobby", &both).await }
            },
        )
        .await;
        opt.unwrap();
        assert_close_adds_no_new_tag(&pool, id, closed.unwrap()).await;

        // Voice disable first, on a fresh member with no suppression row.
        sqlx::query("DELETE FROM privacy_opt_outs")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM event_voice_sessions")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM event_attendance_windows")
            .execute(&pool)
            .await
            .unwrap();
        let id = insert_attendance_session(&pool, &session(now, &["game-night"]))
            .await
            .unwrap();
        assert_eq!(count(&pool, "guild_analytics_settings").await, 0);
        let db = CutoverDb::from_pool(pool.clone());
        let (set, closed) = queued_race(
            &pool,
            &lock_key("analytics_settings", false),
            async move { set_analytics_settings(&db, GUILD_A, true, true, false).await },
            {
                let (pool, both) = (pool.clone(), both.clone());
                async move { close_attendance_session(&pool, id, now, "Lobby", &both).await }
            },
        )
        .await;
        set.unwrap();
        assert_close_adds_no_new_tag(&pool, id, closed.unwrap()).await;
    })
    .await;
}

#[tokio::test]
async fn first_close_time_tag_across_an_omission_gets_no_backcredit() {
    with_database(|pool| async move {
        let now = OffsetDateTime::now_utc();
        let stamp = |minutes| iso(now - Duration::minutes(minutes));
        let mut a = event(now);
        a.starts_at = iso(now - Duration::minutes(60));
        let mut b = event(now);
        b.id = EVENT_B.to_owned();
        b.starts_at = iso(now - Duration::minutes(10));
        b.ends_at = Some(iso(now + Duration::minutes(30)));
        upsert_event(&pool, GUILD_A, &stamp(45), &a).await.unwrap();
        upsert_event(&pool, GUILD_A, &stamp(45), &b).await.unwrap();
        map(&pool, GUILD_A, "a", EVENT_A).await;
        map(&pool, GUILD_A, "b", EVENT_B).await;

        let mut first = session(now, &["a"]);
        first.joined_at = now - Duration::minutes(40);
        let id = insert_attendance_session(&pool, &first).await.unwrap();
        // B is omitted twice before it is observed again; A stays present.
        replace_events(&pool, GUILD_A, &stamp(20), std::slice::from_ref(&a))
            .await
            .unwrap();
        replace_events(&pool, GUILD_A, &stamp(15), std::slice::from_ref(&a))
            .await
            .unwrap();
        upsert_event(&pool, GUILD_A, &stamp(10), &b).await.unwrap();
        assert_eq!(
            attendance_windows_for_guild(&pool, GUILD_A)
                .await
                .unwrap()
                .len(),
            2,
            "B is capturable again for spans that begin after the omission"
        );

        let requested = vec!["a".to_owned(), "b".to_owned()];
        assert_eq!(
            close_attendance_session(&pool, id, now, "Lobby", &requested)
                .await
                .unwrap(),
            1
        );
        let a_rows = attendance_sessions_for_event_key(&pool, GUILD_A, "a")
            .await
            .unwrap();
        assert_eq!(a_rows.len(), 1);
        assert_eq!(a_rows[0].event_keys, vec!["a"]);
        assert!(a_rows[0].left_at.is_some());
        assert!(attendance_window_for_key(&pool, GUILD_A, "b")
            .await
            .unwrap()
            .is_none());
        assert!(attendance_sessions_for_event_key(&pool, GUILD_A, "b")
            .await
            .unwrap()
            .is_empty());

        // Delayed joins that happened during the absence cannot back-credit
        // from their timestamp merely because positive evidence exists now.
        let mut during_gap = session(now, &["b"]);
        during_gap.joined_at = now - Duration::minutes(12);
        assert!(insert_attendance_session(&pool, &during_gap).await.is_err());
        let mut gap_a = session(now, &["a"]);
        gap_a.joined_at = during_gap.joined_at;
        let gap_a_id = insert_attendance_session(&pool, &gap_a).await.unwrap();
        assert_eq!(
            close_attendance_session(&pool, gap_a_id, now, "Lobby", &["b".to_owned()])
                .await
                .unwrap(),
            1
        );
        assert!(attendance_window_for_key(&pool, GUILD_A, "b")
            .await
            .unwrap()
            .is_none());

        let mut fresh = session(now, &["b"]);
        fresh.joined_at = now - Duration::minutes(5);
        upsert_event(&pool, GUILD_A, &stamp(2), &b).await.unwrap();
        let recovered: OffsetDateTime = sqlx::query_scalar("SELECT attendance_recovered_at FROM event_instance_windows WHERE guild_id = $1 AND event_id = $2 AND is_current")
            .bind(GUILD_A).bind(EVENT_B).fetch_one(&pool).await.unwrap();
        assert_eq!(iso(recovered), stamp(10), "positive refresh must not move recovery");
        let fresh_id = insert_attendance_session(&pool, &fresh).await.unwrap();
        let b_rows = attendance_sessions_for_event_key(&pool, GUILD_A, "b")
            .await
            .unwrap();
        assert_eq!(b_rows.len(), 1);
        assert_eq!(b_rows[0].id, fresh_id);
        assert_eq!(b_rows[0].event_keys, vec!["b"]);
    })
    .await;
}

#[tokio::test]
async fn positive_refresh_preserves_post_recovery_close_time_overlap() {
    with_database(|pool| async move {
        let now = OffsetDateTime::now_utc();
        let stamp = |minutes| iso(now - Duration::minutes(minutes));
        let mut a = event(now);
        a.starts_at = iso(now - Duration::minutes(60));
        let mut b = event(now);
        b.id = EVENT_B.to_owned();
        b.starts_at = iso(now - Duration::minutes(5));
        upsert_event(&pool, GUILD_A, &stamp(60), &a).await.unwrap();
        upsert_event(&pool, GUILD_A, &stamp(60), &b).await.unwrap();
        map(&pool, GUILD_A, "a", EVENT_A).await;
        map(&pool, GUILD_A, "b", EVENT_B).await;
        replace_events(&pool, GUILD_A, &stamp(50), std::slice::from_ref(&a))
            .await
            .unwrap();
        upsert_event(&pool, GUILD_A, &stamp(40), &b).await.unwrap();
        let mut first = session(now, &["a"]);
        first.joined_at = now - Duration::minutes(30);
        let id = insert_attendance_session(&pool, &first).await.unwrap();
        upsert_event(&pool, GUILD_A, &stamp(10), &b).await.unwrap();
        assert_eq!(
            close_attendance_session(&pool, id, now, "Lobby", &["b".to_owned()])
                .await
                .unwrap(),
            1
        );
        let rows = attendance_sessions_for_event_key(&pool, GUILD_A, "b")
            .await
            .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "a harmless refresh must not discard the overlap"
        );
        assert_eq!(rows[0].event_keys, vec!["a", "b"]);
        assert!(attendance_window_for_key(&pool, GUILD_A, "b")
            .await
            .unwrap()
            .is_some());
    })
    .await;
}
