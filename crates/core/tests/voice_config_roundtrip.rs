//! V11 configuration export/import round-trip acceptance ([TOG-12506]).
//!
//! Tests-only slice against the existing pure codec in
//! `two_bot_core::voice_config` (`export_configuration`,
//! `import_configuration`, `validate_configuration`). No `src` changes, no
//! Discord, no database.
//!
//! Pins the `docs/voice-rooms.md` V11 contract on a synthetic guild
//! inventory:
//! - export-to-import round-trips creators, templates, aliases, lists and
//!   logging, with no runtime room/owner state on the wire;
//! - unknown channel IDs are reported (`UnknownReference`) and can be skipped
//!   caller-side, with nothing written until confirm (failed
//!   validate/import leaves inputs untouched and a valid import still
//!   succeeds afterwards);
//! - versioned JSON: wrong versions refused on import and export; oversize
//!   payloads refused at [`MAX_IMPORT_BYTES`] before parsing.

use std::collections::BTreeMap;

use serde_json::{json, Value};
use two_bot_core::voice_config::{
    export_configuration, import_configuration, validate_configuration, ChannelKind,
    ChannelReference, GuildInventory, VoiceConfigError, VoiceConfiguration, MAX_IMPORT_BYTES,
    VOICE_CONFIG_VERSION,
};

const GUILD: &str = "18446744073709551615";

// Synthetic guild inventory plus a complete version-1 configuration that
// exercises every exported section. IDs are synthetic only.
fn fixture() -> (VoiceConfiguration, GuildInventory) {
    let config = serde_json::from_value(json!({
        "version": 1,
        "guild_id": GUILD,
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
                "channel_id": "106",
                "name_template": "@@owner@@ ##",
                "status_template": null,
                "default_limit": 0,
                "always_private": false,
                "text_channels": false,
                "position": "below",
                "first_number": 1,
                "group_by_category": false,
                "permission_source": {"kind": "creator"}
            }
        ],
        "templates": [
            {"channel_id": "102", "name_template": "__resting/@@num@@ people__", "status_template": null},
            {"channel_id": "103", "name_template": "Stage night", "status_template": "LIVE"}
        ],
        "aliases": [
            {"game": "A game", "alias": "遊び"},
            {"game": "B game", "alias": "Bee"}
        ],
        "lists": [
            {"name": "rooms", "choices": ["den", "crew", "🎮"]},
            {"name": "moods", "choices": ["cozy", "loud"]}
        ],
        "logging": {
            "channel_id": "104",
            "detail": "verbose",
            "mention_member_ids": ["301"],
            "mention_role_ids": ["201"]
        },
        "settings": {
            "creation_enabled": true,
            "unique_names": true,
            "no_game_label": "General",
            "force_single_game": false,
            "count_members_without_activity": true,
            "time_zone": "Europe/London",
            "text_channel_name": "voice-chat",
            "text_viewer_role_id": GUILD,
            "command_role_id": "201",
            "command_roles": [{"command": "kick", "role_ids": ["201"]}]
        }
    }))
    .unwrap();
    let channels = [
        ("101", ChannelKind::Voice),
        ("102", ChannelKind::Voice),
        ("103", ChannelKind::Stage),
        ("104", ChannelKind::Text),
        ("105", ChannelKind::Category),
        ("106", ChannelKind::Voice),
    ]
    .into_iter()
    .map(|(id, kind)| {
        (
            id.to_owned(),
            ChannelReference {
                guild_id: GUILD.to_owned(),
                kind,
            },
        )
    })
    .collect();
    let inventory = GuildInventory {
        guild_id: GUILD.to_owned(),
        channels,
        roles: BTreeMap::from([
            ("201".to_owned(), GUILD.to_owned()),
            (GUILD.to_owned(), GUILD.to_owned()),
        ]),
        members: BTreeMap::from([("301".to_owned(), GUILD.to_owned())]),
    };
    (config, inventory)
}

fn import_value(
    value: &Value,
    inventory: &GuildInventory,
) -> Result<VoiceConfiguration, VoiceConfigError> {
    import_configuration(&serde_json::to_vec(value).unwrap(), inventory)
}

#[test]
fn export_to_import_round_trips_every_section_without_runtime_state() {
    let (config, inventory) = fixture();

    let wire = export_configuration(&config, &inventory).unwrap();
    let decoded = import_configuration(&wire, &inventory).unwrap();
    assert_eq!(decoded, config);
    // Deterministic: re-exporting the decoded config reproduces the wire.
    assert_eq!(export_configuration(&decoded, &inventory).unwrap(), wire);

    // The wire is versioned JSON carrying exactly the configuration sections.
    let value: Value = serde_json::from_slice(&wire).unwrap();
    assert_eq!(value["version"], json!(VOICE_CONFIG_VERSION));
    assert_eq!(value["guild_id"], json!(GUILD));
    let mut keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "aliases",
            "creators",
            "guild_id",
            "lists",
            "logging",
            "settings",
            "templates",
            "version"
        ]
    );

    // No runtime room/owner state travels in the export.
    let text = String::from_utf8(wire).unwrap();
    for leaked in [
        "room_id",
        "caretaker",
        "original_creator",
        "owner_id",
        "current_members",
    ] {
        assert!(
            !text.contains(leaked),
            "export leaks runtime state: {leaked}"
        );
    }
}

#[test]
fn unknown_channel_ids_are_reported_and_skippable_with_nothing_written() {
    let (config, inventory) = fixture();
    let original = serde_json::to_value(&config).unwrap();

    // Each unknown channel ID is reported with its field and kind, never
    // echoing uploaded text.
    let rows: Vec<(&str, Box<dyn Fn(&mut Value)>, &str)> = vec![
        (
            "creator channel",
            Box::new(|value: &mut Value| {
                value["creators"][0]["channel_id"] = json!("999");
            }),
            "creators[0].channel_id",
        ),
        (
            "creator permission source channel",
            Box::new(|value: &mut Value| {
                value["creators"][0]["permission_source"] =
                    json!({"kind": "channel", "channel_id": "999"});
            }),
            "creators[0].permission_source.channel_id",
        ),
        (
            "template channel",
            Box::new(|value: &mut Value| {
                value["templates"][0]["channel_id"] = json!("999");
            }),
            "templates[0].channel_id",
        ),
        (
            "logging channel",
            Box::new(|value: &mut Value| {
                value["logging"]["channel_id"] = json!("999");
            }),
            "logging.channel_id",
        ),
    ];
    for (name, mutate, field) in rows {
        let mut value = original.clone();
        mutate(&mut value);
        let bytes = serde_json::to_vec(&value).unwrap();
        let saved_bytes = bytes.clone();
        let saved_inventory = inventory.clone();
        let error = import_configuration(&bytes, &inventory).unwrap_err();
        assert!(
            matches!(&error, VoiceConfigError::UnknownReference { field: actual, kind }
                if actual == field && *kind == "channel"),
            "{name}: got {error}"
        );
        assert!(
            !error.to_string().contains("untrusted"),
            "{name}: error echoes uploaded text"
        );
        // Nothing written until confirm: inputs are untouched and a valid
        // import still succeeds afterwards.
        assert_eq!(bytes, saved_bytes);
        assert_eq!(inventory, saved_inventory);
        let valid = export_configuration(&config, &inventory).unwrap();
        assert_eq!(import_configuration(&valid, &inventory).unwrap(), config);
    }

    // Skipped caller-side: dropping the unknown creator entry leaves a valid
    // configuration with the remaining sections intact.
    let mut skipped = original.clone();
    skipped["creators"][0]["channel_id"] = json!("999");
    assert!(import_value(&skipped, &inventory).is_err());
    skipped
        .as_object_mut()
        .unwrap()
        .insert("creators".to_owned(), json!([original["creators"][1]]));
    let remaining = import_value(&skipped, &inventory).unwrap();
    assert_eq!(remaining.creators.len(), 1);
    assert_eq!(remaining.creators[0].channel_id, "106");
    assert_eq!(remaining.templates, config.templates);
    assert_eq!(remaining.aliases, config.aliases);
    assert_eq!(remaining.lists, config.lists);
    validate_configuration(&remaining, &inventory).unwrap();
    assert_eq!(
        import_configuration(
            &export_configuration(&remaining, &inventory).unwrap(),
            &inventory
        )
        .unwrap(),
        remaining
    );
}

#[test]
fn wrong_versions_are_refused_on_import_and_export() {
    let (mut config, inventory) = fixture();
    assert_eq!(VOICE_CONFIG_VERSION, 1);
    for version in [0, 2, u32::MAX] {
        config.version = version;
        assert!(
            matches!(
                export_configuration(&config, &inventory),
                Err(VoiceConfigError::UnsupportedVersion(v)) if v == version
            ),
            "export version {version}"
        );
        let bytes = serde_json::to_vec(&config).unwrap();
        assert!(
            matches!(
                import_configuration(&bytes, &inventory),
                Err(VoiceConfigError::UnsupportedVersion(v)) if v == version
            ),
            "import version {version}"
        );
    }
    // The fixture itself is current-version and round-trips.
    let (config, inventory) = fixture();
    assert_eq!(config.version, VOICE_CONFIG_VERSION);
    let wire = export_configuration(&config, &inventory).unwrap();
    assert_eq!(import_configuration(&wire, &inventory).unwrap(), config);
}

#[test]
fn oversize_payloads_are_refused_before_parsing_without_partial_application() {
    let (config, inventory) = fixture();
    let saved_inventory = inventory.clone();
    let valid = export_configuration(&config, &inventory).unwrap();
    assert!(
        valid.len() < MAX_IMPORT_BYTES,
        "fixture must leave headroom below the cap"
    );

    // The cap is inclusive: exactly MAX_IMPORT_BYTES still imports (trailing
    // JSON whitespace), one byte more fails on the document before parsing.
    let mut exact = valid.clone();
    exact.extend(std::iter::repeat_n(b' ', MAX_IMPORT_BYTES - valid.len()));
    assert_eq!(exact.len(), MAX_IMPORT_BYTES);
    assert_eq!(import_configuration(&exact, &inventory).unwrap(), config);

    let mut over = exact.clone();
    over.push(b' ');
    let error = import_configuration(&over, &inventory).unwrap_err();
    assert!(
        matches!(
            &error,
            VoiceConfigError::Invalid { field, reason }
            if field == "document" && *reason == "exceeds the import size limit"
        ),
        "one byte over: got {error}"
    );

    // Oversize non-JSON fails at the size gate, not as Malformed.
    for (name, bytes) in [
        ("oversized garbage", vec![b'x'; MAX_IMPORT_BYTES + 1]),
        ("oversized whitespace", vec![b' '; MAX_IMPORT_BYTES + 1]),
    ] {
        let error = import_configuration(&bytes, &inventory).unwrap_err();
        assert!(
            matches!(
                &error,
                VoiceConfigError::Invalid { field, .. } if field == "document"
            ),
            "{name}: got {error}"
        );
    }

    // Nothing partially applied: inventory untouched, valid import works.
    assert_eq!(inventory, saved_inventory);
    assert_eq!(import_configuration(&valid, &inventory).unwrap(), config);
}
