//! Ordered V1 lifecycle executor. Gateway publication must continue independently
//! while a worker awaits HTTP/SQL: every write rechecks the latest snapshot.
//!
//! [`VoiceRuntime`] (below) is the V1 wiring: a per-guild actor registry fed
//! by a gateway sink. Each guild gets one actor task owning its
//! [`GuildRoomWorker`]; the sink translates twilight events into actor
//! commands, and a timer drains the worker's ordered queue. Single-attempt
//! REST plus deferred rename backoff are preserved so a rename backlog can
//! never monopolize a guild lane. The runtime is inert unless constructed
//! (gated on `TWO_VOICE=1` by the binary).

use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, RwLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::voice_room_plan::{
    category_room_ids, plan_room_diagnosed, RoomPlanError, RoomPlanInput, BOT_ROOM_ACCESS,
};
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};
use twilight_cache_inmemory::DefaultInMemoryCache;
use twilight_gateway::Event;
use twilight_model::{
    application::interaction::{
        application_command::{CommandDataOption, CommandOptionValue},
        Interaction, InteractionData, InteractionType,
    },
    channel::{
        message::{
            component::{ActionRow, Button, ButtonStyle, Component},
            AllowedMentions, MessageFlags,
        },
        permission_overwrite::PermissionOverwriteType,
        Channel, ChannelType,
    },
    guild::{Permissions, Role},
    http::{
        attachment::Attachment,
        interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    },
    id::{
        marker::{AttachmentMarker, RoleMarker, UserMarker},
        Id,
    },
};
use two_bot_core::{
    evaluate_permissions as evaluate_health, metrics, now_iso,
    voice_access::{
        is_voice_command, may_create_room, may_use_command, validate_access_controls,
        AccessControls, AccessDecision, AccessDenyReason, AccessMember,
    },
    voice_config::{
        decode_configuration, export_configuration, validate_configuration, ChannelKind,
        ChannelReference, GuildInventory, VoiceConfigError, VoiceConfiguration, MAX_IMPORT_BYTES,
        VOICE_CONFIG_VERSION,
    },
    voice_config_diff::{
        diff_configuration, diff_content_hash, render_preview, skip_unknown_channels,
        DIFF_HASH_CHARS,
    },
    voice_create_admission::{CreateAdmissionConfig, RefusalReason},
    voice_custom_id::{
        import_cancel_custom_id, import_confirm_custom_id, parse_voice_custom_id, VoiceAction,
        IMPORT_HASH_CHARS,
    },
    voice_logging::{
        parse_detail_level, resolve_log_target, should_log, DetailLevel, LogTarget,
        LoggingCandidates, LoggingSettings, RepeatLedger,
    },
    voice_name_filter::{
        filter_channel_name, resolve_create_name, BlockedRoomName, NameError, NameFilterContext,
        ResolvedRoomName, NAME_BLOCKED_AUDIT_REASON,
    },
    voice_ownership::{
        decide_ownership, OwnershipDecision, OwnershipError, OwnershipRequest, RoomActor,
        RoomMember, RoomOwnership,
    },
    voice_rooms::{
        category_full_message, is_usable_channel_name, voice_commands, ActionQueue, CreatorChannel,
        NewRoomSpec, PermissionSource, ProposeOutcome, QueuedAction, RenameCoalescer, RoomAction,
        RoomPosition, TextCompanion, VoiceGates, VoiceRoom, MAX_CHANNELS_PER_CATEGORY,
        MAX_CHANNEL_NAME_LEN, QUEUE_MAX_ATTEMPTS, RENAME_MIN_INTERVAL_MS,
    },
    voice_text_channel::{
        admin_view_roles, occupancy_diff, text_channel_plan,
        OverwriteTarget as TextOverwriteTarget, TextChannelPlan, VoiceRoomFacts,
        DEFAULT_TEXT_CHANNEL_NAME, MAX_TEXT_CHANNEL_NAME_CHARS,
    },
    voice_utilities::{invite_render, ping_render},
    voice_vote_kick::{
        VoteBallot, VoteCancellation, VoteClock, VoteKickCore, VoteKickError, VoteKickRef,
        VoteKickStatus, VoteKickUpdate, VoteRoomFacts,
    },
    AutomodPolicy, CommandDefinition, OverwriteTarget, PermissionFinding,
    PermissionOverwrite as HealthOverwrite, Snowflake, VoicePermission, VoicePermissionScope,
};
use two_bot_cutover::voice_rooms::{CreateClaim, PgRoomStore};
use two_bot_discord::voice_rooms::{
    can_enforce_kick, can_manage_room, effective_permissions, RoomChannelAttributes, RoomHttp,
    RoomHttpError,
};

#[path = "voice_name_panel.rs"]
mod name_panel;
pub use name_panel::NameDirectory;
use name_panel::{
    handle_name_interaction, name_component_action, name_directory_from_cache, NameCommand,
    NameInteraction, NameReply,
};

pub type WriteGuard = Arc<dyn Fn() -> bool + Send + Sync>;

/// Keep a permission refusal observed at send time distinct from a stale
/// ticket cancellation, without changing the adapter's boolean guard contract.
struct GuardedWrite {
    check: WriteGuard,
    permission_failure: Arc<Mutex<Option<LifecycleFailure>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    Unavailable,
    CredentialRefused,
    Conflict,
}

impl std::fmt::Display for StoreError {
    /// Plain words for an admin-facing reply; never the variant name or any
    /// driver detail.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unavailable => "the store is unavailable",
            Self::CredentialRefused => "the store refused the bot credential",
            Self::Conflict => "the store reported a conflict",
        })
    }
}

/// No synchronous database work is allowed on the gateway event loop.
pub trait RoomPersistence: Send + Sync {
    fn creators(
        &self,
        guild: Snowflake,
    ) -> impl Future<Output = Result<Vec<CreatorChannel>, StoreError>> + Send;
    fn rooms(
        &self,
        guild: Snowflake,
    ) -> impl Future<Output = Result<Vec<VoiceRoom>, StoreError>> + Send;
    fn add_creator(
        &self,
        creator: &CreatorChannel,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
    /// The creator row for one channel, if an admin marked it (V9d
    /// `/textchannels` edits this row through the [`Self::add_creator`]
    /// upsert; the settings snapshot on existing companions never changes).
    fn creator_for(
        &self,
        guild: Snowflake,
        channel: Snowflake,
    ) -> impl Future<Output = Result<Option<CreatorChannel>, StoreError>> + Send;
    fn persist(&self, room: &VoiceRoom) -> impl Future<Output = Result<(), StoreError>> + Send;
    /// Persist a V2 caretaker/command handoff on an already-tracked room.
    /// Returns `Ok(true)` when the row existed, `Ok(false)` when the tracked
    /// row has no database counterpart (deleted out-of-band).
    fn update_ownership(
        &self,
        room: &VoiceRoom,
    ) -> impl Future<Output = Result<bool, StoreError>> + Send;
    /// Every V3 `/name` custom-name override in the guild, for worker load.
    /// Rooms on their template name have no entry.
    fn custom_names(
        &self,
        guild: Snowflake,
    ) -> impl Future<Output = Result<Vec<(Snowflake, String)>, StoreError>> + Send;
    /// Persist a V3 `/name` override on a tracked room, or clear it with
    /// `None` (restore). Returns `Ok(true)` when the row existed, `Ok(false)`
    /// when the tracked row has no database counterpart (deleted out-of-band).
    fn save_custom_name(
        &self,
        guild: Snowflake,
        channel: Snowflake,
        custom_name: Option<&str>,
    ) -> impl Future<Output = Result<bool, StoreError>> + Send;
    /// The guild's V10b controls; an unconfigured guild reads as the defaults.
    fn access_controls(
        &self,
        guild: Snowflake,
    ) -> impl Future<Output = Result<AccessControls, StoreError>> + Send;
    /// Replace the guild's controls (validated before the write).
    fn save_access_controls(
        &self,
        guild: Snowflake,
        controls: &AccessControls,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
    /// The guild's V10a logging settings; an unconfigured guild reads as the defaults.
    fn logging_settings(
        &self,
        guild: Snowflake,
    ) -> impl Future<Output = Result<LoggingSettings, StoreError>> + Send;
    /// Replace the guild's logging settings.
    fn save_logging_settings(
        &self,
        guild: Snowflake,
        settings: &LoggingSettings,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
    /// The guild's V11 voice configuration; a never-configured guild reads
    /// as the defaults, so `/export` works on a fresh guild.
    fn config_snapshot(
        &self,
        guild: Snowflake,
    ) -> impl Future<Output = Result<VoiceConfiguration, StoreError>> + Send;
    /// Replace the guild's V11 voice configuration in one transaction
    /// (`/import` Confirm only; never touches live rooms or companions).
    /// Compare-and-swap on `expected` (the snapshot the preview was rendered
    /// from): a concurrent change is `Conflict`, never a silent overwrite.
    /// The preview hash already binds (`current`, `candidate`); this closes
    /// the re-read-to-write window under the store's per-guild lock.
    fn config_apply(
        &self,
        guild: Snowflake,
        config: &VoiceConfiguration,
        expected: &VoiceConfiguration,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
    fn forget(
        &self,
        guild: Snowflake,
        channel: Snowflake,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
    /// Claim one room create under the durable admission limits (caps,
    /// cooldown, rolling burst), serialized per guild. An admitted claim is
    /// persisted before this returns; a refusal writes nothing. `now_secs` is
    /// the worker's Unix-seconds wall clock.
    fn claim_create(
        &self,
        guild: Snowflake,
        user: Snowflake,
        config: &CreateAdmissionConfig,
        now_secs: i64,
    ) -> impl Future<Output = Result<CreateClaim, StoreError>> + Send;
    /// Bind the claim to the channel Discord just created, before the room row
    /// is written. The durable channel witness: a restarted worker rediscovers
    /// the channel through [`Self::orphaned_create_channels`] if persist and
    /// compensation both fail. The claim keeps holding its cap slot.
    fn bind_create_channel(
        &self,
        guild: Snowflake,
        reservation_id: &str,
        channel: Snowflake,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
    /// Unsettled claims bound to a created channel with no tracked room, as
    /// `(reservation id, channel id)`: creates whose persist or compensation
    /// never finished. Read at worker load so compensation survives restart.
    fn orphaned_create_channels(
        &self,
        guild: Snowflake,
    ) -> impl Future<Output = Result<Vec<(String, Snowflake)>, StoreError>> + Send;
    /// Persist the successful create and transfer its capacity hold atomically
    /// under the claim's guild lock. Failure retains the unbound hold; replay
    /// never POSTs again. No intermediate double occupancy is visible.
    fn persist_create(
        &self,
        reservation_id: &str,
        room: &VoiceRoom,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
    /// Release a claim only after known no-side-effect failure, confirmed
    /// absence or successful compensation. Never release an unknown outcome.
    /// The history row remains for burst and cooldown accounting.
    fn settle_create(
        &self,
        reservation_id: &str,
    ) -> impl Future<Output = Result<bool, StoreError>> + Send;
    /// Every companion tracked in the guild, for worker load and startup
    /// reconciliation. Each row carries its creation-time settings snapshot.
    fn companions(
        &self,
        guild: Snowflake,
    ) -> impl Future<Output = Result<Vec<TextCompanion>, StoreError>> + Send;
    /// Insert-once like [`Self::persist`]: false when this room already has a
    /// companion record. The settings snapshot is never updated in place.
    fn add_companion(
        &self,
        companion: &TextCompanion,
    ) -> impl Future<Output = Result<bool, StoreError>> + Send;
    /// Delete the companion tracked for a room; returns the removed row.
    fn remove_companion(
        &self,
        guild: Snowflake,
        room: Snowflake,
    ) -> impl Future<Output = Result<Option<TextCompanion>, StoreError>> + Send;
}

impl RoomPersistence for PgRoomStore {
    async fn creators(&self, guild: Snowflake) -> Result<Vec<CreatorChannel>, StoreError> {
        self.creators(guild).await.map_err(store_error)
    }

    async fn rooms(&self, guild: Snowflake) -> Result<Vec<VoiceRoom>, StoreError> {
        self.rooms_in_guild(guild).await.map_err(store_error)
    }

    async fn add_creator(&self, creator: &CreatorChannel) -> Result<(), StoreError> {
        self.add_creator(creator).await.map_err(store_error)?;
        Ok(())
    }

    async fn creator_for(
        &self,
        guild: Snowflake,
        channel: Snowflake,
    ) -> Result<Option<CreatorChannel>, StoreError> {
        self.creator_for(guild, channel).await.map_err(store_error)
    }

    async fn persist(&self, room: &VoiceRoom) -> Result<(), StoreError> {
        if !self.add_room(room).await.map_err(store_error)?
            && self
                .room_for(room.guild_id, room.channel_id)
                .await
                .map_err(store_error)?
                .as_ref()
                != Some(room)
        {
            return Err(StoreError::Conflict);
        }
        Ok(())
    }

    async fn update_ownership(&self, room: &VoiceRoom) -> Result<bool, StoreError> {
        self.update_ownership(
            room.guild_id,
            room.channel_id,
            room.owner_id,
            room.original_creator_id,
        )
        .await
        .map_err(store_error)
    }

    async fn custom_names(&self, guild: Snowflake) -> Result<Vec<(Snowflake, String)>, StoreError> {
        self.custom_names(guild).await.map_err(store_error)
    }

    async fn save_custom_name(
        &self,
        guild: Snowflake,
        channel: Snowflake,
        custom_name: Option<&str>,
    ) -> Result<bool, StoreError> {
        self.set_custom_name(guild, channel, custom_name)
            .await
            .map_err(store_error)
    }

    async fn access_controls(&self, guild: Snowflake) -> Result<AccessControls, StoreError> {
        self.access_controls(guild).await.map_err(store_error)
    }

    async fn save_access_controls(
        &self,
        guild: Snowflake,
        controls: &AccessControls,
    ) -> Result<(), StoreError> {
        self.save_access_controls(guild, controls)
            .await
            .map_err(store_error)
    }

    async fn logging_settings(&self, guild: Snowflake) -> Result<LoggingSettings, StoreError> {
        self.logging_settings(guild).await.map_err(store_error)
    }

    async fn save_logging_settings(
        &self,
        guild: Snowflake,
        settings: &LoggingSettings,
    ) -> Result<(), StoreError> {
        self.save_logging_settings(guild, settings)
            .await
            .map_err(store_error)
    }

    async fn forget(&self, guild: Snowflake, channel: Snowflake) -> Result<(), StoreError> {
        self.remove_room(guild, channel)
            .await
            .map_err(store_error)?;
        Ok(())
    }

    async fn claim_create(
        &self,
        guild: Snowflake,
        user: Snowflake,
        config: &CreateAdmissionConfig,
        now_secs: i64,
    ) -> Result<CreateClaim, StoreError> {
        self.claim_create(guild, user, config, now_secs)
            .await
            .map_err(store_error)
    }

    async fn bind_create_channel(
        &self,
        guild: Snowflake,
        reservation_id: &str,
        channel: Snowflake,
    ) -> Result<(), StoreError> {
        self.bind_create_channel(guild, reservation_id, channel)
            .await
            .map_err(store_error)
    }

    async fn orphaned_create_channels(
        &self,
        guild: Snowflake,
    ) -> Result<Vec<(String, Snowflake)>, StoreError> {
        self.orphaned_create_channels(guild)
            .await
            .map_err(store_error)
    }

    async fn persist_create(
        &self,
        reservation_id: &str,
        room: &VoiceRoom,
    ) -> Result<(), StoreError> {
        self.persist_create(reservation_id, room)
            .await
            .map_err(store_error)
    }

    async fn settle_create(&self, reservation_id: &str) -> Result<bool, StoreError> {
        self.settle_create(reservation_id)
            .await
            .map_err(store_error)
    }

    async fn config_snapshot(&self, guild: Snowflake) -> Result<VoiceConfiguration, StoreError> {
        self.voice_configs()
            .snapshot(guild)
            .await
            .map_err(store_error)
    }

    async fn config_apply(
        &self,
        guild: Snowflake,
        config: &VoiceConfiguration,
        expected: &VoiceConfiguration,
    ) -> Result<(), StoreError> {
        match self.voice_configs().apply(guild, config, expected).await {
            Ok(()) => Ok(()),
            // The store's compare-and-swap sentinel: the guild changed
            // between the Confirm re-read and the locked write.
            Err(sqlx::Error::RowNotFound) => Err(StoreError::Conflict),
            Err(error) => Err(store_error(error)),
        }
    }

    async fn companions(&self, guild: Snowflake) -> Result<Vec<TextCompanion>, StoreError> {
        self.companions_in_guild(guild).await.map_err(store_error)
    }

    async fn add_companion(&self, companion: &TextCompanion) -> Result<bool, StoreError> {
        self.add_companion(companion).await.map_err(store_error)
    }

    async fn remove_companion(
        &self,
        guild: Snowflake,
        room: Snowflake,
    ) -> Result<Option<TextCompanion>, StoreError> {
        self.remove_companion(guild, room)
            .await
            .map_err(store_error)
    }
}

/// Wall clock in Unix seconds for the durable create admission; a clock
/// before the epoch reads as 0.
fn unix_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

fn store_error(error: sqlx::Error) -> StoreError {
    match &error {
        sqlx::Error::Database(error)
            if error
                .code()
                .is_some_and(|code| code.starts_with("28") || code == "42501") =>
        {
            StoreError::CredentialRefused
        }
        _ => StoreError::Unavailable,
    }
}

/// Bounded outcome class for a terminal Discord error (TOG-13543). Rate
/// limits are retries, never outcomes; callers skip them before reaching
/// here. `Rejected` status/code values never become labels.
fn voice_outcome_from_http(error: &RoomHttpError) -> &'static str {
    match error {
        RoomHttpError::Cancelled => "cancelled",
        _ => "discord",
    }
}

/// Bounded outcome class for a terminal store error (TOG-13543). All three
/// variants share one `persistence` outcome to keep cardinality fixed; the
/// sanitized debug already rides the failure line for `/setup`.
fn voice_outcome_from_store(_error: &StoreError) -> &'static str {
    "persistence"
}

/// Bounded dead-letter family for a queue action (TOG-13543). Companion
/// creates/grants/revokes share `companion`; unknown shapes share `other`.
fn voice_dead_action(action: &RoomAction) -> &'static str {
    match action {
        RoomAction::CreateRoom { .. } => "create",
        RoomAction::MoveMember { .. } => "move",
        RoomAction::DeleteRoom { .. } => "delete",
        RoomAction::CreateCompanion { .. }
        | RoomAction::GrantCompanionView { .. }
        | RoomAction::RevokeCompanionView { .. } => "companion",
        RoomAction::UpdateOwnership { .. } => "ownership",
        RoomAction::KickMember { .. } => "kick",
        // The override write belongs to the rename family: same feature,
        // and the bounded `action` label set stays as documented.
        RoomAction::RenameRoom { .. } | RoomAction::SetCustomName { .. } => "rename",
    }
}

/// One finished room lifecycle outcome (TOG-13543): a fixed-cardinality
/// counter plus a token-free log line. Event name `voice_operation` with
/// `op`/`outcome` fields is the catalog entry coordinated with blocked
/// TOG-10870 (which owns JSON formatting): no IDs, bodies, tokens or member
/// data, only the bounded operation and outcome.
fn observe_voice_operation(op: &'static str, outcome: &'static str) {
    metrics::global().voice_operation(op, outcome);
    if outcome == "success" {
        info!(
            voice_event = "voice_operation",
            op = op,
            outcome = outcome,
            "voice_operation succeeded"
        );
    } else {
        warn!(
            voice_event = "voice_operation",
            op = op,
            outcome = outcome,
            "voice_operation failed"
        );
    }
}

/// Where one operator notice is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeTarget {
    Channel(Snowflake),
    DirectMessage(Snowflake),
}

/// The production adapter performs one attempt and returns 429s to this worker.
pub trait RoomWrites: Send + Sync {
    fn create(
        &self,
        guild: Snowflake,
        name: &str,
        attributes: &RoomChannelAttributes,
        guard: WriteGuard,
    ) -> impl Future<Output = Result<Channel, RoomHttpError>> + Send;
    fn move_member(
        &self,
        guild: Snowflake,
        member: Snowflake,
        channel: Snowflake,
        guard: WriteGuard,
    ) -> impl Future<Output = Result<(), RoomHttpError>> + Send;
    /// V4 vote-kick enforcement: disconnect the member from voice
    /// (`channel_id: null`); 404 (already left) is success.
    fn disconnect(
        &self,
        guild: Snowflake,
        member: Snowflake,
        guard: WriteGuard,
    ) -> impl Future<Output = Result<(), RoomHttpError>> + Send;
    /// V4 vote-kick enforcement: deny Connect to the member on one room
    /// channel only (member-scoped overwrite, not a guild kick or ban).
    fn deny_connect(
        &self,
        channel: Snowflake,
        member: Snowflake,
        guard: WriteGuard,
    ) -> impl Future<Output = Result<(), RoomHttpError>> + Send;
    fn delete(
        &self,
        channel: Snowflake,
        guard: WriteGuard,
    ) -> impl Future<Output = Result<(), RoomHttpError>> + Send;
    fn rename(
        &self,
        channel: Snowflake,
        name: &str,
    ) -> impl Future<Output = Result<(), RoomHttpError>> + Send;
    /// Download a Discord-hosted `/import` file, capped at `max_bytes`
    /// (the caller checks the attachment size before asking).
    fn download_attachment(
        &self,
        url: &str,
        max_bytes: usize,
    ) -> impl Future<Output = Result<Vec<u8>, RoomHttpError>> + Send;

    /// Create a V9 companion text channel with its full overwrite set in the
    /// POST (never patched afterwards). The bot's own View allow rides the
    /// POST via `bot_id`.
    fn create_companion(
        &self,
        plan: &TextChannelPlan,
        bot_id: Snowflake,
        guard: WriteGuard,
    ) -> impl Future<Output = Result<Channel, RoomHttpError>> + Send;
    /// Grant one occupant View on the companion (V9 join): a Member allow,
    /// never a deny.
    fn grant_companion_view(
        &self,
        text_channel_id: Snowflake,
        member_id: Snowflake,
        guard: WriteGuard,
    ) -> impl Future<Output = Result<(), RoomHttpError>> + Send;
    /// Delete one occupant's overwrite on the companion (V9 leave): never a
    /// deny. Deleting an absent overwrite is success.
    fn revoke_companion_view(
        &self,
        text_channel_id: Snowflake,
        member_id: Snowflake,
        guard: WriteGuard,
    ) -> impl Future<Output = Result<(), RoomHttpError>> + Send;
    /// V10 error notice. `mention_role` is pinged on channel targets only.
    /// The default refuses, so a writer that cannot post notices fails closed
    /// (the worker counts the attempt and moves on) instead of dropping them
    /// silently as delivered.
    fn send_notice(
        &self,
        target: NoticeTarget,
        content: &str,
        mention_role: Option<Snowflake>,
    ) -> impl Future<Output = Result<(), RoomHttpError>> + Send {
        let _ = (target, content, mention_role);
        async { Err(RoomHttpError::InvalidRequest) }
    }
}

impl RoomWrites for RoomHttp {
    async fn create(
        &self,
        guild: Snowflake,
        name: &str,
        attributes: &RoomChannelAttributes,
        guard: WriteGuard,
    ) -> Result<Channel, RoomHttpError> {
        self.create_room(guild, name, attributes, move || guard())
            .await
    }

    async fn move_member(
        &self,
        guild: Snowflake,
        member: Snowflake,
        channel: Snowflake,
        guard: WriteGuard,
    ) -> Result<(), RoomHttpError> {
        self.move_member(guild, member, channel, move || guard())
            .await
    }

    async fn disconnect(
        &self,
        guild: Snowflake,
        member: Snowflake,
        guard: WriteGuard,
    ) -> Result<(), RoomHttpError> {
        self.disconnect_member(guild, member, move || guard()).await
    }

    async fn deny_connect(
        &self,
        channel: Snowflake,
        member: Snowflake,
        guard: WriteGuard,
    ) -> Result<(), RoomHttpError> {
        self.deny_member_connect(channel, member, move || guard())
            .await
    }

    async fn delete(&self, channel: Snowflake, guard: WriteGuard) -> Result<(), RoomHttpError> {
        self.delete_room(channel, move || guard()).await
    }

    async fn rename(&self, channel: Snowflake, name: &str) -> Result<(), RoomHttpError> {
        self.rename_room(channel, name).await
    }

    async fn download_attachment(
        &self,
        url: &str,
        max_bytes: usize,
    ) -> Result<Vec<u8>, RoomHttpError> {
        self.download_attachment(url, max_bytes).await
    }

    async fn create_companion(
        &self,
        plan: &TextChannelPlan,
        bot_id: Snowflake,
        guard: WriteGuard,
    ) -> Result<Channel, RoomHttpError> {
        self.create_companion(plan, bot_id, move || guard()).await
    }

    async fn grant_companion_view(
        &self,
        text_channel_id: Snowflake,
        member_id: Snowflake,
        guard: WriteGuard,
    ) -> Result<(), RoomHttpError> {
        self.grant_companion_view(text_channel_id, member_id, move || guard())
            .await
    }

    async fn revoke_companion_view(
        &self,
        text_channel_id: Snowflake,
        member_id: Snowflake,
        guard: WriteGuard,
    ) -> Result<(), RoomHttpError> {
        self.revoke_companion_view(text_channel_id, member_id, move || guard())
            .await
    }

    async fn send_notice(
        &self,
        target: NoticeTarget,
        content: &str,
        mention_role: Option<Snowflake>,
    ) -> Result<(), RoomHttpError> {
        match target {
            NoticeTarget::Channel(channel) => {
                self.post_notice(channel, content, mention_role).await
            }
            NoticeTarget::DirectMessage(user) => self.direct_notice(user, content).await,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BotAccess {
    pub member_id: Snowflake,
    pub guild_owner_id: Snowflake,
    /// The guild's system channel, first stop for V10 error notices.
    pub system_channel_id: Option<Snowflake>,
    pub member_roles: Vec<Id<RoleMarker>>,
    pub roles: Vec<Role>,
}

#[derive(Debug, Clone, Copy)]
pub struct VoiceMember {
    pub member_id: Snowflake,
    pub channel_id: Snowflake,
    /// Unknown member identity counts as human: never delete on incomplete bot data.
    pub bot: Option<bool>,
}

/// Only publish after GuildCreate (or an authoritative reconnect refresh) has
/// populated channels, voice states, roles and the bot member. READY alone is not
/// sufficient. A partial channel listing must never be published as complete.
#[derive(Debug, Clone)]
pub struct GuildSnapshot {
    pub channels: Vec<Channel>,
    pub members: Vec<VoiceMember>,
    pub bot: BotAccess,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinTicket {
    member_id: Snowflake,
    creator_id: Snowflake,
    generation: u64,
    transition: u64,
}

#[derive(Debug, Clone)]
struct MemberState {
    channel_id: Option<Snowflake>,
    bot: Option<bool>,
    transition: u64,
    /// Wall-clock millis when the current continuous stay in `channel_id`
    /// began. Leave/rejoin resets it; same-channel updates keep it. This is
    /// the V2 caretaker ordering source ("longest-present member").
    joined_at_ms: u64,
}

/// Wall-clock millis for V2 tenure stamps. Coarse ordering only: equal
/// stamps fall back to ascending member id in `decide_ownership`.
fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[derive(Debug, Default)]
struct LiveState {
    ready: bool,
    generation: u64,
    next_transition: u64,
    channels: HashMap<Snowflake, Channel>,
    members: HashMap<Snowflake, MemberState>,
    bot: Option<BotAccess>,
}

impl LiveState {
    fn ticket_valid(&self, ticket: JoinTicket) -> bool {
        self.ready
            && self.generation == ticket.generation
            && self.members.get(&ticket.member_id).is_some_and(|member| {
                member.channel_id == Some(ticket.creator_id)
                    && member.transition == ticket.transition
            })
    }

    fn permissions(&self, guild: Snowflake, channel: Snowflake) -> Option<Permissions> {
        let bot = self.bot.as_ref()?;
        let channel = self.channels.get(&channel)?;
        effective_permissions(
            guild,
            bot.guild_owner_id,
            bot.member_id,
            &bot.member_roles,
            &bot.roles,
            channel.permission_overwrites.as_deref().unwrap_or_default(),
        )
    }

    /// Guild roles holding Manage Channels (V9d AC7), read from the live role
    /// snapshot so a role promoted later is covered the next time a plan or
    /// protected set is built, with no per-room overwrite update.
    fn admin_role_ids(&self, guild: Snowflake) -> Vec<Snowflake> {
        let Some(bot) = self.bot.as_ref() else {
            return Vec::new();
        };
        let roles: Vec<(Snowflake, u64)> = bot
            .roles
            .iter()
            .map(|role| (role.id.get(), role.permissions.bits()))
            .collect();
        admin_view_roles(guild, &roles)
    }

    fn humans(&self, channel: Snowflake) -> usize {
        self.members
            .values()
            .filter(|member| member.channel_id == Some(channel) && member.bot != Some(true))
            .count()
    }

    /// Human occupants of one channel, sorted for deterministic vote facts.
    fn occupants(&self, channel: Snowflake) -> Vec<Snowflake> {
        let mut ids: Vec<Snowflake> = self
            .members
            .iter()
            .filter(|(_, member)| member.channel_id == Some(channel) && member.bot != Some(true))
            .map(|(id, _)| *id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Current occupants of one room as a V2 ownership snapshot. Unknown
    /// identity counts as human, matching [`LiveState::humans`]: never treat
    /// incomplete bot data as a bot.
    fn ownership_snapshot(&self, channel: Snowflake) -> Vec<RoomMember> {
        let mut members: Vec<RoomMember> = self
            .members
            .iter()
            .filter(|(_, member)| member.channel_id == Some(channel))
            .map(|(member_id, member)| RoomMember {
                member_id: *member_id,
                joined_at_ms: member.joined_at_ms,
                is_bot: member.bot == Some(true),
            })
            .collect();
        members.sort_by_key(|member| member.member_id);
        members
    }
}

/// Findings from the same snapshot and surface as the create/move gate.
/// The caller already owns the read lock; do not reacquire it here.
fn write_permission_findings(
    state: &LiveState,
    guild_id: Snowflake,
    channel_id: Snowflake,
) -> Vec<PermissionFinding> {
    let (Some(bot), Some(channel)) = (state.bot.as_ref(), state.channels.get(&channel_id)) else {
        return Vec::new();
    };
    let Some(base) = effective_permissions(
        guild_id,
        bot.guild_owner_id,
        bot.member_id,
        &bot.member_roles,
        &bot.roles,
        &[],
    ) else {
        return Vec::new();
    };
    let Some(effective) = state.permissions(guild_id, channel_id) else {
        return Vec::new();
    };
    two_bot_core::voice_permission_health::evaluate_write_permissions(
        base.bits(),
        effective.bits(),
        BOT_ROOM_ACCESS,
        if channel.kind == ChannelType::GuildCategory {
            VoicePermissionScope::Category
        } else {
            VoicePermissionScope::Channel
        },
        channel_id,
    )
}

/// Shared with the gateway, not locked across network/database awaits.
#[derive(Debug, Clone)]
pub struct LiveGuild {
    guild_id: Snowflake,
    inner: Arc<RwLock<LiveState>>,
}

impl LiveGuild {
    pub fn new(guild_id: Snowflake) -> Self {
        Self {
            guild_id,
            inner: Arc::new(RwLock::new(LiveState::default())),
        }
    }

    pub fn publish(&self, snapshot: GuildSnapshot) -> bool {
        if snapshot
            .channels
            .iter()
            .any(|channel| channel.guild_id.map(Id::get) != Some(self.guild_id))
        {
            self.disconnect();
            return false;
        }
        let mut live = self.inner.write().expect("live voice lock");
        live.generation += 1;
        live.channels = snapshot
            .channels
            .into_iter()
            .map(|channel| (channel.id.get(), channel))
            .collect();
        // One shared stamp: bootstrap order is unknown, so tenure ties
        // break by member id until transitions establish real seniority.
        // A snapshot rebuild resets tenure — tenure is continuous tracked
        // presence in this session, not a durable fact.
        let published_at_ms = wall_ms();
        live.members = snapshot
            .members
            .into_iter()
            .map(|member| {
                (
                    member.member_id,
                    MemberState {
                        channel_id: Some(member.channel_id),
                        bot: member.bot,
                        transition: 0,
                        joined_at_ms: published_at_ms,
                    },
                )
            })
            .collect();
        live.bot = Some(snapshot.bot);
        live.ready = true;
        true
    }

    pub fn disconnect(&self) {
        let mut live = self.inner.write().expect("live voice lock");
        live.ready = false;
        live.generation += 1;
    }

    /// V10 health check: the bot's missing Manage Channels, Move Members,
    /// Manage Roles and View Channel permissions across the given creator
    /// channels and their categories, attributed to the outermost level that
    /// removes each one. Incomplete cache data (no bot snapshot, a missing
    /// @everyone or bot role) reports nothing rather than a false failure, and
    /// the guild owner and Administrator roles never have findings.
    #[must_use]
    pub fn permission_findings(&self, creators: &[Snowflake]) -> Vec<PermissionFinding> {
        let live = self.inner.read().expect("live voice lock");
        let Some(bot) = live.bot.as_ref() else {
            return Vec::new();
        };
        if bot.member_id == bot.guild_owner_id {
            return Vec::new();
        }
        let Some(everyone) = bot.roles.iter().find(|role| role.id.get() == self.guild_id) else {
            return Vec::new();
        };
        let mut base = everyone.permissions.bits();
        for role_id in &bot.member_roles {
            let Some(role) = bot.roles.iter().find(|role| role.id == *role_id) else {
                return Vec::new();
            };
            base |= role.permissions.bits();
        }
        let bot_roles: Vec<Snowflake> = bot.member_roles.iter().map(|id| id.get()).collect();
        let mut findings = Vec::new();
        for creator_id in creators {
            let Some(channel) = live.channels.get(creator_id) else {
                continue;
            };
            let category = channel
                .parent_id
                .and_then(|parent| live.channels.get(&parent.get()));
            // A creator outside a category, or whose category is not cached,
            // has no category-level evidence: only guild and channel scopes.
            let (category_id, category_overwrites) = category.map_or_else(
                || (0, Vec::new()),
                |category| {
                    (
                        category.id.get(),
                        health_overwrites(self.guild_id, category),
                    )
                },
            );
            for finding in evaluate_health(
                base,
                category_id,
                &category_overwrites,
                *creator_id,
                &health_overwrites(self.guild_id, channel),
                bot.member_id,
                &bot_roles,
            ) {
                if !findings.contains(&finding) {
                    findings.push(finding);
                }
            }
        }
        findings
    }

    /// Refresh the bot access snapshot after role changes. Generation is
    /// unchanged: role edits do not invalidate in-flight tickets, they only
    /// affect the next guard evaluation.
    pub fn refresh_bot(&self, access: BotAccess) {
        self.inner.write().expect("live voice lock").bot = Some(access);
    }

    /// System channel and owner for V10 notice routing; `None` until the bot
    /// evidence is published.
    fn notice_context(&self) -> Option<(Option<Snowflake>, Snowflake)> {
        let state = self.inner.read().expect("live voice lock");
        let bot = state.bot.as_ref()?;
        Some((bot.system_channel_id, bot.guild_owner_id))
    }

    /// Publish before enqueueing the ticket. Same-channel mute/deaf updates do
    /// not create rooms; leaving and returning yields a different ticket even
    /// if an older HTTP call is still awaiting its response.
    pub fn voice_update(
        &self,
        member: Snowflake,
        channel: Option<Snowflake>,
        bot: Option<bool>,
    ) -> Option<JoinTicket> {
        self.voice_update_at(member, channel, bot, wall_ms())
    }

    /// Deterministic tenure seam: `voice_update` stamps the wall clock;
    /// tests pin `now_ms` to control caretaker ordering.
    fn voice_update_at(
        &self,
        member: Snowflake,
        channel: Option<Snowflake>,
        bot: Option<bool>,
        now_ms: u64,
    ) -> Option<JoinTicket> {
        let mut live = self.inner.write().expect("live voice lock");
        if let Some(previous) = live.members.get_mut(&member) {
            if previous.channel_id == channel {
                previous.bot = bot.or(previous.bot);
                return None;
            }
        }
        let bot = bot.or_else(|| live.members.get(&member).and_then(|previous| previous.bot));
        live.next_transition += 1;
        let transition = live.next_transition;
        live.members.insert(
            member,
            MemberState {
                channel_id: channel,
                bot,
                transition,
                joined_at_ms: now_ms,
            },
        );
        channel.filter(|_| live.ready).map(|creator_id| JoinTicket {
            member_id: member,
            creator_id,
            generation: live.generation,
            transition,
        })
    }

    pub fn upsert_channel(&self, channel: Channel) {
        if channel.guild_id.map(Id::get) == Some(self.guild_id) {
            self.inner
                .write()
                .expect("live voice lock")
                .channels
                .insert(channel.id.get(), channel);
        }
    }

    pub fn remove_channel(&self, channel: Snowflake) {
        self.inner
            .write()
            .expect("live voice lock")
            .channels
            .remove(&channel);
    }

    fn join_guard(&self, ticket: JoinTicket) -> GuardedWrite {
        self.join_write_guard(ticket, RefusedWrite::Create, vec![ticket.creator_id])
    }

    fn move_guard(&self, ticket: JoinTicket, channel: Snowflake) -> GuardedWrite {
        self.join_write_guard(ticket, RefusedWrite::Move, vec![ticket.creator_id, channel])
    }

    fn join_write_guard(
        &self,
        ticket: JoinTicket,
        write: RefusedWrite,
        channels: Vec<Snowflake>,
    ) -> GuardedWrite {
        let live = self.clone();
        let permission_failure = Arc::new(Mutex::new(None));
        let observed = Arc::clone(&permission_failure);
        let check = Arc::new(move || {
            let state = live.inner.read().expect("live voice lock");
            // Lost authority or a member who left is not a permission finding.
            if !state.ticket_valid(ticket) {
                return false;
            }
            for channel_id in &channels {
                let Some(permissions) = state.permissions(live.guild_id, *channel_id) else {
                    return false;
                };
                if !can_manage_room(Some(permissions)) {
                    *observed.lock().expect("voice guard lock") =
                        Some(LifecycleFailure::MissingPermission {
                            write,
                            channel_id: *channel_id,
                            findings: write_permission_findings(&state, live.guild_id, *channel_id),
                        });
                    return false;
                }
            }
            true
        });
        GuardedWrite {
            check,
            permission_failure,
        }
    }

    /// Guard for a passed vote's writes: evidence must be authoritative and the
    /// room channel must still exist. The target having left is not a reason to
    /// skip the room-scoped Connect deny.
    fn room_guard(&self, channel: Snowflake) -> WriteGuard {
        let live = self.clone();
        Arc::new(move || {
            let state = live.inner.read().expect("live voice lock");
            state.ready && state.channels.contains_key(&channel)
        })
    }

    /// Guard for a passed vote's disconnect. A disconnect drops the member from
    /// whatever voice channel they are in when it lands, so it is only allowed
    /// while the member is still in the vote's room, evaluated at send time. A
    /// target that moved on is skipped, never disconnected from another channel.
    fn member_in_room_guard(&self, channel: Snowflake, member: Snowflake) -> WriteGuard {
        let live = self.clone();
        Arc::new(move || {
            let state = live.inner.read().expect("live voice lock");
            state.ready
                && state
                    .members
                    .get(&member)
                    .is_some_and(|current| current.channel_id == Some(channel))
        })
    }
}

/// Role and member overwrites of one cached channel, in the health core's
/// shape. @everyone is the role whose id is the guild id.
fn health_overwrites(guild_id: Snowflake, channel: &Channel) -> Vec<HealthOverwrite> {
    channel
        .permission_overwrites
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter_map(|overwrite| {
            let target = match overwrite.kind {
                PermissionOverwriteType::Role if overwrite.id.get() == guild_id => {
                    OverwriteTarget::Everyone
                }
                PermissionOverwriteType::Role => OverwriteTarget::Role(overwrite.id.get()),
                PermissionOverwriteType::Member => OverwriteTarget::Member(overwrite.id.get()),
                _ => return None,
            };
            Some(HealthOverwrite {
                target,
                allow: overwrite.allow.bits(),
                deny: overwrite.deny.bits(),
            })
        })
        .collect()
}

fn permission_name(permission: VoicePermission) -> &'static str {
    match permission {
        VoicePermission::ManageChannels => "Manage Channels",
        VoicePermission::MoveMembers => "Move Members",
        VoicePermission::ManageRoles => "Manage Roles",
        VoicePermission::ViewChannel => "View Channel",
        VoicePermission::Connect => "Connect",
    }
}

/// What removes one permission from the bot, naming the category or channel
/// override responsible. Ids only: the category or channel is mentioned,
/// never named.
fn finding_clause(finding: &PermissionFinding) -> String {
    let permission = permission_name(finding.permission);
    match (finding.scope, finding.category_id, finding.channel_id) {
        (VoicePermissionScope::Category, Some(category), _) => format!(
            "the permission override on category <#{category}> removes {permission} from the bot"
        ),
        (VoicePermissionScope::Channel, _, Some(channel)) => {
            format!("the permission override on <#{channel}> removes {permission} from the bot")
        }
        _ => format!("the bot lacks {permission} for the whole server"),
    }
}

/// One `/setup` line for a missing permission. Ids only: the category or
/// channel is mentioned, never named.
#[must_use]
pub fn health_line(finding: &PermissionFinding) -> String {
    format!("health: {}", finding_clause(finding))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleFailure {
    CategoryFull {
        creator_id: Snowflake,
        message: String,
    },
    /// The durable admission limits (caps, cooldown, rolling burst) refused a
    /// create before any Discord call. `reason.code()` is the stable legacy
    /// code; `message` is the legacy user-facing text for it.
    CreateRefused {
        creator_id: Snowflake,
        reason: RefusalReason,
        message: String,
    },
    Discord {
        channel_id: Snowflake,
        error: RoomHttpError,
    },
    Persistence {
        channel_id: Option<Snowflake>,
        error: StoreError,
    },
    /// Discord (or the live permission cache) refused a join-time write for a
    /// missing permission. `findings` captures the refused operation's actual
    /// permissions and causal surface; empty when a Discord refusal cannot be
    /// explained by the cache (for example, stale roles).
    MissingPermission {
        write: RefusedWrite,
        channel_id: Snowflake,
        findings: Vec<PermissionFinding>,
    },
    /// The joiner's room name, and even the bare template, is blocked by the
    /// automod name filter: no room was created. An operator misconfiguration
    /// to fix, not a member to punish.
    NameBlocked {
        creator_id: Snowflake,
        error: NameError,
    },
}

/// The join-time Discord write that was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusedWrite {
    /// Creating the room channel (needs Manage Channels).
    Create,
    /// Moving the joiner into the new room (needs Move Members).
    Move,
}

#[derive(Debug, Clone)]
struct Creation {
    ticket: JoinTicket,
    spec: NewRoomSpec,
    /// The durable admission reservation, once claimed. Kept across a 429
    /// requeue so the retry never claims (and counts) a second slot.
    reservation: Option<String>,
}

/// One mutable worker per guild. `load` must succeed before use. `reconcile`
/// responds to complete snapshots, channel/role changes and voice transitions;
/// `dispatch_one` is driven by the actor's timer, not by the gateway itself.
pub struct GuildRoomWorker<S, H> {
    live: LiveGuild,
    store: S,
    http: H,
    creators: HashMap<Snowflake, CreatorChannel>,
    access: AccessControls,
    /// Caps and cooldown for the durable create admission; the rolling burst
    /// limits are fixed legacy constants.
    admission: CreateAdmissionConfig,
    rooms: HashMap<Snowflake, VoiceRoom>,
    /// Companion records by room (V9c), loaded from the store. The row is
    /// the durable intent: its settings snapshot rebuilds the plan after a
    /// restart, so later `/textchannels` changes never alter this channel.
    companions: HashMap<Snowflake, TextCompanion>,
    /// Per-companion Discord text channel ids, for overwrite guards.
    companion_channels: HashMap<Snowflake, Snowflake>,
    /// Rooms whose companion channel exists on Discord but whose row write
    /// failed: the create retry persists the row instead of treating the
    /// in-memory record as already durable.
    unpersisted_companions: HashSet<Snowflake>,
    /// Last occupancy each companion's overwrites were synced to (V9c).
    /// Compared against live occupancy via [`occupancy_diff`] on reconcile.
    companion_seen: HashMap<Snowflake, Vec<Snowflake>>,
    queue: ActionQueue,
    renames: RenameCoalescer,
    desired_names: HashMap<Snowflake, String>,
    /// V3 `/name` overrides by room: the owner's text as typed, template
    /// tokens intact. A room without an entry uses its template name.
    custom_names: HashMap<Snowflake, String>,
    creations: HashMap<u64, Creation>,
    accepted: HashMap<Snowflake, (u64, u64)>,
    moves: HashMap<Snowflake, JoinTicket>,
    uncertain_moves: HashMap<Snowflake, JoinTicket>,
    deletes: HashSet<Snowflake>,
    compensation: HashSet<Snowflake>,
    /// A failed atomic room persist leaves this durable cap hold unsettled.
    /// Release it only after the compensation delete/absence is confirmed.
    compensation_reservations: HashMap<Snowflake, String>,
    denied: HashMap<Snowflake, (u64, Option<Permissions>)>,
    failures: VecDeque<LifecycleFailure>,
    notices: Vec<NoticeState>,
    halted: bool,
    votes: VoteKickCore,
    /// Every vote started this session, by its initiating interaction ID. A
    /// button carries only that ID; guild, room and target come from here, never
    /// from the payload.
    vote_refs: HashMap<Snowflake, VoteKickRef>,
    active_votes: Vec<VoteKickRef>,
    /// Automod policy the create-path name filter runs under. The default
    /// policy still blocks invite and external links; the runtime installs the
    /// configured word list through [`GuildRoomWorker::with_name_policy`].
    name_policy: Arc<AutomodPolicy>,
}

/// Why `/kick` could not start or accept a ballot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KickRefusal {
    /// Live voice evidence is not authoritative (disconnected or not yet
    /// published), so no occupancy-based decision may be made.
    Unavailable,
    /// The channel is not a tracked temporary room.
    NotARoom,
    Vote(VoteKickError),
}

/// Monotonic actor time handed to the vote core.
struct ActorClock(u64);

impl VoteClock for ActorClock {
    fn now_ms(&self) -> u64 {
        self.0
    }
}

/// Minimum gap between notices about the same failure: one initial notice plus
/// two repeats ([`two_bot_core::voice_logging::MAX_LOG_SENDS`]) span half an hour.
const NOTICE_REPEAT_INTERVAL_MS: u64 = 15 * 60 * 1000;

/// Longest notice body; Discord's message limit is 2000 characters.
const NOTICE_MAX_CHARS: usize = 1500;

/// Repeat bookkeeping for one tracked failure. `last_attempt_ms` is the
/// actor's monotonic clock, so a restart begins a fresh budget.
#[derive(Debug, Clone)]
struct NoticeState {
    failure: LifecycleFailure,
    ledger: RepeatLedger,
    last_attempt_ms: Option<u64>,
}

impl<S: RoomPersistence, H: RoomWrites> GuildRoomWorker<S, H> {
    pub async fn load(live: LiveGuild, store: S, http: H) -> Result<Self, StoreError> {
        let creators = store
            .creators(live.guild_id)
            .await?
            .into_iter()
            .map(|c| (c.channel_id, c))
            .collect();
        let access = store.access_controls(live.guild_id).await?;
        let rooms = store
            .rooms(live.guild_id)
            .await?
            .into_iter()
            .map(|r| (r.channel_id, r))
            .collect();
        let companions = store
            .companions(live.guild_id)
            .await?
            .into_iter()
            .map(|c| (c.room_channel_id, c))
            .collect();
        // Creates whose persist or compensation never finished (a crash, or a
        // refused delete) keep their cap slot and their channel witness; the
        // first authoritative reconcile deletes or confirms absence of each.
        let mut compensation = HashSet::new();
        let mut compensation_reservations = HashMap::new();
        for (reservation, channel) in store.orphaned_create_channels(live.guild_id).await? {
            compensation.insert(channel);
            compensation_reservations.insert(channel, reservation);
        }
        let custom_names = store
            .custom_names(live.guild_id)
            .await?
            .into_iter()
            .collect();
        Ok(Self {
            live,
            store,
            http,
            creators,
            access,
            admission: CreateAdmissionConfig::default(),
            rooms,
            companions,
            companion_channels: HashMap::new(),
            unpersisted_companions: HashSet::new(),
            companion_seen: HashMap::new(),
            queue: ActionQueue::new(),
            renames: RenameCoalescer::new(),
            desired_names: HashMap::new(),
            custom_names,
            creations: HashMap::new(),
            accepted: HashMap::new(),
            moves: HashMap::new(),
            uncertain_moves: HashMap::new(),
            deletes: HashSet::new(),
            compensation,
            compensation_reservations,
            denied: HashMap::new(),
            failures: VecDeque::new(),
            notices: Vec::new(),
            halted: false,
            votes: VoteKickCore::new(),
            vote_refs: HashMap::new(),
            active_votes: Vec::new(),
            name_policy: Arc::new(AutomodPolicy::default()),
        })
    }

    /// Run the create-path name filter under `policy` from here on.
    #[must_use]
    pub fn with_name_policy(mut self, policy: Arc<AutomodPolicy>) -> Self {
        self.name_policy = policy;
        self
    }

    /// Accept one join-to-create ticket. `display` is the joiner's display
    /// name: the room name is rendered from it and passed through the automod
    /// name filter before anything is queued. A name containing a blocked term
    /// is retried without the username; when even the bare template is
    /// blocked, no create is queued and the refusal is recorded as
    /// [`LifecycleFailure::NameBlocked`]. Returns whether a create was queued.
    pub fn accept_join(
        &mut self,
        ticket: JoinTicket,
        display: &str,
        seed: u64,
        created_at: String,
    ) -> bool {
        if self.halted
            || !may_create_room(&self.access)
            || !self.creators.contains_key(&ticket.creator_id)
            || !self
                .live
                .inner
                .read()
                .expect("live voice lock")
                .ticket_valid(ticket)
            || self
                .accepted
                .get(&ticket.member_id)
                .is_some_and(|seen| *seen >= (ticket.generation, ticket.transition))
        {
            return false;
        }
        self.accepted
            .insert(ticket.member_id, (ticket.generation, ticket.transition));
        let context = NameFilterContext {
            guild_id: self.live.guild_id.to_string(),
            channel_id: ticket.creator_id.to_string(),
            user_id: ticket.member_id.to_string(),
        };
        let name = match resolve_room_name(display, &self.name_policy, &context) {
            Ok(resolved) => resolved.name,
            Err(blocked) => {
                self.record(LifecycleFailure::NameBlocked {
                    creator_id: ticket.creator_id,
                    error: blocked.error,
                });
                return false;
            }
        };
        let id = self.queue.enqueue(
            self.live.guild_id,
            RoomAction::CreateRoom {
                creator_channel_id: ticket.creator_id,
                owner_id: ticket.member_id,
                name,
                seed,
            },
        );
        self.creations.insert(
            id,
            Creation {
                ticket,
                spec: NewRoomSpec {
                    guild_id: self.live.guild_id,
                    creator_channel_id: ticket.creator_id,
                    owner_id: ticket.member_id,
                    seed,
                    created_at,
                },
                reservation: None,
            },
        );
        true
    }

    /// Replace the caps and cooldown used by the durable create admission
    /// (legacy defaults: 1 room per member, 40 per guild, 30 s cooldown).
    pub fn set_admission_config(&mut self, config: CreateAdmissionConfig) {
        self.admission = config;
    }

    fn queue_delete(&mut self, channel: Snowflake, compensate: bool) {
        if compensate {
            self.compensation.insert(channel);
        }
        if self.deletes.insert(channel) {
            self.queue.enqueue(
                self.live.guild_id,
                RoomAction::DeleteRoom {
                    channel_id: channel,
                },
            );
        }
    }

    /// IDs that keep companion View even after leaving the room: the viewer
    /// role (when set), every Manage Channels admin role (V9d AC7) and the
    /// bot itself (V9 AC6). Admin roles are resolved live, so a role promoted
    /// later is protected without touching existing rooms. Takes the live
    /// state the caller already holds: re-reading the lock here could
    /// deadlock behind a queued gateway publish.
    fn protected_ids(&self, live: &LiveState, companion: &TextCompanion) -> Vec<Snowflake> {
        let mut protected = Vec::with_capacity(3);
        if let Some(viewer) = companion.settings.viewer_role_id {
            protected.push(viewer);
        }
        protected.extend(live.admin_role_ids(self.live.guild_id));
        if let Some(bot) = live.bot.as_ref().map(|bot| bot.member_id) {
            protected.push(bot);
        }
        protected
    }

    /// The Discord text channel id for a companion record: the live session's
    /// id once created, falling back to the stored row.
    fn companion_channel(&self, companion: &TextCompanion) -> Option<Snowflake> {
        self.companion_channels
            .get(&companion.room_channel_id)
            .copied()
            .or(if companion.text_channel_id == 0 {
                None
            } else {
                Some(companion.text_channel_id)
            })
    }

    /// Plan the companion for a freshly created room from the creator row at
    /// room-creation time, and enqueue its create through the same per-guild
    /// ordered lane. Nothing is planned when the toggle is off. The plan's
    /// settings snapshot rides the action so later setting changes never
    /// alter this channel.
    fn enqueue_companion_create(&mut self, channel_id: Snowflake) {
        let Some(room) = self.rooms.get(&channel_id) else {
            return;
        };
        let Some(creator) = self.creators.get(&room.creator_channel_id) else {
            return;
        };
        let settings = creator.text_channel_settings();
        if !settings.enabled {
            return;
        }
        let live = self.live.inner.read().expect("live voice lock");
        let Some(channel) = live.channels.get(&channel_id) else {
            return;
        };
        let category_id = match channel.parent_id {
            Some(parent) => parent.get(),
            None => return,
        };
        let occupants: Vec<Snowflake> = live
            .members
            .iter()
            .filter(|(_, member)| member.channel_id == Some(channel_id) && member.bot != Some(true))
            .map(|(member_id, _)| *member_id)
            .collect();
        // Manage Channels admins ride `Role` View allows, resolved live so a
        // later promotion is covered. Administrator roles and the guild owner
        // bypass overwrites; occupying admins also get the occupant grants.
        let admin_role_ids = live.admin_role_ids(self.live.guild_id);
        let admin_ids: &[Snowflake] = &[];
        let facts = VoiceRoomFacts {
            guild_id: self.live.guild_id,
            room_id: channel_id,
            category_id,
            occupants: &occupants,
            admin_ids,
            admin_role_ids: &admin_role_ids,
        };
        let Some(plan) = text_channel_plan(&settings, &facts) else {
            return;
        };
        drop(live);
        self.queue.enqueue(
            self.live.guild_id,
            RoomAction::CreateCompanion {
                room_channel_id: channel_id,
                plan,
            },
        );
    }

    pub fn reconcile(&mut self) {
        if self.halted {
            return;
        }
        let live = self.live.inner.read().expect("live voice lock");
        if !live.ready {
            return;
        }
        let mut empty = Vec::new();
        let mut occupied = Vec::new();
        let mut suspended: u64 = 0;
        let mut resumed: u64 = 0;
        // Held creates found at load have no tracked room: they still need the
        // same access evidence and delete/absence confirmation as a room.
        let held: Vec<Snowflake> = self
            .rooms
            .keys()
            .copied()
            .chain(
                self.compensation_reservations
                    .keys()
                    .copied()
                    .filter(|channel| !self.rooms.contains_key(channel)),
            )
            .collect();
        for channel in held {
            if !live.channels.contains_key(&channel) {
                self.queue.resume(self.live.guild_id, channel);
                resumed = resumed.saturating_add(1);
                empty.push(channel);
                continue;
            }
            let permissions = live.permissions(self.live.guild_id, channel);
            let evidence = (live.generation, permissions);
            if self.denied.get(&channel) == Some(&evidence) {
                continue;
            }
            self.denied.remove(&channel);
            let accessible = if self.compensation.contains(&channel) {
                permissions.is_some_and(|p| {
                    p.contains(Permissions::VIEW_CHANNEL | Permissions::MANAGE_CHANNELS)
                })
            } else {
                can_manage_room(permissions)
            };
            // Pending moves must reach their failure/compensation path even if
            // Move Members was revoked between the create and move.
            if !accessible && !self.moves.contains_key(&channel) {
                self.queue.suspend(self.live.guild_id, channel);
                suspended = suspended.saturating_add(1);
                continue;
            }
            self.queue.resume(self.live.guild_id, channel);
            resumed = resumed.saturating_add(1);
            let move_pending = self.moves.contains_key(&channel)
                || self
                    .uncertain_moves
                    .get(&channel)
                    .is_some_and(|ticket| live.ticket_valid(*ticket));
            if live.humans(channel) == 0 && !move_pending {
                empty.push(channel);
            } else if live.humans(channel) > 0 && !move_pending {
                occupied.push(channel);
            }
        }
        let mut view_syncs = Vec::new();
        for channel in self.rooms.keys().copied() {
            if !live.channels.contains_key(&channel) {
                continue;
            }
            let Some(companion) = self.companions.get(&channel) else {
                continue;
            };
            let Some(text_channel_id) = self.companion_channel(companion) else {
                continue;
            };
            if !live.channels.contains_key(&text_channel_id) {
                continue;
            }
            // Suspended (access-lost) rooms skip view syncs along with the
            // rest of their lane: grants/revokes enqueue once access returns
            // and the diff is recomputed from the last synced occupancy.
            if self.queue.is_suspended(self.live.guild_id, channel) {
                continue;
            }
            let current: Vec<Snowflake> = live
                .members
                .iter()
                .filter(|(_, member)| {
                    member.channel_id == Some(channel) && member.bot != Some(true)
                })
                .map(|(member_id, _)| *member_id)
                .collect();
            let before = self
                .companion_seen
                .get(&channel)
                .cloned()
                .unwrap_or_default();
            let protected = self.protected_ids(&live, companion);
            let diff = occupancy_diff(&before, &current, &protected);
            if !diff.grants.is_empty() || !diff.revokes.is_empty() {
                view_syncs.push((channel, text_channel_id, diff));
            }
            self.companion_seen.insert(channel, current);
        }
        drop(live);
        let delete_enqueued = empty.len() as u64;
        for channel in empty {
            self.queue_delete(channel, false);
        }
        for (channel, text_channel_id, diff) in view_syncs {
            for member_id in diff.grants {
                self.queue.enqueue(
                    self.live.guild_id,
                    RoomAction::GrantCompanionView {
                        room_channel_id: channel,
                        text_channel_id,
                        member_id,
                    },
                );
            }
            for member_id in diff.revokes {
                self.queue.enqueue(
                    self.live.guild_id,
                    RoomAction::RevokeCompanionView {
                        room_channel_id: channel,
                        text_channel_id,
                        member_id,
                    },
                );
            }
        }
        let mut succession_enqueued: u64 = 0;
        for channel in occupied {
            if self.apply_succession(channel) {
                succession_enqueued = succession_enqueued.saturating_add(1);
            }
        }
        metrics::global().voice_reconcile("delete_enqueued", delete_enqueued);
        metrics::global().voice_reconcile("suspended", suspended);
        metrics::global().voice_reconcile("resumed", resumed);
        metrics::global().voice_reconcile("succession_enqueued", succession_enqueued);
        self.observe_voice_state();
        if delete_enqueued > 0 || suspended > 0 || resumed > 0 || succession_enqueued > 0 {
            info!(
                voice_event = "voice_reconcile",
                delete_enqueued = delete_enqueued,
                suspended = suspended,
                resumed = resumed,
                succession_enqueued = succession_enqueued,
                "voice_reconcile planned"
            );
        }
    }

    /// V2 caretaker succession: when the tracked owner is gone but humans
    /// remain, hand the room to the longest-present occupant (spec
    /// `docs/voice-rooms.md` §V2). The worker row updates first so the next
    /// tick is idempotent; the queued [`RoomAction::UpdateOwnership`] persists
    /// the handoff. Skipped while the owner's move is still in flight and
    /// while occupancy is uncertain after a successful move.
    fn apply_succession(&mut self, channel: Snowflake) -> bool {
        if self.moves.contains_key(&channel) {
            return false;
        }
        let (room, occupants) = {
            let live = self.live.inner.read().expect("live voice lock");
            if !live.ready {
                return false;
            }
            let Some(room) = self.rooms.get(&channel).cloned() else {
                return false;
            };
            if self
                .uncertain_moves
                .get(&channel)
                .is_some_and(|ticket| live.ticket_valid(*ticket))
            {
                return false;
            }
            (room, live.ownership_snapshot(channel))
        };
        if occupants.is_empty()
            || occupants
                .iter()
                .any(|member| member.member_id == room.owner_id)
        {
            return false;
        }
        let ownership = RoomOwnership {
            owner_id: room.owner_id,
            original_creator_id: room.original_creator_id,
        };
        let next = match decide_ownership(ownership, &occupants, OwnershipRequest::Reconcile) {
            Ok(OwnershipDecision::Changed { next, .. }) => next,
            Ok(_) => return false,
            Err(error) => {
                warn!(channel_id = channel, %error, "voice succession refused");
                return false;
            }
        };
        let mut updated = room;
        updated.owner_id = next.owner_id;
        updated.original_creator_id = next.original_creator_id;
        self.rooms.insert(channel, updated);
        self.queue.enqueue(
            self.live.guild_id,
            RoomAction::UpdateOwnership {
                channel_id: channel,
                owner_id: next.owner_id,
                original_creator_id: next.original_creator_id,
            },
        );
        true
    }

    /// V2 ownership command (`/reclaim`, `/transfer`), serialized in the guild
    /// actor: resolve the caller's current room, decide with the pure core,
    /// apply the handoff to the worker row and persist it through the urgent
    /// [`RoomAction::UpdateOwnership`] lane. Returns the ephemeral reply text.
    /// Refusals change nothing. Both commands act on the caller's current
    /// room, except an admin invoking `/transfer` without a tracked room of
    /// their own, who may name any occupied room by its recipient. A
    /// membership racing the decision heals on the next `reconcile`
    /// succession pass; a retried interaction replays safely (reclaim is
    /// idempotent, a former owner's transfer replay fails authorization), so
    /// no interaction-ID dedupe is needed here.
    fn apply_ownership(
        &mut self,
        actor_id: Snowflake,
        is_admin: bool,
        command: OwnershipCommand,
    ) -> String {
        if self.halted {
            return "Voice rooms are paused: Discord refused the bot credential. \
                    Fix the token, then restart the bot."
                .to_owned();
        }
        let request = match command {
            OwnershipCommand::Reclaim => OwnershipRequest::Reclaim {
                member_id: actor_id,
            },
            OwnershipCommand::Transfer { target_id } => OwnershipRequest::Transfer {
                actor: RoomActor {
                    member_id: actor_id,
                    is_admin,
                },
                target_id,
            },
        };
        let (channel, room, occupants) = {
            let live = self.live.inner.read().expect("live voice lock");
            if !live.ready {
                return "The voice worker isn't warmed up yet — try again in a moment.".to_owned();
            }
            // Admins may use owner commands in any room: an admin invoking
            // `/transfer` with no tracked room of their own falls back to the
            // recipient's room (for example, an admin parked in an untracked
            // channel). Ordinary members only ever act on the room they are
            // in, so any untracked-channel failure is reported as-is.
            let own = live
                .members
                .get(&actor_id)
                .and_then(|member| member.channel_id);
            let channel = match (own, command, is_admin) {
                (Some(channel), _, _) if self.rooms.contains_key(&channel) => Some(channel),
                (Some(_), OwnershipCommand::Reclaim, _) => own,
                (Some(_), OwnershipCommand::Transfer { .. }, false) => own,
                (_, OwnershipCommand::Transfer { target_id }, true) => live
                    .members
                    .get(&target_id)
                    .and_then(|member| member.channel_id),
                (None, _, _) => None,
            };
            let Some(channel) = channel else {
                return match command {
                    OwnershipCommand::Reclaim => {
                        "You need to be in a voice room to use /reclaim.".to_owned()
                    }
                    OwnershipCommand::Transfer { .. } => {
                        "You need to be in a voice room to use /transfer.".to_owned()
                    }
                };
            };
            let Some(room) = self.rooms.get(&channel).cloned() else {
                return "That voice channel isn't a temporary room I manage.".to_owned();
            };
            (channel, room, live.ownership_snapshot(channel))
        };
        let ownership = RoomOwnership {
            owner_id: room.owner_id,
            original_creator_id: room.original_creator_id,
        };
        let was_creator = room.original_creator_id == actor_id;
        let next = match decide_ownership(ownership, &occupants, request) {
            Ok(OwnershipDecision::Unchanged(_)) => {
                return match command {
                    OwnershipCommand::Reclaim => {
                        "You're already the owner of this room.".to_owned()
                    }
                    OwnershipCommand::Transfer { target_id } => {
                        format!("Ownership is already with <@{target_id}>.")
                    }
                };
            }
            Ok(OwnershipDecision::Changed { next, .. }) => next,
            Ok(OwnershipDecision::EmptyRoom) | Err(OwnershipError::EmptyRoom) => {
                return "There's nobody in this room right now.".to_owned();
            }
            Err(error) => return ownership_refusal(error),
        };
        let mut updated = room;
        updated.owner_id = next.owner_id;
        updated.original_creator_id = next.original_creator_id;
        self.rooms.insert(channel, updated);
        // The occupants snapshot just proved this room needs no succession, so
        // any lingering post-move uncertainty is stale — clear it so the next
        // reconcile (and the persisted handoff) stop skipping this channel.
        self.uncertain_moves.remove(&channel);
        self.queue.enqueue(
            self.live.guild_id,
            RoomAction::UpdateOwnership {
                channel_id: channel,
                owner_id: next.owner_id,
                original_creator_id: next.original_creator_id,
            },
        );
        match command {
            OwnershipCommand::Reclaim if was_creator => {
                "You're the owner of this room again.".to_owned()
            }
            OwnershipCommand::Reclaim => "The room's owner is gone, so it's yours now.".to_owned(),
            OwnershipCommand::Transfer { target_id } => {
                format!("Transferred ownership of this room to <@{target_id}>.")
            }
        }
    }

    pub fn propose_name(
        &mut self,
        channel: Snowflake,
        name: &str,
        now_ms: u64,
    ) -> Option<ProposeOutcome> {
        if !self.rooms.contains_key(&channel) {
            return None;
        }
        let live = self.live.inner.read().expect("live voice lock");
        let current = live
            .channels
            .get(&channel)?
            .name
            .as_deref()
            .unwrap_or_default();
        self.desired_names.insert(channel, name.to_owned());
        Some(self.renames.propose(channel, current, name, now_ms))
    }

    /// The tracked temporary room a member is currently in, if any. `None`
    /// while live evidence is not authoritative, when the member is not in
    /// voice, or when their channel is not a tracked room. The router claim
    /// check uses this to tell vote-kick targets from moderation targets.
    pub fn kick_room_of(&self, member: Snowflake) -> Option<Snowflake> {
        let live = self.live.inner.read().expect("live voice lock");
        if !live.ready || self.halted {
            return None;
        }
        let channel = live.members.get(&member)?.channel_id?;
        self.rooms.contains_key(&channel).then_some(channel)
    }

    /// Owner, original creator and human occupants of a tracked room. `None`
    /// unless live evidence is authoritative and the room is tracked.
    fn kick_facts(
        &self,
        room_id: Snowflake,
    ) -> Result<(Snowflake, Snowflake, Vec<Snowflake>), KickRefusal> {
        let live = self.live.inner.read().expect("live voice lock");
        if !live.ready || self.halted {
            return Err(KickRefusal::Unavailable);
        }
        let room = self.rooms.get(&room_id).ok_or(KickRefusal::NotARoom)?;
        Ok((
            room.owner_id,
            room.original_creator_id,
            live.occupants(room_id),
        ))
    }

    /// Fold a vote update into worker state: forget finished votes and queue the
    /// room-scoped enforcement exactly once (the core emits the decision only on
    /// the first transition to passed).
    fn settle_vote(&mut self, update: VoteKickUpdate) -> VoteKickUpdate {
        if update.status != VoteKickStatus::Active {
            self.active_votes.retain(|vote| vote.id != update.vote.id);
        }
        if let Some(kick) = update.kick {
            self.queue.enqueue(
                self.live.guild_id,
                RoomAction::KickMember {
                    channel_id: kick.room_id,
                    member_id: kick.target_id,
                },
            );
        }
        update
    }

    /// Start a vote in `room_id`. `vote_id` must be the unique initiating
    /// interaction ID. Starting casts no ballot.
    pub fn kick_start(
        &mut self,
        vote_id: Snowflake,
        room_id: Snowflake,
        initiator_id: Snowflake,
        target_id: Snowflake,
        now_ms: u64,
    ) -> Result<VoteKickUpdate, KickRefusal> {
        let (owner_id, original_creator_id, occupants) = self.kick_facts(room_id)?;
        let facts = VoteRoomFacts {
            guild_id: self.live.guild_id,
            room_id,
            owner_id,
            original_creator_id,
            occupants: &occupants,
        };
        let update = self
            .votes
            .start(vote_id, facts, initiator_id, target_id, &ActorClock(now_ms))
            .map_err(KickRefusal::Vote)?;
        self.vote_refs.insert(vote_id, update.vote);
        self.active_votes.push(update.vote);
        Ok(self.settle_vote(update))
    }

    /// Cast one ballot button press, addressed by vote ID. The core rejects
    /// repeats and voters who are not current occupants other than the target.
    pub fn kick_cast(
        &mut self,
        vote_id: Snowflake,
        voter_id: Snowflake,
        ballot: VoteBallot,
        now_ms: u64,
    ) -> Result<VoteKickUpdate, KickRefusal> {
        let reference = *self
            .vote_refs
            .get(&vote_id)
            .ok_or(KickRefusal::Vote(VoteKickError::UnknownVote))?;
        let (owner_id, original_creator_id, occupants) = self.kick_facts(reference.room_id)?;
        let facts = VoteRoomFacts {
            guild_id: self.live.guild_id,
            room_id: reference.room_id,
            owner_id,
            original_creator_id,
            occupants: &occupants,
        };
        let update = self
            .votes
            .cast(reference, facts, voter_id, ballot, &ActorClock(now_ms))
            .map_err(KickRefusal::Vote)?;
        Ok(self.settle_vote(update))
    }

    /// Timer entry: expire votes and react to roster, ownership and room-delete
    /// changes. Returns the updates that finished a vote. Skipped while live
    /// evidence is not authoritative: a stale roster must not cancel a vote.
    pub fn kick_refresh(&mut self, now_ms: u64) -> Vec<VoteKickUpdate> {
        if self.active_votes.is_empty() {
            return Vec::new();
        }
        let ready = self.live.inner.read().expect("live voice lock").ready;
        if !ready || self.halted {
            return Vec::new();
        }
        let mut finished = Vec::new();
        for reference in self.active_votes.clone() {
            let (owner_id, original_creator_id, occupants) =
                match self.kick_facts(reference.room_id) {
                    Ok(facts) => facts,
                    // Evidence went stale mid-pass: leave the vote untouched.
                    Err(KickRefusal::Unavailable) => continue,
                    // A room that is gone has no occupants.
                    Err(KickRefusal::NotARoom | KickRefusal::Vote(_)) => Default::default(),
                };
            let facts = VoteRoomFacts {
                guild_id: self.live.guild_id,
                room_id: reference.room_id,
                owner_id,
                original_creator_id,
                occupants: &occupants,
            };
            // A room that is gone has no occupants, so the core cancels the vote.
            let Ok(update) = self.votes.refresh(reference, facts, &ActorClock(now_ms)) else {
                self.active_votes.retain(|vote| vote.id != reference.id);
                continue;
            };
            let update = self.settle_vote(update);
            if update.status != VoteKickStatus::Active {
                finished.push(update);
            }
        }
        finished
    }

    pub fn failures(&self) -> &VecDeque<LifecycleFailure> {
        &self.failures
    }
    pub fn halted(&self) -> bool {
        self.halted
    }
    pub fn tracked(&self) -> &HashMap<Snowflake, VoiceRoom> {
        &self.rooms
    }

    /// Send at most one due error notice per call, per the guild's
    /// `/logging` settings. Each failure is noticed once plus two repeats at
    /// least `NOTICE_REPEAT_INTERVAL_MS` apart, then stays listed in
    /// `/setup` but silent. Every delivery attempt counts, delivered or not,
    /// so an unreachable guild cannot cause unbounded REST traffic. Returns
    /// whether an attempt was made.
    pub async fn send_notices(&mut self, now_ms: u64) -> bool {
        if self.halted {
            return false;
        }
        let failures = &self.failures;
        self.notices
            .retain(|state| failures.contains(&state.failure));
        let due = self.failures.iter().find(|failure| {
            match self.notices.iter().find(|state| &state.failure == *failure) {
                None => true,
                Some(state) => {
                    state.ledger.should_send()
                        && match state.last_attempt_ms {
                            None => true,
                            Some(last) => now_ms.saturating_sub(last) >= NOTICE_REPEAT_INTERVAL_MS,
                        }
                }
            }
        });
        let Some(failure) = due.cloned() else {
            return false;
        };
        let settings = match self.store.logging_settings(self.live.guild_id).await {
            Ok(settings) => settings,
            Err(error) => {
                // Unreadable settings: send nothing, look again next interval.
                warn!(
                    guild = self.live.guild_id,
                    ?error,
                    "voice notice settings unreadable"
                );
                self.note_attempt(&failure, now_ms, false);
                return false;
            }
        };
        if !should_log(settings.level, false) {
            self.note_attempt(&failure, now_ms, false);
            return false;
        }
        let text = notice_text(&failure, settings.level);
        let delivered = self.deliver_notice(&settings, &text).await;
        if !delivered {
            warn!(
                guild = self.live.guild_id,
                "voice notice had no working destination"
            );
        }
        self.note_attempt(&failure, now_ms, true);
        true
    }

    fn note_attempt(&mut self, failure: &LifecycleFailure, now_ms: u64, counted: bool) {
        let index = match self
            .notices
            .iter()
            .position(|state| &state.failure == failure)
        {
            Some(index) => index,
            None => {
                self.notices.push(NoticeState {
                    failure: failure.clone(),
                    ledger: RepeatLedger::new(),
                    last_attempt_ms: None,
                });
                self.notices.len() - 1
            }
        };
        let state = &mut self.notices[index];
        state.last_attempt_ms = Some(now_ms);
        if counted {
            state.ledger.record_send();
        }
    }

    /// First working destination: the configured channel, then the guild
    /// system channel, a DM to the guild owner, and finally a creator
    /// channel's chat. A destination that fails is dropped and the next is
    /// tried.
    async fn deliver_notice(&self, settings: &LoggingSettings, text: &str) -> bool {
        let mention = settings.mention_role_id.filter(|role| *role != 0);
        if let Some(channel) = settings.channel_id.filter(|channel| *channel != 0) {
            if self
                .http
                .send_notice(NoticeTarget::Channel(channel), text, mention)
                .await
                .is_ok()
            {
                return true;
            }
        }
        let Some((system_channel_id, owner_id)) = self.live.notice_context() else {
            return false;
        };
        let mut candidates = LoggingCandidates {
            system_channel_id,
            dm_user_id: Some(owner_id),
            dm_reachable: true,
            creator_channel_id: self.creators.keys().min().copied(),
            setup_user_id: None,
        };
        while let Some(target) = resolve_log_target(candidates) {
            let (notice_target, role) = match target {
                LogTarget::SystemChannel { channel_id, .. } => {
                    (NoticeTarget::Channel(channel_id), mention)
                }
                LogTarget::DirectMessage { user_id } => {
                    (NoticeTarget::DirectMessage(user_id), None)
                }
                LogTarget::CreatorChat { channel_id } => {
                    (NoticeTarget::Channel(channel_id), mention)
                }
            };
            if self
                .http
                .send_notice(notice_target, text, role)
                .await
                .is_ok()
            {
                return true;
            }
            match target {
                LogTarget::SystemChannel { .. } => candidates.system_channel_id = None,
                LogTarget::DirectMessage { .. } => candidates.dm_reachable = false,
                LogTarget::CreatorChat { .. } => candidates.creator_channel_id = None,
            }
        }
        false
    }

    fn record(&mut self, failure: LifecycleFailure) {
        if self.failures.back() == Some(&failure) {
            return;
        }
        if self.failures.len() == 20 {
            self.failures.pop_front();
        }
        self.failures.push_back(failure);
    }

    /// Publish current ghost-verification gauges (TOG-13543): tracked rooms
    /// plus compensation-pending orphans. No IDs leave the process.
    fn observe_voice_state(&self) {
        metrics::global().voice_state(self.rooms.len() as u64, self.compensation.len() as u64);
    }

    /// Retry through the queue budget and observe the dead letter (TOG-13543).
    /// Returns the queue's release verdict. On the terminal attempt the
    /// bounded family counter advances and one token-free warn line names
    /// the family and attempt budget, never the channel, member or reason
    /// body.
    fn mark_failed_observed(&self, action: QueuedAction, reason: String, now_ms: u64) -> bool {
        let family = voice_dead_action(&action.action);
        let will_dead_letter = action.attempts.saturating_add(1) >= QUEUE_MAX_ATTEMPTS;
        let released = self.queue.mark_failed(action, reason, now_ms);
        if will_dead_letter && released {
            metrics::global().voice_dead_letter(family);
            warn!(
                voice_event = "voice_dead_letter",
                action = family,
                attempts = QUEUE_MAX_ATTEMPTS,
                "voice action dead-lettered"
            );
        }
        released
    }

    /// Release only a confirmed absent create. Errors retain the durable hold
    /// and reach the bounded setup failure report; credential refusals stop ALL
    /// subsequent writes just like claim/persist failures.
    async fn settle_reservation(&mut self, reservation: &str, channel: Option<Snowflake>) -> bool {
        match self.store.settle_create(reservation).await {
            Ok(_) => true,
            Err(error) => {
                warn!(
                    guild = self.live.guild_id,
                    ?error,
                    "voice create reservation settle failed"
                );
                self.record(LifecycleFailure::Persistence {
                    channel_id: channel,
                    error,
                });
                if error == StoreError::CredentialRefused {
                    self.halted = true;
                }
                false
            }
        }
    }

    fn prepare(&self, ticket: JoinTicket) -> Result<RoomChannelAttributes, RoomPlanError> {
        let live = self.live.inner.read().expect("live voice lock");
        if !live.ticket_valid(ticket) {
            return Err(RoomHttpError::Cancelled.into());
        }
        let settings = self
            .creators
            .get(&ticket.creator_id)
            .ok_or(RoomHttpError::Cancelled)?;
        let channel = live
            .channels
            .get(&ticket.creator_id)
            .ok_or(RoomHttpError::NotFound)?;
        let permissions = live.permissions(self.live.guild_id, ticket.creator_id);
        if !can_manage_room(permissions) {
            return Err(RoomPlanError {
                error: RoomHttpError::AccessDenied,
                source_id: Some(ticket.creator_id),
                findings: write_permission_findings(&live, self.live.guild_id, ticket.creator_id),
            });
        }
        if let Some(parent) = channel.parent_id {
            if live
                .channels
                .values()
                .filter(|c| c.parent_id == Some(parent))
                .count()
                >= MAX_CHANNELS_PER_CATEGORY
            {
                return Err(RoomHttpError::Rejected {
                    status: 400,
                    code: 50035,
                }
                .into());
            }
        }
        let bot = live.bot.as_ref().ok_or(RoomHttpError::AccessDenied)?;
        let grouped = settings.group_by_category;
        let group_room_ids = if grouped {
            category_room_ids(channel, &live.channels, &self.rooms)
        } else {
            Vec::new()
        };
        plan_room_diagnosed(&RoomPlanInput {
            guild_id: self.live.guild_id,
            owner_id: ticket.member_id,
            settings,
            creator: channel,
            channels: &live.channels,
            creators: &self.creators,
            rooms: &self.rooms,
            bot,
            bot_permissions: permissions,
            grouped,
            group_room_ids: &group_room_ids,
        })
    }

    /// Executes at most one write, releasing the guild lane on every outcome.
    /// `now_ms` is a monotonic actor clock, not a wall-clock timestamp.
    pub async fn dispatch_one(&mut self, now_ms: u64) -> bool {
        if self.halted || !self.live.inner.read().expect("live voice lock").ready {
            return false;
        }
        for (channel_id, name) in self.renames.take_due(now_ms) {
            self.queue.enqueue(
                self.live.guild_id,
                RoomAction::RenameRoom { channel_id, name },
            );
        }
        let Some(action) = self.queue.pop_due(self.live.guild_id, now_ms) else {
            return false;
        };
        let started = Instant::now();
        match action.action.clone() {
            RoomAction::CreateRoom {
                name,
                creator_channel_id,
                ..
            } => {
                let Some(creation) = self.creations.get(&action.id).cloned() else {
                    self.queue.mark_succeeded(&action);
                    return true;
                };
                let attributes = match self.prepare(creation.ticket) {
                    Ok(attributes) => attributes,
                    Err(failure) => {
                        let error = failure.error;
                        if error
                            == (RoomHttpError::Rejected {
                                status: 400,
                                code: 50035,
                            })
                        {
                            self.record(LifecycleFailure::CategoryFull {
                                creator_id: creator_channel_id,
                                message: category_full_message(),
                            });
                            observe_voice_operation("create", "category_full");
                        } else if error != RoomHttpError::Cancelled {
                            let outcome = voice_outcome_from_http(&error);
                            if error == RoomHttpError::AccessDenied {
                                self.record(LifecycleFailure::MissingPermission {
                                    write: RefusedWrite::Create,
                                    channel_id: failure
                                        .source_id
                                        .filter(|_| failure.findings.is_empty())
                                        .unwrap_or(creator_channel_id),
                                    findings: failure.findings,
                                });
                            } else {
                                self.record(LifecycleFailure::Discord {
                                    channel_id: failure.source_id.unwrap_or(creator_channel_id),
                                    error,
                                });
                            }
                            observe_voice_operation("create", outcome);
                        }
                        if let Some(held) = &creation.reservation {
                            self.settle_reservation(held, None).await;
                        }
                        self.creations.remove(&action.id);
                        self.queue.mark_succeeded(&action);
                        return true;
                    }
                };
                // Durable admission (caps, cooldown, rolling burst) runs after
                // the cheap checks and before the only Discord create call. A
                // refusal never reaches Discord; a 429 requeue keeps its claim.
                let reservation = match creation.reservation.clone() {
                    Some(held) => held,
                    None => match self
                        .store
                        .claim_create(
                            self.live.guild_id,
                            creation.ticket.member_id,
                            &self.admission,
                            unix_now_secs(),
                        )
                        .await
                    {
                        Ok(CreateClaim::Admitted { reservation_id }) => {
                            if let Some(entry) = self.creations.get_mut(&action.id) {
                                entry.reservation = Some(reservation_id.clone());
                            }
                            reservation_id
                        }
                        Ok(CreateClaim::Refused(reason)) => {
                            self.record(LifecycleFailure::CreateRefused {
                                creator_id: creator_channel_id,
                                reason,
                                message: reason.user_message(&self.admission),
                            });
                            self.creations.remove(&action.id);
                            self.queue.mark_succeeded(&action);
                            return true;
                        }
                        Err(error) => {
                            observe_voice_operation("create", voice_outcome_from_store(&error));
                            self.record(LifecycleFailure::Persistence {
                                channel_id: None,
                                error,
                            });
                            if error == StoreError::CredentialRefused {
                                self.halted = true;
                            }
                            self.creations.remove(&action.id);
                            self.queue.mark_succeeded(&action);
                            return true;
                        }
                    },
                };
                let guard = self.live.join_guard(creation.ticket);
                match self
                    .http
                    .create(
                        self.live.guild_id,
                        &name,
                        &attributes,
                        Arc::clone(&guard.check),
                    )
                    .await
                {
                    Ok(channel) => {
                        self.queue.mark_succeeded(&action);
                        self.creations.remove(&action.id);
                        // Retain the exact successful create result even when SQL fails.
                        let channel_id = channel.id.get();
                        let room = VoiceRoom::from_spec(creation.spec, channel_id);
                        self.live.upsert_channel(channel);
                        self.rooms.insert(channel_id, room.clone());
                        // The room and its reservation transfer commit together.
                        // On failure, keep the hold through compensation; neither
                        // a SQL error nor elapsed time proves channel absence.
                        // The channel is bound to its claim first so a restart
                        // can still find it if persist and compensation fail.
                        let transferred = match self
                            .store
                            .bind_create_channel(self.live.guild_id, &reservation, channel_id)
                            .await
                        {
                            Ok(()) => self.store.persist_create(&reservation, &room).await,
                            Err(error) => Err(error),
                        };
                        match transferred {
                            Ok(()) => {
                                // V9c: companion first (same ordered lane), then
                                // the move. The plan's settings snapshot is the
                                // creator row at room-creation time, so later
                                // `/textchannels` changes never alter it.
                                observe_voice_operation("create", "success");
                                self.observe_voice_state();
                                self.enqueue_companion_create(channel_id);
                                if self
                                    .live
                                    .inner
                                    .read()
                                    .expect("live voice lock")
                                    .ticket_valid(creation.ticket)
                                {
                                    self.moves.insert(channel_id, creation.ticket);
                                    self.queue.enqueue(
                                        self.live.guild_id,
                                        RoomAction::MoveMember {
                                            member_id: creation.ticket.member_id,
                                            channel_id,
                                        },
                                    );
                                } else {
                                    self.queue_delete(channel_id, true);
                                }
                            }
                            Err(error) => {
                                let outcome = voice_outcome_from_store(&error);
                                self.record(LifecycleFailure::Persistence {
                                    channel_id: Some(channel_id),
                                    error,
                                });
                                observe_voice_operation("create", outcome);
                                if error == StoreError::CredentialRefused {
                                    self.halted = true;
                                }
                                self.compensation_reservations
                                    .insert(channel_id, reservation);
                                self.queue_delete(channel_id, true);
                                self.observe_voice_state();
                            }
                        }
                    }
                    Err(RoomHttpError::RateLimited { retry_after_ms, .. }) => {
                        self.queue.mark_rate_limited(
                            self.live.guild_id,
                            retry_after_ms,
                            elapsed_ms(now_ms, started),
                            action,
                        );
                    }
                    Err(error) => {
                        // Unknown outcomes must never produce another POST or
                        // refund capacity: Discord may have created the room.
                        self.creations.remove(&action.id);
                        if error != RoomHttpError::UnknownOutcome {
                            self.settle_reservation(&reservation, None).await;
                        }
                        if error != RoomHttpError::Cancelled {
                            observe_voice_operation("create", voice_outcome_from_http(&error));
                        }
                        self.finish_join_error(
                            action,
                            creator_channel_id,
                            error,
                            RefusedWrite::Create,
                            &guard,
                        );
                    }
                }
            }
            RoomAction::MoveMember {
                member_id,
                channel_id,
            } => {
                let Some(ticket) = self.moves.get(&channel_id).copied() else {
                    self.queue.mark_succeeded(&action);
                    return true;
                };
                let guard = self.live.move_guard(ticket, channel_id);
                let result = if !self
                    .live
                    .inner
                    .read()
                    .expect("live voice lock")
                    .ticket_valid(ticket)
                {
                    Err(RoomHttpError::Cancelled)
                } else if !can_manage_room(
                    self.live
                        .inner
                        .read()
                        .expect("live voice lock")
                        .permissions(self.live.guild_id, channel_id),
                ) {
                    Err(RoomHttpError::AccessDenied)
                } else {
                    self.http
                        .move_member(
                            self.live.guild_id,
                            member_id,
                            channel_id,
                            Arc::clone(&guard.check),
                        )
                        .await
                };
                match result {
                    Ok(()) => {
                        self.queue.mark_succeeded(&action);
                        self.moves.remove(&channel_id);
                        // Occupancy may lag the successful REST response. Wait for
                        // this member's transition or a complete refresh before pruning.
                        self.uncertain_moves.insert(channel_id, ticket);
                        observe_voice_operation("move", "success");
                    }
                    Err(RoomHttpError::RateLimited { retry_after_ms, .. }) => {
                        self.queue.mark_rate_limited(
                            self.live.guild_id,
                            retry_after_ms,
                            elapsed_ms(now_ms, started),
                            action,
                        );
                    }
                    Err(RoomHttpError::UnknownOutcome) => {
                        self.moves.remove(&channel_id);
                        self.uncertain_moves.insert(channel_id, ticket);
                        observe_voice_operation("move", "discord");
                        self.complete_error(action, channel_id, RoomHttpError::UnknownOutcome);
                    }
                    Err(error) => {
                        self.moves.remove(&channel_id);
                        if error != RoomHttpError::Cancelled {
                            observe_voice_operation("move", voice_outcome_from_http(&error));
                        }
                        self.finish_join_error(
                            action,
                            channel_id,
                            error,
                            RefusedWrite::Move,
                            &guard,
                        );
                        self.queue_delete(channel_id, true);
                        self.observe_voice_state();
                    }
                }
            }
            RoomAction::DeleteRoom { channel_id } => {
                if !self.rooms.contains_key(&channel_id)
                    && !self.compensation_reservations.contains_key(&channel_id)
                {
                    self.queue.mark_succeeded(&action);
                    self.deletes.remove(&channel_id);
                    return true;
                }
                let live = self.live.clone();
                let compensate = self.compensation.contains(&channel_id);
                let guard: WriteGuard = Arc::new(move || {
                    let state = live.inner.read().expect("live voice lock");
                    state.ready
                        && state.humans(channel_id) == 0
                        && state
                            .permissions(live.guild_id, channel_id)
                            .is_some_and(|p| {
                                if compensate {
                                    p.contains(
                                        Permissions::VIEW_CHANNEL | Permissions::MANAGE_CHANNELS,
                                    )
                                } else {
                                    can_manage_room(Some(p))
                                }
                            })
                });
                let present = self
                    .live
                    .inner
                    .read()
                    .expect("live voice lock")
                    .channels
                    .contains_key(&channel_id);
                let result = if !present {
                    Ok(())
                } else {
                    self.http.delete(channel_id, guard).await
                };
                match result {
                    Ok(()) | Err(RoomHttpError::NotFound) => {
                        self.live.remove_channel(channel_id);
                        // V9c: the companion goes with its room. Its Discord
                        // delete runs first so a failed companion delete
                        // retries with the room delete instead of leaking the
                        // channel; NotFound (or no companion at all) is
                        // success, keeping the delete idempotent.
                        let companion_gone = self.delete_companion_for(channel_id).await;
                        if !companion_gone {
                            // Companion Discord delete failed (or its row
                            // write did): retry with the room delete instead
                            // of leaking the text channel.
                            self.mark_failed_observed(
                                action,
                                "companion delete unavailable".to_owned(),
                                elapsed_ms(now_ms, started),
                            );
                            return true;
                        }
                        if let Some(held) = self.compensation_reservations.get(&channel_id).cloned()
                        {
                            if !self.settle_reservation(&held, Some(channel_id)).await {
                                if self.halted {
                                    self.queue.mark_succeeded(&action);
                                } else {
                                    // The channel is known gone; retry only SQL,
                                    // keeping the hold and never creating again.
                                    self.mark_failed_observed(
                                        action,
                                        "voice create reservation unavailable".to_owned(),
                                        elapsed_ms(now_ms, started),
                                    );
                                }
                                return true;
                            }
                            self.compensation_reservations.remove(&channel_id);
                        }
                        match self.store.forget(self.live.guild_id, channel_id).await {
                            Ok(()) => {
                                self.queue.mark_succeeded(&action);
                                self.queue.drop_for_channel(self.live.guild_id, channel_id);
                                self.rooms.remove(&channel_id);
                                self.companions.remove(&channel_id);
                                self.companion_channels.remove(&channel_id);
                                self.unpersisted_companions.remove(&channel_id);
                                self.companion_seen.remove(&channel_id);
                                self.deletes.remove(&channel_id);
                                self.compensation.remove(&channel_id);
                                self.denied.remove(&channel_id);
                                self.renames.forget(channel_id);
                                self.moves.remove(&channel_id);
                                self.uncertain_moves.remove(&channel_id);
                                self.desired_names.remove(&channel_id);
                                self.custom_names.remove(&channel_id);
                                observe_voice_operation("delete", "success");
                                self.observe_voice_state();
                            }
                            Err(error) => {
                                let outcome = voice_outcome_from_store(&error);
                                self.record(LifecycleFailure::Persistence {
                                    channel_id: Some(channel_id),
                                    error,
                                });
                                if error == StoreError::CredentialRefused {
                                    self.halted = true;
                                    observe_voice_operation("delete", outcome);
                                    self.queue.mark_succeeded(&action);
                                } else {
                                    self.mark_failed_observed(
                                        action,
                                        "voice-room persistence unavailable".to_owned(),
                                        elapsed_ms(now_ms, started),
                                    );
                                }
                            }
                        }
                    }
                    Err(RoomHttpError::RateLimited { retry_after_ms, .. }) => {
                        self.queue.mark_rate_limited(
                            self.live.guild_id,
                            retry_after_ms,
                            elapsed_ms(now_ms, started),
                            action,
                        );
                    }
                    Err(RoomHttpError::AccessDenied) => {
                        let state = self.live.inner.read().expect("live voice lock");
                        self.denied.insert(
                            channel_id,
                            (
                                state.generation,
                                state.permissions(self.live.guild_id, channel_id),
                            ),
                        );
                        drop(state);
                        self.queue.suspend(self.live.guild_id, channel_id);
                        self.record(LifecycleFailure::Discord {
                            channel_id,
                            error: RoomHttpError::AccessDenied,
                        });
                        observe_voice_operation("delete", "discord");
                        self.mark_failed_observed(
                            action,
                            "Discord access denied".to_owned(),
                            elapsed_ms(now_ms, started),
                        );
                    }
                    Err(RoomHttpError::Cancelled) => {
                        self.queue.mark_succeeded(&action);
                        self.deletes.remove(&channel_id);
                        observe_voice_operation("delete", "cancelled");
                    }
                    Err(RoomHttpError::UnknownOutcome) => {
                        self.mark_failed_observed(
                            action,
                            "Discord delete outcome unknown".to_owned(),
                            elapsed_ms(now_ms, started),
                        );
                    }
                    Err(error) => {
                        self.deletes.remove(&channel_id);
                        if error != RoomHttpError::Cancelled {
                            observe_voice_operation("delete", voice_outcome_from_http(&error));
                        }
                        self.complete_error(action, channel_id, error);
                        self.observe_voice_state();
                    }
                }
            }
            RoomAction::CreateCompanion {
                room_channel_id,
                plan,
            } => {
                if !self.rooms.contains_key(&room_channel_id) {
                    self.queue.mark_succeeded(&action);
                    return true;
                }
                // Idempotent: a companion already tracked (or persisted by a
                // racing dispatch) is success without another POST. One whose
                // row write failed earlier retries only the row, never the
                // POST.
                if let Some(companion) = self.companions.get(&room_channel_id).cloned() {
                    if self.unpersisted_companions.contains(&room_channel_id) {
                        self.persist_companion(
                            &action,
                            room_channel_id,
                            companion,
                            &plan,
                            now_ms,
                            started,
                        )
                        .await;
                    } else {
                        self.queue.mark_succeeded(&action);
                    }
                    return true;
                }
                let live = self.live.clone();
                let bot_id = live
                    .inner
                    .read()
                    .expect("live voice lock")
                    .bot
                    .as_ref()
                    .map(|bot| bot.member_id)
                    .unwrap_or(0);
                let guard: WriteGuard = Arc::new(move || {
                    let state = live.inner.read().expect("live voice lock");
                    state.ready
                        && state.channels.contains_key(&room_channel_id)
                        && can_manage_room(state.permissions(live.guild_id, room_channel_id))
                });
                match self.http.create_companion(&plan, bot_id, guard).await {
                    Ok(channel) => {
                        let text_channel_id = channel.id.get();
                        self.live.upsert_channel(channel);
                        let companion = TextCompanion::from_plan(&plan, text_channel_id, now_iso());
                        self.persist_companion(
                            &action,
                            room_channel_id,
                            companion,
                            &plan,
                            now_ms,
                            started,
                        )
                        .await;
                    }
                    Err(RoomHttpError::RateLimited { retry_after_ms, .. }) => {
                        self.queue.mark_rate_limited(
                            self.live.guild_id,
                            retry_after_ms,
                            elapsed_ms(now_ms, started),
                            action,
                        );
                    }
                    Err(RoomHttpError::UnknownOutcome) => {
                        // The POST may have succeeded: never blindly retry a
                        // create. Adopt the channel when the live snapshot
                        // already shows it; otherwise back off and let the
                        // next dispatch recheck.
                        if let Some(adopted) = self.adopt_companion(&plan) {
                            let companion = TextCompanion::from_plan(&plan, adopted, now_iso());
                            self.persist_companion(
                                &action,
                                room_channel_id,
                                companion,
                                &plan,
                                now_ms,
                                started,
                            )
                            .await;
                        } else {
                            self.mark_failed_observed(
                                action,
                                "companion create outcome unknown".to_owned(),
                                elapsed_ms(now_ms, started),
                            );
                        }
                    }
                    Err(error) => {
                        self.complete_error(action, room_channel_id, error);
                    }
                }
            }
            RoomAction::GrantCompanionView {
                room_channel_id,
                text_channel_id,
                member_id,
            } => {
                self.dispatch_companion_view(
                    &action,
                    room_channel_id,
                    text_channel_id,
                    member_id,
                    true,
                    now_ms,
                    started,
                )
                .await;
            }
            RoomAction::RevokeCompanionView {
                room_channel_id,
                text_channel_id,
                member_id,
            } => {
                self.dispatch_companion_view(
                    &action,
                    room_channel_id,
                    text_channel_id,
                    member_id,
                    false,
                    now_ms,
                    started,
                )
                .await;
            }
            RoomAction::UpdateOwnership {
                channel_id,
                owner_id,
                original_creator_id,
            } => {
                // No Discord write: the worker row already carries this
                // handoff. A stale action (a newer succession moved the row,
                // or the room was forgotten) persists nothing.
                let Some(room) = self.rooms.get(&channel_id).cloned() else {
                    self.queue.mark_succeeded(&action);
                    return true;
                };
                if room.owner_id != owner_id || room.original_creator_id != original_creator_id {
                    self.queue.mark_succeeded(&action);
                    return true;
                }
                match self.store.update_ownership(&room).await {
                    Ok(true) => {
                        self.queue.mark_succeeded(&action);
                    }
                    Ok(false) => {
                        // Tracked but no database row (deleted out-of-band):
                        // never retry a write that cannot land. The handoff
                        // stays in the worker row and surfaces below.
                        self.record(LifecycleFailure::Persistence {
                            channel_id: Some(channel_id),
                            error: StoreError::Conflict,
                        });
                        self.queue.mark_succeeded(&action);
                    }
                    Err(StoreError::CredentialRefused) => {
                        self.record(LifecycleFailure::Persistence {
                            channel_id: Some(channel_id),
                            error: StoreError::CredentialRefused,
                        });
                        self.halted = true;
                        self.queue.mark_succeeded(&action);
                    }
                    Err(error) => {
                        self.record(LifecycleFailure::Persistence {
                            channel_id: Some(channel_id),
                            error,
                        });
                        self.mark_failed_observed(
                            action,
                            "voice-room persistence unavailable".to_owned(),
                            elapsed_ms(now_ms, started),
                        );
                    }
                }
            }
            RoomAction::KickMember {
                channel_id,
                member_id,
            } => {
                let permissions = self
                    .live
                    .inner
                    .read()
                    .expect("live voice lock")
                    .permissions(self.live.guild_id, channel_id);
                let result = if self.rooms.get(&channel_id).is_none_or(|room| {
                    member_id == room.owner_id || member_id == room.original_creator_id
                }) {
                    // Either the room is gone and its overwrites went with it, or
                    // ownership changed after the vote passed and the target is now
                    // the owner or original creator: write nothing.
                    Ok(())
                } else if !can_enforce_kick(permissions) {
                    Err(RoomHttpError::AccessDenied)
                } else {
                    // Deny first so the target cannot rejoin between the writes.
                    // The deny is room-scoped, so the target having left does not
                    // skip it; the disconnect is not, so it re-checks presence.
                    match self
                        .http
                        .deny_connect(channel_id, member_id, self.live.room_guard(channel_id))
                        .await
                    {
                        Ok(()) => match self
                            .http
                            .disconnect(
                                self.live.guild_id,
                                member_id,
                                self.live.member_in_room_guard(channel_id, member_id),
                            )
                            .await
                        {
                            // The target left or moved on since the deny landed:
                            // nothing to disconnect.
                            Err(RoomHttpError::Cancelled) => Ok(()),
                            other => other,
                        },
                        Err(error) => Err(error),
                    }
                };
                match result {
                    Ok(()) => {
                        self.queue.mark_succeeded(&action);
                    }
                    Err(RoomHttpError::RateLimited { retry_after_ms, .. }) => {
                        self.queue.mark_rate_limited(
                            self.live.guild_id,
                            retry_after_ms,
                            elapsed_ms(now_ms, started),
                            action,
                        );
                    }
                    Err(RoomHttpError::UnknownOutcome) => {
                        // Both writes are idempotent, so a retry is safe.
                        self.mark_failed_observed(
                            action,
                            "Discord kick outcome unknown".to_owned(),
                            elapsed_ms(now_ms, started),
                        );
                    }
                    Err(error) => self.complete_error(action, channel_id, error),
                }
            }
            RoomAction::SetCustomName {
                channel_id,
                custom_name,
            } => {
                self.dispatch_custom_name(action, channel_id, custom_name, now_ms, started)
                    .await;
            }
            RoomAction::RenameRoom { channel_id, name } => {
                let valid = {
                    let live = self.live.inner.read().expect("live voice lock");
                    self.rooms.contains_key(&channel_id)
                        && self.desired_names.get(&channel_id) == Some(&name)
                        && live
                            .channels
                            .get(&channel_id)
                            .is_some_and(|c| c.name.as_deref() != Some(&name))
                        && can_manage_room(live.permissions(self.live.guild_id, channel_id))
                };
                if !valid {
                    self.queue.mark_succeeded(&action);
                    return true;
                }
                match self.http.rename(channel_id, &name).await {
                    Ok(()) => {
                        self.queue.mark_succeeded(&action);
                        if let Some(channel) = self
                            .live
                            .inner
                            .write()
                            .expect("live voice lock")
                            .channels
                            .get_mut(&channel_id)
                        {
                            channel.name = Some(name);
                        }
                    }
                    Err(RoomHttpError::RateLimited { retry_after_ms, .. }) => {
                        self.queue.mark_rate_limited(
                            self.live.guild_id,
                            retry_after_ms.max(RENAME_MIN_INTERVAL_MS),
                            elapsed_ms(now_ms, started),
                            action,
                        );
                    }
                    Err(RoomHttpError::RenameDeferred | RoomHttpError::UnknownOutcome) => {
                        self.queue.mark_rate_limited(
                            self.live.guild_id,
                            RENAME_MIN_INTERVAL_MS,
                            elapsed_ms(now_ms, started),
                            action,
                        );
                    }
                    Err(error) => self.complete_error(action, channel_id, error),
                }
            }
        }
        true
    }

    /// Adopt a companion text channel the live snapshot already shows after
    /// an unknown create outcome: a text channel in the room's category
    /// carrying the planned name. The default name is the same constant for
    /// every room, so a candidate must also be untracked (not another room's
    /// companion) and newer than the room (snowflakes grow with time, and the
    /// companion is created after its room). The check reads only the
    /// worker's own live map, never the network. `None` means no such
    /// channel is visible yet.
    fn adopt_companion(&self, plan: &TextChannelPlan) -> Option<Snowflake> {
        let live = self.live.inner.read().expect("live voice lock");
        live.channels
            .values()
            .filter(|channel| {
                channel.kind == twilight_model::channel::ChannelType::GuildText
                    && channel.parent_id.map(twilight_model::id::Id::get) == Some(plan.category_id)
                    && channel.name.as_deref() == Some(plan.name.as_str())
                    && channel.id.get() > plan.room_id
                    && !self.companion_text_channel_tracked(channel.id.get())
            })
            .map(|channel| channel.id.get())
            .min()
    }

    /// True when some room already owns this text channel as its companion,
    /// in memory or in a loaded row.
    fn companion_text_channel_tracked(&self, text_channel_id: Snowflake) -> bool {
        self.companion_channels
            .values()
            .any(|tracked| *tracked == text_channel_id)
            || self
                .companions
                .values()
                .any(|companion| companion.text_channel_id == text_channel_id)
    }

    /// Persist the companion row after its Discord channel exists, and track
    /// it. A non-credential write failure keeps the in-memory record (so
    /// grants/revokes and the room delete still find the channel), flags it
    /// unpersisted, and backs off: the retry writes only the row, never
    /// another POST. A refused credential halts the worker.
    async fn persist_companion(
        &mut self,
        action: &QueuedAction,
        room_channel_id: Snowflake,
        companion: TextCompanion,
        plan: &TextChannelPlan,
        now_ms: u64,
        started: Instant,
    ) {
        let text_channel_id = companion.text_channel_id;
        match self.store.add_companion(&companion).await {
            Ok(_) => {
                self.queue.mark_succeeded(action);
                self.unpersisted_companions.remove(&room_channel_id);
                self.companions.insert(room_channel_id, companion);
                self.companion_channels
                    .insert(room_channel_id, text_channel_id);
                self.companion_seen.insert(
                    room_channel_id,
                    plan.overwrites
                        .iter()
                        .filter_map(|overwrite| match overwrite.target {
                            TextOverwriteTarget::Member(id) => Some(id),
                            _ => None,
                        })
                        .collect(),
                );
            }
            Err(error) => {
                self.record(LifecycleFailure::Persistence {
                    channel_id: Some(room_channel_id),
                    error,
                });
                if error == StoreError::CredentialRefused {
                    self.halted = true;
                    self.queue.mark_succeeded(action);
                } else {
                    self.companions.insert(room_channel_id, companion);
                    self.companion_channels
                        .insert(room_channel_id, text_channel_id);
                    self.unpersisted_companions.insert(room_channel_id);
                    self.mark_failed_observed(
                        action.clone(),
                        "companion persistence unavailable".to_owned(),
                        elapsed_ms(now_ms, started),
                    );
                }
            }
        }
    }

    /// Delete the companion tracked for a room: Discord delete first, then
    /// the row. True when no companion state remains (nothing tracked,
    /// channel already gone, or both deletes done). A failed Discord delete
    /// (other than NotFound) returns false so the caller retries with its own
    /// action; a failed row write also returns false, keeping the in-memory
    /// record so the retry still finds the channel.
    async fn delete_companion_for(&mut self, room_channel_id: Snowflake) -> bool {
        let Some(companion) = self.companions.get(&room_channel_id).cloned() else {
            return true;
        };
        let Some(text_channel_id) = self.companion_channel(&companion) else {
            return true;
        };
        let present = self
            .live
            .inner
            .read()
            .expect("live voice lock")
            .channels
            .contains_key(&text_channel_id);
        if present {
            // The room is already gone from the live map when its companion
            // is deleted, so the guard reads the text channel's own
            // permissions (View + Manage Channels) rather than the room's.
            let live = self.live.clone();
            let guild_id = self.live.guild_id;
            let guard: WriteGuard = Arc::new(move || {
                let state = live.inner.read().expect("live voice lock");
                state.ready
                    && state
                        .permissions(guild_id, text_channel_id)
                        .is_some_and(|permissions| {
                            permissions
                                .contains(Permissions::VIEW_CHANNEL | Permissions::MANAGE_CHANNELS)
                        })
            });
            if let Err(error) = self.http.delete(text_channel_id, guard).await {
                self.record(LifecycleFailure::Discord {
                    channel_id: text_channel_id,
                    error,
                });
                return false;
            }
            self.live.remove_channel(text_channel_id);
        }
        match self
            .store
            .remove_companion(self.live.guild_id, room_channel_id)
            .await
        {
            Ok(_) => {
                self.companions.remove(&room_channel_id);
                self.companion_channels.remove(&room_channel_id);
                self.unpersisted_companions.remove(&room_channel_id);
                self.companion_seen.remove(&room_channel_id);
                true
            }
            Err(error) => {
                self.record(LifecycleFailure::Persistence {
                    channel_id: Some(room_channel_id),
                    error,
                });
                if error == StoreError::CredentialRefused {
                    self.halted = true;
                }
                false
            }
        }
    }

    /// One V9 join/leave overwrite edit through the ordered lane. Skipped
    /// (success) when the room or its companion is gone, or when the grant
    /// target left again before dispatch. Revokes always run: the leave path
    /// deletes the overwrite even if the member rejoined elsewhere. A 429
    /// honours retry-after; other failures back off with the queue budget
    /// (dead-lettered after [`two_bot_core::voice_rooms::QUEUE_MAX_ATTEMPTS`],
    /// surfaced via `/setup`).
    #[allow(clippy::too_many_arguments)]
    async fn dispatch_companion_view(
        &mut self,
        action: &QueuedAction,
        room_channel_id: Snowflake,
        text_channel_id: Snowflake,
        member_id: Snowflake,
        grant: bool,
        now_ms: u64,
        started: Instant,
    ) {
        let known = self
            .companions
            .get(&room_channel_id)
            .and_then(|companion| self.companion_channel(companion))
            == Some(text_channel_id);
        if !known || !self.rooms.contains_key(&room_channel_id) {
            self.queue.mark_succeeded(action);
            return;
        }
        if grant {
            let present = self
                .live
                .inner
                .read()
                .expect("live voice lock")
                .members
                .get(&member_id)
                .is_some_and(|member| member.channel_id == Some(room_channel_id));
            if !present {
                self.queue.mark_succeeded(action);
                return;
            }
        }
        let live = self.live.clone();
        let guild_id = self.live.guild_id;
        let guard: WriteGuard = Arc::new(move || {
            let state = live.inner.read().expect("live voice lock");
            state.ready
                && state.channels.contains_key(&text_channel_id)
                && can_manage_room(state.permissions(guild_id, room_channel_id))
        });
        let result = if grant {
            self.http
                .grant_companion_view(text_channel_id, member_id, guard)
                .await
        } else {
            self.http
                .revoke_companion_view(text_channel_id, member_id, guard)
                .await
        };
        match result {
            Ok(()) => {
                self.queue.mark_succeeded(action);
            }
            Err(RoomHttpError::RateLimited { retry_after_ms, .. }) => {
                self.queue.mark_rate_limited(
                    self.live.guild_id,
                    retry_after_ms,
                    elapsed_ms(now_ms, started),
                    action.clone(),
                );
            }
            Err(error) => {
                self.record(LifecycleFailure::Discord {
                    channel_id: text_channel_id,
                    error,
                });
                self.mark_failed_observed(
                    action.clone(),
                    if grant {
                        "companion view grant unavailable".to_owned()
                    } else {
                        "companion view revoke unavailable".to_owned()
                    },
                    elapsed_ms(now_ms, started),
                );
            }
        }
    }

    fn finish_join_error(
        &mut self,
        action: QueuedAction,
        channel_id: Snowflake,
        error: RoomHttpError,
        write: RefusedWrite,
        guard: &GuardedWrite,
    ) {
        if error == RoomHttpError::Cancelled {
            if let Some(failure) = guard
                .permission_failure
                .lock()
                .expect("voice guard lock")
                .take()
            {
                self.queue.mark_succeeded(&action);
                observe_voice_operation(
                    match write {
                        RefusedWrite::Create => "create",
                        RefusedWrite::Move => "move",
                    },
                    voice_outcome_from_http(&RoomHttpError::AccessDenied),
                );
                self.record(failure);
                return;
            }
        }
        self.finish_error(action, channel_id, error, Some(write));
    }

    fn complete_error(
        &mut self,
        action: QueuedAction,
        channel_id: Snowflake,
        error: RoomHttpError,
    ) {
        self.finish_error(action, channel_id, error, None);
    }

    /// Settle a failed write. A join-time create or move (`write` set) that
    /// Discord or the permission cache refused for access names the missing
    /// permission; every other failure keeps the plain Discord line.
    fn finish_error(
        &mut self,
        action: QueuedAction,
        channel_id: Snowflake,
        error: RoomHttpError,
        write: Option<RefusedWrite>,
    ) {
        self.queue.mark_succeeded(&action);
        if error == RoomHttpError::Unauthorized {
            self.halted = true;
        }
        if error == RoomHttpError::Cancelled {
            return;
        }
        match write {
            Some(write) => self.record_refusal(write, channel_id, error),
            None => self.record(LifecycleFailure::Discord { channel_id, error }),
        }
    }

    /// Record a refused join-time write. An access refusal becomes
    /// [`LifecycleFailure::MissingPermission`], checking the write gate's
    /// actual requirements (including Connect), not unrelated health gaps.
    /// Planner and final-guard refusals use their already-captured findings.
    fn record_refusal(&mut self, write: RefusedWrite, channel_id: Snowflake, error: RoomHttpError) {
        if error == RoomHttpError::AccessDenied {
            let findings = write_permission_findings(
                &self.live.inner.read().expect("live voice lock"),
                self.live.guild_id,
                channel_id,
            );
            self.record(LifecycleFailure::MissingPermission {
                write,
                channel_id,
                findings,
            });
        } else {
            self.record(LifecycleFailure::Discord { channel_id, error });
        }
    }
}

fn elapsed_ms(now_ms: u64, started: Instant) -> u64 {
    now_ms.saturating_add(started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64)
}

// --- V1 runtime: per-guild actors + gateway sink ------------------------------

/// Gateway-loop sink for voice events. The shard runner calls [`VoiceEventSink::handle`]
/// after the pipeline's cache update, so every snapshot the runtime builds is
/// complete. Live evidence is published synchronously before actor commands;
/// network/database writes remain serialized on each guild actor's timer.
pub trait VoiceEventSink: Send + Sync {
    fn handle(&self, event: &Event, cache: &DefaultInMemoryCache);
    /// Invalidate occupancy immediately on connection loss, including while an
    /// actor is awaiting SQL, HTTP or token-global rate-limit backoff.
    fn disconnect(&self);
    /// A cold RESUME has replayed durably but cannot populate a fresh cache.
    /// The supervisor must IDENTIFY after committing RESUMED, not before replay.
    fn needs_bootstrap(&self, cache: &DefaultInMemoryCache) -> bool;
    /// Resolve the tracked room a member is currently in, if any. The shared
    /// router asks this for `/kick`: a tracked-room target means the voice
    /// sink owns the interaction (vote-kick) and the router must stay silent;
    /// anything else keeps the existing moderation path. Boxed (not `impl
    /// Future`) so the trait stays object-safe behind `Arc<dyn _>`.
    fn kick_claim_room(
        &self,
        guild: Snowflake,
        member: Snowflake,
    ) -> Pin<Box<dyn Future<Output = Option<Snowflake>> + Send + '_>> {
        let _ = (guild, member);
        Box::pin(async { None })
    }
}

/// The gateway shares only live evidence; the actor owns the mutable queue.
#[derive(Clone)]
struct GuildActor {
    live: LiveGuild,
    tx: mpsc::UnboundedSender<ActorCommand>,
}

enum ActorCommand {
    Reconcile,
    Join {
        ticket: JoinTicket,
        /// The joiner's display name; the worker renders and filters the room
        /// name from it.
        display: String,
        seed: u64,
        created_at: String,
    },
    /// A new or edited creator row (`/create`, `/textchannels`): the worker
    /// swaps it in for rooms created from here on.
    CreatorAdded(CreatorChannel),
    /// The guild's saved access controls changed (`/access`).
    AccessChanged(AccessControls),
    /// One-shot worker snapshot for `/setup` (room count, failures, halt).
    Status(oneshot::Sender<WorkerStatus>),
    /// V2 ownership command (`/reclaim`, `/transfer`): the worker resolves
    /// the caller's current room, decides via the pure core, applies the
    /// handoff and replies with the user-facing text.
    Ownership {
        actor_id: Snowflake,
        is_admin: bool,
        command: OwnershipCommand,
        reply: oneshot::Sender<String>,
    },
    /// V4: start a vote-kick; the reply carries the vote state or the refusal.
    KickStart {
        vote_id: Snowflake,
        room_id: Snowflake,
        initiator_id: Snowflake,
        target_id: Snowflake,
        reply: oneshot::Sender<KickReply>,
    },
    /// V4: cast one ballot, addressed by vote ID.
    KickBallot {
        vote_id: Snowflake,
        voter_id: Snowflake,
        ballot: VoteBallot,
        reply: oneshot::Sender<KickReply>,
    },
    /// V4: resolve the tracked room a member is currently in, if any. The
    /// shared router uses this to tell vote-kick targets (voice owns the
    /// interaction) from moderation targets (the router keeps it).
    KickRoomOf {
        member: Snowflake,
        reply: oneshot::Sender<Option<Snowflake>>,
    },
    /// V3 `/name`: authorize against the room's current owner, then show the
    /// panel, open the modal, or apply a custom name or a restore.
    Name {
        command: NameCommand,
        reply: oneshot::Sender<NameReply>,
    },
}

/// A V2 ownership command from an interaction. `Reclaim` takes the caller's
/// room back as its original creator (or claims one whose owner is gone);
/// `Transfer` hands the caller's room to an occupant and makes them the
/// remembered creator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipCommand {
    Reclaim,
    Transfer { target_id: Snowflake },
}

pub type KickReply = Result<VoteKickUpdate, KickRefusal>;

/// Per-guild actor registry. Actors spawn lazily on the first complete
/// snapshot and exit when their guild leaves (sender dropped) or their store
/// load fails (respawned on the next event via `UnboundedSender::is_closed`).
/// How long an `/import` preview stays confirmable. Past that the Confirm
/// button answers "expired" and writes nothing; the member uploads again.
pub const PENDING_IMPORT_TTL: Duration = Duration::from_secs(15 * 60);
/// Upper bound on remembered previews across all guilds; the oldest expired
/// entries are evicted first, then the oldest entries, so a flood of uploads
/// cannot grow memory without bound.
const MAX_PENDING_IMPORTS: usize = 128;

/// An `/import` preview awaiting Confirm: the validated, unknown-channel
/// skipped candidate plus its expiry. The bytes are never re-downloaded:
/// attachment URLs expire, so Confirm works from this copy and re-diffs it
/// against freshly read state.
#[derive(Debug, Clone)]
struct PendingImport {
    candidate: VoiceConfiguration,
    expires_at: Instant,
}

pub struct VoiceRuntime<S, H> {
    make: Arc<dyn Fn() -> (S, H) + Send + Sync>,
    tick: Duration,
    enabled: bool,
    seeds: AtomicU64,
    actors: Mutex<HashMap<Snowflake, GuildActor>>,
    /// Serializes `/access` read-modify-write cycles so two admins cannot
    /// overwrite each other's change (rare, admin-only, so one lock is enough).
    access_lock: tokio::sync::Mutex<()>,
    /// Previewed import candidates by (guild, uploading member, content
    /// hash). Confirm consumes the entry, so a double click cannot apply
    /// twice; Cancel and expiry remove it with no write.
    pending_imports: Mutex<HashMap<(Snowflake, Snowflake, String), PendingImport>>,
    /// Automod policy every room-name and `/create` name is filtered under.
    name_policy: Arc<AutomodPolicy>,
}

impl<S, H> VoiceRuntime<S, H>
where
    S: RoomPersistence + Send + 'static,
    H: RoomWrites + Send + 'static,
{
    /// Build the runtime. `make` produces a fresh store/http pair per guild
    /// actor; `tick` drives the ordered-queue drain (250 ms production);
    /// `enabled` gates on `TWO_VOICE=1` so construction without the gate is inert.
    pub fn new(
        make: impl Fn() -> (S, H) + Send + Sync + 'static,
        tick: Duration,
        enabled: bool,
    ) -> Self {
        Self {
            make: Arc::new(make),
            tick,
            enabled,
            seeds: AtomicU64::new(initial_seed()),
            actors: Mutex::new(HashMap::new()),
            access_lock: tokio::sync::Mutex::new(()),
            pending_imports: Mutex::new(HashMap::new()),
            name_policy: Arc::new(AutomodPolicy::default()),
        }
    }

    /// Filter room names under the guild's configured automod policy. Without
    /// this the default policy applies (links blocked, no word list).
    #[must_use]
    pub fn with_name_policy(mut self, policy: AutomodPolicy) -> Self {
        self.name_policy = Arc::new(policy);
        self
    }

    fn live_actor(&self, guild: Snowflake) -> Option<GuildActor> {
        self.actors
            .lock()
            .expect("voice runtime lock")
            .get(&guild)
            .filter(|actor| !actor.tx.is_closed())
            .cloned()
    }

    fn ensure_actor(&self, guild: Snowflake) -> Option<GuildActor> {
        if !self.enabled {
            return None;
        }
        let mut actors = self.actors.lock().expect("voice runtime lock");
        if let Some(actor) = actors.get(&guild) {
            if !actor.tx.is_closed() {
                return Some(actor.clone());
            }
            actor.live.disconnect();
            actors.remove(&guild);
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = GuildActor {
            live: LiveGuild::new(guild),
            tx,
        };
        let live = actor.live.clone();
        let make = Arc::clone(&self.make);
        let tick = self.tick;
        let name_policy = Arc::clone(&self.name_policy);
        tokio::spawn(async move {
            let (store, http) = make();
            let Ok(worker) = GuildRoomWorker::load(live, store, http).await else {
                return;
            };
            let mut worker = worker.with_name_policy(name_policy);
            run_actor(&mut worker, rx, tick).await;
        });
        actors.insert(guild, actor.clone());
        Some(actor)
    }

    /// Guilds with a live actor (reconnect refresh iterates these).
    fn known_guilds(&self) -> Vec<Snowflake> {
        self.actors
            .lock()
            .expect("voice runtime lock")
            .keys()
            .copied()
            .collect()
    }

    /// Publish a complete snapshot, spawning the guild actor on first use.
    pub fn publish_snapshot(&self, guild: Snowflake, snapshot: GuildSnapshot) -> bool {
        let Some(actor) = self.ensure_actor(guild) else {
            return false;
        };
        actor.live.publish(snapshot) && actor.tx.send(ActorCommand::Reconcile).is_ok()
    }

    /// Feed one voice transition to an existing actor. Returns false when the
    /// guild has no actor yet (frames before the first GuildCreate are
    /// dropped; the snapshot that follows replays complete state).
    pub fn voice_frame(
        &self,
        guild: Snowflake,
        member: Snowflake,
        channel: Option<Snowflake>,
        bot: Option<bool>,
        display: String,
    ) -> bool {
        let Some(actor) = self.live_actor(guild) else {
            return false;
        };
        let command = match actor.live.voice_update(member, channel, bot) {
            Some(ticket) => ActorCommand::Join {
                ticket,
                display,
                seed: self.seeds.fetch_add(1, Ordering::Relaxed),
                created_at: now_iso(),
            },
            None => ActorCommand::Reconcile,
        };
        actor.tx.send(command).is_ok()
    }

    /// Fresh store/http pair from the actor factory (used by the S4 command
    /// handlers for creator rows and channel writes outside the queue).
    fn make_pair(&self) -> (S, H) {
        (self.make)()
    }

    /// Snapshot the live worker for `/setup`. `None` when the guild has no
    /// actor yet (before the first GuildCreate) or its inbox already drained.
    pub async fn worker_status(&self, guild: Snowflake) -> Option<WorkerStatus> {
        let actor = self.live_actor(guild)?;
        let (reply, inbox) = oneshot::channel();
        actor.tx.send(ActorCommand::Status(reply)).ok()?;
        inbox.await.ok()
    }

    /// Run one V2 ownership command (`/reclaim`, `/transfer`) on the live
    /// worker and return the ephemeral reply text. The decision, worker-row
    /// update and urgent persistence enqueue happen together in the guild
    /// actor. `None` when the guild has no actor yet (before the first
    /// GuildCreate) or its inbox already drained.
    pub async fn run_ownership(
        &self,
        guild: Snowflake,
        actor_id: Snowflake,
        is_admin: bool,
        command: OwnershipCommand,
    ) -> Option<String> {
        let actor = self.live_actor(guild)?;
        let (reply, inbox) = oneshot::channel();
        actor
            .tx
            .send(ActorCommand::Ownership {
                actor_id,
                is_admin,
                command,
                reply,
            })
            .ok()?;
        inbox.await.ok()
    }

    /// Start a vote-kick through the guild actor. `vote_id` is the initiating
    /// interaction ID. `None` when the guild has no live actor.
    pub async fn kick_start(
        &self,
        guild: Snowflake,
        vote_id: Snowflake,
        room_id: Snowflake,
        initiator_id: Snowflake,
        target_id: Snowflake,
    ) -> Option<KickReply> {
        let actor = self.live_actor(guild)?;
        let (reply, inbox) = oneshot::channel();
        actor
            .tx
            .send(ActorCommand::KickStart {
                vote_id,
                room_id,
                initiator_id,
                target_id,
                reply,
            })
            .ok()?;
        inbox.await.ok()
    }

    /// Cast one ballot through the guild actor. `None` when the guild has no
    /// live actor.
    pub async fn kick_ballot(
        &self,
        guild: Snowflake,
        vote_id: Snowflake,
        voter_id: Snowflake,
        ballot: VoteBallot,
    ) -> Option<KickReply> {
        let actor = self.live_actor(guild)?;
        let (reply, inbox) = oneshot::channel();
        actor
            .tx
            .send(ActorCommand::KickBallot {
                vote_id,
                voter_id,
                ballot,
                reply,
            })
            .ok()?;
        inbox.await.ok()
    }

    /// Resolve the tracked room a member is currently in, if any. `None`
    /// when the guild has no live actor, while evidence is stale, or when
    /// the member is not in a tracked room.
    pub async fn kick_room_of(&self, guild: Snowflake, member: Snowflake) -> Option<Snowflake> {
        let actor = self.live_actor(guild)?;
        let (reply, inbox) = oneshot::channel();
        actor
            .tx
            .send(ActorCommand::KickRoomOf { member, reply })
            .ok()?;
        inbox.await.ok()?
    }

    /// Invalidate first: even an in-flight task cannot write after GuildDelete.
    pub fn remove_guild(&self, guild: Snowflake) {
        if let Some(actor) = self
            .actors
            .lock()
            .expect("voice runtime lock")
            .remove(&guild)
        {
            actor.live.disconnect();
        }
    }

    fn update_live(&self, guild: Snowflake, update: impl FnOnce(&LiveGuild)) {
        if let Some(actor) = self.live_actor(guild) {
            update(&actor.live);
            let _ = actor.tx.send(ActorCommand::Reconcile);
        }
    }

    /// Push freshly saved access controls to the live actor, so a creation
    /// switch takes effect without a restart.
    fn access_changed(&self, guild: Snowflake, controls: AccessControls) {
        if let Some(actor) = self.live_actor(guild) {
            let _ = actor.tx.send(ActorCommand::AccessChanged(controls));
        }
    }

    /// Pass an admin-supplied `/create` name through the same automod name
    /// filter as generated room names. Returns the sanitized name to create
    /// with, or the user-facing refusal; nothing is created for a refusal.
    fn filter_creator_name(
        &self,
        guild_id: Snowflake,
        channel_id: Snowflake,
        user_id: Snowflake,
        name: &str,
    ) -> Result<String, String> {
        let context = NameFilterContext {
            guild_id: guild_id.to_string(),
            channel_id: channel_id.to_string(),
            user_id: user_id.to_string(),
        };
        filter_channel_name(name, &self.name_policy, &context).map_err(|error| error.to_string())
    }

    fn creator_added(&self, creator: &CreatorChannel, channel: Channel) {
        if let Some(actor) = self.live_actor(creator.guild_id) {
            actor.live.upsert_channel(channel);
            let _ = actor.tx.send(ActorCommand::CreatorAdded(creator.clone()));
        }
    }

    /// Remember a previewed import candidate for one Confirm. Expired
    /// entries are dropped first; past the cap the oldest entry goes.
    fn remember_pending_import(
        &self,
        guild_id: Snowflake,
        member_id: Snowflake,
        hash: &str,
        candidate: VoiceConfiguration,
    ) {
        let mut pending = self.pending_imports.lock().expect("voice runtime lock");
        let now = Instant::now();
        pending.retain(|_, entry| entry.expires_at > now);
        if pending.len() >= MAX_PENDING_IMPORTS {
            let oldest = pending
                .iter()
                .min_by_key(|(_, entry)| entry.expires_at)
                .map(|(key, _)| key.clone());
            if let Some(key) = oldest {
                pending.remove(&key);
            }
        }
        pending.insert(
            (guild_id, member_id, hash.to_owned()),
            PendingImport {
                candidate,
                expires_at: now + PENDING_IMPORT_TTL,
            },
        );
    }

    /// Consume a previewed candidate for Confirm. `None` means no preview,
    /// a foreign member/hash, or expiry: the caller answers without writing.
    /// Expired entries are dropped as they are found.
    fn take_pending_import(
        &self,
        guild_id: Snowflake,
        member_id: Snowflake,
        hash: &str,
    ) -> Option<VoiceConfiguration> {
        let mut pending = self.pending_imports.lock().expect("voice runtime lock");
        let now = Instant::now();
        pending.retain(|_, entry| entry.expires_at > now);
        pending
            .remove(&(guild_id, member_id, hash.to_owned()))
            .map(|entry| entry.candidate)
    }

    /// Drop a preview without writing (Cancel). Missing entries are fine:
    /// Cancel is idempotent and also covers already-consumed previews.
    fn cancel_pending_import(&self, guild_id: Snowflake, member_id: Snowflake, hash: &str) {
        self.pending_imports
            .lock()
            .expect("voice runtime lock")
            .remove(&(guild_id, member_id, hash.to_owned()));
    }

    /// Hand an edited creator row to the guild worker (V9d `/textchannels`).
    /// Only rooms created afterwards read it: each companion keeps the
    /// settings snapshot taken when it was created.
    fn creator_updated(&self, creator: &CreatorChannel) {
        if let Some(actor) = self.live_actor(creator.guild_id) {
            let _ = actor.tx.send(ActorCommand::CreatorAdded(creator.clone()));
        }
    }

    pub fn disconnect(&self) {
        for actor in self.actors.lock().expect("voice runtime lock").values() {
            actor.live.disconnect();
        }
    }
}

impl<S, H> VoiceEventSink for VoiceRuntime<S, H>
where
    S: RoomPersistence + Send + 'static,
    H: RoomWrites + Send + 'static,
{
    fn handle(&self, event: &Event, cache: &DefaultInMemoryCache) {
        if !self.enabled {
            return;
        }
        match event {
            Event::GuildCreate(gc) => {
                if let twilight_model::gateway::payload::incoming::GuildCreate::Available(_) =
                    gc.as_ref()
                {
                    let guild_id = gc.id().get();
                    if let Some(snapshot) = snapshot_from_cache(cache, guild_id) {
                        self.publish_snapshot(guild_id, snapshot);
                    }
                }
            }
            Event::VoiceStateUpdate(update) => {
                let Some(guild_id) = update.guild_id.map(|id| id.get()) else {
                    return;
                };
                let member_id = update.user_id.get();
                let bot = update.member.as_ref().map(|member| member.user.bot);
                self.voice_frame(
                    guild_id,
                    member_id,
                    update.channel_id.map(|id| id.get()),
                    bot,
                    display_name(cache, guild_id, member_id),
                );
            }
            Event::ChannelCreate(created) => {
                if let Some(guild_id) = created.guild_id.map(|id| id.get()) {
                    self.update_live(guild_id, |live| live.upsert_channel(created.0.clone()));
                }
            }
            Event::ChannelUpdate(updated) => {
                if let Some(guild_id) = updated.guild_id.map(|id| id.get()) {
                    self.update_live(guild_id, |live| live.upsert_channel(updated.0.clone()));
                }
            }
            Event::ChannelDelete(deleted) => {
                if let Some(guild_id) = deleted.guild_id.map(|id| id.get()) {
                    self.update_live(guild_id, |live| live.remove_channel(deleted.id.get()));
                }
            }
            Event::RoleCreate(created) => {
                let guild_id = created.guild_id.get();
                if let Some(access) = bot_access_from_cache(cache, guild_id) {
                    self.update_live(guild_id, |live| live.refresh_bot(access));
                }
            }
            Event::RoleUpdate(updated) => {
                let guild_id = updated.guild_id.get();
                if let Some(access) = bot_access_from_cache(cache, guild_id) {
                    self.update_live(guild_id, |live| live.refresh_bot(access));
                }
            }
            Event::RoleDelete(deleted) => {
                let guild_id = deleted.guild_id.get();
                if let Some(access) = bot_access_from_cache(cache, guild_id) {
                    self.update_live(guild_id, |live| live.refresh_bot(access));
                }
            }
            Event::MemberUpdate(updated) => {
                if cache
                    .current_user()
                    .is_some_and(|bot| bot.id == updated.user.id)
                {
                    let guild_id = updated.guild_id.get();
                    if let Some(access) = bot_access_from_cache(cache, guild_id) {
                        self.update_live(guild_id, |live| live.refresh_bot(access));
                    }
                }
            }
            // A warm RESUME replays missed dispatches before RESUMED; only then
            // may cached occupancy become authoritative again. READY alone is
            // not sufficient (GuildCreate follows with state).
            Event::Resumed => {
                for guild_id in self.known_guilds() {
                    if let Some(snapshot) = snapshot_from_cache(cache, guild_id) {
                        self.publish_snapshot(guild_id, snapshot);
                    }
                }
            }
            Event::GuildDelete(deleted) => {
                self.remove_guild(deleted.id.get());
            }
            _ => {}
        }
    }

    fn disconnect(&self) {
        VoiceRuntime::disconnect(self);
    }

    fn needs_bootstrap(&self, cache: &DefaultInMemoryCache) -> bool {
        self.enabled && cache.current_user().is_none()
    }

    fn kick_claim_room(
        &self,
        guild: Snowflake,
        member: Snowflake,
    ) -> Pin<Box<dyn Future<Output = Option<Snowflake>> + Send + '_>> {
        Box::pin(self.kick_room_of(guild, member))
    }
}

async fn run_actor<S: RoomPersistence, H: RoomWrites>(
    worker: &mut GuildRoomWorker<S, H>,
    mut inbox: mpsc::UnboundedReceiver<ActorCommand>,
    tick: Duration,
) {
    let start = Instant::now();
    let mut timer = tokio::time::interval(tick);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            command = inbox.recv() => {
                let Some(command) = command else { break };
                let now_ms = start
                    .elapsed()
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64;
                apply_command(worker, command, now_ms);
            }
            _ = timer.tick() => {
                worker.reconcile();
                let now_ms = start
                    .elapsed()
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64;
                // Expire votes and react to roster changes before the queue
                // drains, so a passed vote's enforcement is dispatchable now.
                worker.kick_refresh(now_ms);
                // Return to the inbox after each await. Evidence is already
                // live, but creator configuration/status commands must not sit
                // behind a 64-write burst either.
                worker.dispatch_one(now_ms).await;
                worker.send_notices(now_ms).await;
            }
        }
    }
}

fn apply_command<S: RoomPersistence, H: RoomWrites>(
    worker: &mut GuildRoomWorker<S, H>,
    command: ActorCommand,
    now_ms: u64,
) {
    match command {
        ActorCommand::Reconcile => worker.reconcile(),
        ActorCommand::Join {
            ticket,
            display,
            seed,
            created_at,
        } => {
            worker.accept_join(ticket, &display, seed, created_at);
            worker.reconcile();
        }
        ActorCommand::CreatorAdded(creator) => {
            worker.creators.insert(creator.channel_id, creator);
            worker.reconcile();
        }
        ActorCommand::AccessChanged(controls) => worker.access = controls,
        ActorCommand::Status(reply) => {
            let mut creator_ids: Vec<Snowflake> = worker.creators.keys().copied().collect();
            creator_ids.sort_unstable();
            let _ = reply.send(WorkerStatus {
                health: worker
                    .live
                    .permission_findings(&creator_ids)
                    .iter()
                    .map(health_line)
                    .collect(),
                tracked_rooms: worker.tracked().len(),
                failures: worker.failures().iter().map(failure_line).collect(),
                halted: worker.halted(),
            });
        }
        ActorCommand::Ownership {
            actor_id,
            is_admin,
            command,
            reply,
        } => {
            let _ = reply.send(worker.apply_ownership(actor_id, is_admin, command));
        }
        ActorCommand::KickStart {
            vote_id,
            room_id,
            initiator_id,
            target_id,
            reply,
        } => {
            let _ =
                reply.send(worker.kick_start(vote_id, room_id, initiator_id, target_id, now_ms));
        }
        ActorCommand::KickBallot {
            vote_id,
            voter_id,
            ballot,
            reply,
        } => {
            let _ = reply.send(worker.kick_cast(vote_id, voter_id, ballot, now_ms));
        }
        ActorCommand::KickRoomOf { member, reply } => {
            let _ = reply.send(worker.kick_room_of(member));
        }
        ActorCommand::Name { command, reply } => {
            let _ = reply.send(worker.apply_name(command, now_ms));
        }
    }
}

/// Build the production sink: lifecycle actors plus the S4 responder.
pub fn build_production_runtime(
    token: &str,
    pool: sqlx::PgPool,
    name_policy: AutomodPolicy,
) -> Result<VoiceResponder<PgRoomStore, RoomHttp, RoomHttp>, RoomHttpError> {
    let replies = RoomHttp::new(token.to_owned())?;
    let http = replies.clone();
    let store = PgRoomStore::new(pool);
    Ok(VoiceResponder::new(
        Arc::new(
            VoiceRuntime::new(
                move || (store.clone(), http.clone()),
                Duration::from_millis(250),
                true,
            )
            .with_name_policy(name_policy),
        ),
        Arc::new(replies),
    ))
}

/// Complete guild snapshot from the post-update cache. Returns None until the
/// cache holds the guild, the bot user and the voice states — never publish a
/// partial listing as complete.
fn snapshot_from_cache(cache: &DefaultInMemoryCache, guild_id: Snowflake) -> Option<GuildSnapshot> {
    let guild_key = Id::new(guild_id);
    cache.guild(guild_key)?;
    let channels: Vec<Channel> = cache
        .guild_channels(guild_key)?
        .iter()
        .filter_map(|id| cache.channel(*id).map(|channel| channel.value().clone()))
        .collect();
    let members: Vec<VoiceMember> = cache
        .guild_voice_states(guild_key)?
        .iter()
        .filter_map(|user_id| {
            let state = cache.voice_state(*user_id, guild_key)?;
            Some(VoiceMember {
                member_id: user_id.get(),
                channel_id: state.channel_id().get(),
                bot: cache.user(*user_id).map(|user| user.bot),
            })
        })
        .collect();
    Some(GuildSnapshot {
        channels,
        members,
        bot: bot_access_from_cache(cache, guild_id)?,
    })
}

fn bot_access_from_cache(cache: &DefaultInMemoryCache, guild_id: Snowflake) -> Option<BotAccess> {
    let guild_key = Id::new(guild_id);
    let guild = cache.guild(guild_key)?;
    let bot_id = cache.current_user()?.id;
    let member_roles = cache
        .member(guild_key, bot_id)
        .map(|member| member.roles().to_vec())
        .unwrap_or_default();
    let roles = cache
        .guild_roles(guild_key)?
        .iter()
        .filter_map(|role_id| cache.role(*role_id).map(|role| role.resource().clone()))
        .collect();
    Some(BotAccess {
        member_id: bot_id.get(),
        guild_owner_id: guild.owner_id().get(),
        system_channel_id: guild.system_channel_id().map(|id| id.get()),
        member_roles,
        roles,
    })
}

/// Trusted guild inventory for V11 export/import, built from the live
/// gateway cache at dispatch time, never from uploaded data. `None` when the
/// guild itself is not cached yet (the caller refuses with "try again").
/// Channel, role and member sets are best-effort: a set the cache has not
/// populated yet reads as empty, and validation then refuses references to
/// it instead of silently passing them.
#[must_use]
pub fn inventory_from_cache(
    cache: &DefaultInMemoryCache,
    guild_id: Snowflake,
) -> Option<GuildInventory> {
    let guild_key = Id::new(guild_id);
    cache.guild(guild_key)?;
    let guild_string = guild_id.to_string();
    let mut channels = std::collections::BTreeMap::new();
    if let Some(ids) = cache.guild_channels(guild_key) {
        for id in ids.value().iter() {
            let Some(channel) = cache.channel(*id) else {
                continue;
            };
            let kind = match channel.kind {
                ChannelType::GuildText => ChannelKind::Text,
                ChannelType::GuildVoice => ChannelKind::Voice,
                ChannelType::GuildStageVoice => ChannelKind::Stage,
                ChannelType::GuildCategory => ChannelKind::Category,
                _ => continue,
            };
            if channel.guild_id.map(Id::get) != Some(guild_id) {
                continue;
            }
            channels.insert(
                id.get().to_string(),
                ChannelReference {
                    guild_id: guild_string.clone(),
                    kind,
                },
            );
        }
    }
    let mut roles = std::collections::BTreeMap::new();
    if let Some(ids) = cache.guild_roles(guild_key) {
        for id in ids.value().iter() {
            roles.insert(id.get().to_string(), guild_string.clone());
        }
    }
    let mut members = std::collections::BTreeMap::new();
    if let Some(ids) = cache.guild_members(guild_key) {
        for id in ids.value().iter() {
            members.insert(id.get().to_string(), guild_string.clone());
        }
    }
    Some(GuildInventory {
        guild_id: guild_string,
        channels,
        roles,
        members,
    })
}

fn display_name(cache: &DefaultInMemoryCache, guild_id: Snowflake, member_id: Snowflake) -> String {
    let guild_key = Id::new(guild_id);
    let user_key = Id::new(member_id);
    if let Some(member) = cache.member(guild_key, user_key) {
        if let Some(nick) = member.nick() {
            if !nick.is_empty() {
                return nick.to_owned();
            }
        }
    }
    if let Some(user) = cache.user(user_key) {
        if let Some(global) = user.global_name.clone() {
            if !global.is_empty() {
                return global;
            }
        }
        return user.name.clone();
    }
    "member".to_owned()
}

fn initial_seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
}

/// V1 room name until the V5 template engine owns naming: the joiner's
/// display name, truncated to the Discord 100-character ceiling.
fn room_name(display: &str) -> String {
    const SUFFIX: &str = "'s room";
    let base = format!("{display}{SUFFIX}");
    if base.chars().count() <= MAX_CHANNEL_NAME_LEN as usize {
        return base;
    }
    let keep = (MAX_CHANNEL_NAME_LEN as usize).saturating_sub(SUFFIX.chars().count());
    let head: String = display.chars().take(keep).collect();
    format!("{head}{SUFFIX}")
}

/// V1 room-name template until the V5 engine owns naming, in the legacy
/// placeholder syntax the create-path filter renders.
const ROOM_NAME_TEMPLATE: &str = "{username}'s room";
/// Characters of the template's fixed text, which a long display name must
/// leave room for inside the Discord 100-character ceiling.
const ROOM_NAME_SUFFIX_CHARS: usize = "'s room".len();

/// The V1 room name for a joiner, passed through the automod name filter: the
/// joiner's display name plus the template suffix, truncated to the Discord
/// ceiling by shortening the display name. A name containing a blocked term is
/// retried without the username; only a blocked bare template is refused.
fn resolve_room_name(
    display: &str,
    policy: &AutomodPolicy,
    context: &NameFilterContext,
) -> Result<ResolvedRoomName, BlockedRoomName> {
    let keep = (MAX_CHANNEL_NAME_LEN as usize).saturating_sub(ROOM_NAME_SUFFIX_CHARS);
    let username: String = display.chars().take(keep).collect();
    resolve_create_name(ROOM_NAME_TEMPLATE, &username, 0, 0, policy, context)
}

// --- `/create` + `/setup` decisions (pure, no Discord) -------------------------

/// `/create` input: the admin-supplied name for a new creator channel.
pub struct CreateChannelRequest {
    pub guild_id: Snowflake,
    pub name: String,
}

/// What `/create` means. Refusals are ephemeral-safe strings; the S4 handler
/// executes `Create` via the guarded adapter and stores the creator row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateChannelPlan {
    Create { guild_id: Snowflake, name: String },
    Refuse { message: String },
}

/// Validate a `/create` name: non-blank and within the Discord name ceiling.
#[must_use]
pub fn decide_create_channel(request: CreateChannelRequest) -> CreateChannelPlan {
    let name = request.name.trim().to_owned();
    if name.is_empty() {
        return CreateChannelPlan::Refuse {
            message: "Give the new creator channel a name, then try again.".to_owned(),
        };
    }
    if name.chars().count() > MAX_CHANNEL_NAME_LEN as usize {
        return CreateChannelPlan::Refuse {
            message: format!(
                "That name is too long: Discord channel names top out at {} characters.",
                MAX_CHANNEL_NAME_LEN
            ),
        };
    }
    CreateChannelPlan::Create {
        guild_id: request.guild_id,
        name,
    }
}

/// `/textchannels` input after parsing. Unset options keep the stored value;
/// `enabled` defaults to on, so naming a channel or role also switches the
/// companions on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChannelsRequest {
    pub enabled: Option<bool>,
    pub name: Option<String>,
    pub viewer_role: Option<Snowflake>,
}

/// What `/textchannels` means for one creator channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextChannelsPlan {
    Update(CreatorChannel),
    Refuse { message: String },
}

/// Fold a `/textchannels` request into the creator row, or refuse. Pure: the
/// handler reads the row, runs this, then stores the result. The stored
/// row only governs rooms created afterwards (snapshot rule).
#[must_use]
pub fn decide_text_channels(
    creator: Option<CreatorChannel>,
    request: &TextChannelsRequest,
) -> TextChannelsPlan {
    let refuse = |message: &str| TextChannelsPlan::Refuse {
        message: message.to_owned(),
    };
    let Some(mut creator) = creator else {
        return refuse("That channel is not a voice-room creator. Pick one made with /create.");
    };
    creator.text_channels = request.enabled.unwrap_or(true);
    if let Some(name) = &request.name {
        if !is_usable_channel_name(name) {
            return TextChannelsPlan::Refuse {
                message: format!(
                    "Give the companion channel a name of 1 to {} characters, then try again.",
                    MAX_TEXT_CHANNEL_NAME_CHARS
                ),
            };
        }
        creator.text_channel_name = Some(name.trim().to_owned());
    }
    if let Some(role) = request.viewer_role {
        creator.text_viewer_role_id = Some(role);
    }
    if creator.validate().is_err() {
        return refuse("Those companion settings are not valid. Check the name and role.");
    }
    TextChannelsPlan::Update(creator)
}

// --- V8 per-creator settings decisions (pure, no Discord) ------------------------
//
// Each command edits one creator row through `add_creator` (single-field
// write) with the same bounds the V11 import validates: limits 0-99,
// first numbers >= 1, permission sources creator/category/channel. The
// stored row only governs rooms created afterwards; open rooms keep theirs.

/// `/position` input after parsing. Unset options keep the stored value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionRequest {
    pub position: Option<RoomPosition>,
    pub first_number: Option<i64>,
}

/// What `/position` means for one creator channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PositionPlan {
    Update(CreatorChannel),
    Refuse { message: String },
}

/// Fold a `/position` request into the creator row, or refuse. Pure.
#[must_use]
pub fn decide_position(creator: Option<CreatorChannel>, request: &PositionRequest) -> PositionPlan {
    let refuse = |message: &str| PositionPlan::Refuse {
        message: message.to_owned(),
    };
    let Some(mut creator) = creator else {
        return refuse("That channel is not a voice-room creator. Pick one made with /create.");
    };
    if request.position.is_none() && request.first_number.is_none() {
        return refuse(
            "Choose a position (above or below) or a first room number, then try again.",
        );
    }
    if let Some(position) = request.position {
        creator.position = position;
    }
    if let Some(first_number) = request.first_number {
        creator.first_room_number = first_number;
    }
    if creator.validate().is_err() {
        return refuse("The first room number must be 1 or higher.");
    }
    PositionPlan::Update(creator)
}

/// `/group` input after parsing. An omitted toggle turns grouping on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupRequest {
    pub enabled: Option<bool>,
}

/// What `/group` means for one creator channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupPlan {
    Update(CreatorChannel),
    Refuse { message: String },
}

/// Fold a `/group` request into the creator row, or refuse. Pure.
#[must_use]
pub fn decide_group(creator: Option<CreatorChannel>, request: &GroupRequest) -> GroupPlan {
    let refuse = |message: &str| GroupPlan::Refuse {
        message: message.to_owned(),
    };
    let Some(mut creator) = creator else {
        return refuse("That channel is not a voice-room creator. Pick one made with /create.");
    };
    creator.group_by_category = request.enabled.unwrap_or(true);
    if creator.validate().is_err() {
        return refuse("Those grouping settings are not valid. Try again.");
    }
    GroupPlan::Update(creator)
}

/// `/inheritpermissions` input after parsing. `source` is the raw option
/// value (`creator`, `category` or `channel`); `source_channel` rides along
/// only for the channel source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InheritPermissionsRequest {
    pub source: Option<String>,
    pub source_channel: Option<Snowflake>,
}

/// What `/inheritpermissions` means for one creator channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InheritPermissionsPlan {
    Update(CreatorChannel),
    Refuse { message: String },
}

/// Fold an `/inheritpermissions` request into the creator row, or refuse.
/// Pure: same source bounds the V11 import validates.
#[must_use]
pub fn decide_inherit_permissions(
    creator: Option<CreatorChannel>,
    request: &InheritPermissionsRequest,
) -> InheritPermissionsPlan {
    let refuse = |message: &str| InheritPermissionsPlan::Refuse {
        message: message.to_owned(),
    };
    let Some(mut creator) = creator else {
        return refuse("That channel is not a voice-room creator. Pick one made with /create.");
    };
    let Some(source) = request.source.as_deref() else {
        return refuse("Choose where new rooms copy overrides from: creator, category or channel.");
    };
    match source {
        "creator" => {
            if request.source_channel.is_some() {
                return refuse(
                    "Only set a source channel together with source channel, not source creator.",
                );
            }
            creator.permission_source = PermissionSource::Creator;
            creator.permission_channel_id = None;
        }
        "category" => {
            if request.source_channel.is_some() {
                return refuse(
                    "Only set a source channel together with source channel, not source category.",
                );
            }
            creator.permission_source = PermissionSource::Category;
            creator.permission_channel_id = None;
        }
        "channel" => {
            let Some(channel) = request.source_channel.filter(|channel| *channel != 0) else {
                return refuse(
                    "Name the channel to copy overrides from together with source channel.",
                );
            };
            creator.permission_source = PermissionSource::Channel(channel);
            creator.permission_channel_id = Some(channel);
        }
        _ => {
            return refuse(
                "Choose where new rooms copy overrides from: creator, category or channel.",
            );
        }
    }
    if creator.validate().is_err() {
        return refuse("Those permission settings are not valid. Check the source channel.");
    }
    InheritPermissionsPlan::Update(creator)
}

/// `/defaultlimit` input after parsing. `None` clears to inherit the
/// creator channel's limit; `Some(0)` is unlimited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultLimitRequest {
    pub limit: Option<i64>,
}

/// What `/defaultlimit` means for one creator channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefaultLimitPlan {
    Update(CreatorChannel),
    Refuse { message: String },
}

/// Fold a `/defaultlimit` request into the creator row, or refuse. Pure:
/// same 0-99 bounds the V11 import validates.
#[must_use]
pub fn decide_default_limit(
    creator: Option<CreatorChannel>,
    request: &DefaultLimitRequest,
) -> DefaultLimitPlan {
    let refuse = |message: &str| DefaultLimitPlan::Refuse {
        message: message.to_owned(),
    };
    let Some(mut creator) = creator else {
        return refuse("That channel is not a voice-room creator. Pick one made with /create.");
    };
    creator.default_limit = request.limit;
    if creator.validate().is_err() {
        return refuse(
            "The starting limit must be 0-99 (0 is unlimited), or leave it empty to inherit.",
        );
    }
    DefaultLimitPlan::Update(creator)
}

/// `/alwaysprivate` input after parsing. An omitted toggle turns it on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlwaysPrivateRequest {
    pub enabled: Option<bool>,
}

/// What `/alwaysprivate` means for one creator channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlwaysPrivatePlan {
    Update(CreatorChannel),
    Refuse { message: String },
}

/// Fold an `/alwaysprivate` request into the creator row, or refuse. Pure.
#[must_use]
pub fn decide_always_private(
    creator: Option<CreatorChannel>,
    request: &AlwaysPrivateRequest,
) -> AlwaysPrivatePlan {
    let refuse = |message: &str| AlwaysPrivatePlan::Refuse {
        message: message.to_owned(),
    };
    let Some(mut creator) = creator else {
        return refuse("That channel is not a voice-room creator. Pick one made with /create.");
    };
    creator.private_default = request.enabled.unwrap_or(true);
    if creator.validate().is_err() {
        return refuse("Those privacy settings are not valid. Try again.");
    }
    AlwaysPrivatePlan::Update(creator)
}

fn text_channels_summary(creator: &CreatorChannel) -> String {
    let channel_id = creator.channel_id;
    if !creator.text_channels {
        return format!(
            "Companion text channels are off for <#{channel_id}>. Rooms already open keep theirs; new rooms get none."
        );
    }
    let name = creator
        .text_channel_name
        .as_deref()
        .unwrap_or(DEFAULT_TEXT_CHANNEL_NAME);
    let viewers = match creator.text_viewer_role_id {
        None => "occupants and admins".to_owned(),
        Some(role) if role == creator.guild_id => "everyone".to_owned(),
        Some(role) => format!("occupants, admins and <@&{role}>"),
    };
    format!(
        "Companion text channels are on for <#{channel_id}>: named `{name}`, visible to {viewers}. Applies to rooms created from now on; open rooms keep their settings."
    )
}

fn position_summary(creator: &CreatorChannel) -> String {
    let channel_id = creator.channel_id;
    let side = match creator.position {
        RoomPosition::Above => "above",
        RoomPosition::Below => "below",
    };
    format!(
        "New rooms from <#{channel_id}> go {side}, numbering from {}. Applies to rooms created from now on; open rooms stay where they are.",
        creator.first_room_number
    )
}

fn group_summary(creator: &CreatorChannel) -> String {
    let channel_id = creator.channel_id;
    if creator.group_by_category {
        format!(
            "Shared category numbering is on for <#{channel_id}>: new rooms join the category's room block. Applies to rooms created from now on."
        )
    } else {
        format!(
            "Shared category numbering is off for <#{channel_id}>. New rooms number per creator. Applies to rooms created from now on."
        )
    }
}

fn inherit_permissions_summary(creator: &CreatorChannel) -> String {
    let channel_id = creator.channel_id;
    let source = match creator.permission_source {
        PermissionSource::Creator => "the creator channel".to_owned(),
        PermissionSource::Category => "the category".to_owned(),
        PermissionSource::Channel(id) => format!("<#{id}>"),
    };
    format!(
        "New rooms from <#{channel_id}> copy overrides from {source}. Applies to rooms created from now on; open rooms keep theirs."
    )
}

fn default_limit_summary(creator: &CreatorChannel) -> String {
    let channel_id = creator.channel_id;
    let limit = match creator.default_limit {
        None => "inherit the creator channel's limit".to_owned(),
        Some(0) => "start unlimited".to_owned(),
        Some(limit) => format!("start with a limit of {limit}"),
    };
    format!(
        "New rooms from <#{channel_id}> {limit}. Applies to rooms created from now on; open rooms keep theirs."
    )
}

fn always_private_summary(creator: &CreatorChannel) -> String {
    let channel_id = creator.channel_id;
    if creator.private_default {
        format!(
            "New rooms from <#{channel_id}> start private. Applies to rooms created from now on; open rooms keep theirs."
        )
    } else {
        format!(
            "New rooms from <#{channel_id}> start public. Applies to rooms created from now on; open rooms keep theirs."
        )
    }
}

/// `/setup` input: creator rows plus live worker state for the guild.
/// `store_error` is `Some` when the creator read failed — the panel surfaces
/// it instead of rendering an empty list as "no creators".
pub struct SetupSummary {
    pub guild_id: Snowflake,
    pub creators: Vec<CreatorChannel>,
    pub tracked_rooms: usize,
    pub failures: Vec<String>,
    pub halted: bool,
    pub store_error: Option<String>,
}

/// `/setup` panel text. The command stays open to every member, but only an
/// admin gets [`setup_panel`], which names creator channels, store errors and
/// the worker's failure lines. Everyone else gets [`setup_member_panel`]: a
/// generic status with no ids, error classes or permission gaps. The S4
/// handler also gates the quick action and settings buttons on admin (spec V1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupPanel {
    pub title: String,
    pub description: String,
}

/// The member (non-admin) `/setup` view: running or paused, and whether
/// anything needs attention. Deliberately carries no channel ids, store or
/// Discord error text, or permission gaps; those stay admin-only.
#[must_use]
pub fn setup_member_panel(halted: bool, needs_attention: bool) -> SetupPanel {
    let status = if halted {
        "Voice rooms are paused right now."
    } else {
        "Voice rooms are running."
    };
    let mut lines = vec![status.to_owned()];
    if halted || needs_attention {
        lines.push("Ask a server admin to check /setup for details.".to_owned());
    }
    SetupPanel {
        title: "Voice rooms".to_owned(),
        description: lines.join("\n"),
    }
}

#[must_use]
pub fn setup_panel(summary: &SetupSummary) -> SetupPanel {
    let mut lines = Vec::new();
    if summary.halted {
        lines.push(
            "Voice rooms are paused: Discord refused the bot credential. \
             Fix the token, then restart the bot."
                .to_owned(),
        );
    } else {
        lines.push("Voice rooms are running.".to_owned());
    }
    if let Some(error) = &summary.store_error {
        lines.push(format!(
            "Could not load creator channels ({error}); the worker state below is current."
        ));
    }
    if summary.creators.is_empty() {
        if summary.store_error.is_none() {
            lines.push("No creator channels yet. Use /create to add the first one.".to_owned());
        }
    } else {
        lines.push(format!("Creator channels ({})", summary.creators.len()));
        for creator in &summary.creators {
            let position = match creator.position {
                RoomPosition::Above => "above",
                RoomPosition::Below => "below",
            };
            lines.push(format!("- <#{}>: new rooms {position}", creator.channel_id));
        }
    }
    lines.push(format!("Tracked rooms: {}.", summary.tracked_rooms));
    if summary.failures.is_empty() {
        lines.push("No recent failures.".to_owned());
    } else {
        lines.push(format!("Recent failures ({})", summary.failures.len()));
        for failure in summary.failures.iter().take(5) {
            lines.push(format!("- {failure}"));
        }
    }
    SetupPanel {
        title: "Voice rooms".to_owned(),
        description: lines.join("\n"),
    }
}

/// Voice definitions for the guild command merge, gated on `TWO_VOICE=1`.
/// Registration merges these first-wins via [`merge_commands`](two_bot_core::merge_commands);
/// the S4 slice owns the REST call.
#[must_use]
pub fn voice_command_set(gates: &VoiceGates) -> Vec<CommandDefinition> {
    if gates.enabled {
        voice_commands()
    } else {
        Vec::new()
    }
}

// --- V11 `/export` + `/import` decisions (pure, no Discord) ---------------------
//
// `/export` snapshots the store and encodes it; `/import` previews an upload
// against a trusted inventory and only writes on Confirm. Parsing, skipping,
// revalidation, diffing and hashing stay testable here; the S4 handlers below
// supply the store snapshot, the attachment bytes and the pending preview.

/// Preview lines in one `/import` message body. The diff renderer also caps
/// the whole body at its Discord limit; this bounds the line count.
pub const MAX_IMPORT_PREVIEW_LINES: usize = 20;

/// The spec's Manage Server gate for `/export`, `/import` and their
/// Confirm/Cancel buttons: Manage Guild, with Administrator implying it.
/// Fail closed on missing permissions.
fn may_manage_server(permissions: Option<Permissions>) -> bool {
    permissions.is_some_and(|permissions| {
        permissions.intersects(Permissions::ADMINISTRATOR | Permissions::MANAGE_GUILD)
    })
}

/// Versioned export filename: the codec version plus the guild, never room
/// or owner state (the document itself carries none either).
fn export_filename(guild_id: Snowflake) -> String {
    format!("voice-config-guild-{guild_id}-v{VOICE_CONFIG_VERSION}.json")
}

/// One `/import` planning outcome. Refusals and notices reply with text
/// alone and store nothing; only `Preview` writes a pending entry and only
/// `Apply` touches the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportDecision {
    /// Refused before any write: malformed upload, failed revalidation, or
    /// an unreadable guild. Reply with `message` alone.
    Refuse { message: String },
    /// Nothing to confirm (empty diff, cancel, expiry): reply with `text`
    /// alone and store nothing.
    Notice { text: String },
    /// Show `text` with Confirm/Cancel bound to (`member_id`, `hash`) and
    /// remember `candidate` for Confirm.
    Preview {
        candidate: VoiceConfiguration,
        hash: String,
        text: String,
    },
    /// Confirm verified against fresh state: apply `candidate`, then reply
    /// with `message`.
    Apply {
        candidate: VoiceConfiguration,
        message: String,
    },
}

/// Plan an `/import` preview from uploaded bytes: strict-decode (never from
/// a trusted source), report and skip unknown channels, revalidate the
/// remainder, then diff against `current`. Unknown-channel, cross-guild and
/// wrong-kind entries refuse here with a safe field-level message; uploaded
/// text is never echoed.
#[must_use]
pub fn plan_import_preview(
    current: &VoiceConfiguration,
    bytes: &[u8],
    inventory: &GuildInventory,
) -> ImportDecision {
    debug_assert_eq!(DIFF_HASH_CHARS, IMPORT_HASH_CHARS);
    if bytes.len() > MAX_IMPORT_BYTES {
        return ImportDecision::Refuse {
            message: format!(
                "That file is too large ({} bytes; the limit is {} bytes). Nothing was changed.",
                bytes.len(),
                MAX_IMPORT_BYTES,
            ),
        };
    }
    // The strict codec decode, never plain `serde_json`: the derived top-level
    // decoder also takes the positional-array form the codec forbids.
    let incoming = match decode_configuration(bytes) {
        Ok(config) => config,
        Err(VoiceConfigError::Malformed { line, column }) => {
            return ImportDecision::Refuse {
                message: format!(
                    "Could not import that file: malformed configuration JSON at line {line}, column {column}. Nothing was changed.",
                ),
            };
        }
        Err(error) => {
            return ImportDecision::Refuse {
                message: format!("Could not import that file: {error}. Nothing was changed."),
            };
        }
    };
    let (remaining, _) = skip_unknown_channels(&incoming, inventory);
    if let Err(error) = validate_configuration(&remaining, inventory) {
        return ImportDecision::Refuse {
            message: format!("Could not import that file: {error} Nothing was changed."),
        };
    }
    // Diff the full upload, not the pruned remainder: `diff_configuration`
    // re-skips unknown channels itself, so diffing `remaining` would always
    // report an empty skipped list and an unknown-channels-only file would
    // read as `No changes`. The pruned `remaining` stays the candidate.
    let diff = diff_configuration(current, &incoming, inventory);
    let text = render_preview(&diff, MAX_IMPORT_PREVIEW_LINES);
    if diff.change_count() == 0 {
        return ImportDecision::Notice { text };
    }
    ImportDecision::Preview {
        hash: diff_content_hash(current, &remaining),
        candidate: remaining,
        text,
    }
}

/// Plan a Confirm click: re-skip the remembered candidate against a fresh
/// inventory, revalidate, re-diff against freshly read `current` and compare
/// the content hash with the button's. A match applies; any concurrent
/// change (or a guild edit that invalidates the candidate) re-previews or
/// refuses instead of applying stale state.
#[must_use]
pub fn plan_import_confirm(
    current: &VoiceConfiguration,
    candidate: &VoiceConfiguration,
    inventory: &GuildInventory,
    hash: &str,
) -> ImportDecision {
    debug_assert_eq!(DIFF_HASH_CHARS, IMPORT_HASH_CHARS);
    let (remaining, _) = skip_unknown_channels(candidate, inventory);
    if let Err(error) = validate_configuration(&remaining, inventory) {
        return ImportDecision::Refuse {
            message: format!(
                "That preview no longer applies cleanly: {error} Nothing was changed. Upload the file again for a fresh preview.",
            ),
        };
    }
    if diff_content_hash(current, &remaining) == hash {
        let changes = diff_configuration(current, &remaining, inventory).change_count();
        return ImportDecision::Apply {
            candidate: remaining,
            message: format!(
                "Import applied: {} {}.",
                changes,
                if changes == 1 { "change" } else { "changes" },
            ),
        };
    }
    let diff = diff_configuration(current, &remaining, inventory);
    let text = render_preview(&diff, MAX_IMPORT_PREVIEW_LINES);
    if diff.change_count() == 0 {
        return ImportDecision::Notice { text };
    }
    ImportDecision::Preview {
        hash: diff_content_hash(current, &remaining),
        candidate: remaining,
        text,
    }
}

/// How many creator channels `candidate` adds over `current`.
fn added_creator_count(current: &VoiceConfiguration, candidate: &VoiceConfiguration) -> usize {
    candidate
        .creators
        .iter()
        .filter(|creator| {
            !current
                .creators
                .iter()
                .any(|existing| existing.channel_id == creator.channel_id)
        })
        .count()
}

/// Manage Server alone may not turn an existing voice channel into a creator:
/// `/create` needs Manage Channels, and an import is the same act without the
/// channel being created. Returns the refusal text when `candidate` adds a
/// creator row over `current` and the member lacks Manage Channels (admins
/// pass). Removing or editing creators is not gated here.
fn import_creator_gate(
    current: &VoiceConfiguration,
    candidate: &VoiceConfiguration,
    permissions: Option<Permissions>,
) -> Option<String> {
    if added_creator_count(current, candidate) == 0 || is_voice_admin(permissions) {
        return None;
    }
    Some(
        "That file adds creator channels, which needs Manage Channels like /create. Ask a member with that permission to import it, or remove the new creator entries. Nothing was changed."
            .to_owned(),
    )
}

/// Confirm/Cancel buttons for a preview, bound to the uploading member and
/// the diff's content hash via the voice custom-id codec.
fn import_preview_components(member_id: Snowflake, hash: &str) -> Vec<Component> {
    let button = |label: &str, style: ButtonStyle, custom_id: String| {
        Component::Button(Button {
            id: None,
            custom_id: Some(custom_id),
            disabled: false,
            emoji: None,
            label: Some(label.to_owned()),
            style,
            url: None,
            sku_id: None,
        })
    };
    vec![Component::ActionRow(ActionRow {
        id: None,
        components: vec![
            button(
                "Confirm",
                ButtonStyle::Success,
                import_confirm_custom_id(member_id, hash),
            ),
            button(
                "Cancel",
                ButtonStyle::Secondary,
                import_cancel_custom_id(member_id, hash),
            ),
        ],
    })]
}

/// Ephemeral `/export` reply carrying the JSON as a versioned file.
fn export_file_response(content: &str, filename: String, bytes: Vec<u8>) -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(InteractionResponseData {
            content: Some(content.to_owned()),
            attachments: Some(vec![Attachment::from_bytes(filename, bytes, 0)]),
            flags: Some(MessageFlags::EPHEMERAL),
            ..Default::default()
        }),
    }
}

/// Ephemeral `/import` preview reply with Confirm/Cancel buttons. Mentions
/// are disabled: the preview quotes uploaded template text, which must never
/// ping.
fn import_preview_response(text: &str, member_id: Snowflake, hash: &str) -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(InteractionResponseData {
            content: Some(text.to_owned()),
            components: Some(import_preview_components(member_id, hash)),
            flags: Some(MessageFlags::EPHEMERAL),
            allowed_mentions: Some(AllowedMentions {
                parse: Vec::new(),
                users: Vec::new(),
                roles: Vec::new(),
                replied_user: false,
            }),
            ..Default::default()
        }),
    }
}

/// The invoking member's user id, binding previews to their uploader.
/// Guild slash and component interactions carry it on the member; fall back
/// to the top-level user for other contexts. `None` refuses the command.
fn invoker_member_id(interaction: &Interaction) -> Option<Snowflake> {
    interaction
        .member
        .as_ref()
        .and_then(|member| member.user.as_ref())
        .map(|user| user.id.get())
        .or_else(|| interaction.user.as_ref().map(|user| user.id.get()))
}

/// A V11 `/import` Confirm/Cancel component, or `None` for anything this
/// slice does not own (other commands' components stay untouched).
fn voice_import_action(interaction: &Interaction) -> Option<VoiceAction> {
    if interaction.kind != InteractionType::MessageComponent {
        return None;
    }
    let InteractionData::MessageComponent(data) = interaction.data.as_ref()? else {
        return None;
    };
    match parse_voice_custom_id(&data.custom_id)? {
        action @ (VoiceAction::ImportConfirm { .. } | VoiceAction::ImportCancel { .. }) => {
            Some(action)
        }
        _ => None,
    }
}

/// Guild-level role gate shared by voice slash commands and the `/import`
/// Confirm/Cancel buttons. `None` means proceed; `Some(response)` is the
/// refusal to send (required role, per-command restriction, or unreadable
/// settings for non-admins).
async fn command_gate<S: RoomPersistence + Send + 'static>(
    store: &S,
    guild_id: Snowflake,
    member: &AccessMember,
    command_name: &str,
) -> Option<InteractionResponse> {
    match store.access_controls(guild_id).await {
        Ok(controls) => match may_use_command(&controls, member, command_name) {
            AccessDecision::Allow => None,
            AccessDecision::Deny(reason) => Some(ephemeral_response(access_denied_text(reason))),
        },
        Err(_) if !member.is_admin => Some(ephemeral_response(
            "Voice-room settings are unavailable right now. Try again shortly.",
        )),
        Err(_) => None,
    }
}

// --- `/create` + `/setup` interaction handlers (S4) ---------------------------
//
// Pure parse and auth stay testable without Discord; execution runs one
// guarded REST/SQL round-trip per command and answers with a single
// ephemeral response. Reply transport errors are the caller's to log: the
// handler attempts the response exactly once.

/// A voice slash command carried by an interaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceCommand {
    Create {
        name: String,
    },
    Setup,
    Ping,
    Invite,
    TextChannels {
        channel_id: Snowflake,
        request: TextChannelsRequest,
    },
    /// V8 per-creator placement: side plus first room number.
    Position {
        channel_id: Snowflake,
        request: PositionRequest,
    },
    /// V8 per-creator shared category numbering toggle.
    Group {
        channel_id: Snowflake,
        request: GroupRequest,
    },
    /// V8 per-creator permission-override source.
    InheritPermissions {
        channel_id: Snowflake,
        request: InheritPermissionsRequest,
    },
    /// V8 per-creator starting user limit.
    DefaultLimit {
        channel_id: Snowflake,
        request: DefaultLimitRequest,
    },
    /// V8 per-creator private default.
    AlwaysPrivate {
        channel_id: Snowflake,
        request: AlwaysPrivateRequest,
    },
    /// Original creator (or a member whose room owner is gone) claims the room.
    Reclaim,
    /// Owner (or admin) hands the room to an occupant, who becomes the
    /// remembered creator.
    Transfer {
        target_id: Snowflake,
    },
    Access(AccessAction),
    Logging(LoggingAction),
    Export,
    /// `/import file`: the resolved attachment id; `None` when the option
    /// is missing or of the wrong type (answered, never ignored).
    Import {
        file_id: Option<Id<AttachmentMarker>>,
    },
    /// V4 vote-kick: start a vote against a room occupant.
    Kick {
        target: Snowflake,
        reason: Option<String>,
    },
    /// V4 vote-kick: one ballot button press, addressed by vote ID.
    Ballot {
        vote_id: Snowflake,
        ballot: VoteBallot,
    },
    /// V3 `/name`: the owner's panel to set a custom name or restore the
    /// template name.
    Name,
}

/// One `/logging` sub-command. `Invalid` is a malformed or unknown shape; it
/// is answered, never silently ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoggingAction {
    Show,
    Level(String),
    Channel(Option<Snowflake>),
    Mention(Option<Snowflake>),
    Invalid,
}

/// One `/access` sub-command. `Invalid` is a malformed or unknown shape; it
/// is answered, never silently ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessAction {
    Show,
    Creation(bool),
    RequiredRole(Option<Snowflake>),
    Restrict {
        command: String,
        roles: Vec<Snowflake>,
    },
    Unrestrict(String),
    Invalid,
}

/// Guild the interaction was invoked in. `PartialMember` carries no guild, so
/// the top-level `guild_id` is authoritative.
#[must_use]
pub fn interaction_guild(interaction: &Interaction) -> Option<Snowflake> {
    interaction.guild_id.map(|id| id.get())
}

/// Parse a voice command, or `None` for anything this slice does not own
/// (non-command interactions, other commands, guild-less invocations).
#[must_use]
pub fn parse_voice_command(interaction: &Interaction) -> Option<VoiceCommand> {
    // Ballot buttons bypass the slash parser: the custom ID carries the vote.
    // Unknown custom IDs stay silent here so the shared router keeps them.
    if interaction.kind == InteractionType::MessageComponent {
        let InteractionData::MessageComponent(component) = interaction.data.as_ref()? else {
            return None;
        };
        interaction_guild(interaction)?;
        return parse_vote_button(&component.custom_id);
    }
    if interaction.kind != InteractionType::ApplicationCommand {
        return None;
    }
    let InteractionData::ApplicationCommand(command) = interaction.data.as_ref()? else {
        return None;
    };
    interaction_guild(interaction)?;
    match command.name.as_str() {
        "create" => {
            let name = command
                .options
                .iter()
                .find(|option| option.name == "name")
                .and_then(|option| match &option.value {
                    CommandOptionValue::String(value) => Some(value.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            Some(VoiceCommand::Create { name })
        }
        "setup" => Some(VoiceCommand::Setup),
        "ping" => Some(VoiceCommand::Ping),
        "invite" => Some(VoiceCommand::Invite),
        "textchannels" => {
            let mut channel_id = 0;
            let mut request = TextChannelsRequest {
                enabled: None,
                name: None,
                viewer_role: None,
            };
            for option in &command.options {
                match (option.name.as_str(), &option.value) {
                    ("channel", CommandOptionValue::Channel(id)) => channel_id = id.get(),
                    ("enabled", CommandOptionValue::Boolean(value)) => {
                        request.enabled = Some(*value);
                    }
                    ("name", CommandOptionValue::String(value)) => {
                        request.name = Some(value.clone());
                    }
                    ("viewer-role", CommandOptionValue::Role(id)) => {
                        request.viewer_role = Some(id.get());
                    }
                    _ => {}
                }
            }
            Some(VoiceCommand::TextChannels {
                channel_id,
                request,
            })
        }
        "position" => {
            let mut channel_id = 0;
            let mut request = PositionRequest {
                position: None,
                first_number: None,
            };
            for option in &command.options {
                match (option.name.as_str(), &option.value) {
                    ("channel", CommandOptionValue::Channel(id)) => channel_id = id.get(),
                    ("position", CommandOptionValue::String(value)) => {
                        request.position = match value.as_str() {
                            "above" => Some(RoomPosition::Above),
                            "below" => Some(RoomPosition::Below),
                            _ => None,
                        };
                    }
                    ("first-number", CommandOptionValue::Integer(value)) => {
                        request.first_number = Some(*value);
                    }
                    _ => {}
                }
            }
            Some(VoiceCommand::Position {
                channel_id,
                request,
            })
        }
        "group" => {
            let mut channel_id = 0;
            let mut request = GroupRequest { enabled: None };
            for option in &command.options {
                match (option.name.as_str(), &option.value) {
                    ("channel", CommandOptionValue::Channel(id)) => channel_id = id.get(),
                    ("enabled", CommandOptionValue::Boolean(value)) => {
                        request.enabled = Some(*value);
                    }
                    _ => {}
                }
            }
            Some(VoiceCommand::Group {
                channel_id,
                request,
            })
        }
        "inheritpermissions" => {
            let mut channel_id = 0;
            let mut request = InheritPermissionsRequest {
                source: None,
                source_channel: None,
            };
            for option in &command.options {
                match (option.name.as_str(), &option.value) {
                    ("channel", CommandOptionValue::Channel(id)) => channel_id = id.get(),
                    ("source", CommandOptionValue::String(value)) => {
                        request.source = Some(value.clone());
                    }
                    ("source-channel", CommandOptionValue::Channel(id)) => {
                        request.source_channel = Some(id.get());
                    }
                    _ => {}
                }
            }
            Some(VoiceCommand::InheritPermissions {
                channel_id,
                request,
            })
        }
        "defaultlimit" => {
            let mut channel_id = 0;
            let mut request = DefaultLimitRequest { limit: None };
            for option in &command.options {
                match (option.name.as_str(), &option.value) {
                    ("channel", CommandOptionValue::Channel(id)) => channel_id = id.get(),
                    ("limit", CommandOptionValue::Integer(value)) => {
                        request.limit = Some(*value);
                    }
                    _ => {}
                }
            }
            Some(VoiceCommand::DefaultLimit {
                channel_id,
                request,
            })
        }
        "alwaysprivate" => {
            let mut channel_id = 0;
            let mut request = AlwaysPrivateRequest { enabled: None };
            for option in &command.options {
                match (option.name.as_str(), &option.value) {
                    ("channel", CommandOptionValue::Channel(id)) => channel_id = id.get(),
                    ("enabled", CommandOptionValue::Boolean(value)) => {
                        request.enabled = Some(*value);
                    }
                    _ => {}
                }
            }
            Some(VoiceCommand::AlwaysPrivate {
                channel_id,
                request,
            })
        }
        "reclaim" => Some(VoiceCommand::Reclaim),
        "transfer" => {
            // The `member` option is required at registration, so Discord
            // always sends it; zero means a malformed payload and refuses
            // with "choose a member" in the handler, never silence.
            let target_id = command
                .options
                .iter()
                .find(|option| option.name == "member")
                .and_then(|option| match &option.value {
                    CommandOptionValue::User(id) => Some(id.get()),
                    _ => None,
                })
                .unwrap_or(0);
            Some(VoiceCommand::Transfer { target_id })
        }
        "access" => Some(VoiceCommand::Access(parse_access_action(&command.options))),
        "logging" => Some(VoiceCommand::Logging(parse_logging_action(
            &command.options,
        ))),
        "export" => Some(VoiceCommand::Export),
        "import" => Some(VoiceCommand::Import {
            file_id: command.options.iter().find_map(|option| {
                if option.name == "file" {
                    match &option.value {
                        CommandOptionValue::Attachment(id) => Some(*id),
                        _ => None,
                    }
                } else {
                    None
                }
            }),
        }),
        // The published shape may be ours (`member`, reason optional) or the
        // moderation one (`target`, reason required): runtime dispatch, not
        // the published shape, decides vote-kick versus moderation kick, so
        // accept both. An unparsable shape stays silent here so the shared
        // router keeps the interaction.
        "kick" => parse_kick_target(&command.options).map(|target| VoiceCommand::Kick {
            target,
            reason: parse_kick_reason(&command.options),
        }),
        "name" => Some(VoiceCommand::Name),
        _ => None,
    }
}

/// The vote target: our `member` option or the moderation `target` option.
fn parse_kick_target(options: &[CommandDataOption]) -> Option<Snowflake> {
    options.iter().find_map(|option| match &option.value {
        CommandOptionValue::User(id) if option.name == "member" || option.name == "target" => {
            Some(id.get())
        }
        _ => None,
    })
}

/// The optional vote reason, trimmed and length-capped like the definition.
fn parse_kick_reason(options: &[CommandDataOption]) -> Option<String> {
    options
        .iter()
        .find(|option| option.name == "reason")
        .and_then(|option| match &option.value {
            CommandOptionValue::String(value) => {
                let reason: String = value.trim().chars().take(512).collect();
                (!reason.is_empty()).then_some(reason)
            }
            _ => None,
        })
}

/// Invoking member id plus effective Manage Channels authority, from the
/// interaction payload (guild `member.user`, else the top-level user).
/// `None` when Discord sent no identifiable invoker — the handler refuses
/// closed rather than acting as nobody.
fn interaction_actor(interaction: &Interaction) -> Option<(Snowflake, bool)> {
    let user = interaction
        .member
        .as_ref()
        .and_then(|member| member.user.as_ref())
        .or(interaction.user.as_ref())?;
    let is_admin = is_voice_admin(
        interaction
            .member
            .as_ref()
            .and_then(|member| member.permissions),
    );
    Some((user.id.get(), is_admin))
}

fn parse_logging_action(options: &[CommandDataOption]) -> LoggingAction {
    let Some(sub) = options.first() else {
        return LoggingAction::Invalid;
    };
    let CommandOptionValue::SubCommand(args) = &sub.value else {
        return LoggingAction::Invalid;
    };
    let arg = |name: &str| args.iter().find(|option| option.name == name);
    match sub.name.as_str() {
        "show" => LoggingAction::Show,
        "level" => arg("level")
            .and_then(|option| match &option.value {
                CommandOptionValue::String(value) => Some(LoggingAction::Level(value.clone())),
                _ => None,
            })
            .unwrap_or(LoggingAction::Invalid),
        "channel" => LoggingAction::Channel(arg("channel").and_then(|option| match option.value {
            CommandOptionValue::Channel(channel) => Some(channel.get()),
            _ => None,
        })),
        "mention" => LoggingAction::Mention(arg("role").and_then(|option| match option.value {
            CommandOptionValue::Role(role) => Some(role.get()),
            _ => None,
        })),
        _ => LoggingAction::Invalid,
    }
}

fn parse_access_action(options: &[CommandDataOption]) -> AccessAction {
    let Some(sub) = options.first() else {
        return AccessAction::Invalid;
    };
    let CommandOptionValue::SubCommand(args) = &sub.value else {
        return AccessAction::Invalid;
    };
    let roles: Vec<Snowflake> = ["role", "role2", "role3"]
        .iter()
        .filter_map(|name| args.iter().find(|option| option.name == *name))
        .filter_map(|option| match option.value {
            CommandOptionValue::Role(role) => Some(role.get()),
            _ => None,
        })
        .fold(Vec::new(), |mut roles, role| {
            if !roles.contains(&role) {
                roles.push(role);
            }
            roles
        });
    let command = args
        .iter()
        .find(|option| option.name == "command")
        .and_then(|option| match &option.value {
            CommandOptionValue::String(value) => Some(value.trim().to_ascii_lowercase()),
            _ => None,
        });
    match sub.name.as_str() {
        "show" => AccessAction::Show,
        "creation" => args
            .iter()
            .find(|option| option.name == "enabled")
            .and_then(|option| match option.value {
                CommandOptionValue::Boolean(enabled) => Some(AccessAction::Creation(enabled)),
                _ => None,
            })
            .unwrap_or(AccessAction::Invalid),
        "role" => AccessAction::RequiredRole(roles.first().copied()),
        "restrict" => command.map_or(AccessAction::Invalid, |command| AccessAction::Restrict {
            command,
            roles,
        }),
        "unrestrict" => command.map_or(AccessAction::Invalid, AccessAction::Unrestrict),
        _ => AccessAction::Invalid,
    }
}

/// `/create`, `/textchannels` and the V8 per-creator settings commands need
/// Manage Channels; admins pass everywhere. `/setup` is open to everyone but
/// shows detail to admins only, and `/access` checks the admin flag itself.
/// Fail closed on missing permissions.
fn may_create(permissions: Option<Permissions>) -> bool {
    is_voice_admin(permissions)
}

/// The spec's "admin": Manage Channels (Administrator implies it). Fail
/// closed on missing permissions.
fn is_voice_admin(permissions: Option<Permissions>) -> bool {
    permissions.is_some_and(|permissions| {
        permissions.intersects(Permissions::ADMINISTRATOR | Permissions::MANAGE_CHANNELS)
    })
}

impl VoiceCommand {
    /// The slash-command name per-command role restrictions are keyed on.
    fn name(&self) -> &'static str {
        match self {
            Self::Create { .. } => "create",
            Self::Setup => "setup",
            Self::Ping => "ping",
            Self::Invite => "invite",
            Self::TextChannels { .. } => "textchannels",
            Self::Position { .. } => "position",
            Self::Group { .. } => "group",
            Self::InheritPermissions { .. } => "inheritpermissions",
            Self::DefaultLimit { .. } => "defaultlimit",
            Self::AlwaysPrivate { .. } => "alwaysprivate",
            Self::Access(_) => "access",
            Self::Reclaim => "reclaim",
            Self::Transfer { .. } => "transfer",
            Self::Logging(_) => "logging",
            Self::Export => "export",
            Self::Import { .. } => "import",
            // Ballots share the `kick` restriction surface: one role gate
            // covers starting votes and casting them.
            Self::Kick { .. } | Self::Ballot { .. } => "kick",
            Self::Name => "name",
        }
    }
}

/// Button namespace for V4 ballots. The button carries only the vote ID (the
/// initiating interaction ID); guild, room and target always come from the
/// worker ledger, never from the payload.
const VOTE_BUTTON_PREFIX: &str = "votekick:";

/// Parse one ballot button press. `None` when the custom ID is not ours.
fn parse_vote_button(custom_id: &str) -> Option<VoiceCommand> {
    let rest = custom_id.strip_prefix(VOTE_BUTTON_PREFIX)?;
    let (vote_id, ballot) = rest.split_once(':')?;
    let vote_id: Snowflake = vote_id.parse().ok()?;
    let ballot = match ballot {
        "yes" => VoteBallot::Yes,
        "no" => VoteBallot::No,
        _ => return None,
    };
    Some(VoiceCommand::Ballot { vote_id, ballot })
}

/// Button custom ID for one ballot. Inverse of [`parse_vote_button`].
fn vote_button_id(vote_id: Snowflake, ballot: VoteBallot) -> String {
    let vote = match ballot {
        VoteBallot::Yes => "yes",
        VoteBallot::No => "no",
    };
    format!("{VOTE_BUTTON_PREFIX}{vote_id}:{vote}")
}

/// The invoking member's access facts: effective admin flag and role IDs.
fn access_member(interaction: &Interaction) -> AccessMember {
    let member = interaction.member.as_ref();
    AccessMember {
        is_admin: is_voice_admin(member.and_then(|member| member.permissions)),
        roles: member.map_or_else(Vec::new, |member| {
            member.roles.iter().map(|role| role.get()).collect()
        }),
    }
}

fn access_denied_text(reason: AccessDenyReason) -> &'static str {
    match reason {
        AccessDenyReason::RequiredRole => {
            "You need the server's required role to use voice-room commands."
        }
        AccessDenyReason::CommandRestricted => {
            "You do not have a role that may use this command here."
        }
    }
}

/// Discord snowflake epoch (2015-01-01T00:00:00Z) in Unix milliseconds.
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

/// Milliseconds between Discord creating an interaction and `now_ms`.
/// A host clock behind Discord's saturates to zero instead of underflowing.
#[must_use]
fn interaction_latency_ms(interaction_id: u64, now_ms: u64) -> u64 {
    let created_ms = (interaction_id >> 22).saturating_add(DISCORD_EPOCH_MS);
    now_ms.saturating_sub(created_ms)
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Public vote-kick ballot message for a freshly started vote.
#[must_use]
fn vote_message(
    update: &VoteKickUpdate,
    initiator: Snowflake,
    target: Snowflake,
    reason: Option<&str>,
) -> InteractionResponse {
    let mut lines = vec![format!(
        "<@{target}> — <@{initiator}> started a vote to disconnect them from this voice room."
    )];
    if let Some(reason) = reason {
        lines.push(format!("Reason: {reason}"));
    }
    lines.push(format!(
        "Vote with the buttons: {}/{} needed. Not voting counts as No. The vote ends in 2 minutes.",
        update.progress.required, update.progress.total,
    ));
    ballot_response(
        &lines.join("\n"),
        update,
        target,
        InteractionResponseType::ChannelMessageWithSource,
        false,
    )
}

/// Follow-up ballot state for an in-flight vote, delivered as an in-place
/// message update so the ballot stays a single message.
#[must_use]
fn vote_update_message(update: &VoteKickUpdate) -> InteractionResponse {
    let content = match update.status {
        VoteKickStatus::Active => format!(
            "Vote-kick <@{}>: {}/{} needed. Not voting counts as No. The vote ends in 2 minutes.",
            update.vote.target_id, update.progress.required, update.progress.total,
        ),
        VoteKickStatus::Passed => format!(
            "Vote passed: <@{}> will be disconnected and kept out of this room.",
            update.vote.target_id
        ),
        VoteKickStatus::Expired => "Vote expired with too few Yes votes.".to_owned(),
        VoteKickStatus::Cancelled(VoteCancellation::TargetLeft) => {
            "Vote cancelled: the target left the room.".to_owned()
        }
        VoteKickStatus::Cancelled(VoteCancellation::TargetProtected) => {
            "Vote cancelled: the target can no longer be voted out.".to_owned()
        }
    };
    ballot_response(
        &content,
        update,
        update.vote.target_id,
        InteractionResponseType::UpdateMessage,
        update.status != VoteKickStatus::Active,
    )
}

/// Shared ballot shell: public message plus Yes/No buttons bound to the vote.
/// Buttons disable once the vote leaves Active so late presses cannot imply
/// a live ballot (the worker still rejects them by ID).
fn ballot_response(
    content: &str,
    update: &VoteKickUpdate,
    target: Snowflake,
    kind: InteractionResponseType,
    disabled: bool,
) -> InteractionResponse {
    let button = |ballot: VoteBallot, label: &str, style: ButtonStyle| {
        Component::Button(Button {
            id: None,
            custom_id: Some(vote_button_id(update.vote.id, ballot)),
            disabled,
            emoji: None,
            label: Some(label.to_owned()),
            style,
            url: None,
            sku_id: None,
        })
    };
    InteractionResponse {
        kind,
        data: Some(InteractionResponseData {
            content: Some(content.to_owned()),
            allowed_mentions: Some(AllowedMentions {
                parse: Vec::new(),
                users: vec![Id::<UserMarker>::new(target)],
                roles: Vec::new(),
                replied_user: false,
            }),
            components: Some(vec![Component::ActionRow(ActionRow {
                id: None,
                components: vec![
                    button(VoteBallot::Yes, "Yes", ButtonStyle::Danger),
                    button(VoteBallot::No, "No", ButtonStyle::Secondary),
                ],
            })]),
            ..Default::default()
        }),
    }
}

/// One-line refusal for a rejected vote-kick start or ballot.
fn kick_refusal_text(refusal: &KickRefusal) -> &'static str {
    match refusal {
        KickRefusal::Unavailable => "Voice state is syncing right now. Try again shortly.",
        KickRefusal::NotARoom => "That member is not in a temporary voice room.",
        KickRefusal::Vote(VoteKickError::InitiatorNotOccupant) => {
            "Only room occupants can start a vote."
        }
        KickRefusal::Vote(VoteKickError::TargetNotOccupant) => "The target must be in the room.",
        KickRefusal::Vote(VoteKickError::SelfTarget) => "You cannot start a vote against yourself.",
        KickRefusal::Vote(VoteKickError::ProtectedTarget) => {
            "The room owner and original creator cannot be voted out."
        }
        KickRefusal::Vote(VoteKickError::ActiveVoteExists) => {
            "A vote is already active for that member."
        }
        KickRefusal::Vote(VoteKickError::ReusedVoteId) => {
            "That vote was already started. Try again."
        }
        KickRefusal::Vote(VoteKickError::UnknownVote | VoteKickError::WrongVoteBoundary) => {
            "That vote has already ended."
        }
        KickRefusal::Vote(VoteKickError::IneligibleVoter) => {
            "Only current occupants other than the target can vote."
        }
        KickRefusal::Vote(VoteKickError::RepeatedVote) => "You have already voted.",
        KickRefusal::Vote(VoteKickError::InvalidTime) => {
            "The vote clock disagrees with this device. Try again shortly."
        }
    }
}

/// Ephemeral `ChannelMessageWithSource` reply shell.
#[must_use]
pub fn ephemeral_response(content: &str) -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(InteractionResponseData {
            content: Some(content.to_owned()),
            flags: Some(MessageFlags::EPHEMERAL),
            ..Default::default()
        }),
    }
}

/// Live worker snapshot for `/setup`: room count, recent failure lines and
/// the credential-halt flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerStatus {
    pub tracked_rooms: usize,
    /// Current permission-health findings, rendered for `/setup`.
    pub health: Vec<String>,
    pub failures: Vec<String>,
    pub halted: bool,
}

/// Notice body for one failure. `brief` includes actionable permission/name
/// causes; `full` adds every failure. Callers gate `off` on [`should_log`] first.
fn notice_text(failure: &LifecycleFailure, level: DetailLevel) -> String {
    let mut text = match (level, failure) {
        (DetailLevel::Full, _)
        | (DetailLevel::Brief, LifecycleFailure::MissingPermission { .. })
        | (DetailLevel::Brief, LifecycleFailure::NameBlocked { .. }) => format!(
            "Voice rooms need attention: {}. Run /setup to see all current problems.",
            failure_line(failure)
        ),
        _ => "Voice rooms need attention. Run /setup to see the current problems.".to_owned(),
    };
    if text.chars().count() > NOTICE_MAX_CHARS {
        text = text.chars().take(NOTICE_MAX_CHARS).collect();
    }
    text
}

fn failure_line(failure: &LifecycleFailure) -> String {
    match failure {
        LifecycleFailure::CategoryFull {
            creator_id,
            message,
        } => format!("create <#{creator_id}>: {message}"),
        LifecycleFailure::CreateRefused {
            creator_id,
            reason,
            message,
        } => format!(
            "create <#{creator_id}> refused ({}): {message}",
            reason.code()
        ),
        LifecycleFailure::Discord { channel_id, error } => {
            format!("channel <#{channel_id}>: {error}")
        }
        LifecycleFailure::Persistence { channel_id, error } => match channel_id {
            Some(channel) => format!("store <#{channel}>: {error}"),
            None => format!("store: {error}"),
        },
        LifecycleFailure::MissingPermission {
            write,
            channel_id,
            findings,
        } => {
            let (verb, doing) = match write {
                RefusedWrite::Create => ("create", "creating the room"),
                RefusedWrite::Move => ("move", "moving the member into the room"),
            };
            if findings.is_empty() {
                format!(
                    "{verb} <#{channel_id}>: Discord refused {doing}; the bot needs Manage Channels, Move Members, View Channel and Connect on <#{channel_id}>, so check for a deny override"
                )
            } else {
                let causes: Vec<String> = findings.iter().map(finding_clause).collect();
                format!(
                    "{verb} <#{channel_id}>: Discord refused {doing}; {}",
                    causes.join("; ")
                )
            }
        }
        LifecycleFailure::NameBlocked { creator_id, error } => {
            let why = error.filter().map_or_else(
                || "invalid name".to_owned(),
                |filter| filter.as_str().replace('_', " "),
            );
            format!(
                "create <#{creator_id}>: {NAME_BLOCKED_AUDIT_REASON}, no room created (the room name template is blocked by the automod name filter: {why}); fix the template or the automod policy"
            )
        }
    }
}

/// Execute one `/create`: validate the name, single-attempt the voice channel
/// POST, then store the creator row. A REST failure refuses without touching
/// the store; a store failure deletes the new channel as compensation, and a
/// failed compensation names the orphan channel id (untracked, so reconcile
/// will not delete it) for manual cleanup instead of claiming removal.
async fn execute_create<S: RoomPersistence, H: RoomWrites>(
    store: &S,
    http: &H,
    guild_id: Snowflake,
    name: &str,
    on_created: impl FnOnce(&CreatorChannel, Channel),
) -> String {
    let attributes = RoomChannelAttributes {
        parent_id: None,
        bitrate: None,
        rtc_region: None,
        video_quality_mode: None,
        nsfw: false,
        user_limit: 0,
        position: None,
        overwrites: Vec::new(),
    };
    let always: WriteGuard = Arc::new(|| true);
    let channel = match http
        .create(guild_id, name, &attributes, always.clone())
        .await
    {
        Ok(channel) => channel,
        Err(error) => return create_error_text(error),
    };
    let channel_id = channel.id.get();
    let creator = CreatorChannel::new(guild_id, channel_id);
    match store.add_creator(&creator).await {
        Ok(()) => {
            on_created(&creator, channel);
            format!("Created <#{channel_id}>: join it to spin up temporary voice rooms.")
        }
        Err(error) => {
            let compensation = http.delete(channel_id, always).await;
            let removed = compensation.is_ok();
            if !removed {
                // Untracked orphan: reconcile will not delete it, so count it
                // here with no channel ID. The ephemeral reply above names the
                // channel for manual cleanup.
                metrics::global().voice_orphan();
                warn!(
                    voice_event = "voice_creator_orphan",
                    outcome = "manual_needed",
                    "voice creator orphan needs manual deletion"
                );
            }
            match error {
                StoreError::CredentialRefused if removed => "Voice rooms are paused: the database refused the bot credential, so the new channel was removed. Tell an admin to fix it, then restart the bot.".to_owned(),
                StoreError::CredentialRefused => format!("Voice rooms are paused: the database refused the bot credential, and removing the new channel failed. Delete <#{channel_id}> manually, then tell an admin to fix the database."),
                _ if removed => "Could not save the new creator channel, so it was removed. Try again.".to_owned(),
                _ => format!("Could not save the new creator channel, and removing it failed. Delete <#{channel_id}> manually and try again."),
            }
        }
    }
}

/// Render the controls for `/access show` and as the confirmation after a change.
fn access_summary(controls: &AccessControls) -> String {
    let mut lines = vec![
        format!(
            "Room creation: {}",
            if controls.room_creation_enabled {
                "on"
            } else {
                "off (existing rooms and commands keep working)"
            }
        ),
        match controls.required_role {
            Some(role) => format!("Required role: <@&{role}>"),
            None => "Required role: none".to_owned(),
        },
    ];
    if controls.command_roles.is_empty() {
        lines.push("Restricted commands: none".to_owned());
    } else {
        lines.push("Restricted commands:".to_owned());
        for (command, roles) in &controls.command_roles {
            let who = if roles.is_empty() {
                "admins only".to_owned()
            } else {
                roles
                    .iter()
                    .map(|role| format!("<@&{role}>"))
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            lines.push(format!("- /{command}: {who}"));
        }
    }
    lines.join("\n")
}

/// Apply one `/access` action: read, change, validate, save, then hand the
/// saved controls to `on_saved` (the live actor). A failed read or write
/// changes nothing and says so.
async fn execute_access<S: RoomPersistence>(
    store: &S,
    guild_id: Snowflake,
    action: AccessAction,
    on_saved: impl FnOnce(AccessControls),
) -> String {
    let Ok(mut controls) = store.access_controls(guild_id).await else {
        return "Could not read the voice-room settings. Nothing was changed; try again."
            .to_owned();
    };
    match action {
        AccessAction::Show => return access_summary(&controls),
        AccessAction::Invalid => {
            return "Unknown /access option. Use show, creation, role, restrict or unrestrict."
                .to_owned()
        }
        AccessAction::Creation(enabled) => controls.room_creation_enabled = enabled,
        AccessAction::RequiredRole(role) => controls.required_role = role,
        AccessAction::Restrict { command, roles } => {
            if !is_voice_command(&command) {
                return format!("/{command} is not a voice-room command, so nothing was changed.");
            }
            controls.command_roles.insert(command, roles);
        }
        AccessAction::Unrestrict(command) => {
            if controls.command_roles.remove(&command).is_none() {
                return format!("/{command} has no role restriction, so nothing was changed.");
            }
        }
    }
    if validate_access_controls(&controls).is_err() {
        return "Those settings are not valid, so nothing was changed.".to_owned();
    }
    match store.save_access_controls(guild_id, &controls).await {
        Ok(()) => {
            let summary = access_summary(&controls);
            on_saved(controls);
            format!("Saved.\n{summary}")
        }
        Err(_) => {
            "Could not save the voice-room settings. Nothing was changed; try again.".to_owned()
        }
    }
}

/// Render the settings for `/logging show` and as the confirmation after a change.
fn logging_summary(settings: &LoggingSettings) -> String {
    let level = match settings.level {
        DetailLevel::Off => "off (no notices are sent)",
        DetailLevel::Brief => "brief",
        DetailLevel::Full => "full",
    };
    let channel = match settings.channel_id {
        Some(channel) => format!("<#{channel}>"),
        None => "not set (falls back to the server's system channel, then a DM, then the creator channel's chat)".to_owned(),
    };
    let mention = match settings.mention_role_id {
        Some(role) => format!("<@&{role}>"),
        None => "none".to_owned(),
    };
    format!("Log level: {level}\nLog channel: {channel}\nMentioned on errors: {mention}")
}

/// Apply one `/logging` action: read, change, save. A failed read or write
/// changes nothing and says so.
async fn execute_logging<S: RoomPersistence>(
    store: &S,
    guild_id: Snowflake,
    action: LoggingAction,
) -> String {
    let Ok(mut settings) = store.logging_settings(guild_id).await else {
        return "Could not read the logging settings. Nothing was changed; try again.".to_owned();
    };
    match action {
        LoggingAction::Show => return logging_summary(&settings),
        LoggingAction::Invalid => {
            return "Unknown /logging option. Use show, level, channel or mention.".to_owned()
        }
        LoggingAction::Level(raw) => match parse_detail_level(&raw) {
            Ok(level) => settings.level = level,
            Err(error) => return format!("{error} Nothing was changed."),
        },
        LoggingAction::Channel(channel) => settings.channel_id = channel,
        LoggingAction::Mention(role) => settings.mention_role_id = role,
    }
    match store.save_logging_settings(guild_id, &settings).await {
        Ok(()) => format!("Saved.\n{}", logging_summary(&settings)),
        Err(_) => "Could not save the logging settings. Nothing was changed; try again.".to_owned(),
    }
}

fn create_error_text(error: RoomHttpError) -> String {
    match error {
        RoomHttpError::Unauthorized => "Voice rooms are paused: Discord refused the bot credential. Tell an admin to fix the token, then restart the bot.".to_owned(),
        RoomHttpError::AccessDenied => {
            "I cannot create channels here: I need Manage Channels. Tell an admin to fix my permissions.".to_owned()
        }
        RoomHttpError::RateLimited { retry_after_ms, .. } => {
            format!("Discord is rate-limiting channel creates; try again in {}s.", retry_after_ms.div_ceil(1000))
        }
        _ => "Discord refused the channel create. Check my permissions and try again.".to_owned(),
    }
}

/// Translate a V2 ownership refusal into the ephemeral reply text (spec
/// `docs/voice-rooms.md` §V2 accept lines). Refusals change nothing: the
/// caller decides again on the next attempt.
fn ownership_refusal(error: OwnershipError) -> String {
    match error {
        OwnershipError::NotOriginalCreator => {
            "Only the original creator can reclaim this room while its owner is still here."
                .to_owned()
        }
        OwnershipError::NotOwner => {
            "Only the room owner or an admin can transfer this room.".to_owned()
        }
        OwnershipError::ActorNotInRoom => {
            "You need to be in the room to use this command.".to_owned()
        }
        OwnershipError::TargetNotInRoom => "The transfer recipient must be in the room.".to_owned(),
        OwnershipError::BotActor | OwnershipError::BotTarget => {
            "Bots can't own temporary rooms.".to_owned()
        }
        OwnershipError::InvalidMemberId
        | OwnershipError::DuplicateMember(_)
        | OwnershipError::BotOwnership
        | OwnershipError::EmptyRoom => {
            "Something's off with this room's membership data — leave and rejoin, then try again."
                .to_owned()
        }
    }
}

/// Handle one interaction, replying exactly once. Returns true when the
/// interaction was a voice command (even when refused); false means another
/// slice owns it. `reply` performs the single response attempt.
pub async fn handle_voice_interaction<S, H, F>(
    runtime: &VoiceRuntime<S, H>,
    interaction: &Interaction,
    reply: impl FnOnce(InteractionResponse) -> F + Send,
) -> bool
where
    S: RoomPersistence + Send + 'static,
    H: RoomWrites + Send + 'static,
    F: Future<Output = ()> + Send,
{
    handle_voice_interaction_with(runtime, interaction, None, None, reply).await
}

/// [`handle_voice_interaction`] with the guild's vanity invite code, which
/// only the gateway cache knows. `None` renders the "no invite" notice.
/// `inventory` is the trusted live guild inventory for `/export` and
/// `/import`; `None` refuses those two commands as unavailable.
pub async fn handle_voice_interaction_with<S, H, F>(
    runtime: &VoiceRuntime<S, H>,
    interaction: &Interaction,
    invite_code: Option<&str>,
    inventory: Option<&GuildInventory>,
    reply: impl FnOnce(InteractionResponse) -> F + Send,
) -> bool
where
    S: RoomPersistence + Send + 'static,
    H: RoomWrites + Send + 'static,
    F: Future<Output = ()> + Send,
{
    handle_voice_interaction_full(
        runtime,
        interaction,
        invite_code,
        inventory,
        &NameDirectory::default(),
        reply,
    )
    .await
}

/// [`handle_voice_interaction_with`] plus the display names `/name` renders
/// `@@owner@@` from, which only the gateway cache knows. An empty directory
/// renders every member as "member".
pub async fn handle_voice_interaction_full<S, H, F>(
    runtime: &VoiceRuntime<S, H>,
    interaction: &Interaction,
    invite_code: Option<&str>,
    inventory: Option<&GuildInventory>,
    names: &NameDirectory,
    reply: impl FnOnce(InteractionResponse) -> F + Send,
) -> bool
where
    S: RoomPersistence + Send + 'static,
    H: RoomWrites + Send + 'static,
    F: Future<Output = ()> + Send,
{
    let Some(guild_id) = interaction_guild(interaction) else {
        return false;
    };
    if let Some(action) = voice_import_action(interaction) {
        return handle_import_component(runtime, interaction, guild_id, action, inventory, reply)
            .await;
    }
    if let Some(action) = name_component_action(interaction) {
        return handle_name_interaction(runtime, interaction, guild_id, names, action, true, reply)
            .await;
    }
    let Some(command) = parse_voice_command(interaction) else {
        return false;
    };
    // Guild-level role gate first. Settings that cannot be read fail closed for
    // members: only an admin proceeds without them.
    let member = access_member(interaction);
    let (gate_store, _) = runtime.make_pair();
    if let Some(denial) = command_gate(&gate_store, guild_id, &member, command.name()).await {
        reply(denial).await;
        return true;
    }
    match command {
        VoiceCommand::Ping => {
            let latency = interaction_latency_ms(interaction.id.get(), unix_now_ms());
            reply(ephemeral_response(&ping_render(latency))).await;
            true
        }
        VoiceCommand::Invite => {
            reply(ephemeral_response(&invite_render(invite_code))).await;
            true
        }
        VoiceCommand::Access(action) => {
            if !member.is_admin {
                reply(ephemeral_response(
                    "You need Manage Channels to use /access.",
                ))
                .await;
                return true;
            }
            let _serialized = runtime.access_lock.lock().await;
            let (store, _) = runtime.make_pair();
            let text = execute_access(&store, guild_id, action, |controls| {
                runtime.access_changed(guild_id, controls);
            })
            .await;
            reply(ephemeral_response(&text)).await;
            true
        }
        VoiceCommand::Logging(action) => {
            if !member.is_admin {
                reply(ephemeral_response(
                    "You need Manage Channels to use /logging.",
                ))
                .await;
                return true;
            }
            // Same lock as `/access`: admin-only and rare, so one lock is enough.
            let _serialized = runtime.access_lock.lock().await;
            let (store, _) = runtime.make_pair();
            let text = execute_logging(&store, guild_id, action).await;
            reply(ephemeral_response(&text)).await;
            true
        }
        VoiceCommand::Kick { target, reason } => {
            let Some(initiator) = interaction.author_id().map(|id| id.get()) else {
                reply(ephemeral_response(
                    "Could not tell who started the vote. Try again.",
                ))
                .await;
                return true;
            };
            // Resolve the room first. The sink only reaches this arm for
            // claimed votes (it returns before deferring otherwise, leaving
            // the router to answer), so a refusal here is exactly once.
            // `kick_start` revalidates atomically, so a join in between
            // cannot corrupt the ledger.
            let Some(room_id) = runtime.kick_room_of(guild_id, target).await else {
                reply(ephemeral_response(kick_refusal_text(
                    &KickRefusal::NotARoom,
                )))
                .await;
                return true;
            };
            match runtime
                .kick_start(guild_id, interaction.id.get(), room_id, initiator, target)
                .await
            {
                None => {
                    reply(ephemeral_response(
                        "Voice state is syncing right now. Try again shortly.",
                    ))
                    .await;
                }
                Some(Ok(update)) => {
                    reply(vote_message(&update, initiator, target, reason.as_deref())).await;
                }
                Some(Err(refusal)) => {
                    reply(ephemeral_response(kick_refusal_text(&refusal))).await;
                }
            }
            true
        }
        VoiceCommand::Ballot { vote_id, ballot } => {
            let Some(voter) = interaction.author_id().map(|id| id.get()) else {
                reply(ephemeral_response("Could not tell who voted. Try again.")).await;
                return true;
            };
            match runtime.kick_ballot(guild_id, vote_id, voter, ballot).await {
                None => {
                    reply(ephemeral_response(
                        "Voice state is syncing right now. Try again shortly.",
                    ))
                    .await;
                }
                Some(Ok(update)) => {
                    reply(vote_update_message(&update)).await;
                }
                Some(Err(refusal)) => {
                    reply(ephemeral_response(kick_refusal_text(&refusal))).await;
                }
            }
            true
        }
        VoiceCommand::Setup => {
            let permissions = interaction
                .member
                .as_ref()
                .and_then(|member| member.permissions);
            let status = runtime.worker_status(guild_id).await;
            let panel = if is_voice_admin(permissions) {
                let (store, _) = runtime.make_pair();
                let (creators, store_error) = match store.creators(guild_id).await {
                    Ok(creators) => (creators, None),
                    Err(error) => (Vec::new(), Some(error.to_string())),
                };
                setup_panel(&SetupSummary {
                    guild_id,
                    creators,
                    tracked_rooms: status.as_ref().map_or(0, |status| status.tracked_rooms),
                    // Current health findings first: they are live, failures are history.
                    failures: status.as_ref().map_or_else(Vec::new, |status| {
                        status
                            .health
                            .iter()
                            .chain(status.failures.iter())
                            .cloned()
                            .collect()
                    }),
                    halted: status.as_ref().is_some_and(|status| status.halted),
                    store_error,
                })
            } else {
                // No store read for a member: the generic view needs none, and
                // an open command should not cost a query per invocation.
                setup_member_panel(
                    status.as_ref().is_some_and(|status| status.halted),
                    status.as_ref().is_some_and(|status| {
                        !status.health.is_empty() || !status.failures.is_empty()
                    }),
                )
            };
            reply(ephemeral_response(&format!(
                "**{}**\n{}",
                panel.title, panel.description
            )))
            .await;
            true
        }
        VoiceCommand::Create { name } => {
            let permissions = interaction
                .member
                .as_ref()
                .and_then(|member| member.permissions);
            if !may_create(permissions) {
                reply(ephemeral_response(
                    "You need Manage Channels to use /create.",
                ))
                .await;
                return true;
            }
            let text = match decide_create_channel(CreateChannelRequest { guild_id, name }) {
                CreateChannelPlan::Refuse { message } => message,
                CreateChannelPlan::Create { guild_id, name } => {
                    let channel_id = interaction
                        .channel
                        .as_ref()
                        .map_or(0, |channel| channel.id.get());
                    let user_id = interaction.author_id().map_or(0, |id| id.get());
                    match runtime.filter_creator_name(guild_id, channel_id, user_id, &name) {
                        Err(refusal) => refusal,
                        Ok(name) => {
                            let (store, http) = runtime.make_pair();
                            execute_create(&store, &http, guild_id, &name, |creator, channel| {
                                runtime.creator_added(creator, channel);
                            })
                            .await
                        }
                    }
                }
            };
            reply(ephemeral_response(&text)).await;
            true
        }
        VoiceCommand::TextChannels {
            channel_id,
            request,
        } => {
            let permissions = interaction
                .member
                .as_ref()
                .and_then(|member| member.permissions);
            if !may_create(permissions) {
                reply(ephemeral_response(
                    "You need Manage Channels to use /textchannels.",
                ))
                .await;
                return true;
            }
            let (store, _) = runtime.make_pair();
            let text = match store.creator_for(guild_id, channel_id).await {
                Err(error) => format!("Could not read the creator channel ({error}). Try again."),
                Ok(creator) => match decide_text_channels(creator, &request) {
                    TextChannelsPlan::Refuse { message } => message,
                    TextChannelsPlan::Update(creator) => match store.add_creator(&creator).await {
                        Ok(()) => {
                            runtime.creator_updated(&creator);
                            text_channels_summary(&creator)
                        }
                        Err(StoreError::CredentialRefused) => "Voice rooms are paused: the database refused the bot credential. Tell an admin to fix it, then restart the bot.".to_owned(),
                        Err(error) => {
                            format!("Could not save the companion settings ({error}). Try again.")
                        }
                    },
                },
            };
            reply(ephemeral_response(&text)).await;
            true
        }
        VoiceCommand::Position {
            channel_id,
            request,
        } => {
            let permissions = interaction
                .member
                .as_ref()
                .and_then(|member| member.permissions);
            if !may_create(permissions) {
                reply(ephemeral_response(
                    "You need Manage Channels to use /position.",
                ))
                .await;
                return true;
            }
            let (store, _) = runtime.make_pair();
            let text = match store.creator_for(guild_id, channel_id).await {
                Err(error) => format!("Could not read the creator channel ({error}). Try again."),
                Ok(creator) => match decide_position(creator, &request) {
                    PositionPlan::Refuse { message } => message,
                    PositionPlan::Update(creator) => match store.add_creator(&creator).await {
                        Ok(()) => {
                            runtime.creator_updated(&creator);
                            position_summary(&creator)
                        }
                        Err(StoreError::CredentialRefused) => "Voice rooms are paused: the database refused the bot credential. Tell an admin to fix it, then restart the bot.".to_owned(),
                        Err(error) => {
                            format!("Could not save the position settings ({error}). Try again.")
                        }
                    },
                },
            };
            reply(ephemeral_response(&text)).await;
            true
        }
        VoiceCommand::Group {
            channel_id,
            request,
        } => {
            let permissions = interaction
                .member
                .as_ref()
                .and_then(|member| member.permissions);
            if !may_create(permissions) {
                reply(ephemeral_response(
                    "You need Manage Channels to use /group.",
                ))
                .await;
                return true;
            }
            let (store, _) = runtime.make_pair();
            let text = match store.creator_for(guild_id, channel_id).await {
                Err(error) => format!("Could not read the creator channel ({error}). Try again."),
                Ok(creator) => match decide_group(creator, &request) {
                    GroupPlan::Refuse { message } => message,
                    GroupPlan::Update(creator) => match store.add_creator(&creator).await {
                        Ok(()) => {
                            runtime.creator_updated(&creator);
                            group_summary(&creator)
                        }
                        Err(StoreError::CredentialRefused) => "Voice rooms are paused: the database refused the bot credential. Tell an admin to fix it, then restart the bot.".to_owned(),
                        Err(error) => {
                            format!("Could not save the grouping settings ({error}). Try again.")
                        }
                    },
                },
            };
            reply(ephemeral_response(&text)).await;
            true
        }
        VoiceCommand::InheritPermissions {
            channel_id,
            request,
        } => {
            let permissions = interaction
                .member
                .as_ref()
                .and_then(|member| member.permissions);
            if !may_create(permissions) {
                reply(ephemeral_response(
                    "You need Manage Channels to use /inheritpermissions.",
                ))
                .await;
                return true;
            }
            let (store, _) = runtime.make_pair();
            let text = match store.creator_for(guild_id, channel_id).await {
                Err(error) => format!("Could not read the creator channel ({error}). Try again."),
                Ok(creator) => match decide_inherit_permissions(creator, &request) {
                    InheritPermissionsPlan::Refuse { message } => message,
                    InheritPermissionsPlan::Update(creator) => match store.add_creator(&creator).await {
                        Ok(()) => {
                            runtime.creator_updated(&creator);
                            inherit_permissions_summary(&creator)
                        }
                        Err(StoreError::CredentialRefused) => "Voice rooms are paused: the database refused the bot credential. Tell an admin to fix it, then restart the bot.".to_owned(),
                        Err(error) => {
                            format!("Could not save the permission settings ({error}). Try again.")
                        }
                    },
                },
            };
            reply(ephemeral_response(&text)).await;
            true
        }
        VoiceCommand::DefaultLimit {
            channel_id,
            request,
        } => {
            let permissions = interaction
                .member
                .as_ref()
                .and_then(|member| member.permissions);
            if !may_create(permissions) {
                reply(ephemeral_response(
                    "You need Manage Channels to use /defaultlimit.",
                ))
                .await;
                return true;
            }
            let (store, _) = runtime.make_pair();
            let text = match store.creator_for(guild_id, channel_id).await {
                Err(error) => format!("Could not read the creator channel ({error}). Try again."),
                Ok(creator) => match decide_default_limit(creator, &request) {
                    DefaultLimitPlan::Refuse { message } => message,
                    DefaultLimitPlan::Update(creator) => match store.add_creator(&creator).await {
                        Ok(()) => {
                            runtime.creator_updated(&creator);
                            default_limit_summary(&creator)
                        }
                        Err(StoreError::CredentialRefused) => "Voice rooms are paused: the database refused the bot credential. Tell an admin to fix it, then restart the bot.".to_owned(),
                        Err(error) => {
                            format!("Could not save the limit settings ({error}). Try again.")
                        }
                    },
                },
            };
            reply(ephemeral_response(&text)).await;
            true
        }
        VoiceCommand::AlwaysPrivate {
            channel_id,
            request,
        } => {
            let permissions = interaction
                .member
                .as_ref()
                .and_then(|member| member.permissions);
            if !may_create(permissions) {
                reply(ephemeral_response(
                    "You need Manage Channels to use /alwaysprivate.",
                ))
                .await;
                return true;
            }
            let (store, _) = runtime.make_pair();
            let text = match store.creator_for(guild_id, channel_id).await {
                Err(error) => format!("Could not read the creator channel ({error}). Try again."),
                Ok(creator) => match decide_always_private(creator, &request) {
                    AlwaysPrivatePlan::Refuse { message } => message,
                    AlwaysPrivatePlan::Update(creator) => match store.add_creator(&creator).await {
                        Ok(()) => {
                            runtime.creator_updated(&creator);
                            always_private_summary(&creator)
                        }
                        Err(StoreError::CredentialRefused) => "Voice rooms are paused: the database refused the bot credential. Tell an admin to fix it, then restart the bot.".to_owned(),
                        Err(error) => {
                            format!("Could not save the privacy settings ({error}). Try again.")
                        }
                    },
                },
            };
            reply(ephemeral_response(&text)).await;
            true
        }
        VoiceCommand::Reclaim => {
            let Some((actor_id, is_admin)) = interaction_actor(interaction) else {
                reply(ephemeral_response(
                    "I couldn't tell who invoked /reclaim — try again.",
                ))
                .await;
                return true;
            };
            let text = runtime
                .run_ownership(guild_id, actor_id, is_admin, OwnershipCommand::Reclaim)
                .await
                .unwrap_or_else(|| {
                    "The voice worker isn't warmed up yet — try again in a moment.".to_owned()
                });
            reply(ephemeral_response(&text)).await;
            true
        }
        VoiceCommand::Transfer { target_id } => {
            let Some((actor_id, is_admin)) = interaction_actor(interaction) else {
                reply(ephemeral_response(
                    "I couldn't tell who invoked /transfer — try again.",
                ))
                .await;
                return true;
            };
            // Zero is a malformed payload, not a member (parse default): fail
            // closed before touching the worker.
            if target_id == 0 {
                reply(ephemeral_response(
                    "Choose a member in the room to transfer to.",
                ))
                .await;
                return true;
            }
            let text = runtime
                .run_ownership(
                    guild_id,
                    actor_id,
                    is_admin,
                    OwnershipCommand::Transfer { target_id },
                )
                .await
                .unwrap_or_else(|| {
                    "The voice worker isn't warmed up yet — try again in a moment.".to_owned()
                });
            reply(ephemeral_response(&text)).await;
            true
        }
        VoiceCommand::Name => {
            handle_name_interaction(
                runtime,
                interaction,
                guild_id,
                names,
                NameInteraction::Panel,
                false,
                reply,
            )
            .await
        }
        VoiceCommand::Export => {
            let Some(inventory) = inventory else {
                reply(ephemeral_response(
                    "Voice configuration is unavailable right now. Try again shortly.",
                ))
                .await;
                return true;
            };
            let permissions = interaction
                .member
                .as_ref()
                .and_then(|member| member.permissions);
            if !may_manage_server(permissions) {
                reply(ephemeral_response("You need Manage Server to use /export.")).await;
                return true;
            }
            let (store, _) = runtime.make_pair();
            let config = match store.config_snapshot(guild_id).await {
                Ok(config) => config,
                Err(_) => {
                    reply(ephemeral_response(
                        "Could not read the voice configuration. Nothing was sent; try again.",
                    ))
                    .await;
                    return true;
                }
            };
            match export_configuration(&config, inventory) {
                Ok(bytes) => {
                    reply(export_file_response(
                        &format!(
                            "Voice configuration for this server (v{}). Re-import it with /import.",
                            VOICE_CONFIG_VERSION
                        ),
                        export_filename(guild_id),
                        bytes,
                    ))
                    .await;
                }
                Err(error) => {
                    reply(ephemeral_response(&format!(
                        "Could not export the voice configuration: {error} Nothing was sent."
                    )))
                    .await;
                }
            }
            true
        }
        VoiceCommand::Import { file_id } => {
            handle_import_upload(runtime, interaction, guild_id, file_id, inventory, reply).await
        }
    }
}

/// Handle one `/import file` upload: Manage Server check, attachment size
/// check before download, snapshot, plan, and either refuse, show the diff
/// preview with Confirm/Cancel, or note an empty diff. Always replies
/// exactly once and returns true.
async fn handle_import_upload<S, H, F>(
    runtime: &VoiceRuntime<S, H>,
    interaction: &Interaction,
    guild_id: Snowflake,
    file_id: Option<Id<AttachmentMarker>>,
    inventory: Option<&GuildInventory>,
    reply: impl FnOnce(InteractionResponse) -> F + Send,
) -> bool
where
    S: RoomPersistence + Send + 'static,
    H: RoomWrites + Send + 'static,
    F: Future<Output = ()> + Send,
{
    let Some(inventory) = inventory else {
        reply(ephemeral_response(
            "Voice configuration is unavailable right now. Try again shortly.",
        ))
        .await;
        return true;
    };
    let permissions = interaction
        .member
        .as_ref()
        .and_then(|member| member.permissions);
    if !may_manage_server(permissions) {
        reply(ephemeral_response("You need Manage Server to use /import.")).await;
        return true;
    }
    let Some(member_id) = invoker_member_id(interaction) else {
        reply(ephemeral_response(
            "Could not tell who uploaded that file. Nothing was changed; try again.",
        ))
        .await;
        return true;
    };
    let Some(file_id) = file_id else {
        reply(ephemeral_response(
            "Attach a voice configuration JSON file to /import, then try again.",
        ))
        .await;
        return true;
    };
    let attachment = match &interaction.data {
        Some(InteractionData::ApplicationCommand(command)) => command
            .resolved
            .as_ref()
            .and_then(|resolved| resolved.attachments.get(&file_id))
            .cloned(),
        _ => None,
    };
    let Some(attachment) = attachment else {
        reply(ephemeral_response(
            "Could not read that attachment. Nothing was changed; attach the file again.",
        ))
        .await;
        return true;
    };
    if attachment.size > MAX_IMPORT_BYTES as u64 {
        reply(ephemeral_response(&format!(
            "That file is too large ({} bytes; the limit is {} bytes). Nothing was changed.",
            attachment.size, MAX_IMPORT_BYTES,
        )))
        .await;
        return true;
    }
    let (store, http) = runtime.make_pair();
    let current = match store.config_snapshot(guild_id).await {
        Ok(config) => config,
        Err(_) => {
            reply(ephemeral_response(
                "Could not read the current voice configuration. Nothing was changed; try again.",
            ))
            .await;
            return true;
        }
    };
    let bytes = match http
        .download_attachment(&attachment.url, MAX_IMPORT_BYTES)
        .await
    {
        Ok(bytes) => bytes,
        Err(_) => {
            reply(ephemeral_response(
                "Could not download that file. Nothing was changed; attach it again.",
            ))
            .await;
            return true;
        }
    };
    match plan_import_preview(&current, &bytes, inventory) {
        ImportDecision::Refuse { message } => {
            reply(ephemeral_response(&message)).await;
        }
        ImportDecision::Notice { text } => {
            reply(ephemeral_response(&text)).await;
        }
        ImportDecision::Preview {
            candidate,
            hash,
            text,
        } => {
            if let Some(message) = import_creator_gate(&current, &candidate, permissions) {
                reply(ephemeral_response(&message)).await;
                return true;
            }
            runtime.remember_pending_import(guild_id, member_id, &hash, candidate);
            reply(import_preview_response(&text, member_id, &hash)).await;
        }
        ImportDecision::Apply { .. } => {
            warn!(
                guild_id,
                "import preview planned an apply; refusing without writing"
            );
            reply(ephemeral_response(
                "Could not plan that import. Nothing was changed; try again.",
            ))
            .await;
        }
    }
    true
}

/// Handle one `/import` Confirm/Cancel click: the clicker must be the
/// uploader with Manage Server, and the preview must be unexpired. Confirm
/// re-reads current state, re-diffs and applies on a hash match, or shows a
/// fresh preview when the guild changed underneath. Cancel and expiry write
/// nothing. Always replies exactly once and returns true.
async fn handle_import_component<S, H, F>(
    runtime: &VoiceRuntime<S, H>,
    interaction: &Interaction,
    guild_id: Snowflake,
    action: VoiceAction,
    inventory: Option<&GuildInventory>,
    reply: impl FnOnce(InteractionResponse) -> F + Send,
) -> bool
where
    S: RoomPersistence + Send + 'static,
    H: RoomWrites + Send + 'static,
    F: Future<Output = ()> + Send,
{
    let (member_id, hash, confirm) = match action {
        VoiceAction::ImportConfirm { member_id, hash } => (member_id, hash, true),
        VoiceAction::ImportCancel { member_id, hash } => (member_id, hash, false),
        _ => return false,
    };
    if invoker_member_id(interaction) != Some(member_id) {
        reply(ephemeral_response(
            "Only the member who uploaded the file can answer this preview.",
        ))
        .await;
        return true;
    }
    let permissions = interaction
        .member
        .as_ref()
        .and_then(|member| member.permissions);
    if !may_manage_server(permissions) {
        reply(ephemeral_response(
            "You need Manage Server to confirm an import.",
        ))
        .await;
        return true;
    }
    let member = access_member(interaction);
    let (store, _) = runtime.make_pair();
    if let Some(denial) = command_gate(&store, guild_id, &member, "import").await {
        reply(denial).await;
        return true;
    }
    // Cancel needs no inventory: dropping a pending preview must work even
    // when the guild cache is briefly unavailable.
    if !confirm {
        runtime.cancel_pending_import(guild_id, member_id, &hash);
        reply(ephemeral_response("Import cancelled. Nothing was changed.")).await;
        return true;
    }
    let Some(inventory) = inventory else {
        reply(ephemeral_response(
            "Voice configuration is unavailable right now. Try again shortly.",
        ))
        .await;
        return true;
    };
    let Some(candidate) = runtime.take_pending_import(guild_id, member_id, &hash) else {
        reply(ephemeral_response(
            "That preview expired. Nothing was changed; upload the file again for a fresh preview.",
        ))
        .await;
        return true;
    };
    let current = match store.config_snapshot(guild_id).await {
        Ok(config) => config,
        Err(_) => {
            // A transient store blip must not force a re-upload: keep the
            // preview so Confirm can be retried.
            runtime.remember_pending_import(guild_id, member_id, &hash, candidate);
            reply(ephemeral_response(
                "Could not read the current configuration. Nothing was changed; try confirming again.",
            ))
            .await;
            return true;
        }
    };
    match plan_import_confirm(&current, &candidate, inventory, &hash) {
        ImportDecision::Refuse { message } => {
            reply(ephemeral_response(&message)).await;
        }
        ImportDecision::Notice { text } => {
            reply(ephemeral_response(&text)).await;
        }
        ImportDecision::Preview {
            candidate,
            hash,
            text,
        } => {
            if let Some(message) = import_creator_gate(&current, &candidate, permissions) {
                reply(ephemeral_response(&message)).await;
                return true;
            }
            runtime.remember_pending_import(guild_id, member_id, &hash, candidate);
            reply(import_preview_response(&text, member_id, &hash)).await;
        }
        ImportDecision::Apply { candidate, message } => {
            if let Some(refusal) = import_creator_gate(&current, &candidate, permissions) {
                reply(ephemeral_response(&refusal)).await;
                return true;
            }
            match store.config_apply(guild_id, &candidate, &current).await {
                Ok(()) => {
                    reply(ephemeral_response(&message)).await;
                }
                Err(StoreError::Conflict) => {
                    // Compare-and-swap lost under the lock: the guild changed
                    // between the Confirm re-read and the locked write. The
                    // preview hash already bound (`current`, `candidate`); that
                    // binding stays, and the admin gets a fresh preview of the
                    // new state instead of a silent overwrite.
                    match store.config_snapshot(guild_id).await {
                        Ok(fresh) => {
                            match plan_import_confirm(&fresh, &candidate, inventory, &hash) {
                                ImportDecision::Refuse { message } => {
                                    reply(ephemeral_response(&message)).await;
                                }
                                ImportDecision::Notice { text } => {
                                    reply(ephemeral_response(&text)).await;
                                }
                                ImportDecision::Preview {
                                    candidate,
                                    hash,
                                    text,
                                } => {
                                    runtime.remember_pending_import(
                                        guild_id, member_id, &hash, candidate,
                                    );
                                    reply(import_preview_response(&text, member_id, &hash)).await;
                                }
                                ImportDecision::Apply {
                                    candidate: retry,
                                    message: fresh_message,
                                } => {
                                    // The fresh state still matches the hash
                                    // (the concurrent change reverted): retry
                                    // once with the fresh snapshot as expected.
                                    // The reply carries the fresh plan's
                                    // message: its change count was computed
                                    // against the fresh state, not the stale
                                    // preview's.
                                    match store.config_apply(guild_id, &retry, &fresh).await {
                                        Ok(()) => {
                                            reply(ephemeral_response(&fresh_message)).await;
                                        }
                                        Err(_) => {
                                            runtime.remember_pending_import(
                                                guild_id, member_id, &hash, retry,
                                            );
                                            reply(ephemeral_response(
                                                "That preview is stale: the configuration changed underneath. Nothing was changed; try confirming again.",
                                            ))
                                            .await;
                                        }
                                    }
                                }
                            }
                        }
                        Err(_) => {
                            runtime.remember_pending_import(guild_id, member_id, &hash, candidate);
                            reply(ephemeral_response(
                                "That preview is stale: the configuration changed underneath. Nothing was changed; try confirming again.",
                            ))
                            .await;
                        }
                    }
                }
                Err(_) => {
                    runtime.remember_pending_import(guild_id, member_id, &hash, candidate);
                    reply(ephemeral_response(
                        "Could not save the import. Nothing was changed; try confirming again.",
                    ))
                    .await;
                }
            }
        }
    }
    true
}

/// The invoking guild's vanity invite code from the gateway cache, if any.
fn vanity_code_from_cache(
    cache: &DefaultInMemoryCache,
    interaction: &Interaction,
) -> Option<String> {
    let guild_id = interaction.guild_id?;
    cache
        .guild(guild_id)
        .and_then(|guild| guild.vanity_url_code().map(str::to_owned))
}

/// Reply seam for deterministic responder tests; errors are sanitized.
pub trait InteractionReplies: Send + Sync {
    fn defer(
        &self,
        interaction: &Interaction,
    ) -> impl Future<Output = Result<(), RoomHttpError>> + Send;
    fn complete(
        &self,
        interaction: &Interaction,
        response: InteractionResponse,
    ) -> impl Future<Output = Result<(), RoomHttpError>> + Send;
    /// Send the response as the initial interaction callback (type 4 or 7).
    /// Vote-kick uses this: the public ballot and its in-place updates must
    /// not travel the ephemeral-defer + PATCH path.
    fn respond(
        &self,
        interaction: &Interaction,
        response: InteractionResponse,
    ) -> impl Future<Output = Result<(), RoomHttpError>> + Send;
}

impl InteractionReplies for RoomHttp {
    async fn defer(&self, interaction: &Interaction) -> Result<(), RoomHttpError> {
        let response = InteractionResponse {
            kind: InteractionResponseType::DeferredChannelMessageWithSource,
            data: Some(InteractionResponseData {
                flags: Some(MessageFlags::EPHEMERAL),
                ..Default::default()
            }),
        };
        self.respond_interaction(
            interaction.application_id,
            interaction.id,
            &interaction.token,
            &response,
        )
        .await
    }

    async fn complete(
        &self,
        interaction: &Interaction,
        response: InteractionResponse,
    ) -> Result<(), RoomHttpError> {
        let data = response.data.as_ref();
        let content = data
            .and_then(|data| data.content.as_deref())
            .unwrap_or_default();
        // Discord limits message content to 2000 characters, including setup
        // listings. Keep the transport valid even in a large guild.
        let content: String = content.chars().take(2000).collect();
        let attachments: &[Attachment] = data
            .and_then(|data| data.attachments.as_deref())
            .unwrap_or(&[]);
        let components: Option<&[Component]> = data.and_then(|data| data.components.as_deref());
        self.complete_interaction(
            interaction.application_id,
            &interaction.token,
            &content,
            attachments,
            components,
        )
        .await
    }

    async fn respond(
        &self,
        interaction: &Interaction,
        response: InteractionResponse,
    ) -> Result<(), RoomHttpError> {
        self.respond_interaction(
            interaction.application_id,
            interaction.id,
            &interaction.token,
            &response,
        )
        .await
    }
}

/// Gateway wrapper: retain lifecycle publication, spawn command work off-loop.
/// Discord accepts one initial callback per interaction, so a rejected or
/// ambiguous acknowledgement must never execute a non-idempotent create.
pub struct VoiceResponder<S, H, R> {
    runtime: Arc<VoiceRuntime<S, H>>,
    replies: Arc<R>,
}

impl<S, H, R> VoiceResponder<S, H, R>
where
    S: RoomPersistence + 'static,
    H: RoomWrites + 'static,
    R: InteractionReplies + 'static,
{
    pub fn new(runtime: Arc<VoiceRuntime<S, H>>, replies: Arc<R>) -> Self {
        Self { runtime, replies }
    }

    /// [`Self::respond_named`] with no display-name directory: every member
    /// renders as "member" in a `/name` template.
    #[cfg(test)]
    async fn respond_with(
        runtime: &VoiceRuntime<S, H>,
        replies: &R,
        interaction: &Interaction,
        invite_code: Option<&str>,
        inventory: Option<GuildInventory>,
    ) {
        Self::respond_named(
            runtime,
            replies,
            interaction,
            invite_code,
            inventory,
            NameDirectory::default(),
        )
        .await;
    }

    async fn respond_named(
        runtime: &VoiceRuntime<S, H>,
        replies: &R,
        interaction: &Interaction,
        invite_code: Option<&str>,
        inventory: Option<GuildInventory>,
        names: NameDirectory,
    ) {
        // Vote-kick owns its transport. A non-room `/kick` belongs to the
        // moderation path, so return before any acknowledgement and let the
        // router answer: deferring here would race it and hang on "thinking".
        if let Some(VoiceCommand::Kick { target, .. }) = parse_voice_command(interaction) {
            let Some(guild) = interaction_guild(interaction) else {
                return;
            };
            if runtime.kick_room_of(guild, target).await.is_none() {
                return;
            }
            let answered: Arc<Mutex<Option<InteractionResponse>>> = Arc::new(Mutex::new(None));
            let writer = Arc::clone(&answered);
            handle_voice_interaction_with(
                runtime,
                interaction,
                invite_code,
                inventory.as_ref(),
                move |response| async move {
                    *writer.lock().unwrap() = Some(response);
                },
            )
            .await;
            let response = answered.lock().unwrap().take();
            if let Some(response) = response {
                if let Err(error) = replies.respond(interaction, response).await {
                    warn!(interaction_id = interaction.id.get(), %error,
                        "voice vote response failed; not retried");
                }
            }
            return;
        }
        if let Some(VoiceCommand::Ballot { .. }) = parse_voice_command(interaction) {
            // Ballots update the public message in place (type 7). The
            // defer + PATCH path would edit each voter's own ephemeral
            // followup instead, so answer with the initial callback.
            let answered: Arc<Mutex<Option<InteractionResponse>>> = Arc::new(Mutex::new(None));
            let writer = Arc::clone(&answered);
            handle_voice_interaction_with(
                runtime,
                interaction,
                invite_code,
                inventory.as_ref(),
                move |response| async move {
                    *writer.lock().unwrap() = Some(response);
                },
            )
            .await;
            let response = answered.lock().unwrap().take();
            if let Some(response) = response {
                if let Err(error) = replies.respond(interaction, response).await {
                    warn!(interaction_id = interaction.id.get(), %error,
                        "voice ballot response failed; not retried");
                }
            }
            return;
        }
        if matches!(
            name_component_action(interaction),
            Some(NameInteraction::OpenModal { .. })
        ) {
            // A modal must be the initial callback; a deferred
            // acknowledgement cannot open one.
            let answered: Arc<Mutex<Option<InteractionResponse>>> = Arc::new(Mutex::new(None));
            let writer = Arc::clone(&answered);
            handle_voice_interaction_full(
                runtime,
                interaction,
                invite_code,
                inventory.as_ref(),
                &names,
                move |response| async move {
                    *writer.lock().unwrap() = Some(response);
                },
            )
            .await;
            let response = answered.lock().unwrap().take();
            if let Some(response) = response {
                if let Err(error) = replies.respond(interaction, response).await {
                    warn!(interaction_id = interaction.id.get(), %error,
                        "voice name modal response failed; not retried");
                }
            }
            return;
        }
        if let Err(error) = replies.defer(interaction).await {
            warn!(interaction_id = interaction.id.get(), %error,
                "voice acknowledgement failed; command not executed");
            return;
        }
        handle_voice_interaction_full(
            runtime,
            interaction,
            invite_code,
            inventory.as_ref(),
            &names,
            |response| async move {
                if let Err(error) = replies.complete(interaction, response).await {
                    warn!(interaction_id = interaction.id.get(), %error,
                        "voice response completion failed; not retried");
                }
            },
        )
        .await;
    }
}

impl<S, H, R> VoiceEventSink for VoiceResponder<S, H, R>
where
    S: RoomPersistence + 'static,
    H: RoomWrites + 'static,
    R: InteractionReplies + 'static,
{
    fn handle(&self, event: &Event, cache: &DefaultInMemoryCache) {
        self.runtime.handle(event, cache);
        if !self.runtime.enabled {
            return;
        }
        if let Event::InteractionCreate(created) = event {
            if parse_voice_command(&created.0).is_none()
                && voice_import_action(&created.0).is_none()
                && name_component_action(&created.0).is_none()
            {
                return;
            }
            let interaction = created.0.clone();
            let invite_code = vanity_code_from_cache(cache, &interaction);
            let inventory = interaction_guild(&interaction)
                .and_then(|guild_id| inventory_from_cache(cache, guild_id));
            let names = name_directory_from_cache(cache, &interaction);
            let runtime = Arc::clone(&self.runtime);
            let replies = Arc::clone(&self.replies);
            tokio::spawn(async move {
                Self::respond_named(
                    &runtime,
                    &replies,
                    &interaction,
                    invite_code.as_deref(),
                    inventory,
                    names,
                )
                .await;
            });
        }
    }

    fn disconnect(&self) {
        self.runtime.disconnect();
    }

    fn needs_bootstrap(&self, cache: &DefaultInMemoryCache) -> bool {
        self.runtime.needs_bootstrap(cache)
    }

    fn kick_claim_room(
        &self,
        guild: Snowflake,
        member: Snowflake,
    ) -> Pin<Box<dyn Future<Output = Option<Snowflake>> + Send + '_>> {
        let runtime = Arc::clone(&self.runtime);
        Box::pin(async move { runtime.kick_room_of(guild, member).await })
    }
}

#[cfg(test)]
#[path = "voice_rooms_tests.rs"]
mod tests;
