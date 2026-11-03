//! Self-only `voice.move_member` decision: pure room-state evaluation.
//!
//! The signed `POST /internal/actions` receiver serves this verb by gathering
//! live facts (REST guild/channel/member snapshots plus the shared gateway
//! voice-state cache) and running [`decide`] over them. Discord exposes no
//! REST voice-state list, so the gateway cache is the only source for who is
//! where; the supervisor's readiness gate runs before any of this, so a warm
//! cache while the gateway is down never reaches the decision — the handler
//! answers `discord_unavailable` instead of guessing `not_connected`.
//!
//! [`decide`] is hermetic: every input is a caller-supplied scalar, and the
//! only outputs are the closed [`VoiceMoveOutcome`] words. Anything the facts
//! cannot prove (unknown role, malformed override, mid-mutation cache gap)
//! returns [`VoiceMoveDecision::Incomplete`], which the caller fails closed
//! into `discord_unavailable` — never a guess, never a bypass. Permission
//! evaluation follows the Discord overwrite order (guild base, @everyone
//! overwrite, combined role overwrites, member overwrite) using the same
//! [`PERM_*`](two_bot_core::voice_permissions) bits as the room builder, so
//! the member's own Connect on the target is computed bot-side from live
//! role/overwrite state: the move can never become a private-room bypass.
//! The guild owner and `ADMINISTRATOR` holders pass every check, exactly like
//! the moderation resolver.

use std::sync::Arc;

use twilight_cache_inmemory::DefaultInMemoryCache;
use two_bot_core::voice_permissions::{
    ChannelOverride, OverrideKind, PERM_ADMINISTRATOR, PERM_CONNECT, PERM_MOVE_MEMBERS,
    PERM_VIEW_CHANNEL,
};
use two_bot_core::{internal_actions::VoiceMoveOutcome, Snowflake};

/// Bot-side grants required on the source and the target room: the bot must be
/// able to see both rooms, join them, and move members between them.
const BOT_ROOM_BITS: u64 = PERM_MOVE_MEMBERS | PERM_VIEW_CHANNEL | PERM_CONNECT;

/// One guild role with the fields permission evaluation needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GuildRoleFact {
    pub id: Snowflake,
    pub position: i64,
    pub permissions: u64,
}

/// One channel with the fields the decision needs: kind (voice or not),
/// whether it is the guild AFK channel, the join cap (`0` is unlimited), and
/// the live permission overwrites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChannelFact {
    pub id: Snowflake,
    pub is_voice: bool,
    pub is_afk: bool,
    pub user_limit: u32,
    pub overwrites: Vec<ChannelOverride>,
}

/// Everything [`decide`] needs, gathered live per move. No cache handles, no
/// Discord types, no bodies: plain scalars the handler assembles from the
/// REST snapshots and one bounded cache read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VoiceMoveFacts {
    pub guild_id: Snowflake,
    pub guild_owner_id: Snowflake,
    pub guild_roles: Vec<GuildRoleFact>,
    pub target: ChannelFact,
    pub source: ChannelFact,
    /// The member's current voice channel from the gateway cache, or `None`
    /// when they have no voice state in the guild.
    pub member_channel_id: Option<Snowflake>,
    /// Voice-state count in the target channel (bots included: Discord
    /// enforces `user_limit` against every occupant, so the bot must too).
    pub target_occupants: u32,
    pub member_id: Snowflake,
    pub member_roles: Vec<Snowflake>,
    pub bot_id: Snowflake,
    pub bot_roles: Vec<Snowflake>,
}

/// The decision over one live fact set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VoiceMoveDecision {
    /// Every check passed: PATCH the member into the target room from here.
    Proceed { from_channel_id: Snowflake },
    /// Answer this domain word with HTTP 200, no Discord call.
    Refuse(VoiceMoveOutcome),
    /// The facts cannot prove the answer (unknown role, malformed override,
    /// cache gap): fail closed into `discord_unavailable`, never proceed.
    Incomplete,
}

/// Evaluate the contract's validation order over one live fact set: target is
/// a usable voice room, member connected, member's own Connect on the target,
/// room cap, bot's grants on source and target. `already_in_room` short-circuits
/// before any permission math: nothing moves, so nothing needs proving.
pub(crate) fn decide(facts: &VoiceMoveFacts) -> VoiceMoveDecision {
    if !facts.target.is_voice || facts.target.is_afk {
        return VoiceMoveDecision::Refuse(VoiceMoveOutcome::RoomGone);
    }
    let Some(current) = facts.member_channel_id else {
        return VoiceMoveDecision::Refuse(VoiceMoveOutcome::NotConnected);
    };
    if current == facts.target.id {
        return VoiceMoveDecision::Refuse(VoiceMoveOutcome::AlreadyInRoom);
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
    if member_perms & PERM_CONNECT == 0 {
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

/// Effective channel permission for one user: guild base (@everyone plus held
/// roles), then the @everyone overwrite, the combined role overwrites, then
/// the member overwrite. The owner and `ADMINISTRATOR` holders get everything.
/// `None` when the snapshot cannot prove the answer — an unknown held role or
/// a missing @everyone row — so the caller fails closed instead of guessing.
fn effective_perms(
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

/// Parse one Discord channel-overwrite entry (`id`/`type`/`allow`/`deny`, with
/// bitfields as decimal strings). `None` fails the fact set closed: an
/// overwrite the bot cannot read is never treated as no overwrite.
pub(crate) fn parse_override(value: &serde_json::Value) -> Option<ChannelOverride> {
    let id: Snowflake = value.get("id")?.as_str()?.parse().ok()?;
    if id == 0 {
        return None;
    }
    let kind = match value.get("type")?.as_u64()? {
        0 => OverrideKind::Role,
        1 => OverrideKind::Member,
        _ => return None,
    };
    let allow: u64 = value.get("allow")?.as_str()?.parse().ok()?;
    let deny: u64 = value.get("deny")?.as_str()?.parse().ok()?;
    Some(ChannelOverride {
        id,
        kind,
        allow,
        deny,
    })
}

/// Read the member's current voice channel and the target room's occupant
/// count from the shared gateway cache. `Ok(None, 0)` when the cache holds no
/// voice-state set for the guild: for a connected gateway Twilight drops the
/// set when the last member leaves, so a missing set is an empty guild, not a
/// gap. `Err(())` only on a mid-mutation race (a listed state failing lookup),
/// which the caller fails closed into `discord_unavailable`.
pub(crate) fn occupancy(
    cache: &Arc<DefaultInMemoryCache>,
    guild_id: Snowflake,
    member_id: Snowflake,
    target_id: Snowflake,
) -> Result<(Option<Snowflake>, u32), ()> {
    use twilight_model::id::{
        marker::{GuildMarker, UserMarker},
        Id,
    };
    let guild: Id<GuildMarker> = Id::new(guild_id);
    let users: Vec<Id<UserMarker>> = match cache.guild_voice_states(guild) {
        Some(users) => users.iter().copied().collect(),
        None => return Ok((None, 0)),
    };
    let mut current = None;
    let mut occupants = 0u32;
    for user in &users {
        let state = cache.voice_state(*user, guild).ok_or(())?;
        if state.channel_id().get() == target_id {
            occupants = occupants.saturating_add(1);
        }
        if user.get() == member_id {
            current = Some(state.channel_id().get());
        }
    }
    Ok((current, occupants))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUILD: Snowflake = 100;
    const MEMBER: Snowflake = 7;
    const BOT: Snowflake = 9;
    const SOURCE: Snowflake = 10;
    const TARGET: Snowflake = 11;

    fn roles() -> Vec<GuildRoleFact> {
        vec![
            GuildRoleFact {
                id: GUILD,
                position: 0,
                permissions: PERM_VIEW_CHANNEL | PERM_CONNECT,
            },
            GuildRoleFact {
                id: 20,
                position: 3,
                permissions: PERM_VIEW_CHANNEL | PERM_CONNECT | PERM_MOVE_MEMBERS,
            },
        ]
    }

    fn open_room(id: Snowflake) -> ChannelFact {
        ChannelFact {
            id,
            is_voice: true,
            is_afk: false,
            user_limit: 0,
            overwrites: Vec::new(),
        }
    }

    fn base_facts() -> VoiceMoveFacts {
        VoiceMoveFacts {
            guild_id: GUILD,
            guild_owner_id: 1,
            guild_roles: roles(),
            target: open_room(TARGET),
            source: open_room(SOURCE),
            member_channel_id: Some(SOURCE),
            target_occupants: 0,
            member_id: MEMBER,
            member_roles: vec![20],
            bot_id: BOT,
            bot_roles: vec![20],
        }
    }

    #[test]
    fn open_move_proceeds_from_the_current_room() {
        assert_eq!(
            decide(&base_facts()),
            VoiceMoveDecision::Proceed {
                from_channel_id: SOURCE
            }
        );
    }

    #[test]
    fn non_voice_afk_and_missing_targets_are_room_gone() {
        for target in [
            ChannelFact {
                is_voice: false,
                ..open_room(TARGET)
            },
            ChannelFact {
                is_afk: true,
                ..open_room(TARGET)
            },
        ] {
            let mut facts = base_facts();
            facts.target = target;
            assert_eq!(
                decide(&facts),
                VoiceMoveDecision::Refuse(VoiceMoveOutcome::RoomGone)
            );
        }
    }

    #[test]
    fn disconnected_member_is_not_connected() {
        let mut facts = base_facts();
        facts.member_channel_id = None;
        assert_eq!(
            decide(&facts),
            VoiceMoveDecision::Refuse(VoiceMoveOutcome::NotConnected)
        );
    }

    #[test]
    fn already_in_target_short_circuits_before_permission_math() {
        let mut facts = base_facts();
        facts.member_channel_id = Some(TARGET);
        // Even with empty roles that could not prove Connect, the no-op wins:
        // nothing moves, so nothing needs proving.
        facts.member_roles = Vec::new();
        facts.guild_roles = Vec::new();
        assert_eq!(
            decide(&facts),
            VoiceMoveDecision::Refuse(VoiceMoveOutcome::AlreadyInRoom)
        );
    }

    #[test]
    fn connect_denied_room_is_not_permitted() {
        let mut facts = base_facts();
        facts.target.overwrites = vec![ChannelOverride {
            id: 20,
            kind: OverrideKind::Role,
            allow: 0,
            deny: PERM_CONNECT,
        }];
        assert_eq!(
            decide(&facts),
            VoiceMoveDecision::Refuse(VoiceMoveOutcome::NotPermitted)
        );
    }

    #[test]
    fn private_room_member_overwrite_restores_connect() {
        let mut facts = base_facts();
        // The bot is allow-listed like in a real private room: the @everyone
        // deny strips its base Connect, so its own member overwrite carries
        // the grant back. First so the sanity pop below drops the member's.
        facts.target.overwrites = vec![
            ChannelOverride {
                id: BOT,
                kind: OverrideKind::Member,
                allow: PERM_CONNECT,
                deny: 0,
            },
            ChannelOverride {
                id: GUILD,
                kind: OverrideKind::Role,
                allow: 0,
                deny: PERM_CONNECT,
            },
            ChannelOverride {
                id: MEMBER,
                kind: OverrideKind::Member,
                allow: PERM_CONNECT,
                deny: 0,
            },
        ];
        assert_eq!(
            decide(&facts),
            VoiceMoveDecision::Proceed {
                from_channel_id: SOURCE
            }
        );
        // Sanity: without the member overwrite the @everyone deny holds.
        facts.target.overwrites.pop();
        assert_eq!(
            decide(&facts),
            VoiceMoveDecision::Refuse(VoiceMoveOutcome::NotPermitted)
        );
    }

    #[test]
    fn overwrite_order_in_the_array_does_not_matter() {
        // Member overwrite first, role deny second: the member grant still
        // wins, because application follows Discord's fixed pass order.
        let mut facts = base_facts();
        facts.target.overwrites = vec![
            ChannelOverride {
                id: MEMBER,
                kind: OverrideKind::Member,
                allow: PERM_CONNECT,
                deny: 0,
            },
            ChannelOverride {
                id: 20,
                kind: OverrideKind::Role,
                allow: 0,
                deny: PERM_CONNECT,
            },
            // The bot's own allow-listing rides the member pass like above.
            ChannelOverride {
                id: BOT,
                kind: OverrideKind::Member,
                allow: PERM_CONNECT,
                deny: 0,
            },
        ];
        assert_eq!(
            decide(&facts),
            VoiceMoveDecision::Proceed {
                from_channel_id: SOURCE
            }
        );
    }

    #[test]
    fn full_room_refuses_before_bot_checks() {
        let mut facts = base_facts();
        facts.target.user_limit = 2;
        facts.target_occupants = 2;
        assert_eq!(
            decide(&facts),
            VoiceMoveDecision::Refuse(VoiceMoveOutcome::RoomFull)
        );
        // One free seat proceeds; zero limit is unlimited.
        facts.target_occupants = 1;
        assert_eq!(
            decide(&facts),
            VoiceMoveDecision::Proceed {
                from_channel_id: SOURCE
            }
        );
    }

    #[test]
    fn bot_without_move_members_is_refused() {
        // The bot holds only @everyone (View + Connect, no Move Members) on
        // both rooms: the member's own Connect proves, so the bot check fires.
        let mut facts = base_facts();
        facts.bot_roles = Vec::new();
        assert_eq!(
            decide(&facts),
            VoiceMoveDecision::Refuse(VoiceMoveOutcome::BotNotPermitted)
        );
    }

    #[test]
    fn bot_blind_on_the_source_room_is_refused() {
        // A View deny on the source strips the bot's sight there while the
        // member's Connect on the target still proves.
        let mut facts = base_facts();
        facts.bot_roles = Vec::new();
        facts.source.overwrites = vec![ChannelOverride {
            id: GUILD,
            kind: OverrideKind::Role,
            allow: 0,
            deny: PERM_VIEW_CHANNEL,
        }];
        assert_eq!(
            decide(&facts),
            VoiceMoveDecision::Refuse(VoiceMoveOutcome::BotNotPermitted)
        );
    }

    #[test]
    fn unknown_member_role_is_incomplete_not_a_bypass() {
        let mut facts = base_facts();
        facts.member_roles = vec![999];
        assert_eq!(decide(&facts), VoiceMoveDecision::Incomplete);
    }

    #[test]
    fn owner_and_admin_pass_everything() {
        let mut facts = base_facts();
        facts.member_id = 1; // guild owner
        facts.member_roles = Vec::new();
        // The owner bypasses the member check, but the bot still needs its
        // own Connect grant on the denied target to earn the move.
        facts.target.overwrites = vec![
            ChannelOverride {
                id: GUILD,
                kind: OverrideKind::Role,
                allow: 0,
                deny: PERM_CONNECT,
            },
            ChannelOverride {
                id: BOT,
                kind: OverrideKind::Member,
                allow: PERM_CONNECT,
                deny: 0,
            },
        ];
        assert_eq!(
            decide(&facts),
            VoiceMoveDecision::Proceed {
                from_channel_id: SOURCE
            }
        );
        let mut facts = base_facts();
        facts.guild_roles.push(GuildRoleFact {
            id: 30,
            position: 9,
            permissions: PERM_ADMINISTRATOR,
        });
        facts.member_roles = vec![30];
        assert_eq!(
            decide(&facts),
            VoiceMoveDecision::Proceed {
                from_channel_id: SOURCE
            }
        );
    }

    #[test]
    fn override_parsing_rejects_shapes_it_cannot_prove() {
        let good = serde_json::json!({"id": "20", "type": 0, "allow": "1048576", "deny": "0"});
        assert_eq!(
            parse_override(&good),
            Some(ChannelOverride {
                id: 20,
                kind: OverrideKind::Role,
                allow: PERM_CONNECT,
                deny: 0,
            })
        );
        for bad in [
            serde_json::json!({"id": "0", "type": 0, "allow": "0", "deny": "0"}),
            serde_json::json!({"id": "20", "type": 7, "allow": "0", "deny": "0"}),
            serde_json::json!({"id": "nope", "type": 0, "allow": "0", "deny": "0"}),
            serde_json::json!({"id": "20", "type": 1, "allow": "1048576"}),
            serde_json::json!({"id": "20", "type": 0, "allow": "0x10", "deny": "0"}),
        ] {
            assert_eq!(parse_override(&bad), None);
        }
        // Member-kind override parses to the member target.
        let member = serde_json::json!({"id": "7", "type": 1, "allow": "0", "deny": "1024"});
        assert_eq!(
            parse_override(&member),
            Some(ChannelOverride {
                id: 7,
                kind: OverrideKind::Member,
                allow: 0,
                deny: PERM_VIEW_CHANNEL,
            })
        );
    }
}
