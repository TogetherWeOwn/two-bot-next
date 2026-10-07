//! Durable room-create admission: persisted reservations, the per-guild
//! serialized claim, and burst/cooldown history that outlives rooms and
//! restarts. Fixed test-container target (`agent-testdb` only): each test
//! creates its own disposable database and never reads `DATABASE_URL`.
//! Run: TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_ci \
//!   cargo test -p two-bot-cutover --test voice_create_admission_store --locked -- --ignored

use two_bot_core::voice_create_admission::{CreateAdmissionConfig, RefusalReason};
use two_bot_core::voice_rooms::{NewRoomSpec, VoiceRoom};
use two_bot_cutover::voice_rooms::{CreateClaim, PgRoomStore};
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
) -> Result<String, Box<dyn std::error::Error>> {
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
    store.persist_create(&id, &room(user, channel)).await?;
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
    store.persist_create(&in_flight, &room(300, 510)).await?;
    store.persist_create(&in_flight, &room(300, 510)).await?;
    assert!(
        !store.settle_create(&in_flight).await?,
        "a bound live room cannot be rolled back"
    );
    assert_eq!(
        refused(&store, 300, &two, T0 + 61).await?,
        RefusalReason::UserCap
    );
    // Rolling the other create back frees its slot, but not its cooldown.
    assert!(store.settle_create(&second).await?);
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

    // Age never proves absence: a crash, unknown POST or live 429 waiter
    // retains its slot even after the former 300 s expiry and after a day.
    let lone = config(1, 40, 0);
    admitted(&store, 800, &lone, T0 + 1000).await?;
    for age in [299, 300, 86_400] {
        assert_eq!(
            refused(&store, 800, &lone, T0 + 1000 + age).await?,
            RefusalReason::UserCap
        );
    }
    db.close().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires TWO_TEST_DATABASE_URL on agent-testdb"]
async fn aged_claim_blocks_competing_store_and_resumes_without_new_history() -> TestResult {
    let db = database().await;
    let original = PgRoomStore::new(db.pool().clone());
    let restarted = PgRoomStore::new(db.independent_pool().await?);
    let cap = config(10, 1, 0);
    let held = admitted(&original, 300, &cap, T0).await?;
    for age in [299, 300, 3600, 86_400] {
        assert_eq!(
            refused(&restarted, 301, &cap, T0 + age).await?,
            RefusalReason::GuildCap
        );
    }
    // The slow old worker resumes. Its original slot never went to another
    // worker, and transferring it does not count a second burst attempt.
    original.persist_create(&held, &room(300, 500)).await?;
    assert_eq!(
        refused(&restarted, 301, &cap, T0 + 86_400).await?,
        RefusalReason::GuildCap
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM voice_create_reservations")
        .fetch_one(db.pool())
        .await?;
    assert_eq!(count, 1);
    original.remove_room(GUILD, 500).await?;
    assert!(
        original
            .persist_create(&held, &room(300, 500))
            .await
            .is_err(),
        "replay cannot resurrect a deleted room"
    );
    admitted(&restarted, 301, &cap, T0 + 86_400).await?;
    db.close().await?;
    Ok(())
}

async fn wait_for_blocked_query(pool: &sqlx::PgPool, pattern: &str) -> TestResult {
    tokio::time::timeout(std::time::Duration::from_secs(4), async {
        loop {
            let blocked: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid
                 WHERE NOT l.granted AND a.datname = current_database() AND a.query LIKE $1)",
            )
            .bind(pattern)
            .fetch_one(pool)
            .await?;
            if blocked {
                return Ok::<(), sqlx::Error>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await??;
    Ok(())
}

#[tokio::test]
#[ignore = "requires TWO_TEST_DATABASE_URL on agent-testdb"]
async fn room_persist_and_settlement_are_not_separately_observable_to_claimers() -> TestResult {
    let db = database().await;
    let store = PgRoomStore::new(db.pool().clone());
    let peer = PgRoomStore::new(db.independent_pool().await?);
    let cap = config(10, 2, 0);
    let held = admitted(&store, 300, &cap, T0).await?;
    // Hold an INSERT after it has written the row, before reservation UPDATE.
    // The barrier is database-scoped and transaction-owned: no fixed global
    // test lock, sleeps-as-evidence or session lock returned to a pool.
    sqlx::raw_sql(
        "CREATE FUNCTION pause_room_transfer() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           PERFORM pg_advisory_xact_lock(hashtextextended(current_database() || ':room_transfer_fixture', 0));
           RETURN NEW;
         END $$;
         CREATE TRIGGER pause_room_transfer AFTER INSERT ON voice_rooms
         FOR EACH ROW EXECUTE FUNCTION pause_room_transfer();",
    ).execute(db.pool()).await?;
    let mut barrier = db.pool().begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended(current_database() || ':room_transfer_fixture', 0))")
        .execute(&mut *barrier).await?;
    let transfer = tokio::spawn(async move { store.persist_create(&held, &room(300, 500)).await });
    wait_for_blocked_query(db.pool(), "INSERT INTO voice_rooms%").await?;
    let claimant = tokio::spawn(async move { peer.claim_create(GUILD, 301, &cap, T0 + 60).await });
    wait_for_blocked_query(db.pool(), "SELECT pg_advisory_xact_lock%").await?;
    assert!(!transfer.is_finished());
    assert!(
        !claimant.is_finished(),
        "claimer must wait for the complete transfer"
    );
    let rooms: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM voice_rooms")
        .fetch_one(db.pool())
        .await?;
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM voice_create_reservations WHERE settled_at IS NULL",
    )
    .fetch_one(db.pool())
    .await?;
    assert_eq!(
        (rooms, pending),
        (0, 1),
        "outside readers see only the old capacity"
    );
    barrier.commit().await?;
    transfer.await??;
    assert!(
        matches!(claimant.await??, CreateClaim::Admitted { .. }),
        "one live room plus the next claim is two slots, not three"
    );
    assert_eq!(
        refused(
            &PgRoomStore::new(db.pool().clone()),
            302,
            &config(10, 2, 0),
            T0 + 60
        )
        .await?,
        RefusalReason::GuildCap
    );
    db.close().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires TWO_TEST_DATABASE_URL on agent-testdb"]
async fn failed_settlement_rolls_back_the_room_insert_and_retains_capacity() -> TestResult {
    let db = database().await;
    let store = PgRoomStore::new(db.pool().clone());
    let cap = config(10, 1, 0);
    let held = admitted(&store, 300, &cap, T0).await?;
    // Synthetic UPDATE-only refusal; never revoke real credentials/grants.
    sqlx::raw_sql(
        "CREATE FUNCTION refuse_create_settlement() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'fixture settlement refusal' USING ERRCODE = '42501'; END $$;
         CREATE TRIGGER refuse_create_settlement BEFORE UPDATE ON voice_create_reservations
         FOR EACH ROW EXECUTE FUNCTION refuse_create_settlement();",
    )
    .execute(db.pool())
    .await?;
    let error = store
        .persist_create(&held, &room(300, 500))
        .await
        .unwrap_err();
    assert_eq!(
        error.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("42501")
    );
    assert!(
        store.rooms_in_guild(GUILD).await?.is_empty(),
        "no partial room insert"
    );
    assert_eq!(
        refused(&store, 301, &cap, T0 + 86_400).await?,
        RefusalReason::GuildCap
    );
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM voice_create_reservations WHERE settled_at IS NULL",
    )
    .fetch_one(db.pool())
    .await?;
    assert_eq!(pending, 1);
    db.close().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires TWO_TEST_DATABASE_URL on agent-testdb"]
async fn bound_channel_witness_survives_restart_and_holds_the_slot_until_settled() -> TestResult {
    let db = database().await;
    let store = PgRoomStore::new(db.pool().clone());
    let cap = config(10, 1, 0);
    let held = admitted(&store, 300, &cap, T0).await?;
    // Nothing is witnessed before the create returns a channel.
    assert!(store.orphaned_create_channels(GUILD).await?.is_empty());

    // Discord returned channel 500: bind it BEFORE any room row exists. Bind
    // is idempotent for the same channel and refuses a different one.
    store.bind_create_channel(GUILD, &held, 500).await?;
    store.bind_create_channel(GUILD, &held, 500).await?;
    assert!(store.bind_create_channel(GUILD, &held, 501).await.is_err());
    assert!(store
        .bind_create_channel(GUILD, "missing", 500)
        .await
        .is_err());
    assert!(store
        .bind_create_channel(GUILD + 1, &held, 500)
        .await
        .is_err());

    // The persist failed and the delete was refused: a restarted worker, a
    // brand-new pool, still learns the channel and the slot is still held.
    drop(store);
    let restarted = PgRoomStore::new(db.independent_pool().await?);
    assert_eq!(
        restarted.orphaned_create_channels(GUILD).await?,
        [(held.clone(), 500)]
    );
    assert_eq!(
        refused(&restarted, 301, &cap, T0 + 86_400).await?,
        RefusalReason::GuildCap
    );
    // A claim bound to one channel cannot transfer to a different room.
    assert!(restarted
        .persist_create(&held, &room(300, 501))
        .await
        .is_err());

    // Confirmed delete or 404: only now does the settlement free the slot,
    // and the witness disappears with it. History still counts for the burst.
    assert!(restarted.settle_create(&held).await?);
    assert!(restarted.orphaned_create_channels(GUILD).await?.is_empty());
    assert!(!restarted.settle_create(&held).await?);
    let next = admitted(&restarted, 301, &cap, T0 + 86_401).await?;

    // A bound claim whose persist later succeeds transfers atomically: the
    // room is tracked, the claim settles, no witness remains, no double count.
    restarted.bind_create_channel(GUILD, &next, 502).await?;
    restarted.persist_create(&next, &room(301, 502)).await?;
    assert!(restarted.orphaned_create_channels(GUILD).await?.is_empty());
    assert!(
        !restarted.settle_create(&next).await?,
        "a bound live room cannot be rolled back"
    );
    assert!(restarted
        .bind_create_channel(GUILD, &next, 502)
        .await
        .is_err());
    assert_eq!(
        refused(&restarted, 302, &cap, T0 + 86_402).await?,
        RefusalReason::GuildCap
    );
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
