//! Inert voice-move decision over caller-supplied facts; performs no effects.

use crate::internal_actions::VoiceMoveOutcome;
use crate::voice_move_facts::{effective_perms, GuildRoleFact};
use crate::voice_permissions::{
    ChannelOverride, PERM_ADMINISTRATOR, PERM_CONNECT, PERM_MOVE_MEMBERS, PERM_VIEW_CHANNEL,
};
use crate::Snowflake;

const BOT_ROOM_BITS: u64 = PERM_MOVE_MEMBERS | PERM_VIEW_CHANNEL | PERM_CONNECT;

/// A room's kind, capacity and permission overwrites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelFact {
    pub id: Snowflake,
    pub is_voice: bool,
    pub is_afk: bool,
    pub user_limit: u32,
    pub overwrites: Vec<ChannelOverride>,
}

/// The complete scalar snapshot required for one decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceMoveFacts {
    pub guild_id: Snowflake,
    pub guild_owner_id: Snowflake,
    pub guild_roles: Vec<GuildRoleFact>,
    pub target: ChannelFact,
    pub source: ChannelFact,
    pub member_channel_id: Option<Snowflake>,
    pub target_occupants: u32,
    pub member_id: Snowflake,
    pub member_roles: Vec<Snowflake>,
    pub member_timed_out: bool,
    pub bot_id: Snowflake,
    pub bot_roles: Vec<Snowflake>,
}

/// A proven decision, or incomplete facts that must fail closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceMoveDecision {
    Proceed { from_channel_id: Snowflake },
    Refuse(VoiceMoveOutcome),
    Incomplete,
}

/// Evaluate target, connection, member permissions, capacity and bot permissions in order.
pub fn decide(facts: &VoiceMoveFacts) -> VoiceMoveDecision {
    if !facts.target.is_voice || facts.target.is_afk {
        return VoiceMoveDecision::Refuse(VoiceMoveOutcome::RoomGone);
    }
    let Some(current) = facts.member_channel_id else {
        return VoiceMoveDecision::Refuse(VoiceMoveOutcome::NotConnected);
    };
    if current == facts.target.id {
        return VoiceMoveDecision::Refuse(VoiceMoveOutcome::AlreadyInRoom);
    }
    if facts.source.id != current {
        return VoiceMoveDecision::Incomplete;
    }
    let member_perms = match effective_perms(
        &facts.guild_roles,
        &facts.target.overwrites,
        facts.guild_id,
        facts.guild_owner_id,
        facts.member_id,
        &facts.member_roles,
    ) {
        Some(perms) => perms,
        None => return VoiceMoveDecision::Incomplete,
    };
    let member_room_bits = PERM_VIEW_CHANNEL | PERM_CONNECT;
    // Timeout removes Connect after overwrites; owner/admin are exempt.
    if (facts.member_timed_out && member_perms & PERM_ADMINISTRATOR == 0)
        || member_perms & member_room_bits != member_room_bits
    {
        return VoiceMoveDecision::Refuse(VoiceMoveOutcome::NotPermitted);
    }
    if facts.target.user_limit > 0 && facts.target_occupants >= facts.target.user_limit {
        return VoiceMoveDecision::Refuse(VoiceMoveOutcome::RoomFull);
    }
    for room in [&facts.source, &facts.target] {
        let bot_perms = match effective_perms(
            &facts.guild_roles,
            &room.overwrites,
            facts.guild_id,
            facts.guild_owner_id,
            facts.bot_id,
            &facts.bot_roles,
        ) {
            Some(perms) => perms,
            None => return VoiceMoveDecision::Incomplete,
        };
        if bot_perms & BOT_ROOM_BITS != BOT_ROOM_BITS {
            return VoiceMoveDecision::Refuse(VoiceMoveOutcome::BotNotPermitted);
        }
    }
    VoiceMoveDecision::Proceed {
        from_channel_id: current,
    }
}

#[cfg(test)]
mod tests;
