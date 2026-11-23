//! Synthetic gateway observations against an owned disposable database.

use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use two_bot::event_attendance::planning::{OpenCapture, VoiceObservation};
use two_bot_core::internal_action_store::InternalActionStore;
use two_bot_cutover::{
    db::CutoverDb,
    event_attendance::finalization::{close_observed, CloseOutcome},
    privacy::{opt_back_in, opt_out_member, set_analytics_settings},
};
use two_bot_testsupport::TestDatabase;

const GUILD: u64 = 100000000000000001;
const OTHER_GUILD: u64 = 100000000000000002;
const EVENT: &str = "200000000000000001";
const MEMBER: u64 = 300000000000000001;
const CHANNEL: u64 = 400000000000000001;

async fn write(pool: &PgPool, frame: VoiceObservation<'_>) -> bool {
    OpenCapture::Enabled
        .write_observation(pool, &frame)
        .await
        .unwrap()
}

#[tokio::test]
async fn mapped_gateway_opens_respect_privacy_and_replay() {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test database required");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .unwrap();
    // Await teardown before propagating an assertion failure from the fixture.
    let result = tokio::spawn(exercise_writer(db.pool().clone())).await;
    db.close().await.unwrap();
    result.unwrap();
}

async fn exercise_writer(pool: PgPool) {
    let pool = &pool;
    sqlx::raw_sql(include_str!("fixtures/event_attendance_writer.sql"))
        .execute(pool)
        .await
        .unwrap();
    let guild = GUILD.to_string();
    let member = MEMBER.to_string();
    let (instance, starts): (OffsetDateTime, OffsetDateTime) = sqlx::query_as(
        "SELECT instance_starts_at, starts_at FROM event_instance_windows WHERE event_id = $1",
    )
    .bind(EVENT)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_ne!(instance, starts);
    let at = starts - Duration::minutes(15);
    let frame = VoiceObservation {
        guild_id: GUILD,
        member_id: MEMBER,
        previous: None,
        current: Some(CHANNEL),
        channel_name: "Fixture room",
        is_bot: false,
        afk_channel: Some(CHANNEL),
        observed_at: at,
    };
    assert!(!write(pool, frame).await);
    let mappings = InternalActionStore::new(pool.clone());
    for key in ["show", "alias"] {
        mappings.put_event_key(&guild, key, EVENT).await.unwrap();
    }
    let mut outside = frame;
    for observed_at in [at - Duration::microseconds(1), at + Duration::seconds(30)] {
        outside.observed_at = observed_at;
        assert!(!write(pool, outside).await);
    }

    let privacy = CutoverDb::from_pool(pool.clone());
    set_analytics_settings(&privacy, &guild, true, true, false)
        .await
        .unwrap();
    assert!(!write(pool, frame).await);
    opt_out_member(&privacy, &guild, &member, "fixture")
        .await
        .unwrap();
    set_analytics_settings(&privacy, &guild, true, true, true)
        .await
        .unwrap();
    assert!(!write(pool, frame).await);
    let history_empty = "SELECT NOT EXISTS (SELECT 1 FROM event_attendance_windows)";
    let unused: bool = sqlx::query_scalar(history_empty)
        .fetch_one(pool)
        .await
        .unwrap();
    assert!(unused, "refusals must not write historical windows");
    let foreign = VoiceObservation {
        guild_id: OTHER_GUILD,
        ..frame
    };
    assert!(write(pool, foreign).await);
    assert!(opt_back_in(&privacy, &guild, &member).await.unwrap());

    let (a, b) = tokio::join!(write(pool, frame), write(pool, frame));
    assert_ne!(a, b, "concurrent frames admit one span");
    let micros = instance.unix_timestamp_nanos() / 1_000;
    let tags = ["alias", "show"].map(|key| format!("{key}/{EVENT}/{micros}"));
    let id = sqlx::query_scalar(
        "SELECT id FROM event_voice_sessions
         WHERE guild_id = $1 AND member_id = $2 AND channel_id = $3
           AND joined_at = $4 AND name_observed_at = $4 AND afk AND event_keys = $5",
    )
    .bind(&guild)
    .bind(&member)
    .bind(CHANNEL.to_string())
    .bind(at)
    .bind(&tags[..])
    .fetch_one(pool)
    .await
    .expect("scoped span must keep the exact join, AFK state and occurrence tags");
    let next = VoiceObservation {
        previous: Some(CHANNEL),
        current: Some(CHANNEL + 1),
        observed_at: at + Duration::seconds(1),
        ..frame
    };
    assert!(!write(pool, next).await);
    let closed = close_observed(pool, &guild, &member, id, next.observed_at)
        .await
        .unwrap();
    assert_eq!(closed, CloseOutcome::Closed);
    let mut replay = frame;
    for observed_at in [at, at + Duration::milliseconds(500)] {
        replay.observed_at = observed_at;
        assert!(!write(pool, replay).await, "a closed span cannot reopen");
    }
    assert!(write(pool, next).await, "a later move can open");
    sqlx::query("ALTER TABLE privacy_opt_outs RENAME TO fixture_privacy_unavailable")
        .execute(pool)
        .await
        .unwrap();
    let unavailable = OpenCapture::Enabled
        .write_observation(pool, &next)
        .await
        .unwrap_err();
    let message = unavailable.to_string();
    assert_eq!(message, "attendance open storage unavailable");
    pool.close().await;
    let mut skipped = frame;
    for (mode, previous, current) in [
        (OpenCapture::default(), None, Some(CHANNEL)),
        (OpenCapture::Enabled, Some(CHANNEL), Some(CHANNEL)),
        (OpenCapture::Enabled, Some(CHANNEL), None),
    ] {
        skipped.previous = previous;
        skipped.current = current;
        let opened = mode.write_observation(pool, &skipped).await.unwrap();
        assert!(!opened);
    }
}
