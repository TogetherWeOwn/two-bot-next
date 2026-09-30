//! Framework-free domain core for two-bot-next.
//!
//! This crate knows nothing about Discord wire types: it models guild-member,
//! voice-session and moderation events as plain Rust data so the same logic
//! can be driven by the twilight adapter (`two-bot-discord`), by unit tests,
//! or by future transports. Slices S3+ build on these seams.

pub mod commands;
pub mod config;
pub mod events;
pub mod expected_joins;
pub mod feature_commands;
pub mod funnel;
pub mod handlers;
pub mod health;
pub mod invites;
pub mod leveling;
pub mod moderation;
pub mod scheduled;
#[cfg(feature = "db")]
pub mod scheduled_store;
pub mod voice;

pub use commands::{merge_commands, CommandDefinition, CustomCommand, RegistryError};
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
pub use scheduled::{
    advance_next_run_iso, advance_next_run_ms, clamp_retry_delay_ms, format_iso_ms, lease_until_ms,
    next_run_at_ms, no_such_schedule_text, no_unique_match_text, parse_iso_ms,
    post_failure_retryable, resolve_scheduled_id, schedule_cancelled_text, schedule_confirm_text,
    schedule_list_line, schedule_list_text, validate_schedule, IdResolution, OccurrenceOutcome,
    ScheduleError, ScheduleInput, ValidatedSchedule, CLAIM_LEASE_MS, EVERY_MINUTES_MAX,
    EVERY_MINUTES_MIN, INTERVAL_SECONDS_MAX, INTERVAL_SECONDS_MIN, IN_MINUTES_MAX, IN_MINUTES_MIN,
    MAX_BODY_CHARS, RETRY_DEFAULT_MS, RETRY_MAX_MS, RETRY_MIN_MS, SCHEDULER_TICK_MS,
    TICKER_BATCH_LIMIT,
};
#[cfg(feature = "db")]
pub use scheduled_store::{
    audit_scheduled, claim_due, complete_run, delete_scheduled, get_scheduled, list_scheduled,
    put_scheduled, resolve_scheduled_id as resolve_scheduled_id_store, retry_scheduled,
    ScheduledAuditInput, ScheduledMessageRow, ScheduledStoreError, ScheduledWrite,
};
pub use voice::{
    average_known_voice_duration, count_unknown_starts_per_window, find_blind_windows,
    known_voice_durations, parse_voice_end_metadata, resolve_voice_end, summarize_voice_durations,
    BlindWindow, BlindWindowCount, OpenSession, VoiceDurationRow, VoiceDurationSummary, VoiceEnd,
    VoiceSessionTracker, DEFAULT_BLIND_WINDOW_MAX_GAP_MS,
};
