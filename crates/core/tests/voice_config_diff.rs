use std::collections::BTreeMap;

use proptest::prelude::*;
use serde_json::json;
use two_bot_core::voice_config::{
    export_configuration, validate_configuration, ChannelKind, ChannelReference, GuildInventory,
    VoiceConfiguration,
};
use two_bot_core::voice_config_diff::{
    apply_diff, diff_configuration, render_preview, PREVIEW_CHAR_LIMIT,
};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn property_voice_diff_self_is_empty(raw in proptest::collection::vec(any::<u8>(), 0..512)) {
        let (_, inventory) = fixture();
        if let Ok(config) = two_bot_core::voice_config::import_configuration(&raw, &inventory) {
            let diff = diff_configuration(&config, &config, &inventory);
            prop_assert!(diff.is_empty());
            prop_assert_eq!(diff.change_count(), 0);
            prop_assert_eq!(render_preview(&diff, usize::MAX), "No changes");
        }
    }

    #[test]
    fn property_voice_apply_matches_filtered_incoming(
        removals in proptest::collection::vec(any::<bool>(), 0..16),
    ) {
        let (mut current, inventory) = fixture();
        let mut incoming = current.clone();
        // Bounded mutation: toggle one flag, swap one label and drop entries.
        incoming.settings.creation_enabled = !incoming.settings.creation_enabled;
        incoming.settings.no_game_label = if incoming.settings.no_game_label == "General" {
            "Lobby".to_owned()
        } else {
            "General".to_owned()
        };
        for (i, drop) in removals.iter().enumerate() {
            match i % 4 {
                0 if *drop && !incoming.creators.is_empty() => { incoming.creators.pop(); }
                1 if *drop && !incoming.aliases.is_empty() => { incoming.aliases.pop(); }
                2 if *drop && !incoming.lists.is_empty() => { incoming.lists.pop(); }
                3 if *drop && !incoming.templates.is_empty() => { incoming.templates.pop(); }
                _ => {}
            }
        }
        incoming.creators.extend(current.creators.drain(..).take(1));
        prop_assert!(validate_configuration(&incoming, &inventory).is_ok());
        let diff = diff_configuration(&current, &incoming, &inventory);
        let applied = apply_diff(&current, &diff);
        let mut expected = incoming.clone();
        sort_configuration(&mut expected);
        let mut applied_sorted = applied.clone();
        sort_configuration(&mut applied_sorted);
        prop_assert_eq!(applied_sorted, expected);
        // The applied result revalidates: the diff never invents IDs.
        prop_assert!(validate_configuration(&applied, &inventory).is_ok());
        // Entry order is insignificant: shuffling changes nothing.
        let mut shuffled = incoming.clone();
        shuffled.creators.reverse();
        shuffled.aliases.reverse();
        prop_assert_eq!(diff_configuration(&current, &shuffled, &inventory), diff);
    }

    #[test]
    fn property_voice_preview_never_exceeds_limit(
        lines in 0usize..32,
        extra in proptest::collection::vec(0u8..8, 0..64),
    ) {
        let (current, inventory) = fixture();
        let mut incoming = current.clone();
        for (i, byte) in extra.iter().enumerate() {
            incoming.aliases.push(two_bot_core::voice_config::GameAlias {
                game: format!("game {i} {byte}"),
                alias: format!("alias {i}"),
            });
        }
        let diff = diff_configuration(&current, &incoming, &inventory);
        let preview = render_preview(&diff, lines);
        prop_assert!(preview.chars().count() <= PREVIEW_CHAR_LIMIT);
    }
}

// Synthetic IDs only, mirroring the codec fixture. No Discord or DB.
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
    (config, inventory())
}

fn extended_inventory() -> GuildInventory {
    let mut inventory = inventory();
    let guild_id = inventory.guild_id.clone();
    for (id, kind) in [
        ("106", ChannelKind::Voice),
        ("107", ChannelKind::Text),
        ("108", ChannelKind::Stage),
    ] {
        inventory.channels.insert(
            id.to_owned(),
            ChannelReference {
                guild_id: guild_id.clone(),
                kind,
            },
        );
    }
    inventory
}

fn inventory() -> GuildInventory {
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
    GuildInventory {
        guild_id: guild_id.clone(),
        channels,
        roles: BTreeMap::from([
            ("201".to_owned(), guild_id.clone()),
            (guild_id.clone(), guild_id.clone()),
        ]),
        members: BTreeMap::from([("301".to_owned(), guild_id)]),
    }
}

fn sort_configuration(config: &mut VoiceConfiguration) {
    config
        .creators
        .sort_by(|a, b| a.channel_id.cmp(&b.channel_id));
    config
        .templates
        .sort_by(|a, b| a.channel_id.cmp(&b.channel_id));
    config.aliases.sort_by(|a, b| a.game.cmp(&b.game));
    config.lists.sort_by(|a, b| a.name.cmp(&b.name));
}

#[test]
fn golden_full_preview_reports_every_section() {
    let (current, _) = fixture();
    let inventory = extended_inventory();
    let mut incoming = current.clone();
    incoming.creators[0].default_limit = 4;
    incoming.creators.push(
        serde_json::from_value(json!({
            "channel_id": "106",
            "name_template": "new",
            "status_template": null,
            "default_limit": 0,
            "always_private": false,
            "text_channels": false,
            "position": "below",
            "first_number": 1,
            "group_by_category": false,
            "permission_source": {"kind": "creator"}
        }))
        .unwrap(),
    );
    incoming.templates.retain(|t| t.channel_id != "102");
    incoming.templates[0].name_template = "renamed".to_owned();
    incoming.aliases = vec![
        serde_json::from_value(json!({"game": "A game", "alias": "changed"})).unwrap(),
        serde_json::from_value(json!({"game": "B game", "alias": "new"})).unwrap(),
    ];
    incoming.lists = vec![];
    incoming.logging = None;
    incoming.settings.unique_names = false;

    let diff = diff_configuration(&current, &incoming, &inventory);
    assert_eq!(diff.creators_added.len(), 1);
    assert_eq!(diff.creators_changed.len(), 1);
    assert_eq!(diff.creators_changed[0].fields, ["default_limit"]);
    assert_eq!(diff.templates_removed.len(), 1);
    assert_eq!(diff.templates_changed.len(), 1);
    assert_eq!(diff.aliases_added.len(), 1);
    assert_eq!(diff.aliases_changed.len(), 1);
    assert_eq!(diff.lists_removed.len(), 1);
    assert!(diff.logging_removed.is_some());
    assert!(diff.settings_changed.is_some());
    assert!(diff.skipped_unknown_channels.is_empty());

    let preview = render_preview(&diff, usize::MAX);
    for line in [
        "+ creator 106",
        "~ creator 101 (default_limit)",
        "~ template 103 (name_template)",
        "- template 102",
        "+ alias B game",
        "~ alias \"A game\"",
        "- list rooms",
        "- logging",
        "~ settings (unique_names)",
    ] {
        assert!(preview.contains(line), "missing {line} in:\n{preview}");
    }
    assert!(preview.chars().count() <= PREVIEW_CHAR_LIMIT);

    // Applying reaches the incoming exactly (sorted deterministically).
    let applied = apply_diff(&current, &diff);
    let mut expected = incoming.clone();
    sort_configuration(&mut expected);
    let mut applied_sorted = applied.clone();
    sort_configuration(&mut applied_sorted);
    assert_eq!(applied_sorted, expected);
    validate_configuration(&applied, &inventory).unwrap();
    export_configuration(&applied, &inventory).unwrap();
}

#[test]
fn unknown_channels_are_reported_and_skipped() {
    let (current, inventory) = fixture();
    let mut incoming = current.clone();
    incoming.creators.push(
        serde_json::from_value(json!({
            "channel_id": "999",
            "name_template": "ghost",
            "status_template": null,
            "default_limit": 0,
            "always_private": false,
            "text_channels": false,
            "position": "above",
            "first_number": 1,
            "group_by_category": false,
            "permission_source": {"kind": "creator"}
        }))
        .unwrap(),
    );
    incoming.creators[0].permission_source =
        serde_json::from_value(json!({"kind": "channel", "channel_id": "998"})).unwrap();
    incoming.templates[0].channel_id = "997".to_owned();
    incoming.logging.as_mut().unwrap().channel_id = "996".to_owned();

    let diff = diff_configuration(&current, &incoming, &inventory);
    assert_eq!(
        diff.skipped_unknown_channels,
        ["101", "996", "997", "998", "999"]
    );
    assert!(diff.creators_added.is_empty());
    // The known creator 101 keeps its usable shape: the skipped
    // permission-source edit is not applied as a change.
    assert!(diff.creators_changed.is_empty());
    assert!(diff.templates_added.is_empty());
    assert!(diff.templates_changed.is_empty());
    assert!(diff.logging_changed.is_none());
    assert!(diff.logging_removed.is_none());

    let preview = render_preview(&diff, usize::MAX);
    for id in ["101", "996", "997", "998", "999"] {
        assert!(
            preview.contains(&format!("! skipped channel {id}")),
            "missing skip {id} in:\n{preview}"
        );
    }
    // Applying the diff leaves the current config untouched.
    assert_eq!(apply_diff(&current, &diff), current);
}

#[test]
fn diff_ordering_is_deterministic_by_section_then_id() {
    let (current, _) = fixture();
    let inventory = extended_inventory();
    let mut incoming = current.clone();
    // Insert in reverse order; the diff must still sort ascending.
    incoming
        .aliases
        .push(serde_json::from_value(json!({"game": "z game", "alias": "z"})).unwrap());
    incoming
        .aliases
        .push(serde_json::from_value(json!({"game": "a game", "alias": "a"})).unwrap());
    // A creator change plus an alias change checks section order too.
    incoming.creators[0].default_limit = 1;
    let diff = diff_configuration(&current, &incoming, &inventory);
    assert_eq!(
        diff.aliases_added
            .iter()
            .map(|a| a.game.as_str())
            .collect::<Vec<_>>(),
        ["a game", "z game"]
    );
    let preview = render_preview(&diff, usize::MAX);
    let creator_pos = preview.find("creator").unwrap();
    let alias_pos = preview.find("alias").unwrap();
    assert!(creator_pos < alias_pos, "sections out of order:\n{preview}");
}

#[test]
fn empty_diff_renders_no_changes() {
    let (current, inventory) = fixture();
    let diff = diff_configuration(&current, &current, &inventory);
    assert!(diff.is_empty());
    assert_eq!(render_preview(&diff, 10), "No changes");
    assert_eq!(apply_diff(&current, &diff), current);
}

#[test]
fn preview_truncates_with_more_and_stays_within_limit() {
    let (current, inventory) = fixture();
    let mut incoming = current.clone();
    for i in 0..40 {
        incoming
            .aliases
            .push(two_bot_core::voice_config::GameAlias {
                game: format!("game {i:02}"),
                alias: format!("alias {i:02}"),
            });
    }
    let diff = diff_configuration(&current, &incoming, &inventory);
    let preview = render_preview(&diff, 5);
    assert!(
        preview.contains("+36 more"),
        "missing trailer in:\n{preview}"
    );
    assert_eq!(preview.lines().count(), 1 + 4 + 1);
    assert!(preview.chars().count() <= PREVIEW_CHAR_LIMIT);

    // max_lines 0 still reports the count without entry lines.
    let hidden = render_preview(&diff, 0);
    assert!(hidden.contains("+40 more"));
    assert_eq!(hidden.lines().count(), 2);

    // A pathological single entry still fits the Discord limit.
    let mut wide = current.clone();
    wide.lists[0].choices = vec!["x".repeat(5000)];
    let wide_diff = diff_configuration(&current, &wide, &inventory);
    assert!(render_preview(&wide_diff, usize::MAX).chars().count() <= PREVIEW_CHAR_LIMIT);
}
