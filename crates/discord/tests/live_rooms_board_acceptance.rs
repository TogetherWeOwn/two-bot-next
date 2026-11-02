//! Spec-first pure snapshot -> board acceptance. No gateway, DB, clock or HTTP.
//! Protect this file and support/live_rooms_board_contract.rs byte-for-byte.
//! Only support/live_rooms_board_adapter.rs is an editable production bridge.
//! Baseline assertion failures mean the board is missing, not that a production
//! board was exercised. GREEN requires the bridge to call the real pure core.

#[path = "support/live_rooms_board_adapter.rs"]
mod adapter;
#[path = "support/live_rooms_board_contract.rs"]
pub mod contract;

use contract::{Entry, Occupant, Reply, Room, Snapshot, State};
use serde_json::json;
use twilight_model::{
    channel::permission_overwrite::{PermissionOverwrite, PermissionOverwriteType},
    guild::{Permissions, Role},
    id::Id,
};
use two_bot_core::{
    voice_alias::AliasTable,
    voice_conditions::ConditionFacts,
    voice_naming::{GameOptions, RoomContext},
    voice_presence::{apply_room_presence, MemberPresence, OccupantPresence},
};
use two_bot_discord::voice_rooms::effective_permissions;

const GUILD: u64 = 100;
const GUILD_OWNER: u64 = 101;
const VIEWER: u64 = 102;
const ROOM_OWNER: u64 = 103;
const GRANTED: u64 = 104;
const KICKED: u64 = 105;
const ADMIN_ROLE: u64 = 106;
const ACCESS_ROLE: u64 = 107;
const NOW: i64 = 1_793_001_600;

fn role(id: u64, permissions: Permissions) -> Role {
    serde_json::from_value(json!({
        "id": id.to_string(), "name": "fixture role", "color": 0,
        "hoist": false, "managed": false, "mentionable": false, "position": 0,
        "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null },
        "permissions": permissions.bits().to_string(), "flags": 0
    }))
    .expect("synthetic role is valid")
}

fn overwrite(
    id: u64,
    kind: PermissionOverwriteType,
    allow: Permissions,
    deny: Permissions,
) -> PermissionOverwrite {
    PermissionOverwrite {
        id: Id::new(id),
        kind,
        allow,
        deny,
    }
}

fn human(id: u64, game: Option<&str>) -> Occupant {
    Occupant {
        member_id: id,
        is_bot: false,
        presence: game.map(|game| MemberPresence {
            game: Some(game.to_owned()),
            ..MemberPresence::default()
        }),
    }
}

fn room(id: u64, games: &[Option<&str>]) -> Room {
    Room {
        channel_id: id,
        name: "Room name is not a game".to_owned(),
        tracked: true,
        owner_id: ROOM_OWNER,
        user_limit: Some(0),
        overwrites: Vec::new(),
        occupants: games
            .iter()
            .enumerate()
            .map(|(i, game)| human(ROOM_OWNER + u64::try_from(i).expect("small fixture"), *game))
            .collect(),
    }
}

fn snapshot(rooms: Vec<Room>) -> Snapshot {
    Snapshot {
        ready: true,
        generation: 37,
        as_of_unix: NOW,
        guild_id: GUILD,
        guild_owner_id: GUILD_OWNER,
        viewer_id: VIEWER,
        viewer_roles: Some(Vec::new()),
        roles: vec![role(
            GUILD,
            Permissions::VIEW_CHANNEL | Permissions::CONNECT,
        )],
        presence_enabled: true,
        aliases: AliasTable::default(),
        game_options: GameOptions::default(),
        rooms,
    }
}

fn ready(facts: &Snapshot) -> Reply {
    let reply = adapter::render(facts);
    assert_eq!(reply.state, State::Ready, "missing ready board: {reply:?}");
    assert_eq!(
        reply.generation, facts.generation,
        "mixed snapshot generation"
    );
    assert!(
        reply.content.encode_utf16().count() <= 2000,
        "Discord content overflow"
    );
    assert!(!reply.content.contains('\0'), "NUL in rendered content");
    let mut cursor = 0;
    for entry in &reply.entries {
        let link = format!("<#{}>", entry.channel_id);
        let offset = reply.content[cursor..]
            .find(&link)
            .expect("entry link in content");
        cursor += offset + link.len();
    }
    reply
}

fn ids(reply: &Reply) -> Vec<u64> {
    reply.entries.iter().map(|entry| entry.channel_id).collect()
}

fn no_game(reply: &Reply) {
    assert!(reply
        .entries
        .iter()
        .all(|entry| entry.game.is_none() && entry.members_playing.is_none()));
    for forbidden in ["General", "playing", "game unavailable", "presence off"] {
        assert!(
            !reply.content.contains(forbidden),
            "invented game text: {forbidden}"
        );
    }
}

fn no_disclosure(reply: &Reply, id: u64, name: &str, game: &str) {
    assert!(!ids(reply).contains(&id));
    for forbidden in [
        id.to_string(),
        name.to_owned(),
        game.to_owned(),
        "private room".to_owned(),
    ] {
        assert!(
            !reply.content.contains(&forbidden),
            "hidden-room disclosure: {forbidden}"
        );
    }
}

fn private_room(id: u64) -> Room {
    let mut room = room(id, &[Some("Secret Game"), Some("Secret Game")]);
    room.name = "Secret room marker".to_owned();
    room.overwrites = vec![
        overwrite(
            GUILD,
            PermissionOverwriteType::Role,
            Permissions::empty(),
            Permissions::CONNECT,
        ),
        overwrite(
            GRANTED,
            PermissionOverwriteType::Member,
            Permissions::CONNECT,
            Permissions::empty(),
        ),
    ];
    room
}

#[test]
fn public_room_uses_folded_alias_majority_and_actual_playing_count() {
    // Catches counting raw spellings separately or counting idle occupants as playing.
    let mut facts = snapshot(vec![room(
        200,
        &[Some("  APEX LEGENDS  "), Some("Apex"), None],
    )]);
    facts.aliases = AliasTable::from_entries([("apex legends", "Apex")]).expect("valid alias");
    let reply = ready(&facts);
    assert_eq!(
        reply.entries,
        vec![Entry {
            channel_id: 200,
            humans: 3,
            seats: "Open".to_owned(),
            game: Some("Apex".to_owned()),
            members_playing: Some(2),
        }]
    );
    for text in ["<#200>", "3 in voice", "Open", "Apex", "2 playing"] {
        assert!(reply.content.contains(text), "missing {text}");
    }
    assert!(reply.content.starts_with("Live rooms as of <t:1793001600"));
    assert!(!reply.content.contains("APEX LEGENDS"));
}

#[test]
fn private_room_is_absent_for_non_granted_viewer_without_count_or_overflow_leak() {
    // Catches View-only filters, private placeholders, and pre-filter overflow counts.
    let facts = snapshot(vec![private_room(201), room(200, &[Some("Apex")])]);
    let reply = ready(&facts);
    assert_eq!(ids(&reply), vec![200]);
    assert_eq!(reply.omitted, 0);
    no_disclosure(&reply, 201, "Secret room marker", "Secret Game");
    assert!(!reply.content.contains("more rooms"));
}

#[test]
fn member_access_grant_overrides_everyone_private_deny() {
    let mut facts = snapshot(vec![private_room(201)]);
    facts.viewer_id = GRANTED;
    let reply = ready(&facts);
    assert_eq!(ids(&reply), vec![201]);
    assert_eq!(reply.entries[0].humans, 2);
    assert_eq!(reply.entries[0].game.as_deref(), Some("Secret Game"));
}

#[test]
fn vote_kick_member_deny_hides_otherwise_public_room_only_for_denied_viewer() {
    let mut target = room(202, &[Some("Apex")]);
    target.overwrites.push(overwrite(
        KICKED,
        PermissionOverwriteType::Member,
        Permissions::empty(),
        Permissions::CONNECT,
    ));
    let mut facts = snapshot(vec![target]);
    assert_eq!(ids(&ready(&facts)), vec![202]);
    facts.viewer_id = KICKED;
    let denied = ready(&facts);
    assert!(denied.entries.is_empty());
    no_disclosure(&denied, 202, "Room name is not a game", "Apex");
}

#[test]
fn role_access_allow_wins_combined_role_deny_but_member_deny_wins_last() {
    let mut facts = snapshot(vec![private_room(201)]);
    facts.roles.extend([
        role(ACCESS_ROLE, Permissions::empty()),
        role(108, Permissions::empty()),
    ]);
    facts.viewer_roles = Some(vec![Id::new(ACCESS_ROLE), Id::new(108)]);
    facts.rooms[0].overwrites.extend([
        overwrite(
            108,
            PermissionOverwriteType::Role,
            Permissions::empty(),
            Permissions::CONNECT,
        ),
        overwrite(
            ACCESS_ROLE,
            PermissionOverwriteType::Role,
            Permissions::CONNECT,
            Permissions::empty(),
        ),
    ]);
    assert_eq!(ids(&ready(&facts)), vec![201]);
    facts.rooms[0].overwrites.push(overwrite(
        VIEWER,
        PermissionOverwriteType::Member,
        Permissions::empty(),
        Permissions::CONNECT,
    ));
    assert!(ready(&facts).entries.is_empty());
}

#[test]
fn view_and_connect_are_both_required_with_no_extra_manage_permission() {
    let mut facts = snapshot(vec![room(200, &[None])]);
    assert_eq!(ids(&ready(&facts)), vec![200]);
    for permissions in [
        Permissions::VIEW_CHANNEL,
        Permissions::CONNECT,
        Permissions::empty(),
    ] {
        facts.roles = vec![role(GUILD, permissions)];
        let reply = ready(&facts);
        assert!(reply.entries.is_empty(), "leaked for {permissions:?}");
        assert_eq!(reply.omitted, 0);
    }
}

#[test]
fn unknown_assigned_role_fails_closed_instead_of_using_everyone_allow() {
    let mut facts = snapshot(vec![private_room(201), room(200, &[Some("Apex")])]);
    facts.viewer_roles = Some(vec![Id::new(999)]);
    let reply = adapter::render(&facts);
    assert_ne!(
        reply.state,
        State::MissingFeature,
        "board core not installed"
    );
    assert!(reply.entries.is_empty());
    assert_eq!(reply.omitted, 0);
    no_disclosure(&reply, 200, "Room name is not a game", "Apex");
    no_disclosure(&reply, 201, "Secret room marker", "Secret Game");
}

#[test]
fn missing_everyone_role_fails_closed() {
    let mut facts = snapshot(vec![room(200, &[Some("Apex")])]);
    facts.roles.clear();
    let reply = adapter::render(&facts);
    assert_ne!(
        reply.state,
        State::MissingFeature,
        "board core not installed"
    );
    assert!(reply.entries.is_empty());
    assert_eq!(reply.omitted, 0);
    no_disclosure(&reply, 200, "Room name is not a game", "Apex");
}

#[test]
fn administrator_and_guild_owner_bypass_denies_but_not_empty_or_untracked_filter() {
    let mut facts = snapshot(vec![private_room(201), room(202, &[None]), room(203, &[])]);
    facts.rooms[1].tracked = false;
    facts.rooms[0].overwrites.push(overwrite(
        VIEWER,
        PermissionOverwriteType::Member,
        Permissions::empty(),
        Permissions::VIEW_CHANNEL | Permissions::CONNECT,
    ));
    facts
        .roles
        .push(role(ADMIN_ROLE, Permissions::ADMINISTRATOR));
    facts.viewer_roles = Some(vec![Id::new(ADMIN_ROLE)]);
    assert_eq!(ids(&ready(&facts)), vec![201]);
    facts.viewer_id = GUILD_OWNER;
    facts.viewer_roles = Some(Vec::new());
    assert_eq!(ids(&ready(&facts)), vec![201]);
}

#[test]
fn presence_disabled_ignores_even_cached_games_and_custom_naming_fallback() {
    let mut facts = snapshot(vec![room(200, &[Some("Apex"), Some("Apex")])]);
    facts.presence_enabled = false;
    facts.game_options.no_game_label = "Invented fallback".to_owned();
    let reply = ready(&facts);
    assert_eq!(ids(&reply), vec![200]);
    no_game(&reply);
    for text in ["Apex", "Invented fallback", "Room name is not a game"] {
        assert!(!reply.content.contains(text));
    }
}

#[test]
fn absent_presence_has_no_game_text_or_naming_fallback() {
    let mut facts = snapshot(vec![room(200, &[None, None])]);
    // Empty stored presence is not a game fact either.
    facts.rooms[0].occupants[0].presence = Some(MemberPresence::default());
    let reply = ready(&facts);
    assert_eq!(reply.entries[0].humans, 2);
    no_game(&reply);
}

#[test]
fn real_game_named_general_is_not_confused_with_naming_fallback() {
    let reply = ready(&snapshot(vec![room(200, &[Some("General")])]));
    assert_eq!(reply.entries[0].game.as_deref(), Some("General"));
    assert_eq!(reply.entries[0].members_playing, Some(1));
    assert!(reply.content.contains("General"));
}

#[test]
fn majority_ties_owner_preference_and_idle_settings_match_room_name_rules() {
    // Literal expected labels, not values computed by the resolver under test.
    let cases = [
        (
            vec![Some("Valheim"), Some("Apex")],
            false,
            false,
            Some("Apex & Valheim"),
        ),
        (
            vec![Some("Valheim"), Some("Apex"), Some("Apex")],
            true,
            false,
            Some("Valheim"),
        ),
        (
            vec![Some("Valheim"), Some("Apex"), None],
            false,
            true,
            Some("Apex"),
        ),
        (
            vec![Some("Valheim"), Some("Apex"), Some("Doom")],
            false,
            false,
            None,
        ),
    ];
    for (games, force_single, count_idle, expected) in cases {
        let mut facts = snapshot(vec![room(200, &games)]);
        facts.game_options.force_single = force_single;
        facts.game_options.count_idle_toward_majority = count_idle;
        let reply = ready(&facts);
        assert_eq!(reply.entries[0].game.as_deref(), expected);
        if expected.is_some() {
            assert_eq!(
                reply.entries[0].members_playing,
                Some(games.iter().flatten().count())
            );
        } else {
            no_game(&reply);
        }
    }
}

#[test]
fn zero_or_absent_limit_is_open_and_finite_limits_have_exact_seat_math() {
    let cases = [
        (None, 2, "Open"),
        (Some(0), 2, "Open"),
        (Some(5), 2, "3 seats open"),
        (Some(4), 4, "Full"),
        (Some(2), 4, "Full"),
    ];
    for (limit, count, expected) in cases {
        let mut facts = snapshot(vec![room(200, &vec![None; count])]);
        facts.rooms[0].user_limit = limit;
        let reply = ready(&facts);
        assert_eq!(reply.entries[0].seats, expected);
        assert!(reply.content.contains(expected));
        if expected == "Full" {
            assert!(!reply.content.contains("seats open"));
        }
    }
}

#[test]
fn sort_is_open_then_human_count_descending_then_channel_id_ascending() {
    let mut facts = snapshot(vec![
        room(205, &[None, None, None, None]),
        room(204, &[None]),
        room(203, &[None, None]),
        room(202, &[None, None]),
        room(201, &[None, None]),
    ]);
    facts.rooms[0].user_limit = Some(4);
    facts.rooms[4].user_limit = Some(1);
    let reply = ready(&facts);
    assert_eq!(ids(&reply), vec![202, 203, 204, 205, 201]);
    assert_eq!(reply.entries[3].seats, "Full");
    assert_eq!(reply.entries[4].seats, "Full");
    facts.rooms.reverse();
    assert_eq!(ids(&ready(&facts)), vec![202, 203, 204, 205, 201]);
}

#[test]
fn bots_do_not_supply_occupants_seats_or_majority_games() {
    let mut mixed = room(
        200,
        &[
            Some("Apex"),
            Some("Secret Bot Game"),
            Some("Secret Bot Game"),
        ],
    );
    mixed.user_limit = Some(3);
    mixed.occupants[1].is_bot = true;
    mixed.occupants[2].is_bot = true;
    let mut bots = room(201, &[Some("Secret Bot Game")]);
    bots.occupants[0].is_bot = true;
    let reply = ready(&snapshot(vec![mixed, bots, room(202, &[])]));
    assert_eq!(
        reply.entries,
        vec![Entry {
            channel_id: 200,
            humans: 1,
            seats: "2 seats open".to_owned(),
            game: Some("Apex".to_owned()),
            members_playing: Some(1)
        }]
    );
    assert!(!reply.content.contains("Secret Bot Game"));
    assert!(!reply.content.contains("<#201>"));
    assert!(!reply.content.contains("<#202>"));
}

#[test]
fn empty_guild_and_fully_filtered_guild_have_same_honest_empty_state() {
    let empty = ready(&snapshot(Vec::new()));
    let hidden = ready(&snapshot(vec![private_room(201)]));
    assert!(empty.entries.is_empty());
    assert!(hidden.entries.is_empty());
    assert_eq!(empty.content, hidden.content);
    assert!(empty
        .content
        .contains("No public rooms are live right now."));
    assert_eq!(empty.omitted, 0);
    assert_eq!(hidden.omitted, 0);
    no_game(&empty);
}

#[test]
fn twenty_six_eligible_rooms_have_twenty_five_links_and_truthful_overflow() {
    let rooms = (200..226).rev().map(|id| room(id, &[None])).collect();
    let reply = ready(&snapshot(rooms));
    assert_eq!(ids(&reply), (200..225).collect::<Vec<_>>());
    assert_eq!(reply.omitted, 1);
    assert!(reply.content.contains("and 1 more rooms"));
    assert_eq!(reply.content.matches("<#").count(), 25);
    assert!(!reply.content.contains("<#225>"));
}

#[test]
fn denied_rooms_before_cap_never_crowd_out_eligible_rooms_or_inflate_overflow() {
    let mut rooms: Vec<_> = (200..226).map(private_room).collect();
    rooms.extend([room(400, &[None]), room(401, &[None])]);
    let reply = ready(&snapshot(rooms));
    assert_eq!(ids(&reply), vec![400, 401]);
    assert_eq!(reply.omitted, 0);
    assert!(!reply.content.contains("more rooms"));
    assert!(!reply.content.contains("Secret"));
    assert_eq!(reply.content.matches("<#").count(), 2);
}

#[test]
fn game_display_is_bounded_to_forty_eight_unicode_chars_with_visible_ellipsis() {
    let title = "🎮".repeat(90);
    let reply = ready(&snapshot(vec![room(200, &[Some(&title)])]));
    let label = reply.entries[0]
        .game
        .as_deref()
        .expect("visible bounded game");
    assert!(label.starts_with("🎮🎮🎮"));
    assert!(label.chars().count() <= 48);
    assert!(label.ends_with('…') || label.ends_with("..."));
    assert!(!reply.content.contains(&title));
}

#[test]
fn dense_unicode_board_shortens_text_before_dropping_rooms_and_keeps_truthful_total() {
    let title = "🎮".repeat(90);
    let base = 9_000_000_000_000_000_000;
    let rooms = (0..26).map(|i| room(base + i, &[Some(&title)])).collect();
    let reply = ready(&snapshot(rooms));
    assert!(!reply.entries.is_empty());
    assert!(reply.entries.len() <= 25);
    assert_eq!(reply.entries.len() + reply.omitted, 26);
    assert!(reply
        .content
        .contains(&format!("and {} more rooms", reply.omitted)));
    assert!(reply.content.contains('…') || reply.content.contains("..."));
    // Twenty-five links, counts and seat labels fit after shortening games.
    assert_eq!(reply.entries.len(), 25, "shorten text before dropping rows");
    assert_eq!(reply.content.matches("<#").count(), reply.entries.len());
}

#[test]
fn hostile_member_text_cannot_forge_mentions_channels_or_active_markdown() {
    // Each attack fits below the display cap, so truncation cannot vacuously
    // hide the unsafe suffix. Exercise both raw presence and aliased titles.
    for hostile in [
        "Pilot @everyone",
        "Pilot @here",
        "Pilot <@123456789>",
        "Pilot <@&123456789>",
        "Pilot <#123456789>",
        "Pilot **LOUD** `code`",
        "[Pilot](https://example.invalid)",
    ] {
        for aliased in [false, true] {
            let raw = if aliased { "Hostile raw game" } else { hostile };
            let mut facts = snapshot(vec![room(200, &[Some(raw)])]);
            facts.rooms[0].name = hostile.to_owned();
            if aliased {
                facts.aliases =
                    AliasTable::from_entries([("Hostile raw game", hostile)]).expect("valid alias");
            }
            let reply = ready(&facts);
            assert_eq!(ids(&reply), vec![200]);
            assert!(reply.entries[0].game.is_some());
            assert!(reply.content.contains("Pilot"), "retain readable game text");
            for token in [
                "@everyone",
                "@here",
                "<@",
                "<#123456789>",
                "**LOUD**",
                "`code`",
                "](",
            ] {
                assert!(
                    !reply.content.contains(token),
                    "active member-influenced token: {token}"
                );
            }
            assert_eq!(
                reply.content.matches("<#").count(),
                1,
                "only the real channel link is a pill"
            );
            assert!(
                !reply.content.contains(&ROOM_OWNER.to_string()),
                "no per-member output"
            );
        }
    }
}

#[test]
fn not_ready_or_missing_viewer_roles_refuses_without_partial_truth() {
    for missing_roles in [false, true] {
        let mut facts = snapshot(vec![room(200, &[Some("Apex")]), private_room(201)]);
        facts.ready = missing_roles;
        if missing_roles {
            facts.viewer_roles = None;
        }
        let reply = adapter::render(&facts);
        assert_eq!(reply.state, State::CatchingUp);
        assert!(reply.entries.is_empty());
        assert_eq!(reply.omitted, 0);
        assert!(reply.content.to_lowercase().contains("still catching up"));
        assert!(reply.content.to_lowercase().contains("try again"));
        assert!(!reply.content.contains("No public rooms"));
        no_disclosure(&reply, 200, "Room name is not a game", "Apex");
        no_disclosure(&reply, 201, "Secret room marker", "Secret Game");
    }
}

#[test]
fn repeated_pure_renders_use_supplied_generation_and_time_not_wall_clock() {
    let mut facts = snapshot(vec![room(200, &[None])]);
    let before = format!("{facts:?}");
    let first = ready(&facts);
    let again = ready(&facts);
    assert_eq!(first.content, again.content);
    assert_eq!(first.entries, again.entries);
    assert_eq!(format!("{facts:?}"), before, "render mutated input facts");
    facts.generation = 38;
    facts.as_of_unix = NOW + 60;
    facts.rooms.clear();
    let next = ready(&facts);
    assert_eq!(next.generation, 38);
    assert!(
        next.entries.is_empty(),
        "no stale rooms cached across generations"
    );
}

#[test]
fn control_existing_permission_resolver_proves_private_grant_and_vote_kick_facts() {
    let mut facts = snapshot(vec![private_room(201)]);
    let resolved = |facts: &Snapshot, viewer| {
        effective_permissions(
            facts.guild_id,
            facts.guild_owner_id,
            viewer,
            &[],
            &facts.roles,
            &facts.rooms[0].overwrites,
        )
    };
    let ordinary = resolved(&facts, VIEWER).expect("complete role table");
    assert!(ordinary.contains(Permissions::VIEW_CHANNEL));
    assert!(!ordinary.contains(Permissions::CONNECT));
    assert!(resolved(&facts, GRANTED)
        .expect("granted member")
        .contains(Permissions::CONNECT));
    facts.rooms[0].overwrites[1] = overwrite(
        GRANTED,
        PermissionOverwriteType::Member,
        Permissions::empty(),
        Permissions::CONNECT,
    );
    assert!(!resolved(&facts, GRANTED)
        .expect("vote-kick deny")
        .contains(Permissions::CONNECT));
    assert_eq!(
        effective_permissions(
            GUILD,
            GUILD_OWNER,
            VIEWER,
            &[Id::new(999)],
            &facts.roles,
            &[]
        ),
        None
    );
}

#[test]
fn control_existing_room_name_presence_path_agrees_with_literal_alias_expectation() {
    let presences = [
        human(ROOM_OWNER, Some("APEX LEGENDS")),
        human(104, Some("Apex")),
        human(105, None),
    ];
    let occupants: Vec<_> = presences
        .iter()
        .map(|member| OccupantPresence {
            presence: member.presence.as_ref(),
            live_discord: false,
            is_owner: member.member_id == ROOM_OWNER,
        })
        .collect();
    let aliases = AliasTable::from_entries([("apex legends", "Apex")]).expect("valid alias");
    let mut context = RoomContext::default();
    apply_room_presence(
        &mut context,
        &mut ConditionFacts::default(),
        &occupants,
        &aliases,
        &GameOptions::default(),
    );
    assert_eq!(context.game_name, "Apex");
    assert_eq!(context.members_playing, 2);
}
