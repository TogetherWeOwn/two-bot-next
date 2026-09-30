//! Framework-free domain core for two-bot-next.
//!
//! This crate knows nothing about Discord wire types: it models guild-member,
//! voice-session and moderation events as plain Rust data so the same logic
//! can be driven by the twilight adapter (`two-bot-discord`), by unit tests,
//! or by future transports. Slices S3+ build on these seams.

pub mod automod;
pub mod commands;
pub mod community;
#[cfg(feature = "db")]
pub mod community_store;
pub mod config;
pub mod events;
pub mod expected_joins;
pub mod feature_commands;
pub mod funnel;
pub mod handlers;
pub mod health;
pub mod inactivity;
#[cfg(feature = "db")]
pub mod inactivity_store;
pub mod invites;
pub mod leveling;
pub mod moderation;
pub mod presence;
#[cfg(feature = "db")]
pub mod presence_store;
pub mod voice;

pub use automod::{
    match_automod, normalize_content, sanction_for, validate_automod_rules, AutomodConfig,
    AutomodExportError, AutomodExportRule, AutomodFilter, AutomodGateError, AutomodMessage,
    AutomodPolicy, AutomodSanction, RepeatTracker, SanctionAction, DEFAULT_BLOCKED_ATTACHMENTS,
    DEFAULT_SANCTIONS,
};
pub use commands::{merge_commands, CommandDefinition, CustomCommand, RegistryError};
pub use community::{
    build_scorecard, classify, is_scorecard_run_time, previous_closed_week, scorecard_tick,
    week_start_ms, Classification, ClassifierConfig, ClassifyInput, FactRow, ScorecardGates,
    ScorecardInputs, ScorecardOutcome, StreamCoverage, COMMUNITY_CLASSIFICATIONS,
    COMMUNITY_FACT_TYPES, SCORECARD_TICK_INTERVAL_MS,
};
pub use config::Config;
pub use events::{CoreEvent, VoiceSessionDelta};
pub use expected_joins::{ExpectedJoins, EXPECTED_JOIN_TTL_SECONDS, WEB_ONE_CLICK_SOURCE};
pub use feature_commands::{
    announcement_commands, automation_commands, feature_commands, scorecard_attendance_command,
    FeatureGates, GateError,
};
pub use funnel::{
    format_iso_millis, idempotency_key, is_measurable_gate_clearing, now_iso, parse_iso_millis,
    EventType, FunnelEvent, Snowflake, MESSAGE_RUNGS,
};
pub use handlers::{
    ChannelClass, FactsSink, FunnelHandlers, FunnelStore, GateClearedInput, HandlerOutcome,
    JoinInput, LevelOutcome, LevelingHook, MemStore, MemberJoinFact, MessageFact, MessageInput,
    NoopFacts, NoopLeveling, RecordOutcome, RulesAcceptedFact, StoredRow, VoiceEndedFact,
    VoiceInput, VoiceStartedFact,
};
pub use health::{ComponentStatus, HealthReport};
pub use inactivity::{
    flag_inactive, inactivity_cutoff_ms, member_inactive_event_key, parse_inactivity_days,
    select_inactive, should_flag, FlaggedMember, InactivityCandidate, InactivityOutcome,
    INACTIVITY_EVENT_SOURCE, INACTIVITY_EVENT_TYPE, INACTIVITY_SWEEP_INTERVAL_MS,
};
pub use invites::{
    attribute_joins, attribution_category, count_downtime_unknown_joins, invite_growth,
    summarize_attribution_split, AttributionCategory, AttributionSplit, DowntimeWindow,
    DowntimeWindowCount, InviteSnapshotStore, InviteState, InviteTracker, JoinAttribution,
    MemSnapshots,
};
pub use moderation::{
    assert_moderation_allowed, moderation_commands, moderation_target_protection,
    require_moderation_reason, ModerationAction, ModerationActor, ModerationGateError,
    ModerationGates, ModerationPolicy, ModerationRequest, ModerationTarget, PolicyError,
    ReasonError, TargetProtection,
};
pub use presence::{
    bot_floor_due, daily_peaks, decide_probe_cycle, evaluate_trigger, latest_bot_floor,
    sanitize_presence_count, DailyPeak, PresenceReading, ProbeDecision, TriggerOptions,
    TriggerStatus, TriggerVerdict, BOT_FLOOR_MAX_AGE_MS, PRESENCE_PROBE_INTERVAL_MS,
    REOPEN_PEAK_THRESHOLD,
};
pub use voice::{
    average_known_voice_duration, count_unknown_starts_per_window, find_blind_windows,
    known_voice_durations, parse_voice_end_metadata, resolve_voice_end, summarize_voice_durations,
    BlindWindow, BlindWindowCount, OpenSession, VoiceDurationRow, VoiceDurationSummary, VoiceEnd,
    VoiceSessionTracker, DEFAULT_BLIND_WINDOW_MAX_GAP_MS,
};
