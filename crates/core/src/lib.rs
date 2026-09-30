//! Framework-free domain core for two-bot-next.
//!
//! This crate knows nothing about Discord wire types: it models guild-member,
//! voice-session and moderation events as plain Rust data so the same logic
//! can be driven by the twilight adapter (`two-bot-discord`), by unit tests,
//! or by future transports. Slices S3+ build on these seams.

pub mod action_outcomes;
pub mod audit;
#[cfg(feature = "db")]
pub mod audit_store;
pub mod automod;
pub mod backup;
pub mod channel_moderation;
#[cfg(feature = "db")]
pub mod channel_moderation_store;
pub mod classify;
pub mod commands;
pub mod community;
pub mod community_snapshots;
#[cfg(feature = "db")]
pub mod community_store;
pub mod config;
pub mod containment;
pub mod events;
pub mod expected_joins;
pub mod feature_commands;
pub mod funnel;
pub mod gateway_funnel;
pub mod gateway_session;
pub mod handlers;
pub mod health;
pub mod inactivity;
#[cfg(feature = "db")]
pub mod inactivity_store;
#[cfg(feature = "db")]
pub mod internal_action_store;
pub mod internal_actions;
pub mod invites;
pub mod leveling;
#[cfg(feature = "db")]
pub mod leveling_store;
pub mod lfg;
#[cfg(feature = "db")]
pub mod lfg_store;
pub mod mac;
pub mod moderation;
pub mod onboarding;
#[cfg(feature = "db")]
pub mod onboarding_store;
pub mod presence;
#[cfg(feature = "db")]
pub mod presence_store;
pub mod raid;
pub mod router;
pub mod rsvp;
#[cfg(feature = "db")]
pub mod rsvp_store;
pub mod scheduled_events;
pub mod settings;
pub mod sticky;
pub mod voice;
pub mod voice_config;
pub mod voice_ownership;
pub mod voice_vote_kick;
#[cfg(feature = "db")]
pub mod website_store;

pub use action_outcomes::{
    backoff_ms, classify_kick_status, clear_send_bit, lockdown_overwrite, pace_wait_ms,
    parse_retry_after_secs, retry_after_ms, set_send_bit, unlock_overwrite, ActionOutcome,
    KickOutcome, KickResult, KickStatus, ModerationExecution, BACKOFF_BASE_MS, MAX_HTTP_TRIES,
    MAX_RETRY_AFTER_MS, RETRY_AFTER_PADDING_MS,
};
pub use automod::{
    match_automod, normalize_content, sanction_for, validate_automod_rules, AutomodConfig,
    AutomodExportError, AutomodExportRule, AutomodFilter, AutomodGateError, AutomodMessage,
    AutomodPolicy, AutomodSanction, RepeatTracker, SanctionAction, DEFAULT_BLOCKED_ATTACHMENTS,
    DEFAULT_SANCTIONS,
};
pub use channel_moderation::{
    moderation_result_text, plan_lockdown, plan_unlock, require_channel_reason,
    validate_purge_count, validate_slowmode_seconds, BoundsError, ChannelModerationVerb,
    ChannelOutcome, EveryoneOverwrite, LockdownPlan, LockdownRecord, LockdownSeed, MaskError,
    UnlockError, UnlockPlan, MAX_PURGE_COUNT, MAX_SLOWMODE_SECONDS, MIN_PURGE_COUNT,
    SEND_MESSAGES_BIT,
};
#[cfg(feature = "db")]
pub use channel_moderation_store::{
    ChannelAuditRow, ChannelClaim, ChannelClaimTicket, ChannelModerationStore, DB_POOL_MAX_DEFAULT,
    STATEMENT_TIMEOUT_MS,
};
pub use commands::{merge_commands, CommandDefinition, CustomCommand, RegistryError};
pub use community::{
    build_scorecard, classify, is_scorecard_run_time, previous_closed_week, scorecard_tick,
    week_start_ms, Classification, ClassifierConfig, ClassifyInput, FactRow, ScorecardGates,
    ScorecardInputs, ScorecardOutcome, StreamCoverage, COMMUNITY_CLASSIFICATIONS,
    COMMUNITY_FACT_TYPES, SCORECARD_TICK_INTERVAL_MS,
};
pub use community_snapshots::{
    build_community_snapshot, build_counter_reading, match_rank_roles, window_bounds,
    CommunitySnapshot, CounterReading, CounterSkip, JobGate, JobGuard, MemberRank, RaidAnomaly,
    RaidWindow, RankKey, RankRole, RankRow, RankSkip, RosterMember, LIVE_COUNTER_INTERVAL_MS,
    RAID_ANOMALIES, RANK_SNAPSHOT_INTERVAL_MS,
};
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
pub use inactivity::{
    flag_inactive, inactivity_cutoff_ms, member_inactive_event_key, parse_inactivity_days,
    select_inactive, should_flag, FlaggedMember, InactivityCandidate, InactivityOutcome,
    INACTIVITY_EVENT_SOURCE, INACTIVITY_EVENT_TYPE, INACTIVITY_SWEEP_INTERVAL_MS,
};
pub use internal_actions::{
    assert_allowed, assert_private_bind, auth_failure, authorize, body_hash, build_channel_keys,
    build_key_map, build_role_keys, canonical_string, check_setting_value_size, is_private_address,
    new_request_id, normalise_bind_host, parse_keys, require_field_str, require_reason,
    require_settings_key, require_snowflake, require_timestamp, sign, signatures_match, utf16_len,
    valid_idempotency_key, valid_nonce_format, validate_announcement, validate_event_input,
    validate_guild_add_member, validate_idempotency_key, validate_moderation_numbers,
    validate_role_assign, within_skew, ActionError, AuthDecision, AuthHeaders, BindError,
    BucketDecision, BucketSpec, ErrorCode, EventInput, EventPlace, InternalFlags, KeyMapError,
    KeyRing, KeySpecError, NonceCache, SigningKey, TokenBuckets, ACTIONS_PATH, ADD_MEMBER_BUCKET,
    AUTH_FAILURE_MESSAGE, CLAIM_STALE_SECONDS, DEFAULT_BUCKET, IMPLEMENTED_ACTIONS, MAX_BODY_BYTES,
    MAX_EVENT_DESCRIPTION_CHARS, MAX_EVENT_NAME_CHARS, MAX_MESSAGE_CHARS, MAX_SETTING_KEY_LEN,
    MAX_SETTING_VALUE_BYTES, MIN_KEY_SECRET_LEN, MODERATION_ACTIONS, NEEDS_IDEMPOTENCY_KEY,
    NEEDS_SETTINGS_STORE, NONCE_TTL_SECONDS, REQUEST_ID_LEN, SKEW_SECONDS,
};
pub use invites::{
    attribute_joins, attribution_category, count_downtime_unknown_joins, invite_growth,
    summarize_attribution_split, AttributionCategory, AttributionSplit, DowntimeWindow,
    DowntimeWindowCount, InviteSnapshotStore, InviteState, InviteTracker, JoinAttribution,
    MemSnapshots,
};
#[cfg(feature = "db")]
pub use leveling_store::{
    award as award_leveling_xp, award_message as award_leveling_message,
    award_voice as award_leveling_voice, current_award as current_leveling_award,
    leaderboard as leveling_leaderboard, profile as leveling_profile,
    replace_role_rewards as replace_leveling_role_rewards, role_rewards as leveling_role_rewards,
    LevelingStoreError,
};
pub use lfg::{
    adjudicate_signup, close_reply, created_reply, iso_millis_utc, leave_reply, lfg_content,
    lfg_custom_id, lfg_nonce, lfg_select_options, normalize_starts_at, parse_lfg_select,
    parse_role_spec, require_manage_events as lfg_require_manage_events, role_fill, signup_reply,
    spec_roles, valid_role_key, validate_title, LfgPermissionError, LfgPost, LfgRole, LfgRoleSpec,
    LfgSelectAction, LfgSelectOption, LfgSignup, LfgStatus, RoleSpecError, SignupOutcome,
    StartsAtError, TitleError, LFG_LEAVE_VALUE, LFG_SELECT_PREFIX, MAX_LFG_ROLES,
    MAX_OPTION_LABEL_CHARS, MAX_ROLE_SLOTS, MAX_TITLE_CHARS,
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
pub use presence::{
    bot_floor_due, daily_peaks, decide_probe_cycle, evaluate_trigger, latest_bot_floor,
    sanitize_presence_count, DailyPeak, PresenceReading, ProbeDecision, TriggerOptions,
    TriggerStatus, TriggerVerdict, BOT_FLOOR_MAX_AGE_MS, PRESENCE_PROBE_INTERVAL_MS,
    REOPEN_PEAK_THRESHOLD,
};
pub use raid::{
    count_recent_join_risks, JoinRiskEvidence, JoinRiskInput, JoinRiskObservation, JoinRiskPolicy,
    RaidAlert, RaidConfigError, RaidTuning, RaidWatch, RecordedJoinRisk, StaffAlertMessage,
    DEFAULT_JOIN_RISK_THRESHOLD, DEFAULT_JOIN_RISK_WINDOW_SECONDS, DEFAULT_RAID_COOLDOWN_SECONDS,
    DEFAULT_RAID_MAX_IDS, DEFAULT_RAID_THRESHOLD, DEFAULT_RAID_WINDOW_SECONDS,
};
pub use router::{
    ComponentHandler, ComponentOutcome, HandlerId, InteractionHandler, InteractionRouter,
    RouterGates, RouterRefusal, SlashContext, SlashOutcome, SurfaceFlags,
    ANNOUNCEMENTS_DISABLED_REPLY, AUTOMATIONS_DISABLED_REPLY, GUILD_RESTRICTED_REPLY, LFG_PREFIX,
    MANAGE_EVENTS_REQUIRED, MANAGE_SERVER_REQUIRED, MODERATION_DISABLED_REPLY,
    SCORECARD_DISABLED_REPLY, SELF_ROLE_PREFIX, TICKET_CLAIM_ID, TICKET_CLOSE_ID, TICKET_OPEN_ID,
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
pub use scheduled_events::{
    normalize_event, normalize_events, EventStatus, RawScheduledEvent, ScheduledEvent,
    ScheduledEventsSkip, SCHEDULED_EVENTS_INTERVAL_MS,
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
pub use voice_vote_kick::{
    RoomKickDecision, VoteBallot, VoteCancellation, VoteClock, VoteKickCore, VoteKickError,
    VoteKickRef, VoteKickStatus, VoteKickUpdate, VoteProgress, VoteRoomFacts, VOTE_KICK_TTL_MS,
};
#[cfg(feature = "db")]
pub use website_store::{
    apply_web_contract, read_raid_windows, replace_events, write_counter, write_rank_snapshot,
    WebsiteStoreError, WEB_CONTRACT_VERSION, WEB_CONTRACT_VIEWS,
};
