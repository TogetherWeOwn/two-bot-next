//! Import-validation edge rows for the merged V11 codec ([TOG-12091]).
//!
//! Each row pins a documented `two_bot_core::voice_config` behaviour:
//! - malformed table: anything that is not a JSON object document (including
//!   YAML shapes, truncation and non-UTF-8 bytes) fails as
//!   [`VoiceConfigError::Malformed`] with line/column only, never echoing
//!   uploaded text;
//! - oversized table: the [`MAX_IMPORT_BYTES`] gate in
//!   `import_configuration` fires before parsing as `Invalid` on the
//!   document, so an oversized upload can never partially apply;
//! - duplicate table: the uniqueness checks in `validate_configuration`
//!   report the second occurrence (`Invalid` naming its field) even when the
//!   two entries carry different payloads, never silently merging.

use std::collections::BTreeMap;

use serde_json::{json, Value};
use two_bot_core::voice_config::{
    export_configuration, import_configuration, ChannelKind, ChannelReference, GuildInventory,
    VoiceConfigError, VoiceConfiguration, MAX_IMPORT_BYTES,
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
fn malformed_payloads_are_rejected_with_line_and_column_only() {
    let (config, inventory) = fixture();
    let valid = export_configuration(&config, &inventory).unwrap();
    let rows: Vec<(&str, Vec<u8>)> = vec![
        ("empty", b"".to_vec()),
        ("yaml document marker", b"---\nversion: 1\n".to_vec()),
        (
            "yaml block mapping",
            b"version: 1\nguild_id: '101'\nmarker: untrusted-content\n".to_vec(),
        ),
        (
            "yaml flow mapping with unquoted keys",
            b"{version: 1}".to_vec(),
        ),
        (
            "yaml single-quoted whole document",
            b"'{\"version\": 1}'".to_vec(),
        ),
        ("nul byte", vec![0x00]),
        ("non-utf8 bytes", vec![0xff, 0xfe, 0x00, 0x41]),
        (
            "truncated valid document",
            valid[..valid.len() / 2].to_vec(),
        ),
        (
            "valid document with trailing garbage",
            [valid.as_slice(), b" {\"version\": 1}"].concat(),
        ),
    ];
    for (name, bytes) in rows {
        let error = import_configuration(&bytes, &inventory).unwrap_err();
        assert!(
            matches!(error, VoiceConfigError::Malformed { .. }),
            "{name}: got {error}"
        );
        assert!(
            !error.to_string().contains("untrusted-content"),
            "{name}: error echoes uploaded text"
        );
    }
}

#[test]
fn oversized_imports_fail_at_the_documented_limit_without_partial_application() {
    let (config, inventory) = fixture();
    let saved_inventory = inventory.clone();
    let valid = export_configuration(&config, &inventory).unwrap();
    assert!(
        valid.len() < MAX_IMPORT_BYTES,
        "fixture must leave headroom below the cap"
    );

    // The cap is inclusive: a document of exactly MAX_IMPORT_BYTES imports.
    let mut exact = valid.clone();
    exact.extend(std::iter::repeat_n(b' ', MAX_IMPORT_BYTES - valid.len()));
    assert_eq!(exact.len(), MAX_IMPORT_BYTES);
    assert_eq!(import_configuration(&exact, &inventory).unwrap(), config);

    // One byte over the cap fails on the document before parsing.
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
    assert_eq!(
        error.to_string(),
        "invalid configuration field document: exceeds the import size limit"
    );

    // Oversized non-JSON fails at the size gate, not as Malformed.
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

    // Nothing partially applied: inputs are untouched and a valid import
    // still succeeds afterwards.
    assert_eq!(inventory, saved_inventory);
    assert_eq!(import_configuration(&valid, &inventory).unwrap(), config);
}

#[test]
fn duplicate_room_definitions_are_reported_not_silently_merged() {
    type DuplicateRow = (&'static str, Box<dyn Fn(&mut Value)>, &'static str);
    let (config, inventory) = fixture();
    let original = serde_json::to_value(&config).unwrap();
    let rows: Vec<DuplicateRow> = vec![
        (
            "second creator reuses the channel with a different template",
            Box::new(|value: &mut Value| {
                let mut duplicate = value["creators"][0].clone();
                duplicate["name_template"] = json!("@@owner@@'s second room ##");
                duplicate["default_limit"] = json!(4);
                value["creators"].as_array_mut().unwrap().push(duplicate);
            }),
            "creators[1].channel_id",
        ),
        (
            "creator takes a template channel",
            Box::new(|value: &mut Value| {
                value["creators"][0]["channel_id"] = json!("102");
            }),
            "templates[0].channel_id",
        ),
        (
            "template takes a creator channel",
            Box::new(|value: &mut Value| {
                value["templates"][0]["channel_id"] = json!("101");
            }),
            "templates[0].channel_id",
        ),
        (
            "alias reuses a game with a different alias",
            Box::new(|value: &mut Value| {
                value["aliases"].as_array_mut().unwrap().push(json!({
                    "game": "A game",
                    "alias": "a different alias",
                }));
            }),
            "aliases[1].game",
        ),
        (
            "list reuses a name with different choices",
            Box::new(|value: &mut Value| {
                value["lists"].as_array_mut().unwrap().push(json!({
                    "name": "rooms",
                    "choices": ["other"],
                }));
            }),
            "lists[1].name",
        ),
        (
            "command restriction reuses a command with different roles",
            Box::new(|value: &mut Value| {
                value["settings"]["command_roles"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"command": "kick", "role_ids": []}));
            }),
            "settings.command_roles[1].command",
        ),
        (
            "logging mention member repeats",
            Box::new(|value: &mut Value| {
                value["logging"]["mention_member_ids"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!("301"));
            }),
            "logging.mention_member_ids[1]",
        ),
        (
            "command role id repeats",
            Box::new(|value: &mut Value| {
                value["settings"]["command_roles"][0]["role_ids"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!("201"));
            }),
            "settings.command_roles[0].role_ids[1]",
        ),
    ];
    for (name, mutate, field) in rows {
        let mut value = original.clone();
        mutate(&mut value);
        let error = import_value(&value, &inventory).unwrap_err();
        assert!(
            matches!(
                &error,
                VoiceConfigError::Invalid { field: actual, reason }
                if actual == field && *reason == "duplicate entry"
            ),
            "{name}: got {error}"
        );
    }
}
