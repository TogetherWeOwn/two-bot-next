#![cfg(test)]

use super::*;
use two_bot_core::voice_vote_kick::{VOTE_KICK_COOLDOWN_MS, VOTE_KICK_TTL_MS};

const ROOM: u64 = 500;
const VOTE: u64 = 7_000;
const TARGET: u64 = 303;
const MESSAGE: MessageRef = MessageRef {
    channel_id: 900,
    message_id: 901,
};

type Worker = GuildRoomWorker<Store, Http>;

#[tokio::test]
async fn ballot_delivery_binding_reports_unavailable_or_closed_actor() {
    let runtime = test_runtime(Trace::default());
    assert!(!runtime.kick_message(GUILD, VOTE, MESSAGE));
    let (tx, mut rx) = mpsc::unbounded_channel();
    runtime.actors.lock().unwrap().insert(
        GUILD,
        GuildActor {
            live: LiveGuild::new(GUILD),
            tx,
        },
    );
    assert!(runtime.kick_message(GUILD, VOTE, MESSAGE));
    assert!(
        matches!(rx.recv().await, Some(ActorCommand::KickMessage { vote_id: VOTE, message }) if message == MESSAGE)
    );
    drop(rx);
    assert!(!runtime.kick_message(GUILD, VOTE, MESSAGE));
}

struct CaptureReplies {
    inner: Replies,
    message: Result<Option<MessageRef>, RoomHttpError>,
}

impl InteractionReplies for CaptureReplies {
    async fn defer(&self, interaction: &Interaction) -> Result<(), RoomHttpError> {
        self.inner.defer(interaction).await
    }
    async fn complete(
        &self,
        interaction: &Interaction,
        response: InteractionResponse,
    ) -> Result<(), RoomHttpError> {
        self.inner.complete(interaction, response).await
    }
    async fn respond(
        &self,
        interaction: &Interaction,
        response: InteractionResponse,
    ) -> Result<(), RoomHttpError> {
        self.inner.respond(interaction, response).await
    }
    async fn original_message(&self, _: &Interaction) -> Result<Option<MessageRef>, RoomHttpError> {
        self.inner
            .trace
            .lock()
            .unwrap()
            .push("capture_original".to_owned());
        self.message
    }
}

#[tokio::test(start_paused = true)]
async fn ballot_delivery_responder_captures_original_after_callback_and_timer_uses_it() {
    let trace = Trace::default();
    let runtime = kick_room_runtime(
        &trace,
        true,
        vec![
            voice_member_in(KICK_VOTER, KICK_ROOM),
            voice_member_in(KICK_TARGET, KICK_ROOM),
        ],
    );
    let replies = CaptureReplies {
        inner: Replies::new(trace.clone()),
        message: Ok(Some(MESSAGE)),
    };
    assert!(
        VoiceResponder::start_kick_vote(
            &runtime,
            &replies,
            &kick_sink_interaction(KICK_TARGET, KICK_VOTER)
        )
        .await
    );
    let calls = trace.lock().unwrap().clone();
    assert!(
        calls.iter().position(|call| call == "respond").unwrap()
            < calls
                .iter()
                .position(|call| call == "capture_original")
                .unwrap()
    );
    tokio::time::advance(Duration::from_millis(VOTE_KICK_TTL_MS)).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    let calls = trace.lock().unwrap().clone();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with("edit_prompt:900:901:") && call.contains("expired"))
            .count(),
        1,
        "{calls:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn ballot_delivery_failed_reference_read_does_not_repeat_callback_or_post_a_message() {
    let trace = Trace::default();
    let runtime = kick_room_runtime(
        &trace,
        true,
        vec![
            voice_member_in(KICK_VOTER, KICK_ROOM),
            voice_member_in(KICK_TARGET, KICK_ROOM),
        ],
    );
    let replies = CaptureReplies {
        inner: Replies::new(trace.clone()),
        message: Err(RoomHttpError::UnknownOutcome),
    };
    assert!(
        VoiceResponder::start_kick_vote(
            &runtime,
            &replies,
            &kick_sink_interaction(KICK_TARGET, KICK_VOTER)
        )
        .await
    );
    tokio::time::advance(Duration::from_millis(VOTE_KICK_TTL_MS)).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    let calls = trace.lock().unwrap().clone();
    assert_eq!(calls.iter().filter(|call| *call == "respond").count(), 1);
    assert_eq!(
        calls
            .iter()
            .filter(|call| *call == "capture_original")
            .count(),
        1
    );
    assert!(
        !calls
            .iter()
            .any(|call| call.starts_with("edit_prompt:") || call.starts_with("prompt:")),
        "{calls:?}"
    );
}

async fn setup() -> (Worker, Trace) {
    let trace = Trace::default();
    let live = LiveGuild::new(GUILD);
    let members = [MEMBER, 301, 302, TARGET, 304]
        .into_iter()
        .map(|member_id| VoiceMember {
            member_id,
            channel_id: ROOM,
            bot: Some(false),
        })
        .collect();
    live.publish(snapshot(&[ROOM], members));
    let store = Store::new(trace.clone());
    store.rooms.lock().unwrap().insert(ROOM, room(ROOM));
    let mut worker = GuildRoomWorker::load(live, store, Http::new(trace.clone()))
        .await
        .unwrap();
    worker.kick_start(VOTE, ROOM, 301, TARGET, 0).unwrap();
    (worker, trace)
}

fn pass(worker: &mut Worker) {
    for member in [301, 302, 304] {
        worker.kick_cast(VOTE, member, VoteBallot::Yes, 2).unwrap();
    }
}

#[tokio::test]
async fn ballot_delivery_timer_edits_captured_message_once_with_disabled_buttons() {
    let (mut worker, trace) = setup().await;
    worker.bind_kick_message(VOTE, MESSAGE);
    worker.kick_refresh(VOTE_KICK_TTL_MS);
    assert!(worker.dispatch_one(VOTE_KICK_TTL_MS + 1).await);
    let calls = trace.lock().unwrap().clone();
    assert_eq!(
        calls,
        ["edit_prompt:900:901:Vote expired with too few Yes votes.:1"]
    );
    let edit = worker.http.component_edits.lock().unwrap()[0].clone();
    assert_eq!(edit.message, MESSAGE);
    assert_eq!(edit.mention, None);
    for row in edit.components {
        let Component::ActionRow(row) = row else {
            panic!("button row")
        };
        assert_eq!(row.components.len(), 2);
        for button in row.components {
            assert!(matches!(button, Component::Button(button) if button.disabled));
        }
    }
    assert!(!worker.dispatch_one(VOTE_KICK_TTL_MS + 2).await);
    assert!(worker.kick_refresh(VOTE_KICK_TTL_MS + 3).is_empty());
}

#[tokio::test]
async fn ballot_delivery_latest_enforcement_outcome_supersedes_timer_pass() {
    let (mut worker, trace) = setup().await;
    worker.bind_kick_message(VOTE, MESSAGE);
    for member in [301, 302] {
        worker.kick_cast(VOTE, member, VoteBallot::Yes, 1).unwrap();
    }
    worker.live.voice_update(MEMBER, None, Some(false));
    worker.live.voice_update(304, None, Some(false));
    assert_eq!(worker.kick_refresh(3)[0].status, VoteKickStatus::Passed);
    worker.rooms.get_mut(&ROOM).unwrap().owner_id = TARGET;
    assert!(worker.dispatch_one(4).await);
    assert!(worker.dispatch_one(5).await);
    assert!(!worker.dispatch_one(6).await);
    let calls = trace.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].contains("protected"), "{calls:?}");
    assert!(!calls[0].contains("passed"), "{calls:?}");
    assert!(worker.flush_kick_audit(7).await);
    let rows = worker.store.kick_audit.lock().unwrap();
    let rows: Vec<_> = rows
        .iter()
        .filter(|row| row.event == KickAuditEvent::Enforcement)
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome, "skipped_target_protected");
}

#[tokio::test]
async fn ballot_delivery_unknown_retries_only_correct_after_budget_exhaustion() {
    let (mut worker, trace) = setup().await;
    worker.bind_kick_message(VOTE, MESSAGE);
    pass(&mut worker);
    for _ in 0..QUEUE_MAX_ATTEMPTS {
        worker
            .http
            .kick_errors
            .lock()
            .unwrap()
            .push_back(RoomHttpError::UnknownOutcome);
    }
    for attempt in 1..=QUEUE_MAX_ATTEMPTS {
        assert!(worker.dispatch_one(u64::from(attempt) * 10_000_000).await);
        assert!(!trace
            .lock()
            .unwrap()
            .iter()
            .any(|call| call.starts_with("edit_prompt:")));
    }
    assert!(worker.dispatch_one(100_000_000).await);
    let calls = trace.lock().unwrap().clone();
    let edits: Vec<_> = calls
        .iter()
        .filter(|call| call.starts_with("edit_prompt:"))
        .collect();
    assert_eq!(edits.len(), 1);
    assert!(edits[0].contains("could not confirm"));
    assert!(edits[0].contains("moderator"));
    assert!(!edits[0].contains("passed"));
    assert!(!worker.dispatch_one(100_000_001).await);
}

#[tokio::test]
async fn ballot_delivery_missing_reference_is_quiet_and_late_capture_repairs_it() {
    let (mut worker, trace) = setup().await;
    worker.kick_refresh(VOTE_KICK_TTL_MS);
    assert!(worker.dispatch_one(VOTE_KICK_TTL_MS + 1).await);
    assert!(!worker.dispatch_one(VOTE_KICK_TTL_MS + 2).await);
    assert!(trace.lock().unwrap().is_empty());
    worker.bind_kick_message(VOTE, MESSAGE);
    assert!(worker.dispatch_one(VOTE_KICK_TTL_MS + 3).await);
    assert_eq!(trace.lock().unwrap().len(), 1);
    assert!(!worker.dispatch_one(VOTE_KICK_TTL_MS + 4).await);
}

#[tokio::test]
async fn ballot_delivery_late_callback_cannot_restore_passed_copy_or_change_reference() {
    let (mut worker, trace) = setup().await;
    worker.bind_kick_message(VOTE, MESSAGE);
    pass(&mut worker);
    worker.rooms.remove(&ROOM);
    assert!(worker.dispatch_one(3).await);
    assert!(worker.dispatch_one(4).await);
    let update = worker.kick_ballots[&VOTE].update;
    assert!(!worker
        .kick_ballot_response(&update)
        .data
        .unwrap()
        .content
        .unwrap()
        .contains("passed"));
    // The callback could have been in flight while the correction landed.
    worker.bind_kick_message(VOTE, MESSAGE);
    worker.bind_kick_message(
        VOTE,
        MessageRef {
            channel_id: 999,
            message_id: 999,
        },
    );
    assert!(worker.dispatch_one(5).await);
    let calls = trace.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert!(calls
        .iter()
        .all(|call| call.starts_with("edit_prompt:900:901:") && call.contains("room is gone")));
    assert!(!worker.dispatch_one(6).await);
}

#[test]
fn ballot_delivery_reason_specific_copy_never_promises_success_and_has_bounded_next_action() {
    let update = VoteKickUpdate {
        vote: VoteKickRef {
            id: VOTE,
            guild_id: GUILD,
            room_id: ROOM,
            target_id: TARGET,
        },
        status: VoteKickStatus::Passed,
        progress: VoteProgress {
            yes: 3,
            required: 3,
            total: 4,
        },
        kick: None,
    };
    for (outcome, phrase, next) in [
        (
            EnforcementOutcome::SkippedTargetProtected,
            "protected",
            "moderator",
        ),
        (
            EnforcementOutcome::SkippedRoomGone,
            "room is gone",
            "No further action",
        ),
        (EnforcementOutcome::GaveUp, "could not confirm", "moderator"),
        (
            EnforcementOutcome::PermissionMissing,
            "permissions",
            "moderator",
        ),
        (
            EnforcementOutcome::DiscordError,
            "Discord rejected",
            "moderator",
        ),
        (
            EnforcementOutcome::ConnectDeniedTargetAbsent,
            "already left",
            "No further action",
        ),
    ] {
        let mut ballot = kick_ballots::KickBallot::new(update);
        ballot.outcome = Some(outcome);
        let response = ballot.response();
        assert_eq!(response.kind, InteractionResponseType::UpdateMessage);
        let data = response.data.unwrap();
        let text = data.content.unwrap();
        assert!(
            text.contains(phrase) && text.contains(next),
            "{outcome:?}: {text}"
        );
        assert!(
            !text.contains("passed") && !text.contains("will be disconnected"),
            "{text}"
        );
        assert!(text.len() < 400);
        for row in data.components.unwrap() {
            let Component::ActionRow(row) = row else {
                panic!("button row")
            };
            assert!(row
                .components
                .iter()
                .all(|button| matches!(button, Component::Button(button) if button.disabled)));
        }
    }
}

#[tokio::test]
async fn ballot_delivery_eviction_fences_pending_edit_then_releases_all_state() {
    let (mut worker, _) = setup().await;
    worker.bind_kick_message(VOTE, MESSAGE);
    let after = VOTE_KICK_TTL_MS + VOTE_KICK_COOLDOWN_MS + 1;
    worker.kick_refresh(after);
    assert!(!worker.vote_refs.contains_key(&VOTE));
    assert!(worker.kick_ballots.contains_key(&VOTE));
    assert!(worker.dispatch_one(after + 1).await);
    worker.kick_refresh(after + 2);
    assert!(!worker.kick_ballots.contains_key(&VOTE));
    assert!(!worker.vote_initiators.contains_key(&VOTE));
    worker.bind_kick_message(VOTE, MESSAGE);
    assert!(worker.kick_ballots.is_empty());
}

#[tokio::test]
async fn ballot_delivery_edit_failures_back_off_and_exhaust_a_finite_budget() {
    for error in [
        RoomHttpError::UnknownOutcome,
        RoomHttpError::RateLimited {
            retry_after_ms: 2_000,
            global: false,
        },
    ] {
        let (mut worker, _) = setup().await;
        worker.bind_kick_message(VOTE, MESSAGE);
        worker.kick_refresh(VOTE_KICK_TTL_MS);
        for _ in 0..QUEUE_MAX_ATTEMPTS {
            worker
                .http
                .component_edit_errors
                .lock()
                .unwrap()
                .push_back(error);
        }
        for attempt in 1..=QUEUE_MAX_ATTEMPTS {
            let now = VOTE_KICK_TTL_MS + u64::from(attempt) * 10_000_000;
            assert!(worker.dispatch_one(now).await);
            assert!(
                !worker.dispatch_one(now + 1).await,
                "backoff must prevent a busy retry"
            );
        }
        assert!(!worker.dispatch_one(u64::MAX).await);
        assert_eq!(
            worker.http.component_edits.lock().unwrap().len(),
            QUEUE_MAX_ATTEMPTS as usize
        );
        assert!(!worker.halted());
        assert!(worker.flush_kick_audit(u64::MAX).await);
        assert!(worker
            .store
            .kick_audit
            .lock()
            .unwrap()
            .iter()
            .all(|row| row.event != KickAuditEvent::Enforcement));
    }
}

#[tokio::test]
async fn ballot_delivery_missing_message_and_unauthorized_edits_do_not_retry() {
    for error in [
        RoomHttpError::InvalidRequest,
        RoomHttpError::NotFound,
        RoomHttpError::Cancelled,
        RoomHttpError::Unauthorized,
    ] {
        let (mut worker, _) = setup().await;
        worker.bind_kick_message(VOTE, MESSAGE);
        worker.kick_refresh(VOTE_KICK_TTL_MS);
        worker
            .http
            .component_edit_errors
            .lock()
            .unwrap()
            .push_back(error);
        assert!(worker.dispatch_one(VOTE_KICK_TTL_MS + 1).await);
        assert!(!worker.dispatch_one(u64::MAX).await);
        assert_eq!(worker.http.component_edits.lock().unwrap().len(), 1);
        assert_eq!(
            worker.halted_by_discord(),
            error == RoomHttpError::Unauthorized
        );
    }
}

#[tokio::test]
async fn ballot_delivery_uncertain_attempt_then_protected_skip_does_not_claim_no_changes() {
    let (mut worker, _) = setup().await;
    worker.bind_kick_message(VOTE, MESSAGE);
    pass(&mut worker);
    worker
        .http
        .kick_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::UnknownOutcome);
    assert!(worker.dispatch_one(3).await);
    worker.rooms.get_mut(&ROOM).unwrap().owner_id = TARGET;
    assert!(worker.dispatch_one(10_000_000).await);
    assert!(worker.dispatch_one(10_000_001).await);
    let edits = worker.http.component_edits.lock().unwrap();
    assert_eq!(edits.len(), 1);
    assert!(edits[0].content.contains("protected"));
    assert!(edits[0]
        .content
        .contains("No further changes were requested"));
    assert!(!edits[0].content.contains("Nothing was changed"));
}

#[tokio::test]
async fn ballot_delivery_binding_and_cast_do_not_enqueue_active_edits() {
    let (mut worker, trace) = setup().await;
    worker.bind_kick_message(
        VOTE,
        MessageRef {
            channel_id: 0,
            message_id: 1,
        },
    );
    worker.bind_kick_message(
        VOTE,
        MessageRef {
            channel_id: 1,
            message_id: 0,
        },
    );
    worker.bind_kick_message(VOTE, MESSAGE);
    let update = worker.kick_cast(VOTE, 301, VoteBallot::Yes, 1).unwrap();
    assert_eq!(
        serde_json::to_value(worker.kick_ballot_response(&update)).unwrap(),
        serde_json::to_value(vote_update_message(&update)).unwrap()
    );
    assert!(!worker.dispatch_one(2).await);
    assert!(trace.lock().unwrap().is_empty());
}
