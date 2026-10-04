//! Fixed test-container target: never read DATABASE_URL or use staging credentials.

use sqlx::{postgres::PgConnectOptions, postgres::PgPoolOptions, PgPool, Postgres, QueryBuilder};
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use two_bot_core::voice_access::AccessControls;
use two_bot_core::voice_logging::{DetailLevel, LoggingSettings};
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

    // V2 caretaker handoff: owner fields move on the tracked row, nothing
    // else changes, and a missing row reports false instead of erroring.
    // The handoff also stamps `owner_touched_at` for the rollback delta.
    let touched_before: String =
        sqlx::query_scalar("SELECT owner_touched_at::text FROM voice_rooms WHERE guild_id = '100' AND channel_id = '500'")
            .fetch_one(pool)
            .await?;
    assert!(store.update_ownership(100, 500, 301, 300).await?);
    let handed = VoiceRoom {
        owner_id: 301,
        ..first.clone()
    };
    assert_eq!(store.room_for(100, 500).await?, Some(handed.clone()));
    // Channel 501 (`second`) already belongs to owner 301, so both rows
    // now list under 301 in channel order.
    assert_eq!(
        store.rooms_for_owner(100, 301).await?,
        vec![handed.clone(), second.clone()]
    );
    assert!(store.rooms_for_owner(100, 300).await?.is_empty());
    let touched_after: String =
        sqlx::query_scalar("SELECT owner_touched_at::text FROM voice_rooms WHERE guild_id = '100' AND channel_id = '500'")
            .fetch_one(pool)
            .await?;
    assert!(
        touched_after >= touched_before,
        "handoff must not move the rollback stamp backwards"
    );
    assert!(!store.update_ownership(100, 599, 301, 300).await?);
    verify_owner_grant_journal(&store, pool, &handed).await?;

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
    // V9c worker load: guild-scoped companion listing for startup
    // reconciliation, ordered by room.
    let mut companion_two = companion.clone();
    companion_two.room_channel_id = 501;
    companion_two.text_channel_id = 601;
    assert!(store.add_companion(&companion_two).await?);
    assert_eq!(
        store.companions_in_guild(100).await?,
        vec![companion.clone(), companion_two.clone()]
    );
    assert!(store.companions_in_guild(101).await?.is_empty());
    assert_eq!(store.remove_companion(100, 501).await?, Some(companion_two));
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
    // Channel 500 still carries the handoff above, so the reload sees
    // `handed` (not the pre-handoff `first`) alongside `second`.
    assert_eq!(
        restarted.rooms_in_guild(100).await?,
        vec![handed.clone(), second.clone()]
    );
    assert_eq!(restarted.remove_room(101, 500).await?, None);
    assert_eq!(restarted.remove_room(100, 500).await?, Some(handed));
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
    verify_access_controls(&store, pool).await?;
    verify_logging_settings(&store, pool).await
}

async fn verify_owner_grant_journal(
    store: &PgRoomStore,
    pool: &PgPool,
    room: &VoiceRoom,
) -> TestResult {
    let mut proposed = room.clone();
    proposed.owner_id = 302;
    proposed.original_creator_id = 302;
    let first = store
        .prepare_owner_grants(&proposed, room.owner_id)
        .await?
        .unwrap();
    assert_eq!(first.members, [301, 302]);
    let restarted = PgRoomStore::new(pool.clone());
    assert_eq!(restarted.pending_owner_grants(100).await?, [500]);
    assert!(restarted.pending_owner_grants(101).await?.is_empty());
    assert_eq!(restarted.room_for(100, 500).await?, Some(room.clone()));

    // Refuse the final ownership UPDATE after the preparation committed. The
    // transaction must roll back without acknowledging any possible grant.
    sqlx::query("ALTER TABLE voice_rooms ADD CONSTRAINT journal_test_refuse CHECK (owner_id <> '302') NOT VALID")
        .execute(pool).await?;
    assert!(store
        .complete_owner_grants(&proposed, &first.revision)
        .await
        .is_err());
    assert_eq!(restarted.pending_owner_grants(100).await?, [500]);
    assert_eq!(restarted.room_for(100, 500).await?, Some(room.clone()));
    sqlx::query("ALTER TABLE voice_rooms DROP CONSTRAINT journal_test_refuse")
        .execute(pool)
        .await?;

    proposed.owner_id = 303;
    proposed.original_creator_id = 303;
    let newer = restarted
        .prepare_owner_grants(&proposed, 302)
        .await?
        .unwrap();
    assert_ne!(first.revision, newer.revision);
    assert_eq!(newer.members, [301, 302, 303]);
    assert!(
        !store
            .complete_owner_grants(&proposed, &first.revision)
            .await?,
        "stale completion must not clear a newer intent"
    );
    assert_eq!(restarted.pending_owner_grants(100).await?, [500]);
    assert!(
        restarted
            .complete_owner_grants(&proposed, &newer.revision)
            .await?
    );
    assert!(store.pending_owner_grants(100).await?.is_empty());
    assert_eq!(store.room_for(100, 500).await?, Some(proposed));
    let retained: Vec<String> = sqlx::query_scalar("SELECT member_id FROM voice_owner_grants WHERE guild_id = '100' AND channel_id = '500' ORDER BY member_id")
        .fetch_all(pool).await?;
    assert_eq!(retained, ["303"]);

    // Restore the round-trip fixture with the same fenced completion path.
    let restore = store.prepare_owner_grants(room, 303).await?.unwrap();
    assert!(store.complete_owner_grants(room, &restore.revision).await?);
    let mut missing = room.clone();
    missing.guild_id = 101;
    assert!(store.prepare_owner_grants(&missing, 301).await?.is_none());
    assert!(
        !store
            .complete_owner_grants(&missing, &restore.revision)
            .await?
    );
    Ok(())
}

async fn verify_access_controls(store: &PgRoomStore, pool: &PgPool) -> TestResult {
    // A never-configured guild reads as the defaults.
    assert_eq!(store.access_controls(100).await?, AccessControls::default());

    let controls = AccessControls {
        room_creation_enabled: false,
        required_role: Some(u64::MAX),
        command_roles: BTreeMap::from([
            ("kick".to_owned(), vec![7, u64::MAX]),
            // Present-but-empty denies every non-admin and must survive a reload.
            ("template".to_owned(), vec![]),
        ]),
    };
    store.save_access_controls(100, &controls).await?;
    assert_eq!(store.access_controls(100).await?, controls);
    assert_eq!(
        PgRoomStore::new(pool.clone()).access_controls(100).await?,
        controls
    );
    assert_eq!(
        store.access_controls(101).await?,
        AccessControls::default(),
        "controls are per guild"
    );

    // Saving replaces the whole row: lifting every restriction sticks.
    store
        .save_access_controls(100, &AccessControls::default())
        .await?;
    assert_eq!(store.access_controls(100).await?, AccessControls::default());

    // Refused before the database: unknown command, zero role ids.
    let mut bad = AccessControls::default();
    bad.command_roles.insert("kik".to_owned(), vec![7]);
    assert!(store.save_access_controls(100, &bad).await.is_err());
    bad.command_roles.clear();
    bad.required_role = Some(0);
    assert!(store.save_access_controls(100, &bad).await.is_err());
    bad.required_role = None;
    bad.command_roles.insert("kick".to_owned(), vec![0]);
    assert!(store.save_access_controls(100, &bad).await.is_err());
    assert_eq!(store.access_controls(100).await?, AccessControls::default());

    // The table's own checks hold when SQL bypasses the adapter.
    assert!(sqlx::query(
        "UPDATE voice_access_controls SET required_role_id = '0' WHERE guild_id = '100'"
    )
    .execute(pool)
    .await
    .is_err());
    assert!(sqlx::query(
        "UPDATE voice_access_controls SET command_roles = '[]'::jsonb WHERE guild_id = '100'"
    )
    .execute(pool)
    .await
    .is_err());
    Ok(())
}

async fn verify_logging_settings(store: &PgRoomStore, pool: &PgPool) -> TestResult {
    // A never-configured guild reads as the defaults.
    assert_eq!(
        store.logging_settings(100).await?,
        LoggingSettings::default()
    );

    let settings = LoggingSettings {
        level: DetailLevel::Full,
        channel_id: Some(u64::MAX),
        mention_role_id: Some(7),
    };
    store.save_logging_settings(100, &settings).await?;
    assert_eq!(store.logging_settings(100).await?, settings);
    assert_eq!(
        PgRoomStore::new(pool.clone()).logging_settings(100).await?,
        settings
    );
    assert_eq!(
        store.logging_settings(101).await?,
        LoggingSettings::default(),
        "settings are per guild"
    );

    // Saving replaces the whole row: clearing the channel and mention sticks.
    let off = LoggingSettings {
        level: DetailLevel::Off,
        channel_id: None,
        mention_role_id: None,
    };
    store.save_logging_settings(100, &off).await?;
    assert_eq!(store.logging_settings(100).await?, off);

    // Refused before the database: zero ids.
    for bad in [
        LoggingSettings {
            channel_id: Some(0),
            ..off
        },
        LoggingSettings {
            mention_role_id: Some(0),
            ..off
        },
    ] {
        assert!(store.save_logging_settings(100, &bad).await.is_err());
    }
    assert_eq!(store.logging_settings(100).await?, off);

    // The table's own checks hold when SQL bypasses the adapter.
    for statement in [
        "UPDATE voice_logging_settings SET detail_level = 'loud' WHERE guild_id = '100'",
        "UPDATE voice_logging_settings SET log_channel_id = '0' WHERE guild_id = '100'",
        "UPDATE voice_logging_settings SET mention_role_id = '0' WHERE guild_id = '100'",
    ] {
        assert!(
            sqlx::query(statement).execute(pool).await.is_err(),
            "{statement}"
        );
    }
    Ok(())
}
