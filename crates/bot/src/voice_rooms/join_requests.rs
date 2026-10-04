//! V3 join requests: an outsider enters a private room's Join channel, the
//! room's owner gets Approve / Deny / Block buttons, and the answer is applied.
//!
//! The decisions are pure and live in `two_bot_core::voice_private`
//! (`enter_join_channel`, `decide`, `set_owner`); this module is the worker's
//! side of them. It is a child of `voice_rooms`, hooked at a few points: the
//! worker field, `reconcile`, `dispatch_one`, the actor command, the owner
//! handoff, `/public` and the room delete cleanup.
//!
//! **Entry.** `reconcile` sees who sits in each private room's Join channel.
//! A member counts as having *entered* when the gateway recorded a new
//! transition for them (or they are new to the Join channel), so an owner's
//! Deny does not raise the same member again every tick: they leave and come
//! back to ask again, and the core hands that request a fresh id.
//!
//! **Prompt.** The owner is asked in the room's own chat. The message carries
//! three `two:voice:join-*` buttons bound to the room and the request id. Only
//! the owner is pinged. There is no DM fallback: a press on a DM message
//! carries no guild, so it could not be routed or role-gated.
//!
//! **Answer.** A click is authorized against the room's *current* owner, never
//! the message it sits on, and against the request's current state. A stale
//! button (answered, withdrawn, from before a restart) is answered with a
//! refusal and changes nothing. Approve queues one write that gives the member
//! a Connect allow on the room only and moves them in from the Join channel;
//! Block persists through the privacy record; Deny writes nothing.
//!
//! **Restarts.** Pending requests, grants and the posted prompts are runtime
//! only. A request id is `epoch << 20 | n`, where the epoch is the worker's
//! start time in milliseconds (never lower than any earlier worker's in this
//! process), so a button minted before a restart can never match a request
//! raised after it.
//!
//! The grant is a member overwrite that nothing records durably. A restart
//! forgets which members were approved, so a later `/public` cannot take their
//! Connect allow back; the block list is durable and unaffected.

use two_bot_core::{
    voice_custom_id::join_custom_id,
    voice_private::{
        ChannelId, EntryOutcome, JoinChannel, JoinDecision, MemberId, PrivacyEffect, PrivacyError,
        RequestId,
    },
};

use super::{private_runtime::can_edit_overwrites, *};

/// Request ids sit above the epoch's low bits, leaving a million ids per
/// millisecond of epoch before one worker could reach the next one's range.
const EPOCH_SHIFT: u32 = 20;

/// Highest epoch a worker may use before the shift would overflow `u64`.
const MAX_EPOCH_MS: u64 = (1 << 43) - 1;

/// The base every worker in this process has handed out so far: a respawned
/// worker (same millisecond, or a clock that stepped back) still gets a
/// strictly later range.
static LAST_EPOCH_BASE: AtomicU64 = AtomicU64::new(0);

const PAUSED: &str = "Voice rooms are paused: Discord refused the bot credential. \
                      Fix the token, then restart the bot.";
const WARMING: &str = "The voice worker isn't warmed up yet — try again in a moment.";
const ROOM_GONE: &str = "That room no longer exists.";
const NOT_OWNER: &str = "Only the room's current owner can answer join requests.";
const NOT_PENDING: &str = "This join request is no longer pending.";
const CORRUPT: &str = "This room's privacy settings look corrupt. Ask an admin to check /setup.";
const NEEDS_MANAGE_ROLES: &str = "I can't let anyone in: I need the Manage Roles permission on \
                                  this room. Ask an admin to check my permissions.";

/// The worker's join-request bookkeeping. All of it is runtime-only.
#[derive(Debug)]
pub(super) struct JoinRequests {
    /// First request id this worker may hand out.
    pub(super) epoch_base: u64,
    /// `(Join channel, member)` entries already handled, with the gateway
    /// transition they were handled at.
    pub(super) entries: HashMap<(Snowflake, Snowflake), u64>,
    /// The posted prompt for each request still waiting for an answer.
    pub(super) prompts: HashMap<(Snowflake, u64), MessageRef>,
}

impl JoinRequests {
    /// `epoch_ms` is the worker's start time in wall-clock milliseconds.
    pub(super) fn new(epoch_ms: u64) -> Self {
        let clock_base = epoch_ms.min(MAX_EPOCH_MS) << EPOCH_SHIFT;
        let step = 1u64 << EPOCH_SHIFT;
        let previous = LAST_EPOCH_BASE
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |previous| {
                Some(clock_base.max(previous.saturating_add(step)))
            })
            .unwrap_or(0);
        Self {
            epoch_base: clock_base.max(previous.saturating_add(step)),
            entries: HashMap::new(),
            prompts: HashMap::new(),
        }
    }
}

/// One Approve / Deny / Block press, parsed from its component id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct JoinClick {
    pub decision: JoinDecision,
    pub room_id: Snowflake,
    pub request_id: u64,
}

/// A press handed to the guild worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct JoinDecisionCommand {
    pub actor_id: Snowflake,
    pub click: JoinClick,
}

/// What the worker decided; the handler turns it into the Discord response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum JoinReply {
    /// An ephemeral refusal. Nothing changed.
    Refused(String),
    /// The request is answered; the text replaces the prompt and its buttons.
    Decided(String),
}

/// Parse a join-request button press. `None` for anything else, so other
/// components (including other `two:voice:` ids) stay with their owners.
pub(super) fn join_component_action(interaction: &Interaction) -> Option<JoinClick> {
    if interaction.kind != InteractionType::MessageComponent {
        return None;
    }
    interaction_guild(interaction)?;
    let InteractionData::MessageComponent(data) = interaction.data.as_ref()? else {
        return None;
    };
    let (decision, room_id, request_id) = match parse_voice_custom_id(&data.custom_id)? {
        VoiceAction::JoinApprove {
            room_id,
            request_id,
        } => (JoinDecision::Approve, room_id, request_id),
        VoiceAction::JoinDeny {
            room_id,
            request_id,
        } => (JoinDecision::Deny, room_id, request_id),
        VoiceAction::JoinBlock {
            room_id,
            request_id,
        } => (JoinDecision::Block, room_id, request_id),
        _ => return None,
    };
    Some(JoinClick {
        decision,
        room_id,
        request_id,
    })
}

/// The owner prompt: who is waiting, plus Approve / Deny / Block bound to the
/// room and the request. The only text is ids, never a member's name.
fn prompt_message(
    owner: Snowflake,
    member: Snowflake,
    room: Snowflake,
    request_id: u64,
) -> (String, Vec<Component>) {
    let button = |label: &str, style: ButtonStyle, decision: JoinDecision| {
        Component::Button(Button {
            id: None,
            custom_id: Some(join_custom_id(decision, room, request_id)),
            disabled: false,
            emoji: None,
            label: Some(label.to_owned()),
            style,
            url: None,
            sku_id: None,
        })
    };
    (
        format!(
            "<@{owner}>, <@{member}> is waiting in the Join channel and wants to join this room."
        ),
        vec![Component::ActionRow(ActionRow {
            id: None,
            components: vec![
                button("Approve", ButtonStyle::Success, JoinDecision::Approve),
                button("Deny", ButtonStyle::Secondary, JoinDecision::Deny),
                button("Block", ButtonStyle::Danger, JoinDecision::Block),
            ],
        })],
    )
}

/// The answer that replaces the prompt: its buttons are gone and nobody is
/// pinged by the text.
pub(super) fn decided_response(text: &str) -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::UpdateMessage,
        data: Some(InteractionResponseData {
            content: Some(text.to_owned()),
            components: Some(Vec::new()),
            allowed_mentions: Some(AllowedMentions::default()),
            ..Default::default()
        }),
    }
}

/// Handle one join-request button press. The guild role gate of `/private`
/// applies first; the worker then decides. Always replies exactly once and
/// returns true.
pub(super) async fn handle_join_interaction<S, H, F>(
    runtime: &VoiceRuntime<S, H>,
    interaction: &Interaction,
    guild_id: Snowflake,
    click: JoinClick,
    reply: impl FnOnce(InteractionResponse) -> F + Send,
) -> bool
where
    S: RoomPersistence + Send + 'static,
    H: RoomWrites + Send + 'static,
    F: Future<Output = ()> + Send,
{
    let Some(actor_id) = invoker_member_id(interaction) else {
        reply(ephemeral_response(
            "I couldn't tell who pressed that button — try again.",
        ))
        .await;
        return true;
    };
    let member = access_member(interaction, runtime.guild_permissions(interaction));
    let (store, _) = runtime.make_pair();
    if let Some(denial) = command_gate(&store, guild_id, &member, "private").await {
        reply(denial).await;
        return true;
    }
    let response = match runtime
        .run_join_decision(guild_id, JoinDecisionCommand { actor_id, click })
        .await
    {
        None => ephemeral_response(WARMING),
        Some(JoinReply::Refused(text)) => ephemeral_response(&text),
        Some(JoinReply::Decided(text)) => decided_response(&text),
    };
    reply(response).await;
    true
}

impl<S, H> VoiceRuntime<S, H>
where
    S: RoomPersistence + Send + 'static,
    H: RoomWrites + Send + 'static,
{
    /// Run one press through the guild actor. `None` when the guild has no
    /// live actor yet or its inbox already drained.
    async fn run_join_decision(
        &self,
        guild: Snowflake,
        decision: JoinDecisionCommand,
    ) -> Option<JoinReply> {
        let actor = self.live_actor(guild)?;
        let (reply, inbox) = oneshot::channel();
        actor
            .tx
            .send(ActorCommand::JoinDecision { decision, reply })
            .ok()?;
        inbox.await.ok()
    }
}

impl<S: RoomPersistence, H: RoomWrites> GuildRoomWorker<S, H> {
    /// Raise a request for every member who has newly entered a private
    /// room's Join channel. Runs on every reconcile; cheap when no room is
    /// private.
    pub(super) fn scan_join_entries(&mut self) {
        if self.halted {
            return;
        }
        let joins: HashMap<Snowflake, Snowflake> = self
            .privacy
            .iter()
            .filter(|(_, state)| state.private)
            .filter_map(|(room, _)| self.join_channel_of(*room).map(|join| (join, *room)))
            .collect();
        if joins.is_empty() {
            self.join.entries.clear();
            return;
        }
        let mut entrants = Vec::new();
        {
            let live = self.live.inner.read().expect("live voice lock");
            if !live.ready {
                return;
            }
            let mut present = HashSet::new();
            for (member, state) in &live.members {
                let Some(join) = state
                    .channel_id
                    .filter(|channel| joins.contains_key(channel))
                else {
                    continue;
                };
                // Unknown identity counts as human; only a known bot is skipped.
                if state.bot == Some(true) {
                    continue;
                }
                present.insert((join, *member));
                let fresh = match self.join.entries.get(&(join, *member)) {
                    None => true,
                    // A snapshot rebuild resets every transition to 0: that is
                    // the same stay, not a new entry.
                    Some(seen) => *seen != state.transition && state.transition != 0,
                };
                if fresh {
                    entrants.push((joins[&join], join, *member, state.transition));
                }
            }
            // Members who left forget their entry, so coming back counts.
            self.join.entries.retain(|key, _| present.contains(key));
        }
        entrants.sort_unstable();
        for (room, join, member, transition) in entrants {
            self.join.entries.insert((join, member), transition);
            self.raise_join_request(room, join, member);
        }
    }

    /// Run the core's entry decision for one member and queue the owner's
    /// prompt when it raised a request.
    fn raise_join_request(&mut self, room: Snowflake, join: Snowflake, member: Snowflake) {
        self.join_owner_changed(room);
        let Some(mut state) = self.privacy.get(&room).cloned() else {
            return;
        };
        let occupants: Vec<MemberId> = self
            .live
            .inner
            .read()
            .expect("live voice lock")
            .occupants(room)
            .into_iter()
            .map(MemberId)
            .collect();
        // Never hand out an id a previous worker could have minted.
        state.next_request_id = state.next_request_id.max(self.join.epoch_base);
        match state.enter_join_channel(ChannelId(join), MemberId(member), &occupants) {
            Ok(entry) => {
                self.privacy.insert(room, entry.plan.room);
                if let EntryOutcome::Raised(request) = entry.outcome {
                    self.queue.enqueue(
                        self.live.guild_id,
                        RoomAction::AskJoinOwner {
                            room_channel_id: room,
                            member_id: request.member_id.0,
                            request_id: request.id.0,
                        },
                    );
                }
            }
            Err(error) => warn!(channel_id = room, %error, "voice join entry refused"),
        }
    }

    /// Apply one Approve / Deny / Block press. Authorization is the room's
    /// current owner, checked here on every press: a button carries only the
    /// room and request ids, so a stale press from a previous owner, or for a
    /// request that was answered or withdrawn, is refused and changes nothing.
    pub(super) fn apply_join_decision(&mut self, command: JoinDecisionCommand) -> JoinReply {
        let JoinDecisionCommand { actor_id, click } = command;
        if self.halted {
            return JoinReply::Refused(PAUSED.to_owned());
        }
        let bot_permissions = {
            let live = self.live.inner.read().expect("live voice lock");
            if !live.ready {
                return JoinReply::Refused(WARMING.to_owned());
            }
            live.permissions(self.live.guild_id, click.room_id)
        };
        if !self.rooms.contains_key(&click.room_id) {
            return JoinReply::Refused(ROOM_GONE.to_owned());
        }
        self.join_owner_changed(click.room_id);
        let Some(state) = self.privacy.get(&click.room_id).cloned() else {
            return JoinReply::Refused(NOT_PENDING.to_owned());
        };
        let requester = state
            .pending
            .values()
            .find(|request| request.id.0 == click.request_id)
            .map(|request| request.member_id.0);
        let plan = match state.decide(
            MemberId(actor_id),
            RequestId(click.request_id),
            click.decision,
        ) {
            Ok(plan) => plan,
            Err(PrivacyError::NotOwner) => return JoinReply::Refused(NOT_OWNER.to_owned()),
            Err(PrivacyError::NotPrivate | PrivacyError::RequestNotPending) => {
                return JoinReply::Refused(NOT_PENDING.to_owned());
            }
            Err(_) => return JoinReply::Refused(CORRUPT.to_owned()),
        };
        let Some(member) = requester else {
            return JoinReply::Refused(NOT_PENDING.to_owned());
        };
        if click.decision == JoinDecision::Approve {
            if !can_edit_overwrites(bot_permissions) {
                return JoinReply::Refused(NEEDS_MANAGE_ROLES.to_owned());
            }
            // A vote-kick leaves a member-scoped Connect deny on the room.
            // Approving would overwrite it, so the owner cannot undo a vote.
            if self.member_denied_connect(click.room_id, member) {
                return JoinReply::Refused(format!(
                    "<@{member}> was removed from this room by a vote, so I can't let them back \
                     in. You can still deny or block them."
                ));
            }
        }
        self.privacy.insert(click.room_id, plan.room);
        self.join.prompts.remove(&(click.room_id, click.request_id));
        match click.decision {
            JoinDecision::Approve => {
                self.queue.enqueue(
                    self.live.guild_id,
                    RoomAction::ApproveJoin {
                        room_channel_id: click.room_id,
                        member_id: member,
                    },
                );
                JoinReply::Decided(format!(
                    "Approved: <@{member}> is being let in and moved to this room."
                ))
            }
            JoinDecision::Deny => {
                JoinReply::Decided(format!("Denied: <@{member}> was turned away."))
            }
            JoinDecision::Block => {
                self.privacy_dirty.insert(click.room_id);
                self.queue.enqueue(
                    self.live.guild_id,
                    RoomAction::SavePrivacy {
                        channel_id: click.room_id,
                    },
                );
                JoinReply::Decided(format!(
                    "Blocked: <@{member}> can no longer ask to join this room."
                ))
            }
        }
    }

    /// Whether the room carries a member-scoped Connect deny for `member`.
    fn member_denied_connect(&self, room: Snowflake, member: Snowflake) -> bool {
        self.live_overwrites(room).is_some_and(|overwrites| {
            overwrites.iter().any(|overwrite| {
                overwrite.kind == PermissionOverwriteType::Member
                    && overwrite.id.get() == member
                    && overwrite.deny.contains(Permissions::CONNECT)
            })
        })
    }

    /// The room has a new owner: requests raised to the previous owner are
    /// withdrawn (their buttons retire) and everyone now waiting in the Join
    /// channel is asked again, of the new owner. Privacy, grants and the block
    /// list are kept. The Join channel keeps the name Discord holds.
    pub(super) fn join_owner_changed(&mut self, room: Snowflake) {
        let Some(owner) = self.rooms.get(&room).map(|room| room.owner_id) else {
            return;
        };
        let Some(state) = self.privacy.get(&room).cloned() else {
            return;
        };
        if state.owner_id == MemberId(owner) {
            return;
        }
        let plan = match state.set_owner(MemberId(owner), &state.owner_display) {
            Ok(plan) => plan,
            Err(error) => {
                warn!(channel_id = room, %error, "voice join owner change refused");
                return;
            }
        };
        let mut next = plan.room;
        // Renaming the Join channel for the new owner is not this slice's
        // write: keep recording the name Discord holds.
        next.join_channel = state.join_channel.clone();
        self.privacy.insert(room, next);
        self.enqueue_join_effects(room, &plan.effects);
        if let Some(JoinChannel::Created { id, .. }) = &state.join_channel {
            let join = id.0;
            self.join.entries.retain(|(channel, _), _| *channel != join);
        }
    }

    /// Queue the Discord work of a privacy plan that touches join requests:
    /// take approved members' Connect allow back and retire withdrawn
    /// requests' buttons. Other effects belong to their own paths.
    pub(super) fn enqueue_join_effects(&mut self, room: Snowflake, effects: &[PrivacyEffect]) {
        for effect in effects {
            match effect {
                PrivacyEffect::RevokeConnect { member_id, .. } => {
                    self.queue.enqueue(
                        self.live.guild_id,
                        RoomAction::RevokeJoinAccess {
                            room_channel_id: room,
                            member_id: member_id.0,
                        },
                    );
                }
                PrivacyEffect::WithdrawRequest { request } => {
                    self.retire_join_prompt(room, request.id.0);
                }
                _ => {}
            }
        }
    }

    fn retire_join_prompt(&mut self, room: Snowflake, request_id: u64) {
        if let Some(message) = self.join.prompts.remove(&(room, request_id)) {
            self.queue.enqueue(
                self.live.guild_id,
                RoomAction::RetireJoinPrompt {
                    room_channel_id: room,
                    message_id: message.message_id,
                },
            );
        }
    }

    /// The room is being forgotten: its prompts live in the room's chat and go
    /// with the channel, so only the bookkeeping is dropped, along with what
    /// the scan remembered about its Join channel.
    pub(super) fn forget_join_requests(&mut self, room: Snowflake) {
        self.join.prompts.retain(|(channel, _), _| *channel != room);
        if let Some(join) = self.join_channel_of(room) {
            self.join.entries.retain(|(channel, _), _| *channel != join);
        }
    }

    /// Route one join-request write from `dispatch_one`.
    pub(super) async fn dispatch_join(
        &mut self,
        action: QueuedAction,
        now_ms: u64,
        started: Instant,
    ) {
        match action.action.clone() {
            RoomAction::AskJoinOwner {
                room_channel_id,
                member_id,
                request_id,
            } => {
                self.dispatch_ask_owner(
                    action,
                    room_channel_id,
                    member_id,
                    request_id,
                    now_ms,
                    started,
                )
                .await;
            }
            RoomAction::ApproveJoin {
                room_channel_id,
                member_id,
            } => {
                self.dispatch_approve(action, room_channel_id, member_id, now_ms, started)
                    .await;
            }
            RoomAction::RevokeJoinAccess {
                room_channel_id,
                member_id,
            } => {
                self.dispatch_revoke(action, room_channel_id, member_id, now_ms, started)
                    .await;
            }
            RoomAction::RetireJoinPrompt {
                room_channel_id,
                message_id,
            } => {
                self.dispatch_retire(
                    action,
                    MessageRef {
                        channel_id: room_channel_id,
                        message_id,
                    },
                    now_ms,
                    started,
                )
                .await;
            }
            _ => {
                self.queue.mark_succeeded(&action);
            }
        }
    }

    /// Post the owner's prompt in the room's own chat.
    async fn dispatch_ask_owner(
        &mut self,
        action: QueuedAction,
        room: Snowflake,
        member: Snowflake,
        request_id: u64,
        now_ms: u64,
        started: Instant,
    ) {
        // Answered, withdrawn or forgotten while this waited: post nothing.
        let still_pending = self.privacy.get(&room).is_some_and(|state| {
            state
                .pending
                .get(&MemberId(member))
                .is_some_and(|request| request.id.0 == request_id)
        });
        let Some(owner) = self.rooms.get(&room).map(|room| room.owner_id) else {
            self.queue.mark_succeeded(&action);
            return;
        };
        if !still_pending {
            self.queue.mark_succeeded(&action);
            return;
        }
        let (content, components) = prompt_message(owner, member, room, request_id);
        let result = self
            .http
            .send_component_message(room, &content, Some(owner), &components)
            .await;
        match result {
            Ok(message) => {
                self.join.prompts.insert((room, request_id), message);
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
                // The post may have landed. A retry can only add a second set
                // of buttons for the same request, which answers once.
                self.mark_failed_observed(
                    action,
                    "Discord join prompt outcome unknown".to_owned(),
                    elapsed_ms(now_ms, started),
                );
            }
            Err(error) => {
                // The owner cannot be asked (the bot cannot post in the room's
                // chat): do not leave the request pending behind buttons nobody
                // got. The failure shows in `/setup`; the member can ask again
                // by coming back.
                self.drop_unreachable_request(room, request_id);
                self.complete_error(action, room, error);
            }
        }
    }

    /// Forget a request whose owner could not be reached, so it does not sit
    /// pending behind buttons that were never delivered.
    fn drop_unreachable_request(&mut self, room: Snowflake, request_id: u64) {
        let Some(state) = self.privacy.get(&room).cloned() else {
            return;
        };
        if let Ok(plan) = state.decide(state.owner_id, RequestId(request_id), JoinDecision::Deny) {
            self.privacy.insert(room, plan.room);
        }
    }

    /// Give an approved member Connect on the room only, then move them in
    /// from the Join channel when they are still there.
    async fn dispatch_approve(
        &mut self,
        action: QueuedAction,
        room: Snowflake,
        member: Snowflake,
        now_ms: u64,
        started: Instant,
    ) {
        let granted = self
            .privacy
            .get(&room)
            .is_some_and(|state| state.private && state.granted.contains(&MemberId(member)));
        if !self.rooms.contains_key(&room) || !granted {
            // Gone, made public meanwhile or never granted: nothing to write.
            self.queue.mark_succeeded(&action);
            return;
        }
        let existing = self
            .live_overwrites(room)
            .unwrap_or_default()
            .into_iter()
            .find(|overwrite| {
                overwrite.kind == PermissionOverwriteType::Member && overwrite.id.get() == member
            });
        let (allow, deny) = existing
            .map_or((Permissions::empty(), Permissions::empty()), |entry| {
                (entry.allow, entry.deny)
            });
        if deny.contains(Permissions::CONNECT) {
            // A vote removed this member after the click: leave the deny alone.
            self.unwind_grant(room, member);
            self.queue.mark_succeeded(&action);
            return;
        }
        let grant = PermissionOverwrite {
            allow: (allow | Permissions::CONNECT) & !Permissions::MANAGE_ROLES,
            deny,
            id: Id::new(member),
            kind: PermissionOverwriteType::Member,
        };
        let mut grant_landed = false;
        let result = match self
            .http
            .put_overwrite(room, grant, self.live.room_guard(room))
            .await
        {
            Ok(()) => {
                grant_landed = true;
                self.note_overwrite(room, &grant);
                self.move_in_from_join(room, member).await
            }
            Err(error) => Err(error),
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
                // The grant is a whole-entry PUT and the move is idempotent.
                self.mark_failed_observed(
                    action,
                    "Discord join approval outcome unknown".to_owned(),
                    elapsed_ms(now_ms, started),
                );
            }
            Err(error) => {
                // A grant that never landed is not claimed, so the member can
                // ask again. One that landed stays recorded even when the
                // move failed (a full room, say): they hold the access and
                // `/public` must still take it back.
                if !grant_landed {
                    self.unwind_grant(room, member);
                }
                self.complete_error(action, room, error);
            }
        }
    }

    /// Move the member into the room, but only out of this room's Join
    /// channel: a member who went elsewhere is never pulled away from it.
    async fn move_in_from_join(
        &mut self,
        room: Snowflake,
        member: Snowflake,
    ) -> Result<(), RoomHttpError> {
        let Some(join) = self.join_channel_of(room) else {
            return Ok(());
        };
        let in_join = self
            .live
            .inner
            .read()
            .expect("live voice lock")
            .members
            .get(&member)
            .is_some_and(|state| state.channel_id == Some(join));
        if !in_join {
            return Ok(());
        }
        match self
            .http
            .move_member(
                self.live.guild_id,
                member,
                room,
                self.live.member_in_room_guard(join, member),
            )
            .await
        {
            // Left the Join channel since the grant landed, or is not in voice
            // at all: they hold the access and can connect themselves.
            Ok(())
            | Err(RoomHttpError::Cancelled | RoomHttpError::NotFound)
            | Err(RoomHttpError::Rejected { code: 40032, .. }) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Stop recording an approval that Discord does not hold.
    fn unwind_grant(&mut self, room: Snowflake, member: Snowflake) {
        if let Some(state) = self.privacy.get_mut(&room) {
            state.granted.remove(&MemberId(member));
        }
    }

    /// Take an approved member's Connect allow back after `/public`.
    async fn dispatch_revoke(
        &mut self,
        action: QueuedAction,
        room: Snowflake,
        member: Snowflake,
        now_ms: u64,
        started: Instant,
    ) {
        // Approved again since (the room went private again): keep the access.
        let regranted = self
            .privacy
            .get(&room)
            .is_some_and(|state| state.granted.contains(&MemberId(member)));
        if !self.rooms.contains_key(&room) || regranted {
            self.queue.mark_succeeded(&action);
            return;
        }
        let existing = self
            .live_overwrites(room)
            .unwrap_or_default()
            .into_iter()
            .find(|overwrite| {
                overwrite.kind == PermissionOverwriteType::Member && overwrite.id.get() == member
            });
        let Some(existing) = existing else {
            self.queue.mark_succeeded(&action);
            return;
        };
        let remaining = existing.allow & !Permissions::CONNECT;
        let result = if remaining.is_empty() && existing.deny.is_empty() {
            self.http
                .delete_overwrite(room, member, self.live.room_guard(room))
                .await
                .map(|()| self.forget_overwrite(room, member))
        } else {
            let kept = PermissionOverwrite {
                allow: remaining,
                ..existing
            };
            self.http
                .put_overwrite(room, kept, self.live.room_guard(room))
                .await
                .map(|()| self.note_overwrite(room, &kept))
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
                self.mark_failed_observed(
                    action,
                    "Discord join revoke outcome unknown".to_owned(),
                    elapsed_ms(now_ms, started),
                );
            }
            Err(error) => self.complete_error(action, room, error),
        }
    }

    /// Drop a deleted overwrite from the live snapshot, so the next decision
    /// reads what Discord now holds without waiting for the gateway echo.
    fn forget_overwrite(&self, room: Snowflake, member: Snowflake) {
        let mut live = self.live.inner.write().expect("live voice lock");
        if let Some(channel) = live.channels.get_mut(&room) {
            if let Some(overwrites) = channel.permission_overwrites.as_mut() {
                overwrites.retain(|entry| {
                    !(entry.kind == PermissionOverwriteType::Member && entry.id.get() == member)
                });
            }
        }
    }

    /// Take the buttons off a prompt whose request can no longer be answered.
    /// Cosmetic: a stale press is refused either way, so only a refused
    /// credential is worth recording.
    async fn dispatch_retire(
        &mut self,
        action: QueuedAction,
        message: MessageRef,
        now_ms: u64,
        started: Instant,
    ) {
        match self
            .http
            .edit_component_message(message, NOT_PENDING, &[])
            .await
        {
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
                    "Discord join prompt edit outcome unknown".to_owned(),
                    elapsed_ms(now_ms, started),
                );
            }
            Err(RoomHttpError::Unauthorized) => {
                self.complete_error(action, message.channel_id, RoomHttpError::Unauthorized);
            }
            Ok(()) | Err(_) => {
                self.queue.mark_succeeded(&action);
            }
        }
    }
}
