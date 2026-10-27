//! Editable test-only bridge from the protected fixture vocabulary to the
//! production pure board core. It only converts types: filtering, seat math,
//! aliasing, sorting, truncation and rendering all live in
//! `two_bot_discord::voice_rooms_board`.

use crate::contract::{Entry, Reply, Snapshot, State};
use two_bot_discord::voice_rooms_board::{
    render_board, BoardOccupant, BoardRoom, BoardSnapshot, BoardState,
};

pub fn render(snapshot: &Snapshot) -> Reply {
    let rooms = snapshot
        .rooms
        .iter()
        .map(|room| BoardRoom {
            channel_id: room.channel_id,
            tracked: room.tracked,
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
        .collect();
    let board = render_board(&BoardSnapshot {
        ready: snapshot.ready,
        generation: snapshot.generation,
        as_of_unix: snapshot.as_of_unix,
        guild_id: snapshot.guild_id,
        guild_owner_id: snapshot.guild_owner_id,
        viewer_id: snapshot.viewer_id,
        viewer_roles: snapshot.viewer_roles.as_deref(),
        roles: &snapshot.roles,
        presence_enabled: snapshot.presence_enabled,
        aliases: &snapshot.aliases,
        game_options: &snapshot.game_options,
        rooms,
    });
    Reply {
        generation: board.generation,
        state: match board.state {
            BoardState::Ready => State::Ready,
            BoardState::CatchingUp => State::CatchingUp,
        },
        entries: board
            .entries
            .into_iter()
            .map(|entry| Entry {
                channel_id: entry.channel_id,
                humans: entry.humans,
                seats: entry.seats.label(),
                game: entry.game,
                members_playing: entry.members_playing,
            })
            .collect(),
        omitted: board.omitted,
        content: board.content,
    }
}
