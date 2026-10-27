//! Builder-owned reconciliation tests: real transactions and a fake Discord wire.
use super::*;
use crate::discord_test_common::{MockRest, ScriptedResponse};
use event_voice_reconcile::Reconciler;
use serde_json::json;
use time::OffsetDateTime;
use two_bot_core::voice_create_admission::CreateAdmissionConfig;
use two_bot_cutover::{
    event_voice::{
        bind_admitted_event_voice_channel, claim_current_event_voice_channel,
        EventVoiceCreateBinding,
    },
    voice_rooms::{CreateClaim, PgRoomStore},
};
use two_bot_testsupport::TestDatabase;

async fn database() -> Option<TestDatabase> {
    let url = std::env::var("TWO_TEST_DATABASE_URL").ok()?;
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .unwrap(),
    )
}

fn rest(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy("synthetic-reconcile-token".into(), Some(mock.origin())).unwrap()
}

async fn bind(db: &TestDatabase, guild: u64, event: &str, channel: &str) -> OffsetDateTime {
    let at = (OffsetDateTime::now_utc() + time::Duration::minutes(10))
        .replace_nanosecond(0)
        .unwrap();
    claim_current_event_voice_channel(db.pool(), event, at, at, None, "initial")
        .await
        .unwrap()
        .unwrap();
    let CreateClaim::Admitted { reservation_id } = PgRoomStore::new(db.pool().clone())
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
        db.pool(),
        &EventVoiceCreateBinding {
            guild_id: guild,
            event_id: event,
            instance_starts_at: at,
            lease: "initial",
            reservation_id: &reservation_id,
            channel_id: channel,
        },
    )
    .await
    .unwrap());
    at
}

async fn state(db: &TestDatabase, event: &str) -> (String, Option<String>, bool) {
    sqlx::query_as(
        "SELECT e.status,e.owner_tick_lease,r.settled_at IS NOT NULL
         FROM event_voice_channels e JOIN voice_create_reservations r
         ON r.channel_id=e.voice_channel_id WHERE e.event_id=$1",
    )
    .bind(event)
    .fetch_one(db.pool())
    .await
    .unwrap()
}

#[tokio::test]
async fn live_reconcile_404_retires_attributable_binding_and_releases_capacity_without_delete() {
    let Some(db) = database().await else { return };
    let at = bind(&db, 100, "500", "700").await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(404)).await;
    let rest = rest(&mock);
    let (_tx, shutdown) = watch::channel(false);
    let result = Reconciler::default()
        .run(db.pool(), &rest, 100, &shutdown)
        .await
        .unwrap();
    assert_eq!(result.missing.len(), 1);
    assert_eq!(result.missing[0].event_id, "500");
    assert_eq!(result.missing[0].instance, at);
    assert_eq!(result.missing[0].channel_id, "700");
    assert_eq!(state(&db, "500").await, ("orphaned".into(), None, true));
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
    assert!(requests[0].path.ends_with("/channels/700"));
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn live_reconcile_never_treats_unavailable_or_mismatched_reads_as_missing() {
    for response in [
        ScriptedResponse::status(403),
        ScriptedResponse::status(500),
        ScriptedResponse::json(200, json!({"id":"700", "guild_id":"100", "type":2})),
        ScriptedResponse::json(200, json!({"id":"701", "guild_id":"100", "type":2})),
        ScriptedResponse::json(200, json!({"id":"700", "guild_id":"101", "type":2})),
    ] {
        let Some(db) = database().await else { return };
        bind(&db, 100, "500", "700").await;
        let mock = MockRest::start(vec![], response).await;
        let rest = rest(&mock);
        let (_tx, shutdown) = watch::channel(false);
        let report = Reconciler::default()
            .run(db.pool(), &rest, 100, &shutdown)
            .await
            .unwrap();
        assert!(report.missing.is_empty());
        assert_eq!(
            state(&db, "500").await,
            ("creating".into(), Some("initial".into()), false)
        );
        assert_eq!(mock.requests().len(), 1);
        assert_eq!(mock.requests()[0].method, "GET");
        mock.shutdown().await;
        db.close().await.unwrap();
    }
}

#[tokio::test]
async fn live_reconcile_channel_429_pauses_only_that_get_across_ticks() {
    let Some(db) = database().await else { return };
    bind(&db, 100, "500", "700").await;
    bind(&db, 100, "501", "701").await;
    let mock = MockRest::start(
        vec![ScriptedResponse::json(429, json!({"retry_after":90.0}))],
        ScriptedResponse::json(200, json!({"id":"701", "guild_id":"100", "type":2})),
    )
    .await;
    let rest = rest(&mock);
    let (_tx, shutdown) = watch::channel(false);
    let reconciler = Reconciler::default();
    let first = reconciler
        .run(db.pool(), &rest, 100, &shutdown)
        .await
        .unwrap();
    let second = reconciler
        .run(db.pool(), &rest, 100, &shutdown)
        .await
        .unwrap();
    assert!(first.missing.is_empty());
    assert!(second.missing.is_empty());
    assert_eq!(first.rate_limited, 1);
    assert_eq!(second.rate_limited, 1);
    let requests = mock.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.path.ends_with("/channels/700"))
            .count(),
        1,
        "a subsequent tick must retain the route's Retry-After"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.path.ends_with("/channels/701"))
            .count(),
        2,
        "another channel GET remains available"
    );
    assert_eq!(
        state(&db, "500").await,
        ("creating".into(), Some("initial".into()), false)
    );
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(91)).await;
    tokio::time::resume();
    let after = reconciler
        .run(db.pool(), &rest, 100, &shutdown)
        .await
        .unwrap();
    assert_eq!(after.rate_limited, 0);
    assert_eq!(
        mock.requests()
            .iter()
            .filter(|r| r.path.ends_with("/channels/700"))
            .count(),
        2,
        "the route reopens after the complete Retry-After delay"
    );
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn live_reconcile_reports_forbidden_and_unavailable_without_crashing_the_tick() {
    for (status, expected) in [(403, (1, 0)), (500, (0, 1))] {
        let Some(db) = database().await else { return };
        bind(&db, 100, "500", "700").await;
        let mock = MockRest::start(vec![], ScriptedResponse::status(status)).await;
        let rest = rest(&mock);
        let (_tx, shutdown) = watch::channel(false);
        let report = Reconciler::default()
            .run(db.pool(), &rest, 100, &shutdown)
            .await
            .unwrap();
        assert_eq!((report.perm_denied, report.unavailable), expected);
        assert_eq!(report.rate_limited, 0);
        assert!(report.missing.is_empty());
        mock.shutdown().await;
        db.close().await.unwrap();
    }
}

#[tokio::test]
async fn live_reconcile_stale_404_cannot_clear_a_newer_lease_or_release_capacity() {
    let Some(db) = database().await else { return };
    bind(&db, 100, "500", "700").await;
    let (mock, response) = MockRest::start_gated(
        vec![ScriptedResponse::status(404)],
        ScriptedResponse::status(404),
    )
    .await;
    let rest = rest(&mock);
    let (_tx, shutdown) = watch::channel(false);
    let reconciler = Reconciler::default();
    let probe = reconciler.run(db.pool(), &rest, 100, &shutdown);
    let change = async {
        response.wait_for_request().await;
        sqlx::query(
            "UPDATE event_voice_channels SET owner_tick_lease='newer' WHERE event_id='500'",
        )
        .execute(db.pool())
        .await
        .unwrap();
        response.release();
    };
    let (report, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(probe, change)
    })
    .await
    .unwrap();
    assert!(report.unwrap().missing.is_empty());
    assert_eq!(
        state(&db, "500").await,
        ("creating".into(), Some("newer".into()), false)
    );
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn live_reconcile_shutdown_during_get_preserves_the_binding_and_capacity() {
    let Some(db) = database().await else { return };
    bind(&db, 100, "500", "700").await;
    let (mock, response) = MockRest::start_gated(
        vec![ScriptedResponse::status(404)],
        ScriptedResponse::status(404),
    )
    .await;
    let rest = rest(&mock);
    let (tx, shutdown) = watch::channel(false);
    let reconciler = Reconciler::default();
    let probe = reconciler.run(db.pool(), &rest, 100, &shutdown);
    let stop = async {
        response.wait_for_request().await;
        tx.send_replace(true);
        response.release();
    };
    let (report, ()) =
        tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(probe, stop) })
            .await
            .unwrap();
    assert!(report.unwrap().missing.is_empty());
    assert_eq!(
        state(&db, "500").await,
        ("creating".into(), Some("initial".into()), false)
    );
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn live_reconcile_persistence_failure_cannot_publish_missing_or_release_capacity() {
    let Some(db) = database().await else { return };
    bind(&db, 100, "500", "700").await;
    sqlx::query("ALTER TABLE event_voice_channels ADD CONSTRAINT fixture_reject_orphan CHECK (status <> 'orphaned') NOT VALID")
        .execute(db.pool()).await.unwrap();
    let mock = MockRest::start(vec![], ScriptedResponse::status(404)).await;
    let rest = rest(&mock);
    let (_tx, shutdown) = watch::channel(false);
    assert!(matches!(
        Reconciler::default()
            .run(db.pool(), &rest, 100, &shutdown)
            .await,
        Err(ErrorClass::Database)
    ));
    assert_eq!(
        state(&db, "500").await,
        ("creating".into(), Some("initial".into()), false)
    );
    mock.shutdown().await;
    db.close().await.unwrap();
}

async fn orphan(db: &TestDatabase) -> OffsetDateTime {
    let at = bind(db, 100, "500", "700").await;
    assert!(
        two_bot_cutover::event_voice::mark_admitted_event_voice_missing(
            db.pool(),
            &two_bot_cutover::event_voice::EventVoiceMissingBinding {
                guild_id: 100,
                event_id: "500",
                instance_starts_at: at,
                channel_id: "700",
                lease: Some("initial"),
            },
        )
        .await
        .unwrap()
    );
    at
}

#[tokio::test]
async fn orphan_restart_requires_a_new_404_before_publishing_missing() {
    let Some(db) = database().await else { return };
    let at = orphan(&db).await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(404)).await;
    let rest = rest(&mock);
    let (_tx, shutdown) = watch::channel(false);
    let peer = db.independent_pool().await.unwrap();
    let report = Reconciler::default()
        .run(&peer, &rest, 100, &shutdown)
        .await
        .unwrap();
    assert_eq!(
        report.missing.len(),
        1,
        "SQL orphan status alone is not fresh absence evidence"
    );
    assert_eq!(report.missing[0].instance, at);
    assert_eq!(mock.requests().len(), 1);
    assert_eq!(mock.requests()[0].method, "GET");
    assert_eq!(state(&db, "500").await, ("orphaned".into(), None, true));
    peer.close().await;
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn orphan_restart_live_channel_restores_capacity_without_post_or_replacement_budget() {
    let Some(db) = database().await else { return };
    orphan(&db).await;
    sqlx::query("UPDATE event_voice_channels SET recreated_at=NOW()-INTERVAL '1 hour'")
        .execute(db.pool())
        .await
        .unwrap();
    let before: OffsetDateTime =
        sqlx::query_scalar("SELECT recreated_at FROM event_voice_channels")
            .fetch_one(db.pool())
            .await
            .unwrap();
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(200, json!({"id":"700", "guild_id":"100", "type":2})),
    )
    .await;
    let rest = rest(&mock);
    let (_tx, shutdown) = watch::channel(false);
    let peer = db.independent_pool().await.unwrap();
    let report = Reconciler::default()
        .run(&peer, &rest, 100, &shutdown)
        .await
        .unwrap();
    assert!(report.missing.is_empty());
    assert_eq!(state(&db, "500").await, ("creating".into(), None, false));
    let (rows, held): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*),COUNT(*) FILTER (WHERE settled_at IS NULL) FROM voice_create_reservations",
    )
    .fetch_one(&peer)
    .await
    .unwrap();
    assert_eq!(
        (rows, held),
        (1, 1),
        "restore the old capacity witness, not a new create"
    );
    let after: OffsetDateTime = sqlx::query_scalar("SELECT recreated_at FROM event_voice_channels")
        .fetch_one(&peer)
        .await
        .unwrap();
    assert_eq!(
        after, before,
        "same-channel rediscovery never resets the one-replacement budget"
    );
    assert!(
        claim_current_event_voice_channel(&peer, "500", before, before, None, "duplicate")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(mock.requests().len(), 1);
    assert_eq!(mock.requests()[0].method, "GET");
    peer.close().await;
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn orphan_restart_unknown_replacement_lease_is_not_recovered_by_old_channel_get() {
    let Some(db) = database().await else { return };
    let at = orphan(&db).await;
    assert!(claim_current_event_voice_channel(
        db.pool(),
        "500",
        at,
        at,
        None,
        "unknown-replacement"
    )
    .await
    .unwrap()
    .is_some());
    let CreateClaim::Admitted { reservation_id } = PgRoomStore::new(db.pool().clone())
        .claim_create(
            100,
            999,
            &CreateAdmissionConfig::new(10, 10, 0).unwrap(),
            at.unix_timestamp(),
        )
        .await
        .unwrap()
    else {
        panic!("replacement fixture admission");
    };
    sqlx::query("UPDATE event_voice_channels SET updated_at=NOW()-INTERVAL '1 day'")
        .execute(db.pool())
        .await
        .unwrap();
    let mock = MockRest::start(vec![], ScriptedResponse::status(404)).await;
    let rest = rest(&mock);
    let (_tx, shutdown) = watch::channel(false);
    let report = Reconciler::default()
        .run(db.pool(), &rest, 100, &shutdown)
        .await
        .unwrap();
    assert!(report.missing.is_empty());
    assert!(
        mock.requests().is_empty(),
        "old channel absence cannot resolve an uncertain replacement POST"
    );
    assert_eq!(
        state(&db, "500").await,
        ("orphaned".into(), Some("unknown-replacement".into()), true)
    );
    let held: bool = sqlx::query_scalar(
        "SELECT settled_at IS NULL AND channel_id IS NULL FROM voice_create_reservations WHERE id=$1")
        .bind(reservation_id).fetch_one(db.pool()).await.unwrap();
    assert!(held);
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn orphan_restart_failed_get_never_restores_or_publishes_absence() {
    let Some(db) = database().await else { return };
    orphan(&db).await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(403)).await;
    let rest = rest(&mock);
    let (_tx, shutdown) = watch::channel(false);
    let report = Reconciler::default()
        .run(db.pool(), &rest, 100, &shutdown)
        .await
        .unwrap();
    assert!(report.missing.is_empty());
    assert_eq!(report.perm_denied, 1);
    assert_eq!(state(&db, "500").await, ("orphaned".into(), None, true));
    assert_eq!(mock.requests().len(), 1);
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn orphan_restart_stale_get_cannot_revive_or_clear_a_new_replacement_lease() {
    for response in [
        ScriptedResponse::status(404),
        ScriptedResponse::json(200, json!({"id":"700", "guild_id":"100", "type":2})),
    ] {
        let Some(db) = database().await else { return };
        let at = orphan(&db).await;
        let (mock, release) =
            MockRest::start_gated(vec![response], ScriptedResponse::status(500)).await;
        let rest = rest(&mock);
        let (_tx, shutdown) = watch::channel(false);
        let reconciler = Reconciler::default();
        let probe = reconciler.run(db.pool(), &rest, 100, &shutdown);
        let race = async {
            release.wait_for_request().await;
            assert!(claim_current_event_voice_channel(
                db.pool(),
                "500",
                at,
                at,
                None,
                "racing-replacement"
            )
            .await
            .unwrap()
            .is_some());
            release.release();
        };
        let (report, ()) =
            tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(probe, race) })
                .await
                .unwrap();
        assert!(report.unwrap().missing.is_empty());
        assert_eq!(
            state(&db, "500").await,
            ("orphaned".into(), Some("racing-replacement".into()), true)
        );
        mock.shutdown().await;
        db.close().await.unwrap();
    }
}

#[tokio::test]
async fn orphan_restart_restore_failure_rolls_back_capacity_reactivation() {
    let Some(db) = database().await else { return };
    orphan(&db).await;
    sqlx::query("ALTER TABLE event_voice_channels ADD CONSTRAINT fixture_reject_restore CHECK (status <> 'creating') NOT VALID")
        .execute(db.pool()).await.unwrap();
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(200, json!({"id":"700", "guild_id":"100", "type":2})),
    )
    .await;
    let rest = rest(&mock);
    let (_tx, shutdown) = watch::channel(false);
    assert!(matches!(
        Reconciler::default()
            .run(db.pool(), &rest, 100, &shutdown)
            .await,
        Err(ErrorClass::Database)
    ));
    assert_eq!(state(&db, "500").await, ("orphaned".into(), None, true));
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn orphan_restart_restore_requires_a_matching_guild_capacity_witness() {
    let Some(db) = database().await else { return };
    let at = orphan(&db).await;
    let receipt = |guild| two_bot_cutover::event_voice::EventVoiceMissingBinding {
        guild_id: guild,
        event_id: "500",
        instance_starts_at: at,
        channel_id: "700",
        lease: None,
    };
    assert!(
        !two_bot_cutover::event_voice::restore_admitted_event_voice_channel(
            db.pool(),
            &receipt(101)
        )
        .await
        .unwrap()
    );
    assert_eq!(state(&db, "500").await, ("orphaned".into(), None, true));
    sqlx::query("UPDATE voice_create_reservations SET channel_id='701'")
        .execute(db.pool())
        .await
        .unwrap();
    assert!(
        !two_bot_cutover::event_voice::restore_admitted_event_voice_channel(
            db.pool(),
            &receipt(100)
        )
        .await
        .unwrap()
    );
    let state: (String, Option<String>, String, bool) = sqlx::query_as(
        "SELECT e.status,e.owner_tick_lease,r.channel_id,r.settled_at IS NOT NULL
         FROM event_voice_channels e CROSS JOIN voice_create_reservations r",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(state, ("orphaned".into(), None, "701".into(), true));
    db.close().await.unwrap();
}

#[tokio::test]
async fn live_reconcile_does_not_probe_other_guild_or_clear_unbound_unknown_post() {
    let Some(db) = database().await else { return };
    bind(&db, 101, "501", "701").await;
    let at = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap();
    claim_current_event_voice_channel(db.pool(), "500", at, at, None, "unknown")
        .await
        .unwrap()
        .unwrap();
    let CreateClaim::Admitted { reservation_id } = PgRoomStore::new(db.pool().clone())
        .claim_create(
            100,
            999,
            &CreateAdmissionConfig::new(10, 10, 0).unwrap(),
            at.unix_timestamp(),
        )
        .await
        .unwrap()
    else {
        panic!("unknown POST capacity fixture");
    };
    sqlx::query(
        "UPDATE event_voice_channels SET updated_at=NOW()-INTERVAL '1 day' WHERE event_id='500'",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let mock = MockRest::start(vec![], ScriptedResponse::status(404)).await;
    let rest = rest(&mock);
    let (_tx, shutdown) = watch::channel(false);
    assert!(Reconciler::default()
        .run(db.pool(), &rest, 100, &shutdown)
        .await
        .unwrap()
        .missing
        .is_empty());
    assert!(mock.requests().is_empty());
    let lease: Option<String> = sqlx::query_scalar(
        "SELECT owner_tick_lease FROM event_voice_channels WHERE event_id='500'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(lease.as_deref(), Some("unknown"));
    let held: bool = sqlx::query_scalar(
        "SELECT settled_at IS NULL AND channel_id IS NULL FROM voice_create_reservations WHERE id=$1",
    )
    .bind(reservation_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert!(held, "unknown POST capacity is not released by lease age");
    assert_eq!(
        state(&db, "501").await,
        ("creating".into(), Some("initial".into()), false)
    );
    mock.shutdown().await;
    db.close().await.unwrap();
}
