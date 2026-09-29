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
pub mod internal_actions;
pub mod leveling;
pub mod moderation;

pub use commands::{merge_commands, CommandDefinition, CustomCommand, RegistryError};
pub use config::Config;
pub use events::{CoreEvent, VoiceSessionDelta};
pub use feature_commands::{
    announcement_commands, automation_commands, feature_commands, scorecard_attendance_command,
    FeatureGates, GateError,
};
pub use health::{ComponentStatus, HealthReport};
pub use internal_actions::{
    assert_allowed, assert_private_bind, auth_failure, authorize, body_hash, build_channel_keys,
    build_key_map, build_role_keys, canonical_string, check_setting_value_size,
    is_declared_env_only, is_env_only_key, is_private_address, is_snowflake, is_storable_key,
    new_request_id, normalise_bind_host, parse_keys, require_field_str, require_reason,
    require_settings_key, require_snowflake, require_timestamp, sign, signatures_match,
    valid_idempotency_key, valid_nonce_format, validate_announcement, validate_event_input,
    validate_guild_add_member, validate_idempotency_key, validate_moderation_numbers,
    validate_role_assign, within_skew, ActionError, AuthDecision, AuthHeaders, BindError,
    BucketDecision, BucketSpec, ErrorCode, EventInput, EventPlace, InternalFlags, KeyMapError,
    KeyRing, KeySpecError, NonceCache, SettingClass, SigningKey, TokenBuckets, ACTIONS_PATH,
    ADD_MEMBER_BUCKET, AUTH_FAILURE_MESSAGE, CLAIM_STALE_SECONDS, DEFAULT_BUCKET,
    ENV_ONLY_KEY_PREFIXES, IMPLEMENTED_ACTIONS, MAX_BODY_BYTES, MAX_EVENT_DESCRIPTION_CHARS,
    MAX_EVENT_NAME_CHARS, MAX_MESSAGE_CHARS, MAX_SETTING_KEY_LEN, MAX_SETTING_VALUE_BYTES,
    MIN_KEY_SECRET_LEN, MODERATION_ACTIONS, NEEDS_IDEMPOTENCY_KEY, NEEDS_SETTINGS_STORE,
    NONCE_TTL_SECONDS, REQUEST_ID_LEN, SKEW_SECONDS,
};
pub use moderation::{
    assert_moderation_allowed, moderation_commands, moderation_target_protection,
    require_moderation_reason, ModerationAction, ModerationActor, ModerationGateError,
    ModerationGates, ModerationPolicy, ModerationRequest, ModerationTarget, PolicyError,
    ReasonError, TargetProtection,
};
