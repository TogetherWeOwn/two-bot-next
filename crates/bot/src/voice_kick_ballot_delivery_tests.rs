#![cfg(test)]

use super::*;
use twilight_model::channel::Message;
use two_bot_core::voice_vote_kick::{VOTE_KICK_COOLDOWN_MS, VOTE_KICK_TTL_MS};

const ROOM: u64 = 500;
const VOTE: u64 = 7_000;
const TARGET: u64 = 303;
const MESSAGE: MessageRef = MessageRef {
    channel_id: 900,
    message_id: 901,
};

type Worker = GuildRoomWorker<Store, Http>;

fn ballot_message(message_id: u64, channel_id: u64) -> Message {
    // Twilight's `Message.interaction` is deprecated, so the struct-literal
    // form needs a waiver the floor guard forbids on new lines. Build
    // through JSON like the sibling `channel`/`user` fixtures instead; the
    // author is pinned to the shared `user(77)` fixture by construction.
    let mut message: Message = serde_json::from_value(serde_json::json!({
        "id": message_id.to_string(),
        "channel_id": channel_id.to_string(),
        "guild_id": GUILD.to_string(),
        "author": {
            "id": "77", "username": "fixture",
            "discriminator": "0001", "avatar": null,
        },
        "content": "ballot",
        "timestamp": "2026-09-30T12:00:00.000+00:00",
        "tts": false,
        "mention_everyone": false,
        "mentions": [],
        "mention_roles": [],
        "attachments": [],
        "embeds": [],
        "pinned": false,
        "flags": 0,
        "type": 0,
    }))
    .expect("fixture message");
    message.author = user(77);
    message
}

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
            backpressure: ActorInboxBackpressure::default(),
        },
    );
    assert!(runtime.kick_message(GUILD, VOTE, MESSAGE));
    assert!(
        matches!(rx.recv().await, Some(ActorCommand::KickMessage { vote_id: VOTE, message }) if message == MESSAGE)
    );
    drop(rx);
    assert!(!runtime.kick_message(GUILD, VOTE, MESSAGE));
}

/// Hold a real actor's receiver without draining it so gate occupancy is
/// synchronous and independent of scheduler timing.
fn held_ballot_actor() -> (
    VoiceRuntime<Store, Http>,
    GuildActor,
    mpsc::UnboundedReceiver<ActorCommand>,
) {
    let runtime = test_runtime(Trace::default());
    let (tx, rx) = mpsc::unbounded_channel();
    runtime.actors.lock().unwrap().insert(
        GUILD,
        GuildActor {
            live: LiveGuild::new(GUILD),
            tx,
            backpressure: ActorInboxBackpressure::default(),
        },
    );
    let actor = runtime.ensure_actor(GUILD).expect("registered actor");
    (runtime, actor, rx)
}

#[tokio::test(flavor = "current_thread")]
async fn ballot_delivery_full_inbox_refuses_runtime_binding_once_per_drop() {
    // Mutation: sending KickMessage directly on tx bypasses the gate and
    // reports success, despite retaining another fire-and-forget entry.
    let (runtime, actor, rx) = held_ballot_actor();
    for member in 0..VOICE_ACTOR_INBOX_CAP {
        assert!(actor.try_send_command(ActorCommand::Display {
            member_id: MEMBER + member as u64,
            display: "fixture".to_owned(),
        }));
    }
    assert_eq!(actor.inbox_depth(), VOICE_ACTOR_INBOX_CAP);
    assert_eq!(rx.len(), VOICE_ACTOR_INBOX_CAP);
    assert_eq!(actor.inbox_overflow_dropped(), 0);
    for refusal in 1..=3 {
        assert!(
            !runtime.kick_message(GUILD, VOTE, MESSAGE),
            "a callback message reference has no reply to preserve and must refuse at capacity"
        );
        assert_eq!(actor.inbox_overflow_dropped(), refusal);
        assert_eq!(actor.inbox_depth(), VOICE_ACTOR_INBOX_CAP);
        assert_eq!(rx.len(), VOICE_ACTOR_INBOX_CAP, "refusal sends nothing");
    }
    runtime.remove_guild(GUILD);
}

#[tokio::test]
async fn ballot_delivery_under_cap_reaches_worker_without_a_reply_or_callback_token() {
    let (runtime, actor, mut rx) = held_ballot_actor();
    assert!(runtime.kick_message(GUILD, VOTE, MESSAGE));
    assert_eq!(actor.inbox_depth(), 1);
    assert_eq!(actor.inbox_overflow_dropped(), 0);
    let command = rx.recv().await.expect("accepted binding");
    // Exhaustive fields: this state message retains only the public reference,
    // not an interaction, callback token or reply sender.
    let ActorCommand::KickMessage { vote_id, message } = command else {
        panic!("accepted binding must reach the actor as KickMessage")
    };
    assert_eq!(vote_id, VOTE);
    assert_eq!(message, MESSAGE);
    let command = ActorCommand::KickMessage { vote_id, message };
    actor.backpressure.release_for(&command);
    assert_eq!(actor.inbox_depth(), 0);
    let (mut worker, trace) = setup().await;
    apply_command(&mut worker, command, 0, &actor.backpressure);
    worker.kick_refresh(VOTE_KICK_TTL_MS);
    assert!(worker.dispatch_one(VOTE_KICK_TTL_MS + 1).await);
    assert_eq!(
        *trace.lock().unwrap(),
        ["edit_prompt:900:901:Vote expired with too few Yes votes.:1"]
    );
    assert!(!worker.dispatch_one(VOTE_KICK_TTL_MS + 2).await);
    runtime.remove_guild(GUILD);
}

#[tokio::test]
async fn ballot_delivery_under_cap_survives_initialization_backoff_and_failure_purge() {
    // Mutation: classifying this fire-and-forget reference as reply-bearing
    // drops it during initialization, leaving no message to settle later.
    let (runtime, actor, mut rx) = held_ballot_actor();
    assert!(runtime.kick_message(GUILD, VOTE, MESSAGE));
    let command = rx.recv().await.expect("accepted binding");
    actor.backpressure.release_for(&command);
    let mut buffered = VecDeque::new();
    buffer_init_command(&mut buffered, command);
    assert_eq!(buffered.len(), 1, "binding must wait for initialization");
    purge_unanswerable_buffered(&mut buffered);
    assert_eq!(buffered.len(), 1, "binding has no reply sender to purge");
    let (mut worker, trace) = setup().await;
    apply_command(
        &mut worker,
        buffered.pop_front().expect("retained binding"),
        0,
        &actor.backpressure,
    );
    worker.kick_refresh(VOTE_KICK_TTL_MS);
    assert!(worker.dispatch_one(VOTE_KICK_TTL_MS + 1).await);
    assert_eq!(
        *trace.lock().unwrap(),
        ["edit_prompt:900:901:Vote expired with too few Yes votes.:1"]
    );
    assert_eq!(actor.inbox_depth(), 0);
    assert_eq!(actor.inbox_overflow_dropped(), 0);
    runtime.remove_guild(GUILD);
}

#[tokio::test]
async fn ballot_delivery_closed_actor_is_not_an_overflow() {
    let (runtime, actor, rx) = held_ballot_actor();
    drop(rx);
    assert!(!runtime.kick_message(GUILD, VOTE, MESSAGE));
    assert_eq!(actor.inbox_depth(), 0, "failed send releases its slot");
    assert_eq!(actor.inbox_overflow_dropped(), 0);
    runtime.remove_guild(GUILD);
}

#[test]
fn ballot_delivery_overflow_metrics_count_refusal_sources_only() {
    // The production sources use the process registry. Run this one test in
    // its own process so exact deltas cannot be supplied by parallel tests.
    const CHILD: &str = "TWO_TEST_VOICE_OVERFLOW_METRICS_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let module = module_path!().split_once("::").expect("test module").1;
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                &format!("{module}::ballot_delivery_overflow_metrics_count_refusal_sources_only"),
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .expect("run isolated overflow source test");
        assert!(
            output.status.success(),
            "isolated source test failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "the exact filter must execute the source test, not zero tests"
        );
        return;
    }
    fn counter(name: &str) -> u64 {
        let text = metrics::global().render(None);
        let prefix = format!("{name} ");
        text.lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .expect("refusal source counter must be rendered without labels")
            .parse()
            .expect("integer counter sample")
    }
    const INBOX: &str = "two_bot_voice_inbox_overflow_dropped_total";
    const QUEUE: &str = "two_bot_voice_queue_overflow_dropped_total";
    assert_eq!(counter(INBOX), 0);
    assert_eq!(counter(QUEUE), 0);
    let (runtime, actor, mut rx) = held_ballot_actor();
    assert!(runtime.kick_message(GUILD, VOTE, MESSAGE));
    let command = rx.try_recv().expect("healthy binding");
    actor.backpressure.release_for(&command);
    assert_eq!(counter(INBOX), 0, "accepted binding is not overflow");
    drop(rx);
    assert!(!runtime.kick_message(GUILD, VOTE, MESSAGE));
    assert_eq!(actor.inbox_depth(), 0);
    assert_eq!(counter(INBOX), 0, "closed receiver is not overflow");
    runtime.remove_guild(GUILD);

    let (runtime, actor, rx) = held_ballot_actor();
    for member in 0..VOICE_ACTOR_INBOX_CAP {
        assert!(actor.try_send_command(ActorCommand::Display {
            member_id: MEMBER + member as u64,
            display: "fixture".to_owned(),
        }));
    }
    assert_eq!(counter(INBOX), 0, "accepted sends do not count");
    for refusal in 1..=3 {
        assert!(!runtime.kick_message(GUILD, VOTE, MESSAGE));
        assert_eq!(actor.inbox_overflow_dropped(), refusal);
        assert_eq!(counter(INBOX), refusal, "one sample per acquire refusal");
        assert_eq!(counter(QUEUE), 0, "inbox refusals leave queue count alone");
        assert_eq!(actor.inbox_depth(), VOICE_ACTOR_INBOX_CAP);
        assert_eq!(rx.len(), VOICE_ACTOR_INBOX_CAP);
    }
    runtime.remove_guild(GUILD);

    let queue = ActionQueue::new();
    let rename_id = queue.try_enqueue(
        GUILD,
        RoomAction::RenameRoom {
            channel_id: ROOM,
            name: "before".to_owned(),
        },
    );
    let respace_id = queue.try_enqueue(
        GUILD,
        RoomAction::RespaceChannels {
            room_channel_id: ROOM,
        },
    );
    assert!(rename_id.is_some());
    assert!(respace_id.is_some());
    for index in 2..two_bot_core::voice_rooms::MAX_QUEUE_PENDING_PER_GUILD {
        assert!(queue
            .try_enqueue(
                GUILD,
                RoomAction::DeleteRoom {
                    channel_id: 10_000 + index as u64
                }
            )
            .is_some());
    }
    assert_eq!(counter(QUEUE), 0, "accepted enqueues do not count");
    assert_eq!(
        queue.try_enqueue(
            GUILD,
            RoomAction::RenameRoom {
                channel_id: ROOM,
                name: "latest".to_owned()
            }
        ),
        rename_id
    );
    assert_eq!(
        queue.try_enqueue(
            GUILD,
            RoomAction::RespaceChannels {
                room_channel_id: ROOM
            }
        ),
        respace_id
    );
    assert_eq!(counter(QUEUE), 0, "coalescing at capacity is not refusal");
    for refusal in 1..=3 {
        assert_eq!(
            queue.try_enqueue(
                GUILD,
                RoomAction::DeleteRoom {
                    channel_id: 20_000 + refusal
                }
            ),
            None
        );
        assert_eq!(queue.overflow_dropped(), refusal);
        assert_eq!(
            counter(QUEUE),
            refusal,
            "one sample per try_enqueue refusal"
        );
        assert_eq!(counter(INBOX), 3, "queue refusals leave inbox count alone");
    }
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
async fn ballot_delivery_terminal_edit_errors_do_not_retry() {
    for error in [
        RoomHttpError::AccessDenied,
        RoomHttpError::Rejected {
            status: 400,
            code: 50035,
        },
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

#[tokio::test]
async fn ballot_delivery_late_bind_after_timer_expiry_requeues_terminal_edit() {
    let (mut worker, trace) = setup().await;
    worker.bind_kick_message(VOTE, MESSAGE);
    worker.kick_refresh(VOTE_KICK_TTL_MS);
    assert!(worker.dispatch_one(VOTE_KICK_TTL_MS + 1).await);
    assert!(!worker.dispatch_one(VOTE_KICK_TTL_MS + 2).await);
    // A type-7 callback rendered before the timer settle lands late and
    // restores the live-looking ballot; the post-callback bind repairs it.
    worker.bind_kick_message(VOTE, MESSAGE);
    assert!(worker.dispatch_one(VOTE_KICK_TTL_MS + 3).await);
    let calls = trace.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert!(calls
        .iter()
        .all(|call| call.starts_with("edit_prompt:900:901:") && call.contains("expired")));
    assert!(!worker.dispatch_one(VOTE_KICK_TTL_MS + 4).await);
}

#[tokio::test]
async fn ballot_delivery_late_bind_after_passed_success_requeues_terminal_edit() {
    let (mut worker, trace) = setup().await;
    worker.bind_kick_message(VOTE, MESSAGE);
    // A passed vote whose enforcement succeeded: the same success outcome
    // the enforcement arm reports through `correct_kick_ballot`.
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
    worker.kick_ballots.get_mut(&VOTE).expect("ballot").update = update;
    worker.correct_kick_ballot(VOTE, EnforcementOutcome::ConnectDeniedAndDisconnected);
    worker.enqueue_kick_ballot(update);
    assert!(worker.dispatch_one(1).await);
    assert!(!worker.dispatch_one(2).await);
    worker.bind_kick_message(VOTE, MESSAGE);
    assert!(worker.dispatch_one(3).await);
    let calls = trace.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert!(calls
        .iter()
        .all(|call| call.starts_with("edit_prompt:900:901:") && call.contains("passed")));
    assert!(!worker.dispatch_one(4).await);
}

#[tokio::test]
async fn ballot_delivery_retryable_edit_errors_are_not_lifecycle_failures() {
    for error in [
        RoomHttpError::RateLimited {
            retry_after_ms: 2_000,
            global: false,
        },
        RoomHttpError::UnknownOutcome,
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
        assert!(
            worker.failures().is_empty(),
            "a retried edit is not a failure"
        );
        // The scheduled retry succeeds; still nothing recorded.
        assert!(worker.dispatch_one(VOTE_KICK_TTL_MS + 10_000_000).await);
        assert!(worker.failures().is_empty());
        assert_eq!(worker.http.component_edits.lock().unwrap().len(), 2);
    }
    // A terminal refusal still records exactly one failure and never retries.
    let (mut worker, _) = setup().await;
    worker.bind_kick_message(VOTE, MESSAGE);
    worker.kick_refresh(VOTE_KICK_TTL_MS);
    worker
        .http
        .component_edit_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::AccessDenied);
    assert!(worker.dispatch_one(VOTE_KICK_TTL_MS + 1).await);
    assert_eq!(worker.failures().len(), 1);
    assert!(!worker.dispatch_one(u64::MAX).await);
}

#[tokio::test(start_paused = true)]
async fn ballot_delivery_button_press_binds_message_when_initial_capture_failed() {
    let trace = Trace::default();
    let runtime = kick_room_runtime(
        &trace,
        true,
        vec![
            voice_member_in(KICK_VOTER, KICK_ROOM),
            voice_member_in(302, KICK_ROOM),
            voice_member_in(KICK_TARGET, KICK_ROOM),
        ],
    );
    let capture = CaptureReplies {
        inner: Replies::new(trace.clone()),
        message: Err(RoomHttpError::UnknownOutcome),
    };
    let open = kick_sink_interaction(KICK_TARGET, KICK_VOTER);
    assert!(VoiceResponder::start_kick_vote(&runtime, &capture, &open).await);
    // The component callback carries the public ballot message; the press
    // binds it so later timer edits have a target.
    let callback = ballot_message(901, 900);
    let want = format!("edit_prompt:900:{}:", callback.id.get());
    let mut press = component_interaction(
        &vote_button_id(open.id.get(), VoteBallot::Yes),
        None,
        KICK_VOTER,
    );
    press.message = Some(callback);
    let replies = Replies::new(trace.clone());
    VoiceResponder::respond_with(&runtime, &replies, &press, None, None).await;
    tokio::time::advance(Duration::from_millis(VOTE_KICK_TTL_MS)).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    let calls = trace.lock().unwrap().clone();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with(&want) && call.contains("expired"))
            .count(),
        1,
        "{calls:?}"
    );
}
