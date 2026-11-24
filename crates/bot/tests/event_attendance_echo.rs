//! Round trip through the dormant Bot attendance path with synthetic event evidence.

use serde_json::json;
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use two_bot::event_attendance::{
    planning::{OpenCapture, VoiceObservation},
    read::{read, ReadRefusal},
    recovery::{AttendanceRecovery, MemberLossFence},
};
use two_bot_core::{
    backup::guild_config::TWO_STAGING_GUILD_ID,
    clock_guard::ClockGuard,
    funnel::format_iso_millis,
    internal_action_store::InternalActionStore,
    internal_actions::{
        sign, AttendanceReadRequest, AuthHeaders, AuthenticatedRequest, ErrorCode, InternalFlags,
        KeyRing, SigningKey, TokenBuckets, SKEW_SECONDS,
    },
    scheduled_events::{EventStatus, ScheduledEvent},
    website_store::upsert_event,
    Secret,
};
use two_bot_cutover::{
    db::CutoverDb,
    member_erasure::{erase_member, ErasureMode},
    privacy::{opt_back_in, opt_out_member, set_analytics_settings},
};
use two_bot_testsupport::TestDatabase;

const EVENT: &str = "200000000000000001";
const EVENT_KEY: &str = "echo-fixture";
const MEMBER: u64 = 300000000000000001;
const CHANNEL: u64 = 400000000000000001;
const ROOM: &str = "Fixture room";

fn iso(at: OffsetDateTime) -> String {
    format_iso_millis(i64::try_from(at.unix_timestamp_nanos().div_euclid(1_000_000)).unwrap())
}

async fn signed_request(
    store: &InternalActionStore,
    now: u64,
    nonce: u64,
) -> Result<AttendanceReadRequest, ErrorCode> {
    // The Web signer sends a canonical body with no idempotency key or guild selector.
    let body = br#"{"action":"event.attendance.read","event_key":"echo-fixture"}"#;
    let key_id = "echo-fixture";
    let key = SigningKey {
        id: key_id.into(),
        secret: Secret::new(vec![0; 32]),
    };
    let timestamp = now.to_string();
    let nonce = format!("{nonce:032x}");
    let signature = sign(key.secret.expose(), &timestamp, &nonce, body);
    let headers = AuthHeaders {
        key_id,
        timestamp: &timestamp,
        nonce: &nonce,
        signature: &signature,
    };
    let verified = AuthenticatedRequest::verify(
        &headers,
        body,
        &KeyRing::new(vec![key]),
        SKEW_SECONDS,
        now * 1_000,
        &mut ClockGuard::new(),
    )
    .map_err(|error| error.code)?;
    let burned = verified
        .burn_durably(store)
        .await
        .map_err(|error| error.code)?;
    let flags = InternalFlags::from_map(
        &[("TWO_INTERNAL_ALLOW_ATTENDANCE_READ".into(), "1".into())].into(),
    );
    burned
        .authorize_attendance_read(&flags, &mut TokenBuckets::new())
        .map_err(|error| error.code)
}

#[tokio::test]
async fn mapped_observation_through_recovery_to_signed_echo_read() {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test database required");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .unwrap();
    // Await teardown even when an assertion in the fixture fails.
    let result = tokio::spawn(exercise_echo(db.pool().clone())).await;
    db.close().await.unwrap();
    result.unwrap();
}

async fn exercise_echo(pool: PgPool) {
    let pool = &pool;
    // The read adapter binds this guild internally. The pool contains only fixture data.
    let guild = TWO_STAGING_GUILD_ID;
    let guild_id = guild.parse().unwrap();
    let member = MEMBER.to_string();
    let store = InternalActionStore::new(pool.clone());
    let privacy = CutoverDb::from_pool(pool.clone());
    set_analytics_settings(&privacy, guild, true, true, true)
        .await
        .unwrap();
    opt_out_member(&privacy, guild, &member, "fixture")
        .await
        .unwrap();
    store.put_event_key(guild, EVENT_KEY, EVENT).await.unwrap();

    // Anchor fresh gateway and signed inputs to the same clock used by storage.
    let now: OffsetDateTime = sqlx::query_scalar("SELECT date_trunc('second', clock_timestamp())")
        .fetch_one(pool)
        .await
        .unwrap();
    let now_secs = u64::try_from(now.unix_timestamp()).unwrap();
    let joined_at = now - Duration::seconds(20);
    let left_at = joined_at + Duration::seconds(5);
    let starts_at = joined_at - Duration::minutes(30);
    let mut event = ScheduledEvent {
        id: EVENT.into(),
        name: "Fixture event".into(),
        starts_at: iso(starts_at),
        ends_at: Some(iso(joined_at - Duration::minutes(1))),
        channel_id: Some(CHANNEL.to_string()),
        description: None,
        status: EventStatus::Active,
    };
    upsert_event(pool, guild, &iso(joined_at - Duration::seconds(2)), &event)
        .await
        .unwrap();
    let observation = VoiceObservation {
        guild_id,
        member_id: MEMBER,
        previous: None,
        current: Some(CHANNEL),
        channel_name: ROOM,
        is_bot: false,
        afk_channel: None,
        observed_at: joined_at,
    };
    let capture = OpenCapture::Enabled;
    assert!(
        !capture.write_observation(pool, &observation).await.unwrap(),
        "opted-out join"
    );
    assert!(opt_back_in(&privacy, guild, &member).await.unwrap());
    // Capture a fresh join. Opting in must never fill in the refused observation.
    let observation = VoiceObservation {
        observed_at: joined_at + Duration::seconds(1),
        ..observation
    };
    assert!(capture.write_observation(pool, &observation).await.unwrap());

    let echo = async |nonce| {
        let request = signed_request(&store, now_secs, nonce).await.unwrap();
        read(request, pool, |scope| {
            assert_eq!(scope, guild_id);
            // This fixture holds a stable read epoch for the duration of each lookup.
            Some(|| true)
        })
        .await
    };
    assert_eq!(echo(1).await, Err(ReadRefusal::NotFound), "active event");

    let mut recovery = AttendanceRecovery::new(guild_id, MemberLossFence::default());
    recovery.retain_departure(MEMBER, CHANNEL, left_at);
    recovery.turn(pool).await.unwrap();
    assert!(recovery.capture_allowed());
    let span: (OffsetDateTime, Option<OffsetDateTime>) = sqlx::query_as(
        "SELECT joined_at, left_at FROM event_voice_sessions WHERE guild_id = $1 AND member_id = $2",
    )
    .bind(guild)
    .bind(&member)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(span, (observation.observed_at, Some(left_at)));
    assert!(
        !capture.write_observation(pool, &observation).await.unwrap(),
        "closed join replay"
    );

    event.status = EventStatus::Completed;
    upsert_event(pool, guild, &iso(left_at), &event)
        .await
        .unwrap();
    assert_eq!(
        echo(2).await,
        Err(ReadRefusal::NotFound),
        "grace still open"
    );

    // Supply an earlier confirmed end so the database sees elapsed grace without a sleep.
    let ends_at = joined_at - Duration::minutes(15) + Duration::seconds(10);
    event.ends_at = Some(iso(ends_at));
    upsert_event(pool, guild, &iso(now), &event).await.unwrap();
    // Web Echo can freeze this v1 result. A real but short visit has zero whole minutes.
    assert_eq!(
        echo(3).await.unwrap(),
        json!({
            "v": 1,
            "event_key": EVENT_KEY,
            "window": { "starts_at": iso(starts_at), "ends_at": iso(ends_at) },
            "threshold_minutes": 10,
            "peak_concurrent": 1,
            "rooms": [{ "channel_id": CHANNEL.to_string(), "name_at_close": ROOM }],
            "members": [{
                "discord_id": member,
                "minutes": 0,
                "sessions": 1,
                "first_joined_at": iso(observation.observed_at),
            }],
            "final": true,
        })
    );
    assert_eq!(
        signed_request(&store, now_secs, 3).await.err(),
        Some(ErrorCode::Replayed),
        "the same signed request cannot reach the finalized reader again"
    );

    let mark_ambiguous = async |ambiguous| {
        sqlx::query(
            "UPDATE event_instance_windows SET identity_ambiguous = $3
             WHERE guild_id = $1 AND event_id = $2",
        )
        .bind(guild)
        .bind(EVENT)
        .bind(ambiguous)
        .execute(pool)
        .await
        .unwrap();
    };
    mark_ambiguous(true).await;
    assert_eq!(
        echo(4).await,
        Err(ReadRefusal::NotFound),
        "ambiguous occurrence"
    );
    mark_ambiguous(false).await;
    opt_out_member(&privacy, guild, &member, "fixture")
        .await
        .unwrap();
    assert_eq!(
        echo(5).await,
        Err(ReadRefusal::NotFound),
        "suppressed member"
    );
    erase_member(
        pool,
        guild,
        &member,
        ErasureMode::Execute { actor: "fixture" },
    )
    .await
    .unwrap();
    assert!(opt_back_in(&privacy, guild, &member).await.unwrap());
    assert!(
        !capture.write_observation(pool, &observation).await.unwrap(),
        "erased history cannot be backfilled"
    );
    assert_eq!(echo(6).await, Err(ReadRefusal::NotFound), "erased span");
}
