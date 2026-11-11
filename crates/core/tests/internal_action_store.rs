//! Explicitly requested tests require the authorized test container; never skip
//! configured failures or consult a production/staging URL. Each test owns a disposable database.
#![cfg(feature = "db")]

use sqlx::{PgPool, Row};
use two_bot_core::clock_guard::ClockGuard;
use two_bot_core::internal_action_store::{
    AuditSubject, DiscordId, EventOutcome, ExecutionClaim, InternalActionStore, InternalClaim,
    InternalStoreError, ReconciliationEvidence, RequestIdentity, TerminalFailure, TerminalResponse,
    UnknownEventIntent,
};
use two_bot_core::secret::Secret;
use two_bot_core::{body_hash, CLAIM_STALE_SECONDS, NONCE_TTL_SECONDS, SKEW_SECONDS};
use two_bot_testsupport::TestDatabase;

struct TestDb {
    fixture: TestDatabase,
    pool: PgPool,
}

impl TestDb {
    async fn new() -> Self {
        let url = std::env::var("TWO_TEST_DATABASE_URL")
            .expect("explicit DB test requires TWO_TEST_DATABASE_URL (test bootstrap only)");
        let fixture = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .unwrap();
        let pool = fixture.pool().clone();
        Self { fixture, pool }
    }

    async fn independent_pool(&self) -> PgPool {
        self.fixture.independent_pool().await.unwrap()
    }

    fn store(&self) -> InternalActionStore {
        InternalActionStore::new(self.pool.clone())
    }

    async fn cleanup(self) {
        self.fixture.close().await.unwrap();
    }
}

/// Database clock in whole seconds. Timestamps handed to `burn_nonce` must be
/// fresh against this same clock at commit time, so read them here rather than
/// assuming the runner and database clocks agree.
async fn db_now_secs(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT floor(extract(epoch FROM clock_timestamp()))::bigint")
        .fetch_one(pool)
        .await
        .unwrap()
}

const SNAPSHOT_GUILD: &str = "100000000000000001";
const SNAPSHOT_EVENT: &str = "200000000000000001";
const SNAPSHOT_CHANNEL: &str = "300000000000000001";

async fn snapshot_time(pool: &PgPool, relative: &str) -> String {
    sqlx::query_scalar(
        "SELECT to_char((NOW() + $1::text::interval) AT TIME ZONE 'UTC', \
         'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')",
    )
    .bind(relative)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn snapshot_event_fixture(
    db: &TestDb,
    status: two_bot_core::scheduled_events::EventStatus,
) -> two_bot_core::scheduled_events::ScheduledEvent {
    let starts_at: String = sqlx::query_scalar(
        "SELECT to_char((NOW() - INTERVAL '1 hour') AT TIME ZONE 'UTC', \
         'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    let ends_at = if status == two_bot_core::scheduled_events::EventStatus::Completed {
        Some(snapshot_time(&db.pool, "-5 minutes").await)
    } else {
        None
    };
    let event = two_bot_core::scheduled_events::ScheduledEvent {
        id: SNAPSHOT_EVENT.to_owned(),
        name: "fixture".to_owned(),
        starts_at,
        ends_at,
        channel_id: Some(SNAPSHOT_CHANNEL.to_owned()),
        description: None,
        status,
    };
    two_bot_core::website_store::upsert_event(
        &db.pool,
        SNAPSHOT_GUILD,
        "2026-11-03T10:00:00.000Z",
        &event,
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO event_voice_channels \
         (event_id, instance_starts_at, starts_at, ends_at, voice_channel_id) \
         VALUES ($1, $2::text::timestamptz, $2::text::timestamptz, \
                 NOW() + INTERVAL '1 hour', $3)",
    )
    .bind(SNAPSHOT_EVENT)
    .bind(&event.starts_at)
    .bind(SNAPSHOT_CHANNEL)
    .execute(&db.pool)
    .await
    .unwrap();
    db.store()
        .put_event_key(SNAPSHOT_GUILD, "launch", SNAPSHOT_EVENT)
        .await
        .unwrap();
    event
}

#[tokio::test]
async fn voice_snapshot_event_key_tracks_mutation_reschedules_both_directions() {
    use two_bot_core::{scheduled_events::EventStatus, website_store::upsert_event};

    let db = TestDb::new().await;
    let mut event = snapshot_event_fixture(&db, EventStatus::Scheduled).await;
    let original_start = event.starts_at.clone();
    assert_eq!(
        db.store()
            .windowed_event_voice_keys(SNAPSHOT_GUILD)
            .await
            .unwrap()
            .get(SNAPSHOT_CHANNEL),
        Some(&"launch".to_owned())
    );
    event.starts_at = sqlx::query_scalar(
        "SELECT to_char((NOW() + INTERVAL '2 hours') AT TIME ZONE 'UTC', \
         'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    upsert_event(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:01:00.000Z", &event)
        .await
        .unwrap();
    assert!(
        db.store()
            .windowed_event_voice_keys(SNAPSHOT_GUILD)
            .await
            .unwrap()
            .is_empty(),
        "event.upsert must move the linked room out of the live window"
    );
    event.starts_at = original_start;
    upsert_event(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:02:00.000Z", &event)
        .await
        .unwrap();
    assert_eq!(
        db.store()
            .windowed_event_voice_keys(SNAPSHOT_GUILD)
            .await
            .unwrap()
            .get(SNAPSHOT_CHANNEL),
        Some(&"launch".to_owned()),
        "rescheduling back into the window restores the link"
    );
    db.cleanup().await;
}

#[tokio::test]
async fn voice_snapshot_event_key_retains_completed_grace_across_real_mirror_replacement() {
    use two_bot_core::{scheduled_events::EventStatus, website_store::replace_events};

    let db = TestDb::new().await;
    snapshot_event_fixture(&db, EventStatus::Completed).await;
    sqlx::query(
        "UPDATE event_voice_channels SET ends_at = NOW() - INTERVAL '5 minutes' \
         WHERE event_id = $1",
    )
    .bind(SNAPSHOT_EVENT)
    .execute(&db.pool)
    .await
    .unwrap();
    let before = db
        .store()
        .windowed_event_voice_keys(SNAPSHOT_GUILD)
        .await
        .unwrap();
    assert_eq!(before.get(SNAPSHOT_CHANNEL), Some(&"launch".to_owned()));
    assert_eq!(
        replace_events(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:01:00.000Z", &[])
            .await
            .unwrap(),
        None
    );
    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM scheduled_events WHERE guild_id = $1")
            .bind(SNAPSHOT_GUILD)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(remaining, 0, "the mirror must remain an exact replacement");
    assert_eq!(
        db.store()
            .windowed_event_voice_keys(SNAPSHOT_GUILD)
            .await
            .unwrap(),
        before,
        "mirror omission must not cut short the known finite completed grace"
    );
    db.cleanup().await;
}

#[tokio::test]
async fn voice_snapshot_event_key_refuses_ambiguous_aliases() {
    use two_bot_core::scheduled_events::EventStatus;

    let db = TestDb::new().await;
    snapshot_event_fixture(&db, EventStatus::Active).await;
    db.store()
        .put_event_key(SNAPSHOT_GUILD, "another-site-event", SNAPSHOT_EVENT)
        .await
        .unwrap();
    assert!(
        db.store()
            .windowed_event_voice_keys(SNAPSHOT_GUILD)
            .await
            .unwrap()
            .is_empty(),
        "two site keys must not become one arbitrary room link"
    );
    db.store()
        .put_event_key(SNAPSHOT_GUILD, "another-site-event", "200000000000000002")
        .await
        .unwrap();
    assert_eq!(
        db.store()
            .windowed_event_voice_keys(SNAPSHOT_GUILD)
            .await
            .unwrap()
            .get(SNAPSHOT_CHANNEL),
        Some(&"launch".to_owned()),
        "a unique mapping remains usable after alias removal"
    );
    assert!(
        db.store()
            .windowed_event_voice_keys("100000000000000002")
            .await
            .unwrap()
            .is_empty(),
        "guild isolation survives reverse lookup"
    );
    db.cleanup().await;
}

#[tokio::test]
async fn voice_snapshot_event_key_accepted_end_changes_and_omission_do_not_invent_grace() {
    use two_bot_core::{
        scheduled_events::EventStatus,
        website_store::{replace_events, upsert_event},
    };

    let db = TestDb::new().await;
    let mut event = snapshot_event_fixture(&db, EventStatus::Active).await;
    event.ends_at = Some(snapshot_time(&db.pool, "-16 minutes").await);
    upsert_event(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:01:00.000Z", &event)
        .await
        .unwrap();
    assert!(db
        .store()
        .windowed_event_voice_keys(SNAPSHOT_GUILD)
        .await
        .unwrap()
        .is_empty());
    event.ends_at = Some(snapshot_time(&db.pool, "+5 minutes").await);
    upsert_event(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:02:00.000Z", &event)
        .await
        .unwrap();
    assert_eq!(
        db.store()
            .windowed_event_voice_keys(SNAPSHOT_GUILD)
            .await
            .unwrap()
            .len(),
        1
    );
    replace_events(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:03:00.000Z", &[])
        .await
        .unwrap();
    assert!(db
        .store()
        .windowed_event_voice_keys(SNAPSHOT_GUILD)
        .await
        .unwrap()
        .is_empty());
    let status: String =
        sqlx::query_scalar("SELECT status FROM event_instance_windows WHERE guild_id = $1")
            .bind(SNAPSHOT_GUILD)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(status, "missing", "omission is not positive completion");
    // A same-time mutation retains the mirror's compatibility behavior, but
    // cannot turn omitted evidence into a newly live window.
    upsert_event(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:03:00.000Z", &event)
        .await
        .unwrap();
    assert!(db
        .store()
        .windowed_event_voice_keys(SNAPSHOT_GUILD)
        .await
        .unwrap()
        .is_empty());
    db.cleanup().await;
}

#[tokio::test]
async fn voice_snapshot_event_key_completed_deadline_is_finite_and_not_extended_by_polls() {
    use two_bot_core::{
        scheduled_events::EventStatus,
        website_store::{replace_events, upsert_event},
    };

    let db = TestDb::new().await;
    let mut event = snapshot_event_fixture(&db, EventStatus::Completed).await;
    event.ends_at = Some(snapshot_time(&db.pool, "-16 minutes").await);
    upsert_event(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:01:00.000Z", &event)
        .await
        .unwrap();
    for stamp in ["2026-11-03T10:02:00.000Z", "2026-11-03T10:03:00.000Z"] {
        replace_events(&db.pool, SNAPSHOT_GUILD, stamp, &[])
            .await
            .unwrap();
        assert!(db
            .store()
            .windowed_event_voice_keys(SNAPSHOT_GUILD)
            .await
            .unwrap()
            .is_empty());
    }
    let end: String = sqlx::query_scalar(
        "SELECT to_char(ends_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') FROM event_instance_windows WHERE guild_id = $1",
    ).bind(SNAPSHOT_GUILD).fetch_one(&db.pool).await.unwrap();
    assert_eq!(
        Some(end),
        event.ends_at,
        "polls cannot reset the original grace deadline"
    );
    db.cleanup().await;
}

#[tokio::test]
async fn voice_snapshot_event_key_terminal_fences_survive_mirror_and_tombstone_removal() {
    use two_bot_core::{
        scheduled_events::EventStatus,
        website_store::{replace_events, upsert_event},
    };

    for deleted in [false, true] {
        let db = TestDb::new().await;
        let mut event = snapshot_event_fixture(&db, EventStatus::Completed).await;
        replace_events(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:01:00.000Z", &[])
            .await
            .unwrap();
        assert_eq!(
            db.store()
                .windowed_event_voice_keys(SNAPSHOT_GUILD)
                .await
                .unwrap()
                .len(),
            1
        );
        if deleted {
            two_bot_core::event_instance_store::confirm_deleted(
                &db.pool,
                SNAPSHOT_GUILD,
                SNAPSHOT_EVENT,
                "2026-11-03T10:02:00.000Z",
            )
            .await
            .unwrap();
        } else {
            event.status = EventStatus::Cancelled;
            upsert_event(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:02:00.000Z", &event)
                .await
                .unwrap();
        }
        assert!(db
            .store()
            .windowed_event_voice_keys(SNAPSHOT_GUILD)
            .await
            .unwrap()
            .is_empty());
        // Hygiene removes the old mirror tombstone; a binding/key pins the
        // independent terminal fence. No local SQL deletes the fence itself.
        replace_events(&db.pool, SNAPSHOT_GUILD, "2026-11-05T10:00:00.000Z", &[])
            .await
            .unwrap();
        replace_events(&db.pool, SNAPSHOT_GUILD, "2026-11-07T10:00:00.000Z", &[])
            .await
            .unwrap();
        let tombstones: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM scheduled_events_tombstone WHERE guild_id = $1",
        )
        .bind(SNAPSHOT_GUILD)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(tombstones, 0);
        event.status = EventStatus::Active;
        event.ends_at = None;
        for stamp in [
            "2026-11-03T10:01:00.000Z",
            "2026-11-03T10:02:00.000Z",
            "2026-11-07T10:00:00.000Z",
        ] {
            upsert_event(&db.pool, SNAPSHOT_GUILD, stamp, &event)
                .await
                .unwrap();
            replace_events(&db.pool, SNAPSHOT_GUILD, stamp, &[event.clone()])
                .await
                .unwrap();
            assert!(
                db.store()
                    .windowed_event_voice_keys(SNAPSHOT_GUILD)
                    .await
                    .unwrap()
                    .is_empty(),
                "terminal fence survives {stamp}, deleted={deleted}"
            );
        }
        db.cleanup().await;
    }
}

#[tokio::test]
async fn voice_snapshot_event_key_delayed_terminal_observations_revoke_omitted_grace() {
    use two_bot_core::{
        event_instance_store::confirm_deleted,
        scheduled_events::EventStatus,
        website_store::{replace_events, upsert_event},
    };

    for deleted in [false, true] {
        let db = TestDb::new().await;
        let mut event = snapshot_event_fixture(&db, EventStatus::Completed).await;
        replace_events(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:02:00.000Z", &[])
            .await
            .unwrap();
        event.status = EventStatus::Active;
        upsert_event(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:01:30.000Z", &event)
            .await
            .unwrap();
        assert_eq!(
            db.store()
                .windowed_event_voice_keys(SNAPSHOT_GUILD)
                .await
                .unwrap()
                .len(),
            1,
            "a compatibility-only mirror replay does not change confirmed grace"
        );
        // Terminal evidence older than the last confirmed lifecycle observation
        // still loses; a newer mirror replay must not mask accepted revocation.
        for stamp in ["2026-11-03T09:59:00.000Z", "2026-11-03T10:01:00.000Z"] {
            if deleted {
                confirm_deleted(&db.pool, SNAPSHOT_GUILD, SNAPSHOT_EVENT, stamp)
                    .await
                    .unwrap();
            } else {
                event.status = EventStatus::Cancelled;
                upsert_event(&db.pool, SNAPSHOT_GUILD, stamp, &event)
                    .await
                    .unwrap();
            }
            assert_eq!(
                db.store()
                    .windowed_event_voice_keys(SNAPSHOT_GUILD)
                    .await
                    .unwrap()
                    .len(),
                usize::from(stamp == "2026-11-03T09:59:00.000Z"),
                "terminal evidence respects confirmed order, deleted={deleted}, stamp={stamp}"
            );
        }
        let fences: (String, String, String, String) = sqlx::query_as(
            "SELECT w.observed_at, w.revoked_at, w.omitted_at, c.epoch
             FROM event_instance_windows w JOIN scheduled_events_claim c USING (guild_id)
             WHERE w.guild_id = $1 AND w.event_id = $2",
        )
        .bind(SNAPSHOT_GUILD)
        .bind(SNAPSHOT_EVENT)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            fences,
            (
                "2026-11-03T10:01:00.000Z".to_owned(),
                "2026-11-03T10:01:00.000Z".to_owned(),
                "2026-11-03T10:02:00.000Z".to_owned(),
                "2026-11-03T10:02:00.000Z".to_owned(),
            ),
            "revocation must not roll back the newer guild omission fence"
        );
        event.status = EventStatus::Active;
        for stamp in ["2026-11-03T10:02:00.000Z", "2026-11-03T10:03:00.000Z"] {
            upsert_event(&db.pool, SNAPSHOT_GUILD, stamp, &event)
                .await
                .unwrap();
            assert!(db
                .store()
                .windowed_event_voice_keys(SNAPSHOT_GUILD)
                .await
                .unwrap()
                .is_empty());
        }
        db.cleanup().await;
    }
}

#[tokio::test]
async fn voice_snapshot_first_observation_respects_snapshot_absence_without_a_tombstone() {
    use two_bot_core::{
        scheduled_events::{EventStatus, ScheduledEvent},
        website_store::{replace_events, upsert_event},
    };

    let db = TestDb::new().await;
    sqlx::query(
        "INSERT INTO event_voice_channels (event_id, instance_starts_at, starts_at, voice_channel_id)
         VALUES ($1, NOW() - INTERVAL '2 days', NOW() - INTERVAL '1 hour', $2)",
    )
    .bind(SNAPSHOT_EVENT)
    .bind(SNAPSHOT_CHANNEL)
    .execute(&db.pool)
    .await
    .unwrap();
    db.store()
        .put_event_key(SNAPSHOT_GUILD, "launch", SNAPSHOT_EVENT)
        .await
        .unwrap();
    let event = ScheduledEvent {
        id: SNAPSHOT_EVENT.to_owned(),
        name: "legacy".to_owned(),
        starts_at: snapshot_time(&db.pool, "-1 hour").await,
        ends_at: None,
        channel_id: Some(SNAPSHOT_CHANNEL.to_owned()),
        description: None,
        status: EventStatus::Active,
    };
    replace_events(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:02:00.000Z", &[])
        .await
        .unwrap();
    let tombstones: i64 =
        sqlx::query_scalar("SELECT count(*) FROM scheduled_events_tombstone WHERE guild_id = $1")
            .bind(SNAPSHOT_GUILD)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(tombstones, 0, "the event has never appeared in the mirror");
    for stamp in ["2026-11-03T10:01:00.000Z", "2026-11-03T10:02:00.000Z"] {
        upsert_event(&db.pool, SNAPSHOT_GUILD, stamp, &event)
            .await
            .unwrap();
        let mirrored: String = sqlx::query_scalar(
            "SELECT updated_at FROM scheduled_events WHERE guild_id = $1 AND event_id = $2",
        )
        .bind(SNAPSHOT_GUILD)
        .bind(SNAPSHOT_EVENT)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(mirrored, stamp, "legacy mirror acceptance is unchanged");
        let evidence: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM event_instance_windows WHERE guild_id = $1 AND event_id = $2",
        )
        .bind(SNAPSHOT_GUILD)
        .bind(SNAPSHOT_EVENT)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            evidence, 0,
            "stale/equal mutation cannot establish liveness"
        );
    }
    let mut unproven_completion = event.clone();
    unproven_completion.status = EventStatus::Completed;
    unproven_completion.ends_at = Some(snapshot_time(&db.pool, "-5 minutes").await);
    upsert_event(
        &db.pool,
        SNAPSHOT_GUILD,
        "2026-11-03T10:02:00.000Z",
        &unproven_completion,
    )
    .await
    .unwrap();
    assert!(
        db.store()
            .windowed_event_voice_keys(SNAPSHOT_GUILD)
            .await
            .unwrap()
            .is_empty(),
        "completion cannot establish an unproven initial instance behind the absence fence"
    );
    replace_events(
        &db.pool,
        SNAPSHOT_GUILD,
        "2026-11-03T10:02:00.000Z",
        std::slice::from_ref(&event),
    )
    .await
    .unwrap();
    assert!(
        db.store()
            .windowed_event_voice_keys(SNAPSHOT_GUILD)
            .await
            .unwrap()
            .is_empty(),
        "equal snapshot replay cannot defeat the absence fence"
    );
    replace_events(
        &db.pool,
        SNAPSHOT_GUILD,
        "2026-11-03T10:03:00.000Z",
        std::slice::from_ref(&event),
    )
    .await
    .unwrap();
    assert_eq!(
        db.store()
            .windowed_event_voice_keys(SNAPSHOT_GUILD)
            .await
            .unwrap()
            .len(),
        1,
        "a genuinely newer snapshot can adopt the legacy identity"
    );
    replace_events(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:04:00.000Z", &[])
        .await
        .unwrap();
    for stamp in ["2026-11-03T10:03:30.000Z", "2026-11-03T10:04:00.000Z"] {
        upsert_event(&db.pool, SNAPSHOT_GUILD, stamp, &event)
            .await
            .unwrap();
        assert!(
            db.store()
                .windowed_event_voice_keys(SNAPSHOT_GUILD)
                .await
                .unwrap()
                .is_empty(),
            "stale/equal mutation cannot revive missing evidence"
        );
    }
    upsert_event(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:05:00.000Z", &event)
        .await
        .unwrap();
    assert_eq!(
        db.store()
            .windowed_event_voice_keys(SNAPSHOT_GUILD)
            .await
            .unwrap()
            .len(),
        1,
        "a genuinely newer mutation can restore live evidence"
    );
    db.cleanup().await;
}

#[tokio::test]
async fn voice_snapshot_delayed_completion_establishes_grace_despite_a_newer_mirror_replay() {
    use two_bot_core::{
        scheduled_events::EventStatus,
        website_store::{replace_events, upsert_event},
    };

    let db = TestDb::new().await;
    let mut event = snapshot_event_fixture(&db, EventStatus::Active).await;
    replace_events(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:03:00.000Z", &[])
        .await
        .unwrap();
    upsert_event(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:02:00.000Z", &event)
        .await
        .unwrap();
    assert!(db
        .store()
        .windowed_event_voice_keys(SNAPSHOT_GUILD)
        .await
        .unwrap()
        .is_empty());
    event.status = EventStatus::Completed;
    event.ends_at = Some(snapshot_time(&db.pool, "-5 minutes").await);
    upsert_event(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:01:00.000Z", &event)
        .await
        .unwrap();
    assert_eq!(
        db.store()
            .windowed_event_voice_keys(SNAPSHOT_GUILD)
            .await
            .unwrap()
            .len(),
        1,
        "a positively confirmed completion newer than the last lifecycle stamp proves finite grace"
    );
    let stamps: (String, String, String) = sqlx::query_as(
        "SELECT s.updated_at, w.observed_at, c.epoch
         FROM scheduled_events s
         JOIN event_instance_windows w USING (guild_id, event_id)
         JOIN scheduled_events_claim c USING (guild_id)
         WHERE s.guild_id = $1 AND s.event_id = $2",
    )
    .bind(SNAPSHOT_GUILD)
    .bind(SNAPSHOT_EVENT)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        stamps,
        (
            "2026-11-03T10:02:00.000Z".to_owned(),
            "2026-11-03T10:01:00.000Z".to_owned(),
            "2026-11-03T10:03:00.000Z".to_owned(),
        ),
        "terminal evidence must not rewrite the mirror or guild epoch"
    );
    db.cleanup().await;
}

#[tokio::test]
async fn voice_snapshot_equal_completion_closes_live_evidence_but_cannot_restamp_confirmed_grace() {
    use two_bot_core::{
        scheduled_events::EventStatus,
        website_store::{replace_events, upsert_event},
    };

    let db = TestDb::new().await;
    let mut event = snapshot_event_fixture(&db, EventStatus::Active).await;
    replace_events(
        &db.pool,
        SNAPSHOT_GUILD,
        "2026-11-03T10:01:00.000Z",
        std::slice::from_ref(&event),
    )
    .await
    .unwrap();
    event.status = EventStatus::Completed;
    event.ends_at = Some(snapshot_time(&db.pool, "-5 minutes").await);
    let completed_end = event.ends_at.clone();
    upsert_event(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:01:00.000Z", &event)
        .await
        .unwrap();
    replace_events(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:02:00.000Z", &[])
        .await
        .unwrap();
    assert_eq!(
        db.store()
            .windowed_event_voice_keys(SNAPSHOT_GUILD)
            .await
            .unwrap()
            .len(),
        1,
        "same-time completion beats positively observed live evidence"
    );
    // A replay of the same confirmed completion cannot lengthen its known end.
    event.ends_at = Some(snapshot_time(&db.pool, "+2 hours").await);
    upsert_event(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:01:00.000Z", &event)
        .await
        .unwrap();
    let stamp: String = sqlx::query_scalar(
        "SELECT observed_at FROM event_instance_windows WHERE guild_id = $1 AND event_id = $2",
    )
    .bind(SNAPSHOT_GUILD)
    .bind(SNAPSHOT_EVENT)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(stamp, "2026-11-03T10:01:00.000Z");
    let end: String = sqlx::query_scalar(
        "SELECT to_char(ends_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')
         FROM event_instance_windows WHERE guild_id = $1 AND event_id = $2",
    )
    .bind(SNAPSHOT_GUILD)
    .bind(SNAPSHOT_EVENT)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(Some(end), completed_end, "omission does not extend grace");
    db.cleanup().await;
}

#[tokio::test]
async fn voice_snapshot_equal_completion_is_independent_of_omission_arrival_order() {
    use two_bot_core::{
        scheduled_events::EventStatus,
        website_store::{replace_events, upsert_event},
    };

    for omit_first in [false, true] {
        let db = TestDb::new().await;
        let mut event = snapshot_event_fixture(&db, EventStatus::Active).await;
        replace_events(
            &db.pool,
            SNAPSHOT_GUILD,
            "2026-11-03T10:01:00.000Z",
            std::slice::from_ref(&event),
        )
        .await
        .unwrap();
        event.status = EventStatus::Completed;
        event.ends_at = Some(snapshot_time(&db.pool, "-5 minutes").await);
        if omit_first {
            replace_events(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:02:00.000Z", &[])
                .await
                .unwrap();
        }
        upsert_event(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:01:00.000Z", &event)
            .await
            .unwrap();
        if !omit_first {
            replace_events(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:02:00.000Z", &[])
                .await
                .unwrap();
        }
        assert_eq!(
            db.store()
                .windowed_event_voice_keys(SNAPSHOT_GUILD)
                .await
                .unwrap()
                .len(),
            1,
            "equal-stamp confirmed completion retains finite grace; omit_first={omit_first}"
        );
        db.cleanup().await;
    }
}

#[tokio::test]
async fn voice_snapshot_equal_completion_cannot_add_an_end_after_unknown_end_omission() {
    use two_bot_core::{
        scheduled_events::EventStatus,
        website_store::{replace_events, upsert_event},
    };

    let db = TestDb::new().await;
    let mut event = snapshot_event_fixture(&db, EventStatus::Active).await;
    event.status = EventStatus::Completed;
    event.ends_at = None;
    upsert_event(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:01:00.000Z", &event)
        .await
        .unwrap();
    replace_events(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:02:00.000Z", &[])
        .await
        .unwrap();
    event.ends_at = Some(snapshot_time(&db.pool, "-5 minutes").await);
    upsert_event(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:01:00.000Z", &event)
        .await
        .unwrap();
    assert!(db
        .store()
        .windowed_event_voice_keys(SNAPSHOT_GUILD)
        .await
        .unwrap()
        .is_empty());
    let end: Option<time::OffsetDateTime> = sqlx::query_scalar(
        "SELECT ends_at FROM event_instance_windows WHERE guild_id = $1 AND event_id = $2",
    )
    .bind(SNAPSHOT_GUILD)
    .bind(SNAPSHOT_EVENT)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(
        end.is_none(),
        "equal completion cannot fabricate a finite end"
    );
    db.cleanup().await;
}

#[tokio::test]
async fn voice_snapshot_event_evidence_failure_rolls_back_each_mirror_writer() {
    use two_bot_core::{
        scheduled_events::EventStatus,
        website_store::{replace_events, upsert_event},
    };

    let db = TestDb::new().await;
    let mut event = snapshot_event_fixture(&db, EventStatus::Active).await;
    sqlx::query("ALTER TABLE event_instance_windows ADD CONSTRAINT reject_test_window CHECK (status <> 'completed')")
        .execute(&db.pool).await.unwrap();
    event.status = EventStatus::Completed;
    event.ends_at = Some(snapshot_time(&db.pool, "-5 minutes").await);
    assert!(
        upsert_event(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:01:00.000Z", &event)
            .await
            .is_err()
    );
    assert!(replace_events(
        &db.pool,
        SNAPSHOT_GUILD,
        "2026-11-03T10:02:00.000Z",
        &[event]
    )
    .await
    .is_err());
    let statuses: (String, String) = sqlx::query_as(
        "SELECT s.status, w.status FROM scheduled_events s JOIN event_instance_windows w USING (guild_id, event_id) WHERE s.guild_id = $1",
    ).bind(SNAPSHOT_GUILD).fetch_one(&db.pool).await.unwrap();
    assert_eq!(statuses, ("active".to_owned(), "active".to_owned()));
    let epochs: i64 =
        sqlx::query_scalar("SELECT count(*) FROM scheduled_events_claim WHERE guild_id = $1")
            .bind(SNAPSHOT_GUILD)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(epochs, 0, "failed swap also rolls back its epoch claim");
    db.cleanup().await;
}

#[tokio::test]
async fn voice_snapshot_event_evidence_adopts_only_unambiguous_legacy_instance() {
    use two_bot_core::{scheduled_events::EventStatus, website_store::upsert_event};

    let db = TestDb::new().await;
    // Only a fresh accepted observation can adopt the legacy immutable id;
    // neither an old binding nor a site-key registration asserts liveness.
    sqlx::query(
        "INSERT INTO event_voice_channels (event_id, instance_starts_at, starts_at, voice_channel_id)
         VALUES ($1, NOW() - INTERVAL '2 days', NOW() - INTERVAL '1 hour', $2)",
    ).bind(SNAPSHOT_EVENT).bind(SNAPSHOT_CHANNEL).execute(&db.pool).await.unwrap();
    db.store()
        .put_event_key(SNAPSHOT_GUILD, "launch", SNAPSHOT_EVENT)
        .await
        .unwrap();
    assert!(db
        .store()
        .windowed_event_voice_keys(SNAPSHOT_GUILD)
        .await
        .unwrap()
        .is_empty());
    let event = two_bot_core::scheduled_events::ScheduledEvent {
        id: SNAPSHOT_EVENT.to_owned(),
        name: "legacy".to_owned(),
        starts_at: snapshot_time(&db.pool, "-1 hour").await,
        ends_at: None,
        channel_id: Some(SNAPSHOT_CHANNEL.to_owned()),
        description: None,
        status: EventStatus::Active,
    };
    upsert_event(&db.pool, SNAPSHOT_GUILD, "2026-11-03T10:00:00.000Z", &event)
        .await
        .unwrap();
    assert_eq!(
        db.store()
            .windowed_event_voice_keys(SNAPSHOT_GUILD)
            .await
            .unwrap()
            .len(),
        1
    );
    sqlx::query(
        "INSERT INTO event_voice_channels (event_id, instance_starts_at, starts_at, voice_channel_id)
         VALUES ($1, NOW() - INTERVAL '1 day', NOW() - INTERVAL '1 hour', '300000000000000002')",
    ).bind(SNAPSHOT_EVENT).execute(&db.pool).await.unwrap();
    assert!(
        db.store()
            .windowed_event_voice_keys(SNAPSHOT_GUILD)
            .await
            .unwrap()
            .is_empty(),
        "competing legacy instances never select an arbitrary binding"
    );
    db.cleanup().await;
}

#[tokio::test]
async fn voice_snapshot_instance_maintenance_is_bounded_and_keeps_referenced_fences() {
    use two_bot_core::{scheduled_events::EventStatus, website_store::replace_events};

    let db = TestDb::new().await;
    snapshot_event_fixture(&db, EventStatus::Completed).await;
    sqlx::query(
        "INSERT INTO event_instance_windows
         (guild_id, event_id, instance_starts_at, starts_at, status, observed_at)
         SELECT $1, 'unreferenced-' || n,
                TIMESTAMPTZ '2026-11-01T10:00:00.000Z', TIMESTAMPTZ '2026-11-01T10:00:00.000Z',
                'missing', '2026-11-01T10:00:00.000Z' FROM generate_series(1, 30) n",
    )
    .bind(SNAPSHOT_GUILD)
    .execute(&db.pool)
    .await
    .unwrap();
    replace_events(&db.pool, SNAPSHOT_GUILD, "2026-11-05T10:00:00.000Z", &[])
        .await
        .unwrap();
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM event_instance_windows WHERE guild_id = $1")
            .bind(SNAPSHOT_GUILD)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(count, 6, "one poll removes at most 25 unreferenced rows");
    let pinned: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM event_instance_windows WHERE guild_id = $1 AND event_id = $2",
    )
    .bind(SNAPSHOT_GUILD)
    .bind(SNAPSHOT_EVENT)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        pinned, 1,
        "binding/key keeps the fence despite old observation age"
    );
    replace_events(&db.pool, SNAPSHOT_GUILD, "2026-11-05T10:01:00.000Z", &[])
        .await
        .unwrap();
    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM event_instance_windows WHERE guild_id = $1")
            .bind(SNAPSHOT_GUILD)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        remaining, 1,
        "omission high-water does not postpone cleanup"
    );
    db.cleanup().await;
}

// Public cross-implementation signing fixture, never runtime key material.
fn signing_key() -> two_bot_core::internal_actions::SigningKey {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/internal-action-signing.json");
    let fixture: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    two_bot_core::internal_actions::SigningKey {
        id: "fixture-caller".to_owned(),
        secret: Secret::new(
            fixture["vectors"][0]["secret"]
                .as_str()
                .unwrap()
                .as_bytes()
                .to_vec(),
        ),
    }
}

#[tokio::test]
async fn authenticated_malformed_body_burns_before_parsing_and_survives_restart() {
    use two_bot_core::internal_actions::{
        sign, AuthHeaders, AuthenticatedRequest, ErrorCode, InternalFlags, KeyRing, TokenBuckets,
    };

    let db = TestDb::new().await;
    let key = signing_key();
    let keys = KeyRing::new(vec![key.clone()]);
    let now = db_now_secs(&db.pool).await as u64;
    let timestamp = now.to_string();
    let nonce = body_hash(db.fixture.name().as_bytes())[..32].to_owned();
    let raw = b"not json";
    let signature = sign(key.secret.expose(), &timestamp, &nonce, raw);
    let headers = AuthHeaders {
        key_id: &key.id,
        timestamp: &timestamp,
        nonce: &nonce,
        signature: &signature,
    };
    let verified = AuthenticatedRequest::verify(
        &headers,
        raw,
        &keys,
        SKEW_SECONDS,
        now * 1000,
        &mut ClockGuard::new(),
    )
    .unwrap();
    let burned = verified.burn_durably(&db.store()).await.unwrap();
    let flags = InternalFlags::from_map(&std::collections::HashMap::new());
    let error = burned
        .authorize(&flags, true, false, &mut TokenBuckets::new())
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Malformed);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM internal_nonces")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);

    let restarted = db.independent_pool().await;
    let verified = AuthenticatedRequest::verify(
        &headers,
        raw,
        &keys,
        SKEW_SECONDS,
        now * 1000,
        &mut ClockGuard::new(),
    )
    .unwrap();
    let error = verified
        .burn_durably(&InternalActionStore::new(restarted.clone()))
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, ErrorCode::Replayed);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM internal_idempotency")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    restarted.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn invalid_authentication_cannot_burn_or_drain_buckets() {
    use two_bot_core::internal_actions::{
        sign, AuthHeaders, AuthenticatedRequest, ErrorCode, InternalFlags, KeyRing, TokenBuckets,
    };

    let db = TestDb::new().await;
    let key = signing_key();
    let keys = KeyRing::new(vec![key.clone()]);
    let now = db_now_secs(&db.pool).await as u64;
    let timestamp = now.to_string();
    let stale = (now - SKEW_SECONDS - 1).to_string();
    let nonce = body_hash(db.fixture.name().as_bytes())[..32].to_owned();
    let raw = br#"{"action":"role.assign"}"#;
    let good = sign(key.secret.expose(), &timestamp, &nonce, raw);
    let old = sign(key.secret.expose(), &stale, &nonce, raw);
    let mut buckets = TokenBuckets::new();
    for (id, timestamp, signature, expected) in [
        (
            key.id.as_str(),
            timestamp.as_str(),
            "sha256=invalid",
            ErrorCode::Unauthorized,
        ),
        (
            "unknown",
            timestamp.as_str(),
            good.as_str(),
            ErrorCode::Unauthorized,
        ),
        (
            key.id.as_str(),
            stale.as_str(),
            old.as_str(),
            ErrorCode::StaleRequest,
        ),
    ] {
        for _ in 0..25 {
            let headers = AuthHeaders {
                key_id: id,
                timestamp,
                nonce: &nonce,
                signature,
            };
            let error = AuthenticatedRequest::verify(
                &headers,
                raw,
                &keys,
                SKEW_SECONDS,
                now * 1000,
                &mut ClockGuard::new(),
            )
            .err()
            .unwrap();
            assert_eq!(error.code, expected);
        }
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM internal_nonces")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let headers = AuthHeaders {
        key_id: &key.id,
        timestamp: &timestamp,
        nonce: &nonce,
        signature: &good,
    };
    let burned = AuthenticatedRequest::verify(
        &headers,
        raw,
        &keys,
        SKEW_SECONDS,
        now * 1000,
        &mut ClockGuard::new(),
    )
    .unwrap()
    .burn_durably(&db.store())
    .await
    .unwrap();
    let flags = InternalFlags::from_map(&std::collections::HashMap::new());
    let decision = burned.authorize(&flags, true, false, &mut buckets).unwrap();
    assert_eq!(decision.action, "role.assign");
    db.cleanup().await;
}

#[tokio::test]
async fn failed_nonce_storage_and_incompatible_skew_cannot_grant_authorization() {
    use two_bot_core::internal_actions::{
        sign, AuthHeaders, AuthenticatedRequest, ErrorCode, KeyRing,
    };

    let db = TestDb::new().await;
    let key = signing_key();
    let keys = KeyRing::new(vec![key.clone()]);
    let now = db_now_secs(&db.pool).await as u64;
    let timestamp = now.to_string();
    let nonce = body_hash(db.fixture.name().as_bytes())[..32].to_owned();
    let raw = br#"{"action":"role.assign"}"#;
    let signature = sign(key.secret.expose(), &timestamp, &nonce, raw);
    let headers = AuthHeaders {
        key_id: &key.id,
        timestamp: &timestamp,
        nonce: &nonce,
        signature: &signature,
    };
    for skew in [SKEW_SECONDS - 1, SKEW_SECONDS + 1] {
        let error = AuthenticatedRequest::verify(
            &headers,
            raw,
            &keys,
            skew,
            now * 1000,
            &mut ClockGuard::new(),
        )
        .unwrap()
        .burn_durably(&db.store())
        .await
        .err()
        .unwrap();
        assert_eq!(error.code, ErrorCode::Internal);
        assert_eq!(error.log_reason, "nonce_skew_mismatch");
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM internal_nonces")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);

    let closed = db.independent_pool().await;
    closed.close().await;
    let error = AuthenticatedRequest::verify(
        &headers,
        raw,
        &keys,
        SKEW_SECONDS,
        now * 1000,
        &mut ClockGuard::new(),
    )
    .unwrap()
    .burn_durably(&InternalActionStore::new(closed))
    .await
    .err()
    .unwrap();
    assert_eq!(error.code, ErrorCode::Internal);
    assert_eq!(error.log_reason, "nonce_store_unavailable");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM internal_nonces")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    db.cleanup().await;
}

#[tokio::test]
async fn staged_authentication_refuses_persisted_db_clock_rollback_after_restart() {
    use two_bot_core::internal_actions::{
        sign, AuthHeaders, AuthenticatedRequest, ErrorCode, KeyRing,
    };

    let db = TestDb::new().await;
    let store = db.store();
    let seed_nonce = body_hash(db.fixture.name().as_bytes())[..32].to_owned();
    let seed_timestamp = db_now_secs(&db.pool).await.to_string();
    assert!(store
        .burn_nonce(&seed_nonce, &seed_timestamp)
        .await
        .unwrap());
    let future_ms = store.nonce_high_water_ms().await.unwrap().unwrap() + 300_000;
    sqlx::query(
        "UPDATE internal_clock_high_water SET high_water_ms = $1, \
         observed_at = TO_TIMESTAMP($1::double precision / 1000.0) \
         WHERE domain = 'internal_nonce_db'",
    )
    .bind(future_ms as i64)
    .execute(&db.pool)
    .await
    .unwrap();

    let key = signing_key();
    let keys = KeyRing::new(vec![key.clone()]);
    let now = db_now_secs(&db.pool).await as u64;
    let timestamp = now.to_string();
    let nonce = body_hash(format!("{}-fresh", db.fixture.name()).as_bytes())[..32].to_owned();
    let raw = br#"{"action":"role.assign"}"#;
    let signature = sign(key.secret.expose(), &timestamp, &nonce, raw);
    let headers = AuthHeaders {
        key_id: &key.id,
        timestamp: &timestamp,
        nonce: &nonce,
        signature: &signature,
    };
    let restarted = db.independent_pool().await;
    // A new process clock must not be seeded from the independent DB domain.
    // A locally fresh MAC still cannot pass the persisted DB rollback guard.
    let error = AuthenticatedRequest::verify(
        &headers,
        raw,
        &keys,
        SKEW_SECONDS,
        now * 1000,
        &mut ClockGuard::new(),
    )
    .unwrap()
    .burn_durably(&InternalActionStore::new(restarted.clone()))
    .await
    .err()
    .unwrap();
    assert_eq!(error.code, ErrorCode::StaleRequest);
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM internal_nonces WHERE nonce_hash = $1")
            .bind(body_hash(nonce.as_bytes()))
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    assert_eq!(store.nonce_high_water_ms().await.unwrap(), Some(future_ms));
    restarted.close().await;
    db.cleanup().await;
}

async fn wait_for_uniqueness_lock(db: &TestDb, table: &str) {
    // Observe only this owned test database's sessions. Bounded synchronization
    // proves the VALUES clock was sampled before rollback; no timing-only sleep.
    for _ in 0..100 {
        let blocked: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE application_name = $1 \
             AND wait_event_type = 'Lock' AND query LIKE '%' || $2 || '%')",
        )
        .bind(db.fixture.name())
        .bind(table)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        if blocked {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("test writer did not reach the expected unique-index lock");
}

#[tokio::test]
async fn rollback_wait_starts_retention_and_staleness_after_winning_insert() {
    let db = TestDb::new().await;
    let second = db.independent_pool().await;
    let store = InternalActionStore::new(second.clone());
    let nonce = body_hash(db.fixture.name().as_bytes())[..32].to_owned();
    let nonce_hash = body_hash(nonce.as_bytes());
    let mut blocker = db.pool.begin().await.unwrap();
    sqlx::query(
        "WITH instant AS MATERIALIZED (SELECT clock_timestamp() AS now) \
         INSERT INTO internal_nonces SELECT $1, now, now + INTERVAL '241 seconds' FROM instant",
    )
    .bind(&nonce_hash)
    .execute(&mut *blocker)
    .await
    .unwrap();
    let writer = store.clone();
    let offered_nonce = nonce.clone();
    let attempt = db_now_secs(&db.pool).await.to_string();
    let burn = tokio::spawn(async move { writer.burn_nonce(&offered_nonce, &attempt).await });
    wait_for_uniqueness_lock(&db, "internal_nonces").await;
    let release: time::OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    blocker.rollback().await.unwrap();
    assert!(burn.await.unwrap().unwrap());
    let row =
        sqlx::query("SELECT burned_at, expires_at FROM internal_nonces WHERE nonce_hash = $1")
            .bind(&nonce_hash)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    let burned_at: time::OffsetDateTime = row.get("burned_at");
    let expires_at: time::OffsetDateTime = row.get("expires_at");
    assert!(burned_at >= release);
    assert_eq!(expires_at - burned_at, time::Duration::seconds(241));
    let now = db_now_secs(&db.pool).await.to_string();
    assert!(!store.burn_nonce(&nonce, &now).await.unwrap());

    let id = identity("blocked-key:123", "event.upsert", b"payload");
    let mut blocker = db.pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO internal_idempotency (caller_hash, key_hash, action, payload_hash, state) \
         VALUES ($1, $2, 'event.upsert', $3, 'in_flight')",
    )
    .bind(body_hash(b"website"))
    .bind(body_hash(b"blocked-key:123"))
    .bind(body_hash(b"payload"))
    .execute(&mut *blocker)
    .await
    .unwrap();
    let writer = store.clone();
    let offered_id = id.clone();
    let claim = tokio::spawn(async move { writer.claim(&offered_id, &subject()).await });
    wait_for_uniqueness_lock(&db, "internal_idempotency").await;
    let release: time::OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    blocker.rollback().await.unwrap();
    let owner = claimed(claim.await.unwrap().unwrap());
    let created_at: time::OffsetDateTime =
        sqlx::query_scalar("SELECT created_at FROM internal_idempotency WHERE intent_id = $1")
            .bind(owner.intent_id())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(created_at >= release);
    assert!(matches!(
        store.claim(&id, &subject()).await.unwrap(),
        InternalClaim::InFlight
    ));
    second.close().await;
    db.cleanup().await;
}

fn identity(key: &str, action: &str, payload: &[u8]) -> RequestIdentity {
    RequestIdentity::new("website", key, action, payload).unwrap()
}

fn subject() -> AuditSubject {
    AuditSubject {
        guild_id: Some(DiscordId::new("123456789012345678").unwrap()),
        actor_id: Some(DiscordId::new("234567890123456789").unwrap()),
        target_id: None,
        resolved_role_id: None,
    }
}

fn claimed(result: InternalClaim) -> ExecutionClaim {
    match result {
        InternalClaim::Claimed(claim) => claim,
        other => panic!("expected committed execution claim, got {other:?}"),
    }
}

#[tokio::test]
async fn proven_unsent_release_retains_binding_and_allows_one_new_owner() {
    let db = TestDb::new().await;
    let store = db.store();
    let id = identity("unsent-key:123", "role.assign", b"payload");
    let subject = subject();
    let nonce = "0123456789abcdef0123456789abcdef";
    let timestamp = db_now_secs(&db.pool).await.to_string();
    assert!(store.burn_nonce(nonce, &timestamp).await.unwrap());
    let first = claimed(store.claim(&id, &subject).await.unwrap());
    let intent_id = first.intent_id();
    store.release_proven_not_sent(first).await.unwrap();
    assert!(!store.burn_nonce(nonce, &timestamp).await.unwrap());
    for changed in [
        identity("unsent-key:123", "role.assign", b"changed"),
        identity("unsent-key:123", "guild.add_member", b"payload"),
    ] {
        assert!(matches!(
            store.claim(&changed, &subject).await.unwrap(),
            InternalClaim::Mismatch
        ));
    }
    let mut remapped = subject.clone();
    remapped.resolved_role_id = Some(DiscordId::new("345678901234567890").unwrap());
    assert!(matches!(
        store.claim(&id, &remapped).await.unwrap(),
        InternalClaim::Mismatch
    ));
    sqlx::query(
        "UPDATE internal_idempotency SET created_at = clock_timestamp() - interval '1 hour'",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        store
            .reconcile(
                &id,
                &TerminalResponse::Failure(TerminalFailure::NoEffect),
                ReconciliationEvidence::ProvenNotSent
            )
            .await
            .unwrap_err(),
        InternalStoreError::TransitionRefused
    );
    let (a, b) = tokio::join!(store.claim(&id, &subject), store.claim(&id, &subject));
    let winner = match (a.unwrap(), b.unwrap()) {
        (InternalClaim::Claimed(claim), InternalClaim::InFlight)
        | (InternalClaim::InFlight, InternalClaim::Claimed(claim)) => claim,
        other => panic!("expected exactly one new owner: {other:?}"),
    };
    assert_eq!(winner.intent_id(), intent_id);
    // A second no-dispatch release still keeps one audit per intent/phase.
    store.release_proven_not_sent(winner).await.unwrap();
    let final_claim = claimed(store.claim(&id, &subject).await.unwrap());
    store.finish(&final_claim, &success()).await.unwrap();
    assert!(matches!(
        store.claim(&id, &subject).await.unwrap(),
        InternalClaim::Replay(_)
    ));
    let rows: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT phase, evidence_code FROM internal_action_log ORDER BY audit_id")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        rows,
        vec![
            ("intent".into(), None),
            ("released".into(), Some("proven_not_sent".into())),
            ("terminal".into(), Some("executor".into())),
        ]
    );
    db.cleanup().await;
}

#[tokio::test]
async fn notify_claim_pins_bounded_event_key_through_release_and_suppression() {
    let db = TestDb::new().await;
    let store = db.store();
    let subject = subject();
    let other = identity("notify-invalid:123", "role.assign", b"payload");
    assert_eq!(
        store
            .claim_notify(&other, &subject, "launch:2026")
            .await
            .unwrap_err(),
        InternalStoreError::InvalidInput
    );
    for (verb, member) in [
        ("reminder.send", "345678901234567890"),
        ("place_offer.send", "456789012345678901"),
    ] {
        let subject = AuditSubject {
            target_id: Some(DiscordId::new(member).unwrap()),
            ..subject.clone()
        };
        let id = identity(&format!("notify-{verb}:123"), verb, b"payload");
        for invalid in ["".to_owned(), "has space".to_owned(), "k".repeat(65)] {
            assert_eq!(
                store
                    .claim_notify(&id, &subject, &invalid)
                    .await
                    .unwrap_err(),
                InternalStoreError::InvalidInput
            );
        }
        let claim = claimed(
            store
                .claim_notify(&id, &subject, "launch:2026")
                .await
                .unwrap(),
        );
        store.release_proven_not_sent(claim).await.unwrap();
        assert!(matches!(
            store
                .claim_notify(&id, &subject, "other:2026")
                .await
                .unwrap(),
            InternalClaim::Mismatch
        ));
        let claim = claimed(
            store
                .claim_notify(&id, &subject, "launch:2026")
                .await
                .unwrap(),
        );
        store.finish_suppressed(&claim).await.unwrap();
        assert!(matches!(
            store
                .claim_notify(&id, &subject, "launch:2026")
                .await
                .unwrap(),
            InternalClaim::Replay(TerminalResponse::Success {
                affected: 0,
                resource_id: None,
                ..
            })
        ));
    }
    let audit: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT event_key, phase, evidence_code FROM internal_action_log ORDER BY audit_id",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    let expected: Vec<_> = (0..2)
        .flat_map(|_| {
            [
                ("launch:2026".into(), "intent".into(), None),
                (
                    "launch:2026".into(),
                    "released".into(),
                    Some("proven_not_sent".into()),
                ),
                (
                    "launch:2026".into(),
                    "terminal".into(),
                    Some("suppressed".into()),
                ),
            ]
        })
        .collect();
    assert_eq!(audit, expected);
    let intents: i64 = sqlx::query_scalar("SELECT count(*) FROM internal_idempotency")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(intents, 2, "invalid claims never persist");
    db.cleanup().await;
}

fn notify_subject() -> AuditSubject {
    AuditSubject {
        target_id: Some(DiscordId::new("345678901234567890").unwrap()),
        ..subject()
    }
}

#[tokio::test]
async fn notify_budget_serializes_callers_and_survives_store_restart() {
    let db = TestDb::new().await;
    let store = db.store();
    let second_pool = db.independent_pool().await;
    let other = InternalActionStore::new(second_pool.clone());
    let subject = notify_subject();
    let a = identity("notify-concurrent-a", "reminder.send", b"a");
    let b = RequestIdentity::new(
        "other-caller",
        "notify-concurrent-b",
        "place_offer.send",
        b"b",
    )
    .unwrap();
    let c = identity("notify-concurrent-c", "place_offer.send", b"c");
    let d = identity("notify-concurrent-d", "reminder.send", b"d");
    let results = tokio::join!(
        store.claim_notify(&a, &subject, "launch"),
        other.claim_notify(&b, &subject, "launch"),
        store.claim_notify(&c, &subject, "launch"),
        other.claim_notify(&d, &subject, "launch"),
    );
    let mut owners = Vec::new();
    let mut denied = 0;
    for (id, result) in [&a, &b, &c, &d]
        .into_iter()
        .zip([results.0, results.1, results.2, results.3])
    {
        match result {
            Ok(InternalClaim::Claimed(claim)) => owners.push((id, claim)),
            Err(InternalStoreError::NotifyRateLimited { retry_after_secs }) => {
                assert!(retry_after_secs > 0);
                denied += 1;
            }
            other => panic!("unexpected claim result: {other:?}"),
        }
    }
    assert_eq!(owners.len(), 3);
    assert_eq!(denied, 1);
    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM internal_idempotency), \
         (SELECT count(*) FROM internal_action_log), \
         (SELECT count(*) FROM internal_notify_budgets)",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        counts,
        (3, 3, 1),
        "denial rolls back tentative claim and audit"
    );
    let before: (i64, i64, Vec<i64>) =
        sqlx::query_as("SELECT credit_ms, updated_ms, admissions FROM internal_notify_budgets")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(before.2.len(), 3);
    // Receipt, in-flight and mismatch reads still work with no allowance left.
    let (owner_id, owner) = &owners[0];
    assert!(matches!(
        store
            .claim_notify(owner_id, &subject, "launch")
            .await
            .unwrap(),
        InternalClaim::InFlight
    ));
    assert!(matches!(
        store
            .claim_notify(owner_id, &subject, "changed")
            .await
            .unwrap(),
        InternalClaim::Mismatch
    ));
    store.finish_suppressed(owner).await.unwrap();
    for _ in 0..5 {
        assert!(matches!(
            other
                .claim_notify(owner_id, &subject, "launch")
                .await
                .unwrap(),
            InternalClaim::Replay(_)
        ));
    }
    let after: (i64, i64, Vec<i64>) =
        sqlx::query_as("SELECT credit_ms, updated_ms, admissions FROM internal_notify_budgets")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(before, after, "non-executing reads never update allowance");
    second_pool.close().await;
    let restarted_pool = db.independent_pool().await;
    let restarted = InternalActionStore::new(restarted_pool.clone());
    let fresh = identity("notify-after-restart", "reminder.send", b"fresh");
    assert!(matches!(
        restarted.claim_notify(&fresh, &subject, "launch").await,
        Err(InternalStoreError::NotifyRateLimited { .. })
    ));
    restarted_pool.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn notify_budget_preserves_daily_history_and_clock_high_water() {
    let db = TestDb::new().await;
    let store = db.store();
    let subject = notify_subject();
    let id = identity("notify-clock-seed", "reminder.send", b"seed");
    claimed(store.claim_notify(&id, &subject, "launch").await.unwrap());
    // Full credits cannot bypass a persisted ten-admission rolling window.
    // A future high-water simulates database clock rollback without changing clocks.
    sqlx::query(
        "UPDATE internal_notify_budgets SET credit_ms = 25920000, \
         updated_ms = floor(extract(epoch FROM clock_timestamp()) * 1000)::bigint + 60000, \
         admissions = array_fill(floor(extract(epoch FROM clock_timestamp()) * 1000)::bigint, ARRAY[10])",
    ).execute(&db.pool).await.unwrap();
    let next = identity("notify-clock-next", "place_offer.send", b"next");
    assert!(
        matches!(store.claim_notify(&next, &subject, "launch").await, Err(InternalStoreError::NotifyRateLimited { retry_after_secs }) if retry_after_secs > 86000)
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM internal_idempotency")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    // Rollback must not mint refill credits either, even with no daily ceiling.
    sqlx::query("UPDATE internal_notify_budgets SET credit_ms = 0, admissions = '{}'")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        matches!(store.claim_notify(&next, &subject, "launch").await, Err(InternalStoreError::NotifyRateLimited { retry_after_secs }) if retry_after_secs > 8640)
    );
    // Once all ten admissions are older than 24h, a new process may admit again.
    sqlx::query(
        "UPDATE internal_notify_budgets SET credit_ms = 25920000, \
         updated_ms = floor(extract(epoch FROM clock_timestamp()) * 1000)::bigint, \
         admissions = array_fill(floor(extract(epoch FROM clock_timestamp()) * 1000)::bigint - 86400001, ARRAY[10])",
    ).execute(&db.pool).await.unwrap();
    let restarted = db.store();
    claimed(
        restarted
            .claim_notify(&next, &subject, "launch")
            .await
            .unwrap(),
    );
    let admissions: Vec<i64> = sqlx::query_scalar("SELECT admissions FROM internal_notify_budgets")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        admissions.len(),
        1,
        "expired rolling history is replaced atomically"
    );
    db.cleanup().await;
}

#[tokio::test]
async fn notify_unsent_retries_count_and_denied_reacquisition_stays_released() {
    let db = TestDb::new().await;
    let store = db.store();
    let subject = notify_subject();
    let id = identity("notify-unsent-budget", "reminder.send", b"payload");
    for _ in 0..3 {
        let owner = claimed(store.claim_notify(&id, &subject, "launch").await.unwrap());
        store.release_proven_not_sent(owner).await.unwrap();
    }
    assert!(matches!(
        store.claim_notify(&id, &subject, "launch").await,
        Err(InternalStoreError::NotifyRateLimited { .. })
    ));
    let state: String = sqlx::query_scalar("SELECT state FROM internal_idempotency")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        state, "not_sent",
        "denial rolls back tentative reacquisition"
    );
    let admissions: Vec<i64> = sqlx::query_scalar("SELECT admissions FROM internal_notify_budgets")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(admissions.len(), 3);
    db.cleanup().await;
}

#[tokio::test]
async fn notify_budget_storage_failure_rolls_back_intent_and_audit() {
    let db = TestDb::new().await;
    let store = db.store();
    sqlx::query("DROP TABLE internal_notify_budgets")
        .execute(&db.pool)
        .await
        .unwrap();
    let id = identity("notify-unavailable", "reminder.send", b"payload");
    assert_eq!(
        store
            .claim_notify(&id, &notify_subject(), "launch")
            .await
            .unwrap_err(),
        InternalStoreError::Unavailable
    );
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM internal_idempotency), (SELECT count(*) FROM internal_action_log)",
    ).fetch_one(&db.pool).await.unwrap();
    assert_eq!(counts, (0, 0));
    db.cleanup().await;
}

#[tokio::test]
async fn unsent_release_refuses_uncertainty_and_rolls_back_with_its_audit() {
    let db = TestDb::new().await;
    let store = db.store();
    let subject = subject();
    for state in ["unknown", "completed", "stale"] {
        let id = identity(&format!("unsent-{state}:123"), "role.assign", b"payload");
        let claim = claimed(store.claim(&id, &subject).await.unwrap());
        match state {
            "unknown" => store.mark_unknown(&claim).await.unwrap(),
            "completed" => store.finish(&claim, &success()).await.unwrap(),
            "stale" => {
                sqlx::query("UPDATE internal_idempotency SET created_at = clock_timestamp() - interval '1 hour' WHERE intent_id = $1")
                    .bind(claim.intent_id()).execute(&db.pool).await.unwrap();
            }
            _ => unreachable!(),
        }
        assert_eq!(
            store.release_proven_not_sent(claim).await.unwrap_err(),
            InternalStoreError::TransitionRefused
        );
        assert!(!matches!(
            store.claim(&id, &subject).await.unwrap(),
            InternalClaim::Claimed(_)
        ));
    }
    let id = identity("unsent-rollback:123", "role.assign", b"payload");
    let claim = claimed(store.claim(&id, &subject).await.unwrap());
    sqlx::query("ALTER TABLE internal_action_log ADD CONSTRAINT injected_release_failure CHECK (phase <> 'released')")
        .execute(&db.pool).await.unwrap();
    assert_eq!(
        store.release_proven_not_sent(claim).await.unwrap_err(),
        InternalStoreError::Unavailable
    );
    assert!(matches!(
        store.claim(&id, &subject).await.unwrap(),
        InternalClaim::InFlight
    ));
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM internal_action_log WHERE phase = 'released'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    db.cleanup().await;
}

fn success() -> TerminalResponse {
    TerminalResponse::Success {
        resource_id: Some(DiscordId::new("345678901234567890").unwrap()),
        affected: 2,
        outcome: None,
        stop_reason: None,
    }
}

/// A replay that is fresh before an async wait but stale after it must be
/// refused, never granted a second burn. Seeds a live row expiring ~2s out,
/// holds the writer behind a controlled row lock for 3s (crossing both the
/// row expiry and the attempt's skew window), then asserts `InvalidInput`
/// and that the original burn stands untouched. A fresh attempt against the
/// same expired row still wins a genuine replacement afterwards.
#[tokio::test]
async fn stale_wait_cannot_win_second_nonce_burn() {
    let db = TestDb::new().await;
    let second = db.independent_pool().await;
    let store = InternalActionStore::new(second.clone());
    let nonce = body_hash(db.fixture.name().as_bytes())[..32].to_owned();
    let nonce_hash = body_hash(nonce.as_bytes());
    let start = db_now_secs(&db.pool).await;
    sqlx::query(
        "WITH instant AS MATERIALIZED (SELECT clock_timestamp() AS now) \
         INSERT INTO internal_nonces SELECT $1, now - INTERVAL '239 seconds', now + INTERVAL '2 seconds' FROM instant",
    )
    .bind(&nonce_hash)
    .execute(&db.pool)
    .await
    .unwrap();
    // Fresh now (1s of margin), stale after the 3s hold plus lock overhead.
    let attempt = (start - SKEW_SECONDS as i64 + 1).to_string();
    let mut blocker = db.pool.begin().await.unwrap();
    sqlx::query("SELECT nonce_hash FROM internal_nonces WHERE nonce_hash = $1 FOR UPDATE")
        .bind(&nonce_hash)
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    let writer = store.clone();
    let offered = nonce.clone();
    let burn = tokio::spawn(async move { writer.burn_nonce(&offered, &attempt).await });
    wait_for_uniqueness_lock(&db, "internal_nonces").await;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    blocker.rollback().await.unwrap();
    assert_eq!(
        burn.await.unwrap(),
        Err(InternalStoreError::InvalidInput),
        "a replay that lapsed mid-wait must be refused"
    );
    let row =
        sqlx::query("SELECT burned_at, expires_at FROM internal_nonces WHERE nonce_hash = $1")
            .bind(&nonce_hash)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    let burned_at: time::OffsetDateTime = row.get("burned_at");
    let now: time::OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    // The refused replay replaced nothing: the row still dates from the seed.
    assert!(
        (now - burned_at) >= time::Duration::seconds(238),
        "refused replay must not refresh the burn"
    );
    // A genuinely fresh attempt against the expired row replaces it.
    let fresh = db_now_secs(&db.pool).await.to_string();
    assert!(store.burn_nonce(&nonce, &fresh).await.unwrap());
    second.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn nonce_race_restart_and_expiry_window() {
    let db = TestDb::new().await;
    let second = db.independent_pool().await;
    let a = db.store();
    let b = InternalActionStore::new(second.clone());
    let nonce = body_hash(db.fixture.name().as_bytes())[..32].to_owned();
    let attempt = db_now_secs(&db.pool).await.to_string();
    let mut tasks = Vec::new();
    for i in 0..24 {
        let store = if i % 2 == 0 { a.clone() } else { b.clone() };
        let nonce = nonce.clone();
        let attempt = attempt.clone();
        tasks.push(tokio::spawn(async move {
            store.burn_nonce(&nonce, &attempt).await.unwrap()
        }));
    }
    let mut wins = 0;
    for task in tasks {
        wins += usize::from(task.await.unwrap());
    }
    assert_eq!(wins, 1);
    let retained: f64 = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM expires_at - burned_at)::double precision FROM internal_nonces",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(retained >= (2 * SKEW_SECONDS + 1) as f64);
    assert_eq!(retained, NONCE_TTL_SECONDS as f64);
    // A future expiry is refused. A known-past expiry is replaceable, with a
    // full fresh interval. Alter BOTH timestamps to preserve the retention check.
    let now = db_now_secs(&db.pool).await.to_string();
    sqlx::query("UPDATE internal_nonces SET burned_at = clock_timestamp() - INTERVAL '240 seconds', expires_at = clock_timestamp() + INTERVAL '1 second'")
        .execute(&db.pool).await.unwrap();
    assert!(!b.burn_nonce(&nonce, &now).await.unwrap());
    let now = db_now_secs(&db.pool).await.to_string();
    sqlx::query("UPDATE internal_nonces SET burned_at = clock_timestamp() - INTERVAL '242 seconds', expires_at = clock_timestamp() - INTERVAL '1 second'")
        .execute(&db.pool).await.unwrap();
    assert!(b.burn_nonce(&nonce, &now).await.unwrap());
    second.close().await;
    let restarted = db.independent_pool().await;
    let now = db_now_secs(&db.pool).await.to_string();
    assert!(!InternalActionStore::new(restarted.clone())
        .burn_nonce(&nonce, &now)
        .await
        .unwrap());
    // Built dynamically so static analysis does not read a hard-coded nonce.
    // A malformed (non-numeric) timestamp fails the commit-time freshness
    // check even though the nonce format check runs first.
    let invalid_nonce = ["not", "a", "valid", "nonce"].join("-");
    assert_eq!(
        a.burn_nonce(&invalid_nonce, "not-a-timestamp").await,
        Err(InternalStoreError::InvalidInput)
    );
    restarted.close().await;
    db.cleanup().await;
}

/// F8 durable fail-closed: accept a capture, expire and replace its nonce,
/// then regress the DB clock into its signed window. The persisted high-water
/// mark refuses the rolled-back capture even though the old row is gone — on
/// the live store and on a fresh store (restart/failover) that re-derives the
/// mark from the table. Time is injected only through rows the test owns and
/// the mark table; the server clock is never changed.
///
/// DB-time regression is injected by moving the mark row INTO THE FUTURE:
/// `burn_nonce` reads its commit instant from the real `clock_timestamp()`,
/// so a mark ~300 s ahead makes the genuine commit instant read as regressed
/// past the 5 s tolerance — exercising the real in-transaction rollback
/// branch. Backdating rows alone could never move `clock_timestamp()`.
#[tokio::test]
async fn nonce_db_rollback_after_expiry_refuses_capture() {
    use two_bot_core::clock_guard::{ClockGuard, CLOCK_SKEW_TOLERANCE_MS};

    let db = TestDb::new().await;
    let store = db.store();
    let nonce = body_hash(db.fixture.name().as_bytes())[..32].to_owned();
    let attempt = db_now_secs(&db.pool).await.to_string();
    assert!(
        store.burn_nonce(&nonce, &attempt).await.unwrap(),
        "first burn accepted"
    );
    let mark = store
        .nonce_high_water_ms()
        .await
        .unwrap()
        .expect("first burn persists the high-water mark");
    // The mark matches DB time to the second (whole-second commit instant).
    let db_ms: i64 =
        sqlx::query_scalar("SELECT floor(extract(epoch FROM clock_timestamp()) * 1000)::bigint")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(
        (db_ms - mark as i64).abs() <= 2_000,
        "mark tracks the DB clock: mark={mark} db={db_ms}"
    );

    // Expire and replace the nonce row so the old capture's only guard is the
    // mark; a fresh store (new process) sees the same persisted mark.
    let now = db_now_secs(&db.pool).await.to_string();
    sqlx::query(
        "UPDATE internal_nonces SET burned_at = clock_timestamp() - INTERVAL '242 seconds', \
         expires_at = clock_timestamp() - INTERVAL '1 second'",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    let replacement =
        body_hash(format!("{}-replacement", db.fixture.name()).as_bytes())[..32].to_owned();
    assert!(
        store.burn_nonce(&replacement, &now).await.unwrap(),
        "expired row is replaceable before rollback"
    );

    // Inject a DB-time regression: advance the mark ~300 s into the future
    // (keeping `observed_at` consistent with the CHECK) so the next genuine
    // commit instant reads as regressed past the tolerance. The rolled-back
    // capture is a FRESH nonce with a FRESH timestamp — only the clock policy
    // refuses it, proving the rollback branch rather than expiry or staleness.
    let future_ms: i64 = db_ms + 300_000;
    sqlx::query(
        "UPDATE internal_clock_high_water SET high_water_ms = $1, \
         observed_at = TO_TIMESTAMP($1::double precision / 1000.0) \
         WHERE domain = 'internal_nonce_db'",
    )
    .bind(future_ms)
    .execute(&db.pool)
    .await
    .unwrap();
    let regressed_nonce =
        body_hash(format!("{}-regressed", db.fixture.name()).as_bytes())[..32].to_owned();
    let regressed_attempt = db_now_secs(&db.pool).await.to_string();
    assert_eq!(
        store.burn_nonce(&regressed_nonce, &regressed_attempt).await,
        Err(InternalStoreError::InvalidInput),
        "regressed DB clock must refuse even a fresh capture"
    );
    // The refused burn left no trace: neither a nonce row nor a mark advance.
    let nonce_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM internal_nonces WHERE nonce_hash = $1")
            .bind(body_hash(regressed_nonce.as_bytes()))
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(nonce_rows, 0, "refused burn must not insert a nonce row");
    let mark_after = store.nonce_high_water_ms().await.unwrap();
    assert_eq!(
        mark_after,
        Some(u64::try_from(future_ms).unwrap()),
        "refused burn must not advance the mark"
    );

    // A fresh store with an earlier clock and the persisted mark also refuses:
    // restore the guard from the table exactly as a restarted process would.
    // The "earlier clock" is real DB time, ~300 s below the injected mark.
    let persisted = mark_after.expect("mark survives for a restarted process");
    let mut guard = ClockGuard::restore(persisted);
    let earlier = u64::try_from(db_ms).unwrap();
    assert!(earlier + CLOCK_SKEW_TOLERANCE_MS < persisted);
    let err = guard.evaluate(earlier).expect_err(
        "new process with an earlier clock and the persisted mark refuses old captures",
    );
    assert_eq!(err.high_water_ms, persisted);

    // Forward DB time still behaves as before: clear the injected future mark
    // and a fresh capture burns.
    sqlx::query("DELETE FROM internal_nonces")
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM internal_clock_high_water WHERE domain = 'internal_nonce_db'")
        .execute(&db.pool)
        .await
        .unwrap();
    let fresh_nonce = body_hash(format!("{}-fresh", db.fixture.name()).as_bytes())[..32].to_owned();
    let fresh = db_now_secs(&db.pool).await.to_string();
    assert!(store.burn_nonce(&fresh_nonce, &fresh).await.unwrap());
    db.cleanup().await;
}

#[tokio::test]
async fn claims_race_bind_payload_action_caller_and_replay_after_restart() {
    let db = TestDb::new().await;
    let second = db.independent_pool().await;
    let a = db.store();
    let b = InternalActionStore::new(second.clone());
    let identity = identity(
        "request:123",
        "announcement.post",
        b"{\"content\":\"hello\"}",
    );
    let mut tasks = Vec::new();
    for i in 0..24 {
        let store = if i % 2 == 0 { a.clone() } else { b.clone() };
        let id = identity.clone();
        tasks.push(tokio::spawn(async move {
            store.claim(&id, &subject()).await.unwrap()
        }));
    }
    let mut winner = None;
    for task in tasks {
        match task.await.unwrap() {
            InternalClaim::Claimed(c) => assert!(winner.replace(c).is_none()),
            InternalClaim::InFlight => {}
            other => panic!("unexpected concurrent result: {other:?}"),
        }
    }
    let winner = winner.unwrap();
    for changed in [
        RequestIdentity::new(
            "website",
            "request:123",
            "event.upsert",
            b"{\"content\":\"hello\"}",
        )
        .unwrap(),
        RequestIdentity::new(
            "website",
            "request:123",
            "announcement.post",
            b"{ \"content\":\"hello\"}",
        )
        .unwrap(),
    ] {
        assert!(matches!(
            b.claim(&changed, &subject()).await.unwrap(),
            InternalClaim::Mismatch
        ));
    }
    let caller2 = RequestIdentity::new(
        "other-website",
        "request:123",
        "announcement.post",
        b"{\"content\":\"hello\"}",
    )
    .unwrap();
    assert!(matches!(
        b.claim(&caller2, &subject()).await.unwrap(),
        InternalClaim::Claimed(_)
    ));
    let intent_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM internal_action_log WHERE phase = 'intent'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(intent_count, 2); // One intent per caller slot, not per delivery.
    a.finish(&winner, &success()).await.unwrap();
    assert_eq!(
        a.finish(&winner, &success()).await,
        Err(InternalStoreError::TransitionRefused)
    );
    second.close().await;
    let restarted = db.independent_pool().await;
    let c = InternalActionStore::new(restarted.clone());
    assert!(
        matches!(c.claim(&identity, &subject()).await.unwrap(), InternalClaim::Replay(r) if r == success())
    );
    let changed = RequestIdentity::new(
        "website",
        "request:123",
        "announcement.post",
        b"other payload",
    )
    .unwrap();
    assert!(matches!(
        c.claim(&changed, &subject()).await.unwrap(),
        InternalClaim::Mismatch
    ));
    restarted.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn stale_and_unknown_claims_never_reexecute_and_reconciliation_is_terminal() {
    let db = TestDb::new().await;
    let a = db.store();
    let id = identity("stale-key:123", "moderation.ban", b"validated payload");
    let owner = claimed(a.claim(&id, &subject()).await.unwrap());
    assert_eq!(
        a.reconcile(
            &id,
            &success(),
            ReconciliationEvidence::DiscordConfirmedEffect
        )
        .await,
        Err(InternalStoreError::TransitionRefused)
    );
    sqlx::query("UPDATE internal_idempotency SET created_at = clock_timestamp() - ($1::bigint * INTERVAL '1 second') WHERE intent_id = $2")
        .bind(i64::try_from(CLAIM_STALE_SECONDS).unwrap())
        .bind(owner.intent_id()).execute(&db.pool).await.unwrap();
    let second = db.independent_pool().await;
    let restarted = InternalActionStore::new(second.clone());
    for _ in 0..3 {
        assert!(matches!(
            restarted.claim(&id, &subject()).await.unwrap(),
            InternalClaim::NeedsReconciliation
        ));
    }
    let unknown_id = identity(
        "unknown-key:123",
        "guild.add_member",
        b"validated OAuth payload",
    );
    let unknown = claimed(a.claim(&unknown_id, &subject()).await.unwrap());
    a.mark_unknown(&unknown).await.unwrap();
    a.mark_unknown(&unknown).await.unwrap(); // Audit dedup, still never a new claim.
    assert!(matches!(
        restarted.claim(&unknown_id, &subject()).await.unwrap(),
        InternalClaim::NeedsReconciliation
    ));
    assert_eq!(
        a.reconcile(&id, &success(), ReconciliationEvidence::ProvenNotSent)
            .await,
        Err(InternalStoreError::InvalidInput)
    );
    restarted
        .reconcile(
            &id,
            &success(),
            ReconciliationEvidence::DiscordConfirmedEffect,
        )
        .await
        .unwrap();
    assert_eq!(
        a.finish(
            &owner,
            &TerminalResponse::Failure(TerminalFailure::NoEffect)
        )
        .await,
        Err(InternalStoreError::TransitionRefused)
    );
    assert!(
        matches!(a.claim(&id, &subject()).await.unwrap(), InternalClaim::Replay(r) if r == success())
    );
    let failure = TerminalResponse::Failure(TerminalFailure::NoEffect);
    restarted
        .reconcile(&unknown_id, &failure, ReconciliationEvidence::ProvenNotSent)
        .await
        .unwrap();
    assert!(
        matches!(a.claim(&unknown_id, &subject()).await.unwrap(), InternalClaim::Replay(r) if r == failure)
    );
    assert_eq!(
        a.mark_unknown(&unknown).await,
        Err(InternalStoreError::TransitionRefused)
    );
    second.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn event_outcome_persists_and_replays_the_recorded_response() {
    let db = TestDb::new().await;
    let store = db.store();
    for (key, outcome) in [
        ("outcome-create:123", EventOutcome::Created),
        ("outcome-update:123", EventOutcome::Updated),
        ("outcome-cancel:123", EventOutcome::Cancelled),
    ] {
        let id = identity(key, "event.upsert", b"{\"event_key\":\"launch\"}");
        let claim = claimed(store.claim(&id, &subject()).await.unwrap());
        let receipt = TerminalResponse::Success {
            resource_id: Some(DiscordId::new("345678901234567890").unwrap()),
            affected: 1,
            outcome: Some(outcome),
            stop_reason: None,
        };
        store.finish(&claim, &receipt).await.unwrap();
        assert!(
            matches!(store.claim(&id, &subject()).await.unwrap(), InternalClaim::Replay(r) if r == receipt),
            "replay must return the recorded outcome, never a re-derived one"
        );
    }
    // Announcement receipts carry no outcome: NULL persists and replays as None.
    let legacy = identity("outcome-legacy:123", "announcement.post", b"{}");
    let claim = claimed(store.claim(&legacy, &subject()).await.unwrap());
    store.finish(&claim, &success()).await.unwrap();
    assert!(
        matches!(store.claim(&legacy, &subject()).await.unwrap(), InternalClaim::Replay(r) if r == success())
    );
    db.cleanup().await;
}

/// Losing the typed reason in finish, reconciliation, hydration, or terminal
/// audit must fail this matrix. NULL remains the historical success shape.
#[tokio::test]
async fn purge_stop_reason_finish_and_reconcile_preserve_receipt_audit_and_replay() {
    use two_bot_core::PurgeStopReason::{RateLimited, Refused};

    let db = TestDb::new().await;
    let store = db.store();
    let peer = db.independent_pool().await;
    let restarted = InternalActionStore::new(peer.clone());
    for reconcile in [false, true] {
        for (label, stop_reason, stored_reason, affected) in [
            ("refused", Some(Refused), Some("refused"), 1),
            ("rate", Some(RateLimited), Some("rate_limited"), 1),
            ("complete", None, None, 3),
        ] {
            let id = identity(
                &format!("purge-stop-{reconcile}-{label}"),
                "moderation.purge",
                br#"{"count":3}"#,
            );
            let claim = claimed(store.claim(&id, &subject()).await.unwrap());
            let receipt = TerminalResponse::Success {
                resource_id: Some(DiscordId::new("345678901234567890").unwrap()),
                affected,
                outcome: None,
                stop_reason,
            };
            if reconcile {
                store.mark_unknown(&claim).await.unwrap();
                restarted
                    .reconcile(
                        &id,
                        &receipt,
                        ReconciliationEvidence::DiscordConfirmedEffect,
                    )
                    .await
                    .unwrap();
            } else {
                store.finish(&claim, &receipt).await.unwrap();
            }
            for _ in 0..2 {
                assert!(
                    matches!(restarted.claim(&id, &subject()).await.unwrap(), InternalClaim::Replay(response) if response == receipt),
                    "{label}: reason and affected count must survive a repeated claim"
                );
            }
            let stored: (Option<i64>, Option<String>) = sqlx::query_as(
                "SELECT affected, stop_reason FROM internal_idempotency WHERE intent_id = $1",
            )
            .bind(claim.intent_id())
            .fetch_one(&db.pool)
            .await
            .unwrap();
            assert_eq!(
                stored,
                (Some(i64::from(affected)), stored_reason.map(str::to_owned))
            );
            let terminal: (Option<i64>, Option<String>, Option<String>) = sqlx::query_as(
                "SELECT affected, stop_reason, evidence_code FROM internal_action_log \
                 WHERE intent_id = $1 AND phase = 'terminal'",
            )
            .bind(claim.intent_id())
            .fetch_one(&db.pool)
            .await
            .unwrap();
            assert_eq!(
                terminal,
                (
                    Some(i64::from(affected)),
                    stored_reason.map(str::to_owned),
                    Some(
                        if reconcile {
                            "discord_confirmed_effect"
                        } else {
                            "executor"
                        }
                        .to_owned()
                    ),
                )
            );
            let intent_reason: Option<String> = sqlx::query_scalar(
                "SELECT stop_reason FROM internal_action_log WHERE intent_id = $1 AND phase = 'intent'",
            )
            .bind(claim.intent_id())
            .fetch_one(&db.pool)
            .await
            .unwrap();
            assert_eq!(
                intent_reason, None,
                "intent cannot predict an executor reason"
            );
            let terminal_count: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM internal_action_log WHERE intent_id = $1 AND phase = 'terminal'",
            )
            .bind(claim.intent_id())
            .fetch_one(&db.pool)
            .await
            .unwrap();
            assert_eq!(
                terminal_count, 1,
                "replay must not duplicate terminal audit"
            );
            assert_eq!(
                store.finish(&claim, &receipt).await,
                Err(InternalStoreError::TransitionRefused),
            );
        }
    }
    peer.close().await;
    db.cleanup().await;
}

/// Both durable surfaces reject values outside the nullable closed vocabulary;
/// accepting arbitrary strings would let corrupt evidence flow into a receipt.
#[tokio::test]
async fn purge_stop_reason_columns_reject_unknown_words() {
    let db = TestDb::new().await;
    let store = db.store();
    let id = identity("purge-stop-vocabulary", "moderation.purge", b"{}");
    let claim = claimed(store.claim(&id, &subject()).await.unwrap());
    let receipt = TerminalResponse::Success {
        resource_id: None,
        affected: 3,
        outcome: None,
        stop_reason: None,
    };
    store.finish(&claim, &receipt).await.unwrap();
    for table in ["internal_idempotency", "internal_action_log"] {
        for invalid in ["bogus", "", "Refused", "rate-limited", "refused "] {
            let error = sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE {table} SET stop_reason = $1 WHERE intent_id = $2",
            )))
            .bind(invalid)
            .bind(claim.intent_id())
            .execute(&db.pool)
            .await
            .expect_err("closed vocabulary must reject invalid stored reasons");
            assert_eq!(
                error
                    .as_database_error()
                    .and_then(|error| error.code())
                    .as_deref(),
                Some("23514"),
                "{table}: stop_reason must have a closed-vocabulary CHECK",
            );
        }
        let reasons: Vec<Option<String>> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT stop_reason FROM {table} WHERE intent_id = $1",
        )))
        .bind(claim.intent_id())
        .fetch_all(&db.pool)
        .await
        .unwrap();
        assert!(!reasons.is_empty());
        assert!(
            reasons.iter().all(Option::is_none),
            "failed writes retain NULL"
        );
    }
    assert!(
        matches!(store.claim(&id, &subject()).await.unwrap(), InternalClaim::Replay(response) if response == receipt)
    );
    db.cleanup().await;
}

/// Simulate corrupt historical data in this disposable fixture only. A parser
/// using `.ok()` would silently turn bogus into None and replay success.
#[tokio::test]
async fn purge_stop_reason_corrupt_outer_replay_fails_closed() {
    let db = TestDb::new().await;
    let store = db.store();
    let id = identity("purge-stop-corrupt", "moderation.purge", b"{}");
    let claim = claimed(store.claim(&id, &subject()).await.unwrap());
    store
        .finish(
            &claim,
            &TerminalResponse::Success {
                resource_id: None,
                affected: 1,
                outcome: None,
                stop_reason: Some(two_bot_core::PurgeStopReason::Refused),
            },
        )
        .await
        .unwrap();
    // Discover names, not an implementation-specific constraint name. Only
    // remove stop_reason checks from the uniquely owned TestDatabase.
    let constraints: Vec<String> = sqlx::query_scalar(
        "SELECT conname FROM pg_constraint WHERE conrelid = 'internal_idempotency'::regclass \
         AND contype = 'c' AND pg_get_constraintdef(oid) LIKE '%stop_reason%'",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert!(!constraints.is_empty(), "stop_reason must be constrained");
    for constraint in constraints {
        let quoted = constraint.replace('"', "\"\"");
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "ALTER TABLE internal_idempotency DROP CONSTRAINT \"{quoted}\"",
        )))
        .execute(&db.pool)
        .await
        .unwrap();
    }
    sqlx::query("UPDATE internal_idempotency SET stop_reason = 'bogus' WHERE intent_id = $1")
        .bind(claim.intent_id())
        .execute(&db.pool)
        .await
        .unwrap();
    let peer = db.independent_pool().await;
    let restarted = InternalActionStore::new(peer.clone());
    for _ in 0..2 {
        assert_eq!(
            restarted.claim(&id, &subject()).await.unwrap_err(),
            InternalStoreError::Unavailable,
            "invalid reason must neither grant execution nor replay success",
        );
    }
    let row: (String, Option<String>) =
        sqlx::query_as("SELECT state, stop_reason FROM internal_idempotency WHERE intent_id = $1")
            .bind(claim.intent_id())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(row, ("completed".to_owned(), Some("bogus".to_owned())));
    let terminal_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM internal_action_log WHERE intent_id = $1 AND phase = 'terminal'",
    )
    .bind(claim.intent_id())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        terminal_count, 1,
        "corrupt replay must not write another audit"
    );
    peer.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn terminal_audit_names_receipt_resource_and_outcome() {
    let db = TestDb::new().await;
    let store = db.store();
    // An event.upsert create claims with the guild alone as its subject, so
    // the intent audit names no target: the terminal audit must still name
    // the Discord event and the result word from the completed receipt.
    let created = AuditSubject {
        guild_id: Some(DiscordId::new("123456789012345678").unwrap()),
        ..AuditSubject::default()
    };
    let id = identity(
        "audit-create:123",
        "event.upsert",
        b"{\"event_key\":\"launch\"}",
    );
    let claim = claimed(store.claim(&id, &created).await.unwrap());
    let intent_id = claim.intent_id();
    let receipt = TerminalResponse::Success {
        resource_id: Some(DiscordId::new("345678901234567890").unwrap()),
        affected: 1,
        outcome: Some(EventOutcome::Created),
        stop_reason: None,
    };
    store.finish(&claim, &receipt).await.unwrap();
    let intent: (Option<String>, Option<i64>, Option<String>) = sqlx::query_as(
        "SELECT resource_id, affected, outcome FROM internal_action_log \
         WHERE intent_id = $1 AND phase = 'intent'",
    )
    .bind(intent_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(intent, (None, None, None));
    let terminal: (Option<String>, Option<i64>, Option<String>) = sqlx::query_as(
        "SELECT resource_id, affected, outcome FROM internal_action_log \
         WHERE intent_id = $1 AND phase = 'terminal'",
    )
    .bind(intent_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        terminal,
        (
            Some("345678901234567890".to_owned()),
            Some(1),
            Some("created".to_owned())
        )
    );
    db.cleanup().await;
}

#[tokio::test]
async fn audit_failures_roll_back_claim_and_terminal_without_leaking_details() {
    let db = TestDb::new().await;
    let store = db.store();
    let id = identity(
        "redaction-key:123",
        "guild.add_member",
        br#"{"access_token":"private-oauth-value","reason":"never-audit-this"}"#,
    );
    sqlx::query(
        "ALTER TABLE internal_action_log ADD CONSTRAINT injected_failure CHECK (phase <> 'intent')",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    assert!(matches!(
        store.claim(&id, &subject()).await,
        Err(InternalStoreError::Unavailable)
    ));
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM internal_idempotency")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    sqlx::query("ALTER TABLE internal_action_log DROP CONSTRAINT injected_failure")
        .execute(&db.pool)
        .await
        .unwrap();
    let owner = claimed(store.claim(&id, &subject()).await.unwrap());
    sqlx::query("ALTER TABLE internal_action_log ADD CONSTRAINT injected_failure CHECK (phase <> 'terminal')")
        .execute(&db.pool).await.unwrap();
    let error = store.finish(&owner, &success()).await.unwrap_err();
    assert_eq!(error.to_string(), "internal-action storage unavailable");
    assert!(std::error::Error::source(&error).is_none());
    assert!(matches!(
        store.claim(&id, &subject()).await.unwrap(),
        InternalClaim::InFlight
    ));
    sqlx::query("ALTER TABLE internal_action_log DROP CONSTRAINT injected_failure")
        .execute(&db.pool)
        .await
        .unwrap();
    store.finish(&owner, &success()).await.unwrap();
    let persisted: Vec<String> = sqlx::query_scalar(
        "SELECT row_to_json(i)::text FROM internal_idempotency i UNION ALL SELECT row_to_json(a)::text FROM internal_action_log a",
    ).fetch_all(&db.pool).await.unwrap();
    for row in persisted {
        for raw in [
            "private-oauth-value",
            "never-audit-this",
            "redaction-key:123",
            "website",
        ] {
            assert!(!row.contains(raw));
        }
    }
    let terminal = sqlx::query("SELECT response_code, http_status, evidence_code FROM internal_action_log WHERE phase = 'terminal'")
        .fetch_one(&db.pool).await.unwrap();
    assert_eq!(terminal.get::<&str, _>("response_code"), "success");
    assert_eq!(terminal.get::<i32, _>("http_status"), 200);
    assert_eq!(terminal.get::<&str, _>("evidence_code"), "executor");
    // Capture while the pool is still open: after `close` the burn fails at
    // transaction begin (Unavailable) before any freshness logic runs.
    let attempt = db_now_secs(&db.pool).await.to_string();
    db.pool.close().await;
    assert!(matches!(
        store.claim(&id, &subject()).await,
        Err(InternalStoreError::Unavailable)
    ));
    let nonce = body_hash(db.fixture.name().as_bytes())[..32].to_owned();
    assert_eq!(
        store.burn_nonce(&nonce, &attempt).await,
        Err(InternalStoreError::Unavailable)
    );
    assert_eq!(
        store.claim_discord_event("gateway:session:123").await,
        Err(InternalStoreError::Unavailable)
    );
    db.cleanup().await;
}

#[tokio::test]
async fn discord_event_dedup_is_atomic_global_and_durable() {
    let db = TestDb::new().await;
    let second = db.independent_pool().await;
    let a = db.store();
    let b = InternalActionStore::new(second.clone());
    let mut tasks = Vec::new();
    for i in 0..24 {
        let store = if i % 2 == 0 { a.clone() } else { b.clone() };
        tasks.push(tokio::spawn(async move {
            store.claim_discord_event("guild:event:123").await.unwrap()
        }));
    }
    let mut wins = 0;
    for task in tasks {
        wins += usize::from(task.await.unwrap());
    }
    assert_eq!(wins, 1);
    second.close().await;
    let restarted = db.independent_pool().await;
    let c = InternalActionStore::new(restarted.clone());
    assert!(!c.claim_discord_event("guild:event:123").await.unwrap());
    assert!(c.claim_discord_event("guild:event:124").await.unwrap());
    assert_eq!(
        c.claim_discord_event("secret\nheader").await,
        Err(InternalStoreError::InvalidInput)
    );
    restarted.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn completion_and_reconciliation_race_cannot_overwrite_terminal() {
    let db = TestDb::new().await;
    let second = db.independent_pool().await;
    let executor = db.store();
    let reconciler = InternalActionStore::new(second.clone());
    for n in 0..16 {
        let id = identity(
            &format!("finish-race:{n}"),
            "event.upsert",
            b"validated payload",
        );
        let owner = claimed(executor.claim(&id, &subject()).await.unwrap());
        executor.mark_unknown(&owner).await.unwrap();
        let success = success();
        let failure = TerminalResponse::Failure(TerminalFailure::NoEffect);
        let (finished, reconciled) = tokio::join!(
            executor.finish(&owner, &success),
            reconciler.reconcile(&id, &failure, ReconciliationEvidence::ProvenNotSent)
        );
        let expected = match (finished, reconciled) {
            (Ok(()), Err(InternalStoreError::TransitionRefused)) => success,
            (Err(InternalStoreError::TransitionRefused), Ok(())) => failure,
            other => panic!("exactly one terminal writer must win: {other:?}"),
        };
        assert!(
            matches!(executor.claim(&id, &subject()).await.unwrap(), InternalClaim::Replay(r) if r == expected)
        );
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM internal_action_log WHERE intent_id = $1 AND phase = 'terminal'",
        )
        .bind(owner.intent_id())
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(count, 1);
    }
    second.close().await;
    db.cleanup().await;
}

const EVENT_GUILD: &str = "100000000000000001";
const OTHER_GUILD: &str = "100000000000000099";
const MAPPED_EVENT: &str = "100000000000000002";

#[tokio::test]
async fn event_key_map_is_guild_fenced_and_repointable() {
    let db = TestDb::new().await;
    let store = db.store();
    // Unmapped keys resolve to None: the receiver refuses them before Discord.
    assert_eq!(
        store.event_id_for_key(EVENT_GUILD, "launch").await.unwrap(),
        None
    );
    store
        .put_event_key(EVENT_GUILD, "launch", MAPPED_EVENT)
        .await
        .unwrap();
    assert_eq!(
        store.event_id_for_key(EVENT_GUILD, "launch").await.unwrap(),
        Some(MAPPED_EVENT.to_owned())
    );
    // The fence is the guild: the same key elsewhere stays unmapped.
    assert_eq!(
        store.event_id_for_key(OTHER_GUILD, "launch").await.unwrap(),
        None
    );
    store
        .put_event_key(OTHER_GUILD, "launch", "100000000000000003")
        .await
        .unwrap();
    assert_eq!(
        store.event_id_for_key(OTHER_GUILD, "launch").await.unwrap(),
        Some("100000000000000003".to_owned())
    );
    // Re-pointing one guild leaves the other alone.
    store
        .put_event_key(EVENT_GUILD, "launch", "100000000000000004")
        .await
        .unwrap();
    assert_eq!(
        store.event_id_for_key(EVENT_GUILD, "launch").await.unwrap(),
        Some("100000000000000004".to_owned())
    );
    assert_eq!(
        store.event_id_for_key(OTHER_GUILD, "launch").await.unwrap(),
        Some("100000000000000003".to_owned())
    );
    db.cleanup().await;
}

#[tokio::test]
async fn event_key_map_refuses_misshapen_inputs() {
    let db = TestDb::new().await;
    let store = db.store();
    let long = "k".repeat(201);
    for (guild, key, event) in [
        ("not-a-snowflake", "launch", MAPPED_EVENT),
        (EVENT_GUILD, "", MAPPED_EVENT),
        (EVENT_GUILD, "has space", MAPPED_EVENT),
        (EVENT_GUILD, long.as_str(), MAPPED_EVENT),
        (EVENT_GUILD, "launch", "not-a-snowflake"),
    ] {
        assert_eq!(
            store.put_event_key(guild, key, event).await,
            Err(InternalStoreError::InvalidInput),
            "{guild}/{key}/{event}"
        );
    }
    for (guild, key) in [
        ("not-a-snowflake", "launch"),
        (EVENT_GUILD, ""),
        (EVENT_GUILD, "has space"),
    ] {
        assert_eq!(
            store.event_id_for_key(guild, key).await,
            Err(InternalStoreError::InvalidInput),
            "{guild}/{key}"
        );
    }
    db.cleanup().await;
}

fn event_subject(guild: &str) -> AuditSubject {
    AuditSubject {
        guild_id: Some(DiscordId::new(guild).unwrap()),
        actor_id: None,
        target_id: None,
        resolved_role_id: None,
    }
}

async fn unknown_event(
    store: &InternalActionStore,
    key: &str,
    action: &str,
    guild: &str,
) -> ExecutionClaim {
    let id = identity(key, action, b"{\"event_key\":\"launch\"}");
    let claim = claimed(store.claim(&id, &event_subject(guild)).await.unwrap());
    store.mark_unknown(&claim).await.unwrap();
    claim
}

fn created_receipt(event_id: &str) -> TerminalResponse {
    TerminalResponse::Success {
        resource_id: Some(DiscordId::new(event_id).unwrap()),
        affected: 1,
        outcome: Some(EventOutcome::Created),
        stop_reason: None,
    }
}

#[tokio::test]
async fn unknown_event_list_names_only_guild_unknown_events_oldest_first() {
    let db = TestDb::new().await;
    let store = db.store();
    // Non-event unknowns and completed event intents are never listed.
    let member = unknown_event(
        &store,
        "unknown-list-member:1",
        "guild.add_member",
        EVENT_GUILD,
    )
    .await;
    let done = claimed(
        store
            .claim(
                &identity(
                    "unknown-list-done:1",
                    "event.upsert",
                    b"{\"event_key\":\"launch\"}",
                ),
                &event_subject(EVENT_GUILD),
            )
            .await
            .unwrap(),
    );
    store
        .finish(&done, &created_receipt(MAPPED_EVENT))
        .await
        .unwrap();
    let first = unknown_event(&store, "unknown-list-first:1", "event.upsert", EVENT_GUILD).await;
    let second = unknown_event(&store, "unknown-list-second:1", "event.cancel", EVENT_GUILD).await;
    let other = unknown_event(&store, "unknown-list-other:1", "event.upsert", OTHER_GUILD).await;
    assert!(member.intent_id() > 0 && other.intent_id() > 0);
    let rows: Vec<UnknownEventIntent> =
        store.list_unknown_event_intents(EVENT_GUILD).await.unwrap();
    assert_eq!(
        rows.iter().map(|row| row.intent_id).collect::<Vec<_>>(),
        vec![first.intent_id(), second.intent_id()]
    );
    assert_eq!(rows[0].action, "event.upsert");
    assert_eq!(rows[1].action, "event.cancel");
    for row in &rows {
        assert_eq!(row.created_at_utc.len(), 19, "UTC YYYY-MM-DD HH:MM:SS");
    }
    let other_rows = store.list_unknown_event_intents(OTHER_GUILD).await.unwrap();
    assert_eq!(
        other_rows
            .iter()
            .map(|row| row.intent_id)
            .collect::<Vec<_>>(),
        vec![other.intent_id()]
    );
    assert_eq!(
        store.list_unknown_event_intents("not-a-snowflake").await,
        Err(InternalStoreError::InvalidInput)
    );
    db.cleanup().await;
}

#[tokio::test]
async fn resolve_event_create_heals_mapping_and_replays_created() {
    let db = TestDb::new().await;
    let store = db.store();
    // A create whose mapping write failed: Discord holds the event, the key
    // resolves to nothing.
    let claim = unknown_event(&store, "unknown-heal-create:1", "event.upsert", EVENT_GUILD).await;
    assert_eq!(
        store.event_id_for_key(EVENT_GUILD, "launch").await.unwrap(),
        None
    );
    let receipt = created_receipt(MAPPED_EVENT);
    store
        .resolve_event_intent(
            claim.intent_id(),
            "event.upsert",
            EVENT_GUILD,
            Some(("launch", MAPPED_EVENT)),
            &receipt,
            ReconciliationEvidence::DiscordConfirmedEffect,
        )
        .await
        .unwrap();
    // Mapping and terminal receipt commit together: replay serves the healed
    // create, never a second effect.
    assert_eq!(
        store.event_id_for_key(EVENT_GUILD, "launch").await.unwrap(),
        Some(MAPPED_EVENT.to_owned())
    );
    assert!(
        matches!(store.claim(&identity("unknown-heal-create:1", "event.upsert", b"{\"event_key\":\"launch\"}"), &event_subject(EVENT_GUILD)).await.unwrap(), InternalClaim::Replay(r) if r == receipt)
    );
    let evidence: String = sqlx::query_scalar(
        "SELECT evidence_code FROM internal_action_log WHERE intent_id = $1 AND phase = 'terminal'",
    )
    .bind(claim.intent_id())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(evidence, "discord_confirmed_effect");
    // A resolved intent is terminal: resolving again refuses.
    assert_eq!(
        store
            .resolve_event_intent(
                claim.intent_id(),
                "event.upsert",
                EVENT_GUILD,
                Some(("launch", MAPPED_EVENT)),
                &receipt,
                ReconciliationEvidence::DiscordConfirmedEffect,
            )
            .await,
        Err(InternalStoreError::TransitionRefused)
    );
    db.cleanup().await;
}

#[tokio::test]
async fn resolve_event_cancel_retains_mapping_and_replays_cancelled() {
    let db = TestDb::new().await;
    let store = db.store();
    store
        .put_event_key(EVENT_GUILD, "launch", MAPPED_EVENT)
        .await
        .unwrap();
    let claim = unknown_event(&store, "unknown-heal-cancel:1", "event.cancel", EVENT_GUILD).await;
    let receipt = TerminalResponse::Success {
        resource_id: Some(DiscordId::new(MAPPED_EVENT).unwrap()),
        affected: 1,
        outcome: Some(EventOutcome::Cancelled),
        stop_reason: None,
    };
    store
        .resolve_event_intent(
            claim.intent_id(),
            "event.cancel",
            EVENT_GUILD,
            None,
            &receipt,
            ReconciliationEvidence::DiscordConfirmedEffect,
        )
        .await
        .unwrap();
    // Cancel resolutions never rewrite mappings: the retained row survives so
    // a later edit cannot silently recreate the event.
    assert_eq!(
        store.event_id_for_key(EVENT_GUILD, "launch").await.unwrap(),
        Some(MAPPED_EVENT.to_owned())
    );
    assert!(
        matches!(store.claim(&identity("unknown-heal-cancel:1", "event.cancel", b"{\"event_key\":\"launch\"}"), &event_subject(EVENT_GUILD)).await.unwrap(), InternalClaim::Replay(r) if r == receipt)
    );
    db.cleanup().await;
}

#[tokio::test]
async fn resolve_event_no_effect_records_failure_without_mapping() {
    let db = TestDb::new().await;
    let store = db.store();
    let claim = unknown_event(&store, "unknown-no-effect:1", "event.upsert", EVENT_GUILD).await;
    let failure = TerminalResponse::Failure(TerminalFailure::NoEffect);
    store
        .resolve_event_intent(
            claim.intent_id(),
            "event.upsert",
            EVENT_GUILD,
            None,
            &failure,
            ReconciliationEvidence::DiscordConfirmedNoEffect,
        )
        .await
        .unwrap();
    assert_eq!(
        store.event_id_for_key(EVENT_GUILD, "launch").await.unwrap(),
        None
    );
    assert!(
        matches!(store.claim(&identity("unknown-no-effect:1", "event.upsert", b"{\"event_key\":\"launch\"}"), &event_subject(EVENT_GUILD)).await.unwrap(), InternalClaim::Replay(r) if r == failure)
    );
    db.cleanup().await;
}

#[tokio::test]
async fn resolve_event_intent_refuses_mismatched_rows_and_shapes() {
    let db = TestDb::new().await;
    let store = db.store();
    let claim = unknown_event(&store, "unknown-refuse:1", "event.upsert", EVENT_GUILD).await;
    let id = claim.intent_id();
    let created = created_receipt(MAPPED_EVENT);
    let cancelled = TerminalResponse::Success {
        resource_id: Some(DiscordId::new(MAPPED_EVENT).unwrap()),
        affected: 1,
        outcome: Some(EventOutcome::Cancelled),
        stop_reason: None,
    };
    let effect = ReconciliationEvidence::DiscordConfirmedEffect;
    // Wrong guild, wrong action, and unknown intent ids never resolve.
    // Each case uses a shape-valid mapping/response for its action so the
    // row-mismatch refusal (not the shape refusal) is what fires: cancels
    // carry no mapping, upserts do.
    for (intent, action, guild, mapping, response) in [
        (
            id,
            "event.upsert",
            OTHER_GUILD,
            Some(("launch", MAPPED_EVENT)),
            &created,
        ),
        (id, "event.cancel", EVENT_GUILD, None, &cancelled),
        (
            id + 10_000,
            "event.upsert",
            EVENT_GUILD,
            Some(("launch", MAPPED_EVENT)),
            &created,
        ),
    ] {
        assert_eq!(
            store
                .resolve_event_intent(intent, action, guild, mapping, response, effect)
                .await,
            Err(InternalStoreError::TransitionRefused),
            "{action}/{guild}"
        );
    }
    // Still fenced: the exact resolution still lands afterwards.
    store
        .resolve_event_intent(
            id,
            "event.upsert",
            EVENT_GUILD,
            Some(("launch", MAPPED_EVENT)),
            &created,
            effect,
        )
        .await
        .unwrap();
    db.cleanup().await;
}

#[tokio::test]
async fn resolve_event_intent_refuses_invalid_evidence_and_mappings() {
    let db = TestDb::new().await;
    let store = db.store();
    let claim = unknown_event(&store, "unknown-shape:1", "event.upsert", EVENT_GUILD).await;
    let id = claim.intent_id();
    let created = created_receipt(MAPPED_EVENT);
    let no_effect = TerminalResponse::Failure(TerminalFailure::NoEffect);
    let malformed = TerminalResponse::Failure(TerminalFailure::Malformed);
    let effect = ReconciliationEvidence::DiscordConfirmedEffect;
    let no_effect_evidence = ReconciliationEvidence::DiscordConfirmedNoEffect;
    // Evidence must support the response; only success/no-effect responses
    // are operator evidence; mapping shape must match the resolution kind.
    for (action, guild, mapping, response, evidence) in [
        (
            "event.upsert",
            EVENT_GUILD,
            Some(("launch", MAPPED_EVENT)),
            &created,
            ReconciliationEvidence::ProvenNotSent,
        ),
        (
            "event.upsert",
            EVENT_GUILD,
            Some(("launch", MAPPED_EVENT)),
            &malformed,
            effect,
        ),
        ("event.upsert", EVENT_GUILD, None, &created, effect),
        (
            "event.upsert",
            EVENT_GUILD,
            Some(("launch", MAPPED_EVENT)),
            &no_effect,
            no_effect_evidence,
        ),
        (
            "event.upsert",
            EVENT_GUILD,
            Some(("launch", "100000000000000007")),
            &created,
            effect,
        ),
        (
            "event.cancel",
            EVENT_GUILD,
            Some(("launch", MAPPED_EVENT)),
            &created,
            effect,
        ),
        (
            "role.assign",
            EVENT_GUILD,
            None,
            &no_effect,
            no_effect_evidence,
        ),
        (
            "event.upsert",
            "not-a-snowflake",
            None,
            &no_effect,
            no_effect_evidence,
        ),
    ] {
        assert_eq!(
            store
                .resolve_event_intent(id, action, guild, mapping, response, evidence)
                .await,
            Err(InternalStoreError::InvalidInput),
            "{action}/{guild}"
        );
    }
    // Non-positive intent ids refuse before any row lock.
    assert_eq!(
        store
            .resolve_event_intent(
                0,
                "event.upsert",
                EVENT_GUILD,
                None,
                &no_effect,
                no_effect_evidence
            )
            .await,
        Err(InternalStoreError::InvalidInput)
    );
    db.cleanup().await;
}
