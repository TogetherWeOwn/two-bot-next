//! V3 `/limit` and `/unlimit` through the room worker (spec
//! `docs/voice-rooms.md` §V3).
//!
//! The pure decisions live in `two_bot_core::voice_room_controls`
//! (`parse_limit`, `unlimit`) and the owner gate is
//! `voice_ownership::require_room_owner`, the check `/transfer` uses. This
//! module is the runtime half: it resolves the caller's current room from
//! worker state, decides, queues one [`RoomAction::SetUserLimit`] through the
//! guild's ordered queue (429 retry-after and dead-lettering included) and
//! writes the ephemeral reply. It is a child of `voice_rooms` so it reaches the
//! worker's private state without widening it.

use super::*;
use two_bot_core::{
    voice_ownership::require_room_owner,
    voice_room_controls::{parse_limit, unlimit, LimitError, RoomLimit, MAX_ROOM_LIMIT},
};

/// How long a `/limit` reply waits for the queued write to finish. Discord
/// wants the interaction answered within three seconds, so a write that is
/// still waiting on the queue or a rate limit is reported as queued instead.
const LIMIT_ACK_WAIT: Duration = Duration::from_millis(2_000);

/// The `/limit` count as the interaction carried it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitArg {
    /// No `count` option: lock the room at its current headcount.
    Absent,
    /// A numeric `count`. Range checking belongs to the pure core.
    Value(i64),
    /// The option arrived with the wrong type. Refused, never read as "lock".
    Malformed,
}

/// One V3 limit command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitCommand {
    Limit(LimitArg),
    Unlimit,
}

impl LimitCommand {
    /// The slash-command name per-command role restrictions are keyed on.
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Limit(_) => "limit",
            Self::Unlimit => "unlimit",
        }
    }
}

/// The worker's answer to one limit command.
pub(super) enum LimitReply {
    /// Refused or answered without a Discord write.
    Done(String),
    /// The write is queued; `ack` resolves when it finishes.
    Queued {
        limit: RoomLimit,
        ack: oneshot::Receiver<LimitAck>,
    },
}

/// How one queued limit write ended, as far as the caller's reply goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LimitAck {
    Applied,
    /// A 429 or an unknown outcome: the write stays queued and retries.
    Delayed,
    /// A terminal Discord error. The write is dead-lettered, not retried.
    Refused(RoomHttpError),
    /// The room is gone, so there is nothing left to change.
    Obsolete,
}

/// Read the optional `count` option of `/limit`.
pub(super) fn parse_limit_arg(options: &[CommandDataOption]) -> LimitArg {
    match options
        .iter()
        .find(|option| option.name == "count")
        .map(|option| &option.value)
    {
        None => LimitArg::Absent,
        Some(CommandOptionValue::Integer(count)) => LimitArg::Value(*count),
        Some(_) => LimitArg::Malformed,
    }
}

fn range_text() -> String {
    format!(
        "The limit must be a whole number from 0 to {MAX_ROOM_LIMIT} (0 means no limit). \
         Nothing was changed."
    )
}

/// Decide the new limit from the command and the room's human headcount.
/// An `Err` is the ephemeral refusal text.
pub(super) fn decide_limit(command: LimitCommand, headcount: u32) -> Result<RoomLimit, String> {
    let arg = match command {
        LimitCommand::Unlimit => return Ok(unlimit()),
        LimitCommand::Limit(LimitArg::Absent) => None,
        // A negative or enormous count cannot be a limit: park it above the
        // range so the core refuses it.
        LimitCommand::Limit(LimitArg::Value(count)) => {
            Some(u32::try_from(count).unwrap_or(u32::MAX))
        }
        LimitCommand::Limit(LimitArg::Malformed) => return Err(range_text()),
    };
    parse_limit(arg, headcount).map_err(|_: LimitError| range_text())
}

fn people(count: u32) -> &'static str {
    if count == 1 {
        "person"
    } else {
        "people"
    }
}

fn applied_text(command: LimitCommand, limit: RoomLimit) -> String {
    match (command, limit) {
        (_, RoomLimit::Unlimited) => "This room no longer has a user limit.".to_owned(),
        (LimitCommand::Limit(LimitArg::Absent), RoomLimit::Limited(count)) => format!(
            "Locked this room at {count} {}. Use /unlimit to open it up again.",
            people(count)
        ),
        (_, RoomLimit::Limited(count)) => {
            format!("This room is now limited to {count} {}.", people(count))
        }
    }
}

fn queued_text(limit: RoomLimit) -> String {
    match limit {
        RoomLimit::Unlimited => {
            "Removing the room's user limit. Discord is busy, so it will apply shortly.".to_owned()
        }
        RoomLimit::Limited(count) => {
            format!("Setting the room limit to {count}. Discord is busy, so it will apply shortly.")
        }
    }
}

fn refused_text(error: RoomHttpError) -> String {
    match error {
        RoomHttpError::AccessDenied => "I couldn't change the room limit: I need Manage Channels \
            on this room. Tell an admin to fix my permissions. Nothing was changed."
            .to_owned(),
        RoomHttpError::Unauthorized => "Voice rooms are paused: Discord refused the bot \
            credential. Tell an admin to fix the token, then restart the bot."
            .to_owned(),
        RoomHttpError::NotFound => "That room no longer exists, so nothing was changed.".to_owned(),
        _ => "Discord refused the limit change. Nothing was changed; try again.".to_owned(),
    }
}

/// Owner-gate refusal text. `NotOwner` names the limit commands; everything
/// else is the shared V2 wording.
fn limit_refusal(error: OwnershipError) -> String {
    match error {
        OwnershipError::NotOwner => {
            "Only the room owner or an admin can change this room's limit.".to_owned()
        }
        other => ownership_refusal(other),
    }
}

/// Answer the caller's side of an ack that never came, or came late.
pub(super) fn outcome_text(
    command: LimitCommand,
    limit: RoomLimit,
    ack: Option<LimitAck>,
) -> String {
    match ack {
        Some(LimitAck::Applied) => applied_text(command, limit),
        Some(LimitAck::Delayed) | None => queued_text(limit),
        Some(LimitAck::Refused(error)) => refused_text(error),
        Some(LimitAck::Obsolete) => {
            "That room is gone or has changed, so nothing was changed.".to_owned()
        }
    }
}

fn tell(ack: Option<oneshot::Sender<LimitAck>>, outcome: LimitAck) {
    if let Some(ack) = ack {
        let _ = ack.send(outcome);
    }
}

impl<S: RoomPersistence, H: RoomWrites> GuildRoomWorker<S, H> {
    /// Resolve, authorize, decide and queue one `/limit` or `/unlimit`. Acts on
    /// the caller's current room only: an admin may use it in any managed room
    /// they are standing in. Refusals change nothing and queue nothing.
    pub(super) fn apply_limit(
        &mut self,
        actor_id: Snowflake,
        is_admin: bool,
        command: LimitCommand,
    ) -> LimitReply {
        let verb = command.name();
        if self.halted {
            return LimitReply::Done(
                "Voice rooms are paused: Discord refused the bot credential. \
                 Fix the token, then restart the bot."
                    .to_owned(),
            );
        }
        let (channel, headcount, writable) = {
            let live = self.live.inner.read().expect("live voice lock");
            if !live.ready {
                return LimitReply::Done(
                    "The voice worker isn't warmed up yet — try again in a moment.".to_owned(),
                );
            }
            let Some(channel) = live
                .members
                .get(&actor_id)
                .and_then(|member| member.channel_id)
            else {
                return LimitReply::Done(format!("You need to be in a voice room to use /{verb}."));
            };
            let Some(room) = self.rooms.get(&channel) else {
                return LimitReply::Done(
                    "That voice channel isn't a temporary room I manage.".to_owned(),
                );
            };
            let ownership = RoomOwnership {
                owner_id: room.owner_id,
                original_creator_id: room.original_creator_id,
            };
            let occupants = live.ownership_snapshot(channel);
            let actor = RoomActor {
                member_id: actor_id,
                is_admin,
            };
            if let Err(error) = require_room_owner(ownership, &occupants, actor) {
                return LimitReply::Done(limit_refusal(error));
            }
            (
                channel,
                u32::try_from(live.humans(channel)).unwrap_or(u32::MAX),
                can_manage_room(live.permissions(self.live.guild_id, channel)),
            )
        };
        let limit = match decide_limit(command, headcount) {
            Ok(limit) => limit,
            Err(text) => return LimitReply::Done(text),
        };
        if !writable {
            return LimitReply::Done(refused_text(RoomHttpError::AccessDenied));
        }
        // Answers whose caller gave up are closed; drop their senders so the
        // map only holds writes somebody is still waiting on.
        self.limit_acks.retain(|_, ack| !ack.is_closed());
        let (ack, receiver) = oneshot::channel();
        let id = self.queue.enqueue(
            self.live.guild_id,
            RoomAction::SetUserLimit {
                channel_id: channel,
                user_limit: limit.user_limit(),
            },
        );
        self.limit_acks.insert(id, ack);
        LimitReply::Queued {
            limit,
            ack: receiver,
        }
    }

    /// Execute one queued [`RoomAction::SetUserLimit`]. The write is
    /// idempotent: a retry after an unknown outcome, or a limit Discord already
    /// shows, is a success. A room that is gone or whose Manage Channels was
    /// lost since the command was accepted writes nothing.
    pub(super) async fn dispatch_set_user_limit(
        &mut self,
        action: QueuedAction,
        channel_id: Snowflake,
        user_limit: u32,
        now_ms: u64,
        started: Instant,
    ) {
        let ack = self.limit_acks.remove(&action.id);
        let state = {
            let live = self.live.inner.read().expect("live voice lock");
            match (
                self.rooms.contains_key(&channel_id),
                live.channels.get(&channel_id),
            ) {
                (true, Some(channel)) => Some((
                    channel.user_limit.unwrap_or(0),
                    can_manage_room(live.permissions(self.live.guild_id, channel_id)),
                )),
                _ => None,
            }
        };
        let Some((current, permitted)) = state else {
            self.queue.mark_succeeded(&action);
            tell(ack, LimitAck::Obsolete);
            return;
        };
        if !permitted {
            self.complete_error(action, channel_id, RoomHttpError::AccessDenied);
            tell(ack, LimitAck::Refused(RoomHttpError::AccessDenied));
            return;
        }
        if current == user_limit {
            self.queue.mark_succeeded(&action);
            tell(ack, LimitAck::Applied);
            return;
        }
        match self
            .http
            .set_user_limit(channel_id, user_limit, self.live.room_guard(channel_id))
            .await
        {
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
                    channel.user_limit = Some(user_limit);
                }
                tell(ack, LimitAck::Applied);
            }
            Err(RoomHttpError::RateLimited { retry_after_ms, .. }) => {
                self.queue.mark_rate_limited(
                    self.live.guild_id,
                    retry_after_ms,
                    elapsed_ms(now_ms, started),
                    action,
                );
                tell(ack, LimitAck::Delayed);
            }
            Err(RoomHttpError::UnknownOutcome) => {
                self.mark_failed_observed(
                    action,
                    "Discord limit outcome unknown".to_owned(),
                    elapsed_ms(now_ms, started),
                );
                tell(ack, LimitAck::Delayed);
            }
            Err(RoomHttpError::Cancelled) => {
                // The room channel vanished while the write was in flight.
                self.queue.mark_succeeded(&action);
                tell(ack, LimitAck::Obsolete);
            }
            Err(error) => {
                self.complete_error(action, channel_id, error);
                tell(ack, LimitAck::Refused(error));
            }
        }
    }
}

impl<S, H> VoiceRuntime<S, H>
where
    S: RoomPersistence + Send + 'static,
    H: RoomWrites + Send + 'static,
{
    /// Run one V3 limit command on the live worker and return the ephemeral
    /// reply text. The reply waits briefly for the queued Discord write so a
    /// refusal is reported as one; a write still pending is reported as
    /// queued. `None` when the guild has no live actor yet.
    pub async fn run_limit(
        &self,
        guild: Snowflake,
        actor_id: Snowflake,
        is_admin: bool,
        command: LimitCommand,
    ) -> Option<String> {
        let actor = self.live_actor(guild)?;
        let (reply, inbox) = oneshot::channel();
        actor
            .tx
            .send(ActorCommand::Limit {
                actor_id,
                is_admin,
                command,
                reply,
            })
            .ok()?;
        match inbox.await.ok()? {
            LimitReply::Done(text) => Some(text),
            LimitReply::Queued { limit, ack } => {
                let outcome = match tokio::time::timeout(LIMIT_ACK_WAIT, ack).await {
                    Ok(Ok(outcome)) => Some(outcome),
                    // The worker dropped the write unanswered (it halted, or
                    // the room was forgotten): we cannot say it landed.
                    Ok(Err(_)) => {
                        return Some(
                            "I couldn't confirm the change. Check the room's limit, then try \
                             again."
                                .to_owned(),
                        )
                    }
                    Err(_) => None,
                };
                Some(outcome_text(command, limit, outcome))
            }
        }
    }
}

/// Handle one `/limit` or `/unlimit` interaction: identify the caller, run it
/// on the worker and reply once, ephemerally. `is_admin` is the invoker's
/// guild-level authority from the access check. Always `true`: the command is
/// ours even when it is refused.
pub(super) async fn handle_limit<S, H, F>(
    runtime: &VoiceRuntime<S, H>,
    interaction: &Interaction,
    guild_id: Snowflake,
    is_admin: bool,
    command: LimitCommand,
    reply: impl FnOnce(InteractionResponse) -> F + Send,
) -> bool
where
    S: RoomPersistence + Send + 'static,
    H: RoomWrites + Send + 'static,
    F: Future<Output = ()> + Send,
{
    let Some(actor_id) = invoker_member_id(interaction) else {
        reply(ephemeral_response(&format!(
            "I couldn't tell who invoked /{} — try again.",
            command.name()
        )))
        .await;
        return true;
    };
    let text = runtime
        .run_limit(guild_id, actor_id, is_admin, command)
        .await
        .unwrap_or_else(|| {
            "The voice worker isn't warmed up yet — try again in a moment.".to_owned()
        });
    reply(ephemeral_response(&text)).await;
    true
}
