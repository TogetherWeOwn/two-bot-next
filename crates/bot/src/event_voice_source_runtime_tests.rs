//! Builder-owned minute-source composition regressions, not protected acceptance tests.
//! Breaks caught: source route delay lost between ticks, stale scope publication,
//! and a queued POST surviving a newer cancellation or an expired observation.
use super::*;
use crate::discord_test_common::{MockRest, ScriptedResponse};
use serde_json::json;
use time::OffsetDateTime;
use two_bot_core::voice_create_admission::CreateAdmissionConfig;
use two_bot_cutover::event_voice::{
    bind_admitted_event_voice_channel, claim_current_event_voice_channel,
    mark_event_voice_orphaned, record_event_voice_join, EventVoiceCreateBinding,
};
use two_bot_cutover::voice_rooms::{CreateClaim, PgRoomStore};
use two_bot_testsupport::TestDatabase;

async fn database() -> TestDatabase {
    TestDatabase::create(
        &std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required"),
        &sqlx::migrate!("../cutover/migrations"),
    )
    .await
    .unwrap()
}

fn document() -> Value {
    json!({"id":"500","guild_id":"100","name":"Fresh event","entity_type":2,
        "status":1,"scheduled_start_time":"2026-11-01T20:10:00.000Z",
        "scheduled_end_time":null,"channel_id":null})
}

async fn queued_snapshot(pool: &PgPool) -> event_voice_source::Snapshot {
    let (_tx, shutdown) = watch::channel(false);
    let mut event = document();
    event["scheduled_start_time"] = json!(two_bot_core::funnel::format_iso_millis(
        i64::try_from(
            (OffsetDateTime::now_utc() + time::Duration::minutes(10)).unix_timestamp_nanos()
                / 1_000_000
        )
        .unwrap()
    ));
    event_voice_source::publish(
        pool,
        "100",
        &json!([event]),
        OffsetDateTime::now_utc(),
        tokio::time::Instant::now(),
        &shutdown,
    )
    .await
    .unwrap()
    .unwrap()
}

fn creator<'a>(
    pool: &'a PgPool,
    rest: &'a ActionExecutor,
) -> event_voice_create_runtime::Creator<'a> {
    event_voice_create_runtime::Creator {
        pool,
        rest,
        guild: "100",
        actor: 999,
        admission: two_bot_core::voice_create_admission::CreateAdmissionConfig::new(10, 10, 0)
            .unwrap(),
    }
}

#[tokio::test]
async fn minute_source_create_rechecks_same_millisecond_cancellation_and_source_age() {
    let db = database().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let rest = executor(&mock);
    let snapshot = queued_snapshot(db.pool()).await;
    sqlx::query("UPDATE scheduled_events SET status='cancelled' WHERE event_id='500'")
        .execute(db.pool())
        .await
        .unwrap();
    assert!(matches!(
        creator(db.pool(), &rest)
            .create_observed(&snapshot, &snapshot.events[0], "200")
            .await
            .unwrap(),
        event_voice_ticker::CreateResult::Ineligible
    ));
    let snapshot = queued_snapshot(db.pool()).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_millis(60_001)).await;
    assert!(matches!(
        creator(db.pool(), &rest)
            .create_observed(&snapshot, &snapshot.events[0], "200")
            .await
            .unwrap(),
        event_voice_ticker::CreateResult::Ineligible
    ));
    tokio::time::resume();
    assert!(mock.requests().is_empty(), "no stale POST");
    let claims: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM event_voice_channels")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(claims, 0);
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn minute_source_create_rechecks_mutation_after_capacity_wait_and_releases_safe_fences() {
    let db = database().await;
    let snapshot = queued_snapshot(db.pool()).await;
    sqlx::raw_sql("CREATE FUNCTION notify_source_claim() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_notify('source_test_claim','claimed'); RETURN NEW; END $$; CREATE TRIGGER notify_source_claim AFTER INSERT ON event_voice_channels FOR EACH ROW EXECUTE FUNCTION notify_source_claim();").execute(db.pool()).await.unwrap();
    let mut listener = sqlx::postgres::PgListener::connect_with(db.pool())
        .await
        .unwrap();
    listener.listen("source_test_claim").await.unwrap();
    let mut capacity = db.pool().begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('voice_create:100',0))")
        .execute(&mut *capacity)
        .await
        .unwrap();
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(
            201,
            json!({"id":"700","guild_id":"100","parent_id":"200","type":2}),
        ),
    )
    .await;
    let rest = executor(&mock);
    let creator = creator(db.pool(), &rest);
    let change = async {
        tokio::time::timeout(Duration::from_secs(5), listener.recv())
            .await
            .unwrap()
            .unwrap();
        sqlx::query("UPDATE scheduled_events SET status='cancelled' WHERE event_id='500'")
            .execute(db.pool())
            .await
            .unwrap();
        capacity.commit().await.unwrap();
    };
    let (result, ()) = tokio::join!(
        creator.create_observed(&snapshot, &snapshot.events[0], "200"),
        change
    );
    assert!(matches!(
        result.unwrap(),
        event_voice_ticker::CreateResult::Ineligible
    ));
    assert!(
        mock.requests().is_empty(),
        "cancellation landed while admission was blocked"
    );
    let state: (Option<String>, bool) = sqlx::query_as("SELECT e.owner_tick_lease,r.settled_at IS NOT NULL FROM event_voice_channels e CROSS JOIN voice_create_reservations r WHERE e.event_id='500'").fetch_one(db.pool()).await.unwrap();
    assert_eq!(state, (None, true));
    drop(listener);
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn minute_source_create_requires_snapshot_membership_and_persists_receipt() {
    let db = database().await;
    let snapshot = queued_snapshot(db.pool()).await;
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(
            201,
            json!({"id":"700","guild_id":"100","parent_id":"200","type":2}),
        ),
    )
    .await;
    let rest = executor(&mock);
    let creator = creator(db.pool(), &rest);
    let mut forged = snapshot.events[0].clone();
    forged["event_id"] = json!("501");
    assert!(matches!(
        creator
            .create_observed(&snapshot, &forged, "200")
            .await
            .unwrap(),
        event_voice_ticker::CreateResult::Ineligible
    ));
    assert!(matches!(
        creator
            .create_observed(&snapshot, &snapshot.events[0], "200")
            .await
            .unwrap(),
        event_voice_ticker::CreateResult::Response(
            two_bot_discord::executor::EventVoiceResponse::Created(_)
        )
    ));
    let channel: String = sqlx::query_scalar(
        "SELECT voice_channel_id FROM event_voice_channels WHERE event_id='500'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(channel, "700");
    assert_eq!(mock.requests().len(), 1);
    mock.shutdown().await;
    db.close().await.unwrap();
}

fn executor(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy("synthetic-minute-source-token".into(), Some(mock.origin())).unwrap()
}

#[tokio::test]
async fn minute_source_publishes_new_event_scope_before_returning_eligibility() {
    let db = database().await;
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!([document()]))).await;
    let (_tx, shutdown) = watch::channel(false);
    let source = event_voice_source::Reader::default();
    let snapshot = source
        .read(db.pool(), &executor(&mock), "100", &shutdown)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.events.len(), 1);
    assert_eq!(snapshot.events[0]["event_id"], "500");
    assert!(snapshot.is_fresh());
    let scope: (String, String) =
        sqlx::query_as("SELECT guild_id,name FROM scheduled_events WHERE event_id='500'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(scope, ("100".into(), "Fresh event".into()));
    assert_eq!(mock.requests().len(), 1);
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn minute_source_retry_after_pauses_only_source_reads_across_ticks() {
    let db = database().await;
    let mock = MockRest::start(
        vec![ScriptedResponse::json(429, json!({"retry_after":180.25}))],
        ScriptedResponse::json(200, json!([])),
    )
    .await;
    let rest = executor(&mock);
    let (_tx, shutdown) = watch::channel(false);
    let source = event_voice_source::Reader::default();
    assert!(source
        .read(db.pool(), &rest, "100", &shutdown)
        .await
        .unwrap()
        .is_none());
    tokio::time::pause();
    tokio::time::advance(Duration::from_millis(180_249)).await;
    assert!(source
        .read(db.pool(), &rest, "100", &shutdown)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        mock.requests().len(),
        1,
        "no new source GET before full Retry-After"
    );
    // An unrelated channel GET is not paused by the source route.
    tokio::time::resume();
    assert!(rest.read_event_voice_channel(100, 700).await.is_err());
    assert_eq!(mock.requests().len(), 2);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::time::resume();
    assert!(source
        .read(db.pool(), &rest, "100", &shutdown)
        .await
        .unwrap()
        .is_some());
    assert_eq!(mock.requests().len(), 3);
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn minute_source_rejects_stale_future_and_cancelled_observations_before_publication() {
    let db = database().await;
    let (tx, shutdown) = watch::channel(false);
    for offset in [-61, 1] {
        assert!(event_voice_source::publish(
            db.pool(),
            "100",
            &json!([document()]),
            OffsetDateTime::now_utc() + time::Duration::seconds(offset),
            tokio::time::Instant::now(),
            &shutdown,
        )
        .await
        .unwrap()
        .is_none());
    }
    tx.send_replace(true);
    assert!(event_voice_source::publish(
        db.pool(),
        "100",
        &json!([document()]),
        OffsetDateTime::now_utc(),
        tokio::time::Instant::now(),
        &shutdown,
    )
    .await
    .unwrap()
    .is_none());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM scheduled_events")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
    db.close().await.unwrap();
}

#[tokio::test]
async fn minute_source_slow_read_cannot_authorize_work_or_publish_an_empty_snapshot() {
    let db = database().await;
    let (_tx, shutdown) = watch::channel(false);
    let event = document();
    let raw = raw_event(&event, "100").unwrap();
    let rows = normalize_events(&[raw]).unwrap();
    replace_events(db.pool(), "100", &now_iso(), &rows)
        .await
        .unwrap();
    let began = tokio::time::Instant::now();
    tokio::time::pause();
    tokio::time::advance(Duration::from_millis(60_001)).await;
    assert!(event_voice_source::publish(
        db.pool(),
        "100",
        &json!([]),
        OffsetDateTime::now_utc(),
        began,
        &shutdown,
    )
    .await
    .unwrap()
    .is_none());
    tokio::time::resume();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM scheduled_events")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 1, "a slow empty read must not erase the last mirror");
    db.close().await.unwrap();
}

#[tokio::test]
async fn minute_source_never_returns_old_eligibility_over_newer_mirror_mutation() {
    let db = database().await;
    let (_tx, shutdown) = watch::channel(false);
    let observed = OffsetDateTime::now_utc() - time::Duration::seconds(1);
    let mut newer = document();
    newer["name"] = json!("Cancelled meanwhile");
    newer["status"] = json!(4);
    let normalized = normalize_events(&[raw_event(&newer, "100").unwrap()]).unwrap();
    two_bot_core::website_store::upsert_event(db.pool(), "100", &now_iso(), &normalized[0])
        .await
        .unwrap();
    let snapshot = event_voice_source::publish(
        db.pool(),
        "100",
        &json!([document()]),
        observed,
        tokio::time::Instant::now(),
        &shutdown,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        snapshot.events.is_empty(),
        "older SCHEDULED source cannot authorize a cancelled event"
    );
    let name: String = sqlx::query_scalar("SELECT name FROM scheduled_events WHERE event_id='500'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(name, "Cancelled meanwhile");
    db.close().await.unwrap();
}

#[tokio::test]
async fn minute_source_lost_epoch_and_equal_delete_tombstone_return_no_eligibility() {
    let db = database().await;
    let (_tx, shutdown) = watch::channel(false);
    let observed = OffsetDateTime::now_utc() - time::Duration::seconds(1);
    replace_events(db.pool(), "100", &now_iso(), &[])
        .await
        .unwrap();
    assert!(event_voice_source::publish(
        db.pool(),
        "100",
        &json!([document()]),
        observed,
        tokio::time::Instant::now(),
        &shutdown,
    )
    .await
    .unwrap()
    .is_none());
    let observed = OffsetDateTime::now_utc();
    let stamp = two_bot_core::funnel::format_iso_millis(
        i64::try_from(observed.unix_timestamp_nanos() / 1_000_000).unwrap(),
    );
    sqlx::query("INSERT INTO scheduled_events_tombstone (guild_id,event_id,deleted_at) VALUES ('100','501',$1)")
        .bind(&stamp).execute(db.pool()).await.unwrap();
    assert!(event_voice_source::publish(
        db.pool(),
        "100",
        &json!([document()]),
        observed,
        tokio::time::Instant::now(),
        &shutdown,
    )
    .await
    .unwrap()
    .is_none());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM scheduled_events")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(
        count, 0,
        "no resurrection of a never-seen event through a suppressed source"
    );
    db.close().await.unwrap();
}

async fn seed_bound(pool: &PgPool, guild: u64, event: &str, channel: &str) -> OffsetDateTime {
    let at = (OffsetDateTime::now_utc() + time::Duration::minutes(10))
        .replace_nanosecond(0)
        .unwrap();
    claim_current_event_voice_channel(pool, event, at, at, None, "seed-lease")
        .await
        .unwrap()
        .unwrap();
    let CreateClaim::Admitted { reservation_id } = PgRoomStore::new(pool.clone())
        .claim_create(
            guild,
            999,
            &CreateAdmissionConfig::new(10, 10, 0).unwrap(),
            at.unix_timestamp(),
        )
        .await
        .unwrap()
    else {
        panic!("fixture admission");
    };
    assert!(bind_admitted_event_voice_channel(
        pool,
        &EventVoiceCreateBinding {
            guild_id: guild,
            event_id: event,
            instance_starts_at: at,
            lease: "seed-lease",
            reservation_id: &reservation_id,
            channel_id: channel,
        },
    )
    .await
    .unwrap());
    at
}

#[tokio::test]
async fn overlay_live_state_reads_open_spans_and_marks_orphaned_bindings_deleted() {
    let db = database().await;
    let at_live = seed_bound(db.pool(), 100, "501", "710").await;
    let at_dead = seed_bound(db.pool(), 100, "502", "711").await;
    record_event_voice_join(db.pool(), "710", "111")
        .await
        .unwrap();
    record_event_voice_join(db.pool(), "711", "222")
        .await
        .unwrap();
    mark_event_voice_orphaned(db.pool(), "502", at_dead)
        .await
        .unwrap();
    let ms = |at: OffsetDateTime| i64::try_from(at.unix_timestamp_nanos() / 1_000_000).unwrap();
    let mut events = vec![
        json!({"event_id":"501","binding_key_ms":ms(at_live),"voice_channel_id":"710",
            "occupancy_known":false,"occupant_ids":[]}),
        json!({"event_id":"502","binding_key_ms":ms(at_dead),"voice_channel_id":"711",
            "occupancy_known":false,"occupant_ids":[]}),
        json!({"event_id":"503","binding_key_ms":ms(at_live),"voice_channel_id":null,
            "occupancy_known":false,"occupant_ids":[]}),
    ];
    event_voice_source::overlay_live_state(db.pool(), &mut events)
        .await
        .unwrap();
    assert_eq!(events[0]["occupant_ids"], json!(["111"]));
    assert_eq!(events[0]["occupancy_known"], json!(true));
    assert_eq!(events[0]["binding_deleted"], json!(false));
    assert_eq!(
        events[1]["binding_deleted"],
        json!(true),
        "an orphaned binding is known-dead: the next create is a recreate"
    );
    assert_eq!(
        events[1]["occupant_ids"],
        json!([]),
        "stale spans on a dead channel must not block its recreate"
    );
    assert_eq!(events[1]["occupancy_known"], json!(true));
    assert_eq!(
        events[2]["occupancy_known"],
        json!(false),
        "unbound events keep the fail-closed default"
    );
    assert!(events[2].get("binding_deleted").is_none());
    db.close().await.unwrap();
}
