//! Round trip through the dormant Bot attendance path with synthetic event evidence.

use std::{
    future::Future,
    sync::atomic::{AtomicUsize, Ordering},
};

use serde_json::{json, Value};
use sqlx::{PgExecutor, PgPool};
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
    retention::{run_retention, RetentionMode},
};
use two_bot_testsupport::TestDatabase;

const EVENT: &str = "200000000000000001";
const EVENT_KEY: &str = "echo-fixture";
const PRE_ENABLE_EVENT: &str = "200000000000000002";
const PRE_ENABLE_KEY: &str = "echo-pre-enable";
const MEMBER: u64 = 300000000000000001;
const CHANNEL: u64 = 400000000000000001;
const ROOM: &str = "Fixture room";
// Canonical Web signer body, without an idempotency key or guild selector.
const READ_BODY: &[u8] = br#"{"action":"event.attendance.read","event_key":"echo-fixture"}"#;
const PRE_ENABLE_BODY: &[u8] =
    br#"{"action":"event.attendance.read","event_key":"echo-pre-enable"}"#;

fn iso(at: OffsetDateTime) -> String {
    format_iso_millis(i64::try_from(at.unix_timestamp_nanos().div_euclid(1_000_000)).unwrap())
}

fn random_nonce() -> String {
    format!("{:032x}", rand::random::<u128>())
}

struct SignedEcho<'a> {
    store: &'a InternalActionStore,
    key: SigningKey,
    keys: KeyRing,
    now: u64,
}

struct SignedEchoRequest<'a> {
    body: &'a [u8],
    key_id: &'a str,
    timestamp: String,
    nonce: &'a str,
    signature: String,
}

impl<'a> SignedEcho<'a> {
    fn new(store: &'a InternalActionStore, now: u64) -> Self {
        let key = SigningKey {
            id: "echo-fixture".into(),
            secret: Secret::new(rand::random::<[u8; 32]>().to_vec()),
        };
        Self {
            store,
            keys: KeyRing::new(vec![key.clone()]),
            key,
            now,
        }
    }

    async fn request(
        &self,
        flags: &InternalFlags,
        body: &[u8],
        sent_at: u64,
        nonce: &str,
    ) -> Result<AttendanceReadRequest, ErrorCode> {
        self.authorize(flags, &self.issue(body, sent_at, nonce))
            .await
    }

    fn issue<'request>(
        &'request self,
        body: &'request [u8],
        sent_at: u64,
        nonce: &'request str,
    ) -> SignedEchoRequest<'request> {
        let timestamp = sent_at.to_string();
        let signature = sign(self.key.secret.expose(), &timestamp, nonce, body);
        SignedEchoRequest {
            body,
            key_id: &self.key.id,
            timestamp,
            nonce,
            signature,
        }
    }

    async fn authorize(
        &self,
        flags: &InternalFlags,
        request: &SignedEchoRequest<'_>,
    ) -> Result<AttendanceReadRequest, ErrorCode> {
        let headers = AuthHeaders {
            key_id: request.key_id,
            timestamp: &request.timestamp,
            nonce: request.nonce,
            signature: &request.signature,
        };
        let verified = AuthenticatedRequest::verify(
            &headers,
            request.body,
            &self.keys,
            SKEW_SECONDS,
            self.now * 1_000,
            &mut ClockGuard::new(),
        )
        .map_err(|error| error.code)?;
        let burned = verified
            .burn_durably(self.store)
            .await
            .map_err(|error| error.code)?;
        burned
            .authorize_attendance_read(flags, &mut TokenBuckets::new())
            .map_err(|error| error.code)
    }
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

async fn stored_attendance<'e>(executor: impl PgExecutor<'e>, guild: &str) -> Value {
    // Each limit is one row above the fixture's expected cardinality.
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
            'keys', ARRAY(SELECT to_jsonb(k) FROM internal_event_keys k
                          WHERE guild_id = $1 ORDER BY event_key LIMIT 3),
            'instances', ARRAY(SELECT to_jsonb(w) FROM event_instance_windows w
                               WHERE guild_id = $1 ORDER BY event_id, instance_starts_at LIMIT 3),
            'windows', ARRAY(SELECT to_jsonb(w) FROM event_attendance_windows w
                             WHERE guild_id = $1 ORDER BY event_key LIMIT 2),
            'spans', ARRAY(SELECT to_jsonb(s) FROM event_voice_sessions s
                           WHERE guild_id = $1 ORDER BY id LIMIT 2)
        )",
    )
    .bind(guild)
    .fetch_one(executor)
    .await
    .unwrap()
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
    // Anchor fresh gateway and signed inputs to the same clock used by storage.
    let now: OffsetDateTime = sqlx::query_scalar("SELECT date_trunc('second', clock_timestamp())")
        .fetch_one(pool)
        .await
        .unwrap();
    let now_secs = u64::try_from(now.unix_timestamp()).unwrap();
    let joined_at = now - Duration::seconds(20);
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
    exercise_pre_enable_window(&store, pool, observation, now).await;

    opt_out_member(&privacy, guild, &member, "fixture")
        .await
        .unwrap();
    store.put_event_key(guild, EVENT_KEY, EVENT).await.unwrap();
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
    assert!(
        !OpenCapture::default()
            .write_observation(pool, &observation)
            .await
            .unwrap(),
        "default-off capture refuses an otherwise admissible join"
    );
    assert!(capture.write_observation(pool, &observation).await.unwrap());

    let signed = SignedEcho::new(&store, now_secs);
    let enabled = InternalFlags::from_map(
        &[("TWO_INTERNAL_ALLOW_ATTENDANCE_READ".into(), "1".into())].into(),
    );
    let read_echo = async |request: AttendanceReadRequest| {
        read(request, pool, |scope| {
            assert_eq!(scope, guild_id);
            // This fixture holds a stable read epoch for the duration of each lookup.
            Some(|| true)
        })
        .await
    };
    let echo = async |body: &[u8], nonce: &str| {
        let request = signed
            .request(&enabled, body, now_secs, nonce)
            .await
            .unwrap();
        read_echo(request).await
    };
    assert_eq!(
        echo(READ_BODY, &random_nonce()).await,
        Err(ReadRefusal::NotFound),
        "active event"
    );

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
        echo(READ_BODY, &random_nonce()).await,
        Err(ReadRefusal::NotFound),
        "grace still open"
    );

    // Supply an earlier confirmed end so the database sees elapsed grace without a sleep.
    let ends_at = joined_at - Duration::minutes(15) + Duration::seconds(10);
    event.ends_at = Some(iso(ends_at));
    upsert_event(pool, guild, &iso(now), &event).await.unwrap();
    let pull_signed = async |flags: &InternalFlags, request: &SignedEchoRequest<'_>| {
        let authorized = signed
            .authorize(flags, request)
            .await
            .map_err(PullRefusal::Request)?;
        read_echo(authorized).await.map_err(PullRefusal::Read)
    };
    let final_nonce = random_nonce();
    let successful = signed.issue(READ_BODY, now_secs, &final_nonce);
    // Keep capture off after seeding. Only the signed reader runs between
    // these snapshots. Its nonce burn is outside attendance storage.
    let before_read = stored_attendance(pool, guild).await;
    for (table, rows) in [("keys", 2), ("instances", 2), ("windows", 1), ("spans", 1)] {
        assert_eq!(
            before_read[table].as_array().unwrap().len(),
            rows,
            "{table}"
        );
    }
    // Web Echo can freeze this v1 result. A real but short visit has zero whole minutes.
    let finalized = pull_signed(&enabled, &successful).await.unwrap();
    assert_eq!(
        stored_attendance(pool, guild).await,
        before_read,
        "the finalized signed read must preserve stored attendance"
    );
    assert_eq!(
        finalized,
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
    // A timestamp outside the response projection still changes stored state.
    let mut control = pool.begin().await.unwrap();
    let changed = sqlx::query(
        "UPDATE event_voice_sessions SET changed_at = changed_at + INTERVAL '1 second'
         WHERE guild_id = $1 AND member_id = $2",
    )
    .bind(guild)
    .bind(&member)
    .execute(&mut *control)
    .await
    .unwrap();
    assert_eq!(changed.rows_affected(), 1);
    assert_ne!(
        stored_attendance(&mut *control, guild).await,
        before_read,
        "the snapshot must detect a stored change outside the read response"
    );
    control.rollback().await.unwrap();
    assert_eq!(stored_attendance(pool, guild).await, before_read);

    assert_eq!(
        echo(PRE_ENABLE_BODY, &random_nonce()).await,
        Err(ReadRefusal::NotFound),
        "the signed key for the pre-enable event has no attendance record"
    );
    assert_eq!(
        pull_signed(&enabled, &successful).await,
        Err(PullRefusal::Request(ErrorCode::Replayed)),
        "the same signed request cannot reach the finalized reader again"
    );

    // A request signed while enabled can arrive for the first time after rollback.
    let delayed_nonce = random_nonce();
    let delayed = signed.issue(READ_BODY, now_secs, &delayed_nonce);
    let disabled = InternalFlags::from_map(&Default::default());
    assert_eq!(
        pull_signed(&disabled, &delayed).await,
        Err(PullRefusal::Request(ErrorCode::ActionNotAllowed)),
        "the disabled reader must refuse a valid first delivery before returning attendance"
    );
    assert_eq!(
        pull_signed(&disabled, &successful).await,
        Err(PullRefusal::Request(ErrorCode::Replayed)),
        "rollback must not replay an earlier successful response"
    );

    let reenabled = InternalFlags::from_map(
        &[("TWO_INTERNAL_ALLOW_ATTENDANCE_READ".into(), "1".into())].into(),
    );
    for issued in [&successful, &delayed] {
        assert_eq!(
            pull_signed(&reenabled, issued).await,
            Err(PullRefusal::Request(ErrorCode::Replayed)),
            "re-enabling must not revive a nonce used before or during rollback"
        );
    }
    let fresh_nonce = random_nonce();
    let fresh = signed.issue(READ_BODY, now_secs, &fresh_nonce);
    assert_eq!(
        pull_signed(&reenabled, &fresh).await.unwrap(),
        finalized,
        "a fresh signed read after re-enabling returns the same finalized v1 record"
    );

    staged_echo_permission_evidence(pool, &signed, &enabled, &finalized).await;
    staged_echo_enablement_and_rollback(pool, &signed, &enabled, &finalized).await;

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
        echo(READ_BODY, &random_nonce()).await,
        Err(ReadRefusal::NotFound),
        "ambiguous occurrence"
    );
    mark_ambiguous(false).await;
    opt_out_member(&privacy, guild, &member, "fixture")
        .await
        .unwrap();
    assert_eq!(
        echo(READ_BODY, &random_nonce()).await,
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
    assert_eq!(
        echo(READ_BODY, &random_nonce()).await,
        Err(ReadRefusal::NotFound),
        "erased span"
    );
}

#[tokio::test]
async fn signed_echo_after_attendance_retention() {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test database required");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .unwrap();
    let result = tokio::spawn(exercise_echo_retention(db.pool().clone())).await;
    db.close().await.unwrap();
    result.unwrap();
}

async fn exercise_echo_retention(pool: PgPool) {
    const EXPIRED_KEY: &str = "echo-expired";
    const EXPIRED_BODY: &[u8] = br#"{"action":"event.attendance.read","event_key":"echo-expired"}"#;
    let guild = TWO_STAGING_GUILD_ID;
    let store = InternalActionStore::new(pool.clone());
    let now: OffsetDateTime = sqlx::query_scalar("SELECT date_trunc('second', clock_timestamp())")
        .fetch_one(&pool)
        .await
        .unwrap();
    let now_secs = u64::try_from(now.unix_timestamp()).unwrap();
    let signed = SignedEcho::new(&store, now_secs);
    let enabled = InternalFlags::from_map(
        &[("TWO_INTERNAL_ALLOW_ATTENDANCE_READ".into(), "1".into())].into(),
    );
    let echo = async |body: &[u8]| {
        let request = signed
            .request(&enabled, body, now_secs, &random_nonce())
            .await
            .unwrap();
        read(request, &pool, |_| Some(|| true)).await
    };
    let seed_and_read = async |event_id: &str,
                               key: &str,
                               body: &[u8],
                               age,
                               member: u64,
                               channel: u64,
                               room: &str| {
        let member = member.to_string();
        let channel = channel.to_string();
        let start = now - Duration::days(age);
        let end = start + Duration::minutes(30);
        let joined = start + Duration::minutes(1);
        let event = ScheduledEvent {
            id: event_id.into(),
            name: "Fixture event".into(),
            starts_at: iso(start),
            ends_at: Some(iso(end)),
            channel_id: Some(channel.clone()),
            description: None,
            status: EventStatus::Completed,
        };
        store.put_event_key(guild, key, event_id).await.unwrap();
        upsert_event(&pool, guild, &iso(end), &event).await.unwrap();
        // Seed historical evidence directly. The live writer admits fresh observations only.
        sqlx::query(
            "WITH captured AS (
                INSERT INTO event_attendance_windows
                    (guild_id, event_key, event_id, instance_starts_at, starts_at, ends_at)
                SELECT guild_id, $3, event_id, instance_starts_at, starts_at, ends_at
                FROM event_instance_windows WHERE guild_id = $1 AND event_id = $2 AND is_current
                RETURNING guild_id, event_key, event_id, instance_starts_at
             )
             INSERT INTO event_voice_sessions
                 (guild_id, member_id, channel_id, channel_name, joined_at, left_at,
                  name_observed_at, event_keys)
             SELECT guild_id, $4, $5, $6, $7, $8, $7,
                    ARRAY[event_key || '/' || event_id || '/' ||
                          trunc(EXTRACT(EPOCH FROM instance_starts_at) * 1000000)::text]
             FROM captured",
        )
        .bind(guild)
        .bind(event_id)
        .bind(key)
        .bind(&member)
        .bind(&channel)
        .bind(room)
        .bind(joined)
        .bind(joined + Duration::minutes(12))
        .execute(&pool)
        .await
        .unwrap();

        let before = echo(body).await.unwrap();
        assert_eq!(
            before,
            json!({
                "v": 1,
                "event_key": key,
                "window": { "starts_at": iso(start), "ends_at": iso(end) },
                "threshold_minutes": 10,
                "peak_concurrent": 1,
                "rooms": [{ "channel_id": channel, "name_at_close": room }],
                "members": [{
                    "discord_id": member, "minutes": 12, "sessions": 1,
                    "first_joined_at": iso(joined),
                }],
                "final": true,
            })
        );
        before
    };
    seed_and_read(
        "200000000000000003",
        EXPIRED_KEY,
        EXPIRED_BODY,
        91,
        300000000000000003,
        400000000000000003,
        "Expired fixture room",
    )
    .await;
    let control = seed_and_read(EVENT, EVENT_KEY, READ_BODY, 89, MEMBER, CHANNEL, ROOM).await;

    let report = run_retention(&pool, RetentionMode::Execute).await.unwrap();
    assert_eq!(report.attendance_raw_deleted, 1);
    let survivors: Vec<String> = sqlx::query_scalar("SELECT member_id FROM event_voice_sessions")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(survivors, [MEMBER.to_string()]);
    let expired_window_remains: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM internal_event_keys k
             JOIN event_attendance_windows h USING (guild_id, event_key, event_id)
             JOIN event_instance_windows w USING (guild_id, event_id, instance_starts_at)
             WHERE k.guild_id = $1 AND k.event_key = $2 AND w.is_current
                 AND w.status = 'completed' AND NOT w.identity_ambiguous AND w.revoked_at IS NULL
         )",
    )
    .bind(guild)
    .bind(EXPIRED_KEY)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        expired_window_remains,
        "retention preserves the mapped window"
    );
    assert_eq!(echo(EXPIRED_BODY).await, Err(ReadRefusal::NotFound));
    assert_eq!(echo(READ_BODY).await.unwrap(), control);
}

async fn exercise_pre_enable_window(
    store: &InternalActionStore,
    pool: &PgPool,
    mut observation: VoiceObservation<'_>,
    now: OffsetDateTime,
) {
    let guild = TWO_STAGING_GUILD_ID;
    observation.observed_at -= Duration::seconds(5);
    let joined_at = observation.observed_at;
    let mut event = ScheduledEvent {
        id: PRE_ENABLE_EVENT.into(),
        name: "Pre-enable fixture event".into(),
        starts_at: iso(joined_at - Duration::hours(2)),
        ends_at: Some(iso(joined_at + Duration::minutes(1))),
        channel_id: Some(CHANNEL.to_string()),
        description: None,
        status: EventStatus::Active,
    };
    store
        .put_event_key(guild, PRE_ENABLE_KEY, PRE_ENABLE_EVENT)
        .await
        .unwrap();
    upsert_event(pool, guild, &iso(joined_at - Duration::seconds(1)), &event)
        .await
        .unwrap();
    assert!(
        !OpenCapture::default()
            .write_observation(pool, &observation)
            .await
            .unwrap(),
        "capture is off for the earlier join"
    );

    // Confirm an earlier actual end to model elapsed grace without sleeping.
    // The original join still sits inside that window's grace and is fresh
    // enough to reach the writer's elapsed-window check after enablement.
    event.status = EventStatus::Completed;
    event.ends_at = Some(iso(joined_at - Duration::minutes(15) + Duration::seconds(2)));
    upsert_event(pool, guild, &iso(now), &event).await.unwrap();
    let elapsed: bool = sqlx::query_scalar(
        "SELECT w.status = 'completed' AND w.ends_at + INTERVAL '15 minutes' > $3
             AND w.ends_at + INTERVAL '15 minutes' <= statement_timestamp()
             AND $3 >= statement_timestamp() - INTERVAL '1 minute'
         FROM internal_event_keys k JOIN event_instance_windows w USING (guild_id, event_id)
         WHERE k.guild_id = $1 AND k.event_key = $2 AND w.is_current",
    )
    .bind(guild)
    .bind(PRE_ENABLE_KEY)
    .bind(joined_at)
    .fetch_one(pool)
    .await
    .unwrap();
    assert!(
        elapsed,
        "mapped window elapsed while the join was still fresh"
    );
    assert!(
        !OpenCapture::Enabled
            .write_observation(pool, &observation)
            .await
            .unwrap(),
        "enabling capture must not backfill the completed window"
    );
}

#[derive(Debug, PartialEq, Eq)]
enum PullRefusal {
    Request(ErrorCode),
    Read(ReadRefusal),
}

// Web owns whether to pull and freeze. The fixture records those effects in
// memory. The real Web parser and public projection are covered in two-web-next.
#[derive(Default)]
struct WebPeer {
    pulls: usize,
    frozen: Vec<Value>,
}

impl WebPeer {
    async fn poll(
        &mut self,
        enabled: bool,
        pull: impl Future<Output = Result<Value, PullRefusal>>,
    ) -> Result<(), PullRefusal> {
        if enabled {
            self.pulls += 1;
            self.frozen.push(pull.await?);
        }
        Ok(())
    }
}

async fn staged_echo_permission_evidence(
    pool: &PgPool,
    signed: &SignedEcho<'_>,
    enabled: &InternalFlags,
    finalized: &Value,
) {
    let mut web = WebPeer::default();
    let approved_revision = 1;
    let expires_at = 10;
    for (change, next_evidence) in [
        ("expires at the boundary", (10, approved_revision)),
        ("changes while still fresh", (9, approved_revision + 1)),
    ] {
        // Model the staged permission readback independently of signing time,
        // rollout identity, lease and the enabled reader flag.
        let nonce = random_nonce();
        let signed_request = signed.issue(READ_BODY, signed.now, &nonce);
        let request = signed.authorize(enabled, &signed_request).await.unwrap();
        let checks = AtomicUsize::new(0);
        let pull = read(request, pool, |guild| {
            assert_eq!(guild.to_string(), TWO_STAGING_GUILD_ID);
            Some(|| {
                // Let lookup start under the approved ticket. Present the
                // changed evidence when the same ticket is checked again.
                let (now, revision) = if checks.fetch_add(1, Ordering::Relaxed) == 0 {
                    (9, approved_revision)
                } else {
                    next_evidence
                };
                now < expires_at && revision == approved_revision
            })
        });
        assert_eq!(
            web.poll(true, async { pull.await.map_err(PullRefusal::Read) })
                .await,
            Err(PullRefusal::Read(ReadRefusal::Unavailable)),
            "permission evidence {change}"
        );
        assert_eq!(checks.load(Ordering::Relaxed), 2);
        assert!(web.frozen.is_empty());
        assert_eq!(
            signed.authorize(enabled, &signed_request).await.err(),
            Some(ErrorCode::Replayed),
            "permission refusal still consumes the signed request"
        );
    }
    assert_eq!(web.pulls, 2);

    // A fresh signed request under refreshed approval can still read the same
    // fixture after the refusals.
    let request = signed
        .request(enabled, READ_BODY, signed.now, &random_nonce())
        .await
        .unwrap();
    let pull = read(request, pool, |_| Some(|| true));
    web.poll(true, async { pull.await.map_err(PullRefusal::Read) })
        .await
        .unwrap();
    assert_eq!(web.frozen.as_slice(), std::slice::from_ref(finalized));
}

async fn staged_echo_enablement_and_rollback(
    pool: &PgPool,
    signed: &SignedEcho<'_>,
    enabled: &InternalFlags,
    finalized: &Value,
) {
    let disabled = InternalFlags::from_map(&Default::default());
    let pull = async |flags: &InternalFlags, body: &[u8], sent_at, ticket: fn() -> bool| {
        let request = signed
            .request(flags, body, sent_at, &random_nonce())
            .await
            .map_err(PullRefusal::Request)?;
        read(request, pool, |_| Some(ticket))
            .await
            .map_err(PullRefusal::Read)
    };
    let steady = async |flags: &InternalFlags| pull(flags, READ_BODY, signed.now, || true).await;

    let stored_spans = async || {
        sqlx::query_scalar::<_, Value>(
            "SELECT to_jsonb(span) FROM event_voice_sessions AS span WHERE guild_id = $1 ORDER BY id",
        )
        .bind(TWO_STAGING_GUILD_ID)
        .fetch_all(pool)
        .await
        .unwrap()
    };
    let before = stored_spans().await;
    assert_eq!(before.len(), 1);
    let denied = Err(PullRefusal::Request(ErrorCode::ActionNotAllowed));

    let mut web_first = WebPeer::default();
    assert_eq!(web_first.poll(true, steady(&disabled)).await, denied);
    assert_eq!(web_first.pulls, 1);
    assert!(web_first.frozen.is_empty());
    web_first.poll(true, steady(enabled)).await.unwrap();
    assert_eq!(web_first.frozen.as_slice(), std::slice::from_ref(finalized));

    let mut bot_first = WebPeer::default();
    // A disabled peer issues no request, even with Bot able to serve this record.
    bot_first.poll(false, steady(enabled)).await.unwrap();
    assert_eq!(bot_first.pulls, 0);
    assert!(bot_first.frozen.is_empty());
    assert_eq!(
        bot_first
            .poll(
                true,
                pull(enabled, READ_BODY, signed.now - SKEW_SECONDS - 1, || true)
            )
            .await,
        Err(PullRefusal::Request(ErrorCode::StaleRequest))
    );
    assert_eq!(
        bot_first
            .poll(true, pull(enabled, READ_BODY, signed.now, || false))
            .await,
        Err(PullRefusal::Read(ReadRefusal::Unavailable))
    );
    // A different verb is how the contract can introduce a breaking request.
    let unsupported = br#"{"action":"event.attendance.read.v2","event_key":"echo-fixture"}"#;
    assert_eq!(
        bot_first
            .poll(true, pull(enabled, unsupported, signed.now, || true))
            .await,
        denied
    );
    assert!(bot_first.frozen.is_empty());
    bot_first.poll(true, steady(enabled)).await.unwrap();
    assert_eq!(bot_first.frozen.as_slice(), std::slice::from_ref(finalized));

    // Roll back Web first and then Bot. Also exercise Bot rollback while Web
    // still pulls. Neither rollback purges the existing records.
    let pulls = bot_first.pulls;
    bot_first.poll(false, steady(enabled)).await.unwrap();
    bot_first.poll(false, steady(&disabled)).await.unwrap();
    assert_eq!(bot_first.pulls, pulls);
    assert_eq!(web_first.poll(true, steady(&disabled)).await, denied);
    web_first.poll(false, steady(&disabled)).await.unwrap();
    assert_eq!(web_first.frozen, bot_first.frozen);
    assert_eq!(bot_first.frozen.as_slice(), std::slice::from_ref(finalized));
    assert_eq!(stored_spans().await, before);
    assert_eq!(
        &steady(enabled).await.unwrap(),
        finalized,
        "re-enabling reads still resolves the preserved event and span"
    );
}
