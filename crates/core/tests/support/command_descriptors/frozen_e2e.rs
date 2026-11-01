//! Frozen, hand-written oracle for the E2E command-matrix metadata.
//!
//! Every value below was transcribed from the shipping `e2e_matrix.rs` rows
//! and is deliberately NOT derived from any registry, descriptor, or matrix
//! function: a derived expectation would let a migration bless its own
//! mistakes. Only the fields a descriptor migration could silently change
//! without breaking the baseline pins are frozen here: the success shape, the
//! permission label, and the full ordered denial list.
//!
//! Scope: the 43 non-voice built-in slash commands, in legacy publish order
//! (core, scorecard, automation, announcement, moderation). Changing a row is
//! a behavior change and needs its own justification.

/// One frozen E2E metadata row.
#[derive(Debug)]
pub struct FrozenE2e {
    pub name: &'static str,
    pub success_shape: &'static str,
    pub permission_name: &'static str,
    pub denials: &'static [&'static str],
}

/// The 43 frozen rows in legacy publish order.
pub const FROZEN_E2E: &[FrozenE2e] = &[
    FrozenE2e {
        name: "rank",
        success_shape: "Ephemeral text reply with XP, level and server rank; optional member option echoes that member.",
        permission_name: "Everyone",
        denials: &[],
    },
    FrozenE2e {
        name: "leaderboard",
        success_shape: "Public mention-suppressed top-ten reply.",
        permission_name: "Everyone",
        denials: &[],
    },
    FrozenE2e {
        name: "help",
        success_shape: "Immediate ephemeral reply (no defer, no store read) listing the live published commands grouped by audience with permission hints.",
        permission_name: "Everyone",
        denials: &[],
    },
    FrozenE2e {
        name: "privacy",
        success_shape: "Deferred ephemeral reply; analytics opt-out purges the invoker's guild rows plus suppression plus one audit receipt, opt-in clears suppression, status reports suppression plus guild analytics switches; delete queues a member-erasure request, and deletion status/receipt reads bind to the authenticated requester and guild.",
        permission_name: "Everyone",
        denials: &[
            "Valid:subcommand:required,opt-out|opt-in|status|delete|deletion-status|deletion-receipt",
            "Valid:request-id:required,max128,deletion-status|deletion-receipt",
        ],
    },
    FrozenE2e {
        name: "reopen",
        success_shape: "Deferred ephemeral reply; a fresh games or session menu edit for the invoker only, built from current roles and current configured picks with zero role, channel or funnel writes. Games menus carry a second no-change preview select alongside the apply route. Disabled kinds, malformed invocations and failed lookups get a private no-effect guidance edit instead of a menu.",
        permission_name: "Everyone",
        denials: &["Valid:kind:required,games|session"],
    },
    FrozenE2e {
        name: "welcome-preview",
        success_shape: "Deferred private preview of the current welcome and picker for a fixed synthetic newcomer; no delivery, member lookup or state mutation. Disabled or invalid configuration returns private guidance.",
        permission_name: "ManageGuild",
        denials: &["Perm:ManageServerRequired"],
    },
    FrozenE2e {
        name: "game-mix",
        success_shape: "Deferred private guild aggregate page: up to ten games per page, play or voice ordering, JSON or readable summary; play milliseconds, in-voice milliseconds and session counts; empty hint or truthful truncation note. Invalid selectors corrected before a read; live guild refused without a read.",
        permission_name: "ManageGuild",
        denials: &["Perm:ManageServerRequired"],
    },
    FrozenE2e {
        name: "attendance",
        success_shape: "Ephemeral confirmation or actionable refusal; bot records a verified human attendee for the occurrence.",
        permission_name: "ManageEvents",
        denials: &[
            "Gate:ScorecardDisabled",
            "Perm:ManageEventsRequired",
            "Valid:event-occurrence:required,max128",
            "Valid:member:required,user",
        ],
    },
    FrozenE2e {
        name: "command",
        success_shape: "Ephemeral confirmation after upsert plus automation audit row.",
        permission_name: "ManageGuild",
        denials: &[
            "Gate:AutomationsDisabled",
            "Perm:ManageServerRequired",
            "Valid:name:required",
            "Valid:template:required",
        ],
    },
    FrozenE2e {
        name: "command-remove",
        success_shape: "Ephemeral confirmation after delete (removed/missing outcome) plus audit row.",
        permission_name: "ManageGuild",
        denials: &[
            "Gate:AutomationsDisabled",
            "Perm:ManageServerRequired",
            "Valid:name:required",
        ],
    },
    FrozenE2e {
        name: "command-list",
        success_shape: "Ephemeral text list of this server's custom commands.",
        permission_name: "ManageGuild",
        denials: &["Gate:AutomationsDisabled", "Perm:ManageServerRequired"],
    },
    FrozenE2e {
        name: "command-inspect",
        success_shape: "Ephemeral defer then one private content-only view: metadata summary by default, saved template, or rendered preview with sample arguments. No channel send, registration, cooldown, mutation or run audit.",
        permission_name: "ManageGuild",
        denials: &[
            "Gate:AutomationsDisabled",
            "Perm:ManageServerRequired",
            "Valid:name:required",
            "Valid:view:choices",
            "Valid:arguments:utf16<=500",
        ],
    },
    FrozenE2e {
        name: "command-disable",
        success_shape: "Ephemeral defer then ephemeral confirmation after the saved definition is paused plus one audit row; a repeated pause is an explicit no-op without republish.",
        permission_name: "ManageGuild",
        denials: &[
            "Gate:AutomationsDisabled",
            "Perm:ManageServerRequired",
            "Valid:name:required",
        ],
    },
    FrozenE2e {
        name: "command-enable",
        success_shape: "Ephemeral defer then ephemeral confirmation after the saved definition is resumed plus one audit row; a repeated resume is an explicit no-op without republish.",
        permission_name: "ManageGuild",
        denials: &[
            "Gate:AutomationsDisabled",
            "Perm:ManageServerRequired",
            "Valid:name:required",
        ],
    },
    FrozenE2e {
        name: "schedule",
        success_shape: "Ephemeral defer then ephemeral completion; one timing option required, both bounded.",
        permission_name: "ManageGuild",
        denials: &[
            "Gate:AutomationsDisabled",
            "Perm:ManageServerRequired",
            "Valid:body:required",
            "Valid:in-minutes:1..=525600",
            "Valid:every-minutes:60..=525600",
        ],
    },
    FrozenE2e {
        name: "schedule-remove",
        success_shape: "Ephemeral confirmation after cancel plus audit row.",
        permission_name: "ManageGuild",
        denials: &[
            "Gate:AutomationsDisabled",
            "Perm:ManageServerRequired",
            "Valid:id:required",
        ],
    },
    FrozenE2e {
        name: "schedule-list",
        success_shape: "Ephemeral text list of scheduled messages for this server.",
        permission_name: "ManageGuild",
        denials: &["Gate:AutomationsDisabled", "Perm:ManageServerRequired"],
    },
    FrozenE2e {
        name: "sticky",
        success_shape: "Ephemeral defer then ephemeral confirmation after validate, upsert and audit.",
        permission_name: "ManageGuild",
        denials: &[
            "Gate:AutomationsDisabled",
            "Perm:ManageServerRequired",
            "Valid:body:required",
            "Valid:debounce:1..=300",
        ],
    },
    FrozenE2e {
        name: "sticky-remove",
        success_shape: "Ephemeral confirmation after removal plus audit row.",
        permission_name: "ManageGuild",
        denials: &["Gate:AutomationsDisabled", "Perm:ManageServerRequired"],
    },
    FrozenE2e {
        name: "giveaway",
        success_shape: "Ephemeral defer then ephemeral confirmation after create/list/cancel plus audit row.",
        permission_name: "ManageGuild",
        denials: &[
            "Gate:AutomationsDisabled",
            "Perm:ManageServerRequired",
            "Valid:prize:required,1..=200",
            "Valid:duration:1..=43200",
            "Valid:winners:1..=100",
        ],
    },
    FrozenE2e {
        name: "rsvp",
        success_shape: "Ephemeral `RSVP saved: <status>.` confirmation; a changed or repeated choice adds `(changed from <previous>)` or `(saved again; time updated)` (pasted links append the event id).",
        permission_name: "Everyone",
        denials: &[
            "Gate:AnnouncementsDisabled",
            "Valid:event-id:required,max128",
            "Valid:status:going|interested|declined",
        ],
    },
    FrozenE2e {
        name: "rsvp-attendance",
        success_shape: "Ephemeral RSVP totals for the scheduled event.",
        permission_name: "Everyone",
        denials: &["Gate:AnnouncementsDisabled", "Valid:event-id:required,max128"],
    },
    FrozenE2e {
        name: "lfg",
        success_shape: "Ephemeral signup post with role slots plus audit row.",
        permission_name: "ManageEvents",
        denials: &[
            "Gate:AnnouncementsDisabled",
            "Perm:ManageEventsRequired",
            "Valid:title:required",
            "Valid:starts-at:required,ISO-8601",
            "Valid:roles:required",
        ],
    },
    FrozenE2e {
        name: "lfg-close",
        success_shape: "Ephemeral confirmation after close plus audit row.",
        permission_name: "ManageEvents",
        denials: &[
            "Gate:AnnouncementsDisabled",
            "Perm:ManageEventsRequired",
            "Valid:id:required",
        ],
    },
    FrozenE2e {
        name: "lfg-edit",
        success_shape: "Ephemeral saved-edit receipt with canonical start time; refreshes the existing post, preserving signups.",
        permission_name: "ManageEvents",
        denials: &[
            "Gate:AnnouncementsDisabled",
            "Perm:ManageEventsRequired",
            "Valid:id:required",
            "Valid:title|starts-at:at-least-one",
            "Valid:starts-at:future,ISO-8601",
            "Valid:post:open,same-guild",
        ],
    },
    FrozenE2e {
        name: "lfg-list",
        success_shape: "Ephemeral current-channel open confirmed-post list; at most five whole rows and bounded cursor continuation, no mutations.",
        permission_name: "ManageEvents",
        denials: &[
            "Gate:AnnouncementsDisabled",
            "Perm:ManageEventsRequired",
            "Valid:cursor:optional,bound-to-guild-and-channel,max-512",
        ],
    },
    FrozenE2e {
        name: "feed-add",
        success_shape: "Ephemeral defer then ephemeral confirmation; relay id generated server-side plus audit row.",
        permission_name: "ManageGuild",
        denials: &[
            "Gate:AnnouncementsDisabled",
            "Perm:ManageServerRequired",
            "Valid:kind:rss|youtube|twitch",
            "Valid:source:required",
        ],
    },
    FrozenE2e {
        name: "feed-remove",
        success_shape: "Ephemeral confirmation after removal (removed/missing outcome) plus audit row.",
        permission_name: "ManageGuild",
        denials: &[
            "Gate:AnnouncementsDisabled",
            "Perm:ManageServerRequired",
            "Valid:id:required",
        ],
    },
    FrozenE2e {
        name: "feed-pause",
        success_shape: "Ephemeral confirmation after pause (paused/already-paused/missing outcome) plus audit row; refused while pending recovery exists.",
        permission_name: "ManageGuild",
        denials: &[
            "Gate:AnnouncementsDisabled",
            "Perm:ManageServerRequired",
            "Valid:id:required",
        ],
    },
    FrozenE2e {
        name: "feed-resume",
        success_shape: "Ephemeral confirmation after resume (resumed/already-resumed/missing outcome) plus audit row.",
        permission_name: "ManageGuild",
        denials: &[
            "Gate:AnnouncementsDisabled",
            "Perm:ManageServerRequired",
            "Valid:id:required",
        ],
    },
    FrozenE2e {
        name: "feed-list",
        success_shape: "Ephemeral guild-scoped relay list.",
        permission_name: "ManageGuild",
        denials: &["Gate:AnnouncementsDisabled", "Perm:ManageServerRequired"],
    },
    FrozenE2e {
        name: "feed-inspect",
        success_shape: "Ephemeral guild-scoped relay detail with safe source and last attempt; absent and invalid ids refuse distinctly without mutation.",
        permission_name: "ManageGuild",
        denials: &[
            "Gate:AnnouncementsDisabled",
            "Perm:ManageServerRequired",
            "Valid:id:required",
        ],
    },
    FrozenE2e {
        name: "ban",
        success_shape: "Ephemeral outcome reply; REST ban effect on success; mandatory audit reason stored.",
        permission_name: "BanMembers",
        denials: &[
            "Guild:GuildRestricted",
            "Gate:ModerationDisabled",
            "Perm:ModerationPermission(Ban)",
            "Policy:MemberModeration",
            "Valid:target:required,user",
            "Valid:reason:required,1..=512",
        ],
    },
    FrozenE2e {
        name: "tempban",
        success_shape: "Ephemeral outcome reply; timed REST ban effect on success; mandatory audit reason stored.",
        permission_name: "BanMembers",
        denials: &[
            "Guild:GuildRestricted",
            "Gate:ModerationDisabled",
            "Perm:ModerationPermission(TempBan)",
            "Policy:MemberModeration",
            "Valid:target:required,user",
            "Valid:duration_seconds:>=60,runtime-max-365d",
            "Valid:reason:required,1..=512",
        ],
    },
    FrozenE2e {
        name: "kick",
        success_shape: "Ephemeral outcome reply; REST kick effect on success; mandatory audit reason stored.",
        permission_name: "KickMembers",
        denials: &[
            "Guild:GuildRestricted",
            "Gate:ModerationDisabled",
            "Perm:ModerationPermission(Kick)",
            "Policy:MemberModeration",
            "Valid:target:required,user",
            "Valid:reason:required,1..=512",
        ],
    },
    FrozenE2e {
        name: "timeout",
        success_shape: "Ephemeral outcome reply; REST timeout effect on success; mandatory audit reason stored.",
        permission_name: "ModerateMembers",
        denials: &[
            "Guild:GuildRestricted",
            "Gate:ModerationDisabled",
            "Perm:ModerationPermission(Timeout)",
            "Policy:MemberModeration",
            "Valid:target:required,user",
            "Valid:duration_seconds:>=60,runtime-max-28d",
            "Valid:reason:required,1..=512",
        ],
    },
    FrozenE2e {
        name: "warn",
        success_shape: "Ephemeral outcome reply; warning recorded; mandatory audit reason stored.",
        permission_name: "ModerateMembers",
        denials: &[
            "Guild:GuildRestricted",
            "Gate:ModerationDisabled",
            "Perm:ModerationPermission(Warn)",
            "Policy:MemberModeration",
            "Valid:target:required,user",
            "Valid:reason:required,1..=512",
        ],
    },
    FrozenE2e {
        name: "purge",
        success_shape: "Ephemeral outcome reply; recent-message delete effect on success; mandatory audit reason stored.",
        permission_name: "ManageMessages",
        denials: &[
            "Guild:GuildRestricted",
            "Gate:ModerationDisabled",
            "Perm:ModerationPermission(Purge)",
            "Valid:count:1..=100",
            "Valid:reason:required,1..=512",
        ],
    },
    FrozenE2e {
        name: "slowmode",
        success_shape: "Ephemeral outcome reply; channel slowmode effect on success (0 disables); mandatory audit reason stored.",
        permission_name: "ManageChannels",
        denials: &[
            "Guild:GuildRestricted",
            "Gate:ModerationDisabled",
            "Perm:ModerationPermission(Slowmode)",
            "Valid:seconds:0..=21600",
            "Valid:reason:required,1..=512",
        ],
    },
    FrozenE2e {
        name: "lockdown",
        success_shape: "Ephemeral outcome reply; @everyone send block effect on success; mandatory audit reason stored.",
        permission_name: "ManageChannels",
        denials: &[
            "Guild:GuildRestricted",
            "Gate:ModerationDisabled",
            "Perm:ModerationPermission(Lockdown)",
            "Valid:reason:required,1..=512",
        ],
    },
    FrozenE2e {
        name: "unlock",
        success_shape: "Ephemeral outcome reply; @everyone send restore effect on success; mandatory audit reason stored.",
        permission_name: "ManageChannels",
        denials: &[
            "Guild:GuildRestricted",
            "Gate:ModerationDisabled",
            "Perm:ModerationPermission(Unlock)",
            "Valid:reason:required,1..=512",
        ],
    },
    FrozenE2e {
        name: "purge-preview",
        success_shape: "Deferred ephemeral sampled counts, candidate time span and exclusions; no delete, audit, claim or lane writes; sampling and race caveats remain explicit.",
        permission_name: "ManageMessages",
        denials: &[
            "Guild:GuildRestricted",
            "Gate:ModerationDisabled",
            "Perm:PurgePreviewPermission",
            "Valid:count:required,1..=100",
            "Valid:options:count-only",
        ],
    },
    FrozenE2e {
        name: "channel-inspect",
        success_shape: "Deferred ephemeral saved-lockdown and live-slowmode snapshot; absent lockdown is none, unavailable metadata is unknown; no config, audit, claim or lane writes.",
        permission_name: "ManageChannels",
        denials: &[
            "Guild:GuildRestricted",
            "Gate:ModerationDisabled",
            "Perm:ChannelInspectPermission",
            "Valid:options:none",
        ],
    },
];
