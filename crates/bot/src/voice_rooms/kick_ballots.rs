//! In-place ballot delivery, separate from room enforcement. Only message
//! snowflakes survive the interaction callback; tokens are never retained.
use super::*;

pub(super) struct KickBallot {
    pub update: VoteKickUpdate,
    pub outcome: Option<EnforcementOutcome>,
    message: Option<MessageRef>,
    retry: Option<(u32, u64)>,
}

impl KickBallot {
    pub(super) fn new(update: VoteKickUpdate) -> Self {
        Self {
            update,
            outcome: None,
            message: None,
            retry: None,
        }
    }

    fn enqueue(&mut self) {
        self.retry.get_or_insert((0, 0));
    }

    pub(super) fn pending(&self) -> bool {
        self.retry.is_some()
    }

    pub(super) fn response(&self) -> InteractionResponse {
        let content = match self.outcome {
            None | Some(EnforcementOutcome::ConnectDeniedAndDisconnected) => {
                return vote_update_message(&self.update);
            }
            Some(EnforcementOutcome::ConnectDeniedTargetAbsent) =>
                "The target already left the room. Re-entry was blocked for this room. No further action is needed.",
            Some(EnforcementOutcome::SkippedTargetProtected) =>
                "Removal skipped: the target is now protected or their authority could not be verified. No further changes were requested. Ask a moderator to check this member and the room permissions.",
            Some(EnforcementOutcome::SkippedRoomGone) =>
                "Removal skipped: the room is gone or no longer available. No further action is needed for this room.",
            Some(EnforcementOutcome::PermissionMissing) =>
                "Removal could not be completed: the bot lacks permissions. Re-entry may already be blocked. Ask a moderator to check this member and the room permissions.",
            Some(EnforcementOutcome::DiscordError) =>
                "Removal could not be completed: Discord rejected a request. Re-entry may already be blocked. Ask a moderator to check this member and the room permissions.",
            Some(EnforcementOutcome::GaveUp) =>
                "Removal outcome unknown: the bot could not confirm the Discord writes and has stopped retrying. Re-entry may already be blocked. Ask a moderator to check this member and the room permissions before trying again.",
        };
        ballot_response(
            content,
            &self.update,
            self.update.vote.target_id,
            InteractionResponseType::UpdateMessage,
            true,
        )
    }
}

impl<S: RoomPersistence, H: RoomWrites> GuildRoomWorker<S, H> {
    pub(super) fn kick_ballot_response(&self, update: &VoteKickUpdate) -> InteractionResponse {
        self.kick_ballots
            .get(&update.vote.id)
            .map_or_else(|| vote_update_message(update), KickBallot::response)
    }

    /// Called after the callback: a late type-7 response must not overwrite a
    /// timer settlement or enforcement correction that landed while that
    /// callback was in flight. A vote's first message binding is immutable,
    /// even on delayed buttons.
    pub(super) fn bind_kick_message(&mut self, vote_id: Snowflake, message: MessageRef) {
        if message.channel_id == 0 || message.message_id == 0 {
            return;
        }
        let Some(ballot) = self.kick_ballots.get_mut(&vote_id) else {
            return;
        };
        if ballot.message.is_some_and(|previous| previous != message) {
            return;
        }
        ballot.message = Some(message);
        // A callback rendered before a timer settle can restore live buttons:
        // any binding of a settled ballot re-renders the latest state.
        if ballot.update.status != VoteKickStatus::Active
            || ballot
                .outcome
                .is_some_and(|outcome| outcome != EnforcementOutcome::ConnectDeniedAndDisconnected)
        {
            ballot.enqueue();
        }
    }

    pub(super) fn enqueue_kick_ballot(&mut self, update: VoteKickUpdate) {
        self.kick_ballots
            .entry(update.vote.id)
            .or_insert_with(|| KickBallot::new(update))
            .enqueue();
    }

    pub(super) fn correct_kick_ballot(&mut self, vote_id: Snowflake, outcome: EnforcementOutcome) {
        if let Some(ballot) = self.kick_ballots.get_mut(&vote_id) {
            ballot.outcome = Some(outcome);
            if outcome != EnforcementOutcome::ConnectDeniedAndDisconnected {
                ballot.enqueue();
            }
        }
    }

    /// One idempotent PATCH at most, below the room-write lane. No room guard:
    /// the ballot may be in a text channel that outlives the temporary room.
    pub(super) async fn dispatch_kick_ballot(&mut self, now_ms: u64) -> bool {
        let Some(vote_id) = self
            .kick_ballots
            .iter()
            .filter(|(_, ballot)| ballot.retry.is_some_and(|(_, due)| due <= now_ms))
            .map(|(id, _)| *id)
            .min()
        else {
            return false;
        };
        let ballot = self
            .kick_ballots
            .get_mut(&vote_id)
            .expect("selected ballot");
        let (attempts, _) = ballot.retry.take().expect("selected pending edit");
        let Some(message) = ballot.message else {
            return true;
        };
        // Render at dispatch, not enqueue: a correction supersedes a delayed
        // timer pass or a backed-off edit rather than restoring stale copy.
        let response = ballot.response();
        let data = response.data.expect("ballot body");
        let started = Instant::now();
        let result = self
            .http
            .edit_component_message(
                message,
                data.content.as_deref().unwrap_or_default(),
                data.components.as_deref().unwrap_or_default(),
            )
            .await;
        match result {
            Ok(()) | Err(RoomHttpError::NotFound | RoomHttpError::Cancelled) => {}
            Err(error) => {
                // Rate limits are retries, never outcomes: record only the
                // errors that will not be retried.
                let retryable = matches!(
                    error,
                    RoomHttpError::UnknownOutcome | RoomHttpError::RateLimited { .. }
                ) && attempts.saturating_add(1) < QUEUE_MAX_ATTEMPTS;
                if !retryable {
                    self.record(LifecycleFailure::Discord {
                        channel_id: message.channel_id,
                        error,
                    });
                }
                if error == RoomHttpError::Unauthorized {
                    self.halted = true;
                    self.halted_by_discord = true;
                } else if retryable {
                    let delay = match error {
                        RoomHttpError::RateLimited { retry_after_ms, .. } => {
                            retry_after_ms.max(1_000)
                        }
                        _ => 1_000u64 << attempts.min(10),
                    };
                    if let Some(ballot) = self.kick_ballots.get_mut(&vote_id) {
                        ballot.retry = Some((
                            attempts + 1,
                            elapsed_ms(now_ms, started).saturating_add(delay),
                        ));
                    }
                }
            }
        }
        true
    }
}
