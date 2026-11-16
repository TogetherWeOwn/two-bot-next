//! Inert voice-move permission facts; this module performs no effects.

use crate::voice_permissions::{ChannelOverride, OverrideKind, PERM_ADMINISTRATOR};
use crate::Snowflake;

/// One guild role with the fields permission evaluation needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuildRoleFact {
    pub id: Snowflake,
    pub position: i64,
    pub permissions: u64,
}

/// Effective channel permission for one user: guild base (@everyone plus held
/// roles), then the @everyone overwrite, the combined role overwrites, then
/// the member overwrite. The owner and `ADMINISTRATOR` holders get everything.
/// `None` when the snapshot cannot prove the answer — an unknown held role or
/// a missing @everyone row — so the caller fails closed instead of guessing.
pub fn effective_perms(
    guild_roles: &[GuildRoleFact],
    overwrites: &[ChannelOverride],
    guild_id: Snowflake,
    owner_id: Snowflake,
    user_id: Snowflake,
    held: &[Snowflake],
) -> Option<u64> {
    if user_id == owner_id {
        return Some(u64::MAX);
    }
    let everyone = guild_roles.iter().find(|role| role.id == guild_id)?;
    let mut base = everyone.permissions;
    for role_id in held {
        // @everyone rides along implicitly: members always hold the guild id.
        if *role_id == guild_id {
            continue;
        }
        base |= guild_roles
            .iter()
            .find(|role| role.id == *role_id)?
            .permissions;
    }
    if base & PERM_ADMINISTRATOR != 0 {
        return Some(u64::MAX);
    }
    // Three passes in Discord's fixed order, independent of array order:
    // @everyone, the combined role overwrites, then the member overwrite.
    // A member overwrite for someone else is irrelevant, never an error.
    for overwrite in overwrites {
        if overwrite.id == guild_id && overwrite.kind == OverrideKind::Role {
            base = (base & !overwrite.deny) | overwrite.allow;
        }
    }
    let mut allow = 0u64;
    let mut deny = 0u64;
    for overwrite in overwrites {
        if overwrite.kind == OverrideKind::Role
            && overwrite.id != guild_id
            && held.contains(&overwrite.id)
        {
            allow |= overwrite.allow;
            deny |= overwrite.deny;
        }
    }
    base = (base & !deny) | allow;
    for overwrite in overwrites {
        if overwrite.kind == OverrideKind::Member && overwrite.id == user_id {
            base = (base & !overwrite.deny) | overwrite.allow;
        }
    }
    Some(base)
}

#[cfg(test)]
mod tests;
