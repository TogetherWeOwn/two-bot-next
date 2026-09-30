//! Ordered V1 lifecycle executor. Gateway publication must continue independently
//! while a worker awaits HTTP/SQL: every write rechecks the latest snapshot.
//! This service is not enabled until the bot wires it to complete guild snapshots.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    sync::{Arc, RwLock},
    time::Instant,
};

use twilight_model::{
    channel::{permission_overwrite::PermissionOverwrite, Channel},
    guild::{Permissions, Role},
    id::{marker::RoleMarker, Id},
};
use two_bot_core::{
    voice_rooms::{
        category_full_message, ActionQueue, CreatorChannel, NewRoomSpec, PermissionSource,
        ProposeOutcome, QueuedAction, RenameCoalescer, RoomAction, VoiceRoom,
        MAX_CHANNELS_PER_CATEGORY, RENAME_MIN_INTERVAL_MS,
    },
    Snowflake,
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

#[cfg(test)]
#[path = "voice_rooms_tests.rs"]
mod tests;
