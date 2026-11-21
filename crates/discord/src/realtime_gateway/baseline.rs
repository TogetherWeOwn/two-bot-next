//! Prove initial topology coverage without using Twilight's guild cache.

use std::collections::HashSet;

use serde::Deserialize;
use serde_json::value::RawValue;
use twilight_model::id::marker::{ChannelMarker, UserMarker};

use super::{
    channel_kind, channel_row, wire, wire_id, ChannelKey, GuildMarker, Id, ProjectionError, RoleRow,
};

pub struct Topology {
    pub guild: wire::Guild,
    pub channels: Vec<wire::Channel>,
    pub roles: Vec<wire::Role>,
    pub voice: Vec<wire::Voice>,
}

#[derive(Deserialize)]
struct Dispatch<'a> {
    op: u8,
    t: Option<&'a str>,
    #[serde(borrow)]
    d: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct GuildScope {
    id: Id<GuildMarker>,
    #[serde(default)]
    unavailable: bool,
}

#[derive(Deserialize)]
struct GuildRows<'a> {
    owner_id: Id<UserMarker>,
    // Nullable, but absence does not prove there is no AFK channel.
    #[serde(deserialize_with = "Deserialize::deserialize")]
    afk_channel_id: Option<Id<ChannelMarker>>,
    #[serde(borrow)]
    channels: Vec<&'a RawValue>,
    #[serde(borrow)]
    roles: Vec<&'a RawValue>,
    #[serde(borrow)]
    voice_states: Vec<&'a RawValue>,
}

#[derive(Deserialize)]
struct ChannelEvidence<'a> {
    guild_id: Option<Id<GuildMarker>>,
    #[serde(borrow)]
    permission_overwrites: Vec<&'a RawValue>,
}

#[derive(Deserialize)]
struct VoiceRow {
    guild_id: Option<Id<GuildMarker>>,
    user_id: Id<UserMarker>,
    channel_id: Id<ChannelMarker>,
}

/// Read the four topology topics from the original, available `GUILD_CREATE`.
/// Missing lists cannot be distinguished from empty lists after Twilight's
/// defaulting. It also replaces embedded guild IDs and truncates permission
/// masks. Read the original rows so none of those changes can hide lost data.
///
/// The caller must join the other topics at the same receive position, apply
/// intervening dispatches in order and validate the full snapshot before use.
pub fn topology(guild_id: Id<GuildMarker>, raw: &str) -> Result<Option<Topology>, ProjectionError> {
    let dispatch: Dispatch<'_> = object(raw.trim_start_matches([' ', '\t', '\n', '\r']))?;
    if dispatch.op != 0 || dispatch.t != Some("GUILD_CREATE") {
        return Ok(None);
    }
    let body = dispatch.d.ok_or(ProjectionError::InvalidPayload)?.get();
    let scope: GuildScope = object(body)?;
    if scope.id != guild_id || scope.unavailable {
        return Ok(None);
    }
    let rows: GuildRows<'_> = object(body)?;
    // Match the v1 snapshot collection bounds before projecting the rows.
    let channels = unique(rows.channels, 500, |row| {
        let key: ChannelKey = object(row)?;
        // Threads have their own GUILD_CREATE list, not a place in this one.
        let kind = channel_kind(key.kind)?.ok_or(ProjectionError::InvalidPayload)?;
        let evidence: ChannelEvidence<'_> = object(row)?;
        check_scope(guild_id, evidence.guild_id)?;
        if evidence.permission_overwrites.len() > 100
            || evidence
                .permission_overwrites
                .iter()
                .any(|entry| !entry.get().starts_with('{'))
        {
            return Err(ProjectionError::InvalidPayload);
        }
        let id = wire_id(key.id);
        let channel = channel_row(id, kind, object(row)?)?;
        let mut targets = HashSet::new();
        channel
            .overwrites
            .iter()
            .all(|entry| targets.insert((entry.kind, entry.id)))
            .then_some((id, channel))
            .ok_or(ProjectionError::InvalidPayload)
    })?;
    let roles = unique(rows.roles, 250, |row| {
        let role: RoleRow = object(row)?;
        let id = wire_id(role.id);
        Ok((
            id,
            wire::Role {
                id,
                permissions: role.permissions,
            },
        ))
    })?;
    let voice = unique(rows.voice_states, 10_000, |row| {
        let state: VoiceRow = object(row)?;
        check_scope(guild_id, state.guild_id)?;
        let user_id = wire_id(state.user_id);
        Ok((
            user_id,
            wire::Voice {
                user_id,
                channel_id: wire_id(state.channel_id),
            },
        ))
    })?;
    if !roles.iter().any(|role| role.id == wire_id(guild_id)) {
        return Err(ProjectionError::InvalidPayload);
    }
    Ok(Some(Topology {
        guild: wire::Guild {
            owner_id: wire_id(rows.owner_id),
            afk_channel_id: rows.afk_channel_id.map(wire_id),
        },
        channels,
        roles,
        voice,
    }))
}

fn check_scope(
    expected: Id<GuildMarker>,
    embedded: Option<Id<GuildMarker>>,
) -> Result<(), ProjectionError> {
    // Discord can omit nested guild IDs because the enclosing guild scopes them.
    if embedded.is_some_and(|id| id != expected) {
        return Err(ProjectionError::InvalidPayload);
    }
    Ok(())
}

fn unique<T>(
    rows: Vec<&RawValue>,
    limit: usize,
    mut project: impl FnMut(&str) -> Result<(wire::Id, T), ProjectionError>,
) -> Result<Vec<T>, ProjectionError> {
    if rows.len() > limit {
        return Err(ProjectionError::InvalidPayload);
    }
    let mut ids = HashSet::with_capacity(rows.len());
    rows.into_iter()
        .map(|raw| {
            let (id, value) = project(raw.get())?;
            ids.insert(id)
                .then_some(value)
                .ok_or(ProjectionError::InvalidPayload)
        })
        .collect()
}

fn object<'a, T: Deserialize<'a>>(raw: &'a str) -> Result<T, ProjectionError> {
    if !raw.starts_with('{') {
        return Err(ProjectionError::InvalidPayload);
    }
    serde_json::from_str(raw).map_err(|_| ProjectionError::InvalidPayload)
}

#[cfg(test)]
mod tests;
