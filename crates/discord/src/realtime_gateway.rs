//! Project allowlisted gateway events into realtime v1 operations. No I/O.

use twilight_model::{
    channel::{permission_overwrite::PermissionOverwriteType, Channel, ChannelType},
    gateway::event::Event,
    guild::Role,
    id::{marker::GuildMarker, Id},
};
use two_bot_core::realtime_wire as wire;
use wire::{Change, Operation};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectionError {
    UnknownChannelKind,
    IncompleteChannel,
    UnsupportedOverwrite,
    InvalidPosition,
}

/// Project a topology or voice change for the configured guild only.
/// `Ok(None)` is outside this allowlist. An error means the reducer cannot
/// apply the update safely and must rebuild rather than keep its old state.
/// The reducer still owns codec validation, reference cleanup and sequencing.
pub fn project(
    guild_id: Id<GuildMarker>,
    event: &Event,
) -> Result<Option<Operation>, ProjectionError> {
    if event.guild_id() != Some(guild_id) {
        return Ok(None);
    }

    let operation = match event {
        Event::ChannelCreate(channel) => return channel_change(&channel.0, false),
        Event::ChannelUpdate(channel) => return channel_change(&channel.0, false),
        Event::ChannelDelete(channel) => return channel_change(&channel.0, true),
        Event::GuildUpdate(guild) => Operation::Guild(Change::Upsert {
            id: wire_id(guild_id),
            value: wire::Guild {
                owner_id: wire_id(guild.owner_id),
                afk_channel_id: guild.afk_channel_id.map(wire_id),
            },
        }),
        Event::RoleCreate(event) => role_change(&event.role),
        Event::RoleUpdate(event) => role_change(&event.role),
        Event::RoleDelete(event) => Operation::Roles(Change::Remove {
            id: wire_id(event.role_id),
        }),
        Event::VoiceStateUpdate(state) => {
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
        _ => return Ok(None),
    };
    Ok(Some(operation))
}

fn wire_id<M>(id: Id<M>) -> wire::Id {
    wire::Id(id.into_nonzero())
}

fn role_change(role: &Role) -> Operation {
    let id = wire_id(role.id);
    Operation::Roles(Change::Upsert {
        id,
        value: wire::Role {
            id,
            permissions: wire::Permissions(role.permissions.bits()),
        },
    })
}

fn channel_change(channel: &Channel, delete: bool) -> Result<Option<Operation>, ProjectionError> {
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

    let name = channel
        .name
        .clone()
        .ok_or(ProjectionError::IncompleteChannel)?;
    let overwrites = channel
        .permission_overwrites
        .as_deref()
        .ok_or(ProjectionError::IncompleteChannel)?
        .iter()
        .map(|entry| {
            let kind = match entry.kind {
                PermissionOverwriteType::Role => wire::OverwriteKind::Role,
                PermissionOverwriteType::Member => wire::OverwriteKind::Member,
                _ => return Err(ProjectionError::UnsupportedOverwrite),
            };
            Ok(wire::Overwrite {
                kind,
                id: wire_id(entry.id),
                allow: wire::Permissions(entry.allow.bits()),
                deny: wire::Permissions(entry.deny.bits()),
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
