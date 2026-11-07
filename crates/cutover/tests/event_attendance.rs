//! Attendance capture consumes accepted lifecycle evidence, never key registration
//! or its own historical windows. Runs against the disposable CI test database.

#[path = "support/raw_expiry.rs"]
mod support;

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

fn iso(at: OffsetDateTime) -> String {
    two_bot_core::funnel::format_iso_millis((at.unix_timestamp_nanos() / 1_000_000) as i64)
}

fn event(now: OffsetDateTime) -> ScheduledEvent {
    ScheduledEvent {
        id: "2001".to_owned(),
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
        map(&pool, GUILD_A, "game-night", "2001").await;
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
        map(&pool, GUILD_A, "game-night", "2001").await;
        let mut other = observed.clone();
        other.id = "2002".to_owned();
        upsert_event(&pool, GUILD_A, &iso(now - Duration::minutes(2)), &other)
            .await
            .unwrap();
        map(&pool, GUILD_A, "overlap", "2002").await;
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
        map(&pool, GUILD_A, "game-night", "2001").await;
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
        map(&pool, GUILD_A, "game-night", "2001").await;
        insert_attendance_session(&pool, &session(now, &["game-night"]))
            .await
            .unwrap();
        let mut other = observed.clone();
        other.id = "2002".to_owned();
        upsert_event(&pool, GUILD_A, &iso(now - Duration::minutes(1)), &other)
            .await
            .unwrap();
        map(&pool, GUILD_A, "game-night", "2002").await;
        assert!(attendance_windows_for_guild(&pool, GUILD_A)
            .await
            .unwrap()
            .is_empty());
        assert!(
            insert_attendance_session(&pool, &session(now, &["game-night"]))
                .await
                .is_err()
        );
        map(&pool, GUILD_A, "new-key", "2002").await;
        assert_eq!(
            attendance_windows_for_guild(&pool, GUILD_A).await.unwrap()[0].event_key,
            "new-key"
        );
        let id = insert_attendance_session(&pool, &session(now, &["new-key"]))
            .await
            .unwrap();
        two_bot_core::event_instance_store::confirm_deleted(&pool, GUILD_A, "2002", &iso(now))
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
        map(&pool, GUILD_A, "game-night", "2001").await;
        sqlx::query("ALTER TABLE event_voice_sessions ADD CONSTRAINT reject_test_capture CHECK (channel_id <> '4001')")
            .execute(&pool).await.unwrap();
        assert!(insert_attendance_session(&pool, &session(now, &["game-night"])).await.is_err());
        assert!(attendance_window_for_key(&pool, GUILD_A, "game-night").await.unwrap().is_none());
        sqlx::query("ALTER TABLE event_voice_sessions DROP CONSTRAINT reject_test_capture").execute(&pool).await.unwrap();
        let id = insert_attendance_session(&pool, &session(now, &["game-night"])).await.unwrap();
        let mut overlap = observed.clone();
        overlap.id = "2002".to_owned();
        upsert_event(&pool, GUILD_A, &iso(now - Duration::minutes(1)), &overlap).await.unwrap();
        map(&pool, GUILD_A, "overlap", "2002").await;
        assert_eq!(close_attendance_session(&pool, id, now, "Lobby", &["game-night".to_owned(), "overlap".to_owned(), "unknown".to_owned()]).await.unwrap(), 1);
        assert!(attendance_window_for_key(&pool, GUILD_A, "overlap").await.unwrap().is_some());
        let rows = attendance_sessions_for_event_key(&pool, GUILD_A, "overlap").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event_keys, vec!["game-night", "overlap"]);
        assert_eq!(close_attendance_session(&pool, i64::MAX, now, "Lobby", &[]).await.unwrap(), 0);
    }).await;
}
