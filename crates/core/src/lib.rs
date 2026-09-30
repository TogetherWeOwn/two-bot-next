//! Framework-free domain core for two-bot-next.
//!
//! This crate knows nothing about Discord wire types: it models guild-member,
//! voice-session and moderation events as plain Rust data so the same logic
//! can be driven by the twilight adapter (`two-bot-discord`), by unit tests,
//! or by future transports. Slices S3+ build on these seams.

pub mod commands;
pub mod community_snapshots;
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
pub mod scheduled_events;
pub mod voice;
#[cfg(feature = "db")]
pub mod website_store;

pub use commands::{merge_commands, CommandDefinition, CustomCommand, RegistryError};
pub use community_snapshots::{
    build_community_snapshot, build_counter_reading, match_rank_roles, now_iso, window_bounds,
    CommunitySnapshot, CounterReading, CounterSkip, JobGate, JobGuard, MemberRank, RaidAnomaly,
    RaidWindow, RankKey, RankRole, RankRow, RankSkip, RosterMember, LIVE_COUNTER_INTERVAL_MS,
    RAID_ANOMALIES, RANK_SNAPSHOT_INTERVAL_MS,
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
pub use scheduled_events::{
    normalize_event, normalize_events, EventStatus, RawScheduledEvent, ScheduledEvent,
    ScheduledEventsSkip, SCHEDULED_EVENTS_INTERVAL_MS,
};
pub use voice::{
    average_known_voice_duration, count_unknown_starts_per_window, find_blind_windows,
    known_voice_durations, parse_voice_end_metadata, resolve_voice_end, summarize_voice_durations,
    BlindWindow, BlindWindowCount, OpenSession, VoiceDurationRow, VoiceDurationSummary, VoiceEnd,
    VoiceSessionTracker, DEFAULT_BLIND_WINDOW_MAX_GAP_MS,
};
#[cfg(feature = "db")]
pub use website_store::{
    apply_web_contract, read_raid_windows, replace_events, write_counter, write_rank_snapshot,
    WebsiteStoreError, WEB_CONTRACT_VERSION, WEB_CONTRACT_VIEWS,
};
