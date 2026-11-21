//! Finalized attendance on the disposable cutover database only.

use two_bot_core::internal_action_store::InternalStoreError;
use two_bot_cutover::event_attendance::finalized_attendance;
use two_bot_testsupport::TestDatabase;

#[tokio::test]
async fn only_finalized_mapped_observations_are_returned() {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test database required");
    let db = TestDatabase::create(&url, &sqlx::migrate!("./migrations"))
        .await
        .unwrap();
    let pool = db.pool();
    let guild = 100000000000000001;
    let read = || finalized_attendance(pool, guild, "read-fixture");
    for (guild_id, key) in [(1, "read-fixture"), (guild, "read/fixture")] {
        assert!(matches!(
            finalized_attendance(pool, guild_id, key).await,
            Err(InternalStoreError::InvalidInput)
        ));
    }
    sqlx::raw_sql("INSERT INTO event_instance_windows
          (guild_id,event_id,instance_starts_at,starts_at,ends_at,status,observed_at)
          VALUES ('100000000000000001','200000000000000001','2026-01-01 00:00Z','2026-01-01 00:00Z',
                  '2026-01-01 01:00Z','active','2026-01-01T01:00:00Z');
        INSERT INTO event_attendance_windows
          (guild_id,event_key,event_id,instance_starts_at,starts_at,ends_at,capture_stops)
          VALUES ('100000000000000001','read-fixture','200000000000000001',
                  '2026-01-01 00:00Z','2026-01-01 00:00Z','2026-01-01 01:00Z',ARRAY['2026-01-01 00:20Z'::timestamptz]);
        INSERT INTO event_voice_sessions (guild_id,member_id,channel_id,joined_at,event_keys)
          VALUES ('100000000000000001','300000000000000001','400000000000000001','2026-01-01 00:00Z',ARRAY['read-fixture/200000000000000001/1767225600000000']),
                 ('100000000000000002','300000000000000002','400000000000000001','2026-01-01 00:00Z',ARRAY['read-fixture/200000000000000001/1767225600000000']),
                 ('100000000000000001','300000000000000003','400000000000000001','2026-01-01 00:00Z',ARRAY['read-fixture', 'other/200000000000000001/1767225600000000']);")
        .execute(pool).await.unwrap();
    assert!(read().await.unwrap().is_none(), "unmapped historical data");
    sqlx::query(
        "INSERT INTO internal_event_keys (guild_id,event_key,event_id)
        VALUES ('100000000000000001','read-fixture','200000000000000001')",
    )
    .execute(pool)
    .await
    .unwrap();
    assert!(
        read().await.unwrap().is_none(),
        "active even after scheduled end"
    );
    sqlx::query("UPDATE event_instance_windows SET status = 'completed', ends_at = NOW()")
        .execute(pool)
        .await
        .unwrap();
    assert!(read().await.unwrap().is_none(), "still inside grace");
    sqlx::query("UPDATE event_instance_windows SET ends_at = '2026-01-01 01:00Z'")
        .execute(pool)
        .await
        .unwrap();
    let record = read().await.unwrap().expect("completed and mapped");
    assert_eq!(record.sessions.len(), 1, "only exact guild and tag");
    let session = &record.sessions[0];
    assert_eq!(session.member_id, 300000000000000001);
    assert_eq!(session.name_observed_at, None);
    assert_eq!(
        session.left_at - session.joined_at,
        time::Duration::minutes(20)
    );
    sqlx::query("UPDATE event_voice_sessions SET left_at = joined_at + INTERVAL '5 minutes', name_observed_at = joined_at")
        .execute(pool).await.unwrap();
    let record = read().await.unwrap().expect("observed departure");
    let session = &record.sessions[0];
    assert_eq!(session.name_observed_at, Some(session.joined_at));
    assert_eq!(
        session.left_at - session.joined_at,
        time::Duration::minutes(5)
    );
    sqlx::query("UPDATE event_voice_sessions SET left_at = NULL, name_observed_at = NULL")
        .execute(pool)
        .await
        .unwrap();

    for (mutation, undo) in [
        ("UPDATE internal_event_keys SET event_id = '200000000000000002'", "UPDATE internal_event_keys SET event_id = '200000000000000001'"),
        ("UPDATE event_instance_windows SET instance_starts_at = '2026-01-02'", "UPDATE event_instance_windows SET instance_starts_at = '2026-01-01 00:00Z'"),
        ("UPDATE event_instance_windows SET is_current = FALSE", "UPDATE event_instance_windows SET is_current = TRUE"),
        ("UPDATE event_instance_windows SET identity_ambiguous = TRUE", "UPDATE event_instance_windows SET identity_ambiguous = FALSE"),
        ("UPDATE event_instance_windows SET revoked_at = '2026-01-01T01:00:00Z'", "UPDATE event_instance_windows SET revoked_at = NULL"),
        ("INSERT INTO privacy_opt_outs (guild_id,member_id) VALUES ('100000000000000001','300000000000000001')", "DELETE FROM privacy_opt_outs"),
    ] {
        sqlx::query(mutation).execute(pool).await.unwrap();
        assert!(read().await.unwrap().is_none(), "must refuse after {mutation}");
        sqlx::query(undo).execute(pool).await.unwrap();
    }
    // An invalid tagged row must not be dropped in favor of a valid one.
    sqlx::query(
        "INSERT INTO event_voice_sessions (guild_id, member_id, channel_id, joined_at, event_keys)
         SELECT guild_id, '300000000000000005', channel_id, joined_at, event_keys
         FROM event_voice_sessions WHERE member_id = '300000000000000001'",
    )
    .execute(pool)
    .await
    .unwrap();
    assert_eq!(read().await.unwrap().unwrap().sessions.len(), 2);
    for (joined_at, left_at, name_observed_at) in [
        ("-infinity", None, None),
        ("2026-01-01 00:00Z", Some("infinity"), None),
        ("2026-01-01 00:00Z", Some("2025-12-31 23:59Z"), None),
        ("2026-01-01 00:00Z", None, Some("12000-01-01")),
        ("2026-01-01 01:16Z", None, None),
    ] {
        // Parse these fixture values in Postgres; Rust cannot encode infinity.
        sqlx::query(
            "UPDATE event_voice_sessions SET joined_at = $1::text::timestamptz,
                 left_at = $2::text::timestamptz, name_observed_at = $3::text::timestamptz
             WHERE member_id = '300000000000000005'",
        )
        .bind(joined_at)
        .bind(left_at)
        .bind(name_observed_at)
        .execute(pool)
        .await
        .unwrap();
        assert!(
            matches!(read().await, Err(InternalStoreError::Unavailable)),
            "must refuse timestamps {joined_at}, {left_at:?}, {name_observed_at:?}"
        );
    }
    sqlx::query("DELETE FROM event_voice_sessions WHERE member_id = '300000000000000005'")
        .execute(pool)
        .await
        .unwrap();
    for (mutation, undo) in [
        ("UPDATE event_instance_windows SET starts_at = '0001-01-01 BC'", "UPDATE event_instance_windows SET starts_at = '2026-01-01 00:00Z'"),
        ("UPDATE event_attendance_windows SET capture_stops = ARRAY['infinity'::timestamptz]", "UPDATE event_attendance_windows SET capture_stops = '{}'"),
        ("UPDATE event_voice_sessions SET channel_id = '18446744073709551616'", "UPDATE event_voice_sessions SET channel_id = '400000000000000001'"),
        ("UPDATE event_voice_sessions SET channel_name = repeat('x', 65537)", "UPDATE event_voice_sessions SET channel_name = ''"),
        ("UPDATE event_attendance_windows SET capture_stops = ARRAY[NULL::timestamptz]", "UPDATE event_attendance_windows SET capture_stops = '{}'"),
        ("UPDATE event_attendance_windows SET capture_stops = array_fill('2026-01-01 00:20Z'::timestamptz, ARRAY[4097])", "UPDATE event_attendance_windows SET capture_stops = '{}'"),
    ] {
        sqlx::query(mutation).execute(pool).await.unwrap();
        assert!(matches!(read().await, Err(InternalStoreError::Unavailable)), "must refuse after {mutation}");
        sqlx::query(undo).execute(pool).await.unwrap();
    }
    let record = read().await.unwrap().expect("no capture stops");
    let session = &record.sessions[0];
    assert_eq!(
        session.left_at,
        record.ends_at + time::Duration::minutes(15)
    );

    for (copies, name_bytes) in [(17, 65536), (4096, 0)] {
        sqlx::query(
            "INSERT INTO event_voice_sessions
                 (guild_id, member_id, channel_id, joined_at, event_keys, channel_name)
             SELECT '100000000000000001', '300000000000000004', '400000000000000001',
                    '2026-01-01 00:00Z', ARRAY['read-fixture/200000000000000001/1767225600000000'], repeat('x', $2)
             FROM generate_series(1, $1::int)",
        )
        .bind(copies)
        .bind(name_bytes)
        .execute(pool)
        .await
        .unwrap();
        assert!(
            matches!(read().await, Err(InternalStoreError::Unavailable)),
            "must refuse after adding {copies} rows with {name_bytes} name bytes each"
        );
        sqlx::query("DELETE FROM event_voice_sessions WHERE member_id = '300000000000000004'")
            .execute(pool)
            .await
            .unwrap();
    }
    // A legitimate reschedule keeps identity; a replacement with identical
    // scheduled bounds must not inherit old observations.
    sqlx::query("UPDATE event_instance_windows SET starts_at = '2026-01-01 00:05Z'")
        .execute(pool)
        .await
        .unwrap();
    assert_eq!(read().await.unwrap().unwrap().sessions.len(), 1);
    for (event_id, instance_starts_at, expected) in [
        ("200000000000000001", "2026-01-01 00:00:00.000001Z", false),
        ("200000000000000001", "2026-01-01 00:00Z", true),
        ("200000000000000002", "2026-01-01 00:00Z", false),
        ("200000000000000001", "2026-01-01 00:00Z", true),
    ] {
        sqlx::query(
            "WITH mapped AS (UPDATE internal_event_keys SET event_id = $1),
                 current_window AS (UPDATE event_instance_windows
                     SET event_id = $1, instance_starts_at = $2::text::timestamptz)
             UPDATE event_attendance_windows
                 SET event_id = $1, instance_starts_at = $2::text::timestamptz",
        )
        .bind(event_id)
        .bind(instance_starts_at)
        .execute(pool)
        .await
        .unwrap();
        assert_eq!(read().await.unwrap().is_some(), expected);
    }
    sqlx::query("DELETE FROM event_voice_sessions")
        .execute(pool)
        .await
        .unwrap();
    assert!(read().await.unwrap().is_none(), "no capture records");
    db.close().await.unwrap();
}
