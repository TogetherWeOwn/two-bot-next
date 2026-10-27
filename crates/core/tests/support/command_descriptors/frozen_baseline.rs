//! Frozen, hand-written oracle for the built-in slash-command contract.
//!
//! Every value below was transcribed from the code that ships today
//! (`router.rs` dispatch arms, `command_permissions.rs`, `help.rs` groups,
//! `e2e_matrix.rs`) and is deliberately NOT derived from any registry or
//! descriptor: a descriptor-derived expectation would let a migration bless
//! its own mistakes. Permission bits are written as literal Discord bit
//! positions, not through the `PERM_*` constants.
//!
//! Scope: the 43 non-voice built-in slash commands. A command added later
//! does not belong here; the consumers of this table compare only the rows
//! they list and ignore everything else, so unrelated additions never break
//! it. Changing a row is a behavior change and needs its own justification.
//!
//! Shared by `tests/command_registry_baseline_acceptance.rs` (runs today
//! against the legacy router) and by the future-API contract files in this
//! directory (they must reproduce the very same rows from descriptors).

#![allow(dead_code)]

/// Which runtime gate a built-in command needs (`RouterGates` field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    Always,
    Scorecard,
    Automations,
    Announcements,
    Moderation,
}

/// One frozen row. `handler`, `denial` and `policy_hook` hold the `Debug`
/// spelling of the corresponding enum value so this file needs no crate import.
#[derive(Debug, Clone, Copy)]
pub struct Frozen {
    pub name: &'static str,
    pub parity_row: u8,
    pub handler: &'static str,
    pub gate: Gate,
    /// Required invoking-member permission bits; 0 = everyone.
    pub mask: u64,
    /// `RouterRefusal` returned when the bits are missing; `None` when open.
    pub denial: Option<&'static str>,
    pub policy_hook: Option<&'static str>,
}

pub const MANAGE_GUILD: u64 = 1 << 5;
pub const MANAGE_EVENTS: u64 = 1 << 33;
pub const BAN_MEMBERS: u64 = 1 << 2;
pub const KICK_MEMBERS: u64 = 1 << 1;
pub const MODERATE_MEMBERS: u64 = 1 << 40;
pub const MANAGE_MESSAGES: u64 = 1 << 13;
pub const MANAGE_CHANNELS: u64 = 1 << 4;
pub const ADMINISTRATOR: u64 = 1 << 3;

const fn r(
    name: &'static str,
    parity_row: u8,
    handler: &'static str,
    gate: Gate,
    mask: u64,
    denial: Option<&'static str>,
    policy_hook: Option<&'static str>,
) -> Frozen {
    Frozen {
        name,
        parity_row,
        handler,
        gate,
        mask,
        denial,
        policy_hook,
    }
}

const GUILD_DENIAL: Option<&str> = Some("ManageServerRequired");
const EVENTS_DENIAL: Option<&str> = Some("ManageEventsRequired");
const MEMBER_POLICY: Option<&str> = Some("MemberModeration");

/// The 43 rows in the legacy publish order with every gate on and voice off
/// (core, scorecard, automation, announcement, moderation).
pub const FROZEN: [Frozen; 43] = [
    r("rank", 1, "Rank", Gate::Always, 0, None, None),
    r("leaderboard", 2, "Leaderboard", Gate::Always, 0, None, None),
    r("help", 31, "Help", Gate::Always, 0, None, None),
    r("privacy", 32, "Privacy", Gate::Always, 0, None, None),
    r("reopen", 34, "Reopen", Gate::Always, 0, None, None),
    r(
        "welcome-preview",
        42,
        "WelcomePreview",
        Gate::Always,
        MANAGE_GUILD,
        GUILD_DENIAL,
        None,
    ),
    r(
        "game-mix",
        43,
        "GameMix",
        Gate::Always,
        MANAGE_GUILD,
        GUILD_DENIAL,
        None,
    ),
    r(
        "attendance",
        12,
        "ScorecardAttendance",
        Gate::Scorecard,
        MANAGE_EVENTS,
        EVENTS_DENIAL,
        None,
    ),
    r(
        "command",
        14,
        "AutomationAdmin",
        Gate::Automations,
        MANAGE_GUILD,
        GUILD_DENIAL,
        None,
    ),
    r(
        "command-remove",
        15,
        "AutomationAdmin",
        Gate::Automations,
        MANAGE_GUILD,
        GUILD_DENIAL,
        None,
    ),
    r(
        "command-list",
        16,
        "AutomationAdmin",
        Gate::Automations,
        MANAGE_GUILD,
        GUILD_DENIAL,
        None,
    ),
    r(
        "command-inspect",
        33,
        "AutomationAdmin",
        Gate::Automations,
        MANAGE_GUILD,
        GUILD_DENIAL,
        None,
    ),
    r(
        "command-disable",
        40,
        "AutomationAdmin",
        Gate::Automations,
        MANAGE_GUILD,
        GUILD_DENIAL,
        None,
    ),
    r(
        "command-enable",
        41,
        "AutomationAdmin",
        Gate::Automations,
        MANAGE_GUILD,
        GUILD_DENIAL,
        None,
    ),
    r(
        "schedule",
        17,
        "AutomationAdmin",
        Gate::Automations,
        MANAGE_GUILD,
        GUILD_DENIAL,
        None,
    ),
    r(
        "schedule-remove",
        18,
        "AutomationAdmin",
        Gate::Automations,
        MANAGE_GUILD,
        GUILD_DENIAL,
        None,
    ),
    r(
        "schedule-list",
        19,
        "AutomationAdmin",
        Gate::Automations,
        MANAGE_GUILD,
        GUILD_DENIAL,
        None,
    ),
    r(
        "sticky",
        20,
        "AutomationAdmin",
        Gate::Automations,
        MANAGE_GUILD,
        GUILD_DENIAL,
        None,
    ),
    r(
        "sticky-remove",
        21,
        "AutomationAdmin",
        Gate::Automations,
        MANAGE_GUILD,
        GUILD_DENIAL,
        None,
    ),
    r(
        "giveaway",
        37,
        "Giveaway",
        Gate::Automations,
        MANAGE_GUILD,
        GUILD_DENIAL,
        None,
    ),
    r("rsvp", 24, "Rsvp", Gate::Announcements, 0, None, None),
    r(
        "rsvp-attendance",
        25,
        "RsvpAttendance",
        Gate::Announcements,
        0,
        None,
        None,
    ),
    r(
        "lfg",
        26,
        "Lfg",
        Gate::Announcements,
        MANAGE_EVENTS,
        EVENTS_DENIAL,
        None,
    ),
    r(
        "lfg-close",
        27,
        "LfgClose",
        Gate::Announcements,
        MANAGE_EVENTS,
        EVENTS_DENIAL,
        None,
    ),
    // Shares parity row 26 with `lfg` on purpose: a parity row is not a
    // unique command key.
    r(
        "lfg-edit",
        26,
        "LfgEdit",
        Gate::Announcements,
        MANAGE_EVENTS,
        EVENTS_DENIAL,
        None,
    ),
    r(
        "lfg-list",
        38,
        "LfgList",
        Gate::Announcements,
        MANAGE_EVENTS,
        EVENTS_DENIAL,
        None,
    ),
    r(
        "feed-add",
        28,
        "FeedAdd",
        Gate::Announcements,
        MANAGE_GUILD,
        GUILD_DENIAL,
        None,
    ),
    r(
        "feed-remove",
        29,
        "FeedRemove",
        Gate::Announcements,
        MANAGE_GUILD,
        GUILD_DENIAL,
        None,
    ),
    r(
        "feed-pause",
        44,
        "FeedPause",
        Gate::Announcements,
        MANAGE_GUILD,
        GUILD_DENIAL,
        None,
    ),
    r(
        "feed-resume",
        45,
        "FeedResume",
        Gate::Announcements,
        MANAGE_GUILD,
        GUILD_DENIAL,
        None,
    ),
    r(
        "feed-list",
        30,
        "FeedList",
        Gate::Announcements,
        MANAGE_GUILD,
        GUILD_DENIAL,
        None,
    ),
    r(
        "feed-inspect",
        39,
        "FeedInspect",
        Gate::Announcements,
        MANAGE_GUILD,
        GUILD_DENIAL,
        None,
    ),
    r(
        "ban",
        3,
        "Moderation(Ban)",
        Gate::Moderation,
        BAN_MEMBERS,
        Some("ModerationPermission(Ban)"),
        MEMBER_POLICY,
    ),
    r(
        "tempban",
        4,
        "Moderation(TempBan)",
        Gate::Moderation,
        BAN_MEMBERS,
        Some("ModerationPermission(TempBan)"),
        MEMBER_POLICY,
    ),
    r(
        "kick",
        5,
        "Moderation(Kick)",
        Gate::Moderation,
        KICK_MEMBERS,
        Some("ModerationPermission(Kick)"),
        MEMBER_POLICY,
    ),
    r(
        "timeout",
        6,
        "Moderation(Timeout)",
        Gate::Moderation,
        MODERATE_MEMBERS,
        Some("ModerationPermission(Timeout)"),
        MEMBER_POLICY,
    ),
    r(
        "warn",
        7,
        "Moderation(Warn)",
        Gate::Moderation,
        MODERATE_MEMBERS,
        Some("ModerationPermission(Warn)"),
        MEMBER_POLICY,
    ),
    r(
        "purge",
        8,
        "Moderation(Purge)",
        Gate::Moderation,
        MANAGE_MESSAGES,
        Some("ModerationPermission(Purge)"),
        None,
    ),
    r(
        "slowmode",
        9,
        "Moderation(Slowmode)",
        Gate::Moderation,
        MANAGE_CHANNELS,
        Some("ModerationPermission(Slowmode)"),
        None,
    ),
    r(
        "lockdown",
        10,
        "Moderation(Lockdown)",
        Gate::Moderation,
        MANAGE_CHANNELS,
        Some("ModerationPermission(Lockdown)"),
        None,
    ),
    r(
        "unlock",
        11,
        "Moderation(Unlock)",
        Gate::Moderation,
        MANAGE_CHANNELS,
        Some("ModerationPermission(Unlock)"),
        None,
    ),
    r(
        "purge-preview",
        35,
        "PurgePreview",
        Gate::Moderation,
        MANAGE_MESSAGES,
        Some("PurgePreviewPermission"),
        None,
    ),
    r(
        "channel-inspect",
        36,
        "ChannelInspect",
        Gate::Moderation,
        MANAGE_CHANNELS,
        Some("ChannelInspectPermission"),
        None,
    ),
];

/// `RouterRefusal` spelling returned while the gate is off but the command is
/// still routed (the router refuses, it does not fall through).
pub fn gate_refusal(gate: Gate) -> Option<&'static str> {
    match gate {
        Gate::Always => None,
        Gate::Scorecard => Some("ScorecardDisabled"),
        Gate::Automations => Some("AutomationsDisabled"),
        Gate::Announcements => Some("AnnouncementsDisabled"),
        Gate::Moderation => Some("ModerationDisabled"),
    }
}

/// `E2eGate::env_flag()` spelling the live suite must set.
pub fn gate_env_flag(gate: Gate) -> Option<&'static str> {
    match gate {
        Gate::Always => None,
        Gate::Scorecard => Some("TWO_COMMUNITY_SCORECARD"),
        Gate::Automations => Some("TWO_AUTOMATIONS"),
        Gate::Announcements => Some("TWO_ANNOUNCEMENTS"),
        Gate::Moderation => Some("TWO_MODERATION"),
    }
}

/// `/help` layout for the frozen commands, voice off, every gate on:
/// `(group title, "needs" label, names in group order)`. Help order is a user
/// facing contract and deliberately differs from publish order (for example
/// `game-mix` renders last and `welcome-preview` renders early).
pub const HELP_LAYOUT: [(&str, Option<&str>, &[&str]); 17] = [
    ("Leveling", None, &["rank", "leaderboard"]),
    ("Discovery", None, &["help"]),
    ("Privacy", None, &["privacy"]),
    ("Pickers", None, &["reopen"]),
    (
        "Welcome preview",
        Some("Manage Server"),
        &["welcome-preview"],
    ),
    ("Community check-in", Some("Manage Events"), &["attendance"]),
    (
        "Automations",
        Some("Manage Server"),
        &[
            "command",
            "command-remove",
            "command-list",
            "command-inspect",
            "command-disable",
            "command-enable",
            "schedule",
            "schedule-remove",
            "schedule-list",
            "sticky",
            "sticky-remove",
        ],
    ),
    ("Giveaways", Some("Manage Server"), &["giveaway"]),
    ("RSVPs", None, &["rsvp", "rsvp-attendance"]),
    (
        "Looking for group",
        Some("Manage Events"),
        &["lfg", "lfg-close", "lfg-edit", "lfg-list"],
    ),
    (
        "Feed relays",
        Some("Manage Server"),
        &[
            "feed-add",
            "feed-remove",
            "feed-pause",
            "feed-resume",
            "feed-list",
            "feed-inspect",
        ],
    ),
    ("Bans", Some("Ban Members"), &["ban", "tempban"]),
    (
        "Kick",
        Some("Kick Members, or a tracked voice room for a vote"),
        &["kick"],
    ),
    (
        "Timeouts and warnings",
        Some("Moderate Members"),
        &["timeout", "warn"],
    ),
    (
        "Message cleanup",
        Some("Manage Messages"),
        &["purge", "purge-preview"],
    ),
    (
        "Channel slowmode and locks",
        Some("Manage Channels"),
        &["slowmode", "lockdown", "unlock", "channel-inspect"],
    ),
    (
        "Game activity (private, live guild unavailable)",
        Some("Manage Server"),
        &["game-mix"],
    ),
];

/// Parity rows that are not static built-in slash commands. They must keep
/// their disposition and never resolve through the built-in lookup.
/// `(row, name, surface Debug spelling, mask, policy_hook Debug spelling)`.
pub const NON_SLASH_PARITY: [(u8, &str, &str, u64, &str); 3] = [
    (
        13,
        "rota-acknowledge",
        "Dropped",
        MANAGE_GUILD,
        "ConfiguredPrimaryActor",
    ),
    (22, "<custom>", "DynamicSlash", 0, "AutomationsEnabled"),
    (23, "<trigger>", "Prefix", 0, "TextCommandsEnabled"),
];
