//! Gateway subscription surface: intents + cache resources.
//!
//! Mirrors the two-bot workload enumerated in ADR 0001: member tracking,
//! moderation audit, voice presence (no audio), messages + reactions, invites,
//! interactions. Privileged `MESSAGE_CONTENT` is compiled in but only
//! requested when the deployment justifies it — `gateway_intents` keeps that
//! decision explicit at the call site (cf. two-bot `src/discord/client.ts`
//! `needsMessageContent`: `TWO_AUTOMOD === '1'` or all three ticket env vars
//! present), plus explicitly enabled custom text commands. `GUILD_MEMBERS`
//! is privileged too but legacy requests it unconditionally (join/leave
//! attribution needs it); only `MESSAGE_CONTENT` is gated.

use twilight_cache_inmemory::ResourceType;
use twilight_model::gateway::Intents;

/// Intents for the two-bot workload.
///
/// `GUILD_MEMBERS` is always requested (legacy parity: join/leave
/// attribution needs it). Passing `message_content = true` additionally
/// requests privileged `MESSAGE_CONTENT` — do so only when enabled automod
/// inspects public messages in memory (`TWO_AUTOMOD=1`) or tickets are
/// configured (same all-three-present check as the legacy registration
/// guard), or custom text commands are explicitly enabled
/// (`TWO_AUTOMATIONS=1` and `TWO_TEXT_COMMANDS=1`). Otherwise the gateway
/// never receives message bodies, matching
/// legacy privacy ("we count that a message happened; we never read it").
#[must_use]
pub fn gateway_intents(message_content: bool) -> Intents {
    let base = Intents::GUILDS
        | Intents::GUILD_MEMBERS
        | Intents::GUILD_MODERATION
        | Intents::GUILD_VOICE_STATES
        | Intents::GUILD_MESSAGES
        | Intents::GUILD_MESSAGE_REACTIONS
        | Intents::GUILD_INVITES;

    if message_content {
        base | Intents::MESSAGE_CONTENT
    } else {
        base
    }
}

/// Whether privileged `MESSAGE_CONTENT` is justified, mirroring legacy
/// `needsMessageContent`: enabled automod (`TWO_AUTOMOD === '1'`, exact
/// match) or configured tickets (all three of
/// `DISCORD_TICKET_CATEGORY_ID`, `DISCORD_TICKET_STAFF_ROLE_ID`,
/// `DISCORD_TICKET_PANEL_CHANNEL_ID` present and non-empty).
#[must_use]
pub fn needs_message_content(env_automod: &str, ticket_vars: [&str; 3]) -> bool {
    env_automod == "1" || ticket_vars.iter().all(|v| !v.is_empty())
}

/// Whether custom text commands justify privileged `MESSAGE_CONTENT`.
/// Both `TWO_AUTOMATIONS` and `TWO_TEXT_COMMANDS` must be exactly `"1"`;
/// enabling slash-only automations or text commands without automations
/// does not justify reading message bodies.
#[must_use]
pub fn needs_text_command_message_content(env_automations: &str, env_text_commands: &str) -> bool {
    env_automations == "1" && env_text_commands == "1"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guild_members_always_requested() {
        for message_content in [false, true] {
            let intents = gateway_intents(message_content);
            assert!(intents.contains(Intents::GUILD_MEMBERS));
            assert!(intents.contains(Intents::GUILDS));
            assert!(intents.contains(Intents::GUILD_INVITES));
        }
    }

    #[test]
    fn message_content_gated() {
        assert!(!gateway_intents(false).contains(Intents::MESSAGE_CONTENT));
        assert!(gateway_intents(true).contains(Intents::MESSAGE_CONTENT));
    }

    #[test]
    fn text_commands_require_both_exact_flags() {
        let values = ["", "0", "1", "true", "01", " 1", "1 "];
        for automations in values {
            for text_commands in values {
                let enabled = needs_text_command_message_content(automations, text_commands);
                assert_eq!(
                    enabled,
                    automations == "1" && text_commands == "1",
                    "automations={automations:?}, text_commands={text_commands:?}"
                );
                assert_eq!(
                    gateway_intents(needs_message_content("", ["", "", ""]) || enabled)
                        .contains(Intents::MESSAGE_CONTENT),
                    enabled
                );
            }
        }
    }

    #[test]
    fn legacy_reasons_do_not_require_text_command_flags() {
        for (automations, text_commands) in [("", ""), ("1", "0"), ("0", "1"), ("1", "1")] {
            for (automod, tickets) in [("1", ["", "", ""]), ("0", ["cat", "role", "panel"])] {
                let message_content = needs_message_content(automod, tickets)
                    || needs_text_command_message_content(automations, text_commands);
                assert!(gateway_intents(message_content).contains(Intents::MESSAGE_CONTENT));
            }
        }
    }

    #[test]
    fn needs_message_content_matches_legacy() {
        // Automod on: justified regardless of tickets.
        assert!(needs_message_content("1", ["", "", ""]));
        // Tickets configured (all three present): justified.
        assert!(needs_message_content("0", ["cat", "role", "panel"]));
        // One ticket var missing: not justified.
        assert!(!needs_message_content("0", ["cat", "", "panel"]));
        // Neither: the gateway never receives message bodies.
        assert!(!needs_message_content("0", ["", "", ""]));
        assert!(!needs_message_content("", ["", "", ""]));
    }
}
