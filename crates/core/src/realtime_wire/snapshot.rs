//! Direct realtime snapshot replies with complete, cross-checked topics.

use std::collections::{HashMap, HashSet};

use serde::Serialize;

use super::{
    encode, validation, Channel, ChannelKind, Event, Guild, Id, Member, OverwriteKind, Role, Voice,
    WireTime, MAX_SEQ,
};

const SNAPSHOT_BYTES: usize = 1024 * 1024;
pub const MAX_SNAPSHOT_CHANNELS: usize = 500;
pub const MAX_SNAPSHOT_ROLES: usize = 250;
pub const MAX_SNAPSHOT_MEMBERS: usize = 10_000;
/// Maximum number of current scheduled events in a v1 snapshot.
pub const MAX_SNAPSHOT_EVENTS: usize = 100;

#[derive(Serialize)]
pub struct Topics {
    pub guild: Guild,
    pub channels: Vec<Channel>,
    pub roles: Vec<Role>,
    pub members: Vec<Member>,
    pub voice: Vec<Voice>,
    pub events: Vec<Event>,
}

#[derive(Serialize)]
struct Snapshot<'a> {
    v: u8,
    guild_id: Id,
    seq: u64,
    observed_at: WireTime,
    topics: &'a Topics,
}

/// Encode the direct action reply after checking snapshot bounds and references.
/// The caller must still prove the gateway baseline is complete and capture it
/// with its matching watermark.
pub fn encode_snapshot(
    guild_id: Id,
    seq: u64,
    observed_at: WireTime,
    topics: &Topics,
) -> Option<Vec<u8>> {
    if seq > MAX_SEQ || !valid_topics(guild_id, topics) {
        return None;
    }
    encode(
        SNAPSHOT_BYTES,
        &Snapshot {
            v: 1,
            guild_id,
            seq,
            observed_at,
            topics,
        },
    )
}

fn valid_topics(guild_id: Id, state: &Topics) -> bool {
    if state.channels.len() > MAX_SNAPSHOT_CHANNELS
        || state.roles.len() > MAX_SNAPSHOT_ROLES
        || state.members.len() > MAX_SNAPSHOT_MEMBERS
        || state.voice.len() > MAX_SNAPSHOT_MEMBERS
        || state.events.len() > MAX_SNAPSHOT_EVENTS
    {
        return false;
    }

    let rooms: HashMap<_, _> = state
        .channels
        .iter()
        .map(|row| (row.id, row.kind))
        .collect();
    let roles: HashSet<_> = state.roles.iter().map(|row| row.id).collect();
    let users: HashSet<_> = state.members.iter().map(|row| row.user_id).collect();
    let voice_room = |id| {
        matches!(
            rooms.get(&id),
            Some(ChannelKind::Voice | ChannelKind::Stage)
        )
    };

    rooms.len() == state.channels.len()
        && roles.len() == state.roles.len()
        && users.len() == state.members.len()
        && validation::distinct(state.voice.iter().map(|row| row.user_id))
        && validation::distinct(state.events.iter().map(|row| row.id))
        && roles.contains(&guild_id)
        && users.contains(&state.guild.owner_id)
        && state
            .guild
            .afk_channel_id
            .is_none_or(|id| matches!(rooms.get(&id), Some(ChannelKind::Voice)))
        && state.channels.iter().all(|row| {
            validation::channel(row)
                && row
                    .parent_id
                    .is_none_or(|id| matches!(rooms.get(&id), Some(ChannelKind::Category)))
                && row
                    .overwrites
                    .iter()
                    .all(|entry| entry.kind != OverwriteKind::Role || roles.contains(&entry.id))
        })
        && state
            .members
            .iter()
            .all(|row| validation::member(row) && row.role_ids.iter().all(|id| roles.contains(id)))
        && state
            .voice
            .iter()
            .all(|row| users.contains(&row.user_id) && voice_room(row.channel_id))
        && state
            .events
            .iter()
            .all(|row| validation::event(row) && row.channel_id.is_none_or(voice_room))
}

#[cfg(test)]
mod tests;
