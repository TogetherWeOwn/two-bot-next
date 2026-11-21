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
    sqlx::raw_sql("INSERT INTO event_instance_windows
          (guild_id,event_id,instance_starts_at,starts_at,ends_at,status,observed_at)
          VALUES ('100000000000000001','200000000000000001','2026-01-01','2026-01-01',
                  '2026-01-01 01:00Z','active','2026-01-01T01:00:00Z');
        INSERT INTO event_attendance_windows
          (guild_id,event_key,event_id,instance_starts_at,starts_at,ends_at,capture_stops)
          VALUES ('100000000000000001','read-fixture','200000000000000001',
                  '2026-01-01','2026-01-01','2026-01-01 01:00Z',ARRAY['2026-01-01 00:20Z'::timestamptz]);
        INSERT INTO event_voice_sessions (guild_id,member_id,channel_id,joined_at,event_keys)
          VALUES ('100000000000000001','300000000000000001','400000000000000001','2026-01-01',ARRAY['read-fixture']),
                 ('100000000000000002','300000000000000002','400000000000000001','2026-01-01',ARRAY['read-fixture']),
                 ('100000000000000001','300000000000000003','400000000000000001','2026-01-01',ARRAY['other']);")
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
    assert_eq!(
        session.left_at - session.joined_at,
        time::Duration::minutes(20)
    );

    for (mutation, undo) in [
        ("UPDATE internal_event_keys SET event_id = '200000000000000002'", "UPDATE internal_event_keys SET event_id = '200000000000000001'"),
        ("UPDATE event_instance_windows SET instance_starts_at = '2026-01-02'", "UPDATE event_instance_windows SET instance_starts_at = '2026-01-01'"),
        ("UPDATE event_instance_windows SET identity_ambiguous = TRUE", "UPDATE event_instance_windows SET identity_ambiguous = FALSE"),
        ("UPDATE event_instance_windows SET revoked_at = '2026-01-01T01:00:00Z'", "UPDATE event_instance_windows SET revoked_at = NULL"),
        ("INSERT INTO privacy_opt_outs (guild_id,member_id) VALUES ('100000000000000001','300000000000000001')", "DELETE FROM privacy_opt_outs"),
    ] {
        sqlx::query(mutation).execute(pool).await.unwrap();
        assert!(read().await.unwrap().is_none(), "must refuse after {mutation}");
        sqlx::query(undo).execute(pool).await.unwrap();
    }
    sqlx::query("UPDATE event_voice_sessions SET channel_name = repeat('x', 1048577)")
        .execute(pool)
        .await
        .unwrap();
    assert!(matches!(read().await, Err(InternalStoreError::Unavailable)));
    sqlx::raw_sql(
        "UPDATE event_voice_sessions SET channel_name = '';
                   UPDATE event_attendance_windows SET capture_stops = ARRAY[NULL::timestamptz]",
    )
    .execute(pool)
    .await
    .unwrap();
    assert!(matches!(read().await, Err(InternalStoreError::Unavailable)));
    sqlx::query("DELETE FROM event_voice_sessions")
        .execute(pool)
        .await
        .unwrap();
    assert!(read().await.unwrap().is_none(), "no capture records");
    db.close().await.unwrap();
}
