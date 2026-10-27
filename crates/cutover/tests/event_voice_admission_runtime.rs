//! Event channel receipts share durable room-create capacity without becoming
//! compensation-delete candidates when the ordinary room worker restarts.

use sqlx::PgPool;
use time::OffsetDateTime;
use two_bot_core::voice_create_admission::{CreateAdmissionConfig, RefusalReason};
use two_bot_cutover::{
    event_voice::{
        bind_admitted_event_voice_channel, claim_event_voice_channel, EventVoiceCreateBinding,
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

async fn reserve(store: &PgRoomStore, guild: u64, user: u64) -> String {
    let config = CreateAdmissionConfig::new(10, 10, 0).unwrap();
    match store
        .claim_create(guild, user, &config, 1_800_000_000)
        .await
        .unwrap()
    {
        CreateClaim::Admitted { reservation_id } => reservation_id,
        other => panic!("unexpected admission refusal: {other:?}"),
    }
}

async fn binding(pool: &PgPool, at: OffsetDateTime) -> Option<String> {
    sqlx::query_scalar("SELECT voice_channel_id FROM event_voice_channels WHERE event_id = '500' AND instance_starts_at = $1")
        .bind(at).fetch_one(pool).await.unwrap()
}

fn receipt(reservation: &str, at: OffsetDateTime) -> EventVoiceCreateBinding<'_> {
    EventVoiceCreateBinding {
        guild_id: 100,
        event_id: "500",
        instance_starts_at: at,
        lease: "tick-a",
        reservation_id: reservation,
        channel_id: "700",
    }
}

#[tokio::test]
async fn event_receipt_holds_capacity_without_becoming_worker_compensation() {
    let Some(db) = database().await else { return };
    let store = PgRoomStore::new(db.pool().clone());
    let at = OffsetDateTime::now_utc();
    assert!(
        claim_event_voice_channel(db.pool(), "500", at, at, None, "tick-a")
            .await
            .unwrap()
    );
    let reservation = reserve(&store, 100, 300).await;
    assert!(
        bind_admitted_event_voice_channel(db.pool(), &receipt(&reservation, at))
            .await
            .unwrap()
    );
    assert_eq!(binding(db.pool(), at).await.as_deref(), Some("700"));
    assert!(
        bind_admitted_event_voice_channel(db.pool(), &receipt(&reservation, at))
            .await
            .unwrap(),
        "the same confirmed receipt is idempotent"
    );
    let peer = db.independent_pool().await.unwrap();
    let restarted = PgRoomStore::new(peer.clone());
    assert!(restarted
        .orphaned_create_channels(100)
        .await
        .unwrap()
        .is_empty());
    assert!(
        !restarted.settle_create(&reservation).await.unwrap(),
        "a live event retains its cap slot"
    );
    let cap = CreateAdmissionConfig::new(10, 1, 0).unwrap();
    assert_eq!(
        restarted
            .claim_create(100, 301, &cap, 1_800_000_100)
            .await
            .unwrap(),
        CreateClaim::Refused(RefusalReason::GuildCap)
    );
    peer.close().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn event_receipt_rejects_wrong_lease_wrong_guild_and_settled_capacity_atomically() {
    let Some(db) = database().await else { return };
    let store = PgRoomStore::new(db.pool().clone());
    let at = OffsetDateTime::now_utc();
    assert!(
        claim_event_voice_channel(db.pool(), "500", at, at, None, "tick-a")
            .await
            .unwrap()
    );
    let reservation = reserve(&store, 100, 300).await;
    let mut wrong_lease = receipt(&reservation, at);
    wrong_lease.lease = "tick-b";
    assert!(!bind_admitted_event_voice_channel(db.pool(), &wrong_lease)
        .await
        .unwrap());
    assert_eq!(binding(db.pool(), at).await, None);
    let reserved_channel: Option<String> =
        sqlx::query_scalar("SELECT channel_id FROM voice_create_reservations WHERE id = $1")
            .bind(&reservation)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(
        reserved_channel, None,
        "lost event lease must not bind capacity either"
    );
    let wrong_guild = reserve(&store, 101, 300).await;
    assert!(
        !bind_admitted_event_voice_channel(db.pool(), &receipt(&wrong_guild, at))
            .await
            .unwrap()
    );
    assert_eq!(binding(db.pool(), at).await, None);
    assert!(store.settle_create(&reservation).await.unwrap());
    assert!(
        !bind_admitted_event_voice_channel(db.pool(), &receipt(&reservation, at))
            .await
            .unwrap()
    );
    assert_eq!(binding(db.pool(), at).await, None);
    db.close().await.unwrap();
}

#[tokio::test]
async fn event_receipt_unique_binding_failure_rolls_back_its_capacity_witness() {
    let Some(db) = database().await else { return };
    let store = PgRoomStore::new(db.pool().clone());
    let at = OffsetDateTime::now_utc();
    for event in ["500", "501"] {
        assert!(
            claim_event_voice_channel(db.pool(), event, at, at, None, "tick-a")
                .await
                .unwrap()
        );
    }
    let first = reserve(&store, 100, 300).await;
    let second = reserve(&store, 100, 301).await;
    assert!(
        bind_admitted_event_voice_channel(db.pool(), &receipt(&first, at))
            .await
            .unwrap()
    );
    let mut collision = receipt(&second, at);
    collision.event_id = "501";
    assert!(bind_admitted_event_voice_channel(db.pool(), &collision)
        .await
        .is_err());
    let reserved_channel: Option<String> =
        sqlx::query_scalar("SELECT channel_id FROM voice_create_reservations WHERE id = $1")
            .bind(&second)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(
        reserved_channel, None,
        "failed binding must not leave a compensation witness"
    );
    let second_channel: Option<String> = sqlx::query_scalar(
        "SELECT voice_channel_id FROM event_voice_channels WHERE event_id = '501'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(second_channel, None);
    db.close().await.unwrap();
}

#[tokio::test]
async fn event_receipt_cannot_overwrite_another_channel_and_ordinary_orphans_still_recover() {
    let Some(db) = database().await else { return };
    let store = PgRoomStore::new(db.pool().clone());
    let at = OffsetDateTime::now_utc();
    assert!(
        claim_event_voice_channel(db.pool(), "500", at, at, None, "tick-a")
            .await
            .unwrap()
    );
    let event_reservation = reserve(&store, 100, 300).await;
    let ordinary_reservation = reserve(&store, 100, 301).await;
    store
        .bind_create_channel(100, &ordinary_reservation, 701)
        .await
        .unwrap();
    assert!(
        bind_admitted_event_voice_channel(db.pool(), &receipt(&event_reservation, at))
            .await
            .unwrap()
    );
    let mut competing = receipt(&event_reservation, at);
    competing.channel_id = "702";
    assert!(!bind_admitted_event_voice_channel(db.pool(), &competing)
        .await
        .unwrap());
    assert_eq!(binding(db.pool(), at).await.as_deref(), Some("700"));
    assert_eq!(
        store.orphaned_create_channels(100).await.unwrap(),
        vec![(ordinary_reservation.clone(), 701)]
    );
    assert!(store.settle_create(&ordinary_reservation).await.unwrap());
    assert!(!store.settle_create(&event_reservation).await.unwrap());
    db.close().await.unwrap();
}
