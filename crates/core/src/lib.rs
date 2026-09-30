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
pub mod onboarding;
#[cfg(feature = "db")]
pub mod onboarding_store;

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
pub use onboarding::{
    adjudicate_game_select, adjudicate_goodbye, adjudicate_session_select, adjudicate_welcome,
    anchor_occurrences_from, anchor_welcome_text, build_session_picks, channel_link,
    channel_routed_row, current_game_keys, days_in_guild, decide_prompt, funnel_idempotency_key,
    game_picker_allowed, game_selected_row, goodbye_text, individual_event_payloads,
    level_role_writes_allowed, live_series_start_epoch, next_anchor_occurrence, occurrence_context,
    pick_by_key, plan_game_selection, plan_session, preselected_game_keys, prompted_row,
    resolve_destination, scheduled_event_payload, session_ack_text, session_pick_by_key,
    session_routed_row, session_welcome_text, staging_session_picks, zoned_epoch_secs, AnchorSpec,
    Destination, FunnelRow, GamePick, GamePickerOutcome, GameSelection, GoodbyeEffect,
    MembershipTrigger, OccurrenceContext, OnboardingGateError, OnboardingGates, OnboardingMode,
    PickerKind, PromptDecision, PromptSkip, ScheduledEventPayload, SessionPick,
    SessionPickerOutcome, SessionPlan, WelcomeEffect, ANCHOR_CHANNEL_ID, DISCORD_ENTITY_VOICE,
    DISCORD_FREQUENCY_WEEKLY, DISCORD_PRIVACY_GUILD_ONLY, DISCORD_WEEKDAY_SUNDAY,
    EVENT_CHANNEL_ROUTED, EVENT_GAME_ROLES_SELECTED, EVENT_ONBOARDING_PROMPTED,
    GAME_HUB_CHANNEL_ID, GAME_PICKS, GAME_SELECT_ID, INTRO_CHANNEL_ID, NEAR_EVENT_SECS,
    PICKER_CLEARED_REPLY, PICKER_DRY_RUN_REPLY, PICKER_ROLE_FAILURE_REPLY, PLATFORM_PICKS,
    SESSION_SELECT_ID, SOURCE_PICKER, SOURCE_SESSION_PICKER, STAGING_LOBBY_VOICE_CHANNEL_ID,
    STAGING_LOOKING_TO_PLAY_CHANNEL_ID, SUNDAY_SQUAD, SUNDAY_SQUAD_DESCRIPTION, TWO_GUILD_ID,
};
#[cfg(feature = "db")]
pub use onboarding_store::{
    has_onboarding_prompt, record_channel_routed, record_game_selected, record_prompted,
    record_session_routed, OnboardingStoreError,
};
