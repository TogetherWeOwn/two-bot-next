//! Builder-owned real-DB + fake-Discord create orchestration, not the snapshot seam.
use super::*;
use crate::discord_test_common::{MockRest, ScriptedResponse};
use event_voice_ticker::{CreateResult, TickIo};
use serde_json::json;
use time::OffsetDateTime;
use two_bot_core::voice_create_admission::CreateAdmissionConfig;
use two_bot_cutover::event_voice::{
    bind_admitted_event_voice_channel, claim_current_event_voice_channel, record_event_voice_join,
    EventVoiceCreateBinding,
};
use two_bot_cutover::voice_rooms::{CreateClaim, PgRoomStore};
use two_bot_discord::executor::EventVoiceResponse;
use two_bot_testsupport::TestDatabase;

async fn database() -> Option<TestDatabase> {
    let url = std::env::var("TWO_TEST_DATABASE_URL").ok()?;
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .unwrap(),
    )
}

fn event() -> Value {
    let starts = (OffsetDateTime::now_utc() + time::Duration::minutes(10)).unix_timestamp() * 1000;
    json!({"event_id":"500", "instance_starts_at_ms": starts, "binding_key_ms":starts,
        "name":"Scheduled voice", "entity_type":"VOICE", "status":"SCHEDULED"})
}

fn rest(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy("synthetic-durable-create-token".into(), Some(mock.origin()))
        .unwrap()
}

fn receipt() -> Value {
    json!({"id":"700", "guild_id":"100", "parent_id":"200", "type":2})
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
        admission: CreateAdmissionConfig::new(10, 10, 0).unwrap(),
    }
}

#[tokio::test]
async fn concurrent_event_creates_claim_once_and_bind_capacity_before_returning() {
    let Some(db) = database().await else { return };
    let mock = MockRest::start(vec![], ScriptedResponse::json(201, receipt())).await;
    let rest = rest(&mock);
    let creator = creator(db.pool(), &rest);
    let event = event();
    let (a, b) = tokio::join!(creator.create(&event, "200"), creator.create(&event, "200"));
    let results = [a.unwrap(), b.unwrap()];
    assert_eq!(results.iter().filter(|r| matches!(r, CreateResult::Response(EventVoiceResponse::Created(id)) if id == "700")).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, CreateResult::ClaimLost))
            .count(),
        1
    );
    assert_eq!(
        mock.requests().len(),
        1,
        "only the durable claim winner may POST"
    );
    let binding: (Option<String>,Option<String>) = sqlx::query_as("SELECT e.voice_channel_id, r.channel_id FROM event_voice_channels e JOIN voice_create_reservations r ON r.channel_id=e.voice_channel_id WHERE e.event_id='500'").fetch_one(db.pool()).await.unwrap();
    assert_eq!(binding, (Some("700".into()), Some("700".into())));
    let restarted = PgRoomStore::new(db.pool().clone());
    assert!(restarted
        .orphaned_create_channels(100)
        .await
        .unwrap()
        .is_empty());
    assert!(matches!(
        creator.create(&event, "200").await.unwrap(),
        CreateResult::ClaimLost
    ));
    assert_eq!(mock.requests().len(), 1);
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn current_instance_concurrent_unseen_reschedules_cannot_create_two_channels() {
    let Some(db) = database().await else { return };
    let peer = db.independent_pool().await.unwrap();
    let mock = MockRest::start(vec![], ScriptedResponse::json(201, receipt())).await;
    let rest = rest(&mock);
    let first = creator(db.pool(), &rest);
    let second = creator(&peer, &rest);
    // Both snapshots were read before either instance was durably claimed.
    let original = event();
    let mut shifted = original.clone();
    shifted["instance_starts_at_ms"] =
        json!(original["instance_starts_at_ms"].as_i64().unwrap() + 60_000);
    shifted["binding_key_ms"] = shifted["instance_starts_at_ms"].clone();
    let (a, b) = tokio::join!(
        first.create(&original, "200"),
        second.create(&shifted, "200")
    );
    let results = [a, b];
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(
                r,
                Ok(CreateResult::Response(EventVoiceResponse::Created(_)))
            ))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, Ok(CreateResult::ClaimLost)))
            .count(),
        1,
        "a different proposed start must still lose to the event's current claim"
    );
    assert_eq!(mock.requests().len(), 1);
    let counts: (i64, i64) = sqlx::query_as("SELECT (SELECT COUNT(*) FROM event_voice_channels), (SELECT COUNT(*) FROM voice_create_reservations)").fetch_one(db.pool()).await.unwrap();
    assert_eq!(counts, (1, 1));
    peer.close().await;
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn current_instance_unknown_post_blocks_a_rescheduled_snapshot_after_restart() {
    let Some(db) = database().await else { return };
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let rest = rest(&mock);
    let event = event();
    assert!(matches!(
        creator(db.pool(), &rest)
            .create(&event, "200")
            .await
            .unwrap(),
        CreateResult::Response(EventVoiceResponse::Uncertain)
    ));
    let before: (OffsetDateTime, Option<String>, OffsetDateTime) = sqlx::query_as(
        "SELECT instance_starts_at, owner_tick_lease, updated_at FROM event_voice_channels",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    let mut shifted = event.clone();
    shifted["instance_starts_at_ms"] =
        json!(event["instance_starts_at_ms"].as_i64().unwrap() + 60_000);
    shifted["binding_key_ms"] = shifted["instance_starts_at_ms"].clone();
    let peer = db.independent_pool().await.unwrap();
    assert!(matches!(
        creator(&peer, &rest).create(&shifted, "200").await.unwrap(),
        CreateResult::ClaimLost
    ));
    let after: (OffsetDateTime, Option<String>, OffsetDateTime) = sqlx::query_as(
        "SELECT instance_starts_at, owner_tick_lease, updated_at FROM event_voice_channels",
    )
    .fetch_one(&peer)
    .await
    .unwrap();
    assert_eq!(
        after, before,
        "losing reschedules cannot refresh unknown lease age or identity"
    );
    assert_eq!(mock.requests().len(), 1);
    let held: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM voice_create_reservations WHERE settled_at IS NULL",
    )
    .fetch_one(&peer)
    .await
    .unwrap();
    assert_eq!(held, 1);
    peer.close().await;
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn current_instance_confirmed_no_effect_retry_binds_the_original_key_after_reschedule() {
    let Some(db) = database().await else { return };
    let mock = MockRest::start(
        vec![ScriptedResponse::status(403)],
        ScriptedResponse::json(201, receipt()),
    )
    .await;
    let rest = rest(&mock);
    let event = event();
    assert!(matches!(
        creator(db.pool(), &rest)
            .create(&event, "200")
            .await
            .unwrap(),
        CreateResult::Response(EventVoiceResponse::Refused(403))
    ));
    let mut shifted = event.clone();
    shifted["instance_starts_at_ms"] =
        json!(event["instance_starts_at_ms"].as_i64().unwrap() + 60_000);
    shifted["binding_key_ms"] = shifted["instance_starts_at_ms"].clone();
    assert!(matches!(
        creator(db.pool(), &rest)
            .create(&shifted, "200")
            .await
            .unwrap(),
        CreateResult::Response(EventVoiceResponse::Created(_))
    ));
    let rows: Vec<(OffsetDateTime, OffsetDateTime, Option<String>)> = sqlx::query_as(
        "SELECT instance_starts_at, starts_at, voice_channel_id FROM event_voice_channels",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "safe retries must reuse the durable occurrence, not the stale proposed key"
    );
    assert_eq!(
        rows[0].0.unix_timestamp() * 1000,
        event["binding_key_ms"].as_i64().unwrap()
    );
    assert_eq!(
        rows[0].1.unix_timestamp() * 1000,
        shifted["instance_starts_at_ms"].as_i64().unwrap()
    );
    assert_eq!(rows[0].2.as_deref(), Some("700"));
    assert_eq!(mock.requests().len(), 2);
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn current_instance_ambiguous_or_terminal_rows_never_authorize_a_post() {
    for ambiguous in [false, true] {
        let Some(db) = database().await else { return };
        let event = event();
        let at =
            OffsetDateTime::from_unix_timestamp(event["binding_key_ms"].as_i64().unwrap() / 1000)
                .unwrap();
        for key in if ambiguous {
            vec![at, at + time::Duration::seconds(30)]
        } else {
            vec![at]
        } {
            two_bot_cutover::event_voice::claim_event_voice_channel(
                db.pool(),
                "500",
                key,
                key,
                None,
                "existing",
            )
            .await
            .unwrap();
        }
        if !ambiguous {
            sqlx::query("UPDATE event_voice_channels SET status='closed', owner_tick_lease=NULL")
                .execute(db.pool())
                .await
                .unwrap();
        }
        let before: Vec<(OffsetDateTime, String, Option<String>)> = sqlx::query_as("SELECT instance_starts_at,status,owner_tick_lease FROM event_voice_channels ORDER BY instance_starts_at").fetch_all(db.pool()).await.unwrap();
        let mock = MockRest::start(vec![], ScriptedResponse::json(201, receipt())).await;
        let rest = rest(&mock);
        let mut snapshot = event.clone();
        if ambiguous {
            snapshot["binding_key_ms"] = json!(event["binding_key_ms"].as_i64().unwrap() + 60_000);
        }
        assert!(matches!(
            creator(db.pool(), &rest)
                .create(&snapshot, "200")
                .await
                .unwrap(),
            CreateResult::ClaimLost
        ));
        assert!(mock.requests().is_empty());
        let after: Vec<(OffsetDateTime, String, Option<String>)> = sqlx::query_as("SELECT instance_starts_at,status,owner_tick_lease FROM event_voice_channels ORDER BY instance_starts_at").fetch_all(db.pool()).await.unwrap();
        assert_eq!(after, before);
        let reserved: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM voice_create_reservations")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(reserved, 0);
        mock.shutdown().await;
        db.close().await.unwrap();
    }
}

#[tokio::test]
async fn admission_refusal_releases_only_the_event_claim_without_posting() {
    let Some(db) = database().await else { return };
    let mock = MockRest::start(vec![], ScriptedResponse::json(201, receipt())).await;
    let rest = rest(&mock);
    let mut creator = creator(db.pool(), &rest);
    creator.admission = CreateAdmissionConfig::new(10, 1, 0).unwrap();
    PgRoomStore::new(db.pool().clone())
        .claim_create(
            100,
            998,
            &creator.admission,
            OffsetDateTime::now_utc().unix_timestamp(),
        )
        .await
        .unwrap();
    assert!(matches!(
        creator.create(&event(), "200").await.unwrap(),
        CreateResult::AdmissionDenied
    ));
    let lease: Option<String> = sqlx::query_scalar(
        "SELECT owner_tick_lease FROM event_voice_channels WHERE event_id='500'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        lease, None,
        "known no-POST admission refusal must remain retryable"
    );
    assert!(mock.requests().is_empty());
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM voice_create_reservations WHERE settled_at IS NULL",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        count, 1,
        "must not release the unrelated existing reservation"
    );
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn confirmed_create_refusals_settle_capacity_and_clear_the_lease_for_a_paced_retry() {
    for status in [403, 429] {
        let Some(db) = database().await else { return };
        let mock = MockRest::start(
            vec![ScriptedResponse::json(status, json!({"retry_after":180}))],
            ScriptedResponse::json(201, receipt()),
        )
        .await;
        let rest = rest(&mock);
        let creator = creator(db.pool(), &rest);
        let event = event();
        let response = creator.create(&event, "200").await.unwrap();
        assert!(matches!(
            response,
            CreateResult::Response(
                EventVoiceResponse::Refused(403)
                    | EventVoiceResponse::RateLimited {
                        retry_after_ms: 180_000
                    }
            )
        ));
        let state: (Option<String>,bool) = sqlx::query_as("SELECT e.owner_tick_lease, r.settled_at IS NOT NULL FROM event_voice_channels e CROSS JOIN voice_create_reservations r WHERE e.event_id='500'").fetch_one(db.pool()).await.unwrap();
        assert_eq!(state, (None, true));
        assert!(matches!(
            creator.create(&event, "200").await.unwrap(),
            CreateResult::Response(EventVoiceResponse::Created(_))
        ));
        assert_eq!(
            mock.requests().len(),
            2,
            "adapter itself never retries a request"
        );
        mock.shutdown().await;
        db.close().await.unwrap();
    }
}

#[tokio::test]
async fn uncertain_create_keeps_both_fences_and_cannot_duplicate_after_restart() {
    for (status, body) in [
        (500, json!({})),
        (
            201,
            json!({"id":"700","guild_id":"100","parent_id":"201","type":2}),
        ),
    ] {
        let Some(db) = database().await else { return };
        let mock = MockRest::start(vec![], ScriptedResponse::json(status, body)).await;
        let rest = rest(&mock);
        let creator = creator(db.pool(), &rest);
        let event = event();
        assert!(matches!(
            creator.create(&event, "200").await.unwrap(),
            CreateResult::Response(EventVoiceResponse::Uncertain)
        ));
        let state: (Option<String>,Option<String>,bool) = sqlx::query_as("SELECT e.owner_tick_lease, e.voice_channel_id, r.settled_at IS NULL FROM event_voice_channels e CROSS JOIN voice_create_reservations r WHERE e.event_id='500'").fetch_one(db.pool()).await.unwrap();
        assert!(state.0.is_some());
        assert_eq!(state.1, None);
        assert!(state.2, "uncertainty must retain capacity");
        let peer = db.independent_pool().await.unwrap();
        assert!(matches!(
            super::event_voice_create_runtime::Creator {
                pool: &peer,
                ..creator
            }
            .create(&event, "200")
            .await
            .unwrap(),
            CreateResult::ClaimLost
        ));
        assert_eq!(mock.requests().len(), 1);
        peer.close().await;
        mock.shutdown().await;
        db.close().await.unwrap();
    }
}

#[tokio::test]
async fn cancelled_post_preserves_capacity_and_claim_across_an_independent_pool() {
    let Some(db) = database().await else { return };
    let (mock, response) = MockRest::start_gated(
        vec![ScriptedResponse::json(201, receipt())],
        ScriptedResponse::json(201, receipt()),
    )
    .await;
    let rest = rest(&mock);
    let creator = creator(db.pool(), &rest);
    let event = event();
    {
        let create = creator.create(&event, "200");
        tokio::pin!(create);
        tokio::select! {
            result = &mut create => panic!("gated POST returned early: {}", result.is_ok()),
            () = response.wait_for_request() => {}
        }
        // Dropping the caller here is the supervisor's timeout/cancellation.
    }
    let peer = db.independent_pool().await.unwrap();
    let state: (bool, bool) = sqlx::query_as("SELECT e.owner_tick_lease IS NOT NULL, r.settled_at IS NULL FROM event_voice_channels e CROSS JOIN voice_create_reservations r WHERE e.event_id='500'").fetch_one(&peer).await.unwrap();
    assert_eq!(state, (true, true));
    assert!(matches!(
        super::event_voice_create_runtime::Creator {
            pool: &peer,
            ..creator
        }
        .create(&event, "200")
        .await
        .unwrap(),
        CreateResult::ClaimLost
    ));
    assert_eq!(mock.requests().len(), 1);
    response.release();
    peer.close().await;
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn lost_binding_fence_never_releases_capacity_or_clears_a_newer_lease() {
    let Some(db) = database().await else { return };
    let (mock, response) = MockRest::start_gated(
        vec![ScriptedResponse::json(201, receipt())],
        ScriptedResponse::json(201, receipt()),
    )
    .await;
    let rest = rest(&mock);
    let creator = creator(db.pool(), &rest);
    let event = event();
    let create = creator.create(&event, "200");
    let lose_fence = async {
        response.wait_for_request().await;
        sqlx::query(
            "UPDATE event_voice_channels SET owner_tick_lease='newer-tick' WHERE event_id='500'",
        )
        .execute(db.pool())
        .await
        .unwrap();
        response.release();
    };
    let (result, ()) = tokio::join!(create, lose_fence);
    assert!(matches!(result, Err(ErrorClass::Database)));
    let state: (Option<String>, Option<String>, bool) = sqlx::query_as("SELECT e.owner_tick_lease, r.channel_id, r.settled_at IS NULL FROM event_voice_channels e CROSS JOIN voice_create_reservations r WHERE e.event_id='500'").fetch_one(db.pool()).await.unwrap();
    assert_eq!(state, (Some("newer-tick".into()), None, true));
    assert!(matches!(
        creator.create(&event, "200").await.unwrap(),
        CreateResult::ClaimLost
    ));
    assert_eq!(mock.requests().len(), 1);
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn expired_or_malformed_create_input_never_claims_or_calls_discord() {
    let Some(db) = database().await else { return };
    let mock = MockRest::start(vec![], ScriptedResponse::json(201, receipt())).await;
    let rest = rest(&mock);
    let creator = creator(db.pool(), &rest);
    let mut expired = event();
    expired["instance_starts_at_ms"] =
        json!((OffsetDateTime::now_utc().unix_timestamp() - 1) * 1000);
    assert!(matches!(
        creator.create(&expired, "200").await.unwrap(),
        CreateResult::Ineligible
    ));
    let mut malformed = event();
    malformed["event_id"] = json!("0500");
    assert!(matches!(
        creator.create(&malformed, "200").await,
        Err(ErrorClass::Rest)
    ));
    assert!(mock.requests().is_empty());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM event_voice_channels")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn durable_guild_cap_denies_an_eleventh_live_channel_without_posting() {
    let Some(db) = database().await else { return };
    let mock = MockRest::start(vec![], ScriptedResponse::json(201, receipt())).await;
    let rest = rest(&mock);
    let mut creator = creator(db.pool(), &rest);
    // Global room caps stay wide open: only the event-voice durable cap may deny.
    creator.admission = CreateAdmissionConfig::new(10, 45, 0).unwrap();
    let at = OffsetDateTime::now_utc() + time::Duration::weeks(2);
    for i in 0..10 {
        let event_id = format!("600{i:02}");
        let channel = format!("820{i:02}");
        let claimed = two_bot_cutover::event_voice::claim_event_voice_channel(
            db.pool(),
            &event_id,
            at,
            at,
            None,
            "tick-cap",
        )
        .await
        .expect("claim runs");
        assert!(claimed);
        assert!(two_bot_cutover::event_voice::bind_event_voice_channel(
            db.pool(),
            &event_id,
            at,
            &channel
        )
        .await
        .expect("bind runs"));
        // Backdated so the 60s guild burst (10/window) does not deny first.
        sqlx::query(
            "INSERT INTO voice_create_reservations (guild_id, user_id, created_at, channel_id) \
             VALUES ('100', '9991', NOW() - make_interval(mins => 5), $1)",
        )
        .bind(&channel)
        .execute(db.pool())
        .await
        .expect("reservation seeds");
    }
    assert!(matches!(
        creator.create(&event(), "200").await.unwrap(),
        CreateResult::AdmissionDenied
    ));
    assert!(
        mock.requests().is_empty(),
        "durable cap denial must not POST"
    );
    // The denied candidate leaves no claim row: the next tick retries cleanly.
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM event_voice_channels WHERE event_id='500'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(count, 0);
    mock.shutdown().await;
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

async fn bound_snapshot(pool: &PgPool, event: &str) -> event_voice_source::Snapshot {
    let (_tx, shutdown) = watch::channel(false);
    let starts = two_bot_core::funnel::format_iso_millis(
        i64::try_from(
            (OffsetDateTime::now_utc() + time::Duration::minutes(10)).unix_timestamp_nanos()
                / 1_000_000,
        )
        .unwrap(),
    );
    event_voice_source::publish(
        pool,
        "100",
        &json!([{
            "id": event, "guild_id": "100", "name": "Bound event", "entity_type": 2,
            "status": 1, "scheduled_start_time": starts,
            "scheduled_end_time": null, "channel_id": null
        }]),
        OffsetDateTime::now_utc(),
        tokio::time::Instant::now(),
        &shutdown,
    )
    .await
    .unwrap()
    .unwrap()
}

fn durable<'a>(
    pool: &'a PgPool,
    rest: &'a ActionExecutor,
    snapshot: &'a event_voice_source::Snapshot,
) -> event_voice_create_runtime::DurableTickIo<'a> {
    event_voice_create_runtime::DurableTickIo {
        pool,
        rest,
        guild: "100",
        snapshot,
        actor: 999,
        admission: CreateAdmissionConfig::new(10, 10, 0).unwrap(),
    }
}

#[tokio::test]
async fn guarded_delete_keeps_occupied_channel_and_issues_no_delete() {
    let Some(db) = database().await else { return };
    let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
    let rest = rest(&mock);
    seed_bound(db.pool(), 100, "500", "700").await;
    record_event_voice_join(db.pool(), "700", "111")
        .await
        .unwrap();
    let snapshot = bound_snapshot(db.pool(), "500").await;
    let outcome = durable(db.pool(), &rest, &snapshot)
        .delete(&snapshot.events[0], "700")
        .await
        .unwrap();
    assert!(
        matches!(outcome, EventVoiceResponse::Uncertain),
        "an occupied channel must never be deleted"
    );
    assert!(
        mock.requests().is_empty(),
        "no DELETE may be issued for an occupied channel"
    );
    let status: String =
        sqlx::query_scalar("SELECT status FROM event_voice_channels WHERE event_id='500'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(status, "creating");
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn guarded_delete_removes_empty_channel_and_retires_binding_and_capacity() {
    let Some(db) = database().await else { return };
    let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
    let rest = rest(&mock);
    seed_bound(db.pool(), 100, "500", "700").await;
    let snapshot = bound_snapshot(db.pool(), "500").await;
    let outcome = durable(db.pool(), &rest, &snapshot)
        .delete(&snapshot.events[0], "700")
        .await
        .unwrap();
    assert!(
        matches!(outcome, EventVoiceResponse::Deleted),
        "an empty far-future channel must be deleted"
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "DELETE");
    assert!(requests[0].path.ends_with("/channels/700"));
    let row: (String, Option<String>) = sqlx::query_as(
        "SELECT status, owner_tick_lease FROM event_voice_channels WHERE event_id='500'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(row, ("orphaned".to_owned(), None));
    let settled: bool = sqlx::query_scalar(
        "SELECT settled_at IS NOT NULL FROM voice_create_reservations WHERE channel_id='700'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert!(
        settled,
        "the purposeful delete must release its capacity witness"
    );
    mock.shutdown().await;
    db.close().await.unwrap();
}
