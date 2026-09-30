//! Framework-free domain core for two-bot-next.
//!
//! This crate knows nothing about Discord wire types: it models guild-member,
//! voice-session and moderation events as plain Rust data so the same logic
//! can be driven by the twilight adapter (`two-bot-discord`), by unit tests,
//! or by future transports. Slices S3+ build on these seams.

pub mod audit;
pub mod automod;
pub mod classify;
pub mod commands;
pub mod config;
pub mod containment;
pub mod events;
pub mod expected_joins;
pub mod feature_commands;
pub mod funnel;
pub mod handlers;
pub mod health;
pub mod invites;
pub mod leveling;
pub mod mac;
pub mod moderation;
pub mod onboarding;
#[cfg(feature = "db")]
pub mod onboarding_store;
pub mod rsvp;
#[cfg(feature = "db")]
pub mod rsvp_store;
pub mod settings;
pub mod sticky;
pub mod voice;
pub mod voice_config;
pub mod voice_ownership;

pub use automod::{
    match_automod, normalize_content, sanction_for, validate_automod_rules, AutomodConfig,
    AutomodExportError, AutomodExportRule, AutomodFilter, AutomodGateError, AutomodMessage,
    AutomodPolicy, AutomodSanction, RepeatTracker, SanctionAction, DEFAULT_BLOCKED_ATTACHMENTS,
    DEFAULT_SANCTIONS,
};
pub use commands::{merge_commands, CommandDefinition, CustomCommand, RegistryError};
pub use config::Config;
pub use containment::{
    plan_quarantine, quarantine_outcome, role_removal_status, ClaimedContainmentEvent,
    ContainmentDisposition, ContainmentEventState, ContainmentIncident, ContainmentIncidentState,
    ContainmentPolicy, ContainmentPolicyError, ContainmentReason, ContainmentRole,
    DestructiveAction, DestructiveAuditEvent, QuarantineFailure, QuarantinePlan, QuarantineRefusal,
};
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
pub use health::{
    classify_voice_error, ComponentStatus, HealthReport, VoiceComponent, VoiceDiagnostic,
    VoiceFailureKind, VoiceHealthReport, VoicePermission, VoicePermissionScope, VoiceReadiness,
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
pub use onboarding::{
    adjudicate_game_select, adjudicate_goodbye, adjudicate_session_select, adjudicate_welcome,
    anchor_welcome_text, build_session_picks, channel_link, channel_routed_row, current_game_keys,
    days_in_guild, decide_prompt, funnel_idempotency_key, game_picker_allowed, game_selected_row,
    goodbye_text, level_role_writes_allowed, next_anchor_occurrence, occurrence_context,
    pick_by_key, plan_game_selection, plan_session, preselected_game_keys, prompted_row,
    resolve_destination, session_ack_text, session_pick_by_key, session_routed_row,
    session_welcome_text, staging_session_picks, welcome_trigger, zoned_epoch_secs, AnchorSpec,
    Destination, FunnelRow, GamePick, GamePickerOutcome, GameSelection, GoodbyeEffect,
    MembershipTrigger, MentionPolicy, OccurrenceContext, OnboardingGateError, OnboardingGates,
    OnboardingMode, PickerKind, PromptDecision, PromptSkip, SessionPick, SessionPickerOutcome,
    SessionPlan, WelcomeEffect, ANCHOR_CHANNEL_ID, EVENT_CHANNEL_ROUTED, EVENT_GAME_ROLES_SELECTED,
    EVENT_ONBOARDING_PROMPTED, GAME_HUB_CHANNEL_ID, GAME_PICKS, GAME_SELECT_ID, INTRO_CHANNEL_ID,
    NEAR_EVENT_SECS, PICKER_CLEARED_REPLY, PICKER_DRY_RUN_REPLY, PICKER_ROLE_FAILURE_REPLY,
    PLATFORM_PICKS, SESSION_SELECT_ID, SOURCE_PICKER, SOURCE_SESSION_PICKER,
    STAGING_LOBBY_VOICE_CHANNEL_ID, STAGING_LOOKING_TO_PLAY_CHANNEL_ID, SUNDAY_SQUAD, TWO_GUILD_ID,
};
#[cfg(feature = "db")]
pub use onboarding_store::{
    begin_prompt, has_onboarding_prompt, record_channel_routed, record_game_selected,
    record_prompted, record_session_routed, OnboardingStoreError, PromptGuard,
};
pub use rsvp::{
    attendance_totals_text, checkin_classification, checkin_duplicate_text,
    checkin_idempotency_key, checkin_metadata_json, checkin_recorded_text, checkin_source,
    checkin_source_event_id, is_snowflake, partition_rsvps, require_manage_events, rsvp_saved_text,
    validate_event_id, validate_occurrence_id, AttendanceClassification, AttendanceProof,
    CheckinError, RsvpAudit, RsvpError, RsvpRecord, RsvpStatus, RsvpTotals, RsvpTransition,
    ATTENDANCE_EVENT_TYPE, RSVP_AUDIT_ACTION,
};
#[cfg(feature = "db")]
pub use rsvp_store::{
    list_rsvps, put_rsvp, record_checkin, write_audit, CheckinWrite, RsvpStoreError,
};
pub use settings::{
    assert_storable_key, classify_key, is_declared_env_only, is_env_only_key, is_storable_key,
    to_env_string, validate_write, EnvOnlyKeyError, IgnoreReason, IgnoredChange, KeyChange,
    RefreshReport, SettingClass, SettingRow, SettingsCache, SettingsSnapshot, ValidatedWrite,
    WriteAction, WriteRefusal, ENV_ONLY_KEY_PREFIXES, HOT_WIRED, POLL_SECONDS, SETTING_CLASSES,
};
pub use sticky::{
    activity_eligible, automations_enabled, claim_blocks, decide_activity, normalize_debounce,
    repost_due, sticky_removed_reply, sticky_set_reply, validate_body, ActivityDecision,
    ActivityOutcome, ClaimGrant, PutSticky, RemoveOutcome, StickyAudit, StickyAuditAction,
    StickyAuditOutcome, StickyError, StickyState, CLAIM_EXPIRY_SECONDS, DEFAULT_DEBOUNCE_SECONDS,
    MAX_BODY_CHARS, MAX_DEBOUNCE_SECONDS, MIN_DEBOUNCE_SECONDS,
};
pub use voice::{
    average_known_voice_duration, count_unknown_starts_per_window, find_blind_windows,
    known_voice_durations, parse_voice_end_metadata, resolve_voice_end, summarize_voice_durations,
    BlindWindow, BlindWindowCount, OpenSession, VoiceDurationRow, VoiceDurationSummary, VoiceEnd,
    VoiceSessionTracker, DEFAULT_BLIND_WINDOW_MAX_GAP_MS,
};
