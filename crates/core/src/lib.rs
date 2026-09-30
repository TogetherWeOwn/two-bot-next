//! Framework-free domain core for two-bot-next.
//!
//! This crate knows nothing about Discord wire types: it models guild-member,
//! voice-session and moderation events as plain Rust data so the same logic
//! can be driven by the twilight adapter (`two-bot-discord`), by unit tests,
//! or by future transports. Slices S3+ build on these seams.

pub mod channel_moderation;
#[cfg(feature = "db")]
pub mod channel_moderation_store;
pub mod commands;
pub mod config;
pub mod events;
pub mod feature_commands;
pub mod health;
pub mod leveling;
pub mod moderation;

pub use channel_moderation::{
    moderation_result_text, plan_lockdown, plan_unlock, require_channel_reason,
    validate_purge_count, validate_slowmode_seconds, BoundsError, ChannelModerationVerb,
    ChannelOutcome, EveryoneOverwrite, LockdownPlan, LockdownRecord, LockdownSeed, MaskError,
    UnlockError, UnlockPlan, MAX_PURGE_COUNT, MAX_SLOWMODE_SECONDS, MIN_PURGE_COUNT,
    SEND_MESSAGES_BIT,
};
#[cfg(feature = "db")]
pub use channel_moderation_store::{
    ChannelAuditRow, ChannelClaim, ChannelModerationStore, DB_POOL_MAX_DEFAULT,
    STATEMENT_TIMEOUT_MS,
};
pub use commands::{merge_commands, CommandDefinition, CustomCommand, RegistryError};
pub use config::Config;
pub use events::{CoreEvent, VoiceSessionDelta};
pub use feature_commands::{
    announcement_commands, automation_commands, feature_commands, scorecard_attendance_command,
    FeatureGates, GateError,
};
pub use health::{ComponentStatus, HealthReport};
pub use moderation::{
    assert_moderation_allowed, moderation_commands, moderation_target_protection,
    require_moderation_reason, ModerationAction, ModerationActor, ModerationGateError,
    ModerationGates, ModerationPolicy, ModerationRequest, ModerationTarget, PolicyError,
    ReasonError, TargetProtection,
};
