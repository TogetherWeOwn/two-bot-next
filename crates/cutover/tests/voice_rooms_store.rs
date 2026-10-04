//! Fixed test-container target: never read DATABASE_URL or use staging credentials.

use sqlx::{postgres::PgConnectOptions, postgres::PgPoolOptions, PgPool, Postgres, QueryBuilder};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use two_bot_core::voice_access::AccessControls;
use two_bot_core::voice_logging::{DetailLevel, LoggingSettings};
use two_bot_core::voice_private::PrivacyRecord;
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

    // V3 `/name` override: stored as typed (tokens and SQL-looking text are
    // data), stamps `name_touched_at`, leaves the tracked room row alone, and
    // clears back to the template name. Out-of-range text and a missing row
    // never write.
    assert!(store.custom_names(100).await?.is_empty());
    let named_stamp = "SELECT name_touched_at::text FROM voice_rooms WHERE guild_id = '100' AND channel_id = '500'";
    assert_eq!(
        sqlx::query_scalar::<_, Option<String>>(named_stamp)
            .fetch_one(pool)
            .await?,
        None,
        "a room never renamed has no name stamp"
    );
    let typed = "@@owner@@'s [[den/crew]]; SELECT 'not SQL'";
    assert!(store.set_custom_name(100, 500, Some(typed)).await?);
    assert_eq!(
        store.custom_names(100).await?,
        vec![(500, typed.to_owned())]
    );
    assert!(store.custom_names(101).await?.is_empty());
    assert_eq!(store.room_for(100, 500).await?, Some(handed.clone()));
    assert!(sqlx::query_scalar::<_, Option<String>>(named_stamp)
        .fetch_one(pool)
        .await?
        .is_some());
    let hundred = "é".repeat(100);
    assert!(store.set_custom_name(100, 501, Some(&hundred)).await?);
    assert!(store
        .set_custom_name(100, 501, Some(&"x".repeat(101)))
        .await
        .is_err());
    assert!(store.set_custom_name(100, 501, Some("")).await.is_err());
    assert_eq!(
        store.custom_names(100).await?,
        vec![(500, typed.to_owned()), (501, hundred.clone())]
    );
    assert!(store.set_custom_name(100, 500, None).await?);
    assert_eq!(store.custom_names(100).await?, vec![(501, hundred)]);
    assert!(sqlx::query_scalar::<_, Option<String>>(named_stamp)
        .fetch_one(pool)
        .await?
        .is_some());
    assert!(!store.set_custom_name(100, 599, Some("missing")).await?);

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
    verify_privacy(&store, pool).await?;
    verify_access_controls(&store, pool).await?;
    verify_logging_settings(&store, pool).await
}

/// V3 privacy persistence (0414): the private flag, the Join channel and the
/// per-room block list round-trip, survive privacy toggles, are bounded by the
/// schema, and go with the room row.
async fn verify_privacy(store: &PgRoomStore, pool: &PgPool) -> TestResult {
    let room = VoiceRoom {
        guild_id: 100,
        channel_id: 510,
        creator_channel_id: 200,
        owner_id: 310,
        original_creator_id: 310,
        name_seed: 1,
        created_at: "2026-09-30T02:00:00.000Z".to_owned(),
    };
    assert!(store.add_room(&room).await?);
    // A new room is public with no Join channel and no blocks, so it has no
    // record at all.
    assert!(store.privacy_in_guild(100).await?.is_empty());
    let stamp = "SELECT privacy_touched_at IS NOT NULL FROM voice_rooms WHERE guild_id = '100' AND channel_id = '510'";
    assert!(!sqlx::query_scalar::<_, bool>(stamp).fetch_one(pool).await?);

    let private = PrivacyRecord {
        private: true,
        join_channel_id: Some(700),
        blocked: BTreeSet::from([900, u64::MAX]),
    };
    assert!(store.save_privacy(100, 510, &private).await?);
    assert_eq!(
        store.privacy_in_guild(100).await?,
        BTreeMap::from([(510, private.clone())])
    );
    assert!(sqlx::query_scalar::<_, bool>(stamp).fetch_one(pool).await?);
    // The same record again changes nothing.
    assert!(store.save_privacy(100, 510, &private).await?);
    assert_eq!(store.privacy_in_guild(100).await?[&510], private);

    // The block list is replaced to match: 900 leaves, 901 joins.
    let edited = PrivacyRecord {
        blocked: BTreeSet::from([901, u64::MAX]),
        ..private.clone()
    };
    assert!(store.save_privacy(100, 510, &edited).await?);
    assert_eq!(store.privacy_in_guild(100).await?[&510], edited);

    // `/public` clears the flag and the Join channel; the block list stays.
    let public = PrivacyRecord {
        private: false,
        join_channel_id: None,
        blocked: edited.blocked.clone(),
    };
    assert!(store.save_privacy(100, 510, &public).await?);
    assert_eq!(store.privacy_in_guild(100).await?[&510], public);
    // Another guild never sees it.
    assert!(store.privacy_in_guild(101).await?.is_empty());

    // A record never outlives its room: an untracked room writes nothing.
    assert!(!store.save_privacy(100, 599, &private).await?);
    let orphans: i64 =
        sqlx::query_scalar("SELECT count(*) FROM voice_room_blocks WHERE room_channel_id = '599'")
            .fetch_one(pool)
            .await?;
    assert_eq!(orphans, 0);

    // The schema refuses a Join channel on a public room and bad ids.
    assert!(sqlx::query(
        "UPDATE voice_rooms SET private = FALSE, join_channel_id = '700'
         WHERE guild_id = '100' AND channel_id = '510'"
    )
    .execute(pool)
    .await
    .is_err());
    for bad in ["0", "abc", "-5"] {
        assert!(
            sqlx::query(
                "UPDATE voice_rooms SET private = TRUE, join_channel_id = $1
                 WHERE guild_id = '100' AND channel_id = '510'"
            )
            .bind(bad)
            .execute(pool)
            .await
            .is_err(),
            "join_channel_id {bad:?} must be refused"
        );
    }
    assert!(sqlx::query(
        "INSERT INTO voice_room_blocks (guild_id, room_channel_id, blocked_member_id)
         VALUES ('100', '510', '0')"
    )
    .execute(pool)
    .await
    .is_err());
    assert!(
        sqlx::query(
            "INSERT INTO voice_room_blocks (guild_id, room_channel_id, blocked_member_id)
             VALUES ('100', '598', '902')"
        )
        .execute(pool)
        .await
        .is_err(),
        "a block row needs its room"
    );

    // The block list dies with the room, not with a privacy toggle.
    assert_eq!(store.remove_room(100, 510).await?, Some(room));
    let blocks: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM voice_room_blocks WHERE guild_id = '100' AND room_channel_id = '510'",
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(blocks, 0);
    assert!(store.privacy_in_guild(100).await?.is_empty());
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
