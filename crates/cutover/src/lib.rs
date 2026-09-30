//! Cutover data path for two-bot-next (TOG-9882).
//!
//! One-shot import/backfill operator tools ported from legacy two-bot
//! (frozen `main` @ `d5d11793`, `scripts/` + `src/leveling/` +
//! `src/backfill/` + `src/core/inviteTracker.ts`). The library holds the pure
//! logic — parsing, planning, reconciliation — so it unit-tests without a
//! database or Discord; `db` and `rest` hold the sqlx/twilight seams and the
//! seven `src/bin/` CLIs are thin arg-parsing shells around them.
//!
//! Conventions inherited from legacy: dry run is the default everywhere that
//! writes (`--apply` is the only thing that writes); every CLI refuses the
//! live guild (`326474832151838730`) without `--allow-live-guild` before it
//! opens any database or file; XP rules are the MEE6-compat ledger (15 XP per
//! message, 5 XP per voice minute, 60s per-source cooldowns) from
//! [`two_bot_core::leveling`].

pub mod backfill_plan;
pub mod cli;
pub mod db;
pub mod dedupe;
pub mod gateway_session;
pub mod invite;
pub mod legacy_copy;
pub mod legacy_mapping;
pub mod legacy_verify;
pub mod mee6_names;
pub mod mee6_rewards;
pub mod mee6_xp;
pub mod message_scan;
pub mod parse;
pub mod raid_tools;
pub mod rest;
pub mod settings;

pub use backfill_plan::{plan_backfill_merge, BackfillMerge, ListedMember, PlannedEvent};
pub use db::{
    connect, mark_bot, record_earliest, record_event, replace_role_rewards, role_rewards,
    touch_activity, CutoverDb, FunnelWrite, ReplaceRewardsError, DB_POOL_MAX_DEFAULT,
    STATEMENT_TIMEOUT_MS,
};
pub use dedupe::{
    collapse_cross_source_duplicates, CollapseResult, DedupableEvent, DEFAULT_TOLERANCE_MS,
};
pub use invite::{attribute_joins, attribute_single, invite_growth, InviteState, JoinAttribution};
pub use mee6_names::{clean_mee6_name, translate_export, translate_mee6_template};
pub use mee6_rewards::{
    parse_mee6_role_rewards, parse_roles_snapshot, plan_reward_role_import, LevelRoleReward,
    MappedReward, Mee6RewardExportError, Mee6RoleReward, RewardImportReport, RewardSkipReason,
    SnapshotRole, UnmappedReward,
};
pub use mee6_xp::{
    apply_mee6_import, mee6_xp_inventory, parse_mee6_export, plan_mee6_import, run_mee6_import,
    ImportError, ImportManifest, ImportSummary, LevelInventory, Mee6ExportError, Mee6ImportRow,
    SkipReason,
};
pub use message_scan::{
    find_early_messages, fold_messages, is_conversation_channel, is_log_channel, offer_message,
    MemberMessages, MessageScanSummary, ScannedMessage, FORUM_CHANNEL_TYPES, TEXT_CHANNEL_TYPES,
    THREAD_CHANNEL_TYPES,
};
pub use parse::{
    channel_id_from_embed, date_to_snowflake, first_snowflake, member_id_from_embed,
    member_log_kind_for_channel, parse_leave_attribution, parse_member_log_message,
    parse_voice_message, snowflake_to_date_ms, EmbedView, LeaveAttributionRecord, MemberLogKind,
    MemberLogRecord, MessageView, VoiceKind, VoiceRecord,
};
pub use rest::{iso_to_millis, timestamp_ms, RestClient, RestError, ScanPage};
pub use settings::{log_refresh_report, SettingsStore, SettingsWriteError};

/// Live TWO guild: every CLI refuses it without `--allow-live-guild`
/// (legacy `LIVE_GUILD_ID` in `src/staging/spec.ts`).
pub const LIVE_GUILD_ID: &str = "326474832151838730";

/// Staging guild for soak runs (legacy `TWO_STAGING_GUILD_ID`).
pub const STAGING_GUILD_ID: &str = "1545644954272137297";

/// True when `s` is a canonical Discord snowflake, safe for identity comparisons.
#[must_use]
pub fn is_snowflake(s: &str) -> bool {
    let len = s.len();
    (17..=20).contains(&len)
        && !s.starts_with('0')
        && s.bytes().all(|b| b.is_ascii_digit())
        && s.parse::<u64>().is_ok()
}
