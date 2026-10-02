use std::collections::BTreeMap;

use proptest::prelude::*;
use serde_json::json;
use two_bot_core::voice_config::{
    export_configuration, validate_configuration, ChannelKind, ChannelReference, ChannelTemplates,
    CommandRoles, CreatorConfiguration, GameAlias, GuildInventory, GuildSettings, LogDetail,
    LoggingConfiguration, PermissionSource, RandomList, RoomPosition, VoiceConfiguration,
};
use two_bot_core::voice_config_diff::{
    apply_diff, diff_configuration, render_preview, skip_unknown_channels, PREVIEW_CHAR_LIMIT,
};

const GUILD: &str = "18446744073709551615";

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn property_voice_diff_of_identical_configs_is_empty(config in config_strategy()) {
        let inventory = inventory();
        prop_assert!(validate_configuration(&config, &inventory).is_ok());
        let diff = diff_configuration(&config, &config, &inventory);
        prop_assert!(diff.is_empty());
        prop_assert_eq!(diff.change_count(), 0);
        prop_assert_eq!(render_preview(&diff, usize::MAX), "No changes");
        prop_assert_eq!(apply_diff(&config, &diff), config);
    }

    #[test]
    fn property_voice_apply_yields_validated_incoming_minus_skipped(
        current in config_strategy(),
        mut incoming in config_strategy(),
        unknown in any::<[bool; 4]>(),
    ) {
        let inventory = inventory();
        let expected_skipped = inject_unknown_channels(&mut incoming, unknown);
        let diff = diff_configuration(&current, &incoming, &inventory);
        prop_assert_eq!(&diff.skipped_unknown_channels, &expected_skipped);

        let (remaining, skipped) = skip_unknown_channels(&incoming, &inventory);
        prop_assert_eq!(&skipped, &expected_skipped);
        prop_assert!(validate_configuration(&remaining, &inventory).is_ok());
        let mut expected = remaining;
        sort_configuration(&mut expected);
        let applied = apply_diff(&current, &diff);
        prop_assert_eq!(&applied, &expected);
        prop_assert!(export_configuration(&applied, &inventory).is_ok());

        // Entry order within a section is insignificant.
        let mut reversed = incoming.clone();
        reversed.creators.reverse();
        reversed.templates.reverse();
        reversed.aliases.reverse();
        reversed.lists.reverse();
        prop_assert_eq!(diff_configuration(&current, &reversed, &inventory), diff);
    }

    #[test]
    fn property_voice_preview_fits_discord_limit(
        current in config_strategy(),
        mut incoming in config_strategy(),
        games in proptest::collection::vec(".{0,300}", 0..40),
        max_lines in 0usize..64,
    ) {
        let inventory = inventory();
        incoming.aliases.extend(games.into_iter().map(|game| GameAlias {
            game,
            alias: "x".to_owned(),
        }));
        let diff = diff_configuration(&current, &incoming, &inventory);
        let preview = render_preview(&diff, max_lines);
        prop_assert!(preview.encode_utf16().count() <= PREVIEW_CHAR_LIMIT);
        prop_assert!(preview.chars().count() <= PREVIEW_CHAR_LIMIT);
        if diff.is_empty() {
            prop_assert_eq!(preview, "No changes");
        } else {
            let mut lines = preview.lines();
            prop_assert!(lines.next().unwrap().starts_with("Import preview: "));
            let rest: Vec<&str> = lines.collect();
            prop_assert!(rest.len() <= max_lines + 1);
            for line in rest {
                prop_assert!(
                    ["+ ", "- ", "~ ", "! "].iter().any(|sign| line.starts_with(sign))
                        || (line.starts_with('+') && line.ends_with(" more")),
                    "unexpected line {:?}",
                    line
                );
            }
        }
    }
}

#[test]
fn golden_full_preview_reports_every_section() {
    let (current, inventory) = fixture();
    let mut incoming = current.clone();
    incoming.creators[0].default_limit = 4;
    incoming
        .creators
        .push(creator("106", PermissionSource::Creator {}));
    incoming.templates.retain(|t| t.channel_id != "102");
    incoming.templates[0].name_template = "renamed".to_owned();
    incoming.aliases = vec![alias("A game", "changed"), alias("B game", "new")];
    incoming.lists = vec![];
    incoming.logging = None;
    incoming.settings.unique_names = false;

    let diff = diff_configuration(&current, &incoming, &inventory);
    assert_eq!(diff.creators.changed[0].fields, ["default_limit"]);
    assert_eq!(diff.templates.changed[0].fields, ["name_template"]);
    assert_eq!(diff.settings.as_ref().unwrap().fields, ["unique_names"]);
    assert!(diff.skipped_unknown_channels.is_empty());
    assert_eq!(
        render_preview(&diff, usize::MAX),
        "Import preview: 9 changes, 0 unknown channels skipped\n\
         ~ creator 101 (default_limit)\n\
         + creator 106\n\
         - template 102\n\
         ~ template 103 (name_template)\n\
         ~ alias \"A game\" (alias)\n\
         + alias \"B game\"\n\
         - list \"rooms\"\n\
         - logging\n\
         ~ settings (unique_names)"
    );

    let applied = apply_diff(&current, &diff);
    let mut expected = incoming;
    sort_configuration(&mut expected);
    assert_eq!(applied, expected);
    export_configuration(&applied, &inventory).unwrap();
}

#[test]
fn golden_unknown_channels_are_reported_and_skipped() {
    let (current, inventory) = fixture();
    let mut incoming = current.clone();
    incoming
        .creators
        .push(creator("999", PermissionSource::Creator {}));
    incoming.creators[0].permission_source = PermissionSource::Channel {
        channel_id: "998".to_owned(),
    };
    incoming.templates.push(template("997"));
    incoming.logging.as_mut().unwrap().channel_id = "996".to_owned();

    let diff = diff_configuration(&current, &incoming, &inventory);
    assert_eq!(diff.skipped_unknown_channels, ["996", "997", "998", "999"]);
    // Known IDs are never reported as skipped, even when their entry was.
    assert!(!diff.skipped_unknown_channels.contains(&"101".to_owned()));
    // Import replaces the configuration, so a skipped entry leaves the
    // current entry under the same key to be removed.
    assert_eq!(
        render_preview(&diff, usize::MAX),
        "Import preview: 2 changes, 4 unknown channels skipped\n\
         - creator 101\n\
         - logging\n\
         ! skipped unknown channel 996\n\
         ! skipped unknown channel 997\n\
         ! skipped unknown channel 998\n\
         ! skipped unknown channel 999"
    );

    let (remaining, skipped) = skip_unknown_channels(&incoming, &inventory);
    assert_eq!(skipped, diff.skipped_unknown_channels);
    validate_configuration(&remaining, &inventory).unwrap();
    assert_eq!(apply_diff(&current, &diff), remaining);
}

#[test]
fn skipped_only_diff_is_not_empty() {
    let (current, inventory) = fixture();
    let mut incoming = current.clone();
    incoming.templates.push(template("997"));
    let diff = diff_configuration(&current, &incoming, &inventory);
    assert_eq!(diff.change_count(), 0);
    assert!(!diff.is_empty());
    assert_eq!(
        render_preview(&diff, 10),
        "Import preview: 0 changes, 1 unknown channel skipped\n\
         ! skipped unknown channel 997"
    );
    assert_eq!(apply_diff(&current, &diff), current);
}

#[test]
fn empty_diff_renders_no_changes() {
    let (current, inventory) = fixture();
    let diff = diff_configuration(&current, &current, &inventory);
    assert!(diff.is_empty());
    assert_eq!(render_preview(&diff, 10), "No changes");
    assert_eq!(render_preview(&diff, 0), "No changes");
    assert_eq!(apply_diff(&current, &diff), current);
}

#[test]
fn diff_is_sorted_by_section_then_key() {
    let (current, inventory) = fixture();
    let mut incoming = current.clone();
    incoming.aliases.push(alias("z game", "z"));
    incoming.aliases.push(alias("a game", "a"));
    incoming
        .creators
        .insert(0, creator("106", PermissionSource::Category {}));
    incoming.creators[1].default_limit = 1;

    let diff = diff_configuration(&current, &incoming, &inventory);
    let games: Vec<&str> = diff.aliases.added.iter().map(|a| a.game.as_str()).collect();
    assert_eq!(games, ["a game", "z game"]);
    assert_eq!(
        render_preview(&diff, usize::MAX),
        "Import preview: 4 changes, 0 unknown channels skipped\n\
         ~ creator 101 (default_limit)\n\
         + creator 106\n\
         + alias \"a game\"\n\
         + alias \"z game\""
    );
}

#[test]
fn preview_truncates_with_more_and_stays_within_limit() {
    let (current, inventory) = fixture();
    let mut incoming = current.clone();
    for i in 0..40 {
        incoming
            .aliases
            .push(alias(&format!("game {i:02}"), &format!("alias {i:02}")));
    }
    let diff = diff_configuration(&current, &incoming, &inventory);
    assert_eq!(
        render_preview(&diff, 5),
        "Import preview: 40 changes, 0 unknown channels skipped\n\
         + alias \"game 00\"\n\
         + alias \"game 01\"\n\
         + alias \"game 02\"\n\
         + alias \"game 03\"\n\
         + alias \"game 04\"\n\
         +35 more"
    );
    assert_eq!(
        render_preview(&diff, 0),
        "Import preview: 40 changes, 0 unknown channels skipped\n+40 more"
    );
    assert!(!render_preview(&diff, 40).contains("more"));

    // Many wide entries: the limit, not max_lines, decides how many show.
    let mut wide = current.clone();
    for i in 0..3000 {
        wide.aliases
            .push(alias(&format!("{i:04} {}", "🎮".repeat(70)), "x"));
    }
    let diff = diff_configuration(&current, &wide, &inventory);
    let preview = render_preview(&diff, usize::MAX);
    assert!(preview.encode_utf16().count() <= PREVIEW_CHAR_LIMIT);
    let shown = preview
        .lines()
        .filter(|line| line.starts_with("+ "))
        .count();
    let hidden: usize = preview
        .lines()
        .last()
        .and_then(|line| line.strip_prefix('+')?.strip_suffix(" more"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(shown > 0);
    assert_eq!(shown + hidden, 3000);
}

#[test]
fn uploaded_text_cannot_break_the_layout() {
    let (current, inventory) = fixture();
    let mut incoming = current.clone();
    incoming.aliases = vec![alias("line\nbreak \"q\"", "x")];
    incoming.templates.push(template("not\na snowflake"));
    incoming.lists[0].name = "y".repeat(500);

    let preview = render_preview(
        &diff_configuration(&current, &incoming, &inventory),
        usize::MAX,
    );
    let lines: Vec<&str> = preview.lines().collect();
    let long_list = format!("+ list \"{}\"…", "y".repeat(80));
    assert_eq!(
        lines[1..],
        [
            "- alias \"A game\"",
            "+ alias \"line\\nbreak \\\"q\\\"\"",
            "- list \"rooms\"",
            long_list.as_str(),
            "! skipped unknown channel \"not\\na snowflake\"",
        ]
    );
}

fn inject_unknown_channels(config: &mut VoiceConfiguration, unknown: [bool; 4]) -> Vec<String> {
    let mut skipped = Vec::new();
    if unknown[0] && config.logging.is_some() {
        config.logging.as_mut().unwrap().channel_id = "996".to_owned();
        skipped.push("996".to_owned());
    }
    if unknown[1] {
        config.templates.push(template("997"));
        skipped.push("997".to_owned());
    }
    if unknown[2] && !config.creators.is_empty() {
        config.creators[0].permission_source = PermissionSource::Channel {
            channel_id: "998".to_owned(),
        };
        skipped.push("998".to_owned());
    }
    if unknown[3] {
        config
            .creators
            .push(creator("999", PermissionSource::Creator {}));
        skipped.push("999".to_owned());
    }
    skipped
}

fn config_strategy() -> impl Strategy<Value = VoiceConfiguration> {
    (
        proptest::option::of(creator_strategy("101")),
        proptest::option::of(creator_strategy("106")),
        proptest::option::of(template_strategy("102")),
        proptest::option::of(template_strategy("103")),
        proptest::option::of(template_strategy("108")),
        proptest::collection::btree_map("[ab]{1,2}", "[xy]", 0..4),
        proptest::collection::btree_map("[ab]{1,2}", proptest::collection::vec("[pq]", 1..3), 0..3),
        proptest::option::of(logging_strategy()),
        settings_strategy(),
    )
        .prop_map(
            |(first, second, voice, stage, other, aliases, lists, logging, settings)| {
                VoiceConfiguration {
                    version: 1,
                    guild_id: GUILD.to_owned(),
                    creators: [first, second].into_iter().flatten().collect(),
                    templates: [voice, stage, other].into_iter().flatten().collect(),
                    aliases: aliases
                        .into_iter()
                        .map(|(game, alias)| GameAlias { game, alias })
                        .collect(),
                    lists: lists
                        .into_iter()
                        .map(|(name, choices)| RandomList { name, choices })
                        .collect(),
                    logging,
                    settings,
                }
            },
        )
}

// Small value domains so both sides often share keys and values.
fn creator_strategy(channel_id: &'static str) -> impl Strategy<Value = CreatorConfiguration> {
    (
        prop::sample::select(vec!["##", "@@owner@@ ##"]),
        any::<[bool; 6]>(),
        prop::sample::select(vec![
            PermissionSource::Creator {},
            PermissionSource::Category {},
            PermissionSource::Channel {
                channel_id: "105".to_owned(),
            },
            PermissionSource::Channel {
                channel_id: "104".to_owned(),
            },
        ]),
    )
        .prop_map(
            move |(name, flags, permission_source)| CreatorConfiguration {
                channel_id: channel_id.to_owned(),
                name_template: name.to_owned(),
                status_template: flags[0].then(|| "@@game_name@@".to_owned()),
                default_limit: if flags[1] { 4 } else { 0 },
                always_private: flags[2],
                text_channels: flags[3],
                position: if flags[4] {
                    RoomPosition::Below
                } else {
                    RoomPosition::Above
                },
                first_number: 1,
                group_by_category: flags[5],
                permission_source,
            },
        )
}

fn template_strategy(channel_id: &'static str) -> impl Strategy<Value = ChannelTemplates> {
    (
        prop::sample::select(vec!["@@num@@", "Stage"]),
        any::<bool>(),
    )
        .prop_map(move |(name, status)| ChannelTemplates {
            channel_id: channel_id.to_owned(),
            name_template: name.to_owned(),
            status_template: status.then(|| "LIVE".to_owned()),
        })
}

fn logging_strategy() -> impl Strategy<Value = LoggingConfiguration> {
    (
        prop::sample::select(vec!["104", "107"]),
        prop::sample::select(vec![LogDetail::Errors, LogDetail::Verbose]),
        any::<bool>(),
        role_ids(),
    )
        .prop_map(
            |(channel_id, detail, mention, mention_role_ids)| LoggingConfiguration {
                channel_id: channel_id.to_owned(),
                detail,
                mention_member_ids: if mention {
                    vec!["301".to_owned()]
                } else {
                    vec![]
                },
                mention_role_ids,
            },
        )
}

fn settings_strategy() -> impl Strategy<Value = GuildSettings> {
    (
        any::<[bool; 4]>(),
        prop::sample::select(vec!["General", "Lobby"]),
        prop::sample::select(vec!["Europe/London", "UTC"]),
        proptest::option::of(prop::sample::select(vec!["201", GUILD])),
        proptest::collection::btree_map(
            prop::sample::select(vec!["kick", "lock"]),
            role_ids(),
            0..3,
        ),
    )
        .prop_map(
            |(flags, no_game_label, time_zone, command_role_id, command_roles)| GuildSettings {
                creation_enabled: flags[0],
                unique_names: flags[1],
                no_game_label: no_game_label.to_owned(),
                force_single_game: flags[2],
                count_members_without_activity: flags[3],
                time_zone: time_zone.to_owned(),
                text_channel_name: "voice-chat".to_owned(),
                text_viewer_role_id: None,
                command_role_id: command_role_id.map(str::to_owned),
                command_roles: command_roles
                    .into_iter()
                    .map(|(command, role_ids)| CommandRoles {
                        command: command.to_owned(),
                        role_ids,
                    })
                    .collect(),
            },
        )
}

fn role_ids() -> impl Strategy<Value = Vec<String>> {
    prop::sample::select(vec![vec![], vec!["201"], vec!["201", GUILD]])
        .prop_map(|ids| ids.into_iter().map(str::to_owned).collect())
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

// The codec's synthetic fixture. No Discord or DB.
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

// The codec fixture's inventory plus spare voice/text/stage channels.
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
