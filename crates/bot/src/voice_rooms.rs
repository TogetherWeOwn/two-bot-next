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
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, RwLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use tokio::sync::{mpsc, oneshot};
use tracing::warn;
use twilight_cache_inmemory::DefaultInMemoryCache;
use twilight_gateway::Event;
use twilight_model::{
    application::interaction::{
        application_command::CommandOptionValue, Interaction, InteractionData, InteractionType,
    },
    channel::{message::MessageFlags, permission_overwrite::PermissionOverwrite, Channel},
    guild::{Permissions, Role},
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    id::{marker::RoleMarker, Id},
};
use two_bot_core::{
    now_iso,
    voice_rooms::{
        category_full_message, voice_commands, ActionQueue, CreatorChannel, NewRoomSpec,
        PermissionSource, ProposeOutcome, QueuedAction, RenameCoalescer, RoomAction, RoomPosition,
        VoiceGates, VoiceRoom, MAX_CHANNELS_PER_CATEGORY, MAX_CHANNEL_NAME_LEN,
        RENAME_MIN_INTERVAL_MS,
    },
    CommandDefinition, Snowflake,
};
use two_bot_cutover::voice_rooms::PgRoomStore;
use two_bot_discord::voice_rooms::{
    can_manage_room, effective_permissions, RoomChannelAttributes, RoomHttp, RoomHttpError,
};

pub type WriteGuard = Arc<dyn Fn() -> bool + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    Unavailable,
    CredentialRefused,
    Conflict,
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
    fn persist(&self, room: &VoiceRoom) -> impl Future<Output = Result<(), StoreError>> + Send;
    fn forget(
        &self,
        guild: Snowflake,
        channel: Snowflake,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
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

    async fn forget(&self, guild: Snowflake, channel: Snowflake) -> Result<(), StoreError> {
        self.remove_room(guild, channel)
            .await
            .map_err(store_error)?;
        Ok(())
    }
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

    async fn delete(&self, channel: Snowflake, guard: WriteGuard) -> Result<(), RoomHttpError> {
        self.delete_room(channel, move || guard()).await
    }

    async fn rename(&self, channel: Snowflake, name: &str) -> Result<(), RoomHttpError> {
        self.rename_room(channel, name).await
    }
}

#[derive(Debug, Clone)]
pub struct BotAccess {
    pub member_id: Snowflake,
    pub guild_owner_id: Snowflake,
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

    fn humans(&self, channel: Snowflake) -> usize {
        self.members
            .values()
            .filter(|member| member.channel_id == Some(channel) && member.bot != Some(true))
            .count()
    }
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

    /// Refresh the bot access snapshot after role changes. Generation is
    /// unchanged: role edits do not invalidate in-flight tickets, they only
    /// affect the next guard evaluation.
    pub fn refresh_bot(&self, access: BotAccess) {
        self.inner.write().expect("live voice lock").bot = Some(access);
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

    fn join_guard(&self, ticket: JoinTicket) -> WriteGuard {
        let live = self.clone();
        Arc::new(move || {
            let state = live.inner.read().expect("live voice lock");
            state.ticket_valid(ticket)
                && can_manage_room(state.permissions(live.guild_id, ticket.creator_id))
        })
    }

    fn move_guard(&self, ticket: JoinTicket, channel: Snowflake) -> WriteGuard {
        let live = self.clone();
        let join = self.join_guard(ticket);
        Arc::new(move || {
            join()
                && can_manage_room(
                    live.inner
                        .read()
                        .expect("live voice lock")
                        .permissions(live.guild_id, channel),
                )
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleFailure {
    CategoryFull {
        creator_id: Snowflake,
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
}

#[derive(Debug, Clone)]
struct Creation {
    ticket: JoinTicket,
    spec: NewRoomSpec,
}

/// One mutable worker per guild. `load` must succeed before use. `reconcile`
/// responds to complete snapshots, channel/role changes and voice transitions;
/// `dispatch_one` is driven by the actor's timer, not by the gateway itself.
pub struct GuildRoomWorker<S, H> {
    live: LiveGuild,
    store: S,
    http: H,
    creators: HashMap<Snowflake, CreatorChannel>,
    rooms: HashMap<Snowflake, VoiceRoom>,
    queue: ActionQueue,
    renames: RenameCoalescer,
    desired_names: HashMap<Snowflake, String>,
    creations: HashMap<u64, Creation>,
    accepted: HashMap<Snowflake, (u64, u64)>,
    moves: HashMap<Snowflake, JoinTicket>,
    uncertain_moves: HashMap<Snowflake, JoinTicket>,
    deletes: HashSet<Snowflake>,
    compensation: HashSet<Snowflake>,
    denied: HashMap<Snowflake, (u64, Option<Permissions>)>,
    failures: VecDeque<LifecycleFailure>,
    halted: bool,
}

impl<S: RoomPersistence, H: RoomWrites> GuildRoomWorker<S, H> {
    pub async fn load(live: LiveGuild, store: S, http: H) -> Result<Self, StoreError> {
        let creators = store
            .creators(live.guild_id)
            .await?
            .into_iter()
            .map(|c| (c.channel_id, c))
            .collect();
        let rooms = store
            .rooms(live.guild_id)
            .await?
            .into_iter()
            .map(|r| (r.channel_id, r))
            .collect();
        Ok(Self {
            live,
            store,
            http,
            creators,
            rooms,
            queue: ActionQueue::new(),
            renames: RenameCoalescer::new(),
            desired_names: HashMap::new(),
            creations: HashMap::new(),
            accepted: HashMap::new(),
            moves: HashMap::new(),
            uncertain_moves: HashMap::new(),
            deletes: HashSet::new(),
            compensation: HashSet::new(),
            denied: HashMap::new(),
            failures: VecDeque::new(),
            halted: false,
        })
    }

    pub fn accept_join(
        &mut self,
        ticket: JoinTicket,
        name: String,
        seed: u64,
        created_at: String,
    ) -> bool {
        if self.halted
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
            },
        );
        true
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

    pub fn reconcile(&mut self) {
        if self.halted {
            return;
        }
        let live = self.live.inner.read().expect("live voice lock");
        if !live.ready {
            return;
        }
        let mut empty = Vec::new();
        for channel in self.rooms.keys().copied() {
            if !live.channels.contains_key(&channel) {
                self.queue.resume(self.live.guild_id, channel);
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
                continue;
            }
            self.queue.resume(self.live.guild_id, channel);
            if live.humans(channel) == 0
                && !self.moves.contains_key(&channel)
                && !self
                    .uncertain_moves
                    .get(&channel)
                    .is_some_and(|ticket| live.ticket_valid(*ticket))
            {
                empty.push(channel);
            }
        }
        drop(live);
        for channel in empty {
            self.queue_delete(channel, false);
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

    pub fn failures(&self) -> &VecDeque<LifecycleFailure> {
        &self.failures
    }
    pub fn halted(&self) -> bool {
        self.halted
    }
    pub fn tracked(&self) -> &HashMap<Snowflake, VoiceRoom> {
        &self.rooms
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

    fn prepare(&self, ticket: JoinTicket) -> Result<RoomChannelAttributes, RoomHttpError> {
        let live = self.live.inner.read().expect("live voice lock");
        if !live.ticket_valid(ticket) {
            return Err(RoomHttpError::Cancelled);
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
            return Err(RoomHttpError::AccessDenied);
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
                });
            }
        }
        let source = if permissions.is_some_and(|p| p.contains(Permissions::MANAGE_ROLES)) {
            match settings.permission_source {
                PermissionSource::Creator => Some(ticket.creator_id),
                PermissionSource::Category => channel.parent_id.map(Id::get),
                PermissionSource::Channel(id) => Some(id),
            }
        } else {
            channel.parent_id.map(Id::get)
        };
        let mut overwrites: Vec<PermissionOverwrite> = match source {
            Some(id) => live
                .channels
                .get(&id)
                .ok_or(RoomHttpError::AccessDenied)?
                .permission_overwrites
                .clone()
                .unwrap_or_default(),
            None => Vec::new(),
        };
        // Privacy/default owner permissions are applied by V8. Never silently
        // create a public room if a later slice has enabled an unsupported default.
        if settings.private_default || settings.text_channels {
            return Err(RoomHttpError::InvalidRequest);
        }
        let bot = live.bot.as_ref().ok_or(RoomHttpError::AccessDenied)?;
        if !can_manage_room(effective_permissions(
            self.live.guild_id,
            bot.guild_owner_id,
            bot.member_id,
            &bot.member_roles,
            &bot.roles,
            &overwrites,
        )) {
            return Err(RoomHttpError::AccessDenied);
        }
        RoomChannelAttributes::from_creator(settings, channel, std::mem::take(&mut overwrites))
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
                    Err(error) => {
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
                        } else if error != RoomHttpError::Cancelled {
                            self.record(LifecycleFailure::Discord {
                                channel_id: creator_channel_id,
                                error,
                            });
                        }
                        self.creations.remove(&action.id);
                        self.queue.mark_succeeded(&action);
                        return true;
                    }
                };
                match self
                    .http
                    .create(
                        self.live.guild_id,
                        &name,
                        &attributes,
                        self.live.join_guard(creation.ticket),
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
                        match self.store.persist(&room).await {
                            Ok(()) => {
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
                                self.record(LifecycleFailure::Persistence {
                                    channel_id: Some(channel_id),
                                    error,
                                });
                                if error == StoreError::CredentialRefused {
                                    self.halted = true;
                                }
                                self.queue_delete(channel_id, true);
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
                        // Unknown create outcomes must never produce another POST.
                        self.creations.remove(&action.id);
                        self.complete_error(action, creator_channel_id, error);
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
                let result = if !can_manage_room(
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
                            self.live.move_guard(ticket, channel_id),
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
                        self.complete_error(action, channel_id, RoomHttpError::UnknownOutcome);
                    }
                    Err(error) => {
                        self.moves.remove(&channel_id);
                        self.complete_error(action, channel_id, error);
                        self.queue_delete(channel_id, true);
                    }
                }
            }
            RoomAction::DeleteRoom { channel_id } => {
                if !self.rooms.contains_key(&channel_id) {
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
                        match self.store.forget(self.live.guild_id, channel_id).await {
                            Ok(()) => {
                                self.queue.mark_succeeded(&action);
                                self.queue.drop_for_channel(self.live.guild_id, channel_id);
                                self.rooms.remove(&channel_id);
                                self.deletes.remove(&channel_id);
                                self.compensation.remove(&channel_id);
                                self.denied.remove(&channel_id);
                                self.renames.forget(channel_id);
                                self.moves.remove(&channel_id);
                                self.uncertain_moves.remove(&channel_id);
                                self.desired_names.remove(&channel_id);
                            }
                            Err(error) => {
                                self.record(LifecycleFailure::Persistence {
                                    channel_id: Some(channel_id),
                                    error,
                                });
                                if error == StoreError::CredentialRefused {
                                    self.halted = true;
                                    self.queue.mark_succeeded(&action);
                                } else {
                                    self.queue.mark_failed(
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
                        self.queue.mark_failed(
                            action,
                            "Discord access denied".to_owned(),
                            elapsed_ms(now_ms, started),
                        );
                    }
                    Err(RoomHttpError::Cancelled) => {
                        self.queue.mark_succeeded(&action);
                        self.deletes.remove(&channel_id);
                    }
                    Err(RoomHttpError::UnknownOutcome) => {
                        self.queue.mark_failed(
                            action,
                            "Discord delete outcome unknown".to_owned(),
                            elapsed_ms(now_ms, started),
                        );
                    }
                    Err(error) => {
                        self.deletes.remove(&channel_id);
                        self.complete_error(action, channel_id, error);
                    }
                }
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

    fn complete_error(
        &mut self,
        action: QueuedAction,
        channel_id: Snowflake,
        error: RoomHttpError,
    ) {
        self.queue.mark_succeeded(&action);
        if error == RoomHttpError::Unauthorized {
            self.halted = true;
        }
        if error != RoomHttpError::Cancelled {
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
        name: String,
        seed: u64,
        created_at: String,
    },
    CreatorAdded(CreatorChannel),
    /// One-shot worker snapshot for `/setup` (room count, failures, halt).
    Status(oneshot::Sender<WorkerStatus>),
}

/// Per-guild actor registry. Actors spawn lazily on the first complete
/// snapshot and exit when their guild leaves (sender dropped) or their store
/// load fails (respawned on the next event via [`UnboundedSender::is_closed`]).
pub struct VoiceRuntime<S, H> {
    make: Arc<dyn Fn() -> (S, H) + Send + Sync>,
    tick: Duration,
    enabled: bool,
    seeds: AtomicU64,
    actors: Mutex<HashMap<Snowflake, GuildActor>>,
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
        }
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
        tokio::spawn(async move {
            let (store, http) = make();
            let Ok(mut worker) = GuildRoomWorker::load(live, store, http).await else {
                return;
            };
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
        name: String,
    ) -> bool {
        let Some(actor) = self.live_actor(guild) else {
            return false;
        };
        let command = match actor.live.voice_update(member, channel, bot) {
            Some(ticket) => ActorCommand::Join {
                ticket,
                name,
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

    fn creator_added(&self, creator: &CreatorChannel, channel: Channel) {
        if let Some(actor) = self.live_actor(creator.guild_id) {
            actor.live.upsert_channel(channel);
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
                let name = room_name(&display_name(cache, guild_id, member_id));
                self.voice_frame(
                    guild_id,
                    member_id,
                    update.channel_id.map(|id| id.get()),
                    bot,
                    name,
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
                apply_command(worker, command);
            }
            _ = timer.tick() => {
                worker.reconcile();
                let now_ms = start
                    .elapsed()
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64;
                // Return to the inbox after each await. Evidence is already
                // live, but creator configuration/status commands must not sit
                // behind a 64-write burst either.
                worker.dispatch_one(now_ms).await;
            }
        }
    }
}

fn apply_command<S: RoomPersistence, H: RoomWrites>(
    worker: &mut GuildRoomWorker<S, H>,
    command: ActorCommand,
) {
    match command {
        ActorCommand::Reconcile => worker.reconcile(),
        ActorCommand::Join {
            ticket,
            name,
            seed,
            created_at,
        } => {
            worker.accept_join(ticket, name, seed, created_at);
            worker.reconcile();
        }
        ActorCommand::CreatorAdded(creator) => {
            worker.creators.insert(creator.channel_id, creator);
            worker.reconcile();
        }
        ActorCommand::Status(reply) => {
            let _ = reply.send(WorkerStatus {
                tracked_rooms: worker.tracked().len(),
                failures: worker.failures().iter().map(failure_line).collect(),
                halted: worker.halted(),
            });
        }
    }
}

/// Build the production sink: lifecycle actors plus the S4 responder.
pub fn build_production_runtime(
    token: &str,
    pool: sqlx::PgPool,
) -> Result<VoiceResponder<PgRoomStore, RoomHttp, RoomHttp>, RoomHttpError> {
    let replies = RoomHttp::new(token.to_owned())?;
    let http = replies.clone();
    let store = PgRoomStore::new(pool);
    Ok(VoiceResponder::new(
        Arc::new(VoiceRuntime::new(
            move || (store.clone(), http.clone()),
            Duration::from_millis(250),
            true,
        )),
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
        member_roles,
        roles,
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

/// `/setup` panel text. Anyone may view it; the S4 handler gates the quick
/// action and settings buttons on admin (spec V1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupPanel {
    pub title: String,
    pub description: String,
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

// --- `/create` + `/setup` interaction handlers (S4) ---------------------------
//
// Pure parse and auth stay testable without Discord; execution runs one
// guarded REST/SQL round-trip per command and answers with a single
// ephemeral response. Reply transport errors are the caller's to log: the
// handler attempts the response exactly once.

/// A voice slash command carried by an interaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceCommand {
    Create { name: String },
    Setup,
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
        _ => None,
    }
}

/// `/create` needs Manage Channels; admins pass everywhere. `/setup` is
/// view-open, so this gate applies to `/create` only. Fail closed on
/// missing permissions.
fn may_create(permissions: Option<Permissions>) -> bool {
    permissions.is_some_and(|permissions| {
        permissions.intersects(Permissions::ADMINISTRATOR | Permissions::MANAGE_CHANNELS)
    })
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
    pub failures: Vec<String>,
    pub halted: bool,
}

fn failure_line(failure: &LifecycleFailure) -> String {
    match failure {
        LifecycleFailure::CategoryFull {
            creator_id,
            message,
        } => format!("create <#{creator_id}>: {message}"),
        LifecycleFailure::Discord { channel_id, error } => {
            format!("channel <#{channel_id}>: {error}")
        }
        LifecycleFailure::Persistence { channel_id, error } => match channel_id {
            Some(channel) => format!("store <#{channel}>: {error:?}"),
            None => format!("store: {error:?}"),
        },
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
            match error {
                StoreError::CredentialRefused if removed => "Voice rooms are paused: the database refused the bot credential, so the new channel was removed. Tell an admin to fix it, then restart the bot.".to_owned(),
                StoreError::CredentialRefused => format!("Voice rooms are paused: the database refused the bot credential, and removing the new channel failed. Delete <#{channel_id}> manually, then tell an admin to fix the database."),
                _ if removed => "Could not save the new creator channel, so it was removed. Try again.".to_owned(),
                _ => format!("Could not save the new creator channel, and removing it failed. Delete <#{channel_id}> manually and try again."),
            }
        }
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
    let Some(command) = parse_voice_command(interaction) else {
        return false;
    };
    let Some(guild_id) = interaction_guild(interaction) else {
        return false;
    };
    match command {
        VoiceCommand::Setup => {
            let (store, _) = runtime.make_pair();
            let (creators, store_error) = match store.creators(guild_id).await {
                Ok(creators) => (creators, None),
                Err(error) => (Vec::new(), Some(format!("{error:?}"))),
            };
            let status = runtime.worker_status(guild_id).await;
            let panel = setup_panel(&SetupSummary {
                guild_id,
                creators,
                tracked_rooms: status.as_ref().map_or(0, |status| status.tracked_rooms),
                failures: status
                    .as_ref()
                    .map_or_else(Vec::new, |status| status.failures.clone()),
                halted: status.as_ref().is_some_and(|status| status.halted),
                store_error,
            });
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
                    let (store, http) = runtime.make_pair();
                    execute_create(&store, &http, guild_id, &name, |creator, channel| {
                        runtime.creator_added(creator, channel);
                    })
                    .await
                }
            };
            reply(ephemeral_response(&text)).await;
            true
        }
    }
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
        let content = response
            .data
            .as_ref()
            .and_then(|data| data.content.as_deref())
            .unwrap_or_default();
        // Discord limits message content to 2000 characters, including setup
        // listings. Keep the transport valid even in a large guild.
        let content: String = content.chars().take(2000).collect();
        self.complete_interaction(interaction.application_id, &interaction.token, &content)
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

    async fn respond(runtime: &VoiceRuntime<S, H>, replies: &R, interaction: &Interaction) {
        if let Err(error) = replies.defer(interaction).await {
            warn!(interaction_id = interaction.id.get(), %error,
                "voice acknowledgement failed; command not executed");
            return;
        }
        handle_voice_interaction(runtime, interaction, |response| async move {
            if let Err(error) = replies.complete(interaction, response).await {
                warn!(interaction_id = interaction.id.get(), %error,
                    "voice response completion failed; not retried");
            }
        })
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
            if parse_voice_command(&created.0).is_none() {
                return;
            }
            let interaction = created.0.clone();
            let runtime = Arc::clone(&self.runtime);
            let replies = Arc::clone(&self.replies);
            tokio::spawn(async move {
                Self::respond(&runtime, &replies, &interaction).await;
            });
        }
    }

    fn disconnect(&self) {
        self.runtime.disconnect();
    }

    fn needs_bootstrap(&self, cache: &DefaultInMemoryCache) -> bool {
        self.runtime.needs_bootstrap(cache)
    }
}

#[cfg(test)]
#[path = "voice_rooms_tests.rs"]
mod tests;
