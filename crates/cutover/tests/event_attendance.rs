//! Attendance capture consumes accepted lifecycle evidence, never key registration
//! or its own historical windows. Runs against the disposable CI test database.

#[path = "support/raw_expiry.rs"]
mod support;

use std::{future::Future, time::Duration as StdDuration};

use sqlx::PgPool;
use support::{with_database, GUILD_A, GUILD_B, MEMBER};
use time::{Duration, OffsetDateTime};
use two_bot_core::{
    internal_action_store::{
        InternalActionStore, InternalStoreError, MAX_ATTENDANCE_CAPTURE_STOPS,
    },
    scheduled_events::{EventStatus, ScheduledEvent},
    website_store::{replace_events, upsert_event},
};
use two_bot_cutover::event_attendance::{
    attendance_member_recovery_page, attendance_recovery_page, attendance_sessions_for_event_key,
    attendance_window_for_key, attendance_windows_for_guild, attendance_windows_for_transition,
    close_attendance_session, insert_attendance_session, is_attendance_suppressed,
    is_attendance_voice_enabled, is_unsupported_stored_tags, open_attendance_sessions,
    recover_attendance_sessions, NewAttendanceSession, ATTENDANCE_MAX_TAG_BYTES,
    ATTENDANCE_MAX_WINDOWS, ATTENDANCE_RECOVERY_PAGE_SIZE,
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
async fn lifecycle_stop_is_exported_without_advancing_last_capture() {
    with_database(|pool| async move {
        let now = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap();
        upsert_event(
            &pool,
            GUILD_A,
            &iso(now - Duration::seconds(1)),
            &event(now),
        )
        .await
        .unwrap();
        map(&pool, GUILD_A, "game-night", EVENT_A).await;
        let id = insert_attendance_session(&pool, &session(now, &["game-night"]))
            .await
            .unwrap();
        let capture = now - Duration::minutes(5);
        let baseline = iso(now - Duration::minutes(1));
        sqlx::query("UPDATE event_attendance_windows SET updated_at = $1, changed_at = $1")
            .bind(capture)
            .execute(&pool)
            .await
            .unwrap();
        // A pre-baseline history row changes only through a real omission.
        replace_events(&pool, GUILD_A, &iso(now), &[])
            .await
            .unwrap();
        let (last_capture, changed, stops): (OffsetDateTime, OffsetDateTime, Vec<OffsetDateTime>) =
            sqlx::query_as(
                "SELECT updated_at, changed_at, capture_stops FROM event_attendance_windows",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(last_capture, capture);
        assert!(changed > now - Duration::minutes(1));
        assert_eq!(stops.len(), 1);
        // A late-processed leave changes the raw row after baseline even when
        // both of its gateway observation boundaries predate baseline.
        sqlx::query("UPDATE event_voice_sessions SET changed_at = $1")
            .bind(capture)
            .execute(&pool)
            .await
            .unwrap();
        close_attendance_session(&pool, id, now - Duration::minutes(2), None, &[])
            .await
            .unwrap();
        let summary = two_bot_cutover::rollback_delta::report(&pool, &baseline)
            .await
            .unwrap();
        let history = summary
            .tables
            .iter()
            .find(|row| row.table == "event_attendance_windows")
            .unwrap();
        assert_eq!(history.count, Some(1));
        assert_eq!(
            summary
                .tables
                .iter()
                .find(|row| row.table == "event_voice_sessions")
                .unwrap()
                .count,
            Some(1)
        );
        // The export's erased writer is not Send. Keep it on one blocking
        // thread while the async fixture remains Send for panic-safe teardown.
        let export_pool = pool.clone();
        let export_baseline = baseline.clone();
        let lines = tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(async move {
                let mut lines = Vec::new();
                two_bot_cutover::rollback_delta::export_delta(
                    &export_pool,
                    &export_baseline,
                    &mut |line: String| {
                        lines.push(line);
                        Ok::<(), std::io::Error>(())
                    },
                )
                .await
                .unwrap();
                lines
            })
        })
        .await
        .unwrap();
        let history: Vec<serde_json::Value> = lines
            .iter()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|row| row["table"] == "event_attendance_windows")
            .collect();
        assert_eq!(history.len(), 1);
        assert_eq!(
            history[0]["row"]["capture_stops"].as_array().unwrap().len(),
            1
        );
        // Repeated omission coalesces against last capture, not the new mutation stamp.
        replace_events(&pool, GUILD_A, &iso(now + Duration::seconds(1)), &[])
            .await
            .unwrap();
        let after: (OffsetDateTime, OffsetDateTime, Vec<OffsetDateTime>) = sqlx::query_as(
            "SELECT updated_at, changed_at, capture_stops FROM event_attendance_windows",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(after, (capture, changed, stops));
    })
    .await;
}

#[tokio::test]
async fn name_observation_survives_credit_caps_and_missing_metadata() {
    with_database(|pool| async move {
        let now = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap();
        upsert_event(
            &pool,
            GUILD_A,
            &iso(now - Duration::seconds(1)),
            &event(now),
        )
        .await
        .unwrap();
        map(&pool, GUILD_A, "game-night", EVENT_A).await;
        let a = session(now, &["game-night"]);
        let a_id = insert_attendance_session(&pool, &a).await.unwrap();
        let mut b = a.clone();
        b.member_id = "30000000000000002".to_owned();
        b.joined_at = now - Duration::minutes(5);
        let b_id = insert_attendance_session(&pool, &b).await.unwrap();
        close_attendance_session(&pool, b_id, now - Duration::minutes(3), Some("Old"), &[])
            .await
            .unwrap();
        close_attendance_session(&pool, a_id, now, Some("Renamed"), &[])
            .await
            .unwrap();
        // Another open row's known name is not re-observed by a missing-name close.
        let mut c = b;
        c.member_id = "30000000000000003".to_owned();
        c.joined_at = now - Duration::minutes(2);
        c.channel_name = "Older".to_owned();
        let c_id = insert_attendance_session(&pool, &c).await.unwrap();
        let original_stamp: OffsetDateTime =
            sqlx::query_scalar("SELECT name_observed_at FROM event_voice_sessions WHERE id = $1")
                .bind(c_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        close_attendance_session(&pool, c_id, now + Duration::seconds(1), None, &[])
            .await
            .unwrap();
        let raw: (String, OffsetDateTime) = sqlx::query_as(
            "SELECT channel_name, name_observed_at FROM event_voice_sessions WHERE id = $1",
        )
        .bind(c_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(raw, ("Older".to_owned(), original_stamp));
        // Cap voice credit before the latest name observation. Read ordering must
        // use the separate name stamp, never the capped left_at or join time.
        sqlx::query("UPDATE event_attendance_windows SET capture_stops = ARRAY[$1::timestamptz]")
            .bind(now - Duration::minutes(4))
            .execute(&pool)
            .await
            .unwrap();
        let store = InternalActionStore::new(pool.clone());
        let rows = store
            .attendance_sessions_for_event_key(GUILD_A, "game-night")
            .await
            .unwrap();
        let a = rows
            .iter()
            .find(|row| row.member_id == a.member_id)
            .unwrap();
        assert_eq!(a.channel_name, "Renamed");
        assert!(a.name_observed_at >= now);
        assert!(a.name_observed_at < original_stamp);
        assert_eq!(a.left_at, Some(now - Duration::minutes(4)));
    })
    .await;
}

#[tokio::test]
async fn signed_read_store_refuses_large_raw_history_even_with_one_output_member() {
    with_database(|pool| async move {
        let now = OffsetDateTime::now_utc();
        upsert_event(&pool, GUILD_A, &iso(now), &event(now)).await.unwrap();
        map(&pool, GUILD_A, "game-night", EVENT_A).await;
        let id = insert_attendance_session(&pool, &session(now, &["game-night"])).await.unwrap();
        sqlx::query("INSERT INTO event_voice_sessions (guild_id, member_id, channel_id, channel_name, is_bot, afk, joined_at, left_at, event_keys)
            SELECT s.guild_id, s.member_id, s.channel_id, s.channel_name, s.is_bot, s.afk, s.joined_at, s.left_at, s.event_keys
            FROM event_voice_sessions s CROSS JOIN generate_series(1, $2::int) WHERE s.id = $1")
            .bind(id).bind(two_bot_core::internal_action_store::MAX_ATTENDANCE_INPUT_SESSIONS as i32)
            .execute(&pool).await.unwrap();
        let store = InternalActionStore::new(pool.clone());
        assert!(store.attendance_sessions_for_event_key(GUILD_A, "game-night").await.is_err());
        sqlx::query("DELETE FROM event_voice_sessions WHERE id = $1").bind(id).execute(&pool).await.unwrap();
        let rows = store.attendance_sessions_for_event_key(GUILD_A, "game-night").await.unwrap();
        assert_eq!(rows.len(), two_bot_core::internal_action_store::MAX_ATTENDANCE_INPUT_SESSIONS);
        sqlx::query("UPDATE event_voice_sessions SET channel_name = repeat('x', 2000000) WHERE id = (SELECT MIN(id) FROM event_voice_sessions)")
            .execute(&pool).await.unwrap();
        assert!(store.attendance_sessions_for_event_key(GUILD_A, "game-night").await.is_err());
    }).await;
}

#[tokio::test]
async fn continuous_captured_span_tags_a_later_window_already_past_grace() {
    with_database(|pool| async move {
        let now = OffsetDateTime::now_utc();
        let mut a = event(now);
        a.starts_at = iso(now - Duration::hours(3));
        a.ends_at = Some(iso(now + Duration::hours(1)));
        upsert_event(&pool, GUILD_A, &iso(now), &a).await.unwrap();
        map(&pool, GUILD_A, "a", EVENT_A).await;
        let mut new = session(now, &["a"]);
        new.joined_at = now - Duration::minutes(150);
        let id = insert_attendance_session(&pool, &new).await.unwrap();
        let mut b = event(now);
        b.id = EVENT_B.to_owned();
        b.starts_at = iso(now - Duration::hours(2));
        b.ends_at = Some(iso(now - Duration::hours(1)));
        // Establish accepted live evidence before its positive completion.
        b.status = EventStatus::Active;
        upsert_event(&pool, GUILD_A, &iso(now - Duration::seconds(1)), &b)
            .await
            .unwrap();
        b.status = EventStatus::Completed;
        upsert_event(&pool, GUILD_A, &iso(now), &b).await.unwrap();
        map(&pool, GUILD_A, "b", EVENT_B).await;
        let windows = attendance_windows_for_guild(&pool, GUILD_A).await.unwrap();
        assert!(windows.iter().any(|window| window.event_key == "b"));
        close_attendance_session(&pool, id, now, Some("Lobby"), &["b".to_owned()])
            .await
            .unwrap();
        let rows = attendance_sessions_for_event_key(&pool, GUILD_A, "b")
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].joined_at.unix_timestamp(),
            new.joined_at.unix_timestamp()
        );
        assert_eq!(
            rows[0].left_at.unwrap().unix_timestamp(),
            (now - Duration::minutes(45)).unix_timestamp()
        );
        // The same expired schedule never authorizes a new span.
        assert!(insert_attendance_session(&pool, &session(now, &["b"]))
            .await
            .is_err());
    })
    .await;
}

#[tokio::test]
async fn raw_retention_store_count_and_delete_share_the_strict_cutoff() {
    with_database(|pool| async move {
        let store = two_bot_cutover::voice_rooms::PgRoomStore::new(pool.clone());
        let cutoff = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap() - Duration::days(90);
        // Raw retention applies to open and closed sessions in every guild.
        for (guild, joined, left) in [
            (GUILD_A, cutoff - Duration::seconds(1), None),
            (GUILD_B, cutoff - Duration::seconds(1), Some(cutoff)),
            (GUILD_A, cutoff, None),
            (GUILD_B, cutoff + Duration::seconds(1), None),
        ] {
            sqlx::query(
                "INSERT INTO event_voice_sessions \
                   (guild_id, member_id, channel_id, joined_at, left_at, event_keys) \
                 VALUES ($1, $2, '4001', $3, $4, ARRAY['game-night'])",
            )
            .bind(guild)
            .bind(MEMBER)
            .bind(joined)
            .bind(left)
            .execute(&pool)
            .await
            .unwrap();
        }
        assert_eq!(
            store
                .count_attendance_sessions_before(cutoff)
                .await
                .unwrap(),
            2
        );
        let total: i64 = sqlx::query_scalar("SELECT count(*) FROM event_voice_sessions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(total, 4, "count is read-only");
        assert_eq!(
            store
                .delete_attendance_sessions_before(cutoff)
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            store
                .count_attendance_sessions_before(cutoff)
                .await
                .unwrap(),
            0
        );
        let survivors: Vec<OffsetDateTime> =
            sqlx::query_scalar("SELECT joined_at FROM event_voice_sessions ORDER BY joined_at")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(survivors, [cutoff, cutoff + Duration::seconds(1)]);
        assert_eq!(
            store
                .delete_attendance_sessions_before(cutoff)
                .await
                .unwrap(),
            0
        );
    })
    .await;
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
        assert_eq!(
            attendance_windows_for_guild(&pool, GUILD_A)
                .await
                .unwrap()
                .len(),
            1,
            "an expired candidate may finalize existing spans, never open one"
        );
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
            close_attendance_session(&pool, id, now, Some("Renamed"), &["unknown".to_owned()])
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            close_attendance_session(&pool, id, now, Some("Again"), &[])
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
            close_attendance_session(&pool, id, now, Some("Lobby"), &[])
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
            close_attendance_session(&pool, id, now, Some("Lobby"), &[])
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
        assert_eq!(close_attendance_session(&pool, id, now, Some("Lobby"), &["game-night".to_owned(), "overlap".to_owned(), "unknown".to_owned()]).await.unwrap(), 1);
        assert!(attendance_window_for_key(&pool, GUILD_A, "overlap").await.unwrap().is_some());
        let rows = attendance_sessions_for_event_key(&pool, GUILD_A, "overlap").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event_keys, vec!["game-night", "overlap"]);
        assert_eq!(close_attendance_session(&pool, i64::MAX, now, Some("Lobby"), &[]).await.unwrap(), 0);
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
                async move { close_attendance_session(&pool, id, now, Some("Lobby"), &both).await }
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
                async move { close_attendance_session(&pool, id, now, Some("Lobby"), &both).await }
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
            close_attendance_session(&pool, id, now, Some("Lobby"), &requested)
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
            close_attendance_session(&pool, gap_a_id, now, Some("Lobby"), &["b".to_owned()])
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
            close_attendance_session(&pool, id, now, Some("Lobby"), &["b".to_owned()])
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

type RawRow = (
    String,
    String,
    bool,
    bool,
    OffsetDateTime,
    Option<OffsetDateTime>,
    Option<OffsetDateTime>,
    Vec<String>,
);

/// Seed one raw row with a distinctive name, tags and observation stamp.
async fn seed_raw(
    pool: &PgPool,
    guild: &str,
    n: usize,
    joined_at: OffsetDateTime,
    left_at: Option<OffsetDateTime>,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO event_voice_sessions (guild_id, member_id, channel_id, channel_name,
            name_observed_at, is_bot, afk, joined_at, left_at, event_keys)
         VALUES ($1, $2, $3, $4, $5, FALSE, TRUE, $6, $7, ARRAY[$8]) RETURNING id",
    )
    .bind(guild)
    .bind(format!("{}", 30_000_000_000_000_000_u64 + n as u64))
    .bind(format!("{}", 40_000_000_000_000_000_u64 + n as u64))
    .bind(format!("Room {n}"))
    .bind(joined_at + Duration::minutes(1))
    .bind(joined_at)
    .bind(left_at)
    .bind(format!("{}", 20_000_000_000_000_000_u64 + n as u64))
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn raw_state(pool: &PgPool, id: i64) -> (RawRow, OffsetDateTime) {
    let row: RawRow = sqlx::query_as(
        "SELECT channel_id, channel_name, is_bot, afk, joined_at, left_at, name_observed_at,
                event_keys FROM event_voice_sessions WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap();
    let changed = sqlx::query_scalar("SELECT changed_at FROM event_voice_sessions WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    (row, changed)
}

async fn row_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM event_voice_sessions")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn recovery_pages_and_disposes_more_than_one_page_of_open_rows() {
    with_database(|pool| async move {
        let base = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap() - Duration::hours(2);
        let total = ATTENDANCE_RECOVERY_PAGE_SIZE * 2 + 5;
        let mut expected = Vec::new();
        for n in 0..total {
            expected
                .push(seed_raw(&pool, GUILD_A, n, base + Duration::seconds(n as i64), None).await);
        }
        // Closed rows and other-guild open rows never appear in the page.
        seed_raw(&pool, GUILD_A, 900, base, Some(base + Duration::minutes(5))).await;
        let other = seed_raw(&pool, GUILD_B, 901, base, None).await;
        let before = row_count(&pool).await;

        let mut seen = Vec::new();
        let mut cursor = 0_i64;
        let mut pages = 0;
        loop {
            let page = attendance_recovery_page(&pool, GUILD_A, cursor)
                .await
                .unwrap();
            if page.is_empty() {
                break;
            }
            assert!(page.len() <= ATTENDANCE_RECOVERY_PAGE_SIZE);
            assert!(page.windows(2).all(|w| w[0].id < w[1].id));
            assert!(page.iter().all(|r| r.id > cursor));
            pages += 1;
            let ids: Vec<i64> = page.iter().map(|r| r.id).collect();
            cursor = *ids.last().unwrap();
            assert_eq!(
                recover_attendance_sessions(&pool, GUILD_A, &ids)
                    .await
                    .unwrap(),
                ids.len() as u64
            );
            seen.extend(page);
        }
        assert_eq!(pages, 3);
        assert_eq!(seen.iter().map(|r| r.id).collect::<Vec<_>>(), expected);
        // Minimal identity only, matching the seeded raw columns.
        let first = &seen[0];
        assert_eq!(first.member_id, "30000000000000000");
        assert_eq!(first.channel_id, "40000000000000000");
        // Everything in the guild is now disposed; the other guild is untouched.
        assert!(attendance_recovery_page(&pool, GUILD_A, 0)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(raw_state(&pool, other).await.0 .5, None);
        assert_eq!(row_count(&pool).await, before, "no manufactured joins");
    })
    .await;
}

#[tokio::test]
async fn recovery_closes_at_join_preserving_names_tags_and_observation() {
    with_database(|pool| async move {
        let joined = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap() - Duration::hours(1);
        let id = seed_raw(&pool, GUILD_A, 1, joined, None).await;
        let (before, changed_before) = raw_state(&pool, id).await;
        assert_eq!(
            recover_attendance_sessions(&pool, GUILD_A, &[id])
                .await
                .unwrap(),
            1
        );
        let (after, changed_after) = raw_state(&pool, id).await;
        assert_eq!(
            after.5,
            Some(joined),
            "left_at equals joined_at: zero credit"
        );
        assert_eq!(
            (&after.0, &after.1, after.2, after.3, after.4, after.6, &after.7),
            (&before.0, &before.1, before.2, before.3, before.4, before.6, &before.7)
        );
        assert_eq!(after.1, "Room 1");
        assert_eq!(after.7, vec!["20000000000000001"]);
        assert!(changed_after > changed_before, "changed_at must advance");
        // No capture window or tag is created by recovery.
        let windows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM event_attendance_windows")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(windows, 0);
        assert_eq!(row_count(&pool).await, 1);
    })
    .await;
}

#[tokio::test]
async fn recovery_preserves_closed_rows_refuses_cross_guild_ids_and_is_idempotent() {
    with_database(|pool| async move {
        let base = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap() - Duration::hours(3);
        let closed_left = base + Duration::minutes(30);
        let closed = seed_raw(&pool, GUILD_A, 1, base, Some(closed_left)).await;
        let open = seed_raw(&pool, GUILD_A, 2, base, None).await;
        let foreign = seed_raw(&pool, GUILD_B, 3, base, None).await;
        let closed_before = raw_state(&pool, closed).await;
        let foreign_before = raw_state(&pool, foreign).await;

        // Only the guild-local open row is affected; closed and foreign IDs are ignored.
        assert_eq!(
            recover_attendance_sessions(&pool, GUILD_A, &[closed, open, foreign])
                .await
                .unwrap(),
            1
        );
        assert_eq!(raw_state(&pool, closed).await, closed_before);
        assert_eq!(raw_state(&pool, closed).await.0 .5, Some(closed_left));
        assert_eq!(raw_state(&pool, foreign).await, foreign_before);
        assert_eq!(raw_state(&pool, foreign).await.0 .5, None);
        let opened = raw_state(&pool, open).await;
        assert_eq!(opened.0 .5, Some(base));

        // Repeating, or replaying the same page, changes nothing further.
        assert_eq!(
            recover_attendance_sessions(&pool, GUILD_A, &[closed, open, foreign])
                .await
                .unwrap(),
            0
        );
        assert_eq!(raw_state(&pool, open).await, opened);
        assert_eq!(raw_state(&pool, closed).await, closed_before);
        assert_eq!(raw_state(&pool, foreign).await, foreign_before);
        // Empty input is a no-op.
        assert_eq!(
            recover_attendance_sessions(&pool, GUILD_A, &[])
                .await
                .unwrap(),
            0
        );
        // The foreign guild can still dispose of its own row.
        assert_eq!(
            recover_attendance_sessions(&pool, GUILD_B, &[foreign])
                .await
                .unwrap(),
            1
        );
        assert_eq!(raw_state(&pool, foreign).await.0 .5, Some(base));
    })
    .await;
}

#[tokio::test]
async fn recovery_rejects_oversize_batches_before_any_mutation() {
    with_database(|pool| async move {
        let base = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap() - Duration::hours(1);
        let mut ids = Vec::new();
        for n in 0..=ATTENDANCE_RECOVERY_PAGE_SIZE {
            ids.push(seed_raw(&pool, GUILD_A, n, base, None).await);
        }
        assert_eq!(ids.len(), ATTENDANCE_RECOVERY_PAGE_SIZE + 1);
        let mut before = Vec::new();
        for id in &ids {
            before.push(raw_state(&pool, *id).await);
        }
        assert!(recover_attendance_sessions(&pool, GUILD_A, &ids)
            .await
            .is_err());
        for (id, expected) in ids.iter().zip(&before) {
            let state = raw_state(&pool, *id).await;
            assert_eq!(&state, expected);
            assert_eq!(state.0 .5, None);
        }
        // Exactly one page is accepted.
        assert_eq!(
            recover_attendance_sessions(&pool, GUILD_A, &ids[..ATTENDANCE_RECOVERY_PAGE_SIZE])
                .await
                .unwrap(),
            ATTENDANCE_RECOVERY_PAGE_SIZE as u64
        );
        assert_eq!(
            raw_state(&pool, ids[ATTENDANCE_RECOVERY_PAGE_SIZE])
                .await
                .0
                 .5,
            None
        );
    })
    .await;
}

/// Seed one raw row for an explicit member, so many open rows share an owner.
async fn seed_member_row(
    pool: &PgPool,
    guild: &str,
    member: &str,
    n: usize,
    joined_at: OffsetDateTime,
    left_at: Option<OffsetDateTime>,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO event_voice_sessions (guild_id, member_id, channel_id, channel_name,
            name_observed_at, is_bot, afk, joined_at, left_at, event_keys)
         VALUES ($1, $2, $3, $4, $5, FALSE, TRUE, $6, $7, ARRAY[$8]) RETURNING id",
    )
    .bind(guild)
    .bind(member)
    .bind(format!("{}", 40_000_000_000_000_000_u64 + n as u64))
    .bind(format!("Room {n}"))
    .bind(joined_at + Duration::minutes(1))
    .bind(joined_at)
    .bind(left_at)
    .bind(format!("{}", 20_000_000_000_000_000_u64 + n as u64))
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn open_rows_page_oldest_first_and_closing_each_page_reaches_every_row() {
    with_database(|pool| async move {
        let base = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap() - Duration::hours(4);
        let total = 32 * 2 + 5;
        // Join order is the reverse of id order, so paging must follow
        // (joined_at, id) rather than insertion order.
        let mut expected = Vec::new();
        for n in 0..total {
            let joined = base + Duration::seconds((total - n) as i64);
            expected.push((
                joined,
                seed_member_row(&pool, GUILD_A, MEMBER, n, joined, None).await,
            ));
        }
        // A tied join sorts by id.
        let tie_joined = base + Duration::seconds(1);
        let tie_a = seed_member_row(&pool, GUILD_A, MEMBER, 700, tie_joined, None).await;
        let tie_b = seed_member_row(&pool, GUILD_A, MEMBER, 701, tie_joined, None).await;
        expected.push((tie_joined, tie_a));
        expected.push((tie_joined, tie_b));
        expected.sort();
        let expected: Vec<i64> = expected.into_iter().map(|(_, id)| id).collect();

        // Other member, other guild and a closed row of this member never appear.
        let other_member =
            seed_member_row(&pool, GUILD_A, "30000000000000999", 800, base, None).await;
        let other_guild = seed_member_row(&pool, GUILD_B, MEMBER, 801, base, None).await;
        let closed_left = base + Duration::minutes(3);
        let closed = seed_member_row(&pool, GUILD_A, MEMBER, 802, base, Some(closed_left)).await;
        let untouched = [
            (other_member, raw_state(&pool, other_member).await),
            (other_guild, raw_state(&pool, other_guild).await),
            (closed, raw_state(&pool, closed).await),
        ];
        let rows_before = row_count(&pool).await;

        let mut before = std::collections::HashMap::new();
        for id in &expected {
            before.insert(*id, raw_state(&pool, *id).await);
        }
        let mut seen: Vec<i64> = Vec::new();
        let mut page_sizes = Vec::new();
        loop {
            let page = open_attendance_sessions(&pool, GUILD_A, MEMBER)
                .await
                .unwrap();
            if page.is_empty() {
                break;
            }
            assert!(page.len() <= 32, "a page never exceeds its bound");
            assert!(page
                .windows(2)
                .all(|w| (w[0].joined_at, w[0].id) < (w[1].joined_at, w[1].id)));
            assert!(
                page.iter().all(|row| !seen.contains(&row.id)),
                "no row repeats"
            );
            assert!(page.iter().all(|row| row.left_at.is_none()));
            page_sizes.push(page.len());
            for row in &page {
                assert_eq!(
                    close_attendance_session(
                        &pool,
                        row.id,
                        row.joined_at + Duration::minutes(5),
                        None,
                        &[]
                    )
                    .await
                    .unwrap(),
                    1
                );
                seen.push(row.id);
            }
            assert!(page_sizes.len() <= 3, "paging must terminate");
        }
        assert_eq!(page_sizes, vec![32, 32, 7]);
        assert_eq!(seen, expected, "no row skipped and oldest-first order held");

        // Original boundary, name, observation and tags survive the close.
        for id in &expected {
            let (old, _) = &before[id];
            let (new, changed) = raw_state(&pool, *id).await;
            assert_eq!(new.5, Some(old.4 + Duration::minutes(5)));
            assert_eq!(
                (&new.0, &new.1, new.2, new.3, new.4, new.6, &new.7),
                (&old.0, &old.1, old.2, old.3, old.4, old.6, &old.7)
            );
            assert!(changed >= before[id].1);
        }
        for (id, state) in &untouched {
            assert_eq!(&raw_state(&pool, *id).await, state);
        }
        assert_eq!(raw_state(&pool, other_member).await.0 .5, None);
        assert_eq!(raw_state(&pool, other_guild).await.0 .5, None);
        assert_eq!(row_count(&pool).await, rows_before);
        // The other open rows are now the only ones left for their owners.
        assert_eq!(
            open_attendance_sessions(&pool, GUILD_B, MEMBER)
                .await
                .unwrap()
                .len(),
            1
        );
    })
    .await;
}

#[tokio::test]
async fn malformed_stored_names_ids_and_tags_read_as_oversize_sentinels_not_truncations() {
    with_database(|pool| async move {
        let joined = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap() - Duration::hours(1);
        let name = seed_member_row(&pool, GUILD_A, MEMBER, 1, joined, None).await;
        let channel = seed_member_row(&pool, GUILD_A, MEMBER, 2, joined + Duration::seconds(1), None).await;
        let many = seed_member_row(&pool, GUILD_A, MEMBER, 3, joined + Duration::seconds(2), None).await;
        let wide = seed_member_row(&pool, GUILD_A, MEMBER, 4, joined + Duration::seconds(3), None).await;
        sqlx::query("UPDATE event_voice_sessions SET channel_name = repeat('n', 2000000) WHERE id = $1")
            .bind(name).execute(&pool).await.unwrap();
        sqlx::query("UPDATE event_voice_sessions SET channel_id = repeat('9', 400) WHERE id = $1")
            .bind(channel).execute(&pool).await.unwrap();
        sqlx::query("UPDATE event_voice_sessions
                     SET event_keys = (SELECT array_agg('k' || g) FROM generate_series(1, $2::int) g)
                     WHERE id = $1")
            .bind(many).bind(ATTENDANCE_MAX_WINDOWS as i32 + 1).execute(&pool).await.unwrap();
        sqlx::query("UPDATE event_voice_sessions
                     SET event_keys = ARRAY[repeat('w', $2::int)] WHERE id = $1")
            .bind(wide).bind(ATTENDANCE_MAX_TAG_BYTES as i32 * 4).execute(&pool).await.unwrap();

        let rows = open_attendance_sessions(&pool, GUILD_A, MEMBER).await.unwrap();
        assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![name, channel, many, wide]);
        // Oversize name: a sentinel just past the limit, never a clipped prefix.
        assert_eq!(rows[0].channel_name.len(), 4097);
        assert_eq!(rows[1].channel_id.len(), 21);
        // Too many tags: one over-budget sentinel tag replaces the array.
        assert!(rows[2].event_keys.iter().map(String::len).sum::<usize>() > ATTENDANCE_MAX_TAG_BYTES);
        // Too much tag text: refused by the same total whether or not stored compressed.
        assert!(rows[3].event_keys.iter().map(String::len).sum::<usize>() > ATTENDANCE_MAX_TAG_BYTES);
        // Well-formed neighbours are unaffected by the malformed rows.
        assert_eq!(rows[0].event_keys, vec!["20000000000000001"]);

        // Expansion still refuses stored oversize tags and leaves the row open.
        for id in [many, wide] {
            let before = raw_state(&pool, id).await;
            assert!(close_attendance_session(&pool, id, joined + Duration::hours(1), Some("Lobby"), &["new-tag".into()])
                .await
                .is_err());
            assert_eq!(raw_state(&pool, id).await, before);
            assert_eq!(before.0 .5, None);
        }
        assert_eq!(open_attendance_sessions(&pool, GUILD_A, MEMBER).await.unwrap().len(), 4);
        // Empty expansion finalizes by id without decoding/rewriting oversized
        // metadata. It must drain, not permanently park the actor's inbox.
        for id in [name, channel, many, wide] {
            let (before, _) = raw_state(&pool, id).await;
            let left = if id == channel { before.4 } else { joined + Duration::hours(1) };
            assert_eq!(close_attendance_session(&pool, id, left, None, &[]).await.unwrap(), 1);
            let (after, changed) = raw_state(&pool, id).await;
            assert_eq!(after.5, Some(left));
            assert_eq!(
                (&after.0, &after.1, after.2, after.3, after.4, after.6, &after.7),
                (&before.0, &before.1, before.2, before.3, before.4, before.6, &before.7)
            );
            assert_eq!(close_attendance_session(&pool, id, left, None, &[]).await.unwrap(), 0);
            assert_eq!(raw_state(&pool, id).await, (after, changed));
        }
        assert!(open_attendance_sessions(&pool, GUILD_A, MEMBER).await.unwrap().is_empty());
        assert_eq!(count(&pool, "event_attendance_windows").await, 0);
    })
    .await;
}

#[tokio::test]
async fn oversize_insert_and_close_inputs_are_rejected_without_mutation() {
    with_database(|pool| async move {
        let now = OffsetDateTime::now_utc();
        upsert_event(&pool, GUILD_A, &iso(now), &event(now))
            .await
            .unwrap();
        map(&pool, GUILD_A, "game-night", EVENT_A).await;
        let valid = session(now, &["game-night"]);

        let mut too_many = valid.clone();
        too_many.event_keys = (0..=ATTENDANCE_MAX_WINDOWS)
            .map(|n| format!("k{n}"))
            .collect();
        let mut long_key = valid.clone();
        long_key.event_keys = vec!["k".repeat(129)];
        let mut long_name = valid.clone();
        long_name.channel_name = "n".repeat(4097);
        for bad in [too_many, long_key, long_name] {
            assert!(insert_attendance_session(&pool, &bad).await.is_err());
            assert_eq!(row_count(&pool).await, 0);
            assert_eq!(count(&pool, "event_attendance_windows").await, 0);
        }

        // The same inputs at the bound are accepted.
        let mut at_bound = valid.clone();
        at_bound.channel_name = "n".repeat(4096);
        let id = insert_attendance_session(&pool, &at_bound).await.unwrap();
        assert_eq!(row_count(&pool).await, 1);
        let windows = count(&pool, "event_attendance_windows").await;

        // Oversize close input leaves the open row and history untouched.
        let before = raw_state(&pool, id).await;
        let many: Vec<String> = (0..=2 * ATTENDANCE_MAX_WINDOWS)
            .map(|n| format!("k{n}"))
            .collect();
        let wide: Vec<String> = (0..3)
            .map(|n| format!("{n}{}", "w".repeat(ATTENDANCE_MAX_TAG_BYTES * 2 / 3)))
            .collect();
        let long_name = "n".repeat(4097);
        let left = now + Duration::minutes(1);
        assert!(close_attendance_session(&pool, id, left, None, &many)
            .await
            .is_err());
        assert!(close_attendance_session(&pool, id, left, None, &wide)
            .await
            .is_err());
        assert!(
            close_attendance_session(&pool, id, left, Some(long_name.as_str()), &[])
                .await
                .is_err()
        );
        assert_eq!(raw_state(&pool, id).await, before);
        assert_eq!(before.0 .5, None);
        assert_eq!(count(&pool, "event_attendance_windows").await, windows);
        assert_eq!(row_count(&pool).await, 1);
    })
    .await;
}

/// An accepted, mapped, completed occurrence: live evidence first, then the
/// positive completion, exactly as the lifecycle writers observe it.
async fn completed_mapped(
    pool: &PgPool,
    now: OffsetDateTime,
    id: &str,
    key: &str,
    starts_at: OffsetDateTime,
    ends_at: OffsetDateTime,
) {
    let mut occurrence = event(now);
    occurrence.id = id.to_owned();
    occurrence.starts_at = iso(starts_at);
    occurrence.ends_at = Some(iso(ends_at));
    occurrence.status = EventStatus::Active;
    upsert_event(pool, GUILD_A, &iso(now - Duration::seconds(1)), &occurrence)
        .await
        .unwrap();
    occurrence.status = EventStatus::Completed;
    upsert_event(pool, GUILD_A, &iso(now), &occurrence)
        .await
        .unwrap();
    map(pool, GUILD_A, key, id).await;
}

fn keys(windows: &[two_bot_cutover::event_attendance::AttendanceWindow]) -> Vec<&str> {
    windows.iter().map(|w| w.event_key.as_str()).collect()
}

#[tokio::test]
async fn transition_planning_filters_unrelated_history_before_the_overlap_limit() {
    with_database(|pool| async move {
        let now = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap();
        // Exactly the planner budget of unrelated retained occurrences, all
        // accepted, mapped and completed days ago. They sort before the live one.
        for n in 0..ATTENDANCE_MAX_WINDOWS {
            let id = format!("{}", 30_000_000_000_000_000_u64 + n as u64);
            let start = now - Duration::days(10) + Duration::minutes(n as i64 * 2);
            completed_mapped(
                &pool,
                now,
                &id,
                &format!("old-{n}"),
                start,
                start + Duration::hours(1),
            )
            .await;
        }
        upsert_event(
            &pool,
            GUILD_A,
            &iso(now - Duration::seconds(1)),
            &event(now),
        )
        .await
        .unwrap();
        map(&pool, GUILD_A, "current", EVENT_A).await;

        // The broad compatibility reader still sees every candidate and hits
        // the 129-row overlap limit, so a caller using it must refuse.
        let broad = attendance_windows_for_guild(&pool, GUILD_A).await.unwrap();
        assert_eq!(broad.len(), ATTENDANCE_MAX_WINDOWS + 1);
        assert!(keys(&broad).contains(&"current"));

        // The scoped planner reads exactly the live occurrence, via both entry points.
        let scoped = attendance_windows_for_transition(&pool, GUILD_A, MEMBER, None, now)
            .await
            .unwrap();
        assert_eq!(keys(&scoped), ["current"]);
        let store = two_bot_cutover::voice_rooms::PgRoomStore::new(pool.clone());
        let via_store = store
            .attendance_windows_for_transition(GUILD_A, MEMBER, Some("4001"), now)
            .await
            .unwrap();
        assert_eq!(keys(&via_store), ["current"]);

        // The real insert is unaffected by the retained history and tags only the live key.
        let mut join = session(now, &["current"]);
        join.joined_at = now - Duration::minutes(5);
        let id = insert_attendance_session(&pool, &join).await.unwrap();
        let tags: Vec<String> =
            sqlx::query_scalar("SELECT event_keys FROM event_voice_sessions WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(tags, ["current"]);
        // An open continuous span in the previous channel adds no unrelated history.
        let continuing =
            attendance_windows_for_transition(&pool, GUILD_A, MEMBER, Some("4001"), now)
                .await
                .unwrap();
        assert_eq!(keys(&continuing), ["current"]);
    })
    .await;
}

#[tokio::test]
async fn transition_planning_keeps_expired_windows_overlapping_the_open_span_only() {
    with_database(|pool| async move {
        let now = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap();
        let mut a = event(now);
        a.starts_at = iso(now - Duration::hours(3));
        a.ends_at = Some(iso(now + Duration::hours(1)));
        upsert_event(&pool, GUILD_A, &iso(now - Duration::seconds(1)), &a)
            .await
            .unwrap();
        map(&pool, GUILD_A, "a", EVENT_A).await;
        let mut span = session(now, &["a"]);
        span.joined_at = now - Duration::minutes(150);
        let id = insert_attendance_session(&pool, &span).await.unwrap();
        // Expired an hour ago but inside the continuous span.
        completed_mapped(
            &pool,
            now,
            EVENT_B,
            "b",
            now - Duration::hours(2),
            now - Duration::hours(1),
        )
        .await;
        // Expired days ago: overlaps neither the observation nor the span.
        completed_mapped(
            &pool,
            now,
            "20000000000000003",
            "c",
            now - Duration::days(10),
            now - Duration::days(10) + Duration::hours(1),
        )
        .await;

        let plan = |member: &'static str, previous: Option<&'static str>| {
            let pool = pool.clone();
            async move {
                attendance_windows_for_transition(&pool, GUILD_A, member, previous, now)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|w| w.event_key)
                    .collect::<Vec<_>>()
            }
        };
        assert_eq!(plan(MEMBER, Some("4001")).await, ["a", "b"]);
        // Wrong previous channel, no previous channel, or another member's span
        // never reach back to the expired window.
        assert_eq!(plan(MEMBER, Some("9999")).await, ["a"]);
        assert_eq!(plan(MEMBER, None).await, ["a"]);
        assert_eq!(plan("30000000000000009", Some("4001")).await, ["a"]);

        // The expired candidate tags the existing span at close, never opens one.
        assert!(insert_attendance_session(&pool, &session(now, &["b"]))
            .await
            .is_err());
        close_attendance_session(&pool, id, now, None, &["b".to_owned()])
            .await
            .unwrap();
        let tags: Vec<String> =
            sqlx::query_scalar("SELECT event_keys FROM event_voice_sessions WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(tags, ["a", "b"]);
        // A closed span no longer extends the lookback.
        assert_eq!(plan(MEMBER, Some("4001")).await, ["a"]);
    })
    .await;
}

#[tokio::test]
async fn null_tag_elements_read_as_oversize_and_drain_by_id_preserving_metadata() {
    with_database(|pool| async move {
        let joined = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap() - Duration::hours(1);
        let poisoned = seed_member_row(&pool, GUILD_A, MEMBER, 1, joined, None).await;
        let healthy = seed_member_row(
            &pool,
            GUILD_A,
            MEMBER,
            2,
            joined + Duration::seconds(1),
            None,
        )
        .await;
        sqlx::query(
            "UPDATE event_voice_sessions SET event_keys = ARRAY['20000000000000001', NULL]::text[]
             WHERE id = $1",
        )
        .bind(poisoned)
        .execute(&pool)
        .await
        .unwrap();
        let rows_before = row_count(&pool).await;
        let healthy_before = raw_state(&pool, healthy).await;

        // The poisoned array is replaced by the oversize sentinel; the neighbour is exact.
        let rows = open_attendance_sessions(&pool, GUILD_A, MEMBER)
            .await
            .unwrap();
        assert_eq!(
            rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            [poisoned, healthy]
        );
        assert!(
            rows[0].event_keys.iter().map(String::len).sum::<usize>() > ATTENDANCE_MAX_TAG_BYTES
        );
        assert_eq!(rows[1].event_keys, ["20000000000000002"]);

        // Expansion decodes the stored tags, so it refuses and leaves the row untouched.
        assert!(close_attendance_session(
            &pool,
            poisoned,
            joined + Duration::minutes(30),
            Some("Lobby"),
            &["new-tag".to_owned()],
        )
        .await
        .is_err());
        let intact = |id: i64| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, bool>(
                    "SELECT left_at IS NULL AND event_keys IS NOT DISTINCT FROM
                            ARRAY['20000000000000001', NULL]::text[]
                     FROM event_voice_sessions WHERE id = $1",
                )
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        assert!(intact(poisoned).await, "failed expansion must not mutate");

        // Draining by id (no expansion) closes both rows and keeps server metadata.
        let left = joined + Duration::minutes(30);
        for id in [poisoned, healthy] {
            assert_eq!(
                close_attendance_session(&pool, id, left, None, &[])
                    .await
                    .unwrap(),
                1
            );
        }
        let drained: bool = sqlx::query_scalar(
            "SELECT left_at = $2 AND event_keys IS NOT DISTINCT FROM
                    ARRAY['20000000000000001', NULL]::text[]
             FROM event_voice_sessions WHERE id = $1",
        )
        .bind(poisoned)
        .bind(left)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            drained,
            "exact metadata tags, including NULL, survive the drain"
        );
        let (after, _) = raw_state(&pool, healthy).await;
        assert_eq!(after.5, Some(left));
        assert_eq!(
            (&after.0, &after.1, after.2, after.3, after.4, after.6, &after.7),
            (
                &healthy_before.0 .0,
                &healthy_before.0 .1,
                healthy_before.0 .2,
                healthy_before.0 .3,
                healthy_before.0 .4,
                healthy_before.0 .6,
                &healthy_before.0 .7
            )
        );
        // Nothing replaced either row and no window was opened or recorded.
        assert!(open_attendance_sessions(&pool, GUILD_A, MEMBER)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(row_count(&pool).await, rows_before);
        assert_eq!(count(&pool, "event_attendance_windows").await, 0);
    })
    .await;
}

/// Replace the window's stops with `total` timestamps: all but two precede
/// the join, a later one follows it, and the first after the join is 2 minutes in.
async fn set_capture_stops(pool: &PgPool, key: &str, total: i32) {
    let updated = sqlx::query(
        "UPDATE event_attendance_windows h
         SET capture_stops = ARRAY(
                 SELECT j.joined_at - INTERVAL '1 hour' - n * INTERVAL '1 second'
                 FROM generate_series(1, $3::int - 2) n)
             || ARRAY[j.joined_at + INTERVAL '5 minutes', j.joined_at + INTERVAL '2 minutes']
         FROM (SELECT joined_at FROM event_voice_sessions
               WHERE guild_id = $1 AND event_keys @> ARRAY[$2::text] LIMIT 1) j
         WHERE h.guild_id = $1 AND h.event_key = $2",
    )
    .bind(GUILD_A)
    .bind(key)
    .bind(total)
    .execute(pool)
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(updated, 1);
}

#[tokio::test]
async fn capture_stops_over_the_bound_refuse_in_both_readers_and_the_bound_is_accepted() {
    with_database(|pool| async move {
        let now = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap();
        upsert_event(
            &pool,
            GUILD_A,
            &iso(now - Duration::seconds(1)),
            &event(now),
        )
        .await
        .unwrap();
        map(&pool, GUILD_A, "game-night", EVENT_A).await;
        let joined = session(now, &["game-night"]).joined_at;
        insert_attendance_session(&pool, &session(now, &["game-night"]))
            .await
            .unwrap();
        let store = InternalActionStore::new(pool.clone());
        let bound = MAX_ATTENDANCE_CAPTURE_STOPS as i32;

        // Exactly at the bound the read succeeds; the first stop after the join caps credit.
        set_capture_stops(&pool, "game-night", bound).await;
        let stored: i32 =
            sqlx::query_scalar("SELECT cardinality(capture_stops) FROM event_attendance_windows")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored, bound);
        let rows = attendance_sessions_for_event_key(&pool, GUILD_A, "game-night")
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].left_at, Some(joined + Duration::minutes(2)));
        let rows = store
            .attendance_sessions_for_event_key(GUILD_A, "game-night")
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].left_at, Some(joined + Duration::minutes(2)));

        // One over, and far over, are refused by both readers without a partial result.
        for total in [bound + 1, 100_000] {
            set_capture_stops(&pool, "game-night", total).await;
            assert!(
                matches!(
                    attendance_sessions_for_event_key(&pool, GUILD_A, "game-night").await,
                    Err(sqlx::Error::Protocol(_))
                ),
                "{total}"
            );
            assert_eq!(
                store
                    .attendance_sessions_for_event_key(GUILD_A, "game-night")
                    .await
                    .unwrap_err(),
                InternalStoreError::Unavailable,
                "{total}"
            );
        }
    })
    .await;
}

/// Stored tag arrays SQLx cannot decode as a plain `text[]`, plus controls.
/// Each expression is a literal, so the read and close paths see the exact shape.
const UNSUPPORTED_TAG_SHAPES: [(&str, &str); 6] = [
    ("two_dimensional", "'{{a,b},{c,d}}'::text[]"),
    ("lower_bound_zero", "'[0:1]={a,b}'::text[]"),
    ("lower_bound_two", "'[2:3]={a,b}'::text[]"),
    ("null_element", "ARRAY['a', NULL]::text[]"),
    (
        "too_many_elements",
        "(SELECT array_agg('k' || g) FROM generate_series(1, 129) g)",
    ),
    ("too_many_bytes", "ARRAY[repeat('w', 16385)]"),
];

type RawShapeRow = (
    String,
    String,
    bool,
    bool,
    OffsetDateTime,
    Option<OffsetDateTime>,
    Option<OffsetDateTime>,
    Option<String>,
);

// Compare server-rendered arrays without asking SQLx to decode malformed shapes.
async fn raw_shape_state(pool: &PgPool, id: i64) -> (RawShapeRow, OffsetDateTime) {
    let row = sqlx::query_as(
        "SELECT channel_id, channel_name, is_bot, afk, joined_at, left_at, name_observed_at,
                event_keys::text FROM event_voice_sessions WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap();
    let changed = sqlx::query_scalar("SELECT changed_at FROM event_voice_sessions WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    (row, changed)
}

/// Exact stored shape: text form (carries bounds) plus dimensions and lower bound.
async fn tag_shape(pool: &PgPool, id: i64) -> (String, Option<i32>, Option<i32>) {
    sqlx::query_as(
        "SELECT event_keys::text, array_ndims(event_keys), array_lower(event_keys, 1)
         FROM event_voice_sessions WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn set_tags(pool: &PgPool, id: i64, expr: &str) {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE event_voice_sessions SET event_keys = {expr} WHERE id = $1"
    )))
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn unsupported_tag_array_shapes_read_as_sentinels_and_drain_by_id_preserving_tags() {
    use self::raw_shape_state as raw_state;

    with_database(|pool| async move {
        let joined = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap() - Duration::hours(2);
        let left = joined + Duration::hours(1);
        let mut bad = Vec::new();
        for (n, (name, expr)) in UNSUPPORTED_TAG_SHAPES.iter().enumerate() {
            let id = seed_member_row(
                &pool,
                GUILD_A,
                MEMBER,
                n + 1,
                joined + Duration::seconds(n as i64),
                None,
            )
            .await;
            set_tags(&pool, id, expr).await;
            bad.push((*name, id));
        }
        let controls = UNSUPPORTED_TAG_SHAPES.len() + 1;
        let empty = seed_member_row(
            &pool,
            GUILD_A,
            MEMBER,
            controls,
            joined + Duration::seconds(20),
            None,
        )
        .await;
        set_tags(&pool, empty, "'{}'::text[]").await;
        let healthy = seed_member_row(
            &pool,
            GUILD_A,
            MEMBER,
            controls + 1,
            joined + Duration::seconds(21),
            None,
        )
        .await;
        // Bounds-1 one-dimensional arrays at the element cap stay healthy.
        let cap = seed_member_row(
            &pool,
            GUILD_A,
            MEMBER,
            controls + 2,
            joined + Duration::seconds(22),
            None,
        )
        .await;
        set_tags(
            &pool,
            cap,
            "(SELECT array_agg('k' || g) FROM generate_series(1, 128) g)",
        )
        .await;

        // 32-row page: 6 bad + 3 controls fit in one page.
        let page = open_attendance_sessions(&pool, GUILD_A, MEMBER)
            .await
            .unwrap();
        assert_eq!(page.len(), bad.len() + 3);
        for (row, (name, id)) in page.iter().zip(&bad) {
            assert_eq!(row.id, *id, "{name}");
            assert!(
                row.event_keys.iter().map(String::len).sum::<usize>() > ATTENDANCE_MAX_TAG_BYTES,
                "{name} must read as the oversize sentinel"
            );
        }
        let controls_page = &page[bad.len()..];
        assert_eq!(controls_page[0].id, empty);
        assert!(controls_page[0].event_keys.is_empty());
        assert_eq!(controls_page[1].id, healthy);
        assert_eq!(
            controls_page[1].event_keys,
            [format!(
                "{}",
                20_000_000_000_000_000_u64 + controls as u64 + 1
            )]
        );
        assert_eq!(controls_page[2].id, cap);
        assert_eq!(controls_page[2].event_keys.len(), 128);

        // Close-time expansion refuses each unsupported shape and mutates nothing.
        let mut before = Vec::new();
        for (name, id) in &bad {
            let raw = raw_state(&pool, *id).await;
            let shape = tag_shape(&pool, *id).await;
            let refused =
                close_attendance_session(&pool, *id, left, Some("Lobby"), &["new-tag".into()])
                    .await;
            assert!(refused.is_err(), "{name} must refuse expansion");
            assert_eq!(raw_state(&pool, *id).await, raw, "{name}");
            assert_eq!(tag_shape(&pool, *id).await, shape, "{name}");
            before.push((raw, shape));
        }

        // Healthy and empty rows still expand/close normally.
        assert_eq!(
            close_attendance_session(&pool, empty, left, None, &["new-tag".into()])
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            close_attendance_session(&pool, cap, left, None, &[])
                .await
                .unwrap(),
            1
        );

        // By-id no-expansion close preserves exact tags, shape, bounds, name and observation.
        for ((name, id), (raw, shape)) in bad.iter().zip(&before) {
            assert_eq!(
                close_attendance_session(&pool, *id, left, None, &[])
                    .await
                    .unwrap(),
                1,
                "{name}"
            );
            let (after, changed) = raw_state(&pool, *id).await;
            assert_eq!(after.5, Some(left), "{name}");
            assert_eq!(
                (&after.0, &after.1, after.2, after.3, after.4, after.6, &after.7),
                (&raw.0 .0, &raw.0 .1, raw.0 .2, raw.0 .3, raw.0 .4, raw.0 .6, &raw.0 .7),
                "{name}"
            );
            assert_eq!(&tag_shape(&pool, *id).await, shape, "{name}");
            // Idempotent retry: the second close changes nothing.
            assert_eq!(
                close_attendance_session(&pool, *id, left, None, &[])
                    .await
                    .unwrap(),
                0,
                "{name}"
            );
            assert_eq!(raw_state(&pool, *id).await, (after, changed), "{name}");
        }

        // The healthy neighbor was never touched by the malformed rows.
        assert_eq!(
            close_attendance_session(&pool, healthy, left, None, &[])
                .await
                .unwrap(),
            1
        );
        assert_eq!(tag_shape(&pool, healthy).await.1, Some(1));
        assert!(open_attendance_sessions(&pool, GUILD_A, MEMBER)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(row_count(&pool).await, bad.len() as i64 + 3);
        assert_eq!(count(&pool, "event_attendance_windows").await, 0);
    })
    .await;
}

#[tokio::test]
async fn tag_shape_changed_between_open_read_and_close_refuses_then_drains_by_id() {
    use self::raw_shape_state as raw_state;

    with_database(|pool| async move {
        let joined = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap() - Duration::hours(2);
        let left = joined + Duration::hours(1);
        let racing = seed_member_row(&pool, GUILD_A, MEMBER, 1, joined, None).await;
        let neighbor = seed_member_row(
            &pool,
            GUILD_A,
            MEMBER,
            2,
            joined + Duration::seconds(1),
            None,
        )
        .await;

        // The first site reads a healthy page and plans an expansion.
        let page = open_attendance_sessions(&pool, GUILD_A, MEMBER)
            .await
            .unwrap();
        assert_eq!(
            page.iter().map(|r| r.id).collect::<Vec<_>>(),
            [racing, neighbor]
        );
        assert_eq!(page[0].event_keys, ["20000000000000001"]);

        // Keep the shape-changing write uncommitted while another connection
        // starts the close. Its second sanitizer must read the new shape after
        // the lock releases instead of trusting the earlier healthy page.
        let mut changing = pool.begin().await.unwrap();
        sqlx::query(
            "UPDATE event_voice_sessions SET event_keys = '[0:1]={a,b}'::text[] WHERE id = $1",
        )
        .bind(racing)
        .execute(&mut *changing)
        .await
        .unwrap();
        let closing_pool = pool.clone();
        let mut closing = tokio::spawn(async move {
            close_attendance_session(
                &closing_pool,
                racing,
                left,
                Some("Lobby"),
                &["new-tag".into()],
            )
            .await
        });
        assert!(
            tokio::time::timeout(StdDuration::from_millis(50), &mut closing)
                .await
                .is_err()
        );
        changing.commit().await.unwrap();
        let raw = raw_state(&pool, racing).await;
        let shape = tag_shape(&pool, racing).await;
        assert_eq!(shape.2, Some(0));

        let refused = closing.await.unwrap().unwrap_err();
        assert!(is_unsupported_stored_tags(&refused), "{refused:?}");
        assert_eq!(raw_state(&pool, racing).await, raw);
        assert_eq!(tag_shape(&pool, racing).await, shape);

        // The fallback closes by id and preserves the new shape; retry is a no-op.
        assert_eq!(
            close_attendance_session(&pool, racing, left, None, &[])
                .await
                .unwrap(),
            1
        );
        assert_eq!(tag_shape(&pool, racing).await, shape);
        let (closed, changed) = raw_state(&pool, racing).await;
        assert_eq!(closed.5, Some(left));
        assert_eq!(
            close_attendance_session(&pool, racing, left, None, &[])
                .await
                .unwrap(),
            0
        );
        assert_eq!(raw_state(&pool, racing).await, (closed, changed));

        // The healthy neighbor still reads exactly and still closes with expansion.
        let rest = open_attendance_sessions(&pool, GUILD_A, MEMBER)
            .await
            .unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].id, neighbor);
        assert_eq!(rest[0].event_keys, ["20000000000000002"]);
        assert_eq!(
            close_attendance_session(&pool, neighbor, left, None, &["20000000000000002".into()])
                .await
                .unwrap(),
            1
        );
        assert_eq!(count(&pool, "event_attendance_windows").await, 0);
    })
    .await;
}

#[tokio::test]
async fn null_stored_array_uses_bounded_sentinel_then_preserving_by_id_close() {
    with_database(|pool| async move {
        // Historical/imported NULL defense, confined to this disposable schema.
        sqlx::query("ALTER TABLE event_voice_sessions ALTER COLUMN event_keys DROP NOT NULL")
            .execute(&pool)
            .await
            .unwrap();
        let joined = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap() - Duration::hours(2);
        let id = seed_member_row(&pool, GUILD_A, MEMBER, 1, joined, None).await;
        set_tags(&pool, id, "NULL::text[]").await;
        let before = raw_shape_state(&pool, id).await;
        assert!(before.0 .7.is_none());
        let page = open_attendance_sessions(&pool, GUILD_A, MEMBER)
            .await
            .unwrap();
        assert_eq!(page.len(), 1);
        assert!(page[0].event_keys[0].len() > ATTENDANCE_MAX_TAG_BYTES);
        let refused = close_attendance_session(
            &pool,
            id,
            joined + Duration::hours(1),
            Some("replacement"),
            &["new".into()],
        )
        .await
        .unwrap_err();
        assert!(is_unsupported_stored_tags(&refused));
        assert_eq!(raw_shape_state(&pool, id).await, before);
        assert_eq!(
            close_attendance_session(&pool, id, joined + Duration::hours(1), None, &[])
                .await
                .unwrap(),
            1
        );
        let after = raw_shape_state(&pool, id).await;
        assert!(after.0 .7.is_none());
        assert_eq!(after.0 .1, before.0 .1);
        assert_eq!(after.0 .6, before.0 .6);
        assert_eq!(after.0 .5, Some(joined + Duration::hours(1)));
        assert!(open_attendance_sessions(&pool, GUILD_A, MEMBER)
            .await
            .unwrap()
            .is_empty());
    })
    .await;
}

#[tokio::test]
async fn member_recovery_page_is_identity_only_bounded_and_isolated_by_guild_and_member() {
    with_database(|pool| async move {
        let base = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap() - Duration::hours(2);
        let other = "30000000000000099";
        let total = ATTENDANCE_RECOVERY_PAGE_SIZE + 5;
        let mut mine = Vec::new();
        for n in 0..total {
            mine.push(
                seed_member_row(
                    &pool,
                    GUILD_A,
                    MEMBER,
                    n + 1,
                    base + Duration::seconds(n as i64),
                    None,
                )
                .await,
            );
        }
        // Closed row, other member, and other guild must never appear.
        seed_member_row(
            &pool,
            GUILD_A,
            MEMBER,
            100,
            base,
            Some(base + Duration::minutes(1)),
        )
        .await;
        let other_member = seed_member_row(&pool, GUILD_A, other, 101, base, None).await;
        let other_guild = seed_member_row(&pool, GUILD_B, MEMBER, 102, base, None).await;
        // A poisoned shape must not affect the identity-only page.
        set_tags(&pool, mine[0], "'{{a,b},{c,d}}'::text[]").await;

        let first = attendance_member_recovery_page(&pool, GUILD_A, MEMBER, 0)
            .await
            .unwrap();
        assert_eq!(first.len(), ATTENDANCE_RECOVERY_PAGE_SIZE);
        assert_eq!(
            first.iter().map(|r| r.id).collect::<Vec<_>>(),
            mine[..ATTENDANCE_RECOVERY_PAGE_SIZE]
        );
        assert!(first.iter().all(|r| r.member_id == MEMBER));
        let last = first.last().unwrap().id;
        let second = attendance_member_recovery_page(&pool, GUILD_A, MEMBER, last)
            .await
            .unwrap();
        assert_eq!(
            second.iter().map(|r| r.id).collect::<Vec<_>>(),
            mine[ATTENDANCE_RECOVERY_PAGE_SIZE..]
        );
        assert!(
            attendance_member_recovery_page(&pool, GUILD_A, MEMBER, *mine.last().unwrap())
                .await
                .unwrap()
                .is_empty()
        );

        let theirs = attendance_member_recovery_page(&pool, GUILD_A, other, 0)
            .await
            .unwrap();
        assert_eq!(
            theirs.iter().map(|r| r.id).collect::<Vec<_>>(),
            [other_member]
        );
        let foreign = attendance_member_recovery_page(&pool, GUILD_B, MEMBER, 0)
            .await
            .unwrap();
        assert_eq!(
            foreign.iter().map(|r| r.id).collect::<Vec<_>>(),
            [other_guild]
        );
        assert!(attendance_member_recovery_page(&pool, GUILD_B, other, 0)
            .await
            .unwrap()
            .is_empty());

        // The guild-wide page is unchanged and still sees every open row.
        let all = attendance_recovery_page(&pool, GUILD_A, 0).await.unwrap();
        assert_eq!(all.len(), ATTENDANCE_RECOVERY_PAGE_SIZE);
    })
    .await;
}
