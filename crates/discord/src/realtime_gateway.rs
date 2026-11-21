//! Project allowlisted gateway events into realtime v1 operations. No I/O.

use serde::{Deserialize, Deserializer};
use serde_json::Value;
use twilight_model::{
    channel::{permission_overwrite::PermissionOverwriteType, ChannelType},
    gateway::payload::incoming::{GuildUpdate, RoleDelete, VoiceStateUpdate},
    id::{
        marker::{ChannelMarker, GenericMarker, GuildMarker, RoleMarker},
        Id,
    },
};
use two_bot_core::realtime_wire as wire;
use wire::{Change, Operation};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectionError {
    InvalidPayload,
    UnknownChannelKind,
    IncompleteChannel,
    UnsupportedOverwrite,
    InvalidPosition,
}

/// Project a topology or voice change for the configured guild only.
/// Pass the dispatch type (`t`) and original data (`d`) from the same Discord
/// envelope, before Twilight's permission decoder can truncate unknown bits.
/// `Ok(None)` is outside this allowlist. An error means the reducer cannot
/// apply the update safely and must rebuild rather than keep its old state.
/// The reducer still owns codec validation, reference cleanup and sequencing.
pub fn project(
    guild_id: Id<GuildMarker>,
    event_type: &str,
    payload: &Value,
) -> Result<Option<Operation>, ProjectionError> {
    let scope_key = if event_type == "GUILD_UPDATE" {
        "id"
    } else {
        "guild_id"
    };
    if Id::<GuildMarker>::deserialize(&payload[scope_key]).ok() != Some(guild_id) {
        return Ok(None);
    }

    // Twilight also maps omitted nullable fields to None; only explicit null clears state.
    let operation = match event_type {
        "CHANNEL_CREATE" | "CHANNEL_UPDATE" | "CHANNEL_DELETE" => {
            return channel_change(payload, event_type == "CHANNEL_DELETE");
        }
        "GUILD_UPDATE" if payload.get("afk_channel_id").is_some() => {
            let guild: GuildUpdate = decode(payload)?;
            Operation::Guild(Change::Upsert {
                id: wire_id(guild_id),
                value: wire::Guild {
                    owner_id: wire_id(guild.owner_id),
                    afk_channel_id: guild.afk_channel_id.map(wire_id),
                },
            })
        }
        "GUILD_ROLE_CREATE" | "GUILD_ROLE_UPDATE" => {
            let role: RoleRow = decode(&payload["role"])?;
            let id = wire_id(role.id);
            Operation::Roles(Change::Upsert {
                id,
                value: wire::Role {
                    id,
                    permissions: role.permissions,
                },
            })
        }
        "GUILD_ROLE_DELETE" => {
            let event: RoleDelete = decode(payload)?;
            Operation::Roles(Change::Remove {
                id: wire_id(event.role_id),
            })
        }
        "VOICE_STATE_UPDATE" if payload.get("channel_id").is_some() => {
            let state: VoiceStateUpdate = decode(payload)?;
            let id = wire_id(state.user_id);
            Operation::Voice(match state.channel_id {
                Some(channel_id) => Change::Upsert {
                    id,
                    value: wire::Voice {
                        user_id: id,
                        channel_id: wire_id(channel_id),
                    },
                },
                None => Change::Remove { id },
            })
        }
        "GUILD_UPDATE" | "VOICE_STATE_UPDATE" => return Err(ProjectionError::InvalidPayload),
        _ => return Ok(None),
    };
    Ok(Some(operation))
}

fn wire_id<M>(id: Id<M>) -> wire::Id {
    wire::Id(id.into_nonzero())
}

fn decode<'a, T: Deserialize<'a>>(payload: &'a Value) -> Result<T, ProjectionError> {
    T::deserialize(payload).map_err(|_| ProjectionError::InvalidPayload)
}

fn permission_mask<'de, D: Deserializer<'de>>(d: D) -> Result<wire::Permissions, D::Error> {
    let mask = <&str>::deserialize(d)?;
    if !mask.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(serde::de::Error::custom("permission mask is not decimal"));
    }
    mask.parse()
        .map(wire::Permissions)
        .map_err(serde::de::Error::custom)
}

#[derive(Deserialize)]
struct RoleRow {
    id: Id<RoleMarker>,
    #[serde(deserialize_with = "permission_mask")]
    permissions: wire::Permissions,
}

#[derive(Deserialize)]
struct ChannelKey {
    id: Id<ChannelMarker>,
    #[serde(rename = "type")]
    kind: ChannelType,
}

#[derive(Deserialize)]
struct ChannelRow {
    name: Option<String>,
    parent_id: Option<Id<ChannelMarker>>,
    position: Option<i32>,
    permission_overwrites: Option<Vec<OverwriteRow>>,
}

#[derive(Deserialize)]
struct OverwriteRow {
    id: Id<GenericMarker>,
    #[serde(rename = "type")]
    kind: PermissionOverwriteType,
    #[serde(deserialize_with = "permission_mask")]
    allow: wire::Permissions,
    #[serde(deserialize_with = "permission_mask")]
    deny: wire::Permissions,
}

fn channel_change(payload: &Value, delete: bool) -> Result<Option<Operation>, ProjectionError> {
    let channel: ChannelKey = decode(payload)?;
    let kind = match channel.kind {
        ChannelType::GuildVoice => wire::ChannelKind::Voice,
        ChannelType::GuildStageVoice => wire::ChannelKind::Stage,
        ChannelType::GuildCategory => wire::ChannelKind::Category,
        ChannelType::GuildText
        | ChannelType::GuildAnnouncement
        | ChannelType::GuildDirectory
        | ChannelType::GuildForum
        | ChannelType::GuildMedia => wire::ChannelKind::Other,
        ChannelType::Private
        | ChannelType::Group
        | ChannelType::AnnouncementThread
        | ChannelType::PublicThread
        | ChannelType::PrivateThread => return Ok(None),
        _ => return Err(ProjectionError::UnknownChannelKind),
    };
    let id = wire_id(channel.id);
    if delete {
        return Ok(Some(Operation::Channels(Change::Remove { id })));
    }

    let channel: ChannelRow = decode(payload)?;
    let name = channel.name.ok_or(ProjectionError::IncompleteChannel)?;
    let overwrites = channel
        .permission_overwrites
        .ok_or(ProjectionError::IncompleteChannel)?
        .into_iter()
        .map(|entry| {
            let kind = match entry.kind {
                PermissionOverwriteType::Role => wire::OverwriteKind::Role,
                PermissionOverwriteType::Member => wire::OverwriteKind::Member,
                _ => return Err(ProjectionError::UnsupportedOverwrite),
            };
            Ok(wire::Overwrite {
                kind,
                id: wire_id(entry.id),
                allow: entry.allow,
                deny: entry.deny,
            })
        })
        .collect::<Result<_, _>>()?;
    let position = channel
        .position
        .map(u32::try_from)
        .transpose()
        .map_err(|_| ProjectionError::InvalidPosition)?;

    Ok(Some(Operation::Channels(Change::Upsert {
        id,
        value: wire::Channel {
            id,
            name,
            kind,
            parent_id: channel.parent_id.map(wire_id),
            position,
            overwrites,
        },
    })))
}

#[cfg(test)]
mod tests;
