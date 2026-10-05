//! V3 owner privacy runtime: `/private`, `/public` and the Join channel.
//!
//! The pure decisions live in `two_bot_core::voice_private`; this module is
//! the worker's side of them. It is a child of `voice_rooms` so it can use the
//! worker's private state, and the lifecycle code hooks it at five points: the
//! worker fields, `reconcile`, `dispatch_one`, the `DeleteRoom` cleanup and the
//! actor command.
//!
//! Discord first, state after. `/private` and `/public` queue only the
//! @everyone Connect write. The room's private flag (and, on `/private`, the
//! Join channel plan) changes only after that write lands, so a refused write
//! leaves the room exactly as it was and the stored flag never claims more than
//! Discord enforces. The flag and Join channel id are then persisted before the
//! next write is queued; a store failure retries through the same action. On
//! `/public` the Join channel is deleted first, inside that action, so its id
//! stays in the stored record until the channel is gone.
//!
//! Every overwrite written here keeps View Channel untouched and never carries
//! Manage Roles as an allow.

use two_bot_core::{
    voice_name_filter::sanitize_channel_name,
    voice_ownership::require_room_owner,
    voice_private::{
        join_channel_name, ChannelId, JoinChannel, MemberId, PrivacyEffect, PrivacyError,
        PrivateRoom,
    },
};

use super::*;

/// A V3 owner command from an interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivacyCommand {
    /// `/private`: deny Connect to @everyone and open the Join channel.
    Private,
    /// `/public`: restore access and delete the Join channel.
    Public,
}

impl PrivacyCommand {
    fn name(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Public => "public",
        }
    }
}

/// The writes one `/private` or `/public` needs: an optional bot-member grant,
/// then the @everyone entry.
type ConnectWrites = (Option<PermissionOverwrite>, PermissionOverwrite);

const PAUSED: &str = "Voice rooms are paused: Discord refused the bot credential. \
                      Fix the token, then restart the bot.";
const WARMING: &str = "The voice worker isn't warmed up yet — try again in a moment.";
const NEEDS_MANAGE_ROLES: &str = "I can't change who can join this room: I need the Manage Roles \
                                  permission on it. Ask an admin to check my permissions.";

/// Rewriting the overwrite needs View Channel and Manage Roles on the room.
pub(super) fn can_edit_overwrites(permissions: Option<Permissions>) -> bool {
    permissions.is_some_and(|permissions| {
        permissions.contains(Permissions::VIEW_CHANNEL | Permissions::MANAGE_ROLES)
    })
}

/// What the bot itself needs on a room to keep managing it: the same four
/// bits `can_manage_room` checks. Manage Roles is deliberately absent.
fn bot_room_access() -> Permissions {
    Permissions::VIEW_CHANNEL
        | Permissions::CONNECT
        | Permissions::MANAGE_CHANNELS
        | Permissions::MOVE_MEMBERS
}

/// The room's @everyone overwrite after `/private` (`deny_connect`) or
/// `/public`. Only the CONNECT bit moves; every other bit the room already has
/// on @everyone is carried over, because the write replaces the whole entry.
/// View Channel is never touched. Manage Roles is dropped from the allow list
/// so this write never emits it.
pub(super) fn everyone_connect_overwrite(
    guild_id: Snowflake,
    current: &[PermissionOverwrite],
    deny_connect: bool,
) -> PermissionOverwrite {
    let (mut allow, mut deny) = current
        .iter()
        .find(|overwrite| {
            overwrite.kind == PermissionOverwriteType::Role && overwrite.id.get() == guild_id
        })
        .map(|overwrite| (overwrite.allow, overwrite.deny))
        .unwrap_or((Permissions::empty(), Permissions::empty()));
    allow.remove(Permissions::MANAGE_ROLES);
    if deny_connect {
        allow.remove(Permissions::CONNECT);
        deny.insert(Permissions::CONNECT);
    } else {
        deny.remove(Permissions::CONNECT);
    }
    PermissionOverwrite {
        allow,
        deny,
        id: Id::new(guild_id),
        kind: PermissionOverwriteType::Role,
    }
}

/// `current` with `overwrite` replacing the entry for the same target.
pub(super) fn with_overwrite(
    current: &[PermissionOverwrite],
    overwrite: &PermissionOverwrite,
) -> Vec<PermissionOverwrite> {
    let mut next: Vec<PermissionOverwrite> = current
        .iter()
        .filter(|entry| !(entry.kind == overwrite.kind && entry.id == overwrite.id))
        .cloned()
        .collect();
    next.push(*overwrite);
    next
}

/// Denying Connect to @everyone also removes it from the bot when its access
/// came from @everyone or a role. When the planned overwrites would leave the
/// bot unable to manage the room (so it could not move, rename or delete it),
/// the minimal bot-member allow that restores exactly the missing bits.
/// `Ok(None)` means no grant is needed; `AccessDenied` means even a grant
/// cannot restore access (the bot's own entry denies it), so nothing is
/// written. Manage Roles is never granted.
pub(super) fn bot_access_grant(
    guild_id: Snowflake,
    bot: &BotAccess,
    planned: &[PermissionOverwrite],
) -> Result<Option<PermissionOverwrite>, RoomHttpError> {
    let evaluate = |overwrites: &[PermissionOverwrite]| {
        effective_permissions(
            guild_id,
            bot.guild_owner_id,
            bot.member_id,
            &bot.member_roles,
            &bot.roles,
            overwrites,
        )
    };
    if can_manage_room(evaluate(planned)) {
        return Ok(None);
    }
    let held = evaluate(planned).ok_or(RoomHttpError::AccessDenied)?;
    let missing = bot_room_access() & !held;
    let (allow, deny) = planned
        .iter()
        .find(|overwrite| {
            overwrite.kind == PermissionOverwriteType::Member && overwrite.id.get() == bot.member_id
        })
        .map(|overwrite| (overwrite.allow, overwrite.deny))
        .unwrap_or((Permissions::empty(), Permissions::empty()));
    let grant = PermissionOverwrite {
        allow: (allow | missing) & !Permissions::MANAGE_ROLES,
        deny,
        id: Id::new(bot.member_id),
        kind: PermissionOverwriteType::Member,
    };
    if can_manage_room(evaluate(&with_overwrite(planned, &grant))) {
        Ok(Some(grant))
    } else {
        Err(RoomHttpError::AccessDenied)
    }
}

fn owner_gate_refusal(error: OwnershipError, command: PrivacyCommand) -> String {
    match error {
        OwnershipError::NotOwner => {
            format!(
                "Only the room owner or an admin can use /{}.",
                command.name()
            )
        }
        OwnershipError::ActorNotInRoom => {
            format!("You need to be in the room to use /{}.", command.name())
        }
        _ => "Something's off with this room's membership data — leave and rejoin, then try again."
            .to_owned(),
    }
}

impl<S: RoomPersistence, H: RoomWrites> GuildRoomWorker<S, H> {
    /// Rebuild every stored room's privacy state at load. A record for a room
    /// the worker does not track is dropped, and a corrupt one is refused
    /// rather than repaired (the room then reads as public).
    pub(super) fn load_privacy(
        rooms: &HashMap<Snowflake, VoiceRoom>,
        records: BTreeMap<Snowflake, PrivacyRecord>,
    ) -> (HashMap<Snowflake, PrivateRoom>, HashSet<Snowflake>) {
        let mut privacy = HashMap::new();
        let mut deletable = HashSet::new();
        for (room_id, record) in records {
            let Some(room) = rooms.get(&room_id) else {
                continue;
            };
            match PrivateRoom::from_record(ChannelId(room_id), MemberId(room.owner_id), "", &record)
            {
                Ok(state) => {
                    if let Some(id) = record.join_channel_id {
                        deletable.insert(id);
                    }
                    privacy.insert(room_id, state);
                }
                Err(error) => warn!(channel_id = room_id, %error, "voice privacy record refused"),
            }
        }
        (privacy, deletable)
    }

    pub(super) fn join_channel_of(&self, room: Snowflake) -> Option<Snowflake> {
        match self.privacy.get(&room)?.join_channel.as_ref()? {
            JoinChannel::Created { id, .. } => Some(id.0),
            JoinChannel::Requested => None,
        }
    }

    /// The room's cached overwrites, for rewriting one entry.
    pub(super) fn live_overwrites(&self, room: Snowflake) -> Option<Vec<PermissionOverwrite>> {
        let live = self.live.inner.read().expect("live voice lock");
        live.channels
            .get(&room)
            .map(|channel| channel.permission_overwrites.clone().unwrap_or_default())
    }

    fn live_everyone_denies_connect(&self, room: Snowflake) -> bool {
        self.live_overwrites(room).is_some_and(|overwrites| {
            overwrites.iter().any(|overwrite| {
                overwrite.kind == PermissionOverwriteType::Role
                    && overwrite.id.get() == self.live.guild_id
                    && overwrite.deny.contains(Permissions::CONNECT)
            })
        })
    }

    /// Record a written overwrite in the live snapshot, so the next decision
    /// reads what Discord now holds without waiting for the gateway echo.
    pub(super) fn note_overwrite(&self, room: Snowflake, overwrite: &PermissionOverwrite) {
        let mut live = self.live.inner.write().expect("live voice lock");
        if let Some(channel) = live.channels.get_mut(&room) {
            let current = channel.permission_overwrites.take().unwrap_or_default();
            channel.permission_overwrites = Some(with_overwrite(&current, overwrite));
        }
    }

    /// The owner's display name as it may appear in the Join channel's name:
    /// sanitized like a `/create` name (no `@`, backtick or control character),
    /// and the full channel name then goes through the same name filter as a
    /// generated room name. A name that fails the filter is left out (the
    /// channel is then just "⇩ Join"). Only the sanitized name is ever kept,
    /// so what the filter vetted is what Discord receives.
    fn joinable_display(&self, room: &VoiceRoom, display: &str) -> String {
        let context = NameFilterContext {
            guild_id: self.live.guild_id.to_string(),
            channel_id: room.channel_id.to_string(),
            user_id: room.owner_id.to_string(),
        };
        let clean = sanitize_channel_name(display);
        match filter_channel_name(&join_channel_name(&clean), &self.name_policy, &context) {
            Ok(_) => clean,
            Err(_) => String::new(),
        }
    }

    fn privacy_state(
        &self,
        room: &VoiceRoom,
        actor_id: Snowflake,
        actor_display: &str,
    ) -> Result<PrivateRoom, PrivacyError> {
        let mut state = self
            .privacy
            .get(&room.channel_id)
            .cloned()
            .unwrap_or_else(|| {
                PrivateRoom::new(ChannelId(room.channel_id), MemberId(room.owner_id), "")
            });
        state.owner_id = MemberId(room.owner_id);
        if actor_id == room.owner_id && !actor_display.trim().is_empty() {
            state.owner_display = self.joinable_display(room, actor_display);
        }
        state.validate()?;
        Ok(state)
    }

    /// `/private` and `/public`, serialized in the guild actor. Resolves the
    /// caller's current room, gates on owner-or-admin, and queues the
    /// @everyone Connect write; the state change follows that write. Returns
    /// the ephemeral reply, which says what was queued, not that it landed:
    /// a Discord failure is recorded like any other room write and surfaces in
    /// `/setup`.
    pub(super) fn apply_privacy(
        &mut self,
        actor_id: Snowflake,
        is_admin: bool,
        actor_display: &str,
        command: PrivacyCommand,
    ) -> String {
        if self.halted {
            return PAUSED.to_owned();
        }
        let (channel, room, members, bot_permissions) = {
            let live = self.live.inner.read().expect("live voice lock");
            if !live.ready {
                return WARMING.to_owned();
            }
            let Some(channel) = live
                .members
                .get(&actor_id)
                .and_then(|member| member.channel_id)
            else {
                return format!("You need to be in a voice room to use /{}.", command.name());
            };
            let Some(room) = self.rooms.get(&channel).cloned() else {
                return "That voice channel isn't a temporary room I manage.".to_owned();
            };
            (
                channel,
                room,
                live.ownership_snapshot(channel),
                live.permissions(self.live.guild_id, channel),
            )
        };
        let ownership = RoomOwnership {
            owner_id: room.owner_id,
            original_creator_id: room.original_creator_id,
        };
        if let Err(error) = require_room_owner(
            ownership,
            &members,
            RoomActor {
                member_id: actor_id,
                is_admin,
            },
        ) {
            return owner_gate_refusal(error, command);
        }
        let Ok(state) = self.privacy_state(&room, actor_id, actor_display) else {
            return "This room's privacy settings look corrupt. Ask an admin to check /setup."
                .to_owned();
        };
        match command {
            PrivacyCommand::Private => self.apply_private(channel, state, bot_permissions),
            PrivacyCommand::Public => self.apply_public(channel, state, bot_permissions),
        }
    }

    fn apply_private(
        &mut self,
        channel: Snowflake,
        state: PrivateRoom,
        bot_permissions: Option<Permissions>,
    ) -> String {
        let Ok(plan) = state.make_private() else {
            return "This room's privacy settings look corrupt. Ask an admin to check /setup."
                .to_owned();
        };
        // A private room whose Discord overwrite no longer denies Connect
        // (edited by hand, or a crash between the write and its record) is
        // re-asserted; otherwise a repeat changes nothing.
        let drifted = state.private && !self.live_everyone_denies_connect(channel);
        if (!state.private || drifted) && !can_edit_overwrites(bot_permissions) {
            return NEEDS_MANAGE_ROLES.to_owned();
        }
        if plan.is_noop(&state) && !drifted {
            self.privacy.insert(channel, state);
            return "This room is already private.".to_owned();
        }
        if !state.private {
            // The flag and the Join channel follow the Connect write; keep the
            // owner's name for the channel meanwhile.
            self.privacy.insert(channel, state);
            self.enqueue_everyone_connect(channel, true);
            return "Making this room private: new members can't join it directly, and a \
                    ⇩ Join channel will appear next to it."
                .to_owned();
        }
        let join_missing = state.join_channel.is_none();
        self.privacy.insert(channel, plan.room.clone());
        if drifted {
            self.enqueue_everyone_connect(channel, true);
        }
        if join_missing {
            self.enqueue_join_creates(channel, &plan.effects);
            return "This room is already private. Recreating its ⇩ Join channel.".to_owned();
        }
        "This room is private, but its Connect deny was missing. Restoring it.".to_owned()
    }

    fn apply_public(
        &mut self,
        channel: Snowflake,
        state: PrivateRoom,
        bot_permissions: Option<Permissions>,
    ) -> String {
        if !state.private {
            // A room an `/alwaysprivate` creator made already denies Connect to
            // @everyone while its stored flag still reads public, so the flag
            // alone cannot say the room is open: check what Discord holds.
            if !self.live_everyone_denies_connect(channel) {
                self.privacy.insert(channel, state);
                return "This room is already public.".to_owned();
            }
            if !can_edit_overwrites(bot_permissions) {
                return NEEDS_MANAGE_ROLES.to_owned();
            }
            self.privacy.insert(channel, state);
            self.enqueue_everyone_connect(channel, false);
            return "This room wasn't marked private, but it was closed to new members. \
                    Opening it to everyone."
                .to_owned();
        }
        if !can_edit_overwrites(bot_permissions) {
            return NEEDS_MANAGE_ROLES.to_owned();
        }
        self.privacy.insert(channel, state);
        self.enqueue_everyone_connect(channel, false);
        "Making this room public again: anyone can join, and its ⇩ Join channel is going away."
            .to_owned()
    }

    fn enqueue_everyone_connect(&mut self, channel: Snowflake, deny: bool) {
        self.queue.enqueue(
            self.live.guild_id,
            RoomAction::SetEveryoneConnect {
                channel_id: channel,
                deny,
            },
        );
    }

    fn enqueue_join_creates(&mut self, room: Snowflake, effects: &[PrivacyEffect]) {
        for effect in effects {
            if let PrivacyEffect::CreateJoinChannel { name, .. } = effect {
                self.queue.enqueue(
                    self.live.guild_id,
                    RoomAction::CreateJoinChannel {
                        room_channel_id: room,
                        name: name.clone(),
                    },
                );
            }
        }
    }

    /// Write the room's current record. `Ok(false)`: the room has no row.
    async fn persist_privacy(&mut self, room: Snowflake) -> Result<(), StoreError> {
        let Some(state) = self.privacy.get(&room) else {
            self.privacy_dirty.remove(&room);
            return Ok(());
        };
        let record = state.to_record();
        match self
            .store
            .save_privacy(self.live.guild_id, room, &record)
            .await
        {
            Ok(true) => {
                self.privacy_dirty.remove(&room);
                Ok(())
            }
            Ok(false) => {
                // Tracked but no database row (deleted out-of-band): never
                // retry a write that cannot land.
                self.privacy_dirty.remove(&room);
                self.record(LifecycleFailure::Persistence {
                    channel_id: Some(room),
                    error: StoreError::Conflict,
                });
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Release `action` after a failed store write: retry through the queue
    /// budget, or halt when the credential was refused.
    fn privacy_store_failed(
        &mut self,
        action: QueuedAction,
        room: Snowflake,
        error: StoreError,
        now_ms: u64,
        started: Instant,
    ) {
        self.record(LifecycleFailure::Persistence {
            channel_id: Some(room),
            error,
        });
        if error == StoreError::CredentialRefused {
            self.halted = true;
            self.queue.mark_succeeded(&action);
        } else {
            self.mark_failed_observed(
                action,
                "voice-room persistence unavailable".to_owned(),
                elapsed_ms(now_ms, started),
            );
        }
    }

    /// Route one privacy write from `dispatch_one`.
    pub(super) async fn dispatch_privacy(
        &mut self,
        action: QueuedAction,
        now_ms: u64,
        started: Instant,
    ) {
        match action.action.clone() {
            RoomAction::SetEveryoneConnect { channel_id, deny } => {
                self.dispatch_everyone_connect(action, channel_id, deny, now_ms, started)
                    .await;
            }
            RoomAction::CreateJoinChannel {
                room_channel_id,
                name,
            } => {
                self.dispatch_create_join(action, room_channel_id, name, now_ms, started)
                    .await;
            }
            RoomAction::DeleteJoinChannel { channel_id, .. } => {
                self.dispatch_delete_join(action, channel_id, now_ms, started)
                    .await;
            }
            RoomAction::SavePrivacy { channel_id } => {
                if !self.rooms.contains_key(&channel_id)
                    || !self.privacy_dirty.contains(&channel_id)
                {
                    self.queue.mark_succeeded(&action);
                    return;
                }
                match self.persist_privacy(channel_id).await {
                    Ok(()) => {
                        self.queue.mark_succeeded(&action);
                    }
                    Err(error) => {
                        self.privacy_store_failed(action, channel_id, error, now_ms, started);
                    }
                }
            }
            _ => {
                self.queue.mark_succeeded(&action);
            }
        }
    }

    /// The @everyone Connect write, then the state change it gates.
    async fn dispatch_everyone_connect(
        &mut self,
        action: QueuedAction,
        room: Snowflake,
        deny: bool,
        now_ms: u64,
        started: Instant,
    ) {
        if !self.rooms.contains_key(&room) {
            self.queue.mark_succeeded(&action);
            return;
        }
        // The room channel is gone: reconcile deletes the row, nothing to write.
        let Some(planned) = self.plan_connect_write(room, deny) else {
            self.queue.mark_succeeded(&action);
            return;
        };
        let result = match planned {
            Ok((bot_grant, everyone)) => {
                match self.write_overwrites(room, bot_grant, &everyone).await {
                    // The Join channel goes before the flag flips and the record is
                    // written: its id stays stored until the channel is gone, so a
                    // restart between the two steps can still delete it (`/public`
                    // again retries the delete), and a failure retries this action.
                    Ok(()) if !deny => self.delete_join_of(room).await,
                    written => written,
                }
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => {
                if deny {
                    self.finish_private(room);
                } else {
                    self.finish_public(room);
                }
                self.privacy_dirty.insert(room);
                match self.persist_privacy(room).await {
                    Ok(()) => {
                        self.queue.mark_succeeded(&action);
                    }
                    Err(error) => {
                        self.privacy_store_failed(action, room, error, now_ms, started);
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
            Err(RoomHttpError::UnknownOutcome) => {
                // Both writes replace a whole entry and the Join delete treats
                // a missing channel as done, so a retry is safe.
                self.mark_failed_observed(
                    action,
                    "Discord privacy write outcome unknown".to_owned(),
                    elapsed_ms(now_ms, started),
                );
            }
            Err(error) => self.complete_error(action, room, error),
        }
    }

    /// One live read of the room and the bot, planned into the writes.
    /// `None` when the room channel is not in the snapshot.
    fn plan_connect_write(
        &self,
        room: Snowflake,
        deny: bool,
    ) -> Option<Result<ConnectWrites, RoomHttpError>> {
        let live = self.live.inner.read().expect("live voice lock");
        let channel = live.channels.get(&room)?;
        let Some(bot) = live.bot.as_ref() else {
            return Some(Err(RoomHttpError::AccessDenied));
        };
        let current = channel.permission_overwrites.clone().unwrap_or_default();
        Some(self.plan_everyone_connect(&live, room, bot, &current, deny))
    }

    /// The writes `/private` or `/public` needs, planned from one live read:
    /// an optional bot-member grant (only when denying would strand the bot)
    /// followed by the @everyone entry. Refuses, writing nothing, when the bot
    /// cannot edit overwrites or could not keep managing the room.
    fn plan_everyone_connect(
        &self,
        live: &LiveState,
        room: Snowflake,
        bot: &BotAccess,
        current: &[PermissionOverwrite],
        deny: bool,
    ) -> Result<ConnectWrites, RoomHttpError> {
        if !can_edit_overwrites(live.permissions(self.live.guild_id, room)) {
            return Err(RoomHttpError::AccessDenied);
        }
        let everyone = everyone_connect_overwrite(self.live.guild_id, current, deny);
        let bot_grant = if deny {
            bot_access_grant(self.live.guild_id, bot, &with_overwrite(current, &everyone))?
        } else {
            None
        };
        Ok((bot_grant, everyone))
    }

    /// Bot grant first so it can never lose access between the two writes,
    /// then @everyone. Each success is recorded in the live snapshot.
    async fn write_overwrites(
        &mut self,
        room: Snowflake,
        bot_grant: Option<PermissionOverwrite>,
        everyone: &PermissionOverwrite,
    ) -> Result<(), RoomHttpError> {
        if let Some(grant) = bot_grant {
            self.http
                .put_overwrite(room, grant, self.live.room_guard(room))
                .await?;
            self.note_overwrite(room, &grant);
        }
        self.http
            .put_overwrite(room, *everyone, self.live.room_guard(room))
            .await?;
        self.note_overwrite(room, everyone);
        Ok(())
    }

    /// `/private` landed: flip the flag and plan the Join channel.
    fn finish_private(&mut self, room: Snowflake) {
        let Some(state) = self.privacy.get(&room).cloned() else {
            return;
        };
        let Ok(plan) = state.make_private() else {
            return;
        };
        self.privacy.insert(room, plan.room.clone());
        self.enqueue_join_creates(room, &plan.effects);
    }

    /// `/public` landed and the Join channel is already deleted: flip the
    /// flag, take back every approved member's Connect allow and retire the
    /// buttons of requests that can no longer be answered.
    fn finish_public(&mut self, room: Snowflake) {
        let Some(state) = self.privacy.get(&room).cloned() else {
            return;
        };
        let Ok(plan) = state.make_public() else {
            return;
        };
        self.privacy.insert(room, plan.room);
        self.enqueue_join_effects(room, &plan.effects);
    }

    /// Create the Join channel next to a private room, once.
    async fn dispatch_create_join(
        &mut self,
        action: QueuedAction,
        room: Snowflake,
        name: String,
        now_ms: u64,
        started: Instant,
    ) {
        let planned = self.privacy.get(&room).is_some_and(|state| {
            state.private && state.join_channel == Some(JoinChannel::Requested)
        });
        if !self.rooms.contains_key(&room) || !planned {
            // Gone, made public meanwhile, or already created: nothing to do.
            self.queue.mark_succeeded(&action);
            return;
        }
        // A retry after an unknown outcome adopts the channel the snapshot now
        // shows instead of creating a second one.
        if action.attempts > 0 {
            if let Some(adopted) = self.adopt_join_channel(room, &name) {
                self.finish_join_created(action, room, adopted, &name, now_ms, started)
                    .await;
                return;
            }
        }
        let placement = self
            .live
            .inner
            .read()
            .expect("live voice lock")
            .channels
            .get(&room)
            .map(|channel| {
                (
                    channel.parent_id.map(Id::get),
                    // Directly after the room, in its category.
                    channel
                        .position
                        .and_then(|position| u64::try_from(position.saturating_add(1)).ok()),
                )
            });
        let Some((parent_id, position)) = placement else {
            self.queue.mark_succeeded(&action);
            return;
        };
        match self
            .http
            .create_join_channel(
                self.live.guild_id,
                &name,
                parent_id,
                position,
                self.live.room_guard(room),
            )
            .await
        {
            Ok(channel) => {
                let id = channel.id.get();
                self.live.upsert_channel(channel);
                self.finish_join_created(action, room, id, &name, now_ms, started)
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
                // The POST may have landed: retry adopts instead of re-creating.
                // On the last attempt forget the plan, so `/private` can ask again.
                if action.attempts.saturating_add(1) >= QUEUE_MAX_ATTEMPTS {
                    self.forget_join_plan(room);
                }
                self.mark_failed_observed(
                    action,
                    "Discord Join channel create outcome unknown".to_owned(),
                    elapsed_ms(now_ms, started),
                );
            }
            Err(error) => {
                self.forget_join_plan(room);
                self.complete_error(action, room, error);
            }
        }
    }

    /// A planned creation will not happen: `/private` may plan it again.
    fn forget_join_plan(&mut self, room: Snowflake) {
        if let Some(state) = self.privacy.get(&room) {
            if let Ok(plan) = state.join_channel_creation_failed() {
                self.privacy.insert(room, plan.room);
            }
        }
    }

    /// A voice channel in the room's category with the planned name, newer
    /// than the room and held by no tracked room or Join channel: the Join
    /// channel an earlier, unknown-outcome create left behind.
    fn adopt_join_channel(&self, room: Snowflake, name: &str) -> Option<Snowflake> {
        let live = self.live.inner.read().expect("live voice lock");
        let parent = live.channels.get(&room)?.parent_id;
        live.channels
            .values()
            .filter(|channel| {
                channel.kind == ChannelType::GuildVoice
                    && channel.parent_id == parent
                    && channel.name.as_deref() == Some(name)
                    && channel.id.get() > room
                    && !self.rooms.contains_key(&channel.id.get())
                    && !self
                        .privacy
                        .keys()
                        .any(|other| self.join_channel_of(*other) == Some(channel.id.get()))
            })
            .map(|channel| channel.id.get())
            .min()
    }

    /// The Join channel exists: record it, persist, and delete it again if
    /// the room stopped wanting one while it was being created.
    async fn finish_join_created(
        &mut self,
        action: QueuedAction,
        room: Snowflake,
        channel_id: Snowflake,
        name: &str,
        now_ms: u64,
        started: Instant,
    ) {
        self.join_deletable.insert(channel_id);
        let mut unwanted = Vec::new();
        if let Some(state) = self.privacy.get(&room).cloned() {
            if let Ok(plan) = state.join_channel_created(ChannelId(channel_id), name) {
                self.privacy.insert(room, plan.room);
                for effect in plan.effects {
                    if let PrivacyEffect::DeleteJoinChannel { channel_id } = effect {
                        unwanted.push(channel_id.0);
                    }
                }
            }
        }
        // A channel the room no longer wants (it went public while this was
        // being created) is deleted before the record is written, so no
        // untracked Join channel outlives a restart. The record cannot hold its
        // id (a public room has none), so a delete that fails here falls back
        // to the queue.
        for id in unwanted {
            if self.delete_join_channel(id).await.is_err() {
                self.queue.enqueue(
                    self.live.guild_id,
                    RoomAction::DeleteJoinChannel {
                        room_channel_id: room,
                        channel_id: id,
                    },
                );
            }
        }
        self.privacy_dirty.insert(room);
        match self.persist_privacy(room).await {
            Ok(()) => {
                self.queue.mark_succeeded(&action);
            }
            Err(error) => {
                // The channel exists: the retry sees the recorded Join channel
                // and writes only the row, never another POST.
                self.privacy_store_failed(action, room, error, now_ms, started);
            }
        }
    }

    /// Delete a Join channel the worker created or loaded from its own store.
    /// An id it does not hold is never deleted.
    async fn dispatch_delete_join(
        &mut self,
        action: QueuedAction,
        channel_id: Snowflake,
        now_ms: u64,
        started: Instant,
    ) {
        if !self.join_deletable.contains(&channel_id) {
            self.queue.mark_succeeded(&action);
            return;
        }
        match self.delete_join_channel(channel_id).await {
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
                self.mark_failed_observed(
                    action,
                    "Discord Join channel delete outcome unknown".to_owned(),
                    elapsed_ms(now_ms, started),
                );
            }
            Err(error) => self.complete_error(action, channel_id, error),
        }
    }

    /// The room is being deleted: delete its Join channel first. `false`
    /// retries the whole room delete (every step is idempotent), so the Join
    /// channel is never leaked behind a forgotten room.
    pub(super) async fn delete_join_for(&mut self, room: Snowflake) -> bool {
        self.delete_join_of(room).await.is_ok()
    }

    /// Delete the room's recorded Join channel, if it has one.
    async fn delete_join_of(&mut self, room: Snowflake) -> Result<(), RoomHttpError> {
        match self.join_channel_of(room) {
            Some(channel_id) => self.delete_join_channel(channel_id).await,
            None => Ok(()),
        }
    }

    /// Delete one Join channel. A channel the snapshot no longer shows, or one
    /// Discord reports gone, counts as deleted.
    async fn delete_join_channel(&mut self, channel_id: Snowflake) -> Result<(), RoomHttpError> {
        let present = self
            .live
            .inner
            .read()
            .expect("live voice lock")
            .channels
            .contains_key(&channel_id);
        if !present {
            self.join_deletable.remove(&channel_id);
            return Ok(());
        }
        let live = self.live.clone();
        let guard: WriteGuard = Arc::new(move || {
            let state = live.inner.read().expect("live voice lock");
            state.ready && state.channels.contains_key(&channel_id)
        });
        match self.http.delete(channel_id, guard).await {
            Ok(()) | Err(RoomHttpError::NotFound) => {
                self.live.remove_channel(channel_id);
                self.join_deletable.remove(&channel_id);
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Drop a deleted room's privacy state. The database rows go with the room
    /// row (`voice_room_blocks` cascades), so nothing else is written.
    pub(super) fn forget_privacy(&mut self, room: Snowflake) {
        self.forget_join_requests(room);
        if let Some(channel_id) = self.join_channel_of(room) {
            self.join_deletable.remove(&channel_id);
        }
        self.privacy.remove(&room);
        self.privacy_dirty.remove(&room);
    }

    /// Reconcile each room's stored Join channel with the live snapshot: one
    /// that still exists is adopted (it stays deletable by this worker), one
    /// that does not is forgotten and the forgetting persisted. The room stays
    /// private either way; `/private` plans a new Join channel. Runs only on
    /// an authoritative snapshot that still shows the room itself.
    pub(super) fn reconcile_privacy(&mut self) {
        let mut forgotten = Vec::new();
        {
            let live = self.live.inner.read().expect("live voice lock");
            if !live.ready {
                return;
            }
            let rooms: Vec<Snowflake> = self.privacy.keys().copied().collect();
            for room in rooms {
                if !live.channels.contains_key(&room) {
                    continue;
                }
                let Some(join) = self.join_channel_of(room) else {
                    continue;
                };
                if live.channels.contains_key(&join) {
                    self.join_deletable.insert(join);
                } else {
                    forgotten.push((room, join));
                }
            }
        }
        for (room, join) in forgotten {
            let Some(state) = self.privacy.get(&room) else {
                continue;
            };
            let Ok(plan) = state.join_channel_deleted(ChannelId(join)) else {
                continue;
            };
            self.privacy.insert(room, plan.room);
            self.join_deletable.remove(&join);
            self.privacy_dirty.insert(room);
            self.queue.enqueue(
                self.live.guild_id,
                RoomAction::SavePrivacy { channel_id: room },
            );
        }
    }
}
