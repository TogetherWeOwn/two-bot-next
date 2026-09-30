//! Framework-free domain core for two-bot-next.
//!
//! This crate knows nothing about Discord wire types: it models guild-member,
//! voice-session and moderation events as plain Rust data so the same logic
//! can be driven by the twilight adapter (`two-bot-discord`), by unit tests,
//! or by future transports. Slices S3+ build on these seams.

pub mod commands;
pub mod config;
#[cfg(feature = "db")]
pub mod custom_command_store;
pub mod custom_commands;
pub mod events;
pub mod feature_commands;
pub mod health;
pub mod leveling;
pub mod moderation;

pub use commands::{merge_commands, CommandDefinition, CustomCommand, RegistryError};
pub use config::Config;
#[cfg(feature = "db")]
pub use custom_command_store::{
    audit as audit_custom_command, delete_command, find_text_trigger, get_command,
    get_command_by_text_trigger, list_commands, lock_command_capacity, put_command,
};
pub use custom_commands::{
    accepted_text_trigger, adjudicate_delete, adjudicate_put, adjudicate_run,
    builtin_command_names, check_capacity, deregister_set, error_code, format_command_list,
    is_builtin_trigger, max_custom_commands, placeholders_in, registry_with_custom,
    render_template, require_automations_enabled, trigger_word, validate_put_input,
    validate_template, AuditRecord, CommandError, DeleteDecision, PutCommandInput, PutDecision,
    RunOutcome, StoredCommand, TemplateContext, TemplateError, AUTOMATIONS_DISABLED_REPLY,
    MAX_COMMAND_NAME_CHARS, MAX_DESCRIPTION_CHARS, MAX_RENDERED_CHARS, MAX_TEMPLATE_CHARS,
    TEMPLATE_PLACEHOLDERS,
};
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
