//! Redacted scheduled-event permission evidence from the shared gateway cache.

use std::collections::HashSet;

use serde::Serialize;
use twilight_cache_inmemory::DefaultInMemoryCache;
use twilight_model::{
    channel::{permission_overwrite::PermissionOverwriteType, ChannelType},
    guild::Permissions,
    id::{
        marker::{GuildMarker, UserMarker},
        Id,
    },
};

use crate::{voice_rooms::effective_permissions, SharedCache};

/// Permission bits for one guild and its eligible channel types. The sample
/// carries only booleans and counts, even when overwrites name other users.
#[derive(Serialize)]
pub struct EventPermissionEvidence {
    owner_or_admin: bool,
    guild_create_events: bool,
    guild_manage_events: bool,
    voice: ChannelCounts,
    stage: ChannelCounts,
}

impl SharedCache {
    /// Read one complete snapshot for the expected gateway bot. The caller
    /// must also fence its gateway connection state across this read.
    pub fn event_permission_evidence(
        &self,
        guild_id: Id<GuildMarker>,
        bot_id: Id<UserMarker>,
    ) -> Option<EventPermissionEvidence> {
        self.with_guild_snapshot(guild_id.get(), |cache| {
            read_permissions(cache, guild_id, bot_id)
        })?
    }
}

#[derive(Default, Serialize)]
struct ChannelCounts {
    channels: usize,
    create_events: usize,
    manage_events: usize,
    either_event_permission: usize,
}

fn read_permissions(
    cache: &DefaultInMemoryCache,
    guild_id: Id<GuildMarker>,
    bot_id: Id<UserMarker>,
) -> Option<EventPermissionEvidence> {
    let current_user = cache.current_user()?;
    if !current_user.bot || current_user.id != bot_id {
        return None;
    }
    let guild = cache.guild(guild_id)?;
    if guild.unavailable() == Some(true) || !cache.guild_members(guild_id)?.contains(&bot_id) {
        return None;
    }
    let member = cache.member(guild_id, bot_id)?;
    if member
        .communication_disabled_until()
        .is_some_and(|until| until.as_secs() > time::OffsetDateTime::now_utc().unix_timestamp())
    {
        return None;
    }
    let held = member.roles();
    if held.len() > 250 {
        return None;
    }
    let everyone = guild_id.cast();
    let role_ids = cache.guild_roles(guild_id)?;
    let roles = std::iter::once(everyone)
        .chain(held.iter().copied())
        .map(|id| {
            let role = cache.role(id)?;
            (role_ids.contains(&id) && role.guild_id() == guild_id && role.id == id)
                .then(|| role.resource().clone())
        })
        .collect::<Option<Vec<_>>>()?;
    let resolve = |overwrites: &[_]| {
        effective_permissions(
            guild_id.get(),
            guild.owner_id().get(),
            bot_id.get(),
            held,
            &roles,
            overwrites,
        )
    };
    let base = resolve(&[])?;
    let mut voice = ChannelCounts::default();
    let mut stage = ChannelCounts::default();
    let channels = cache.guild_channels(guild_id)?;
    // Discord's guild channel limit plus active threads, which the cache also indexes.
    if channels.len() > 2048 {
        return None;
    }
    for id in channels.iter() {
        let channel = cache.channel(*id)?;
        if channel.guild_id != Some(guild_id) || !channel.kind.is_guild() {
            return None;
        }
        let counts = match channel.kind {
            ChannelType::GuildVoice => &mut voice,
            ChannelType::GuildStageVoice => &mut stage,
            _ => continue,
        };
        let overwrites = channel.permission_overwrites.as_deref()?;
        let mut seen = HashSet::new();
        if overwrites.len() > 1024
            || overwrites.iter().any(|overwrite| {
                !matches!(
                    overwrite.kind,
                    PermissionOverwriteType::Role | PermissionOverwriteType::Member
                ) || !seen.insert((overwrite.kind, overwrite.id))
            })
        {
            return None;
        }
        let bits = resolve(overwrites)?;
        counts.channels += 1;
        counts.create_events += usize::from(bits.contains(Permissions::CREATE_EVENTS));
        counts.manage_events += usize::from(bits.contains(Permissions::MANAGE_EVENTS));
        counts.either_event_permission +=
            usize::from(bits.intersects(Permissions::CREATE_EVENTS | Permissions::MANAGE_EVENTS));
    }
    Some(EventPermissionEvidence {
        owner_or_admin: base.contains(Permissions::ADMINISTRATOR),
        guild_create_events: base.contains(Permissions::CREATE_EVENTS),
        guild_manage_events: base.contains(Permissions::MANAGE_EVENTS),
        voice,
        stage,
    })
}

#[cfg(test)]
mod tests;
