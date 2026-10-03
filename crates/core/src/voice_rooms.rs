//! Temporary voice rooms: V1 foundation (spec `docs/voice-rooms.md` §V1).
//!
//! Original implementation written from the behaviour spec only. This module
//! is framework-free: creator channels, tracked rooms, the per-guild ordered
//! action queue, the rename coalescer and the reconnect reconciler are plain
//! data plus pure functions, so lifecycle decisions can be tested without
//! Discord. These decisions alone do not execute V1: the twilight executor
//! and gateway hookup must apply them and verify live occupancy/access.
//!
//! Schema: `crates/cutover/migrations/0224_voice_rooms.sql` mirrors
//! [`CreatorChannel`] / [`VoiceRoom`] in the shared migration history.
//! [`RoomStore`] is a synchronous domain snapshot seam for tests/replay;
//! production persistence needs an asynchronous sqlx adapter.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::commands::{
    CommandChoice, CommandDefinition, CommandOption, CommandOptionType, PERM_MANAGE_CHANNELS,
    PERM_MANAGE_GUILD,
};
use super::voice_text_channel::{
    TextChannelPlan, TextChannelSettings, MAX_TEXT_CHANNEL_NAME_CHARS,
};
use crate::funnel::Snowflake;

/// Longest Discord channel name (spec "Discord API notes": 100 characters).
pub const MAX_CHANNEL_NAME_LEN: u32 = 100;
/// Largest voice user limit (`/limit`, 0 = unlimited, max 99).
pub const MAX_USER_LIMIT: i64 = 99;
/// Discord category ceiling (spec V1: hitting it errors and suggests a
/// second creator channel in another category).
pub const MAX_CHANNELS_PER_CATEGORY: usize = 50;
/// Minimum gap between two renames of the same channel: ~2 renames per
/// 10 minutes per channel (spec "Discord API notes").
pub const RENAME_MIN_INTERVAL_MS: u64 = 300_000;
/// A queued action is dead-lettered after this many failed attempts; the
/// `/setup` panel surfaces dead letters as recent failures (V10).
pub const QUEUE_MAX_ATTEMPTS: u32 = 10;

// --- creator channels -------------------------------------------------------

/// Where a new room copies its permission overrides from (V8 owns the
/// `/inheritpermissions` command; V1 only stores the choice).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionSource {
    /// Copy overrides from the creator channel (default).
    Creator,
    /// Sync to the category.
    Category,
    /// Copy overrides from one chosen channel.
    Channel(Snowflake),
}

/// Whether new rooms appear above or below their creator channel
/// (V8 owns `/position`; V1 only stores the choice).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoomPosition {
    Above,
    Below,
}

/// One creator channel row: an admin-marked voice channel whose joins spawn
/// rooms, plus its per-creator settings (spec V1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatorChannel {
    pub guild_id: Snowflake,
    pub channel_id: Snowflake,
    /// Name template for new rooms. Empty means the V5 default template;
    /// the template engine (V5/V6) owns parsing and validation.
    pub name_template: String,
    pub permission_source: PermissionSource,
    /// Required only when `permission_source` is `Channel`.
    pub permission_channel_id: Option<Snowflake>,
    /// Starting user limit override (None = inherit creator, Some(0) = unlimited).
    pub default_limit: Option<i64>,
    pub private_default: bool,
    pub text_channels: bool,
    /// Configured companion text-channel name (V9 `/textchannels`). `None`
    /// means the default companion name at plan time.
    pub text_channel_name: Option<String>,
    /// The one extra role allowed to view the companion (V9). `None` means
    /// occupants and admins only; `Some(guild_id)` is @everyone.
    pub text_viewer_role_id: Option<Snowflake>,
    pub position: RoomPosition,
    /// First room number; the numbering engine (V5) assigns the lowest free
    /// number at or above this.
    pub first_room_number: i64,
}

impl CreatorChannel {
    /// Spec defaults: creator-source permissions and limit, public, no text
    /// channel, rooms above, numbering from 1.
    #[must_use]
    pub fn new(guild_id: Snowflake, channel_id: Snowflake) -> Self {
        Self {
            guild_id,
            channel_id,
            name_template: String::new(),
            permission_source: PermissionSource::Creator,
            permission_channel_id: None,
            default_limit: None,
            private_default: false,
            text_channels: false,
            text_channel_name: None,
            text_viewer_role_id: None,
            position: RoomPosition::Above,
            first_room_number: 1,
        }
    }

    /// Check the settings before storing (`/create` and the V8/V9 commands
    /// reject through here; the SQL CHECKs mirror these bounds).
    pub fn validate(&self) -> Result<(), CreatorSettingsError> {
        if let Some(limit) = self.default_limit {
            if !(0..=MAX_USER_LIMIT).contains(&limit) {
                return Err(CreatorSettingsError::LimitOutOfRange(limit));
            }
        }
        if self.first_room_number < 1 {
            return Err(CreatorSettingsError::NumberStartOutOfRange(
                self.first_room_number,
            ));
        }
        if let PermissionSource::Channel(id) = self.permission_source {
            match self.permission_channel_id {
                None => return Err(CreatorSettingsError::MissingPermissionChannel),
                Some(stored) if stored != id => {
                    return Err(CreatorSettingsError::PermissionChannelMismatch);
                }
                Some(_) => {}
            }
        }
        if self
            .text_channel_name
            .as_ref()
            .is_some_and(|name| !is_usable_channel_name(name))
        {
            return Err(CreatorSettingsError::TextChannelNameOutOfRange);
        }
        if self.text_viewer_role_id == Some(0) {
            return Err(CreatorSettingsError::TextViewerRoleInvalid(0));
        }
        Ok(())
    }

    /// The V9 per-creator `/textchannels` settings snapshot this creator
    /// carries. Off by default; the name/viewer role ride along only when
    /// the toggle is on.
    #[must_use]
    pub fn text_channel_settings(&self) -> TextChannelSettings {
        TextChannelSettings {
            enabled: self.text_channels,
            configured_name: self.text_channel_name.clone(),
            viewer_role_id: self.text_viewer_role_id,
        }
    }
}

/// Whether a configured companion text-channel name is storable: non-blank
/// once trimmed, at most [`MAX_TEXT_CHANNEL_NAME_CHARS`] characters (mirrors
/// the `voice_creators.text_channel_name` CHECK; blank falls back to the
/// default name at plan time instead of being stored).
#[must_use]
pub fn is_usable_channel_name(name: &str) -> bool {
    let trimmed = name.trim();
    !trimmed.is_empty() && trimmed.chars().count() <= MAX_TEXT_CHANNEL_NAME_CHARS
}

/// Invalid creator settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CreatorSettingsError {
    #[error("default limit must be 0–99, got {0}")]
    LimitOutOfRange(i64),
    #[error("first room number must be >= 1, got {0}")]
    NumberStartOutOfRange(i64),
    #[error("permission source is a channel but no channel was given")]
    MissingPermissionChannel,
    #[error("permission source channel does not match permission_channel_id")]
    PermissionChannelMismatch,
    #[error("text channel name must be 1–100 non-blank characters")]
    TextChannelNameOutOfRange,
    #[error("text viewer role must be a nonzero snowflake, got {0}")]
    TextViewerRoleInvalid(Snowflake),
}

// --- tracked rooms ----------------------------------------------------------

/// One tracked temporary room. The Discord channel id is known only after
/// the create call succeeds, so joins first produce a [`NewRoomSpec`];
/// [`VoiceRoom::from_spec`] completes the row for the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceRoom {
    pub guild_id: Snowflake,
    pub channel_id: Snowflake,
    pub creator_channel_id: Snowflake,
    pub owner_id: Snowflake,
    /// The member whose join created the room. V2 ownership handoff keeps
    /// this stable while `owner_id` moves to the caretaker.
    pub original_creator_id: Snowflake,
    /// Per-room random seed stored at creation and never re-rolled (V5:
    /// `@@random_emoji@@`, `[[a/b/c]]` and named lists draw from this).
    pub name_seed: u64,
    /// ISO-millis creation stamp (funnel `now_iso` shape).
    pub created_at: String,
}

/// A room approved for creation but not yet assigned a channel id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRoomSpec {
    pub guild_id: Snowflake,
    pub creator_channel_id: Snowflake,
    pub owner_id: Snowflake,
    pub seed: u64,
    pub created_at: String,
}

/// One per-room companion text channel record (V9b): the Discord text
/// channel created alongside a room, plus the settings snapshot taken at
/// creation so later `/textchannels` changes do not retroactively alter it.
/// The `text_channels` snapshot is the creator toggle at creation time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextCompanion {
    pub guild_id: Snowflake,
    pub room_channel_id: Snowflake,
    pub text_channel_id: Snowflake,
    pub settings: TextChannelSettings,
    /// ISO-millis creation stamp (funnel `now_iso` shape).
    pub created_at: String,
}

impl TextCompanion {
    /// Complete a creation plan once Discord returns the text channel id.
    /// `now` is the creation stamp (ISO millis, caller clock).
    #[must_use]
    pub fn from_plan(plan: &TextChannelPlan, text_channel_id: Snowflake, now: String) -> Self {
        Self {
            guild_id: plan.guild_id,
            room_channel_id: plan.room_id,
            text_channel_id,
            settings: plan.settings.clone(),
            created_at: now,
        }
    }
}

impl VoiceRoom {
    /// Complete a [`NewRoomSpec`] once Discord returns the channel id. The
    /// joiner is both owner and original creator.
    #[must_use]
    pub fn from_spec(spec: NewRoomSpec, channel_id: Snowflake) -> Self {
        Self {
            guild_id: spec.guild_id,
            channel_id,
            creator_channel_id: spec.creator_channel_id,
            owner_id: spec.owner_id,
            original_creator_id: spec.owner_id,
            name_seed: spec.seed,
            created_at: spec.created_at,
        }
    }
}

/// Synchronous room snapshot seam for domain tests and replay. Production
/// sqlx persistence is asynchronous and must not block on this trait.
pub trait RoomStore: Send + Sync {
    fn add_creator(&self, creator: CreatorChannel);
    fn remove_creator(&self, guild_id: Snowflake, channel_id: Snowflake) -> bool;
    fn creators(&self, guild_id: Snowflake) -> Vec<CreatorChannel>;
    fn creator_for(&self, guild_id: Snowflake, channel_id: Snowflake) -> Option<CreatorChannel>;
    fn add_room(&self, room: VoiceRoom);
    fn remove_room(&self, guild_id: Snowflake, channel_id: Snowflake) -> Option<VoiceRoom>;
    fn room_for(&self, guild_id: Snowflake, channel_id: Snowflake) -> Option<VoiceRoom>;
    fn rooms_in_guild(&self, guild_id: Snowflake) -> Vec<VoiceRoom>;
    fn rooms_for_owner(&self, guild_id: Snowflake, owner_id: Snowflake) -> Vec<VoiceRoom>;
    fn add_companion(&self, companion: TextCompanion);
    fn remove_companion(
        &self,
        guild_id: Snowflake,
        room_channel_id: Snowflake,
    ) -> Option<TextCompanion>;
    fn companion_for(
        &self,
        guild_id: Snowflake,
        room_channel_id: Snowflake,
    ) -> Option<TextCompanion>;
}

#[derive(Debug, Default)]
struct MemRooms {
    creators: HashMap<(Snowflake, Snowflake), CreatorChannel>,
    rooms: HashMap<(Snowflake, Snowflake), VoiceRoom>,
    companions: HashMap<(Snowflake, Snowflake), TextCompanion>,
}

/// In-memory [`RoomStore`] for tests and the replay harness.
#[derive(Debug, Default)]
pub struct MemRoomStore {
    inner: Mutex<MemRooms>,
}

impl MemRoomStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl RoomStore for MemRoomStore {
    fn add_creator(&self, creator: CreatorChannel) {
        self.inner
            .lock()
            .expect("room store lock")
            .creators
            .insert((creator.guild_id, creator.channel_id), creator);
    }

    fn remove_creator(&self, guild_id: Snowflake, channel_id: Snowflake) -> bool {
        self.inner
            .lock()
            .expect("room store lock")
            .creators
            .remove(&(guild_id, channel_id))
            .is_some()
    }

    fn creators(&self, guild_id: Snowflake) -> Vec<CreatorChannel> {
        let mut out: Vec<CreatorChannel> = self
            .inner
            .lock()
            .expect("room store lock")
            .creators
            .values()
            .filter(|c| c.guild_id == guild_id)
            .cloned()
            .collect();
        out.sort_by_key(|c| c.channel_id);
        out
    }

    fn creator_for(&self, guild_id: Snowflake, channel_id: Snowflake) -> Option<CreatorChannel> {
        self.inner
            .lock()
            .expect("room store lock")
            .creators
            .get(&(guild_id, channel_id))
            .cloned()
    }

    fn add_room(&self, room: VoiceRoom) {
        self.inner
            .lock()
            .expect("room store lock")
            .rooms
            .insert((room.guild_id, room.channel_id), room);
    }

    fn remove_room(&self, guild_id: Snowflake, channel_id: Snowflake) -> Option<VoiceRoom> {
        self.inner
            .lock()
            .expect("room store lock")
            .rooms
            .remove(&(guild_id, channel_id))
    }

    fn room_for(&self, guild_id: Snowflake, channel_id: Snowflake) -> Option<VoiceRoom> {
        self.inner
            .lock()
            .expect("room store lock")
            .rooms
            .get(&(guild_id, channel_id))
            .cloned()
    }

    fn rooms_in_guild(&self, guild_id: Snowflake) -> Vec<VoiceRoom> {
        let mut out: Vec<VoiceRoom> = self
            .inner
            .lock()
            .expect("room store lock")
            .rooms
            .values()
            .filter(|r| r.guild_id == guild_id)
            .cloned()
            .collect();
        out.sort_by_key(|r| r.channel_id);
        out
    }

    fn rooms_for_owner(&self, guild_id: Snowflake, owner_id: Snowflake) -> Vec<VoiceRoom> {
        let mut out: Vec<VoiceRoom> = self
            .inner
            .lock()
            .expect("room store lock")
            .rooms
            .values()
            .filter(|r| r.guild_id == guild_id && r.owner_id == owner_id)
            .cloned()
            .collect();
        out.sort_by_key(|r| r.channel_id);
        out
    }

    fn add_companion(&self, companion: TextCompanion) {
        self.inner
            .lock()
            .expect("room store lock")
            .companions
            .insert((companion.guild_id, companion.room_channel_id), companion);
    }

    fn remove_companion(
        &self,
        guild_id: Snowflake,
        room_channel_id: Snowflake,
    ) -> Option<TextCompanion> {
        self.inner
            .lock()
            .expect("room store lock")
            .companions
            .remove(&(guild_id, room_channel_id))
    }

    fn companion_for(
        &self,
        guild_id: Snowflake,
        room_channel_id: Snowflake,
    ) -> Option<TextCompanion> {
        self.inner
            .lock()
            .expect("room store lock")
            .companions
            .get(&(guild_id, room_channel_id))
            .cloned()
    }
}

// --- lifecycle decisions ----------------------------------------------------

/// One voice join, resolved against the creator config the caller looked up.
/// Bots are included: a bot joining a creator creates a room whose zero
/// human occupancy deletes it again on the next frame — exactly one room
/// per join, then the empty rule applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomJoinRequest {
    pub guild_id: Snowflake,
    pub member_id: Snowflake,
    pub channel_id: Snowflake,
    /// `Some` when the joined channel is a creator channel.
    pub creator: Option<CreatorChannel>,
    /// Category holding the joined channel, for the 50-channel guard.
    pub category_id: Option<Snowflake>,
    /// Live channels already in that category.
    pub category_channel_count: usize,
    /// Caller-generated seed, stored on the room and never re-rolled (V5).
    pub seed: u64,
    /// Creation stamp (ISO millis, caller clock).
    pub now: String,
}

/// What one creator-channel join means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomJoinDecision {
    /// Not a creator channel: no room.
    Ignore,
    /// Create the channel with its overrides already included (spec
    /// "Discord API notes"), then move the member; the executor reports
    /// the channel id back via [`VoiceRoom::from_spec`].
    CreateRoom { spec: NewRoomSpec },
    /// The category is full: refuse with a message that suggests a second
    /// creator channel in another category (spec V1 accept).
    RefuseCategoryFull {
        category_id: Snowflake,
        message: String,
    },
}

/// Ephemeral-safe refusal for a full category (spec V1 accept: a clear
/// error suggesting a second creator channel in another category).
#[must_use]
pub fn category_full_message() -> String {
    format!(
        "That category already holds {} channels (the Discord limit), so no room \
         could be created. Ask an admin to add a second creator channel in \
         another category, then try again.",
        MAX_CHANNELS_PER_CATEGORY
    )
}

/// Decide what one voice join means. Each creator-channel join produces
/// exactly one room (spec V1 accept: two simultaneous joins get two rooms —
/// the caller emits one decision per join event, so simultaneity is safe).
#[must_use]
pub fn decide_room_join(req: RoomJoinRequest) -> RoomJoinDecision {
    let Some(creator) = req.creator else {
        return RoomJoinDecision::Ignore;
    };
    if let Some(category_id) = req.category_id {
        if req.category_channel_count >= MAX_CHANNELS_PER_CATEGORY {
            return RoomJoinDecision::RefuseCategoryFull {
                category_id,
                message: category_full_message(),
            };
        }
    }
    debug_assert_eq!(creator.guild_id, req.guild_id);
    debug_assert_eq!(creator.channel_id, req.channel_id);
    RoomJoinDecision::CreateRoom {
        spec: NewRoomSpec {
            guild_id: req.guild_id,
            creator_channel_id: req.channel_id,
            owner_id: req.member_id,
            seed: req.seed,
            created_at: req.now,
        },
    }
}

/// One voice leave, resolved against the tracked room (if any) and the
/// humans still in the channel — bots don't count (spec V1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomLeaveReport {
    pub room: Option<VoiceRoom>,
    pub remaining_humans: usize,
}

/// What one voice leave means. Occupied rooms are untouched here: V2
/// ownership handoff owns that path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomLeaveDecision {
    /// Untracked channel, or room still occupied.
    Ignore,
    /// Last human left: delete within seconds (spec V1 accept).
    DeleteRoom { room: VoiceRoom },
}

#[must_use]
pub fn decide_room_leave(report: RoomLeaveReport) -> RoomLeaveDecision {
    match report.room {
        None => RoomLeaveDecision::Ignore,
        Some(room) if report.remaining_humans == 0 => RoomLeaveDecision::DeleteRoom { room },
        Some(_) => RoomLeaveDecision::Ignore,
    }
}

// --- reconciliation ---------------------------------------------------------

/// A live channel seen at (re)connect: occupancy plus whether the bot still
/// holds View Channel, Connect, Manage Channels and Move Members on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeenChannel {
    pub channel_id: Snowflake,
    pub human_occupants: usize,
    pub manageable: bool,
}

/// What startup/reconnect reconciliation does (spec V1 accept + API notes:
/// reconcile tracked rooms against the channels that actually exist).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcilePlan {
    /// Tracked but gone (deleted by hand): quietly forget, never delete.
    pub forget: Vec<VoiceRoom>,
    /// Tracked, present, manageable, empty: delete.
    pub delete_empty: Vec<VoiceRoom>,
    /// Tracked and present but access lost: stop managing without retry
    /// storms; resume once access comes back (the row stays tracked).
    pub suspend: Vec<VoiceRoom>,
}

/// Reconcile tracked rooms against the channels that actually exist.
/// Channels the bot never tracked are never touched (spec V1 accept).
#[must_use]
pub fn reconcile(tracked: &[VoiceRoom], seen: &[SeenChannel]) -> ReconcilePlan {
    let by_id: HashMap<Snowflake, &SeenChannel> = seen.iter().map(|s| (s.channel_id, s)).collect();
    let mut plan = ReconcilePlan::default();
    for room in tracked {
        match by_id.get(&room.channel_id) {
            None => plan.forget.push(room.clone()),
            Some(live) if !live.manageable => plan.suspend.push(room.clone()),
            Some(live) if live.human_occupants == 0 => plan.delete_empty.push(room.clone()),
            Some(_) => {}
        }
    }
    plan
}

// --- rename coalescer -------------------------------------------------------

/// One pending rename per channel (spec "Discord API notes": keep one
/// pending name, coalesce updates, skip unchanged names).
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingRename {
    name: String,
}

/// What proposing a rename means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposeOutcome {
    /// Desired name already live (and nothing pending): skip the API call.
    Unchanged,
    /// A rename is now pending. `coalesced` tells whether it replaced an
    /// earlier pending name for the same channel.
    Queued { coalesced: bool },
}

/// Coalesces rapid renames into at most one pending name per channel and
/// releases them no faster than [`RENAME_MIN_INTERVAL_MS`] per channel.
/// Rename traffic never blocks room create/delete: the [`ActionQueue`]
/// drains renames on a deferred lane.
#[derive(Debug, Default)]
pub struct RenameCoalescer {
    inner: Mutex<CoalescerInner>,
}

#[derive(Debug, Default)]
struct CoalescerInner {
    pending: HashMap<Snowflake, PendingRename>,
    applied_at_ms: HashMap<Snowflake, u64>,
}

impl RenameCoalescer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Propose `desired` for `channel_id` whose live name is `current`.
    pub fn propose(
        &self,
        channel_id: Snowflake,
        current: &str,
        desired: &str,
        _now_ms: u64,
    ) -> ProposeOutcome {
        let mut inner = self.inner.lock().expect("coalescer lock");
        if desired == current {
            inner.pending.remove(&channel_id);
            return ProposeOutcome::Unchanged;
        }
        let coalesced = inner
            .pending
            .insert(
                channel_id,
                PendingRename {
                    name: desired.to_owned(),
                },
            )
            .is_some();
        ProposeOutcome::Queued { coalesced }
    }

    /// Names ready to apply now: pending, and the channel's rename budget
    /// has recovered. Drains them and stamps the budget.
    pub fn take_due(&self, now_ms: u64) -> Vec<(Snowflake, String)> {
        let mut inner = self.inner.lock().expect("coalescer lock");
        let mut due = Vec::new();
        let ready: Vec<Snowflake> = inner
            .pending
            .keys()
            .copied()
            .filter(|id| {
                inner
                    .applied_at_ms
                    .get(id)
                    .is_none_or(|at| now_ms.saturating_sub(*at) >= RENAME_MIN_INTERVAL_MS)
            })
            .collect();
        for id in ready {
            if let Some(pending) = inner.pending.remove(&id) {
                inner.applied_at_ms.insert(id, now_ms);
                due.push((id, pending.name));
            }
        }
        due.sort_by_key(|(id, _)| *id);
        due
    }

    /// Forget a deleted channel, including its pending name and budget.
    pub fn forget(&self, channel_id: Snowflake) {
        let mut inner = self.inner.lock().expect("coalescer lock");
        inner.pending.remove(&channel_id);
        inner.applied_at_ms.remove(&channel_id);
    }

    /// Pending renames held back by the budget. Diagnostics and tests.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.inner.lock().expect("coalescer lock").pending.len()
    }
}

// --- action queue -----------------------------------------------------------

/// A Discord write the executor performs. Renames are deferred; everything
/// else is urgent so rename backlogs never delay creating or deleting
/// rooms (spec "Discord API notes").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomAction {
    CreateRoom {
        creator_channel_id: Snowflake,
        owner_id: Snowflake,
        name: String,
        seed: u64,
    },
    MoveMember {
        member_id: Snowflake,
        channel_id: Snowflake,
    },
    DeleteRoom {
        channel_id: Snowflake,
    },
    RenameRoom {
        channel_id: Snowflake,
        name: String,
    },
}

/// Queue lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    Urgent,
    Deferred,
}

impl RoomAction {
    fn lane(&self) -> Lane {
        match self {
            Self::RenameRoom { .. } => Lane::Deferred,
            _ => Lane::Urgent,
        }
    }

    /// The room channel this action touches, if any (suspend/drop scope).
    #[must_use]
    pub fn channel_id(&self) -> Option<Snowflake> {
        match self {
            Self::CreateRoom { .. } => None,
            Self::MoveMember { channel_id, .. }
            | Self::DeleteRoom { channel_id }
            | Self::RenameRoom { channel_id, .. } => Some(*channel_id),
        }
    }
}

/// One enqueued write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedAction {
    pub id: u64,
    /// Unique per dispatch, so a stale callback cannot complete a later retry.
    pub dispatch_id: u64,
    pub guild_id: Snowflake,
    pub action: RoomAction,
    pub attempts: u32,
    /// Earliest retry time (millis): 429 backoff or failure backoff.
    pub not_before_ms: u64,
}

/// A write that exhausted [`QUEUE_MAX_ATTEMPTS`]; `/setup` surfaces these
/// as recent failures (V10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedAction {
    pub action: QueuedAction,
    pub reason: String,
    pub failed_at_ms: u64,
}

/// Parse a Discord `retry-after` value (decimal seconds, e.g. `1.5`) into
/// millis. Non-numeric, negative or non-finite values are `None` — the
/// caller falls back to [`fail_backoff_ms`].
#[must_use]
pub fn parse_retry_after_ms(value: &str) -> Option<u64> {
    let secs: f64 = value.trim().parse().ok()?;
    if !secs.is_finite() || secs < 0.0 {
        return None;
    }
    Some((secs * 1000.0).ceil() as u64)
}

/// Backoff after a non-429 failure: 1s doubling per attempt, capped at 60s.
#[must_use]
pub fn fail_backoff_ms(attempts: u32) -> u64 {
    1_000u64.saturating_mul(1u64 << attempts.min(6)).min(60_000)
}

#[derive(Debug, Default)]
struct QueueInner {
    next_id: u64,
    next_dispatch_id: u64,
    in_flight: HashMap<Snowflake, u64>,
    urgent: HashMap<Snowflake, VecDeque<QueuedAction>>,
    deferred: HashMap<Snowflake, VecDeque<QueuedAction>>,
    /// Per-guild 429 backoff: nothing for the guild runs before this.
    guild_not_before_ms: HashMap<Snowflake, u64>,
    /// (guild, channel) pairs with lost access: actions touching them wait.
    suspended: HashSet<(Snowflake, Snowflake)>,
    failed: Vec<FailedAction>,
}

impl QueueInner {
    fn requeue(&mut self, mut action: QueuedAction) {
        let lanes = match action.action.lane() {
            Lane::Urgent => &mut self.urgent,
            Lane::Deferred => &mut self.deferred,
        };
        let queue = lanes.entry(action.guild_id).or_default();
        if let RoomAction::RenameRoom { channel_id, .. } = &action.action {
            if let Some(index) = queue
                .iter()
                .position(|q| q.action.channel_id() == Some(*channel_id))
            {
                action.action = queue.remove(index).expect("pending rename").action;
            }
        }
        queue.push_front(action);
    }
}

/// Per-guild ordered action queue with 429 retry-after (spec "Discord API
/// notes": per-guild ordered queues, honour retry-after on 429).
///
/// Two FIFO lanes per guild: urgent (create/move/delete) drains before
/// deferred (rename). Order is preserved within each lane.
#[derive(Debug, Default)]
pub struct ActionQueue {
    inner: Mutex<QueueInner>,
}

impl ActionQueue {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueue a write; returns its id.
    pub fn enqueue(&self, guild_id: Snowflake, action: RoomAction) -> u64 {
        let mut inner = self.inner.lock().expect("queue lock");
        if let RoomAction::RenameRoom { channel_id, .. } = &action {
            if let Some(pending) = inner.deferred.get_mut(&guild_id).and_then(|queue| {
                queue
                    .iter_mut()
                    .find(|queued| queued.action.channel_id() == Some(*channel_id))
            }) {
                pending.action = action;
                return pending.id;
            }
        }
        inner.next_id += 1;
        let queued = QueuedAction {
            id: inner.next_id,
            dispatch_id: 0,
            guild_id,
            action: action.clone(),
            attempts: 0,
            not_before_ms: 0,
        };
        let lane = match action.lane() {
            Lane::Urgent => &mut inner.urgent,
            Lane::Deferred => &mut inner.deferred,
        };
        lane.entry(guild_id).or_default().push_back(queued);
        inner.next_id
    }

    /// Pop the next due write for the guild: urgent lane first, skipping
    /// backed-off guilds, failed-backoff actions and suspended channels.
    /// At most one action is in flight per guild. The executor must report
    /// every completion via `mark_succeeded`, `mark_rate_limited` or
    /// `mark_failed` before another write for that guild can begin.
    pub fn pop_due(&self, guild_id: Snowflake, now_ms: u64) -> Option<QueuedAction> {
        let mut inner = self.inner.lock().expect("queue lock");
        if inner.in_flight.contains_key(&guild_id) {
            return None;
        }
        if inner
            .guild_not_before_ms
            .get(&guild_id)
            .is_some_and(|t| now_ms < *t)
        {
            return None;
        }
        // Urgent lane (create/move/delete) drains before deferred (rename)
        // so rename backlogs never delay room lifecycle writes. Suspension
        // is snapshotted first so the scan closure holds no `inner` borrow.
        let suspended: Vec<(Snowflake, Snowflake)> = inner.suspended.iter().copied().collect();
        for urgent in [true, false] {
            let queue = if urgent {
                inner.urgent.get_mut(&guild_id)
            } else {
                inner.deferred.get_mut(&guild_id)
            };
            let Some(queue) = queue else {
                continue;
            };
            let idx = queue.iter().position(|q| {
                q.action
                    .channel_id()
                    .is_none_or(|ch| !suspended.contains(&(guild_id, ch)))
            });
            if let Some(i) = idx {
                if queue[i].not_before_ms <= now_ms {
                    let mut action = queue.remove(i)?;
                    inner.next_dispatch_id += 1;
                    action.dispatch_id = inner.next_dispatch_id;
                    inner.in_flight.insert(guild_id, action.dispatch_id);
                    return Some(action);
                }
            }
        }
        None
    }

    /// Release a successful write. Duplicate/stale completions are ignored.
    pub fn mark_succeeded(&self, action: &QueuedAction) -> bool {
        let mut inner = self.inner.lock().expect("queue lock");
        Self::release(&mut inner, action)
    }

    fn release(inner: &mut QueueInner, action: &QueuedAction) -> bool {
        if inner.in_flight.get(&action.guild_id) != Some(&action.dispatch_id) {
            return false;
        }
        inner.in_flight.remove(&action.guild_id);
        true
    }

    /// Retry a route-scoped 429. Lifecycle writes hold the guild; a rename
    /// holds only its deferred lane so it cannot delay creates/deletes.
    /// Global rate limits must additionally be honoured by the HTTP adapter.
    /// Rate limits do not consume the transient-failure budget.
    pub fn mark_rate_limited(
        &self,
        guild_id: Snowflake,
        retry_after_ms: u64,
        now_ms: u64,
        action: QueuedAction,
    ) -> bool {
        let mut inner = self.inner.lock().expect("queue lock");
        if guild_id != action.guild_id || !Self::release(&mut inner, &action) {
            return false;
        }
        let not_before_ms = now_ms.saturating_add(retry_after_ms);
        if action.action.lane() == Lane::Urgent {
            inner.guild_not_before_ms.insert(guild_id, not_before_ms);
        }
        inner.requeue(QueuedAction {
            not_before_ms,
            ..action
        });
        true
    }

    /// The executor failed without a 429: requeue at the front with
    /// backoff, or dead-letter after [`QUEUE_MAX_ATTEMPTS`].
    pub fn mark_failed(&self, action: QueuedAction, reason: String, now_ms: u64) -> bool {
        let mut inner = self.inner.lock().expect("queue lock");
        if !Self::release(&mut inner, &action) {
            return false;
        }
        let attempts = action.attempts.saturating_add(1);
        if attempts >= QUEUE_MAX_ATTEMPTS {
            inner.failed.push(FailedAction {
                action: QueuedAction { attempts, ..action },
                reason,
                failed_at_ms: now_ms,
            });
            return true;
        }
        inner.requeue(QueuedAction {
            attempts,
            not_before_ms: now_ms.saturating_add(fail_backoff_ms(attempts)),
            ..action
        });
        true
    }

    /// Access lost on a room (spec V1): its actions wait, nothing retries.
    pub fn suspend(&self, guild_id: Snowflake, channel_id: Snowflake) {
        self.inner
            .lock()
            .expect("queue lock")
            .suspended
            .insert((guild_id, channel_id));
    }

    /// Access restored: pending actions for the room may run again.
    pub fn resume(&self, guild_id: Snowflake, channel_id: Snowflake) {
        self.inner
            .lock()
            .expect("queue lock")
            .suspended
            .remove(&(guild_id, channel_id));
    }

    #[must_use]
    pub fn is_suspended(&self, guild_id: Snowflake, channel_id: Snowflake) -> bool {
        self.inner
            .lock()
            .expect("queue lock")
            .suspended
            .contains(&(guild_id, channel_id))
    }

    /// Drop every pending action touching the channel (room deleted by hand
    /// or forgotten by reconcile). Returns the dropped count.
    pub fn drop_for_channel(&self, guild_id: Snowflake, channel_id: Snowflake) -> usize {
        let mut inner = self.inner.lock().expect("queue lock");
        let mut dropped = 0;
        if let Some(queue) = inner.urgent.get_mut(&guild_id) {
            let before = queue.len();
            queue.retain(|q| q.action.channel_id() != Some(channel_id));
            dropped += before - queue.len();
        }
        if let Some(queue) = inner.deferred.get_mut(&guild_id) {
            let before = queue.len();
            queue.retain(|q| q.action.channel_id() != Some(channel_id));
            dropped += before - queue.len();
        }
        dropped
    }

    /// Pending (urgent, deferred) counts for the guild. Diagnostics/tests.
    #[must_use]
    pub fn pending_counts(&self, guild_id: Snowflake) -> (usize, usize) {
        let inner = self.inner.lock().expect("queue lock");
        let len = |m: &HashMap<Snowflake, VecDeque<QueuedAction>>| {
            m.get(&guild_id).map_or(0, VecDeque::len)
        };
        (len(&inner.urgent), len(&inner.deferred))
    }

    /// Dead-lettered writes for `/setup` recent failures (V10).
    #[must_use]
    pub fn failed(&self) -> Vec<FailedAction> {
        self.inner.lock().expect("queue lock").failed.clone()
    }

    /// Clear dead letters (after `/setup` shows them, V10).
    pub fn clear_failed(&self) {
        self.inner.lock().expect("queue lock").failed.clear();
    }
}

// --- commands and gates -----------------------------------------------------

/// V1 slash-command shapes. `/create` makes a new creator channel (admin
/// only); `/setup` is the viewable-by-anyone status panel whose actions
/// need admin (enforced by the handler, V1 runtime slice).
#[must_use]
pub fn voice_commands() -> Vec<CommandDefinition> {
    vec![
        CommandDefinition::new(
            "create",
            "Create a new creator channel for temporary voice rooms",
        )
        .permissions(PERM_MANAGE_CHANNELS)
        .options(vec![CommandOption::new(
            "name",
            "Name for the new creator channel",
            CommandOptionType::String,
        )
        .required()
        .max_length(MAX_CHANNEL_NAME_LEN)]),
        CommandDefinition::new(
            "setup",
            "Show voice-room status, health and creator channels",
        ),
        CommandDefinition::new("ping", "Show the bot's response latency"),
        CommandDefinition::new("invite", "Show this server's invite link"),
        CommandDefinition::new(
            "access",
            "Set who can create voice rooms and use room commands",
        )
        .permissions(PERM_MANAGE_CHANNELS)
        .options(vec![
            CommandOption::new(
                "show",
                "Show the current voice-room access settings",
                CommandOptionType::SubCommand,
            ),
            CommandOption::new(
                "creation",
                "Turn temporary room creation on or off",
                CommandOptionType::SubCommand,
            )
            .sub_options(vec![CommandOption::new(
                "enabled",
                "On allows new rooms, off stops them (existing rooms keep working)",
                CommandOptionType::Boolean,
            )
            .required()]),
            CommandOption::new(
                "role",
                "Set or clear the role required to use room commands",
                CommandOptionType::SubCommand,
            )
            .sub_options(vec![CommandOption::new(
                "role",
                "Required role; leave empty to clear it",
                CommandOptionType::Role,
            )]),
            CommandOption::new(
                "restrict",
                "Limit a room command to specific roles (no role = admins only)",
                CommandOptionType::SubCommand,
            )
            .sub_options(vec![
                CommandOption::new(
                    "command",
                    "The command name without the slash, e.g. kick",
                    CommandOptionType::String,
                )
                .required()
                .max_length(32),
                CommandOption::new("role", "Allowed role", CommandOptionType::Role),
                CommandOption::new("role2", "Another allowed role", CommandOptionType::Role),
                CommandOption::new("role3", "Another allowed role", CommandOptionType::Role),
            ]),
            CommandOption::new(
                "unrestrict",
                "Lift a room command's role restriction",
                CommandOptionType::SubCommand,
            )
            .sub_options(vec![CommandOption::new(
                "command",
                "The command name without the slash, e.g. kick",
                CommandOptionType::String,
            )
            .required()
            .max_length(32)]),
        ]),
        CommandDefinition::new(
            "logging",
            "Set where room health notices go and how much they say",
        )
        .permissions(PERM_MANAGE_CHANNELS)
        .options(vec![
            CommandOption::new(
                "show",
                "Show the current logging settings",
                CommandOptionType::SubCommand,
            ),
            CommandOption::new(
                "level",
                "Set how much the bot logs, or turn logging off",
                CommandOptionType::SubCommand,
            )
            .sub_options(vec![CommandOption::new(
                "level",
                "How much to log",
                CommandOptionType::String,
            )
            .required()
            .choices(vec![
                CommandChoice {
                    name: "Off".to_owned(),
                    value: "off".to_owned(),
                },
                CommandChoice {
                    name: "Brief".to_owned(),
                    value: "brief".to_owned(),
                },
                CommandChoice {
                    name: "Full".to_owned(),
                    value: "full".to_owned(),
                },
            ])]),
            CommandOption::new(
                "channel",
                "Set or clear the channel notices are sent to",
                CommandOptionType::SubCommand,
            )
            .sub_options(vec![CommandOption::new(
                "channel",
                "Notice channel; leave empty to use the automatic fallback",
                CommandOptionType::Channel,
            )]),
            CommandOption::new(
                "mention",
                "Set or clear the role mentioned on errors",
                CommandOptionType::SubCommand,
            )
            .sub_options(vec![CommandOption::new(
                "role",
                "Role to mention; leave empty to clear it",
                CommandOptionType::Role,
            )]),
        ]),
        CommandDefinition::new(
            "export",
            "Download this server's voice configuration as a versioned JSON file",
        )
        .permissions(PERM_MANAGE_GUILD),
        CommandDefinition::new(
            "import",
            "Preview a voice configuration file before applying it",
        )
        .permissions(PERM_MANAGE_GUILD)
        .options(vec![CommandOption::new(
            "file",
            "Voice configuration JSON file from /export",
            CommandOptionType::Attachment,
        )
        .required()]),
    ]
}

/// Voice env gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceGates {
    /// `TWO_VOICE=1` — publish + serve the voice commands.
    pub enabled: bool,
}

impl VoiceGates {
    /// Read gates from the process environment.
    pub fn from_env() -> Self {
        Self::from_map(&std::env::vars().collect())
    }

    /// Read gates from an explicit map (tests, staged config).
    pub fn from_map(vars: &HashMap<String, String>) -> Self {
        Self {
            enabled: vars.get("TWO_VOICE").is_some_and(|v| v == "1"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::merge_commands;
    use crate::feature_commands::feature_commands;
    use crate::funnel::now_iso;
    use crate::moderation::moderation_commands;

    const GUILD: Snowflake = 100;
    const CREATOR: Snowflake = 200;
    const MEMBER: Snowflake = 300;

    fn creator() -> CreatorChannel {
        CreatorChannel::new(GUILD, CREATOR)
    }

    fn join_req(member: Snowflake, seed: u64) -> RoomJoinRequest {
        RoomJoinRequest {
            guild_id: GUILD,
            member_id: member,
            channel_id: CREATOR,
            creator: Some(creator()),
            category_id: Some(400),
            category_channel_count: 3,
            seed,
            now: now_iso(),
        }
    }

    fn tracked(channel: Snowflake, owner: Snowflake) -> VoiceRoom {
        VoiceRoom {
            guild_id: GUILD,
            channel_id: channel,
            creator_channel_id: CREATOR,
            owner_id: owner,
            original_creator_id: owner,
            name_seed: 7,
            created_at: now_iso(),
        }
    }

    #[test]
    fn join_non_creator_is_ignored() {
        let mut req = join_req(MEMBER, 1);
        req.creator = None;
        assert_eq!(decide_room_join(req), RoomJoinDecision::Ignore);
    }

    #[test]
    fn each_join_produces_exactly_one_room() {
        // Spec V1 accept: two members joining at the same moment get two rooms.
        let a = decide_room_join(join_req(MEMBER, 1));
        let b = decide_room_join(join_req(MEMBER + 1, 2));
        let (RoomJoinDecision::CreateRoom { spec: sa }, RoomJoinDecision::CreateRoom { spec: sb }) =
            (a, b)
        else {
            panic!("both joins must create rooms");
        };
        assert_eq!(sa.owner_id, MEMBER);
        assert_eq!(sb.owner_id, MEMBER + 1);
        assert_ne!(sa.seed, sb.seed);
        // Completing the specs yields distinct tracked rooms.
        let ra = VoiceRoom::from_spec(sa, 500);
        let rb = VoiceRoom::from_spec(sb, 501);
        assert_eq!(ra.owner_id, ra.original_creator_id);
        assert_ne!(ra.channel_id, rb.channel_id);
    }

    #[test]
    fn full_category_refuses_with_second_creator_hint() {
        let mut req = join_req(MEMBER, 1);
        req.category_channel_count = MAX_CHANNELS_PER_CATEGORY;
        let decision = decide_room_join(req);
        let RoomJoinDecision::RefuseCategoryFull {
            category_id,
            message,
        } = decision
        else {
            panic!("full category must refuse");
        };
        assert_eq!(category_id, 400);
        assert!(message.contains("second creator channel in another category"));
    }

    #[test]
    fn leave_rules_follow_human_occupancy() {
        // Untracked channel: never touch.
        assert_eq!(
            decide_room_leave(RoomLeaveReport {
                room: None,
                remaining_humans: 0,
            }),
            RoomLeaveDecision::Ignore
        );
        // Occupied room (bots don't count, but one human is still there).
        assert_eq!(
            decide_room_leave(RoomLeaveReport {
                room: Some(tracked(500, MEMBER)),
                remaining_humans: 1,
            }),
            RoomLeaveDecision::Ignore
        );
        // Last human left: delete within seconds.
        assert_eq!(
            decide_room_leave(RoomLeaveReport {
                room: Some(tracked(500, MEMBER)),
                remaining_humans: 0,
            }),
            RoomLeaveDecision::DeleteRoom {
                room: tracked(500, MEMBER)
            }
        );
    }

    #[test]
    fn reconcile_forgets_deletes_and_suspends() {
        let gone = tracked(501, MEMBER);
        let empty = tracked(502, MEMBER);
        let locked = tracked(503, MEMBER);
        let lived_in = tracked(504, MEMBER);
        let plan = reconcile(
            &[
                gone.clone(),
                empty.clone(),
                locked.clone(),
                lived_in.clone(),
            ],
            &[
                SeenChannel {
                    channel_id: 502,
                    human_occupants: 0,
                    manageable: true,
                },
                SeenChannel {
                    channel_id: 503,
                    human_occupants: 2,
                    manageable: false,
                },
                SeenChannel {
                    channel_id: 504,
                    human_occupants: 2,
                    manageable: true,
                },
                // Never tracked: must never appear in the plan.
                SeenChannel {
                    channel_id: 999,
                    human_occupants: 0,
                    manageable: true,
                },
            ],
        );
        assert_eq!(plan.forget, vec![gone]);
        assert_eq!(plan.delete_empty, vec![empty]);
        assert_eq!(plan.suspend, vec![locked]);
        let mentioned: Vec<Snowflake> = plan
            .forget
            .iter()
            .chain(plan.delete_empty.iter())
            .chain(plan.suspend.iter())
            .map(|r| r.channel_id)
            .collect();
        assert!(!mentioned.contains(&999));
        assert!(!mentioned.contains(&504));
    }

    #[test]
    fn coalescer_skips_unchanged_and_coalesces() {
        let c = RenameCoalescer::new();
        assert_eq!(c.propose(1, "Room", "Room", 0), ProposeOutcome::Unchanged);
        assert_eq!(
            c.propose(1, "Room", "Apex", 0),
            ProposeOutcome::Queued { coalesced: false }
        );
        assert_eq!(
            c.propose(1, "Room", "Apex Legends", 1_000),
            ProposeOutcome::Queued { coalesced: true }
        );
        // Budget spent only when a rename is released, not when proposed.
        assert_eq!(
            c.take_due(2_000).as_slice(),
            &[(1, "Apex Legends".to_owned())]
        );
        assert_eq!(c.pending_count(), 0);
        // Second rename inside the window stays pending.
        assert_eq!(
            c.propose(1, "Apex Legends", "Valorant", 3_000),
            ProposeOutcome::Queued { coalesced: false }
        );
        assert!(c.take_due(4_000).is_empty());
        // After ~2 per 10 minutes the budget recovers.
        assert_eq!(
            c.take_due(2_000 + RENAME_MIN_INTERVAL_MS).as_slice(),
            &[(1, "Valorant".to_owned())]
        );
    }

    #[test]
    fn queue_is_ordered_per_guild_with_deferred_renames() {
        let q = ActionQueue::new();
        q.enqueue(
            GUILD,
            RoomAction::RenameRoom {
                channel_id: 500,
                name: "slow".to_owned(),
            },
        );
        q.enqueue(GUILD, RoomAction::DeleteRoom { channel_id: 501 });
        q.enqueue(GUILD + 1, RoomAction::DeleteRoom { channel_id: 502 });
        // Urgent delete jumps the earlier rename; other guilds are isolated.
        assert_eq!(
            q.pop_due(GUILD, 0).map(|a| {
                q.mark_succeeded(&a);
                a.action
            }),
            Some(RoomAction::DeleteRoom { channel_id: 501 })
        );
        assert_eq!(
            q.pop_due(GUILD, 0).map(|a| {
                q.mark_succeeded(&a);
                a.action
            }),
            Some(RoomAction::RenameRoom {
                channel_id: 500,
                name: "slow".to_owned(),
            })
        );
        assert_eq!(
            q.pop_due(GUILD + 1, 0).map(|a| {
                q.mark_succeeded(&a);
                a.action
            }),
            Some(RoomAction::DeleteRoom { channel_id: 502 })
        );
        assert_eq!(q.pop_due(GUILD, 0), None);
    }

    #[test]
    fn queue_honours_429_retry_after() {
        let q = ActionQueue::new();
        q.enqueue(GUILD, RoomAction::DeleteRoom { channel_id: 500 });
        let action = q.pop_due(GUILD, 0).expect("due");
        assert_eq!(parse_retry_after_ms("1.5"), Some(1500));
        assert_eq!(parse_retry_after_ms("0"), Some(0));
        assert_eq!(parse_retry_after_ms("nope"), None);
        assert_eq!(parse_retry_after_ms("-2"), None);
        q.mark_rate_limited(GUILD, 1500, 0, action);
        assert_eq!(q.pop_due(GUILD, 1499), None);
        let again = q.pop_due(GUILD, 1500).expect("backoff elapsed");
        assert_eq!(again.attempts, 0);
    }

    #[test]
    fn queue_suspends_without_retry_storms_and_drops_forgotten_rooms() {
        let q = ActionQueue::new();
        q.enqueue(
            GUILD,
            RoomAction::RenameRoom {
                channel_id: 500,
                name: "x".to_owned(),
            },
        );
        q.enqueue(GUILD, RoomAction::DeleteRoom { channel_id: 501 });
        q.suspend(GUILD, 501);
        assert!(q.is_suspended(GUILD, 501));
        // The suspended delete waits; the rename still runs.
        assert_eq!(
            q.pop_due(GUILD, 0).map(|a| {
                q.mark_succeeded(&a);
                a.action
            }),
            Some(RoomAction::RenameRoom {
                channel_id: 500,
                name: "x".to_owned(),
            })
        );
        assert_eq!(q.pop_due(GUILD, 0), None);
        q.resume(GUILD, 501);
        assert_eq!(
            q.pop_due(GUILD, 0).map(|a| {
                q.mark_succeeded(&a);
                a.action
            }),
            Some(RoomAction::DeleteRoom { channel_id: 501 })
        );
        // Forgetting a room drops its pending writes.
        q.enqueue(
            GUILD,
            RoomAction::RenameRoom {
                channel_id: 502,
                name: "y".to_owned(),
            },
        );
        assert_eq!(q.drop_for_channel(GUILD, 502), 1);
        assert_eq!(q.pending_counts(GUILD), (0, 0));
    }

    #[test]
    fn queue_dead_letters_after_max_attempts() {
        let q = ActionQueue::new();
        q.enqueue(GUILD, RoomAction::DeleteRoom { channel_id: 500 });
        let mut action = q.pop_due(GUILD, 0).expect("due");
        for _ in 0..QUEUE_MAX_ATTEMPTS {
            let now = action.not_before_ms;
            q.mark_failed(action.clone(), "boom".to_owned(), now);
            if let Some(next) = q.pop_due(GUILD, u64::MAX) {
                action = next;
            } else {
                break;
            }
        }
        let failed = q.failed();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].reason, "boom");
        assert_eq!(failed[0].action.attempts, QUEUE_MAX_ATTEMPTS);
        assert!(fail_backoff_ms(0) <= fail_backoff_ms(3));
        assert_eq!(fail_backoff_ms(100), 60_000);
        q.clear_failed();
        assert!(q.failed().is_empty());
    }

    #[test]
    fn mem_store_round_trips_creators_and_rooms() {
        let store = MemRoomStore::new();
        let mut c = creator();
        c.default_limit = Some(4);
        store.add_creator(c.clone());
        assert_eq!(store.creators(GUILD), vec![c.clone()]);
        assert_eq!(store.creator_for(GUILD, CREATOR), Some(c));
        assert_eq!(store.creator_for(GUILD, 999), None);

        let room = tracked(500, MEMBER);
        store.add_room(room.clone());
        assert_eq!(store.room_for(GUILD, 500), Some(room.clone()));
        assert_eq!(store.rooms_in_guild(GUILD), vec![room.clone()]);
        assert_eq!(store.rooms_for_owner(GUILD, MEMBER), vec![room.clone()]);
        assert!(store.rooms_for_owner(GUILD, MEMBER + 1).is_empty());

        assert_eq!(store.remove_room(GUILD, 500), Some(room));
        assert!(store.remove_creator(GUILD, CREATOR));
        assert!(!store.remove_creator(GUILD, CREATOR));
        assert!(store.creators(GUILD).is_empty());
    }

    #[test]
    fn creator_settings_validate() {
        assert!(creator().validate().is_ok());
        let mut bad = creator();
        bad.default_limit = Some(100);
        assert_eq!(
            bad.validate(),
            Err(CreatorSettingsError::LimitOutOfRange(100))
        );
        let mut bad = creator();
        bad.first_room_number = 0;
        assert_eq!(
            bad.validate(),
            Err(CreatorSettingsError::NumberStartOutOfRange(0))
        );
        let mut bad = creator();
        bad.permission_source = PermissionSource::Channel(CREATOR);
        bad.permission_channel_id = None;
        assert_eq!(
            bad.validate(),
            Err(CreatorSettingsError::MissingPermissionChannel)
        );
    }

    #[test]
    fn cancelling_a_pending_rename_skips_the_write() {
        let c = RenameCoalescer::new();
        c.propose(500, "Room", "Changed", 0);
        assert_eq!(c.propose(500, "Room", "Room", 1), ProposeOutcome::Unchanged);
        assert_eq!(c.pending_count(), 0);
        assert!(c.take_due(1).is_empty());
    }

    #[test]
    fn retry_after_never_rounds_down() {
        assert_eq!(parse_retry_after_ms("0.0001"), Some(1));
        assert_eq!(parse_retry_after_ms("1.5001"), Some(1501));
        assert_eq!(parse_retry_after_ms("NaN"), None);
        assert_eq!(parse_retry_after_ms("inf"), None);
    }

    #[test]
    fn move_actions_are_suspended_and_dropped_with_their_target() {
        let q = ActionQueue::new();
        q.enqueue(
            GUILD,
            RoomAction::MoveMember {
                member_id: MEMBER,
                channel_id: 500,
            },
        );
        q.suspend(GUILD, 500);
        assert_eq!(q.pop_due(GUILD, 0), None);
        assert_eq!(q.drop_for_channel(GUILD, 500), 1);
        assert_eq!(q.pending_counts(GUILD), (0, 0));
    }

    #[test]
    fn backoff_preserves_lifecycle_order() {
        let q = ActionQueue::new();
        let first = q.enqueue(GUILD, RoomAction::DeleteRoom { channel_id: 500 });
        q.enqueue(GUILD, RoomAction::DeleteRoom { channel_id: 501 });
        let action = q.pop_due(GUILD, 0).expect("first action");
        assert_eq!(action.id, first);
        q.mark_failed(action, "transient".to_owned(), 0);
        assert_eq!(q.pop_due(GUILD, 1), None);
        assert_eq!(q.pop_due(GUILD, 2_000).expect("retry first").id, first);
    }

    #[test]
    fn selected_permission_channel_must_match_the_stored_id() {
        let mut c = creator();
        c.permission_source = PermissionSource::Channel(999);
        assert_eq!(
            c.validate(),
            Err(CreatorSettingsError::MissingPermissionChannel)
        );
        c.permission_channel_id = Some(998);
        assert!(c.validate().is_err());
        c.permission_channel_id = Some(999);
        assert!(c.validate().is_ok());
    }

    #[test]
    fn text_channel_name_bounds_match_the_sql_check() {
        // `None` (default) is always fine, and a normal name passes.
        assert!(creator().validate().is_ok());
        let mut named = creator();
        named.text_channel_name = Some("Lounge".to_owned());
        assert!(named.validate().is_ok());
        // Blank (only whitespace) and over-100-character names are refused
        // here, never reaching the database CHECK.
        for bad in ["", "   ", &"a".repeat(101)] {
            let mut c = creator();
            c.text_channel_name = Some(bad.to_owned());
            assert_eq!(
                c.validate(),
                Err(CreatorSettingsError::TextChannelNameOutOfRange)
            );
        }
        // Exactly 100 characters is the SQL boundary: accepted.
        let mut edge = creator();
        edge.text_channel_name = Some("a".repeat(100));
        assert!(edge.validate().is_ok());
        assert!(is_usable_channel_name("Lounge"));
        assert!(!is_usable_channel_name("   "));
        assert!(!is_usable_channel_name(&"a".repeat(101)));
    }

    #[test]
    fn text_viewer_role_must_be_nonzero() {
        let mut everyone = creator();
        everyone.text_viewer_role_id = Some(GUILD);
        assert!(everyone.validate().is_ok());
        let mut zero = creator();
        zero.text_viewer_role_id = Some(0);
        assert_eq!(
            zero.validate(),
            Err(CreatorSettingsError::TextViewerRoleInvalid(0))
        );
    }

    #[test]
    fn text_channel_settings_default_off_and_snapshot_shape() {
        use crate::voice_text_channel::TextChannelSettings;
        assert_eq!(
            creator().text_channel_settings(),
            TextChannelSettings::default()
        );
        assert!(!creator().text_channel_settings().enabled);
        let mut on = creator();
        on.text_channels = true;
        on.text_channel_name = Some("Lounge".to_owned());
        on.text_viewer_role_id = Some(42);
        assert_eq!(
            on.text_channel_settings(),
            TextChannelSettings {
                enabled: true,
                configured_name: Some("Lounge".to_owned()),
                viewer_role_id: Some(42),
            }
        );
    }

    #[test]
    fn mem_store_round_trips_companion_records() {
        use crate::voice_text_channel::TextChannelSettings;
        let store = MemRoomStore::new();
        let companion = TextCompanion {
            guild_id: GUILD,
            room_channel_id: 500,
            text_channel_id: 600,
            settings: TextChannelSettings {
                enabled: true,
                configured_name: Some("Lounge".to_owned()),
                viewer_role_id: Some(GUILD),
            },
            created_at: crate::funnel::now_iso(),
        };
        assert_eq!(store.companion_for(GUILD, 500), None);
        store.add_companion(companion.clone());
        assert_eq!(store.companion_for(GUILD, 500), Some(companion.clone()));
        assert_eq!(store.companion_for(GUILD + 1, 500), None);
        assert_eq!(store.remove_companion(GUILD, 500), Some(companion));
        assert_eq!(store.remove_companion(GUILD, 500), None);
    }

    #[test]
    fn one_in_flight_write_per_guild_and_stale_callbacks_are_ignored() {
        let q = ActionQueue::new();
        q.enqueue(GUILD, RoomAction::DeleteRoom { channel_id: 500 });
        q.enqueue(GUILD, RoomAction::DeleteRoom { channel_id: 501 });
        q.enqueue(GUILD + 1, RoomAction::DeleteRoom { channel_id: 502 });
        let first = q.pop_due(GUILD, 0).expect("first");
        assert_eq!(q.pop_due(GUILD, 0), None);
        let other = q.pop_due(GUILD + 1, 0).expect("independent guild");
        assert!(q.mark_succeeded(&other));
        assert!(q.mark_rate_limited(GUILD, 1, 0, first.clone()));
        let retry = q.pop_due(GUILD, 1).expect("retry");
        assert_eq!(retry.id, first.id);
        assert_ne!(retry.dispatch_id, first.dispatch_id);
        assert!(!q.mark_succeeded(&first));
        assert!(!q.mark_failed(first, "stale".to_owned(), 1));
        assert_eq!(q.pop_due(GUILD, 1), None);
        assert!(q.mark_succeeded(&retry));
        assert!(!q.mark_succeeded(&retry));
        assert!(q.pop_due(GUILD, 1).is_some());
    }

    #[test]
    fn queued_renames_coalesce_and_their_backoff_does_not_block_lifecycle() {
        let q = ActionQueue::new();
        let id = q.enqueue(
            GUILD,
            RoomAction::RenameRoom {
                channel_id: 500,
                name: "Old".to_owned(),
            },
        );
        assert_eq!(
            q.enqueue(
                GUILD,
                RoomAction::RenameRoom {
                    channel_id: 500,
                    name: "Latest".to_owned()
                }
            ),
            id
        );
        assert_eq!(q.pending_counts(GUILD), (0, 1));
        let rename = q.pop_due(GUILD, 0).expect("rename");
        assert_eq!(
            rename.action,
            RoomAction::RenameRoom {
                channel_id: 500,
                name: "Latest".to_owned()
            }
        );
        q.mark_failed(rename, "transient".to_owned(), 0);
        q.enqueue(GUILD, RoomAction::DeleteRoom { channel_id: 501 });
        let delete = q.pop_due(GUILD, 1).expect("urgent delete");
        assert_eq!(delete.action, RoomAction::DeleteRoom { channel_id: 501 });
        q.mark_succeeded(&delete);
        assert_eq!(q.pop_due(GUILD, 1), None);
    }

    #[test]
    fn rate_limited_in_flight_rename_keeps_latest_name_and_allows_deletion() {
        let q = ActionQueue::new();
        q.enqueue(
            GUILD,
            RoomAction::RenameRoom {
                channel_id: 500,
                name: "Old".to_owned(),
            },
        );
        let old = q.pop_due(GUILD, 0).expect("old rename");
        q.enqueue(
            GUILD,
            RoomAction::RenameRoom {
                channel_id: 500,
                name: "Latest".to_owned(),
            },
        );
        q.mark_rate_limited(GUILD, 600_000, 0, old);
        assert_eq!(q.pending_counts(GUILD), (0, 1));
        q.enqueue(GUILD, RoomAction::DeleteRoom { channel_id: 501 });
        let delete = q.pop_due(GUILD, 1).expect("rename cannot delay delete");
        assert_eq!(delete.action, RoomAction::DeleteRoom { channel_id: 501 });
        q.mark_succeeded(&delete);
        assert_eq!(q.pop_due(GUILD, 599_999), None);
        let retry = q.pop_due(GUILD, 600_000).expect("rename retry");
        assert_eq!(
            retry.action,
            RoomAction::RenameRoom {
                channel_id: 500,
                name: "Latest".to_owned()
            }
        );
    }

    #[test]
    fn voice_command_shapes_and_gates() {
        let defs = voice_commands();
        assert_eq!(
            defs.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
            ["create", "setup", "ping", "invite", "access", "logging", "export", "import"]
        );
        // `/create` is admin-gated (Manage Channels) with a required name.
        assert_eq!(
            defs[0].default_member_permissions,
            Some(PERM_MANAGE_CHANNELS.to_string())
        );
        assert_eq!(defs[0].options.len(), 1);
        assert_eq!(defs[0].options[0].name, "name");
        assert!(defs[0].options[0].required == Some(true));
        // `/setup` is viewable by anyone; handler actions need admin.
        assert_eq!(defs[1].default_member_permissions, None);
        assert!(defs[1].options.is_empty());
        // `/ping` and `/invite` are open to everyone and take no options.
        for def in &defs[2..4] {
            assert_eq!(def.default_member_permissions, None);
            assert!(def.options.is_empty());
        }
        // `/access` is admin-gated and is all sub-commands, each with its
        // required options listed before the optional ones.
        let access = &defs[4];
        let logging = &defs[5];
        assert_eq!(
            access.default_member_permissions,
            Some(PERM_MANAGE_CHANNELS.to_string())
        );
        assert_eq!(
            access
                .options
                .iter()
                .map(|o| o.name.as_str())
                .collect::<Vec<_>>(),
            ["show", "creation", "role", "restrict", "unrestrict"]
        );
        assert_eq!(
            logging.default_member_permissions,
            Some(PERM_MANAGE_CHANNELS.to_string())
        );
        assert_eq!(
            logging
                .options
                .iter()
                .map(|o| o.name.as_str())
                .collect::<Vec<_>>(),
            ["show", "level", "channel", "mention"]
        );
        for sub in access.options.iter().chain(&logging.options) {
            assert_eq!(sub.kind, CommandOptionType::SubCommand.as_u8());
            let required_first = sub
                .options
                .iter()
                .skip_while(|o| o.required == Some(true))
                .all(|o| o.required != Some(true));
            assert!(required_first, "{} lists a required option late", sub.name);
        }
        // `/export` takes no options; `/import` takes one required file
        // attachment. Both are Manage Server (Manage Guild) gated.
        let export = &defs[6];
        let import = &defs[7];
        for def in [export, import] {
            assert_eq!(
                def.default_member_permissions,
                Some(PERM_MANAGE_GUILD.to_string())
            );
        }
        assert!(export.options.is_empty());
        assert_eq!(import.options.len(), 1);
        assert_eq!(import.options[0].name, "file");
        assert_eq!(
            import.options[0].kind,
            CommandOptionType::Attachment.as_u8()
        );
        assert!(import.options[0].required == Some(true));
        // Merges cleanly alongside the other slices, first-wins.
        let merged = merge_commands(
            &[feature_commands(), moderation_commands(), voice_commands()],
            &[],
        )
        .expect("voice merges cleanly");
        assert!(merged.iter().any(|d| d.name == "create"));
        assert!(merged.iter().any(|d| d.name == "setup"));
        assert!(merged.iter().any(|d| d.name == "ping"));
        assert!(merged.iter().any(|d| d.name == "invite"));

        assert!(!VoiceGates::from_map(&Default::default()).enabled);
        let vars: HashMap<String, String> = [("TWO_VOICE".to_owned(), "1".to_owned())]
            .into_iter()
            .collect();
        assert!(VoiceGates::from_map(&vars).enabled);
    }
}
