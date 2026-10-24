//! Protected acceptance: complete voice-config review diff (TOG-39444 test card).
//!
//! This file pins the core-level fixtures and properties the full-review
//! artifact must satisfy. It uses only stable `two_bot_core` seams
//! (`diff_configuration`, `render_preview`, `diff_content_hash`,
//! `skip_unknown_channels`, `apply_diff`) so it compiles and passes on the
//! baseline: these are controls plus a reference shape for the Builder.
//! Behavioral RED for the new attachment/cap wiring lives in the bot
//! acceptance file (`voice_config_full_diff_acceptance_tests.rs`), which
//! drives the real dispatch/store/response seams and fails on the baseline
//! (missing attachment, missing cap refusal).
//!
//! Builder contract (see the card for the full text):
//! - Keep the compact 20-line `render_preview` and Confirm/Cancel UX.
//! - Attach a bounded, versioned plain-text or JSON full review through the
//!   existing ephemeral attachment path. Include every changed path with
//!   complete old/new values plus skipped-channel facts; distinguish
//!   proposed applicable changes from skipped input.
//! - Build summary, attachment and pending candidate/hash from the SAME
//!   admitted snapshot. Never compute the attachment from a later read or
//!   label clipped values complete.
//! - Enforce a documented conservative serialized-byte cap
//!   (recommended: 32 KiB, `FULL_DIFF_MAX_BYTES`). Over cap: truthful
//!   reduce-file/retry refusal, no Confirm, pending state cleaned.
//! - Preserve admin/guild/uploader binding, validation, pruning, hash, TTL,
//!   cancel/replay and CAS apply. No writes before valid Confirm. Mentions
//!   stay disabled. Filename fixed safe/versioned.
//!
//! This file is immutable for the Builder (see the JSON manifest on the
//! card). Corrections go through Test Author follow-ups, not edits.

use std::collections::BTreeMap;

use serde_json::json;
use two_bot_core::voice_config::{
    export_configuration, validate_configuration, ChannelKind, ChannelReference, ChannelTemplates,
    CreatorConfiguration, GameAlias, GuildInventory, PermissionSource, RoomPosition,
    VoiceConfiguration,
};
use two_bot_core::voice_config_diff::{
    apply_diff, diff_configuration, diff_content_hash, render_preview, skip_unknown_channels,
    DIFF_HASH_CHARS, PREVIEW_CHAR_LIMIT,
};

const GUILD: &str = "18446744073709551615";
/// Compact preview line budget the bot enforces (`MAX_IMPORT_PREVIEW_LINES`).
const PREVIEW_LINES: usize = 20;

/// More than the compact preview shows: the summary must truncate with
/// `+N more` while the full-review artifact (bot layer) carries every path.
#[test]
fn full_review_fixture_exceeds_compact_preview_lines() {
    let (current, inventory) = fixture();
    let mut incoming = current.clone();
    for i in 0..25 {
        incoming
            .aliases
            .push(alias(&format!("game-{i:02}"), &format!("Alias {i:02}")));
    }
    let diff = diff_configuration(&current, &incoming, &inventory);
    assert_eq!(diff.change_count(), 25);
    assert!(diff.skipped_unknown_channels.is_empty());

    let preview = render_preview(&diff, PREVIEW_LINES);
    assert!(
        preview.starts_with("Import preview: 25 changes"),
        "{preview:?}"
    );
    assert!(preview.contains("+5 more"), "{preview:?}");
    assert!(preview.encode_utf16().count() <= PREVIEW_CHAR_LIMIT);

    // Exactly at the line budget: no trailer.
    let mut exact = current.clone();
    for i in 0..20 {
        exact
            .aliases
            .push(alias(&format!("exact-{i:02}"), &format!("Exact {i:02}")));
    }
    let diff = diff_configuration(&current, &exact, &inventory);
    let preview = render_preview(&diff, PREVIEW_LINES);
    assert!(!preview.contains("more"), "{preview:?}");

    // One over the budget: a single hidden line.
    exact.aliases.push(alias("exact-20", "Exact 20"));
    let diff = diff_configuration(&current, &exact, &inventory);
    let preview = render_preview(&diff, PREVIEW_LINES);
    assert!(preview.contains("+1 more"), "{preview:?}");
}

/// Long and astral values are clipped in the compact preview (`…`), so a
/// complete review cannot be derived from the preview alone: the artifact
/// must carry the full text.
#[test]
fn preview_clips_long_and_astral_values() {
    let (current, inventory) = fixture();
    let mut incoming = current.clone();
    // 100 chars: the longest valid alias value, over the preview's 80-char
    // per-value clip. Astral scalars exercise UTF-16 vs char counting.
    let long = format!("{}🎮🎮🎮🎮🎮", "z".repeat(95));
    assert_eq!(long.chars().count(), 100);
    incoming.aliases.push(alias("long-game", &long));

    let diff = diff_configuration(&current, &incoming, &inventory);
    let preview = render_preview(&diff, usize::MAX);
    assert!(preview.contains('…'), "{preview:?}");
    assert!(!preview.contains(&long), "{preview:?}");

    // Astral-heavy value: complete in the diff, clipped in the preview.
    // 85 chars, over the 80-char per-value clip; astral scalars exercise
    // multi-byte counting on both sides of the clip.
    let mut incoming = current.clone();
    let astral = format!("{}🎮🎮🎮🎮🎮🎮🎮🎮🎮🎮", "🚀".repeat(75));
    assert_eq!(astral.chars().count(), 85);
    incoming.aliases.push(alias("astral-game", &astral));
    let diff = diff_configuration(&current, &incoming, &inventory);
    let preview = render_preview(&diff, usize::MAX);
    assert!(preview.contains('…'), "{preview:?}");
    assert!(!preview.contains(&astral), "{preview:?}");
    // The diff itself retains the complete values for the artifact to use.
    assert!(diff.aliases.added.iter().any(|a| a.alias == astral));
}

/// Unknown-channels-only input reports skipped facts with zero applicable
/// changes: a Notice, never a Confirm.
#[test]
fn skipped_only_diff_reports_without_applicable_changes() {
    let (current, inventory) = fixture();
    let mut incoming = current.clone();
    incoming.templates.push(template("997"));
    incoming
        .creators
        .push(creator("999", PermissionSource::Creator {}));

    let diff = diff_configuration(&current, &incoming, &inventory);
    assert_eq!(diff.change_count(), 0);
    assert!(!diff.is_empty());
    assert_eq!(diff.skipped_unknown_channels, ["997", "999"]);
    let preview = render_preview(&diff, PREVIEW_LINES);
    assert!(preview.contains("0 changes"), "{preview:?}");
    assert!(preview.contains("997"), "{preview:?}");
    assert!(preview.contains("999"), "{preview:?}");
    assert_eq!(apply_diff(&current, &diff), current);

    let (remaining, skipped) = skip_unknown_channels(&incoming, &inventory);
    assert_eq!(skipped, ["997", "999"]);
    validate_configuration(&remaining, &inventory).unwrap();
}

/// Mixed input reports both proposed changes and skipped facts; the
/// artifact must distinguish the two sections.
#[test]
fn mixed_diff_reports_proposed_and_skipped() {
    let (current, inventory) = fixture();
    let mut incoming = current.clone();
    incoming.aliases.push(alias("new-game", "New Game"));
    incoming.templates.push(template("997"));

    let diff = diff_configuration(&current, &incoming, &inventory);
    assert_eq!(diff.change_count(), 1);
    assert_eq!(diff.skipped_unknown_channels, ["997"]);
    let preview = render_preview(&diff, PREVIEW_LINES);
    assert!(preview.contains("1 change"), "{preview:?}");
    assert!(preview.contains("new-game"), "{preview:?}");
    assert!(preview.contains("997"), "{preview:?}");

    let applied = apply_diff(&current, &diff);
    assert!(applied.aliases.iter().any(|a| a.game == "new-game"));
    assert!(applied.templates.iter().all(|t| t.channel_id != "997"));
}

/// Uploaded mention syntax survives in the preview text, so the response
/// layer must keep mentions disabled (and the artifact filename must stay
/// fixed safe/versioned, never derived from upload text).
#[test]
fn unsafe_mentions_survive_in_preview_text() {
    let (current, inventory) = fixture();
    let mut incoming = current.clone();
    incoming.templates[0].name_template = "@everyone <@123> <#456> <@&789>".to_owned();

    let diff = diff_configuration(&current, &incoming, &inventory);
    let preview = render_preview(&diff, usize::MAX);
    assert!(preview.contains("@everyone"), "{preview:?}");
    assert!(preview.contains("<@123>"), "{preview:?}");
    // Complete value retained for the artifact.
    assert_eq!(
        diff.templates.changed[0].after.name_template,
        "@everyone <@123> <#456> <@&789>"
    );
}

/// The content hash binds the exact (`current`, `candidate`) pair: any
/// concurrent change yields a different hash and must re-preview with
/// fresh evidence instead of applying.
#[test]
fn hash_binds_both_sides_of_the_snapshot() {
    let (current, _) = fixture();
    let mut incoming = current.clone();
    incoming.aliases.push(alias("new-game", "New Game"));
    let baseline = diff_content_hash(&current, &incoming);
    assert_eq!(baseline.len(), DIFF_HASH_CHARS);
    assert!(baseline.bytes().all(|b| b.is_ascii_hexdigit()));

    let mut changed_current = current.clone();
    changed_current
        .aliases
        .push(alias("other-game", "Other Game"));
    assert_ne!(diff_content_hash(&changed_current, &incoming), baseline);

    let mut changed_incoming = incoming.clone();
    changed_incoming
        .aliases
        .push(alias("third-game", "Third Game"));
    assert_ne!(diff_content_hash(&current, &changed_incoming), baseline);
    assert_eq!(
        diff_content_hash(&current, &current),
        diff_content_hash(&current, &current)
    );
}

/// Applying a diff equals the validated remaining candidate (sorted):
/// the artifact's proposed section must describe exactly this.
#[test]
fn apply_yields_validated_remaining_candidate() {
    let (current, inventory) = fixture();
    let mut incoming = current.clone();
    incoming.aliases.push(alias("new-game", "New Game"));
    incoming.templates.push(template("997"));

    let diff = diff_configuration(&current, &incoming, &inventory);
    let (remaining, skipped) = skip_unknown_channels(&incoming, &inventory);
    assert_eq!(skipped, ["997"]);
    validate_configuration(&remaining, &inventory).unwrap();
    let mut expected = remaining.clone();
    sort_configuration(&mut expected);
    let applied = apply_diff(&current, &diff);
    assert_eq!(applied, expected);
    export_configuration(&applied, &inventory).unwrap();
}

/// Reference shape for the full-review artifact: every changed path with
/// complete old/new values plus skipped facts, a version, and the content
/// hash binding it to the preview. The Builder may emit plain-text or
/// JSON, but the bot tests require these facts to be present and the
/// proposed/skipped sections to be distinguishable.
#[test]
fn reference_full_review_shape_carries_complete_values() {
    let (current, inventory) = fixture();
    let mut incoming = current.clone();
    let long = "v".repeat(100);
    incoming.aliases.push(alias("long-game", &long));
    incoming.aliases[0].alias = "changed".to_owned();
    incoming.templates.push(template("997"));

    let diff = diff_configuration(&current, &incoming, &inventory);
    let (remaining, skipped) = skip_unknown_channels(&incoming, &inventory);
    let hash = diff_content_hash(&current, &remaining);

    // Reference JSON the artifact must be at least as complete as.
    let reference = json!({
        "version": 1,
        "hash": hash,
        "change_count": diff.change_count(),
        "proposed": {
            "aliases_added": diff.aliases.added.iter().map(|a| json!({
                "game": a.game, "alias": a.alias,
            })).collect::<Vec<_>>(),
            "aliases_changed": diff.aliases.changed.iter().map(|c| json!({
                "game": c.key, "before": c.before.alias, "after": c.after.alias,
            })).collect::<Vec<_>>(),
        },
        "skipped_unknown_channels": skipped,
    });
    let text = serde_json::to_string_pretty(&reference).unwrap();
    // Complete values, not preview clips.
    assert!(text.contains(&long), "{text:?}");
    assert!(!text.contains('…'), "{text:?}");
    assert!(text.contains("long-game"), "{text:?}");
    assert!(text.contains("997"), "{text:?}");
    assert!(text.contains(&hash), "{text:?}");
    // Proposed vs skipped are separate keys.
    assert!(reference.get("proposed").is_some());
    assert!(reference.get("skipped_unknown_channels").is_some());
}

fn creator(channel_id: &str, permission_source: PermissionSource) -> CreatorConfiguration {
    CreatorConfiguration {
        channel_id: channel_id.to_owned(),
        name_template: "new".to_owned(),
        status_template: None,
        default_limit: 0,
        always_private: false,
        text_channels: false,
        position: RoomPosition::Below,
        first_number: 1,
        group_by_category: false,
        permission_source,
    }
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

fn sort_configuration(config: &mut VoiceConfiguration) {
    config.creators.sort_by_key(|c| c.channel_id.clone());
    config.templates.sort_by_key(|t| t.channel_id.clone());
    config.aliases.sort_by_key(|a| a.game.clone());
    config.lists.sort_by_key(|l| l.name.clone());
}

fn fixture() -> (VoiceConfiguration, GuildInventory) {
    let config = serde_json::from_value(json!({
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
            "text_viewer_role_id": GUILD,
            "command_role_id": "201",
            "command_roles": [{"command": "kick", "role_ids": ["201"]}]
        }
    }))
    .unwrap();
    (config, inventory())
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
