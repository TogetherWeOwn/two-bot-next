use std::collections::BTreeMap;

use serde_json::{json, Value};
use two_bot_core::voice_config::{
    export_configuration, import_configuration, validate_configuration, ChannelKind,
    ChannelReference, GuildInventory, VoiceConfigError, VoiceConfiguration,
};

// Synthetic IDs only. Exercise every version-1 field without Discord or a DB.
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
    })).unwrap();
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
fn valid_export_import_is_lossless_and_deterministic() {
    let (config, inventory) = fixture();
    let json = export_configuration(&config, &inventory).unwrap();
    let decoded = import_configuration(&json, &inventory).unwrap();
    assert_eq!(decoded, config);
    assert_eq!(export_configuration(&decoded, &inventory).unwrap(), json);
    assert!(String::from_utf8(json)
        .unwrap()
        .contains("\"18446744073709551615\""));
}

#[test]
fn disabled_and_empty_configuration_round_trips() {
    let (config, inventory) = fixture();
    let mut value = serde_json::to_value(config).unwrap();
    value["creators"] = json!([]);
    value["templates"] = json!([]);
    value["aliases"] = json!([]);
    value["lists"] = json!([]);
    value["logging"] = Value::Null;
    value["settings"]["creation_enabled"] = json!(false);
    value["settings"]["command_role_id"] = Value::Null;
    value["settings"]["text_viewer_role_id"] = Value::Null;
    value["settings"]["command_roles"] = json!([]);
    let decoded = import_value(&value, &inventory).unwrap();
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

#[test]
fn malformed_json_wrong_types_missing_and_unknown_fields_are_rejected() {
    let (config, inventory) = fixture();
    for bytes in [b"".as_slice(), b"{", b"[]", b"null", b"{}", b"{} trailing"] {
        assert!(matches!(
            import_configuration(bytes, &inventory),
            Err(VoiceConfigError::Malformed { .. })
        ));
    }
    let original = serde_json::to_value(config).unwrap();
    let mut variants = Vec::new();
    let mut value = original.clone();
    value["guild_id"] = json!(101);
    variants.push(value);
    let mut value = original.clone();
    value.as_object_mut().unwrap().remove("version");
    variants.push(value);
    let mut value = original.clone();
    value["unrecognized"] = json!("untrusted-content");
    variants.push(value);
    let mut value = original.clone();
    value["creators"][0]["unrecognized"] = json!(true);
    variants.push(value);
    let mut value = original.clone();
    value["creators"][0]["position"] = json!("sideways");
    variants.push(value);
    let mut value = original.clone();
    value["creators"][0]["permission_source"]["unrecognized"] = json!(true);
    variants.push(value);
    let mut value = original;
    value["creators"][0]["default_limit"] = json!(1.5);
    variants.push(value);
    for value in variants {
        let error = import_value(&value, &inventory).unwrap_err();
        assert!(matches!(error, VoiceConfigError::Malformed { .. }));
        assert!(!error.to_string().contains("untrusted-content"));
    }
    let valid = export_configuration(&fixture().0, &inventory).unwrap();
    let duplicate = String::from_utf8(valid).unwrap().replacen(
        "\"version\": 1",
        "\"version\": 1, \"version\": 1",
        1,
    );
    assert!(matches!(
        import_configuration(duplicate.as_bytes(), &inventory),
        Err(VoiceConfigError::Malformed { .. })
    ));
}

#[test]
fn unsupported_versions_are_rejected_on_import_and_export() {
    let (mut config, inventory) = fixture();
    for version in [0, 2, u32::MAX] {
        config.version = version;
        assert!(
            matches!(export_configuration(&config, &inventory), Err(VoiceConfigError::UnsupportedVersion(v)) if v == version)
        );
        assert!(
            matches!(import_configuration(&serde_json::to_vec(&config).unwrap(), &inventory), Err(VoiceConfigError::UnsupportedVersion(v)) if v == version)
        );
    }
}

#[test]
fn guild_mismatch_is_rejected_even_when_channels_exist() {
    let (mut config, inventory) = fixture();
    config.guild_id = "999".into();
    assert!(matches!(
        validate_configuration(&config, &inventory),
        Err(VoiceConfigError::GuildMismatch)
    ));
}

#[test]
fn snowflakes_reject_noncanonical_zero_and_out_of_range_ids() {
    let (config, inventory) = fixture();
    for id in [
        "",
        "0",
        "01",
        "-1",
        "+1",
        "1.0",
        " 101",
        "١",
        "18446744073709551616",
    ] {
        let mut config = config.clone();
        config.creators[0].channel_id = id.to_owned();
        assert!(
            matches!(
                validate_configuration(&config, &inventory),
                Err(VoiceConfigError::Invalid { .. })
            ),
            "{id}"
        );
    }
}

#[test]
fn all_explicit_references_reject_unknown_and_cross_guild_ids() {
    let (config, inventory) = fixture();
    let original = serde_json::to_value(config).unwrap();
    let paths = [
        ("/creators/0/channel_id", "channel"),
        ("/creators/0/permission_source/channel_id", "channel"),
        ("/templates/0/channel_id", "channel"),
        ("/logging/channel_id", "channel"),
        ("/logging/mention_member_ids/0", "member"),
        ("/logging/mention_role_ids/0", "role"),
        ("/settings/text_viewer_role_id", "role"),
        ("/settings/command_role_id", "role"),
        ("/settings/command_roles/0/role_ids/0", "role"),
    ];
    for (path, kind) in paths {
        let mut value = original.clone();
        *value.pointer_mut(path).unwrap() = json!("999");
        assert!(
            matches!(import_value(&value, &inventory), Err(VoiceConfigError::UnknownReference { kind: k, .. }) if k == kind),
            "{path}"
        );
        let mut foreign = inventory.clone();
        foreign.channels.insert(
            "999".into(),
            ChannelReference {
                guild_id: "888".into(),
                kind: ChannelKind::Voice,
            },
        );
        foreign.roles.insert("999".into(), "888".into());
        foreign.members.insert("999".into(), "888".into());
        assert!(
            matches!(import_value(&value, &foreign), Err(VoiceConfigError::CrossGuildReference { kind: k, .. }) if k == kind),
            "{path}"
        );
    }
}

#[test]
fn channel_kinds_are_checked_at_every_typed_channel_reference() {
    let (config, inventory) = fixture();
    for id in ["101", "102", "103", "104"] {
        let mut inventory = inventory.clone();
        inventory.channels.get_mut(id).unwrap().kind = ChannelKind::Category;
        assert!(
            matches!(
                validate_configuration(&config, &inventory),
                Err(VoiceConfigError::Invalid { .. })
            ),
            "{id}"
        );
    }
}

#[test]
fn user_limit_bounds_are_inclusive_and_negative_input_is_malformed() {
    let (mut config, inventory) = fixture();
    for limit in [0, 1, 99] {
        config.creators[0].default_limit = limit;
        validate_configuration(&config, &inventory).unwrap();
    }
    for limit in [100, u16::MAX] {
        config.creators[0].default_limit = limit;
        assert!(matches!(
            validate_configuration(&config, &inventory),
            Err(VoiceConfigError::Invalid { .. })
        ));
    }
    let mut value = serde_json::to_value(config).unwrap();
    value["creators"][0]["default_limit"] = json!(-1);
    assert!(matches!(
        import_value(&value, &inventory),
        Err(VoiceConfigError::Malformed { .. })
    ));
}

#[test]
fn numbering_names_and_random_choices_have_valid_bounds() {
    let (config, inventory) = fixture();
    let original = serde_json::to_value(&config).unwrap();
    for (path, invalid) in [
        ("/creators/0/first_number", json!(0)),
        ("/settings/text_channel_name", json!("🎮".repeat(101))),
        ("/settings/no_game_label", json!(" ")),
        ("/settings/time_zone", json!("")),
        ("/aliases/0/game", json!("")),
        ("/aliases/0/alias", json!("")),
        ("/lists/0/name", json!("")),
        ("/lists/0/choices", json!([])),
        ("/lists/0/choices/0", json!(" ")),
    ] {
        let mut value = original.clone();
        *value.pointer_mut(path).unwrap() = invalid;
        assert!(
            matches!(
                import_value(&value, &inventory),
                Err(VoiceConfigError::Invalid { .. })
            ),
            "{path}"
        );
    }
    let mut config = config;
    config.settings.text_channel_name = "🎮".repeat(100);
    config.creators[0].first_number = 1;
    // Template source length is NOT the rendered Discord name length.
    config.creators[0].name_template = "{{FULL ?? full // empty}}".repeat(10);
    validate_configuration(&config, &inventory).unwrap();
}

#[test]
fn duplicates_and_creator_standalone_overlap_are_rejected() {
    let (config, inventory) = fixture();
    let original = serde_json::to_value(config).unwrap();
    for path in [
        "/creators",
        "/templates",
        "/aliases",
        "/lists",
        "/settings/command_roles",
        "/logging/mention_member_ids",
        "/logging/mention_role_ids",
        "/settings/command_roles/0/role_ids",
    ] {
        let mut value = original.clone();
        let entries = value.pointer_mut(path).unwrap().as_array_mut().unwrap();
        entries.push(entries[0].clone());
        assert!(
            matches!(
                import_value(&value, &inventory),
                Err(VoiceConfigError::Invalid { .. })
            ),
            "{path}"
        );
    }
    let mut value = original;
    value["templates"][0]["channel_id"] = json!("101");
    assert!(matches!(
        import_value(&value, &inventory),
        Err(VoiceConfigError::Invalid { .. })
    ));
}

#[test]
fn late_failure_never_returns_partial_config_or_mutates_inputs() {
    let (config, inventory) = fixture();
    let saved_inventory = inventory.clone();
    let saved_config = config.clone();
    let mut invalid = config.clone();
    invalid.settings.command_roles[0]
        .role_ids
        .push("999".into());
    let bytes = serde_json::to_vec(&invalid).unwrap();
    let saved_bytes = bytes.clone();
    assert!(import_configuration(&bytes, &inventory).is_err());
    assert!(export_configuration(&invalid, &inventory).is_err());
    assert_eq!(bytes, saved_bytes);
    assert_eq!(inventory, saved_inventory);
    assert_eq!(config, saved_config);
}

#[test]
fn permission_source_variants_and_logging_levels_round_trip_as_data_only() {
    let (config, inventory) = fixture();
    let original = serde_json::to_value(config).unwrap();
    for source in [
        json!({"kind": "creator"}),
        json!({"kind": "category"}),
        json!({"kind": "channel", "channel_id": "104"}),
    ] {
        for detail in ["errors", "lifecycle", "verbose"] {
            let mut value = original.clone();
            value["creators"][0]["permission_source"] = source.clone();
            value["creators"][0]["position"] = json!("below");
            value["logging"]["detail"] = json!(detail);
            let config = import_value(&value, &inventory).unwrap();
            assert_eq!(
                import_configuration(
                    &export_configuration(&config, &inventory).unwrap(),
                    &inventory
                )
                .unwrap(),
                config
            );
        }
    }
}
