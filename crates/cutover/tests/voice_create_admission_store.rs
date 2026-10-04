//! Durable room-create admission: persisted reservations, the per-guild
//! serialized claim, and burst/cooldown history that outlives rooms and
//! restarts. Fixed test-container target (`agent-testdb` only): each test
//! creates its own disposable database and never reads `DATABASE_URL`.
//! Run: TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_ci \
//!   cargo test -p two-bot-cutover --test voice_create_admission_store --locked -- --ignored

use two_bot_core::voice_create_admission::{CreateAdmissionConfig, RefusalReason};
use two_bot_core::voice_rooms::{NewRoomSpec, VoiceRoom};
use two_bot_cutover::voice_rooms::{CreateClaim, PgRoomStore, IN_FLIGHT_RESERVATION_TTL_SECS};
use two_bot_testsupport::TestDatabase;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const GUILD: u64 = 100;
const CREATOR: u64 = 200;
const ROOM_STAMP: &str = "2026-09-30T00:00:00.000000+00:00";
/// Synthetic Unix seconds; only differences matter.
const T0: i64 = 1_800_000_000;

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit test database URL required");
    TestDatabase::create(&url, &sqlx::migrate!("./migrations"))
        .await
        .unwrap()
}

fn config(max_per_user: u32, max_per_guild: u32, cooldown_secs: u32) -> CreateAdmissionConfig {
    CreateAdmissionConfig::new(max_per_user, max_per_guild, cooldown_secs).unwrap()
}

fn room(owner: u64, channel: u64) -> VoiceRoom {
    VoiceRoom::from_spec(
        NewRoomSpec {
            guild_id: GUILD,
            creator_channel_id: CREATOR,
            owner_id: owner,
            seed: 7,
            created_at: ROOM_STAMP.to_owned(),
        },
        channel,
    )
}

async fn admitted(
    store: &PgRoomStore,
    user: u64,
    config: &CreateAdmissionConfig,
    now: i64,
) -> Result<i64, Box<dyn std::error::Error>> {
    match store.claim_create(GUILD, user, config, now).await? {
        CreateClaim::Admitted { reservation_id } => Ok(reservation_id),
        CreateClaim::Refused(reason) => {
            Err(format!("user {user} at +{} refused: {}", now - T0, reason.code()).into())
        }
    }
}

async fn refused(
    store: &PgRoomStore,
    user: u64,
    config: &CreateAdmissionConfig,
    now: i64,
) -> Result<RefusalReason, Box<dyn std::error::Error>> {
    match store.claim_create(GUILD, user, config, now).await? {
        CreateClaim::Refused(reason) => Ok(reason),
        CreateClaim::Admitted { .. } => {
            Err(format!("user {user} at +{} was admitted", now - T0).into())
        }
    }
}

/// A create that completed: the room is tracked, then it is deleted again.
/// Neither step may refund a burst slot.
async fn create_then_delete(
    store: &PgRoomStore,
    user: u64,
    channel: u64,
    config: &CreateAdmissionConfig,
    now: i64,
) -> TestResult {
    let id = admitted(store, user, config, now).await?;
    assert!(store.add_room(&room(user, channel)).await?);
    assert!(store.settle_create(id, Some(channel)).await?);
    assert!(store.remove_room(GUILD, channel).await?.is_some());
    Ok(())
}

#[tokio::test]
#[ignore = "requires TWO_TEST_DATABASE_URL on agent-testdb"]
async fn burst_history_survives_restart_and_deleted_rooms_free_no_slot() -> TestResult {
    let db = database().await;
    // Caps and cooldown out of the way: only the rolling burst limits bite.
    let open = config(10, 45, 0);
    let first = PgRoomStore::new(db.pool().clone());
    for offset in 0..3 {
        create_then_delete(&first, 300, 500 + offset as u64, &open, T0 + offset).await?;
    }
    // Three accepted creates inside the window; every room is already gone.
    assert!(first.rooms_in_guild(GUILD).await?.is_empty());
    assert_eq!(
        refused(&first, 300, &open, T0 + 3).await?,
        RefusalReason::UserBurst
    );

    // A restart is a brand-new store over a brand-new connection pool: only
    // the database remembers the window.
    drop(first);
    let restarted = PgRoomStore::new(db.independent_pool().await?);
    assert_eq!(
        refused(&restarted, 300, &open, T0 + 4).await?,
        RefusalReason::UserBurst
    );
    // The window is strict and rolling: the oldest accepted create (T0) leaves
    // it at exactly T0 + 60, not a moment before.
    assert_eq!(
        refused(&restarted, 300, &open, T0 + 59).await?,
        RefusalReason::UserBurst
    );
    admitted(&restarted, 300, &open, T0 + 60).await?;

    // The guild burst counts everyone's accepted creates: at T0+61 only member
    // 300's creates at T0+2 and T0+60 are still in the window (T0 and T0+1 have
    // left it), eight more members make ten, and the next create is refused.
    for member in 400..408 {
        admitted(&restarted, member, &open, T0 + 61).await?;
    }
    assert_eq!(
        refused(&restarted, 500, &open, T0 + 61).await?,
        RefusalReason::GuildBurst
    );
    // Once the window rolls past them all, creation resumes.
    admitted(&restarted, 500, &open, T0 + 61 + 60).await?;
    db.close().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires TWO_TEST_DATABASE_URL on agent-testdb"]
async fn concurrent_claims_across_stores_cannot_both_pass_the_guild_cap() -> TestResult {
    let db = database().await;
    // Two stores on separate pools stand in for two bot processes.
    let stores = [
        PgRoomStore::new(db.pool().clone()),
        PgRoomStore::new(db.independent_pool().await?),
    ];
    let cap = config(10, 3, 0);
    let mut claims = Vec::new();
    for member in 0..8u64 {
        let store = stores[(member % 2) as usize].clone();
        claims.push(tokio::spawn(async move {
            store.claim_create(GUILD, 1000 + member, &cap, T0).await
        }));
    }
    let mut admitted_count = 0;
    let mut guild_cap_refusals = 0;
    for claim in claims {
        match claim.await?? {
            CreateClaim::Admitted { .. } => admitted_count += 1,
            CreateClaim::Refused(RefusalReason::GuildCap) => guild_cap_refusals += 1,
            CreateClaim::Refused(other) => panic!("unexpected refusal {}", other.code()),
        }
    }
    assert_eq!(admitted_count, 3, "exactly the guild cap passes");
    assert_eq!(guild_cap_refusals, 5);
    db.close().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires TWO_TEST_DATABASE_URL on agent-testdb"]
async fn caps_count_live_and_in_flight_rooms_and_cooldown_counts_rollbacks() -> TestResult {
    let db = database().await;
    let store = PgRoomStore::new(db.pool().clone());

    // Cap of two rooms per member, cooldown of 30 s.
    let two = config(2, 40, 30);
    let in_flight = admitted(&store, 300, &two, T0).await?;
    // The cooldown (30 s) is the first limit the next create trips...
    assert_eq!(
        refused(&store, 300, &two, T0 + 29).await?,
        RefusalReason::Cooldown
    );
    // ...and exactly the cooldown later it passes (equality is allowed).
    let second = admitted(&store, 300, &two, T0 + 30).await?;
    // Two in-flight reservations hold both slots.
    assert_eq!(
        refused(&store, 300, &two, T0 + 60).await?,
        RefusalReason::UserCap
    );
    // Binding one to a tracked room hands its slot to `voice_rooms` without
    // counting it twice: a settled reservation plus its room is one slot.
    assert!(store.add_room(&room(300, 510)).await?);
    assert!(store.settle_create(in_flight, Some(510)).await?);
    assert!(
        !store.settle_create(in_flight, Some(510)).await?,
        "settling twice is a no-op"
    );
    assert_eq!(
        refused(&store, 300, &two, T0 + 61).await?,
        RefusalReason::UserCap
    );
    // Rolling the other create back frees its slot, but not its cooldown.
    assert!(store.settle_create(second, None).await?);
    let three = config(3, 40, 30);
    assert_eq!(
        refused(&store, 300, &three, T0 + 40).await?,
        RefusalReason::Cooldown
    );
    admitted(&store, 300, &three, T0 + 61).await?;

    // A guild at its cap refuses a different member before the cooldown.
    let tiny = config(5, 3, 30);
    // Live rooms: 510 (member 300). In flight: the T0+61 claim above (its
    // reservation is still unsettled). Third slot goes to a new member.
    admitted(&store, 301, &tiny, T0 + 62).await?;
    assert_eq!(
        refused(&store, 302, &tiny, T0 + 63).await?,
        RefusalReason::GuildCap
    );

    // A reservation abandoned by a crash stops holding a slot after the
    // in-flight TTL (it still counts toward burst and cooldown history).
    let lone = config(1, 40, 0);
    admitted(&store, 800, &lone, T0 + 1000).await?;
    assert_eq!(
        refused(
            &store,
            800,
            &lone,
            T0 + 1000 + IN_FLIGHT_RESERVATION_TTL_SECS - 1
        )
        .await?,
        RefusalReason::UserCap
    );
    admitted(
        &store,
        800,
        &lone,
        T0 + 1000 + IN_FLIGHT_RESERVATION_TTL_SECS,
    )
    .await?;
    db.close().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires TWO_TEST_DATABASE_URL on agent-testdb"]
async fn zero_ids_are_refused_before_any_write() -> TestResult {
    let db = database().await;
    let store = PgRoomStore::new(db.pool().clone());
    let open = config(10, 45, 0);
    assert!(store.claim_create(0, 300, &open, T0).await.is_err());
    assert!(store.claim_create(GUILD, 0, &open, T0).await.is_err());
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM voice_create_reservations")
        .fetch_one(db.pool())
        .await?;
    assert_eq!(rows, 0);
    db.close().await?;
    Ok(())
}
