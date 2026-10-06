//! Staging E2E command-scope matrix: the code-derived contract for the live suite.
//!
//! The future staging-guild smoke suite (parent epic: staging-guild E2E smoke
//! for core slash commands) needs a fixed answer to "what does the suite
//! cover". This module is that answer, derived from the same registry code the
//! bot publishes: [`crate::router::InteractionRouter::publish_set`] with all
//! feature gates on, [`crate::command_permissions::command_permission`] for
//! permission bits, and [`crate::router::InteractionRouter::route_slash`] for
//! the denial order (guild fence → feature gate → permission bits →
//! policy/validation).
//!
//! Scope: the 28 built-in slash commands in `docs/commands.md` (core +
//! scorecard + automation + announcement + moderation). Out of scope on
//! purpose: voice commands (`voice_commands`, separate slice with its own
//! `TWO_VOICE` gate and a `kick` name collision resolved first-wins in favour
//! of moderation), `/templateassistant` (voice + assistant gates), DB-backed
//! custom commands and `!` prefix triggers (dynamic), component/modal
//! surfaces, and the dropped `/rota-acknowledge` row. The human-readable
//! rendering lives in `docs/staging-e2e-command-matrix.md`; the
//! `e2e_matrix_coverage` integration test pins this table against the
//! registry and the doc, so shipping a command without a matrix row fails CI.
//!
//! No guild, no staging secrets, no new credentials: everything here is
//! compile-time data plus pure registry reads.

use crate::commands::{
    PERM_BAN_MEMBERS, PERM_KICK_MEMBERS, PERM_MANAGE_CHANNELS, PERM_MANAGE_EVENTS,
    PERM_MANAGE_GUILD, PERM_MANAGE_MESSAGES, PERM_MODERATE_MEMBERS,
};

/// Which publish gate a matrix row needs. The env var is what the live suite
/// must set on staging for the command to be published at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum E2eGate {
    /// Always published (`rank`, `leaderboard`, `help`).
    Always,
    /// `TWO_COMMUNITY_SCORECARD=1`.
    Scorecard,
    /// `TWO_AUTOMATIONS=1`.
    Automations,
    /// `TWO_ANNOUNCEMENTS=1`.
    Announcements,
    /// `TWO_MODERATION=1`.
    Moderation,
}

impl E2eGate {
    /// The env flag the live suite needs, or `None` for always-on commands.
    #[must_use]
    pub fn env_flag(self) -> Option<&'static str> {
        match self {
            Self::Always => None,
            Self::Scorecard => Some("TWO_COMMUNITY_SCORECARD"),
            Self::Automations => Some("TWO_AUTOMATIONS"),
            Self::Announcements => Some("TWO_ANNOUNCEMENTS"),
            Self::Moderation => Some("TWO_MODERATION"),
        }
    }
}

/// One row of the E2E scope contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct E2eMatrixRow {
    /// Slash name without the leading `/`.
    pub command: &'static str,
    /// `docs/parity.md` §1 row (13 is the dropped rota row; 22/23 are the
    /// dynamic custom/prefix surfaces, not slash rows).
    pub parity_row: u8,
    /// Publish gate for the live suite's staging env.
    pub gate: E2eGate,
    /// Discord `default_member_permissions` bitfield; 0 means everyone.
    pub required_permissions: u64,
    /// Human name of the permission gate (`Everyone` when 0).
    pub permission_name: &'static str,
    /// Owning [`crate::router::HandlerId`] variant, as written in code.
    pub handler: &'static str,
    /// Observable success shape the live suite asserts (reply lifecycle from
    /// `docs/interaction-replies.md`: ACK within the 3 s budget as type 4 or
    /// type 5 + PATCH original, mentions suppressed, ≤2000 scalars).
    pub success_shape: &'static str,
    /// Denial paths the live suite may probe, as router outcomes. Every row
    /// additionally shares: foreign/missing guild → moderation answers
    /// `GuildRestricted`, all other builtins stay silent (`Ignore`); stale or
    /// unknown names → ephemeral `UNKNOWN_COMMAND_REPLY`; handler failure →
    /// generic `Something went wrong (ref …)` with no internal text (watch-log
    /// class only for the store path: `store_unavailable`).
    pub denials: &'static [&'static str],
}

// Eight fields because one row is one contract line; splitting the struct
// would split the coverage test's field-by-field pins. Same precedent as the
// other multi-field row builders in this repo.
#[allow(clippy::too_many_arguments)]
const fn row(
    command: &'static str,
    parity_row: u8,
    gate: E2eGate,
    required_permissions: u64,
    permission_name: &'static str,
    handler: &'static str,
    success_shape: &'static str,
    denials: &'static [&'static str],
) -> E2eMatrixRow {
    E2eMatrixRow {
        command,
        parity_row,
        gate,
        required_permissions,
        permission_name,
        handler,
        success_shape,
        denials,
    }
}

// Denial-path vocab (router outcomes + policy/validation):
// - `Gate:<REFUSAL>` — feature gate off while the command is still routed.
// - `Perm:<REFUSAL>` — invoking-member bits fail the permission row.
// - `Policy:MemberModeration` — target presence, self-target, owner/Owen/bot/
//   staff protection, bot hierarchy, actor hierarchy, in that order.
// - `Valid:<FIELD>:<BOUNDS>` — option validation before any side effect.

/// The 28-row contract in publish order (core, scorecard, automation,
/// announcement, moderation). The coverage test asserts this is exactly the
/// all-gates-on [`crate::router::InteractionRouter::publish_set`].
#[must_use]
pub fn e2e_command_matrix() -> Vec<E2eMatrixRow> {
    vec![
        row(
            "rank",
            1,
            E2eGate::Always,
            0,
            "Everyone",
            "Rank",
            "Ephemeral text reply with XP, level and server rank; optional member option echoes that member.",
            &[],
        ),
        row(
            "leaderboard",
            2,
            E2eGate::Always,
            0,
            "Everyone",
            "Leaderboard",
            "Public mention-suppressed top-ten reply.",
            &[],
        ),
        row(
            "help",
            31,
            E2eGate::Always,
            0,
            "Everyone",
            "Help",
            "Immediate ephemeral reply (no defer, no store read) listing the live published commands grouped by audience with permission hints.",
            &[],
        ),
        row(
            "attendance",
            12,
            E2eGate::Scorecard,
            PERM_MANAGE_EVENTS,
            "ManageEvents",
            "ScorecardAttendance",
            "Ephemeral confirmation or actionable refusal; bot records a verified human attendee for the occurrence.",
            &[
                "Gate:ScorecardDisabled",
                "Perm:ManageEventsRequired",
                "Valid:event-occurrence:required,max128",
                "Valid:member:required,user",
            ],
        ),
        row(
            "command",
            14,
            E2eGate::Automations,
            PERM_MANAGE_GUILD,
            "ManageGuild",
            "AutomationAdmin",
            "Ephemeral confirmation after upsert plus automation audit row.",
            &[
                "Gate:AutomationsDisabled",
                "Perm:ManageServerRequired",
                "Valid:name:required",
                "Valid:template:required",
            ],
        ),
        row(
            "command-remove",
            15,
            E2eGate::Automations,
            PERM_MANAGE_GUILD,
            "ManageGuild",
            "AutomationAdmin",
            "Ephemeral confirmation after delete (removed/missing outcome) plus audit row.",
            &[
                "Gate:AutomationsDisabled",
                "Perm:ManageServerRequired",
                "Valid:name:required",
            ],
        ),
        row(
            "command-list",
            16,
            E2eGate::Automations,
            PERM_MANAGE_GUILD,
            "ManageGuild",
            "AutomationAdmin",
            "Ephemeral text list of this server's custom commands.",
            &["Gate:AutomationsDisabled", "Perm:ManageServerRequired"],
        ),
        row(
            "schedule",
            17,
            E2eGate::Automations,
            PERM_MANAGE_GUILD,
            "ManageGuild",
            "AutomationAdmin",
            "Ephemeral defer then ephemeral completion; one timing option required, both bounded.",
            &[
                "Gate:AutomationsDisabled",
                "Perm:ManageServerRequired",
                "Valid:body:required",
                "Valid:in-minutes:1..=525600",
                "Valid:every-minutes:60..=525600",
            ],
        ),
        row(
            "schedule-remove",
            18,
            E2eGate::Automations,
            PERM_MANAGE_GUILD,
            "ManageGuild",
            "AutomationAdmin",
            "Ephemeral confirmation after cancel plus audit row.",
            &[
                "Gate:AutomationsDisabled",
                "Perm:ManageServerRequired",
                "Valid:id:required",
            ],
        ),
        row(
            "schedule-list",
            19,
            E2eGate::Automations,
            PERM_MANAGE_GUILD,
            "ManageGuild",
            "AutomationAdmin",
            "Ephemeral text list of scheduled messages for this server.",
            &["Gate:AutomationsDisabled", "Perm:ManageServerRequired"],
        ),
        row(
            "sticky",
            20,
            E2eGate::Automations,
            PERM_MANAGE_GUILD,
            "ManageGuild",
            "AutomationAdmin",
            "Ephemeral defer then ephemeral confirmation after validate, upsert and audit.",
            &[
                "Gate:AutomationsDisabled",
                "Perm:ManageServerRequired",
                "Valid:body:required",
                "Valid:debounce:1..=300",
            ],
        ),
        row(
            "sticky-remove",
            21,
            E2eGate::Automations,
            PERM_MANAGE_GUILD,
            "ManageGuild",
            "AutomationAdmin",
            "Ephemeral confirmation after removal plus audit row.",
            &["Gate:AutomationsDisabled", "Perm:ManageServerRequired"],
        ),
        row(
            "rsvp",
            24,
            E2eGate::Announcements,
            0,
            "Everyone",
            "Rsvp",
            "Ephemeral `RSVP saved: <status>.` confirmation.",
            &[
                "Gate:AnnouncementsDisabled",
                "Valid:event-id:required",
                "Valid:status:going|interested|declined",
            ],
        ),
        row(
            "rsvp-attendance",
            25,
            E2eGate::Announcements,
            0,
            "Everyone",
            "RsvpAttendance",
            "Ephemeral RSVP totals for the scheduled event.",
            &["Gate:AnnouncementsDisabled", "Valid:event-id:required"],
        ),
        row(
            "lfg",
            26,
            E2eGate::Announcements,
            PERM_MANAGE_EVENTS,
            "ManageEvents",
            "Lfg",
            "Ephemeral signup post with role slots plus audit row.",
            &[
                "Gate:AnnouncementsDisabled",
                "Perm:ManageEventsRequired",
                "Valid:title:required",
                "Valid:starts-at:required,ISO-8601",
                "Valid:roles:required",
            ],
        ),
        row(
            "lfg-close",
            27,
            E2eGate::Announcements,
            PERM_MANAGE_EVENTS,
            "ManageEvents",
            "LfgClose",
            "Ephemeral confirmation after close plus audit row.",
            &[
                "Gate:AnnouncementsDisabled",
                "Perm:ManageEventsRequired",
                "Valid:id:required",
            ],
        ),
        row(
            "feed-add",
            28,
            E2eGate::Announcements,
            PERM_MANAGE_GUILD,
            "ManageGuild",
            "FeedAdd",
            "Ephemeral defer then ephemeral confirmation; relay id generated server-side plus audit row.",
            &[
                "Gate:AnnouncementsDisabled",
                "Perm:ManageServerRequired",
                "Valid:kind:rss|youtube|twitch",
                "Valid:source:required",
            ],
        ),
        row(
            "feed-remove",
            29,
            E2eGate::Announcements,
            PERM_MANAGE_GUILD,
            "ManageGuild",
            "FeedRemove",
            "Ephemeral confirmation after removal (removed/missing outcome) plus audit row.",
            &[
                "Gate:AnnouncementsDisabled",
                "Perm:ManageServerRequired",
                "Valid:id:required",
            ],
        ),
        row(
            "feed-list",
            30,
            E2eGate::Announcements,
            PERM_MANAGE_GUILD,
            "ManageGuild",
            "FeedList",
            "Ephemeral guild-scoped relay list.",
            &["Gate:AnnouncementsDisabled", "Perm:ManageServerRequired"],
        ),
        row(
            "ban",
            3,
            E2eGate::Moderation,
            PERM_BAN_MEMBERS,
            "BanMembers",
            "Moderation(Ban)",
            "Ephemeral outcome reply; REST ban effect on success; mandatory audit reason stored.",
            &[
                "Guild:GuildRestricted",
                "Gate:ModerationDisabled",
                "Perm:ModerationPermission(Ban)",
                "Policy:MemberModeration",
                "Valid:target:required,user",
                "Valid:reason:required,1..=512",
            ],
        ),
        row(
            "tempban",
            4,
            E2eGate::Moderation,
            PERM_BAN_MEMBERS,
            "BanMembers",
            "Moderation(TempBan)",
            "Ephemeral outcome reply; timed REST ban effect on success; mandatory audit reason stored.",
            &[
                "Guild:GuildRestricted",
                "Gate:ModerationDisabled",
                "Perm:ModerationPermission(TempBan)",
                "Policy:MemberModeration",
                "Valid:target:required,user",
                "Valid:duration_seconds:>=60,runtime-max-365d",
                "Valid:reason:required,1..=512",
            ],
        ),
        row(
            "kick",
            5,
            E2eGate::Moderation,
            PERM_KICK_MEMBERS,
            "KickMembers",
            "Moderation(Kick)",
            "Ephemeral outcome reply; REST kick effect on success; mandatory audit reason stored.",
            &[
                "Guild:GuildRestricted",
                "Gate:ModerationDisabled",
                "Perm:ModerationPermission(Kick)",
                "Policy:MemberModeration",
                "Valid:target:required,user",
                "Valid:reason:required,1..=512",
            ],
        ),
        row(
            "timeout",
            6,
            E2eGate::Moderation,
            PERM_MODERATE_MEMBERS,
            "ModerateMembers",
            "Moderation(Timeout)",
            "Ephemeral outcome reply; REST timeout effect on success; mandatory audit reason stored.",
            &[
                "Guild:GuildRestricted",
                "Gate:ModerationDisabled",
                "Perm:ModerationPermission(Timeout)",
                "Policy:MemberModeration",
                "Valid:target:required,user",
                "Valid:duration_seconds:>=60,runtime-max-28d",
                "Valid:reason:required,1..=512",
            ],
        ),
        row(
            "warn",
            7,
            E2eGate::Moderation,
            PERM_MODERATE_MEMBERS,
            "ModerateMembers",
            "Moderation(Warn)",
            "Ephemeral outcome reply; warning recorded; mandatory audit reason stored.",
            &[
                "Guild:GuildRestricted",
                "Gate:ModerationDisabled",
                "Perm:ModerationPermission(Warn)",
                "Policy:MemberModeration",
                "Valid:target:required,user",
                "Valid:reason:required,1..=512",
            ],
        ),
        row(
            "purge",
            8,
            E2eGate::Moderation,
            PERM_MANAGE_MESSAGES,
            "ManageMessages",
            "Moderation(Purge)",
            "Ephemeral outcome reply; recent-message delete effect on success; mandatory audit reason stored.",
            &[
                "Guild:GuildRestricted",
                "Gate:ModerationDisabled",
                "Perm:ModerationPermission(Purge)",
                "Valid:count:1..=100",
                "Valid:reason:required,1..=512",
            ],
        ),
        row(
            "slowmode",
            9,
            E2eGate::Moderation,
            PERM_MANAGE_CHANNELS,
            "ManageChannels",
            "Moderation(Slowmode)",
            "Ephemeral outcome reply; channel slowmode effect on success (0 disables); mandatory audit reason stored.",
            &[
                "Guild:GuildRestricted",
                "Gate:ModerationDisabled",
                "Perm:ModerationPermission(Slowmode)",
                "Valid:seconds:0..=21600",
                "Valid:reason:required,1..=512",
            ],
        ),
        row(
            "lockdown",
            10,
            E2eGate::Moderation,
            PERM_MANAGE_CHANNELS,
            "ManageChannels",
            "Moderation(Lockdown)",
            "Ephemeral outcome reply; @everyone send block effect on success; mandatory audit reason stored.",
            &[
                "Guild:GuildRestricted",
                "Gate:ModerationDisabled",
                "Perm:ModerationPermission(Lockdown)",
                "Valid:reason:required,1..=512",
            ],
        ),
        row(
            "unlock",
            11,
            E2eGate::Moderation,
            PERM_MANAGE_CHANNELS,
            "ManageChannels",
            "Moderation(Unlock)",
            "Ephemeral outcome reply; @everyone send restore effect on success; mandatory audit reason stored.",
            &[
                "Guild:GuildRestricted",
                "Gate:ModerationDisabled",
                "Perm:ModerationPermission(Unlock)",
                "Valid:reason:required,1..=512",
            ],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn matrix_names_are_unique_and_parity_rows_are_unique() {
        let matrix = e2e_command_matrix();
        let mut names = HashSet::new();
        let mut parity = HashSet::new();
        for row in &matrix {
            assert!(
                names.insert(row.command),
                "duplicate matrix row {}",
                row.command
            );
            assert!(
                parity.insert(row.parity_row),
                "duplicate parity row {}",
                row.parity_row
            );
        }
    }

    #[test]
    fn matrix_covers_the_all_gates_on_publish_set() {
        let router = crate::router::InteractionRouter::new(crate::router::RouterGates {
            configured_guild: None,
            scorecard: true,
            automations: true,
            announcements: true,
            moderation: true,
            tickets: true,
            self_roles: true,
            onboarding_picker: true,
            session_picker: true,
        });
        let published = router.publish_set(&[]).expect("full set assembles");
        let mut published_names: Vec<&str> = published.iter().map(|d| d.name.as_str()).collect();
        published_names.sort_unstable();
        let mut matrix_names: Vec<&str> = e2e_command_matrix().iter().map(|r| r.command).collect();
        matrix_names.sort_unstable();
        assert_eq!(
            matrix_names, published_names,
            "matrix must match the published set exactly: add a row when a command ships, remove it when one is dropped"
        );
    }
}
