//! Gateway subscription surface: intents + cache resources.
//!
//! Mirrors the two-bot workload enumerated in ADR 0001: member tracking,
//! moderation audit, voice presence (no audio), messages + reactions, invites,
//! interactions. Privileged intents (`GUILD_MEMBERS`, `MESSAGE_CONTENT`) are
//! compiled in but only requested when the deployment justifies them —
//! `gateway_intents(privileged: bool)` keeps that decision explicit at the
//! call site (cf. two-bot `src/discord/client.ts`).

use twilight_cache_inmemory::ResourceType;
use twilight_model::gateway::Intents;

/// Intents for the two-bot workload.
///
/// `privileged` gates `GUILD_MEMBERS` + `MESSAGE_CONTENT` (Discord approval
/// required; request only when automod/tickets justify it).
#[must_use]
pub fn gateway_intents(privileged: bool) -> Intents {
    let base = Intents::GUILDS
        | Intents::GUILD_MODERATION
        | Intents::GUILD_VOICE_STATES
        | Intents::GUILD_MESSAGES
        | Intents::GUILD_MESSAGE_REACTIONS
        | Intents::GUILD_INVITES;

    if privileged {
        base | Intents::GUILD_MEMBERS | Intents::MESSAGE_CONTENT
    } else {
        base
    }
}

/// Cache resources matching the subscribed intents: guild/channel/role/member
/// metadata, voice states, and messages for reaction correlation. Everything
/// else stays uncached so RSS is a deliberate choice (ADR 0001: `lite` fit).
#[must_use]
pub fn cache_resource_types() -> ResourceType {
    ResourceType::CHANNEL
        | ResourceType::GUILD
        | ResourceType::MEMBER
        | ResourceType::MESSAGE
        | ResourceType::ROLE
        | ResourceType::VOICE_STATE
}
