//! V11 import-map validation cases for the template/settings maps into
//! creator-channel config ([TOG-14145]).
//!
//! Offline fixtures only against the pure `two_bot_core::voice_config` codec
//! (`import_configuration`, `export_configuration`): no Discord, no database,
//! no staging or production.
//!
//! Each row pins a documented mapping behaviour:
//! - unknown-field tolerance: an unrecognised key inside any template,
//!   settings or creator-channel map rejects the whole document as
//!   [`VoiceConfigError::Malformed`] with line/column only, never echoing
//!   uploaded text and never partially applying;
//! - invalid-value rejection: a wrong-typed, blank or out-of-range value in
//!   those maps is rejected (`Malformed` for wire-type breaks, `Invalid` for
//!   validation breaks, `UnknownReference` for dangling references);
//! - empty-map defaults: an empty JSON object (`{}`) at any object boundary
//!   is rejected as `Malformed`, while empty sections (no creators, no
//!   templates, null logging, disabled settings) decode to their defaults
//!   and round-trip.

use std::collections::BTreeMap;

use serde_json::{json, Value};
use two_bot_core::voice_config::{
    export_configuration, import_configuration, ChannelKind, ChannelReference, GuildInventory,
    VoiceConfigError, VoiceConfiguration,
};

// Synthetic IDs only. Same complete version-1 example as the codec tests.
fn fixture() -> (VoiceConfiguration, GuildInventory) {
    let config = serde_json::from_value(json!({
        "version": 1,
        "guild_id": "18446744073709551615",
        "creators": [{
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
        }],
        "templates": [
            {"channel_id": "102", "name_template": "__resting/@@num@@ people__", "status_template": null},
            {"channel_id": "103", "name_template": "🎮 Stage", "status_template": "LIVE"}
        ],
        "aliases": [{"game": "A game", "alias": "遊び"}],
        "lists": [{"name": "rooms", "choices": ["den", "crew", "🎮"]}],
        "logging": {"channel_id": "104", "detail": "verbose", "mention_member_ids": ["301"], "mention_role_ids": ["201"]},
        "settings": {
            "creation_enabled": true,
            "unique_names": true,
            "no_game_label": "General",
            "force_single_game": false,
            "count_members_without_activity": true,
            "time_zone": "Europe/London",
            "text_channel_name": "voice-chat",
            "text_viewer_role_id": "18446744073709551615",
            "command_role_id": "201",
            "command_roles": [{"command": "kick", "role_ids": ["201"]}]
        }
    }))
    .unwrap();
    let guild_id = "18446744073709551615".to_owned();
    let channels = [
        ("101", ChannelKind::Voice),
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
                guild_id: guild_id.clone(),
                kind,
            },
        )
    })
    .collect();
    let inventory = GuildInventory {
        guild_id: guild_id.clone(),
        channels,
        roles: BTreeMap::from([
            ("201".to_owned(), guild_id.clone()),
            (guild_id.clone(), guild_id.clone()),
        ]),
        members: BTreeMap::from([("301".to_owned(), guild_id)]),
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
fn unknown_fields_are_rejected_at_template_settings_and_creator_boundaries() {
    let (config, inventory) = fixture();
    let original = serde_json::to_value(&config).unwrap();
    // (name, JSON pointer to the map, unknown key)
    let rows: Vec<(&str, &str, &str)> = vec![
        ("creator-channel map", "/creators/0", "unrecognized"),
        ("first template map", "/templates/0", "unrecognized"),
        ("second template map", "/templates/1", "unrecognized"),
        ("settings map", "/settings", "unrecognized"),
        (
            "command-roles map",
            "/settings/command_roles/0",
            "unrecognized",
        ),
        ("logging map", "/logging", "unrecognized"),
        ("alias map", "/aliases/0", "unrecognized"),
        ("list map", "/lists/0", "unrecognized"),
        (
            "creator permission-source map",
            "/creators/0/permission_source",
            "unrecognized",
        ),
    ];
    for (name, pointer, key) in rows {
        let mut value = original.clone();
        value.pointer_mut(pointer).unwrap()[key] = json!("untrusted-content");
        let error = import_value(&value, &inventory).unwrap_err();
        assert!(
            matches!(error, VoiceConfigError::Malformed { .. }),
            "{name}: got {error}"
        );
        assert!(
            !error.to_string().contains("untrusted-content"),
            "{name}: error echoes uploaded text"
        );
    }
    // Nothing partially applied: a valid import still succeeds afterwards.
    assert_eq!(import_value(&original, &inventory).unwrap(), config);
}

#[test]
fn invalid_values_are_rejected_at_template_settings_and_creator_boundaries() {
    let (config, inventory) = fixture();
    let original = serde_json::to_value(&config).unwrap();
    // (name, JSON pointer, invalid value, expected error shape)
    let rows: Vec<(&str, &str, Value, &str)> = vec![
        (
            "template channel id with the wrong type",
            "/templates/0/channel_id",
            json!(102),
            "Malformed",
        ),
        (
            "template name with the wrong type",
            "/templates/0/name_template",
            json!(7),
            "Malformed",
        ),
        (
            "template status with the wrong type",
            "/templates/0/status_template",
            json!(7),
            "Malformed",
        ),
        (
            "settings flag with the wrong type",
            "/settings/creation_enabled",
            json!("yes"),
            "Malformed",
        ),
        (
            "settings no-game label blank",
            "/settings/no_game_label",
            json!("   "),
            "Invalid",
        ),
        (
            "settings text channel name over the limit",
            "/settings/text_channel_name",
            json!("x".repeat(101)),
            "Invalid",
        ),
        (
            "settings time zone blank",
            "/settings/time_zone",
            json!(""),
            "Invalid",
        ),
        (
            "settings viewer role with the wrong type",
            "/settings/text_viewer_role_id",
            json!(201),
            "Malformed",
        ),
        (
            "command restriction name blank",
            "/settings/command_roles/0/command",
            json!("  "),
            "Invalid",
        ),
        (
            "command restriction role unknown",
            "/settings/command_roles/0/role_ids/0",
            json!("999"),
            "UnknownReference",
        ),
        (
            "creator name template with the wrong type",
            "/creators/0/name_template",
            json!(7),
            "Malformed",
        ),
        (
            "creator default limit above the maximum",
            "/creators/0/default_limit",
            json!(100),
            "Invalid",
        ),
        (
            "creator first number of zero",
            "/creators/0/first_number",
            json!(0),
            "Invalid",
        ),
        (
            "creator position outside the enum",
            "/creators/0/position",
            json!("sideways"),
            "Malformed",
        ),
        (
            "logging detail outside the enum",
            "/logging/detail",
            json!("everything"),
            "Malformed",
        ),
        (
            "logging channel unknown",
            "/logging/channel_id",
            json!("999"),
            "UnknownReference",
        ),
        (
            "list with no choices",
            "/lists/0/choices",
            json!([]),
            "Invalid",
        ),
        ("alias game blank", "/aliases/0/game", json!(""), "Invalid"),
    ];
    for (name, pointer, invalid, expected) in rows {
        let mut value = original.clone();
        *value.pointer_mut(pointer).unwrap() = invalid;
        let error = import_value(&value, &inventory).unwrap_err();
        let actual = match &error {
            VoiceConfigError::Malformed { .. } => "Malformed",
            VoiceConfigError::Invalid { .. } => "Invalid",
            VoiceConfigError::UnknownReference { .. } => "UnknownReference",
            other => panic!("{name}: unexpected error shape {other}"),
        };
        assert_eq!(actual, expected, "{name}: got {error}");
    }
    // Inputs are untouched: the valid document still imports afterwards.
    assert_eq!(import_value(&original, &inventory).unwrap(), config);
}

#[test]
fn empty_maps_are_rejected_but_empty_sections_decode_to_defaults() {
    let (config, inventory) = fixture();
    let original = serde_json::to_value(&config).unwrap();
    for pointer in [
        "/creators/0",
        "/templates/0",
        "/aliases/0",
        "/lists/0",
        "/logging",
        "/settings",
        "/settings/command_roles/0",
        "/creators/0/permission_source",
    ] {
        let mut value = original.clone();
        *value.pointer_mut(pointer).unwrap() = json!({});
        assert!(
            matches!(
                import_value(&value, &inventory),
                Err(VoiceConfigError::Malformed { .. })
            ),
            "empty map at {pointer} must be rejected"
        );
    }

    // Empty sections are the defaults, not an error: no creators, no
    // templates, no aliases, no lists, no logging, and disabled settings
    // with no command restrictions.
    let mut value = original.clone();
    value["creators"] = json!([]);
    value["templates"] = json!([]);
    value["aliases"] = json!([]);
    value["lists"] = json!([]);
    value["logging"] = Value::Null;
    value["settings"]["creation_enabled"] = json!(false);
    value["settings"]["text_viewer_role_id"] = Value::Null;
    value["settings"]["command_role_id"] = Value::Null;
    value["settings"]["command_roles"] = json!([]);
    let decoded = import_value(&value, &inventory).unwrap();
    assert!(decoded.creators.is_empty());
    assert!(decoded.templates.is_empty());
    assert!(decoded.aliases.is_empty());
    assert!(decoded.lists.is_empty());
    assert!(decoded.logging.is_none());
    assert!(decoded.settings.command_roles.is_empty());
    assert_eq!(serde_json::to_value(&decoded).unwrap(), value);
    assert_eq!(
        import_configuration(
            &export_configuration(&decoded, &inventory).unwrap(),
            &inventory
        )
        .unwrap(),
        decoded
    );
}
