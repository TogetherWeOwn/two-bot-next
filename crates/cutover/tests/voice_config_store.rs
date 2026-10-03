//! V11b configuration persistence (TOG-12875). Fixed test-container target:
//! never read DATABASE_URL or use staging credentials.
//! Run: cargo test -p two-bot-cutover --test voice_config_store --locked -- --ignored

use serde_json::json;
use sqlx::{postgres::PgConnectOptions, postgres::PgPoolOptions, PgPool, Postgres, QueryBuilder};
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use two_bot_core::voice_config::{
    export_configuration, import_configuration, ChannelKind, ChannelReference,
    CreatorConfiguration, GuildInventory, PermissionSource, RoomPosition, VoiceConfiguration,
};
use two_bot_core::voice_rooms::{CreatorChannel, TextCompanion, VoiceRoom};
use two_bot_core::voice_text_channel::TextChannelSettings;
use two_bot_cutover::voice_config_store::PgVoiceConfigStore;
use two_bot_cutover::voice_rooms::PgRoomStore;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const GUILD: u64 = u64::MAX;
const GUILD_ID: &str = "18446744073709551615";

/// Synthetic IDs only; every version-1 field is populated, and the list/set
/// members are already in the store's canonical (sorted) order.
fn fixture() -> (VoiceConfiguration, GuildInventory) {
    let config = serde_json::from_value(json!({
        "version": 1,
        "guild_id": GUILD_ID,
        "creators": [
            {
                "channel_id": "101",
                "name_template": "@@random_emoji@@ @@owner@@'s [[den/crew/lair]] ##",
                "status_template": "@@game_name@@ · @@num@@",
                "default_limit": 99,
                "always_private": true,
                "text_channels": true,
                "position": "above",
                "first_number": 3,
                "group_by_category": true,
                "permission_source": {"kind": "channel", "channel_id": "105"}
            },
            {
                "channel_id": "1010",
                "name_template": "Quiet; SELECT 'not SQL'",
                "status_template": null,
                "default_limit": 0,
                "always_private": false,
                "text_channels": false,
                "position": "below",
                "first_number": 4294967295u32,
                "group_by_category": false,
                "permission_source": {"kind": "category"}
            }
        ],
        "templates": [
            {"channel_id": "102", "name_template": "__resting/@@num@@ people__", "status_template": null},
            {"channel_id": "103", "name_template": "🎮 Stage", "status_template": "LIVE"}
        ],
        "aliases": [
            {"game": "A game", "alias": "遊び"},
            {"game": "Other", "alias": "O"}
        ],
        "lists": [
            {"name": "rooms", "choices": ["den", "crew", "🎮", "den"]},
            {"name": "snacks", "choices": ["tea"]}
        ],
        "logging": {
            "channel_id": "104",
            "detail": "verbose",
            "mention_member_ids": ["301", "3010"],
            "mention_role_ids": ["201", "2010"]
        },
        "settings": {
            "creation_enabled": true,
            "unique_names": true,
            "no_game_label": "General",
            "force_single_game": false,
            "count_members_without_activity": true,
            "time_zone": "Europe/London",
            "text_channel_name": "voice-chat",
            "text_viewer_role_id": GUILD_ID,
            "command_role_id": "201",
            "command_roles": [
                {"command": "kick", "role_ids": ["201", "2010"]},
                {"command": "limit", "role_ids": []}
            ]
        }
    }))
    .expect("fixture decodes");
    let channels = [
        ("101", ChannelKind::Voice),
        ("1010", ChannelKind::Voice),
        ("102", ChannelKind::Voice),
        ("103", ChannelKind::Stage),
        ("104", ChannelKind::Text),
        ("105", ChannelKind::Category),
    ]
    .into_iter()
    .map(|(id, kind)| {
        (
            id.to_owned(),
            ChannelReference {
                guild_id: GUILD_ID.to_owned(),
                kind,
            },
        )
    })
    .collect();
    let inventory = GuildInventory {
        guild_id: GUILD_ID.to_owned(),
        channels,
        roles: BTreeMap::from([
            ("201".to_owned(), GUILD_ID.to_owned()),
            ("2010".to_owned(), GUILD_ID.to_owned()),
            (GUILD_ID.to_owned(), GUILD_ID.to_owned()),
        ]),
        members: BTreeMap::from([
            ("301".to_owned(), GUILD_ID.to_owned()),
            ("3010".to_owned(), GUILD_ID.to_owned()),
        ]),
    };
    (config, inventory)
}

/// A configuration that differs from [`fixture`] in every section.
fn alternate(config: &VoiceConfiguration) -> VoiceConfiguration {
    let mut other = config.clone();
    other.creators[0].name_template = "changed".to_owned();
    other.creators[0].group_by_category = false;
    other.creators.truncate(1);
    other.templates.clear();
    other.aliases[0].alias = "x".to_owned();
    other.lists[0].choices.push("new".to_owned());
    other.logging = None;
    other.settings.unique_names = false;
    other.settings.command_roles.clear();
    other
}

#[tokio::test]
#[ignore = "requires agent-testdb:5432 with agent_test and an empty password"]
async fn voice_config_store_snapshot_apply_and_rollback() -> TestResult {
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
    let schema = format!("voice_config_test_{}_{}", std::process::id(), nonce);
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

/// Rows across every configuration table for one guild.
async fn config_rows(pool: &PgPool, guild: &str) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM voice_creators WHERE guild_id = $1)
              + (SELECT count(*) FROM voice_channel_templates WHERE guild_id = $1)
              + (SELECT count(*) FROM voice_game_aliases WHERE guild_id = $1)
              + (SELECT count(*) FROM voice_random_lists WHERE guild_id = $1)
              + (SELECT count(*) FROM voice_random_list_choices WHERE guild_id = $1)
              + (SELECT count(*) FROM voice_logging WHERE guild_id = $1)
              + (SELECT count(*) FROM voice_logging_mention_members WHERE guild_id = $1)
              + (SELECT count(*) FROM voice_logging_mention_roles WHERE guild_id = $1)
              + (SELECT count(*) FROM voice_guild_settings WHERE guild_id = $1)
              + (SELECT count(*) FROM voice_command_roles WHERE guild_id = $1)
              + (SELECT count(*) FROM voice_command_role_members WHERE guild_id = $1)",
    )
    .bind(guild)
    .fetch_one(pool)
    .await
}

async fn verify_store(pool: &PgPool, schema: &str) -> TestResult {
    sqlx::query("SELECT set_config('search_path', $1, false)")
        .bind(schema)
        .execute(pool)
        .await?;
    sqlx::migrate!("./migrations").run(pool).await?;
    let store = PgVoiceConfigStore::new(pool.clone());
    let rooms = PgRoomStore::new(pool.clone());
    let (config, inventory) = fixture();

    // A guild that was never configured exports its defaults and no rows.
    let fresh = store.snapshot(GUILD).await?;
    assert!(fresh.creators.is_empty() && fresh.templates.is_empty());
    assert!(fresh.aliases.is_empty() && fresh.lists.is_empty() && fresh.logging.is_none());
    assert!(fresh.settings.creation_enabled && fresh.settings.command_roles.is_empty());
    assert_eq!(fresh.guild_id, GUILD_ID);
    assert_eq!(config_rows(pool, GUILD_ID).await?, 0);

    // Live rooms and companions exist before any apply and must never change.
    let room = VoiceRoom {
        guild_id: GUILD,
        channel_id: 500,
        creator_channel_id: 101,
        owner_id: 300,
        original_creator_id: 300,
        name_seed: u64::MAX,
        created_at: "2026-09-30T01:00:00.123Z".to_owned(),
    };
    let companion = TextCompanion {
        guild_id: GUILD,
        room_channel_id: 500,
        text_channel_id: 600,
        settings: TextChannelSettings {
            enabled: true,
            configured_name: Some("Lounge".to_owned()),
            viewer_role_id: Some(7),
        },
        created_at: "2026-09-30T01:00:00.123Z".to_owned(),
    };
    assert!(rooms.add_room(&room).await?);
    assert!(rooms.add_companion(&companion).await?);

    // apply -> snapshot is the identity on a fully populated guild.
    store.apply(GUILD, &config).await?;
    let stored = store.snapshot(GUILD).await?;
    assert_eq!(stored, config);

    // snapshot -> export -> import -> apply -> snapshot is the identity, and
    // the exported document is byte-identical on the second pass.
    let exported = export_configuration(&stored, &inventory)?;
    let imported = import_configuration(&exported, &inventory)?;
    assert_eq!(imported, stored);
    store.apply(GUILD, &imported).await?;
    let again = store.snapshot(GUILD).await?;
    assert_eq!(again, stored);
    assert_eq!(export_configuration(&again, &inventory)?, exported);

    // Another guild is independent of this one.
    let mut neighbour = config.clone();
    neighbour.guild_id = "42".to_owned();
    store.apply(42, &neighbour).await?;
    assert_eq!(store.snapshot(42).await?, neighbour);

    // apply replaces changed rows and removes sections the document lacks.
    let smaller = alternate(&config);
    store.apply(GUILD, &smaller).await?;
    assert_eq!(store.snapshot(GUILD).await?, smaller);
    assert_eq!(
        store.snapshot(42).await?,
        neighbour,
        "other guild untouched"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM voice_logging WHERE guild_id = $1")
            .bind(GUILD_ID)
            .fetch_one(pool)
            .await?,
        0,
        "removed logging section leaves no row"
    );
    // Configuration never touches live rooms or companions.
    assert_eq!(rooms.room_for(GUILD, 500).await?, Some(room.clone()));
    assert_eq!(
        rooms.companion_for(GUILD, 500).await?,
        Some(companion.clone())
    );
    assert_eq!(rooms.rooms_in_guild(GUILD).await?, vec![room.clone()]);

    // A failing row rolls back every section. Each rejected document differs
    // from the stored one in every section; the failure lands in a different
    // section each time (creators, logging, settings).
    store.apply(GUILD, &config).await?;
    let before = store.snapshot(GUILD).await?;
    assert_eq!(before, config);
    let rows_before = config_rows(pool, GUILD_ID).await?;
    let mut bad_creator = alternate(&config);
    bad_creator.creators[0].first_number = 0; // CHECK first_room_number >= 1
    let mut bad_limit = alternate(&config);
    bad_limit.creators[0].default_limit = 100; // CHECK default_limit 0..=99
    let mut bad_logging = alternate(&config);
    bad_logging.logging = config.logging.clone();
    bad_logging
        .logging
        .as_mut()
        .expect("logging")
        .mention_member_ids = vec!["0301".to_owned()];
    let mut bad_settings = alternate(&config);
    bad_settings.settings.time_zone = "  ".to_owned(); // CHECK non-blank
    let mut wrong_guild = alternate(&config);
    wrong_guild.guild_id = "43".to_owned();
    let mut wrong_version = alternate(&config);
    wrong_version.version = 2;
    for (label, document) in [
        ("creator first number", &bad_creator),
        ("creator limit", &bad_limit),
        ("logging snowflake", &bad_logging),
        ("settings time zone", &bad_settings),
        ("guild mismatch", &wrong_guild),
        ("version", &wrong_version),
    ] {
        assert!(
            store.apply(GUILD, document).await.is_err(),
            "{label} must be rejected"
        );
        assert_eq!(
            store.snapshot(GUILD).await?,
            before,
            "{label} left the previous configuration intact"
        );
        assert_eq!(config_rows(pool, GUILD_ID).await?, rows_before);
    }
    assert_eq!(rooms.room_for(GUILD, 500).await?, Some(room));

    // The codec cannot say "inherit the live creator's limit" (stored NULL,
    // snapshot 0): applying a snapshot back keeps the NULL, and the V9b
    // per-creator columns the configuration does not carry survive.
    let mut creator = CreatorChannel::new(77, 7);
    creator.default_limit = None;
    creator.text_channels = true;
    creator.text_channel_name = Some("Squad chat".to_owned());
    creator.text_viewer_role_id = Some(9);
    rooms.add_creator(&creator).await?;
    let snapshot = store.snapshot(77).await?;
    assert_eq!(snapshot.creators.len(), 1);
    assert_eq!(snapshot.creators[0].default_limit, 0);
    store.apply(77, &snapshot).await?;
    assert_eq!(rooms.creator_for(77, 7).await?, Some(creator));
    Ok(())
}

#[tokio::test]
#[ignore = "requires agent-testdb:5432 with agent_test and an empty password"]
async fn voice_config_per_creator_limit_inherit_edges() -> TestResult {
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
    let schema = format!("voice_config_inherit_{}_{}", std::process::id(), nonce);
    QueryBuilder::<Postgres>::new("CREATE SCHEMA ")
        .push(&schema)
        .build()
        .execute(&pool)
        .await?;
    let result = verify_inherit_edges(&pool, &schema).await;
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

/// Stored `voice_creators.default_limit`: `None` is a missing row, `Some(None)`
/// is inherit-the-live-limit, `Some(Some(n))` is an explicit override.
async fn raw_default_limit(
    pool: &PgPool,
    guild: &str,
    channel: &str,
) -> Result<Option<Option<i32>>, sqlx::Error> {
    sqlx::query_scalar::<_, Option<i32>>(
        "SELECT default_limit FROM voice_creators WHERE guild_id = $1 AND channel_id = $2",
    )
    .bind(guild)
    .bind(channel)
    .fetch_optional(pool)
    .await
}

/// Per-creator `default_limit` inherit edges (0225): NULL means "inherit the
/// live creator's limit" and has no codec value, so the snapshot reports it as
/// 0 and an apply of 0 over an existing NULL keeps the NULL. Explicit 0 is
/// unlimited and behaves like any other override.
async fn verify_inherit_edges(pool: &PgPool, schema: &str) -> TestResult {
    sqlx::query("SELECT set_config('search_path', $1, false)")
        .bind(schema)
        .execute(pool)
        .await?;
    sqlx::migrate!("./migrations").run(pool).await?;
    let store = PgVoiceConfigStore::new(pool.clone());
    let rooms = PgRoomStore::new(pool.clone());

    // Three live creators: inherit (NULL), explicit unlimited (0), cap (4).
    let mut inherit = CreatorChannel::new(78, 7);
    inherit.name_template = "Inherit @@owner@@ ##".to_owned();
    inherit.text_channels = true;
    inherit.text_channel_name = Some("Squad chat".to_owned());
    inherit.text_viewer_role_id = Some(9);
    let mut unlimited = CreatorChannel::new(78, 8);
    unlimited.name_template = "Open @@owner@@ ##".to_owned();
    unlimited.default_limit = Some(0);
    unlimited.private_default = true;
    unlimited.position = two_bot_core::voice_rooms::RoomPosition::Below;
    unlimited.first_room_number = 3;
    let mut capped = CreatorChannel::new(78, 9);
    capped.name_template = "Capped @@owner@@ ##".to_owned();
    capped.default_limit = Some(4);
    capped.permission_source = two_bot_core::voice_rooms::PermissionSource::Category;
    capped.first_room_number = 5;
    rooms.add_creator(&inherit).await?;
    rooms.add_creator(&unlimited).await?;
    rooms.add_creator(&capped).await?;
    assert_eq!(raw_default_limit(pool, "78", "7").await?, Some(None));
    assert_eq!(raw_default_limit(pool, "78", "8").await?, Some(Some(0)));
    assert_eq!(raw_default_limit(pool, "78", "9").await?, Some(Some(4)));

    // The snapshot cannot tell inherit from unlimited: both report 0.
    let snapshot = store.snapshot(78).await?;
    assert_eq!(snapshot.creators.len(), 3);
    assert_eq!(snapshot.creators[0].channel_id, "7");
    assert_eq!(snapshot.creators[1].channel_id, "8");
    assert_eq!(snapshot.creators[2].channel_id, "9");
    assert_eq!(snapshot.creators[0].default_limit, 0);
    assert_eq!(snapshot.creators[1].default_limit, 0);
    assert_eq!(snapshot.creators[2].default_limit, 4);

    // A plain round trip keeps the NULL: inherit never becomes unlimited.
    store.apply(78, &snapshot).await?;
    assert_eq!(raw_default_limit(pool, "78", "7").await?, Some(None));
    assert_eq!(raw_default_limit(pool, "78", "8").await?, Some(Some(0)));
    assert_eq!(raw_default_limit(pool, "78", "9").await?, Some(Some(4)));
    assert_eq!(rooms.creator_for(78, 7).await?, Some(inherit.clone()));

    // Editing a creator's other settings through the configuration never flips
    // inherit to unlimited, and the V9b columns the configuration does not
    // carry survive the upsert.
    let mut edited = snapshot.clone();
    let first = &mut edited.creators[0];
    first.name_template = "Renamed @@owner@@ ##".to_owned();
    first.status_template = Some("Live @@game_name@@".to_owned());
    first.always_private = true;
    first.text_channels = false;
    first.position = RoomPosition::Below;
    first.first_number = 9;
    first.group_by_category = true;
    first.permission_source = PermissionSource::Category {};
    store.apply(78, &edited).await?;
    assert_eq!(
        raw_default_limit(pool, "78", "7").await?,
        Some(None),
        "settings edit keeps inherit"
    );
    let stored = store.snapshot(78).await?;
    assert_eq!(stored.creators[0], edited.creators[0]);
    let live = rooms.creator_for(78, 7).await?.expect("creator survives");
    assert_eq!(live.text_channel_name, Some("Squad chat".to_owned()));
    assert_eq!(live.text_viewer_role_id, Some(9));
    assert!(!live.text_channels);

    // An explicit limit over an inherit row clears the inherit ...
    let mut recapped = stored.clone();
    recapped.creators[0].default_limit = 4;
    store.apply(78, &recapped).await?;
    assert_eq!(raw_default_limit(pool, "78", "7").await?, Some(Some(4)));
    // ... and an explicit 0 over a cap stores unlimited, never NULL.
    let mut cleared = store.snapshot(78).await?;
    cleared.creators[0].default_limit = 0;
    store.apply(78, &cleared).await?;
    assert_eq!(raw_default_limit(pool, "78", "7").await?, Some(Some(0)));
    // The second snapshot/apply cycle is stable once the row is explicit.
    let stable = store.snapshot(78).await?;
    store.apply(78, &stable).await?;
    assert_eq!(raw_default_limit(pool, "78", "7").await?, Some(Some(0)));
    assert_eq!(store.snapshot(78).await?, stable);

    // Fresh rows inserted through the configuration are explicit: a 0 limit on
    // a guild that never had the creator stores 0, not NULL, and the 99
    // boundary round-trips alongside per-creator privacy/position/numbering.
    let mut fresh = store.snapshot(79).await?;
    assert!(fresh.creators.is_empty());
    fresh.creators.push(CreatorConfiguration {
        channel_id: "81".to_owned(),
        name_template: "Fresh @@owner@@ ##".to_owned(),
        status_template: None,
        default_limit: 0,
        always_private: false,
        text_channels: false,
        position: RoomPosition::Above,
        first_number: 1,
        group_by_category: false,
        permission_source: PermissionSource::Creator {},
    });
    fresh.creators.push(CreatorConfiguration {
        channel_id: "82".to_owned(),
        name_template: "Full @@owner@@ ##".to_owned(),
        status_template: Some("On a quest".to_owned()),
        default_limit: 99,
        always_private: true,
        text_channels: true,
        position: RoomPosition::Below,
        first_number: 2,
        group_by_category: true,
        permission_source: PermissionSource::Category {},
    });
    store.apply(79, &fresh).await?;
    assert_eq!(raw_default_limit(pool, "79", "81").await?, Some(Some(0)));
    assert_eq!(raw_default_limit(pool, "79", "82").await?, Some(Some(99)));
    assert_eq!(store.snapshot(79).await?, fresh);

    // snapshot -> export -> import -> apply is the identity for the fresh guild
    // too, pinning per-creator separation through the wire format.
    let inventory = GuildInventory {
        guild_id: "79".to_owned(),
        channels: [("81", ChannelKind::Voice), ("82", ChannelKind::Voice)]
            .into_iter()
            .map(|(id, kind)| {
                (
                    id.to_owned(),
                    ChannelReference {
                        guild_id: "79".to_owned(),
                        kind,
                    },
                )
            })
            .collect(),
        roles: BTreeMap::new(),
        members: BTreeMap::new(),
    };
    let exported = export_configuration(&fresh, &inventory)?;
    let imported = import_configuration(&exported, &inventory)?;
    assert_eq!(imported, fresh);
    store.apply(79, &imported).await?;
    assert_eq!(store.snapshot(79).await?, fresh);
    Ok(())
}
