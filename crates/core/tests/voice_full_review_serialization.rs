//! Supplemental regression cover for the complete import review artifact.
//!
//! The protected acceptance files pin the preview/hash/apply seams and stay
//! byte-for-byte untouched. This file pins the two seams they do not call:
//!
//! - [`serialize_full_review`](two_bot_core::voice_config_diff::serialize_full_review):
//!   the exact JSON schema (`schema`, `guild_id`, `hash`, `change_count`,
//!   `proposed`, `skipped_unknown_channels`), complete before/after values
//!   for every section and every [`GuildSettings`] field, JSON escaping of
//!   hostile/astral text, and the absent/unchanged negatives (`null`
//!   settings, empty section arrays, unchanged entries absent).
//! - [`render_review_preview`](two_bot_core::voice_config_diff::render_review_preview):
//!   the UTF-16 2000-character budget with room reserved for the trailing
//!   attachment hint, the summary-first layout, and the exact `+N more`
//!   hidden count near the boundary (ASCII and astral witnesses).
//!
//! Mutant rationale: a serializer that nulls or drops a real `settings`
//! change still passes the `null` absent-negative, but fails the
//! whole-envelope equality in the changed/settings-only tests, which pin the
//! complete before/after settings objects; and a preview that budgets the
//! full
//! [`PREVIEW_CHAR_LIMIT`](two_bot_core::voice_config_diff::PREVIEW_CHAR_LIMIT)
//! instead of reserving the hint overflows 2000 UTF-16 units (or loses the
//! trailer) in the boundary tests.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};
use two_bot_core::voice_config::{
    ChannelKind, ChannelReference, ChannelTemplates, CreatorConfiguration, GameAlias,
    GuildInventory, LogDetail, LoggingConfiguration, PermissionSource, RandomList, RoomPosition,
    VoiceConfiguration,
};
use two_bot_core::voice_config_diff::{
    diff_configuration, diff_content_hash, render_review_preview, serialize_full_review,
    FULL_REVIEW_BYTE_LIMIT, PREVIEW_CHAR_LIMIT,
};

const GUILD: &str = "18446744073709551615";
const REVIEW_HINT: &str = "Full review attached; read it before confirming.";
const REVIEW_SCHEMA: &str = "voice-config-review.v1";

/// Empty added/removed/changed triple shared by untouched sections.
fn empty_section() -> Value {
    section(Vec::new(), Vec::new(), Vec::new())
}

fn section(added: Vec<Value>, removed: Vec<Value>, changed: Vec<Value>) -> Value {
    let mut section = Map::new();
    section.insert("added".to_owned(), Value::Array(added));
    section.insert("removed".to_owned(), Value::Array(removed));
    section.insert("changed".to_owned(), Value::Array(changed));
    Value::Object(section)
}

/// Hand-authored changed entry: the key and field names are literals at each
/// call site; the complete before/after objects come from the fixture inputs
/// via `whole`, never from the diff or the serializer outputs.
fn changed_entry(key: &str, before: Value, after: Value, fields: Value) -> Value {
    let mut entry = Map::new();
    entry.insert("key".to_owned(), Value::String(key.to_owned()));
    entry.insert("before".to_owned(), before);
    entry.insert("after".to_owned(), after);
    entry.insert("fields".to_owned(), fields);
    Value::Object(entry)
}

/// Hand-authored review envelope: schema keys, guild, hash, count and skipped
/// facts are literals at each call site; the six section bodies travel as one
/// array argument in section order (creators, templates, aliases, lists,
/// logging, settings), assembled from fixture-input values by the caller.
fn envelope(hash: &str, change_count: u64, sections: [Value; 6], skipped: Value) -> Value {
    let [creators, templates, aliases, lists, logging, settings] = sections;
    let mut proposed = Map::new();
    proposed.insert("creators".to_owned(), creators);
    proposed.insert("templates".to_owned(), templates);
    proposed.insert("aliases".to_owned(), aliases);
    proposed.insert("lists".to_owned(), lists);
    proposed.insert("logging".to_owned(), logging);
    proposed.insert("settings".to_owned(), settings);
    let mut review = Map::new();
    review.insert("schema".to_owned(), Value::String(REVIEW_SCHEMA.to_owned()));
    review.insert("guild_id".to_owned(), Value::String(GUILD.to_owned()));
    review.insert("hash".to_owned(), Value::String(hash.to_owned()));
    review.insert("change_count".to_owned(), json!(change_count));
    review.insert("proposed".to_owned(), Value::Object(proposed));
    review.insert("skipped_unknown_channels".to_owned(), skipped);
    Value::Object(review)
}

/// Complete JSON value of a fixture input object (never a diff or serializer
/// output): pins whole added/removed/before/after objects exactly.
fn whole<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap()
}

/// Every section carries added/removed/changed entries with complete
/// before/after values, plus skipped-channel facts kept separate.
#[test]
fn full_review_schema_carries_every_section_with_complete_values() {
    let (current, inventory) = fixture_with_spares();
    let mut incoming = current.clone();

    // Creators: change 101, remove the spare 106, add 109.
    incoming.creators[0].name_template = "renamed @@owner@@ ##".to_owned();
    incoming.creators[0].default_limit = 4;
    incoming.creators.retain(|c| c.channel_id != "106");
    incoming.creators.push(CreatorConfiguration {
        channel_id: "109".to_owned(),
        name_template: "fresh".to_owned(),
        status_template: Some("LIVE 🎮".to_owned()),
        default_limit: 0,
        always_private: false,
        text_channels: false,
        position: RoomPosition::Below,
        first_number: 1,
        group_by_category: false,
        permission_source: PermissionSource::Creator {},
    });

    // Templates: change 103 (astral kept whole), remove 102, add 108,
    // and stage an unknown-channel template for the skipped section.
    incoming.templates.retain(|t| t.channel_id != "102");
    incoming.templates[0].name_template = "🎮 Stage Two".to_owned();
    incoming.templates.push(ChannelTemplates {
        channel_id: "108".to_owned(),
        name_template: "new stage".to_owned(),
        status_template: None,
    });
    incoming.templates.push(template("997"));

    // Aliases: change one, remove one, leave one untouched, add an
    // escaping/astral entry and a 100-char value that must not clip.
    let long = "v".repeat(100);
    incoming.aliases[0].alias = "changed 🎮".to_owned();
    incoming.aliases.retain(|a| a.game != "Old game");
    incoming.aliases.push(GameAlias {
        game: "weird \"game\"\n🎮".to_owned(),
        alias: "q \"quoted\" 🎮🚀".to_owned(),
    });
    incoming.aliases.push(alias("Brand new", &long));

    // Lists: change rooms, remove old-list, add new-list.
    incoming.lists.retain(|l| l.name != "old-list");
    incoming.lists[0].choices = vec!["den".to_owned(), "lair".to_owned()];
    incoming.lists.push(RandomList {
        name: "new-list".to_owned(),
        choices: vec!["x".to_owned()],
    });

    // Logging: scalar plus mention edits.
    let logging = incoming.logging.as_mut().unwrap();
    logging.detail = LogDetail::Errors;
    logging.mention_member_ids = vec![];

    // Settings: four fields change; the rest stay to prove full before/after.
    incoming.settings.unique_names = false;
    incoming.settings.no_game_label = "Lobby".to_owned();
    incoming.settings.time_zone = "UTC".to_owned();
    incoming.settings.command_role_id = None;

    let diff = diff_configuration(&current, &incoming, &inventory);
    assert_eq!(diff.skipped_unknown_channels, ["997"]);
    // 3 creators + 3 templates + 4 aliases + 3 lists + 1 logging + 1 settings.
    assert_eq!(diff.change_count(), 15);

    let hash = diff_content_hash(&current, &incoming);
    let bytes = serialize_full_review(&diff, &incoming.guild_id, &hash).unwrap();
    assert!(bytes.len() <= FULL_REVIEW_BYTE_LIMIT);
    let text = std::str::from_utf8(&bytes).unwrap();
    // Complete values, never preview clips.
    assert!(text.contains(&long), "long value clipped");
    assert!(!text.contains('…'), "review must not clip: {text:?}");
    assert!(text.contains("🎮 Stage Two"), "astral value lost");

    let review: Value = serde_json::from_slice(&bytes).unwrap();

    // Whole-envelope equality: every envelope key, every added/removed whole
    // object and every full before/after object plus field names are pinned
    // in one assertion. Complete objects come from the fixture inputs via
    // `whole` (never from the diff or serializer outputs); keys, field
    // names, the count and the skipped facts are hand-authored literals, and
    // section arrays follow the documented key order. An extra or missing
    // key, a clipped value, a dropped field, a misplaced entry or a skipped
    // channel leaking into proposed fails here.
    let expected = envelope(
        &hash,
        15,
        [
            section(
                vec![whole(find_creator(&incoming, "109"))],
                vec![whole(find_creator(&current, "106"))],
                vec![changed_entry(
                    "101",
                    whole(find_creator(&current, "101")),
                    whole(find_creator(&incoming, "101")),
                    json!(["name_template", "default_limit"]),
                )],
            ),
            section(
                vec![whole(find_template(&incoming, "108"))],
                vec![whole(find_template(&current, "102"))],
                vec![changed_entry(
                    "103",
                    whole(find_template(&current, "103")),
                    whole(find_template(&incoming, "103")),
                    json!(["name_template"]),
                )],
            ),
            section(
                vec![
                    whole(find_alias(&incoming, "Brand new")),
                    whole(find_alias(&incoming, "weird \"game\"\n🎮")),
                ],
                vec![whole(find_alias(&current, "Old game"))],
                vec![changed_entry(
                    "A game",
                    whole(find_alias(&current, "A game")),
                    whole(find_alias(&incoming, "A game")),
                    json!(["alias"]),
                )],
            ),
            section(
                vec![whole(find_list(&incoming, "new-list"))],
                vec![whole(find_list(&current, "old-list"))],
                vec![changed_entry(
                    "rooms",
                    whole(find_list(&current, "rooms")),
                    whole(find_list(&incoming, "rooms")),
                    json!(["choices"]),
                )],
            ),
            section(
                Vec::new(),
                Vec::new(),
                vec![changed_entry(
                    "logging",
                    whole(current.logging.as_ref().unwrap()),
                    whole(incoming.logging.as_ref().unwrap()),
                    json!(["detail", "mention_member_ids"]),
                )],
            ),
            changed_entry(
                "settings",
                whole(&current.settings),
                whole(&incoming.settings),
                json!([
                    "unique_names",
                    "no_game_label",
                    "time_zone",
                    "command_role_id"
                ]),
            ),
        ],
        json!(["997"]),
    );
    assert_eq!(review, expected);
}

/// Unchanged settings are an explicit `null` absent-negative: the key is
/// present, never an object, never omitted. A serializer that nulls or drops
/// a real `settings` change still passes here; the changed/settings-only
/// envelopes below are the ones that kill that mutant.
#[test]
fn full_review_unchanged_settings_serialize_as_null() {
    let (current, inventory) = fixture_with_spares();
    let mut incoming = current.clone();
    incoming.aliases.push(alias("new-game", "New Game"));

    let diff = diff_configuration(&current, &incoming, &inventory);
    assert!(diff.settings.is_none());
    assert_eq!(diff.change_count(), 1);

    let hash = diff_content_hash(&current, &incoming);
    let bytes = serialize_full_review(&diff, GUILD, &hash).unwrap();
    let review: Value = serde_json::from_slice(&bytes).unwrap();
    let expected = envelope(
        &hash,
        1,
        [
            empty_section(),
            empty_section(),
            section(
                vec![whole(find_alias(&incoming, "new-game"))],
                Vec::new(),
                Vec::new(),
            ),
            empty_section(),
            empty_section(),
            Value::Null,
        ],
        json!([]),
    );
    assert_eq!(review, expected);
}

/// A settings-only change touches nothing else: every section array is
/// empty while settings carries the full before/after pair.
#[test]
fn full_review_settings_only_diff_leaves_sections_empty() {
    let (current, inventory) = fixture_with_spares();
    let mut incoming = current.clone();
    incoming.settings.no_game_label = "Lobby".to_owned();

    let diff = diff_configuration(&current, &incoming, &inventory);
    assert!(diff.settings.is_some());
    assert_eq!(diff.change_count(), 1);
    assert!(diff.skipped_unknown_channels.is_empty());

    let hash = diff_content_hash(&current, &incoming);
    let bytes = serialize_full_review(&diff, GUILD, &hash).unwrap();
    let review: Value = serde_json::from_slice(&bytes).unwrap();
    let expected = envelope(
        &hash,
        1,
        [
            empty_section(),
            empty_section(),
            empty_section(),
            empty_section(),
            empty_section(),
            changed_entry(
                "settings",
                whole(&current.settings),
                whole(&incoming.settings),
                json!(["no_game_label"]),
            ),
        ],
        json!([]),
    );
    assert_eq!(review, expected);
}

/// Identical configs serialize an empty but well-formed review: zero count,
/// null settings, empty arrays, and the self-hash still bound.
#[test]
fn full_review_empty_diff_reports_absence_not_omission() {
    let (current, inventory) = fixture_with_spares();
    let diff = diff_configuration(&current, &current, &inventory);
    assert!(diff.is_empty());

    let hash = diff_content_hash(&current, &current);
    let bytes = serialize_full_review(&diff, GUILD, &hash).unwrap();
    let review: Value = serde_json::from_slice(&bytes).unwrap();
    let expected = envelope(
        &hash,
        0,
        [
            empty_section(),
            empty_section(),
            empty_section(),
            empty_section(),
            empty_section(),
            Value::Null,
        ],
        json!([]),
    );
    assert_eq!(review, expected);
}

/// The singleton logging section also covers its added and removed shapes.
#[test]
fn full_review_logging_added_and_removed_shapes() {
    let (current, inventory) = fixture_with_spares();

    // Removed: Some -> None.
    let mut removed = current.clone();
    removed.logging = None;
    let diff = diff_configuration(&current, &removed, &inventory);
    assert_eq!(diff.change_count(), 1);
    let hash = diff_content_hash(&current, &removed);
    let bytes = serialize_full_review(&diff, GUILD, &hash).unwrap();
    let review: Value = serde_json::from_slice(&bytes).unwrap();
    let expected = envelope(
        &hash,
        1,
        [
            empty_section(),
            empty_section(),
            empty_section(),
            empty_section(),
            section(
                Vec::new(),
                vec![whole(current.logging.as_ref().unwrap())],
                Vec::new(),
            ),
            Value::Null,
        ],
        json!([]),
    );
    assert_eq!(review, expected);

    // Added: None -> Some carries every logging field.
    let mut without = current.clone();
    without.logging = None;
    let mut restored = without.clone();
    restored.logging = Some(LoggingConfiguration {
        channel_id: "107".to_owned(),
        detail: LogDetail::Verbose,
        mention_member_ids: vec!["301".to_owned()],
        mention_role_ids: vec!["201".to_owned()],
    });
    let diff = diff_configuration(&without, &restored, &inventory);
    assert_eq!(diff.change_count(), 1);
    let hash = diff_content_hash(&without, &restored);
    let bytes = serialize_full_review(&diff, GUILD, &hash).unwrap();
    let review: Value = serde_json::from_slice(&bytes).unwrap();
    let expected = envelope(
        &hash,
        1,
        [
            empty_section(),
            empty_section(),
            empty_section(),
            empty_section(),
            section(
                vec![whole(restored.logging.as_ref().unwrap())],
                Vec::new(),
                Vec::new(),
            ),
            Value::Null,
        ],
        json!([]),
    );
    assert_eq!(review, expected);
}

/// Twenty distinct 80-character game names land near the 2000 UTF-16 budget:
/// the summary stays first, the attachment hint trails in full, the body
/// fits because the hint reserved its room, and the trailer counts exactly
/// the hidden lines. The short `"A"` alias is the witness that shown values
/// arrive complete. A budget mutant using the full limit shows one line too
/// many and overflows past the hint.
#[test]
fn review_preview_ascii_boundary_reserves_hint_and_counts_hidden() {
    let (current, inventory) = fixture_with_spares();
    let mut incoming = current.clone();
    for i in 0..20 {
        let game = format!("{i:02}-{}", "g".repeat(77));
        assert_eq!(game.chars().count(), 80);
        incoming.aliases.push(alias(&game, "A"));
    }

    let diff = diff_configuration(&current, &incoming, &inventory);
    assert_eq!(diff.change_count(), 20);

    let preview = render_review_preview(&diff, usize::MAX);
    let utf16 = preview.encode_utf16().count();
    assert!(utf16 <= 2000, "preview over budget: {utf16}");
    assert!(
        utf16 > PREVIEW_CHAR_LIMIT - 300,
        "fixture not near the boundary: {utf16}"
    );

    let summary = preview.lines().next().unwrap();
    assert!(
        summary.starts_with("Import preview: 20 changes"),
        "summary must lead: {summary:?}"
    );
    assert!(
        preview.ends_with(&format!("\n{REVIEW_HINT}")),
        "hint must trail in full"
    );

    let shown = preview
        .lines()
        .filter(|line| line.starts_with("+ alias"))
        .count();
    let hidden = 20 - shown;
    assert!(hidden >= 1, "boundary fixture hid nothing");
    let trailer = format!("+{hidden} more");
    assert!(
        preview.lines().any(|line| line == trailer),
        "exact hidden count {trailer:?} missing"
    );
    assert!(
        preview.lines().any(|line| line.ends_with("→ \"A\"")),
        "witness alias value missing"
    );
}

/// Astral-heavy keys cost two UTF-16 units per scalar: the same 20-line
/// shape must still fit the budget, keep the summary first and the hint
/// trailing, and report the exact hidden count under UTF-16 accounting.
#[test]
fn review_preview_astral_boundary_counts_utf16_units() {
    let (current, inventory) = fixture_with_spares();
    let mut incoming = current.clone();
    for i in 0..20 {
        let game = format!("{i:02}{}", "🚀".repeat(78));
        assert_eq!(game.chars().count(), 80);
        incoming.aliases.push(alias(&game, "A"));
    }

    let diff = diff_configuration(&current, &incoming, &inventory);
    assert_eq!(diff.change_count(), 20);

    let preview = render_review_preview(&diff, usize::MAX);
    let utf16 = preview.encode_utf16().count();
    assert!(utf16 <= 2000, "preview over budget: {utf16}");
    assert!(
        utf16 > PREVIEW_CHAR_LIMIT - 300,
        "fixture not near the boundary: {utf16}"
    );
    // Astral scalars prove the budget is measured in UTF-16, not chars.
    assert!(preview.chars().count() < utf16);

    let summary = preview.lines().next().unwrap();
    assert!(
        summary.starts_with("Import preview: 20 changes"),
        "summary must lead: {summary:?}"
    );
    assert!(
        preview.ends_with(&format!("\n{REVIEW_HINT}")),
        "hint must trail in full"
    );

    let shown = preview
        .lines()
        .filter(|line| line.starts_with("+ alias"))
        .count();
    let hidden = 20 - shown;
    assert!(hidden >= 1, "boundary fixture hid nothing");
    let trailer = format!("+{hidden} more");
    assert!(
        preview.lines().any(|line| line == trailer),
        "exact hidden count {trailer:?} missing"
    );
}

/// An empty diff still trails the hint after the summary line.
#[test]
fn review_preview_empty_diff_trails_hint() {
    let (current, inventory) = fixture_with_spares();
    let diff = diff_configuration(&current, &current, &inventory);
    assert_eq!(
        render_review_preview(&diff, usize::MAX),
        format!("No changes\n{REVIEW_HINT}")
    );
}

fn find_creator<'a>(config: &'a VoiceConfiguration, id: &str) -> &'a CreatorConfiguration {
    config
        .creators
        .iter()
        .find(|entry| entry.channel_id == id)
        .unwrap()
}

fn find_template<'a>(config: &'a VoiceConfiguration, id: &str) -> &'a ChannelTemplates {
    config
        .templates
        .iter()
        .find(|entry| entry.channel_id == id)
        .unwrap()
}

fn find_alias<'a>(config: &'a VoiceConfiguration, game: &str) -> &'a GameAlias {
    config
        .aliases
        .iter()
        .find(|entry| entry.game == game)
        .unwrap()
}

fn find_list<'a>(config: &'a VoiceConfiguration, name: &str) -> &'a RandomList {
    config
        .lists
        .iter()
        .find(|entry| entry.name == name)
        .unwrap()
}

fn template(channel_id: &str) -> ChannelTemplates {
    ChannelTemplates {
        channel_id: channel_id.to_owned(),
        name_template: "ghost".to_owned(),
        status_template: None,
    }
}

fn alias(game: &str, alias: &str) -> GameAlias {
    GameAlias {
        game: game.to_owned(),
        alias: alias.to_owned(),
    }
}

/// The codec fixture plus a spare voice channel (109) so one diff can add
/// and remove creators, and extra alias/list entries so one diff can add,
/// remove, change and leave entries untouched in the same section.
fn fixture_with_spares() -> (VoiceConfiguration, GuildInventory) {
    let mut config: VoiceConfiguration = serde_json::from_value(json!({
        "version": 1,
        "guild_id": GUILD,
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
        "aliases": [
            {"game": "A game", "alias": "遊び"},
            {"game": "Old game", "alias": "Old alias"},
            {"game": "Change me", "alias": "same 🧪"}
        ],
        "lists": [
            {"name": "rooms", "choices": ["den", "crew", "🎮"]},
            {"name": "old-list", "choices": ["a"]}
        ],
        "logging": {"channel_id": "104", "detail": "verbose", "mention_member_ids": ["301"], "mention_role_ids": ["201"]},
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
    config.creators.push(CreatorConfiguration {
        channel_id: "106".to_owned(),
        name_template: "old spare".to_owned(),
        status_template: None,
        default_limit: 0,
        always_private: false,
        text_channels: false,
        position: RoomPosition::Below,
        first_number: 1,
        group_by_category: false,
        permission_source: PermissionSource::Category {},
    });
    let mut inventory = inventory();
    inventory.channels.insert(
        "109".to_owned(),
        ChannelReference {
            guild_id: GUILD.to_owned(),
            kind: ChannelKind::Voice,
        },
    );
    (config, inventory)
}

fn inventory() -> GuildInventory {
    let channels = [
        ("101", ChannelKind::Voice),
        ("102", ChannelKind::Voice),
        ("103", ChannelKind::Stage),
        ("104", ChannelKind::Text),
        ("105", ChannelKind::Category),
        ("106", ChannelKind::Voice),
        ("107", ChannelKind::Text),
        ("108", ChannelKind::Stage),
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
    GuildInventory {
        guild_id: GUILD.to_owned(),
        channels,
        roles: BTreeMap::from([
            ("201".to_owned(), GUILD.to_owned()),
            (GUILD.to_owned(), GUILD.to_owned()),
        ]),
        members: BTreeMap::from([("301".to_owned(), GUILD.to_owned())]),
    }
}
