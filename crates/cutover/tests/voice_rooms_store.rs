//! Fixed test-container target: never read DATABASE_URL or use staging credentials.

use sqlx::{postgres::PgConnectOptions, postgres::PgPoolOptions, PgPool, Postgres, QueryBuilder};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use two_bot_core::voice_rooms::{
    CreatorChannel, PermissionSource, RoomPosition, TextCompanion, VoiceRoom,
};
use two_bot_cutover::voice_rooms::PgRoomStore;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[tokio::test]
#[ignore = "requires agent-testdb:5432 with agent_test and an empty password"]
async fn voice_rooms_store_round_trip_and_idempotency() -> TestResult {
    let options = PgConnectOptions::new()
        .host("agent-testdb")
        .port(5432)
        .username("agent_test")
        .password("")
        .database("postgres");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(options)
        .await?;
    // Only locally generated ASCII letters/digits/underscores become identifiers.
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let schema = format!("voice_rooms_test_{}_{}", std::process::id(), nonce);
    QueryBuilder::<Postgres>::new("CREATE SCHEMA ")
        .push(&schema)
        .build()
        .execute(&pool)
        .await?;
    let result = verify_store(&pool, &schema).await;
    let cleanup = QueryBuilder::<Postgres>::new("DROP SCHEMA ")
        .push(&schema)
        .push(" CASCADE")
        .build()
        .execute(&pool)
        .await;
    pool.close().await;
    result?;
    cleanup?;
    Ok(())
}

async fn verify_store(pool: &PgPool, schema: &str) -> TestResult {
    sqlx::query("SELECT set_config('search_path', $1, false)")
        .bind(schema)
        .execute(pool)
        .await?;
    sqlx::migrate!("./migrations").run(pool).await?;
    // A second run validates the same checksums and applies nothing twice.
    sqlx::migrate!("./migrations").run(pool).await?;
    let store = PgRoomStore::new(pool.clone());
    let mut creator = CreatorChannel::new(100, 200);
    creator.name_template = "@@owner@@'s room; SELECT 'not SQL'".to_owned();
    creator.permission_source = PermissionSource::Channel(u64::MAX);
    creator.permission_channel_id = Some(u64::MAX);
    creator.default_limit = Some(99);
    creator.private_default = true;
    creator.text_channels = true;
    creator.text_channel_name = Some("Lounge; SELECT 'not SQL'".to_owned());
    creator.text_viewer_role_id = Some(u64::MAX);
    creator.position = RoomPosition::Below;
    creator.first_room_number = 3;
    store.add_creator(&creator).await?;
    assert_eq!(store.creator_for(100, 200).await?, Some(creator.clone()));
    assert_eq!(store.creator_for(101, 200).await?, None);
    assert_eq!(
        store
            .creator_for(100, 200)
            .await?
            .expect("creator row")
            .text_channel_settings(),
        two_bot_core::voice_text_channel::TextChannelSettings {
            enabled: true,
            configured_name: Some("Lounge; SELECT 'not SQL'".to_owned()),
            viewer_role_id: Some(u64::MAX),
        }
    );
    // Clearing the toggle (plus NULL name/viewer columns) decodes to the
    // default (off) settings.
    let mut cleared = creator.clone();
    cleared.text_channels = false;
    cleared.text_channel_name = None;
    cleared.text_viewer_role_id = None;
    store.add_creator(&cleared).await?;
    assert_eq!(
        store
            .creator_for(100, 200)
            .await?
            .expect("creator row")
            .text_channel_settings(),
        two_bot_core::voice_text_channel::TextChannelSettings::default()
    );
    creator.default_limit = Some(4);
    store.add_creator(&creator).await?;
    assert_eq!(store.creators(100).await?, vec![creator.clone()]);
    assert!(store.creators(101).await?.is_empty());

    let first = VoiceRoom {
        guild_id: 100,
        channel_id: 500,
        creator_channel_id: 200,
        owner_id: 300,
        original_creator_id: 300,
        name_seed: u64::MAX,
        created_at: "2026-09-30T01:00:00.123Z".to_owned(),
    };
    let second = VoiceRoom {
        channel_id: 501,
        owner_id: 301,
        original_creator_id: 301,
        ..first.clone()
    };
    let (a, b) = tokio::join!(store.add_room(&first), store.add_room(&second));
    assert!(a? && b?);
    let mut duplicate = first.clone();
    duplicate.owner_id = 999;
    duplicate.name_seed = 0;
    assert!(!store.add_room(&duplicate).await?);
    assert_eq!(store.room_for(100, 500).await?, Some(first.clone()));
    assert_eq!(store.rooms_for_owner(100, 300).await?, vec![first.clone()]);
    assert!(store.rooms_in_guild(101).await?.is_empty());

    // V9b companion records: creation snapshot round-trips, insert-once, and
    // get/delete per room. The snapshot decodes back into the pure settings.
    let companion = TextCompanion {
        guild_id: 100,
        room_channel_id: 500,
        text_channel_id: 600,
        settings: two_bot_core::voice_text_channel::TextChannelSettings {
            enabled: true,
            configured_name: Some("Lounge; SELECT 'not SQL'".to_owned()),
            viewer_role_id: Some(u64::MAX),
        },
        created_at: "2026-09-30T01:00:00.123Z".to_owned(),
    };
    assert!(store.add_companion(&companion).await?);
    let mut companion_dup = companion.clone();
    companion_dup.text_channel_id = 999;
    assert!(!store.add_companion(&companion_dup).await?);
    assert_eq!(
        store.companion_for(100, 500).await?,
        Some(companion.clone())
    );
    assert_eq!(store.companion_for(101, 500).await?, None);
    assert_eq!(store.companion_for(100, 501).await?, None);
    assert_eq!(
        store.remove_companion(101, 500).await?,
        None,
        "wrong guild deletes nothing"
    );
    assert_eq!(store.remove_companion(100, 500).await?, Some(companion));
    assert_eq!(store.remove_companion(100, 500).await?, None);

    // Reconstructing the adapter reloads durable state; removing the creator
    // must not cascade-delete rooms that are still occupied.
    assert!(store.remove_creator(100, 200).await?);
    assert!(!store.remove_creator(100, 200).await?);
    let restarted = PgRoomStore::new(pool.clone());
    assert_eq!(
        restarted.rooms_in_guild(100).await?,
        vec![first.clone(), second.clone()]
    );
    assert_eq!(restarted.remove_room(101, 500).await?, None);
    assert_eq!(restarted.remove_room(100, 500).await?, Some(first));
    assert_eq!(restarted.remove_room(100, 500).await?, None);
    assert_eq!(restarted.rooms_in_guild(100).await?, vec![second]);

    creator.default_limit = Some(100);
    assert!(store.add_creator(&creator).await.is_err());
    creator.default_limit = Some(0);
    store.add_creator(&creator).await?;
    assert_eq!(store.creator_for(100, 200).await?, Some(creator.clone()));
    creator.default_limit = None;
    store.add_creator(&creator).await?;
    assert_eq!(store.creator_for(100, 200).await?, Some(creator.clone()));
    assert!(
        sqlx::query("UPDATE voice_creators SET default_limit = 100 WHERE guild_id = $1")
            .bind("100")
            .execute(pool)
            .await
            .is_err()
    );
    Ok(())
}
