//! Framework-free domain core for two-bot-next.
//!
//! This crate knows nothing about Discord wire types: it models guild-member,
//! voice-session and moderation events as plain Rust data so the same logic
//! can be driven by the twilight adapter (`two-bot-discord`), by unit tests,
//! or by future transports. Slices S3+ build on these seams.

pub mod commands;
pub mod config;
pub mod events;
pub mod feature_commands;
pub mod health;
pub mod leveling;
pub mod lfg;
#[cfg(feature = "db")]
pub mod lfg_store;
pub mod moderation;

pub use commands::{merge_commands, CommandDefinition, CustomCommand, RegistryError};
pub use config::Config;
pub use events::{CoreEvent, VoiceSessionDelta};
pub use feature_commands::{
    announcement_commands, automation_commands, feature_commands, scorecard_attendance_command,
    FeatureGates, GateError,
};
pub use health::{ComponentStatus, HealthReport};
pub use lfg::{
    adjudicate_signup, close_reply, created_reply, iso_millis_utc, leave_reply, lfg_content,
    lfg_custom_id, lfg_nonce, lfg_select_options, normalize_starts_at, parse_lfg_select,
    parse_role_spec, require_manage_events, role_fill, signup_reply, spec_roles, valid_role_key,
    validate_title, LfgPermissionError, LfgPost, LfgRole, LfgRoleSpec, LfgSelectAction,
    LfgSelectOption, LfgSignup, LfgStatus, RoleSpecError, SignupOutcome, StartsAtError, TitleError,
    LFG_LEAVE_VALUE, LFG_SELECT_PREFIX, MAX_LFG_ROLES, MAX_MESSAGE_CHARS, MAX_OPTION_LABEL_CHARS,
    MAX_ROLE_SLOTS, MAX_TITLE_CHARS,
};
pub use moderation::{
    assert_moderation_allowed, moderation_commands, moderation_target_protection,
    require_moderation_reason, ModerationAction, ModerationActor, ModerationGateError,
    ModerationGates, ModerationPolicy, ModerationRequest, ModerationTarget, PolicyError,
    ReasonError, TargetProtection,
};
