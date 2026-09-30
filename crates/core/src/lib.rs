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
pub mod moderation;
pub mod self_roles;

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
pub use self_roles::{
    emoji_identity, event_order_for_event_id, event_order_from_snowflake,
    find_disallowed_permission, find_unsafe_channel_grant, is_snowflake, parse_self_role_custom_id,
    parse_self_role_panels, plan_select_delta, plan_self_role_change, reaction_endpoint_emoji,
    reaction_option_key, self_role_claim_owned, self_role_custom_id, self_role_renew_after_ms,
    self_role_reply, validate_panel_roles, validate_self_role_dispatch, ChannelOverwrite,
    ChannelSnapshot, DispatchCheck, DispatchFailure, DispatchRole, PanelMode, ParsedCustomId,
    PlanRejection, ResolvedRole, RoleOperation, SelfRoleConfigError, SelfRoleGates, SelfRoleOption,
    SelfRolePanel, SelfRolePlan, SettledOutcome, UnsafeGrant, UnsafeGrantKind,
    SELF_ROLE_ALLOWED_MASK, SELF_ROLE_ALLOWED_PERMISSIONS, SELF_ROLE_CLAIM_LEASE_MS,
};
