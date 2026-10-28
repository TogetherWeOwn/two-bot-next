//! Protected, spec-first acceptance for the live-rooms snapshot reader.
//! Synthetic memory only: no actor, gateway, HTTP, database or environment writes.

#![cfg(test)]

use super::*;
use two_bot_core::{voice_alias::AliasTable, voice_naming::GameOptions};
use two_bot_discord::voice_rooms_board::{
    render_board, Board, BoardEntry, BoardOccupant, BoardRoom, BoardSnapshot, BoardState, Seats,
};

#[path = "voice_live_rooms_snapshot_adapter.rs"]
mod adapter;

const GUILD: Snowflake = 1;
const GUILD_OWNER: Snowflake = 999;
const VIEWER: Snowflake = 90;
const ROOM: Snowflake = 111;
const AS_OF: i64 = 1_800_000_000;

/// Owned observation vocabulary, not a required production API. The editable
/// bridge may only convert and call the real production snapshot reader.
#[derive(Debug, Clone)]
struct SnapshotFacts {
    ready: bool,
    generation: u64,
    as_of_unix: i64,
    guild_id: Snowflake,
    guild_owner_id: Snowflake,
    viewer_id: Snowflake,
    viewer_roles: Option<Vec<Id<RoleMarker>>>,
    roles: Vec<Role>,
    presence_enabled: bool,
    rooms: Vec<RoomFacts>,
}

#[derive(Debug, Clone)]
struct RoomFacts {
    channel_id: Snowflake,
    owner_id: Snowflake,
    user_limit: Option<u16>,
    overwrites: Vec<PermissionOverwrite>,
    occupants: Vec<OccupantFacts>,
}

#[derive(Debug, Clone)]
struct OccupantFacts {
    member_id: Snowflake,
    is_bot: bool,
    presence: Option<MemberPresence>,
}

impl SnapshotFacts {
    /// Type conversion only. All permission, majority, seat and rendering
    /// policy remains in the already-shipped pure board core.
    fn render(&self, aliases: &AliasTable, options: &GameOptions) -> Board {
        render_board(&BoardSnapshot {
            ready: self.ready,
            generation: self.generation,
            as_of_unix: self.as_of_unix,
            guild_id: self.guild_id,
            guild_owner_id: self.guild_owner_id,
            viewer_id: self.viewer_id,
            viewer_roles: self.viewer_roles.as_deref(),
            roles: &self.roles,
            presence_enabled: self.presence_enabled,
            aliases,
            game_options: options,
            rooms: self
                .rooms
                .iter()
                .map(|room| BoardRoom {
                    channel_id: room.channel_id,
                    tracked: true,
                    owner_id: room.owner_id,
                    user_limit: room.user_limit,
                    overwrites: &room.overwrites,
                    occupants: room
                        .occupants
                        .iter()
                        .map(|occupant| BoardOccupant {
                            member_id: occupant.member_id,
                            is_bot: occupant.is_bot,
                            presence: occupant.presence.as_ref(),
                        })
                        .collect(),
                })
                .collect(),
        })
    }

    fn board(&self) -> Board {
        let mut aliases = AliasTable::new();
        aliases.add("Apex Legends", "Apex").expect("fixture alias");
        self.render(&aliases, &GameOptions::default())
    }
}

fn role(id: Snowflake, permissions: Permissions) -> Role {
    serde_json::from_value(serde_json::json!({
        "id": id.to_string(), "name": "fixture", "color": 0,
        "hoist": false, "managed": false, "mentionable": false,
        "permissions": permissions.bits().to_string(), "position": 0,
        "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null },
        "flags": 0
    }))
    .expect("synthetic guild role")
}

fn channel(id: Snowflake, limit: Option<u16>, overwrites: Vec<PermissionOverwrite>) -> Channel {
    serde_json::from_value(serde_json::json!({
        "id": id.to_string(), "guild_id": GUILD.to_string(),
        "name": "fixture room", "type": 2, "user_limit": limit,
        "permission_overwrites": overwrites
    }))
    .expect("synthetic voice channel")
}

fn overwrite(
    id: Snowflake,
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

fn tracked(channel_id: Snowflake, owner_id: Snowflake) -> VoiceRoom {
    VoiceRoom {
        guild_id: GUILD,
        channel_id,
        creator_channel_id: 10,
        owner_id,
        original_creator_id: 70,
        name_seed: 42,
        created_at: "2026-10-26T12:00:00.000Z".to_owned(),
    }
}

fn member(channel_id: Option<Snowflake>, bot: Option<bool>) -> MemberState {
    MemberState {
        channel_id,
        bot,
        transition: 1,
        joined_at_ms: 100,
    }
}

fn game(title: &str) -> MemberPresence {
    MemberPresence {
        game: Some(title.to_owned()),
        ..MemberPresence::default()
    }
}

fn ready_state() -> LiveState {
    LiveState {
        ready: true,
        generation: 41,
        channels: HashMap::from([(ROOM, channel(ROOM, Some(3), Vec::new()))]),
        members: HashMap::from([
            (71, member(Some(ROOM), Some(false))),
            (72, member(Some(ROOM), Some(false))),
            (73, member(Some(ROOM), Some(true))),
            (74, member(Some(ROOM), Some(true))),
            (75, member(Some(ROOM), Some(true))),
            (80, member(Some(222), Some(false))),
            (81, member(None, Some(false))),
        ]),
        member_roles: HashMap::from([(VIEWER, vec![7])]),
        bot: Some(BotAccess {
            member_id: 99,
            guild_owner_id: GUILD_OWNER,
            system_channel_id: None,
            member_roles: vec![Id::new(8)],
            roles: vec![
                role(GUILD, Permissions::VIEW_CHANNEL),
                role(7, Permissions::CONNECT),
                role(8, Permissions::ADMINISTRATOR),
            ],
        }),
        presences: HashMap::from([
            (71, game("Apex Legends")),
            (72, game("Apex Legends")),
            (73, game("Bot game")),
            (74, game("Bot game")),
            (75, game("Bot game")),
            (80, game("Other room game")),
            (81, game("Disconnected game")),
        ]),
        ..LiveState::default()
    }
}

fn live(state: LiveState) -> LiveGuild {
    let live = LiveGuild::new(GUILD);
    *live.write_state() = state;
    live
}

fn capture(
    live: &LiveGuild,
    ledger: &[VoiceRoom],
    viewer: Snowflake,
    presence: bool,
) -> SnapshotFacts {
    let result = adapter::capture(live, ledger, viewer, AS_OF, presence);
    assert!(
        result.is_ok(),
        "missing production live-rooms snapshot adapter is not a legitimate refusal: {result:?}"
    );
    result.expect("adapter result checked above")
}

fn human_ids(room: &RoomFacts) -> Vec<Snowflake> {
    let mut ids: Vec<_> = room
        .occupants
        .iter()
        .filter(|occupant| !occupant.is_bot)
        .map(|occupant| occupant.member_id)
        .collect();
    ids.sort_unstable();
    ids
}

fn assert_refusal(snapshot: &SnapshotFacts) {
    assert!(
        snapshot.rooms.is_empty(),
        "a refusal must carry zero room facts"
    );
    let board = snapshot.board();
    assert_eq!(board.state, BoardState::CatchingUp);
    assert!(board.entries.is_empty());
    assert_eq!(board.omitted, 0);
    assert!(!board.content.contains("<#"));
    assert!(!board.content.contains("Apex"));
    assert!(!board.content.contains("No public rooms"));
}

#[test]
fn control_existing_live_occupancy_excludes_bots_and_off_room_presences() {
    let state = ready_state();
    assert_eq!(state.occupants(ROOM), vec![71, 72]);
    assert_eq!(state.humans(ROOM), 2);
    assert_eq!(state.occupants(222), vec![80]);
    assert_eq!(
        state.presences.get(&71).and_then(|p| p.game.as_deref()),
        Some("Apex Legends")
    );
}

#[test]
fn control_existing_role_resolver_distinguishes_empty_missing_and_unmirrored_roles() {
    let mut state = ready_state();
    assert_eq!(
        state.guild_member_permissions(GUILD, VIEWER),
        Some(Permissions::VIEW_CHANNEL | Permissions::CONNECT)
    );
    state.member_roles.insert(VIEWER, vec![]);
    assert_eq!(
        state.guild_member_permissions(GUILD, VIEWER),
        Some(Permissions::VIEW_CHANNEL)
    );
    state.member_roles.remove(&VIEWER);
    assert_eq!(state.guild_member_permissions(GUILD, VIEWER), None);
    state.member_roles.insert(VIEWER, vec![12345]);
    assert_eq!(state.guild_member_permissions(GUILD, VIEWER), None);
    state.member_roles.insert(VIEWER, vec![7]);
    state
        .bot
        .as_mut()
        .expect("fixture bot")
        .roles
        .retain(|r| r.id.get() != GUILD);
    assert_eq!(state.guild_member_permissions(GUILD, VIEWER), None);
}

#[test]
fn ready_snapshot_maps_live_roles_occupants_presence_limit_and_clock_to_core_facts() {
    let live = live(ready_state());
    let snapshot = capture(&live, &[tracked(ROOM, 71)], VIEWER, true);
    assert!(snapshot.ready);
    assert_eq!(snapshot.generation, 41);
    assert_eq!(snapshot.as_of_unix, AS_OF);
    assert_eq!(snapshot.guild_id, GUILD);
    assert_eq!(snapshot.guild_owner_id, GUILD_OWNER);
    assert_eq!(snapshot.viewer_id, VIEWER);
    assert_eq!(snapshot.viewer_roles, Some(vec![Id::new(7)]));
    let mut roles: Vec<_> = snapshot
        .roles
        .iter()
        .map(|r| (r.id.get(), r.permissions.bits()))
        .collect();
    roles.sort_unstable();
    assert_eq!(
        roles,
        vec![
            (1, Permissions::VIEW_CHANNEL.bits()),
            (7, Permissions::CONNECT.bits()),
            (8, Permissions::ADMINISTRATOR.bits())
        ]
    );
    assert!(snapshot.presence_enabled);
    assert_eq!(snapshot.rooms.len(), 1);
    let room = &snapshot.rooms[0];
    assert_eq!(room.channel_id, ROOM);
    assert_eq!(room.owner_id, 71);
    assert_eq!(room.user_limit, Some(3));
    assert!(room.overwrites.is_empty());
    assert_eq!(human_ids(room), vec![71, 72]);
    for occupant in room.occupants.iter().filter(|occupant| !occupant.is_bot) {
        assert_eq!(
            occupant.presence.as_ref().and_then(|p| p.game.as_deref()),
            Some("Apex Legends")
        );
    }
    let board = snapshot.board();
    assert_eq!(board.state, BoardState::Ready);
    assert_eq!(board.generation, 41);
    assert_eq!(
        board.entries,
        vec![BoardEntry {
            channel_id: ROOM,
            humans: 2,
            seats: Seats::Remaining(1),
            game: Some("Apex".to_owned()),
            members_playing: Some(2),
        }]
    );
    assert!(board.content.contains("<t:1800000000"));
}

#[test]
fn pre_authoritative_snapshot_refuses_despite_retained_room_and_presence_data() {
    let mut state = ready_state();
    state.ready = false;
    let snapshot = capture(&live(state), &[tracked(ROOM, 71)], VIEWER, true);
    assert!(!snapshot.ready);
    assert_refusal(&snapshot);
}

#[test]
fn disconnect_refuses_instead_of_publishing_the_previous_ready_generation() {
    let live = live(ready_state());
    live.disconnect();
    let snapshot = capture(&live, &[tracked(ROOM, 71)], VIEWER, true);
    assert_eq!(snapshot.generation, 42);
    assert_refusal(&snapshot);
}

#[test]
fn absent_invoker_role_entry_refuses_without_any_room_facts() {
    let mut state = ready_state();
    state.member_roles.remove(&VIEWER);
    let snapshot = capture(&live(state), &[tracked(ROOM, 71)], VIEWER, true);
    assert!(snapshot.viewer_roles.is_none());
    assert_refusal(&snapshot);
}

#[test]
fn guild_owner_without_a_member_role_entry_still_gets_the_catching_up_refusal() {
    let snapshot = capture(
        &live(ready_state()),
        &[tracked(ROOM, 71)],
        GUILD_OWNER,
        true,
    );
    assert_refusal(&snapshot);
}

#[test]
fn present_empty_invoker_roles_are_known_not_a_missing_member_snapshot() {
    let mut state = ready_state();
    state.member_roles.insert(VIEWER, Vec::new());
    state.bot.as_mut().expect("fixture bot").roles[0].permissions |= Permissions::CONNECT;
    let snapshot = capture(&live(state), &[tracked(ROOM, 71)], VIEWER, true);
    assert_eq!(snapshot.viewer_roles, Some(Vec::new()));
    assert_eq!(snapshot.board().state, BoardState::Ready);
    assert_eq!(snapshot.board().entries.len(), 1);
}

#[test]
fn bot_only_and_empty_rooms_produce_no_snapshot_room_facts() {
    for bots_present in [false, true] {
        let mut state = ready_state();
        state.members.retain(|_, member| member.bot == Some(true));
        if !bots_present {
            state.members.clear();
        }
        let snapshot = capture(&live(state), &[tracked(ROOM, 71)], VIEWER, true);
        assert!(snapshot.rooms.is_empty());
        let board = snapshot.board();
        assert_eq!(board.state, BoardState::Ready);
        assert!(board.entries.is_empty());
        assert_eq!(board.content, "No public rooms are live right now.");
    }
}

#[test]
fn unknown_bot_identity_counts_as_human_using_the_existing_occupancy_rule() {
    let mut state = ready_state();
    state.members = HashMap::from([(71, member(Some(ROOM), None))]);
    let snapshot = capture(&live(state), &[tracked(ROOM, 71)], VIEWER, true);
    assert_eq!(human_ids(&snapshot.rooms[0]), vec![71]);
    assert_eq!(snapshot.board().entries[0].humans, 1);
}

#[test]
fn configured_staging_synthetic_bot_counts_as_human_but_other_bots_do_not() {
    let mut state = ready_state();
    state.members.retain(|_, member| member.bot == Some(true));
    state.synthetic_humans.insert(73);
    let snapshot = capture(&live(state), &[tracked(ROOM, 73)], VIEWER, true);
    assert_eq!(human_ids(&snapshot.rooms[0]), vec![73]);
    assert_eq!(snapshot.board().entries[0].humans, 1);
    assert_eq!(
        snapshot.board().entries[0].game.as_deref(),
        Some("Bot game")
    );
}

#[test]
fn live_guild_never_counts_configured_synthetic_bots_as_human_occupants() {
    let guild: Snowflake = two_bot_core::backup::guild_config::LIVE_GUILD_ID
        .parse()
        .expect("published live guild constant");
    let mut state = ready_state();
    state.members.retain(|_, member| member.bot == Some(true));
    state.synthetic_humans = synthetic_humans_for(guild, &HashSet::from([73]));
    assert!(
        state.synthetic_humans.is_empty(),
        "existing runtime fence must remove synthetic overrides for the live guild"
    );
    state.bot.as_mut().expect("fixture bot").roles[0] = role(guild, Permissions::VIEW_CHANNEL);
    state
        .channels
        .get_mut(&ROOM)
        .expect("fixture channel")
        .guild_id = Some(Id::new(guild));
    let live = LiveGuild::new(guild);
    *live.write_state() = state;
    let mut room = tracked(ROOM, 73);
    room.guild_id = guild;
    let snapshot = capture(&live, &[room], VIEWER, true);
    assert!(snapshot.rooms.is_empty());
    assert_eq!(snapshot.board().state, BoardState::Ready);
    assert!(snapshot.board().entries.is_empty());
}

#[test]
fn ledger_intersection_excludes_untracked_missing_and_other_guild_rows() {
    let mut state = ready_state();
    state
        .channels
        .insert(222, channel(222, Some(8), Vec::new()));
    state
        .channels
        .insert(333, channel(333, Some(8), Vec::new()));
    state.members.insert(82, member(Some(333), Some(false)));
    let mut foreign = tracked(333, 82);
    foreign.guild_id = 2;
    let snapshot = capture(
        &live(state),
        &[tracked(ROOM, 71), tracked(444, 71), foreign],
        VIEWER,
        true,
    );
    assert_eq!(
        snapshot
            .rooms
            .iter()
            .map(|r| r.channel_id)
            .collect::<Vec<_>>(),
        vec![ROOM]
    );
    assert_eq!(
        snapshot
            .board()
            .entries
            .iter()
            .map(|r| r.channel_id)
            .collect::<Vec<_>>(),
        vec![ROOM]
    );
}

#[test]
fn disabled_presence_never_renders_cached_games_or_playing_counts() {
    let snapshot = capture(&live(ready_state()), &[tracked(ROOM, 71)], VIEWER, false);
    assert!(!snapshot.presence_enabled);
    let entry = &snapshot.board().entries[0];
    assert_eq!(entry.humans, 2);
    assert_eq!(entry.game, None);
    assert_eq!(entry.members_playing, None);
}

#[test]
fn missing_presence_has_no_general_game_fallback() {
    let mut state = ready_state();
    state.presences.clear();
    let snapshot = capture(&live(state), &[tracked(ROOM, 71)], VIEWER, true);
    let board = snapshot.board();
    assert_eq!(board.entries[0].game, None);
    assert_eq!(board.entries[0].members_playing, None);
    assert!(!board.content.contains("General"));
}

#[test]
fn ledger_owner_not_original_creator_supplies_force_single_game_tie_break() {
    let mut state = ready_state();
    state.presences.insert(71, game("Owner game"));
    state.presences.insert(72, game("Other game"));
    let snapshot = capture(&live(state), &[tracked(ROOM, 71)], VIEWER, true);
    let board = snapshot.render(
        &AliasTable::new(),
        &GameOptions {
            force_single: true,
            ..GameOptions::default()
        },
    );
    assert_eq!(board.entries[0].game.as_deref(), Some("Owner game"));
    assert_eq!(board.entries[0].members_playing, Some(2));
}

#[test]
fn live_user_limit_preserves_unlimited_full_and_exact_remaining_seats() {
    for (limit, expected) in [
        (None, Seats::Open),
        (Some(0), Seats::Open),
        (Some(1), Seats::Full),
        (Some(2), Seats::Full),
        (Some(5), Seats::Remaining(3)),
    ] {
        let mut state = ready_state();
        state
            .channels
            .get_mut(&ROOM)
            .expect("fixture channel")
            .user_limit = limit;
        let snapshot = capture(&live(state), &[tracked(ROOM, 71)], VIEWER, true);
        assert_eq!(snapshot.rooms[0].user_limit, limit);
        assert_eq!(snapshot.board().entries[0].seats, expected);
    }
}

#[test]
fn enforced_channel_overwrites_win_over_category_permissions_and_bot_administrator() {
    let deny = overwrite(
        GUILD,
        PermissionOverwriteType::Role,
        Permissions::empty(),
        Permissions::CONNECT,
    );
    let mut state = ready_state();
    state
        .channels
        .get_mut(&ROOM)
        .expect("fixture channel")
        .permission_overwrites = Some(vec![deny.clone()]);
    state
        .channels
        .get_mut(&ROOM)
        .expect("fixture channel")
        .parent_id = Some(Id::new(500));
    let mut category = channel(
        500,
        None,
        vec![overwrite(
            7,
            PermissionOverwriteType::Role,
            Permissions::VIEW_CHANNEL | Permissions::CONNECT,
            Permissions::empty(),
        )],
    );
    category.kind = ChannelType::GuildCategory;
    state.channels.insert(500, category);
    let snapshot = capture(&live(state), &[tracked(ROOM, 71)], VIEWER, true);
    assert_eq!(snapshot.rooms[0].overwrites, vec![deny]);
    let board = snapshot.board();
    assert_eq!(board.state, BoardState::Ready);
    assert!(
        board.entries.is_empty(),
        "bot Administrator and a category allow cannot bypass the viewer's channel Connect deny"
    );
    assert_eq!(board.omitted, 0);
}

#[test]
fn direct_access_grant_and_final_member_connect_deny_remain_viewer_specific() {
    let mut state = ready_state();
    state.member_roles.insert(91, vec![7]);
    state
        .channels
        .get_mut(&ROOM)
        .expect("fixture channel")
        .permission_overwrites = Some(vec![
        overwrite(
            GUILD,
            PermissionOverwriteType::Role,
            Permissions::empty(),
            Permissions::CONNECT,
        ),
        overwrite(
            VIEWER,
            PermissionOverwriteType::Member,
            Permissions::CONNECT,
            Permissions::empty(),
        ),
        overwrite(
            91,
            PermissionOverwriteType::Member,
            Permissions::empty(),
            Permissions::CONNECT,
        ),
    ]);
    let live = live(state);
    let granted = capture(&live, &[tracked(ROOM, 71)], VIEWER, true).board();
    let denied = capture(&live, &[tracked(ROOM, 71)], 91, true).board();
    assert_eq!(granted.entries.len(), 1);
    assert_eq!(granted.entries[0].channel_id, ROOM);
    assert_eq!(denied.state, BoardState::Ready);
    assert!(denied.entries.is_empty());
    assert_eq!(denied.omitted, 0);
}

#[test]
fn unmirrored_assigned_or_everyone_roles_never_disclose_a_partial_board() {
    for missing in [GUILD, 7] {
        let mut state = ready_state();
        state
            .bot
            .as_mut()
            .expect("fixture bot")
            .roles
            .retain(|role| role.id.get() != missing);
        let snapshot = capture(&live(state), &[tracked(ROOM, 71)], VIEWER, true);
        let board = snapshot.board();
        assert_eq!(board.state, BoardState::CatchingUp);
        assert!(board.entries.is_empty());
        assert_eq!(board.omitted, 0);
        assert!(!board.content.contains("<#"));
        assert!(!board.content.contains("Apex"));
    }
}

#[test]
fn missing_guild_role_table_refuses_even_when_the_room_cache_claims_ready() {
    let mut state = ready_state();
    state.bot = None;
    let snapshot = capture(&live(state), &[tracked(ROOM, 71)], VIEWER, true);
    assert_refusal(&snapshot);
}

#[test]
fn captured_facts_are_detached_and_reading_never_mutates_live_or_ledger_state() {
    let live = live(ready_state());
    let ledger = vec![tracked(ROOM, 71)];
    let before = format!("{:?}", live.read_state());
    let snapshot = capture(&live, &ledger, VIEWER, true);
    assert_eq!(
        format!("{:?}", live.read_state()),
        before,
        "read-only adapter changed the live snapshot"
    );
    assert_eq!(ledger, vec![tracked(ROOM, 71)]);
    let board_before = snapshot.board();
    {
        let mut state = live.write_state();
        state.generation = 42;
        state.ready = false;
        state.channels.clear();
        state.members.clear();
        state.member_roles.clear();
        state.presences.clear();
        state.bot = None;
    }
    assert_eq!(
        snapshot.board(),
        board_before,
        "later gateway updates changed an already-captured reply"
    );
    assert_eq!(snapshot.generation, 41);
    assert_eq!(human_ids(&snapshot.rooms[0]), vec![71, 72]);
}

fn next_generation() -> LiveState {
    let mut state = ready_state();
    state.generation = 42;
    state.member_roles.insert(VIEWER, vec![9]);
    state
        .bot
        .as_mut()
        .expect("fixture bot")
        .roles
        .push(role(9, Permissions::CONNECT));
    state
        .channels
        .get_mut(&ROOM)
        .expect("fixture channel")
        .user_limit = Some(1);
    state.members = HashMap::from([(72, member(Some(ROOM), Some(false)))]);
    state.presences = HashMap::from([(72, game("Next game"))]);
    state
}

#[test]
fn gateway_change_during_capture_cannot_mix_generations_of_roles_seats_or_presences() {
    use std::sync::mpsc;

    let live = live(ready_state());
    std::thread::scope(|scope| {
        let (start_tx, start_rx) = mpsc::channel::<usize>();
        let (done_tx, done_rx) = mpsc::channel();
        let writer_live = live.clone();
        let writer = scope.spawn(move || {
            while let Ok(round) = start_rx.recv() {
                *writer_live.write_state() = if round % 2 == 0 {
                    next_generation()
                } else {
                    ready_state()
                };
                if done_tx.send(()).is_err() {
                    break;
                }
            }
        });
        for round in 0..256 {
            start_tx.send(round).expect("writer running");
            let result = adapter::capture(&live, &[tracked(ROOM, 71)], VIEWER, AS_OF, true);
            done_rx
                .recv()
                .expect("one gateway replacement completed in this capture window");
            assert!(result.is_ok(), "missing snapshot adapter: {result:?}");
            let snapshot = result.expect("adapter result checked above");
            assert!(snapshot.ready);
            assert_eq!(snapshot.rooms.len(), 1);
            let room = &snapshot.rooms[0];
            let board = snapshot.board();
            assert_eq!(board.generation, snapshot.generation);
            assert_eq!(board.state, BoardState::Ready);
            match snapshot.generation {
                41 => {
                    assert_eq!(snapshot.viewer_roles, Some(vec![Id::new(7)]));
                    assert_eq!(human_ids(room), vec![71, 72]);
                    assert_eq!(room.user_limit, Some(3));
                    assert_eq!(board.entries[0].seats, Seats::Remaining(1));
                    assert_eq!(board.entries[0].game.as_deref(), Some("Apex"));
                    assert_eq!(board.entries[0].members_playing, Some(2));
                }
                42 => {
                    assert_eq!(snapshot.viewer_roles, Some(vec![Id::new(9)]));
                    assert_eq!(human_ids(room), vec![72]);
                    assert_eq!(room.user_limit, Some(1));
                    assert_eq!(board.entries[0].seats, Seats::Full);
                    assert_eq!(board.entries[0].game.as_deref(), Some("Next game"));
                    assert_eq!(board.entries[0].members_playing, Some(1));
                }
                other => panic!("unexpected fixture generation {other}"),
            }
        }
        drop(start_tx);
        writer.join().expect("writer completed without a panic");
    });
}
