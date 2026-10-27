//! Durable missing-channel transitions and the one-replacement budget. These
//! tests exercise real Postgres transactions; Discord verification is the
//! caller's responsibility, never inferred from a missing mirror row.

use time::OffsetDateTime;
use two_bot_core::voice_create_admission::CreateAdmissionConfig;
use two_bot_cutover::{
    event_voice::{
        bind_admitted_event_voice_channel, bind_event_voice_channel,
        claim_current_event_voice_channel, clear_event_voice_lease,
        mark_admitted_event_voice_missing, mark_event_voice_orphaned, EventVoiceCreateBinding,
        EventVoiceMissingBinding,
    },
    voice_rooms::{CreateClaim, PgRoomStore},
};
use two_bot_testsupport::TestDatabase;

async fn database() -> Option<TestDatabase> {
    let url = std::env::var("TWO_TEST_DATABASE_URL").ok()?;
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("./migrations"))
            .await
            .unwrap(),
    )
}

async fn bind(db: &TestDatabase, at: OffsetDateTime, lease: &str, channel: &str) -> String {
    let store = PgRoomStore::new(db.pool().clone());
    let config = CreateAdmissionConfig::new(10, 10, 0).unwrap();
    let CreateClaim::Admitted { reservation_id } = store
        .claim_create(100, 300, &config, 1_800_000_000)
        .await
        .unwrap()
    else {
        panic!("fixture must admit the create");
    };
    assert!(bind_admitted_event_voice_channel(
        db.pool(),
        &EventVoiceCreateBinding {
            guild_id: 100,
            event_id: "500",
            instance_starts_at: at,
            lease,
            reservation_id: &reservation_id,
            channel_id: channel,
        },
    )
    .await
    .unwrap());
    reservation_id
}

fn missing<'a>(
    at: OffsetDateTime,
    lease: &'a str,
    channel: &'a str,
) -> EventVoiceMissingBinding<'a> {
    EventVoiceMissingBinding {
        guild_id: 100,
        event_id: "500",
        instance_starts_at: at,
        lease: Some(lease),
        channel_id: channel,
    }
}

async fn settled(db: &TestDatabase, reservation: &str) -> bool {
    sqlx::query_scalar("SELECT settled_at IS NOT NULL FROM voice_create_reservations WHERE id=$1")
        .bind(reservation)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn missing_receipt_fences_old_binding_and_atomically_releases_only_its_capacity() {
    let Some(db) = database().await else { return };
    let at = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap();
    claim_current_event_voice_channel(db.pool(), "500", at, at, None, "initial")
        .await
        .unwrap()
        .unwrap();
    let first = bind(&db, at, "initial", "700").await;
    let mut receipt = missing(at, "initial", "700");
    for (guild, lease, channel) in [
        (101, "initial", "700"),
        (100, "stale", "700"),
        (100, "initial", "799"),
    ] {
        receipt.guild_id = guild;
        receipt.lease = Some(lease);
        receipt.channel_id = channel;
        assert!(!mark_admitted_event_voice_missing(db.pool(), &receipt)
            .await
            .unwrap());
        assert!(!settled(&db, &first).await);
    }
    receipt = missing(at, "initial", "700");
    let peer = db.independent_pool().await.unwrap();
    let (a, b) = tokio::join!(
        mark_admitted_event_voice_missing(db.pool(), &receipt),
        mark_admitted_event_voice_missing(&peer, &receipt)
    );
    assert_ne!(a.unwrap(), b.unwrap(), "only one 404 transition wins");
    assert!(settled(&db, &first).await);
    assert_eq!(
        claim_current_event_voice_channel(&peer, "500", at, at, None, "replacement")
            .await
            .unwrap(),
        Some(at)
    );
    let second = bind(&db, at, "replacement", "701").await;
    assert!(!mark_admitted_event_voice_missing(db.pool(), &receipt)
        .await
        .unwrap());
    assert!(
        !settled(&db, &second).await,
        "stale 404 cannot release new capacity"
    );
    assert!(
        mark_admitted_event_voice_missing(db.pool(), &missing(at, "replacement", "701"))
            .await
            .unwrap()
    );
    assert!(settled(&db, &second).await);
    assert_eq!(
        claim_current_event_voice_channel(&peer, "500", at, at, None, "second")
            .await
            .unwrap(),
        None
    );
    peer.close().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn missing_transition_failure_rolls_back_capacity_settlement() {
    let Some(db) = database().await else { return };
    let at = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap();
    claim_current_event_voice_channel(db.pool(), "500", at, at, None, "initial")
        .await
        .unwrap()
        .unwrap();
    let first = bind(&db, at, "initial", "700").await;
    sqlx::query("ALTER TABLE event_voice_channels ADD CONSTRAINT fixture_refuse_orphan CHECK (status <> 'orphaned') NOT VALID")
        .execute(db.pool()).await.unwrap();
    assert!(
        mark_admitted_event_voice_missing(db.pool(), &missing(at, "initial", "700"))
            .await
            .is_err()
    );
    assert!(
        !settled(&db, &first).await,
        "failed orphan write must not leak capacity"
    );
    let state: (String, Option<String>) = sqlx::query_as(
        "SELECT status, owner_tick_lease FROM event_voice_channels WHERE event_id='500'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(state, ("creating".into(), Some("initial".into())));
    db.close().await.unwrap();
}

#[tokio::test]
async fn missing_receipt_cannot_release_unbound_unknown_post_or_unattributed_channel() {
    let Some(db) = database().await else { return };
    let at = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap();
    claim_current_event_voice_channel(db.pool(), "500", at, at, None, "initial")
        .await
        .unwrap()
        .unwrap();
    let store = PgRoomStore::new(db.pool().clone());
    let config = CreateAdmissionConfig::new(10, 10, 0).unwrap();
    let CreateClaim::Admitted { reservation_id } = store
        .claim_create(100, 300, &config, 1_800_000_000)
        .await
        .unwrap()
    else {
        panic!("fixture admission");
    };
    assert!(
        !mark_admitted_event_voice_missing(db.pool(), &missing(at, "initial", "700"))
            .await
            .unwrap()
    );
    assert!(!settled(&db, &reservation_id).await);
    assert!(bind_event_voice_channel(db.pool(), "500", at, "700")
        .await
        .unwrap());
    assert!(
        !mark_admitted_event_voice_missing(db.pool(), &missing(at, "initial", "700"))
            .await
            .unwrap(),
        "a binding without guild attribution is not authority"
    );
    assert!(!settled(&db, &reservation_id).await);
    let lease: Option<String> = sqlx::query_scalar(
        "SELECT owner_tick_lease FROM event_voice_channels WHERE event_id='500'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(lease.as_deref(), Some("initial"));
    db.close().await.unwrap();
}

#[tokio::test]
async fn no_effect_replacement_can_retry_but_unknown_post_keeps_its_fence() {
    let Some(db) = database().await else { return };
    let at = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap();
    claim_current_event_voice_channel(db.pool(), "500", at, at, None, "initial")
        .await
        .unwrap()
        .unwrap();
    bind(&db, at, "initial", "700").await;
    assert!(
        mark_admitted_event_voice_missing(db.pool(), &missing(at, "initial", "700"))
            .await
            .unwrap()
    );
    assert_eq!(
        claim_current_event_voice_channel(db.pool(), "500", at, at, None, "refused")
            .await
            .unwrap(),
        Some(at)
    );
    assert!(clear_event_voice_lease(db.pool(), "500", at, "refused")
        .await
        .unwrap());
    assert_eq!(
        claim_current_event_voice_channel(db.pool(), "500", at, at, None, "unknown")
            .await
            .unwrap(),
        Some(at)
    );
    let peer = db.independent_pool().await.unwrap();
    assert_eq!(
        claim_current_event_voice_channel(&peer, "500", at, at, None, "racer")
            .await
            .unwrap(),
        None
    );
    assert!(
        !mark_admitted_event_voice_missing(&peer, &missing(at, "initial", "700"))
            .await
            .unwrap()
    );
    let lease: Option<String> = sqlx::query_scalar(
        "SELECT owner_tick_lease FROM event_voice_channels WHERE event_id='500'",
    )
    .fetch_one(&peer)
    .await
    .unwrap();
    assert_eq!(lease.as_deref(), Some("unknown"));
    peer.close().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn rediscovering_the_same_channel_does_not_spend_the_replacement_budget() {
    let Some(db) = database().await else { return };
    let at = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap();
    claim_current_event_voice_channel(db.pool(), "500", at, at, None, "initial")
        .await
        .unwrap()
        .unwrap();
    bind(&db, at, "initial", "700").await;
    mark_event_voice_orphaned(db.pool(), "500", at)
        .await
        .unwrap();
    claim_current_event_voice_channel(db.pool(), "500", at, at, None, "rediscovery")
        .await
        .unwrap()
        .unwrap();
    bind(&db, at, "rediscovery", "700").await;
    assert!(
        mark_admitted_event_voice_missing(db.pool(), &missing(at, "rediscovery", "700"))
            .await
            .unwrap()
    );
    assert_eq!(
        claim_current_event_voice_channel(db.pool(), "500", at, at, None, "replacement")
            .await
            .unwrap(),
        Some(at),
        "finding the original channel is not a replacement POST"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn current_instance_recreates_a_missing_channel_only_once_across_restart_and_reschedule() {
    let Some(db) = database().await else { return };
    let at = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap();
    assert_eq!(
        claim_current_event_voice_channel(db.pool(), "500", at, at, None, "initial")
            .await
            .unwrap(),
        Some(at)
    );
    bind(&db, at, "initial", "700").await;
    mark_event_voice_orphaned(db.pool(), "500", at)
        .await
        .unwrap();
    assert_eq!(
        claim_current_event_voice_channel(db.pool(), "500", at, at, None, "replacement")
            .await
            .unwrap(),
        Some(at)
    );
    bind(&db, at, "replacement", "701").await;
    mark_event_voice_orphaned(db.pool(), "500", at)
        .await
        .unwrap();

    let peer = db.independent_pool().await.unwrap();
    let shifted = at + time::Duration::hours(1);
    assert_eq!(
        claim_current_event_voice_channel(&peer, "500", shifted, shifted, None, "second")
            .await
            .unwrap(),
        None,
        "a restart or start-time change must not reset the replacement budget"
    );
    let state: (String, Option<String>, String) = sqlx::query_as(
        "SELECT status, owner_tick_lease, voice_channel_id FROM event_voice_channels WHERE event_id='500'",
    )
    .fetch_one(&peer)
    .await
    .unwrap();
    assert_eq!(state, ("orphaned".into(), None, "701".into()));
    peer.close().await;
    db.close().await.unwrap();
}
