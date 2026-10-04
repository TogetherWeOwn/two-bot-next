//! Runtime permission contract for every row of `docs/parity.md` §1.
//!
//! Discord's `default_member_permissions` is a picker default, not an
//! authorization boundary. The router checks resolved invoking-member bits
//! against this table before returning a handler. Target-specific checks stay
//! in moderation policy; dynamic/prefix gates stay in their owning surface.

use crate::commands::{
    PERM_BAN_MEMBERS, PERM_KICK_MEMBERS, PERM_MANAGE_CHANNELS, PERM_MANAGE_EVENTS,
    PERM_MANAGE_GUILD, PERM_MANAGE_MESSAGES, PERM_MODERATE_MEMBERS,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandSurface {
    BuiltinSlash,
    DynamicSlash,
    Prefix,
    Dropped,
}

/// Additional checks that permission bits alone cannot authorize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyHook {
    /// `assert_moderation_allowed`: target presence, self-target refusal,
    /// owner/bot/protected roles, bot hierarchy and actor hierarchy.
    MemberModeration,
    /// Custom slash commands require enabled automations and an enabled row.
    AutomationsEnabled,
    /// Prefix triggers additionally require text commands and exclude builtins.
    TextCommandsEnabled,
    /// Legacy rota only; not published or dispatched by Next.
    ConfiguredPrimaryActor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandPermission {
    pub parity_row: u8,
    /// Next name; RSVP totals intentionally use `rsvp-attendance` (row 25).
    pub command: &'static str,
    pub surface: CommandSurface,
    /// Zero means everyone; absent resolved permissions deny nonzero masks.
    pub required_permissions: u64,
    pub policy_hook: Option<PolicyHook>,
}

impl CommandPermission {
    #[must_use]
    pub fn allows(self, actor_permissions: Option<u64>) -> bool {
        self.required_permissions == 0
            || actor_permissions
                .is_some_and(|bits| bits & self.required_permissions == self.required_permissions)
    }
}

const fn row(
    parity_row: u8,
    command: &'static str,
    surface: CommandSurface,
    required_permissions: u64,
    policy_hook: Option<PolicyHook>,
) -> CommandPermission {
    CommandPermission {
        parity_row,
        command,
        surface,
        required_permissions,
        policy_hook,
    }
}

use CommandSurface::{BuiltinSlash, Dropped, DynamicSlash, Prefix};
use PolicyHook::{
    AutomationsEnabled, ConfiguredPrimaryActor, MemberModeration, TextCommandsEnabled,
};

/// All 30 parity rows, including the three non-static-slash dispositions,
/// plus the Next-only `/help` discovery command (parity row 31 — legacy has
/// no help command).
/// No Administrator-only exception: preserve the legacy resolved-bit check.
pub const COMMAND_PERMISSIONS: [CommandPermission; 31] = [
    row(1, "rank", BuiltinSlash, 0, None),
    row(2, "leaderboard", BuiltinSlash, 0, None),
    row(31, "help", BuiltinSlash, 0, None),
    row(
        3,
        "ban",
        BuiltinSlash,
        PERM_BAN_MEMBERS,
        Some(MemberModeration),
    ),
    row(
        4,
        "tempban",
        BuiltinSlash,
        PERM_BAN_MEMBERS,
        Some(MemberModeration),
    ),
    row(
        5,
        "kick",
        BuiltinSlash,
        PERM_KICK_MEMBERS,
        Some(MemberModeration),
    ),
    row(
        6,
        "timeout",
        BuiltinSlash,
        PERM_MODERATE_MEMBERS,
        Some(MemberModeration),
    ),
    row(
        7,
        "warn",
        BuiltinSlash,
        PERM_MODERATE_MEMBERS,
        Some(MemberModeration),
    ),
    row(8, "purge", BuiltinSlash, PERM_MANAGE_MESSAGES, None),
    row(9, "slowmode", BuiltinSlash, PERM_MANAGE_CHANNELS, None),
    row(10, "lockdown", BuiltinSlash, PERM_MANAGE_CHANNELS, None),
    row(11, "unlock", BuiltinSlash, PERM_MANAGE_CHANNELS, None),
    row(12, "attendance", BuiltinSlash, PERM_MANAGE_EVENTS, None),
    row(
        13,
        "rota-acknowledge",
        Dropped,
        PERM_MANAGE_GUILD,
        Some(ConfiguredPrimaryActor),
    ),
    row(14, "command", BuiltinSlash, PERM_MANAGE_GUILD, None),
    row(15, "command-remove", BuiltinSlash, PERM_MANAGE_GUILD, None),
    row(16, "command-list", BuiltinSlash, PERM_MANAGE_GUILD, None),
    row(17, "schedule", BuiltinSlash, PERM_MANAGE_GUILD, None),
    row(18, "schedule-remove", BuiltinSlash, PERM_MANAGE_GUILD, None),
    row(19, "schedule-list", BuiltinSlash, PERM_MANAGE_GUILD, None),
    row(20, "sticky", BuiltinSlash, PERM_MANAGE_GUILD, None),
    row(21, "sticky-remove", BuiltinSlash, PERM_MANAGE_GUILD, None),
    row(22, "<custom>", DynamicSlash, 0, Some(AutomationsEnabled)),
    row(23, "<trigger>", Prefix, 0, Some(TextCommandsEnabled)),
    row(24, "rsvp", BuiltinSlash, 0, None),
    row(25, "rsvp-attendance", BuiltinSlash, 0, None),
    row(26, "lfg", BuiltinSlash, PERM_MANAGE_EVENTS, None),
    row(27, "lfg-close", BuiltinSlash, PERM_MANAGE_EVENTS, None),
    row(28, "feed-add", BuiltinSlash, PERM_MANAGE_GUILD, None),
    row(29, "feed-remove", BuiltinSlash, PERM_MANAGE_GUILD, None),
    row(30, "feed-list", BuiltinSlash, PERM_MANAGE_GUILD, None),
];

/// Only retained builtins participate in slash-name lookup. The dropped and
/// placeholder rows must never reserve a real custom command name.
#[must_use]
pub fn command_permission(name: &str) -> Option<&'static CommandPermission> {
    COMMAND_PERMISSIONS
        .iter()
        .find(|row| row.surface == BuiltinSlash && row.command == name)
}
