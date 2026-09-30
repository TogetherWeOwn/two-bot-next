//! Framework-free domain core for two-bot-next.
//!
//! This crate knows nothing about Discord wire types: it models guild-member,
//! voice-session and moderation events as plain Rust data so the same logic
//! can be driven by the twilight adapter (`two-bot-discord`), by unit tests,
//! or by future transports. Slices S3+ build on these seams.

pub mod automod;
pub mod automod_runtime;
pub mod commands;
pub mod config;
pub mod events;
pub mod feature_commands;
pub mod health;
pub mod leveling;
pub mod moderation;

pub use automod::{
    match_automod, normalize_content, sanction_for, validate_automod_rules, AutomodConfig,
    AutomodExportError, AutomodExportRule, AutomodFilter, AutomodGateError, AutomodMessage,
    AutomodPolicy, AutomodSanction, RepeatTracker, SanctionAction, DEFAULT_BLOCKED_ATTACHMENTS,
    DEFAULT_SANCTIONS,
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
