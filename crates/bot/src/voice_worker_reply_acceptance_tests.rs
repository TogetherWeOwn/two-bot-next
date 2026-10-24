//! Protected acceptance: five worker reply waits have the same two-second
//! ceiling as /limit. A loaded but stalled/dropped actor is not a cold cache.
//! The builder must not edit this file. No network or database is contacted.
#![cfg(test)]

use super::*;
use twilight_model::gateway::payload::incoming::InteractionCreate;

const ACK_BOUND: Duration = Duration::from_millis(2_000);
const TEST_GUARD: Duration = Duration::from_millis(2_100);

fn stalled_runtime() -> (
    VoiceRuntime<Store, Http>,
    mpsc::UnboundedReceiver<ActorCommand>,
) {
    let runtime = test_runtime(Trace::default());
    let live = LiveGuild::new(GUILD);
    live.publish(command_snapshot());
    let (tx, rx) = mpsc::unbounded_channel();
    runtime.actors.lock().unwrap().insert(
        GUILD,
        GuildActor {
            live,
            tx,
            backpressure: ActorInboxBackpressure::default(),
        },
    );
    (runtime, rx)
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    let start = tokio::time::Instant::now();
    let result = tokio::time::timeout(TEST_GUARD, future)
        .await
        .expect("accepted worker command must return within the two-second /limit budget");
    assert!(
        start.elapsed() <= ACK_BOUND,
        "worker wait exceeded its documented bound"
    );
    result
}

#[tokio::test(start_paused = true)]
async fn ownership_wait_is_bounded_with_a_live_unanswered_inbox() {
    let (runtime, _rx) = stalled_runtime();
    assert!(
        bounded(runtime.run_ownership(GUILD, MEMBER, false, OwnershipCommand::Reclaim))
            .await
            .is_none()
    );
}

#[tokio::test(start_paused = true)]
async fn privacy_wait_is_bounded_with_a_live_unanswered_inbox() {
    let (runtime, _rx) = stalled_runtime();
    assert!(bounded(runtime.run_privacy(
        GUILD,
        MEMBER,
        false,
        "member".to_owned(),
        PrivacyCommand::Private
    ))
    .await
    .is_none());
}

#[tokio::test(start_paused = true)]
async fn kick_start_wait_is_bounded_with_a_live_unanswered_inbox() {
    let (runtime, _rx) = stalled_runtime();
    assert!(bounded(runtime.kick_start(GUILD, 900, 500, MEMBER, 301))
        .await
        .is_none());
}

#[tokio::test(start_paused = true)]
async fn kick_ballot_wait_is_bounded_with_a_live_unanswered_inbox() {
    let (runtime, _rx) = stalled_runtime();
    assert!(
        bounded(runtime.kick_ballot(GUILD, 900, MEMBER, VoteBallot::Yes))
            .await
            .is_none()
    );
}

#[tokio::test(start_paused = true)]
async fn kick_room_lookup_wait_is_bounded_with_a_live_unanswered_inbox() {
    let (runtime, _rx) = stalled_runtime();
    assert!(bounded(runtime.kick_room_of(GUILD, MEMBER)).await.is_none());
}

#[derive(Clone, Copy, Debug)]
enum ReplyCase {
    Transfer,
    Reclaim,
    Private,
    Public,
    Ballot,
    KickLookup,
    KickStart,
}

impl ReplyCase {
    fn interaction(self) -> Interaction {
        match self {
            Self::Transfer => voice_interaction_as(
                Some(command_data("transfer", vec![user_option("member", 301)])),
                None,
                MEMBER,
            ),
            Self::Reclaim => {
                voice_interaction_as(Some(command_data("reclaim", vec![])), None, MEMBER)
            }
            Self::Private => {
                voice_interaction_as(Some(command_data("private", vec![])), None, MEMBER)
            }
            Self::Public => {
                voice_interaction_as(Some(command_data("public", vec![])), None, MEMBER)
            }
            Self::Ballot => component_interaction("votekick:900:yes", None, MEMBER),
            Self::KickLookup | Self::KickStart => kick_sink_interaction(301, MEMBER),
        }
    }
}

fn assert_honest_worker_failure(response: &InteractionResponse) {
    let text = response_text(response);
    let lower = text.to_lowercase();
    assert!(
        !lower.contains("warm") && !lower.contains("syncing"),
        "a loaded worker lost its reply; this is not cold-cache initialization: {text}"
    );
    assert!(
        [
            "timed out",
            "taking longer",
            "unavailable",
            "not responding",
            "couldn't",
            "could not",
            "busy",
            "stopped"
        ]
        .iter()
        .any(|reason| lower.contains(reason)),
        "state the worker/wait failure honestly: {text}"
    );
    assert!(
        ["try again", "retry", "check", "wait"]
            .iter()
            .any(|next| lower.contains(next)),
        "worker failure needs a next action: {text}"
    );
    assert!(
        !text.starts_with("Transferred") && !lower.contains("yours now"),
        "worker loss is not success: {text}"
    );
    assert_eq!(
        response.data.as_ref().and_then(|data| data.flags),
        Some(MessageFlags::EPHEMERAL)
    );
}

async fn lost_worker_reply(case: ReplyCase, drop_reply: bool) {
    let (runtime, mut rx) = stalled_runtime();
    let interaction = case.interaction();
    let call = handle_capture(&runtime, &interaction);
    tokio::pin!(call);
    // Poll the real handler until its command is accepted. Retaining that
    // command also retains the oneshot sender, so only production timeout
    // logic (not a dropped test sender) can finish the stalled case.
    let mut command = tokio::select! {
        command = rx.recv() => command.expect("accepted worker command"),
        _ = &mut call => panic!("{case:?} did not reach its worker boundary"),
    };
    if matches!(case, ReplyCase::KickStart) {
        match command {
            ActorCommand::KickRoomOf { reply, .. } => reply.send(Some(500)).unwrap(),
            _ => panic!("kick must resolve the target room before starting a vote"),
        }
        command = tokio::select! {
            command = rx.recv() => command.expect("accepted vote start"),
            _ = &mut call => panic!("kick did not reach vote start"),
        };
        assert!(matches!(command, ActorCommand::KickStart { .. }));
    }
    let held = if drop_reply {
        drop(command);
        None
    } else {
        Some(command)
    };
    let (owned, response) = bounded(&mut call).await;
    assert!(owned, "{case:?} still owns its exactly-once reply");
    assert_honest_worker_failure(&response.expect("one ephemeral failure reply"));
    drop(held);
}

#[tokio::test(start_paused = true)]
async fn accepted_but_stalled_commands_return_honest_user_copy() {
    for case in [
        ReplyCase::Transfer,
        ReplyCase::Reclaim,
        ReplyCase::Private,
        ReplyCase::Public,
        ReplyCase::Ballot,
        ReplyCase::KickLookup,
        ReplyCase::KickStart,
    ] {
        lost_worker_reply(case, false).await;
    }
}

#[tokio::test(start_paused = true)]
async fn dropped_worker_replies_return_honest_user_copy() {
    for case in [
        ReplyCase::Transfer,
        ReplyCase::Reclaim,
        ReplyCase::Private,
        ReplyCase::Public,
        ReplyCase::Ballot,
        ReplyCase::KickLookup,
        ReplyCase::KickStart,
    ] {
        lost_worker_reply(case, true).await;
    }
}

#[tokio::test(start_paused = true)]
async fn closed_worker_delivery_is_not_reported_as_warming_up() {
    for case in [
        ReplyCase::Transfer,
        ReplyCase::Reclaim,
        ReplyCase::Private,
        ReplyCase::Public,
        ReplyCase::Ballot,
        ReplyCase::KickLookup,
    ] {
        let (runtime, rx) = stalled_runtime();
        drop(rx);
        // Keep the closed inbox: handle_capture would publish a snapshot and respawn it.
        let seen = Arc::new(Mutex::new(None::<InteractionResponse>));
        let writer = seen.clone();
        let owned = bounded(handle_voice_interaction(
            &runtime,
            &case.interaction(),
            |response| {
                *writer.lock().unwrap() = Some(response);
                async {}
            },
        ))
        .await;
        let response = seen.lock().unwrap().clone();
        assert!(owned);
        assert_honest_worker_failure(&response.expect("closed worker reply"));
    }
}

#[tokio::test(start_paused = true)]
async fn delivered_worker_results_are_not_replaced_with_timeout_copy() {
    let (runtime, mut rx) = stalled_runtime();
    let call = runtime.run_ownership(GUILD, MEMBER, false, OwnershipCommand::Reclaim);
    tokio::pin!(call);
    let command = tokio::select! {
        command = rx.recv() => command.unwrap(),
        _ = &mut call => panic!("worker result was invented before delivery"),
    };
    match command {
        ActorCommand::Ownership { reply, .. } => {
            reply.send("already owner control".to_owned()).unwrap();
        }
        _ => panic!("wrong worker command"),
    }
    assert_eq!(
        bounded(call).await.as_deref(),
        Some("already owner control")
    );
}

async fn yield_tasks() {
    // No clock advance: lets every admitted task reach its worker wait before
    // advancing the shared deadline, even on a single-threaded executor.
    for _ in 0..128 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn worker_timeouts_return_responder_permits_and_ignore_late_success() {
    let (runtime, mut rx) = stalled_runtime();
    let runtime = Arc::new(runtime);
    let replies = Arc::new(Replies::new(Trace::default()));
    let sink = VoiceResponder::new(Arc::clone(&runtime), Arc::clone(&replies));
    let cache = DefaultInMemoryCache::new();
    let capacity = sink.admission.permits.available_permits();
    assert!((1..=16).contains(&capacity));
    for index in 0..capacity {
        let mut interaction = ReplyCase::Transfer.interaction();
        interaction.id = Id::new(40_000 + index as u64);
        interaction.member.as_mut().unwrap().user = Some(test_user(20_000 + index as u64));
        sink.handle(
            &Event::InteractionCreate(Box::new(InteractionCreate(interaction))),
            &cache,
        );
    }
    yield_tasks().await;
    assert_eq!(
        rx.len(),
        capacity,
        "every admitted task must be waiting on the worker"
    );
    assert_eq!(sink.admission.permits.available_permits(), 0);
    tokio::time::advance(ACK_BOUND).await;
    yield_tasks().await;
    assert_eq!(
        sink.admission.permits.available_permits(),
        capacity,
        "timeout must release every execution permit, not strand the responder lane"
    );
    {
        let completed = replies.completed.lock().unwrap();
        assert_eq!(completed.len(), capacity);
        for response in completed.iter() {
            assert_honest_worker_failure(response);
        }
    }
    // Delivering an old result must not complete the already answered command
    // again. All of its receiver futures should have been cancelled on expiry.
    for _ in 0..capacity {
        match rx.try_recv().expect("old worker command") {
            ActorCommand::Ownership { reply, .. } => {
                assert!(reply.send("Transferred late".to_owned()).is_err())
            }
            _ => panic!("unexpected worker command"),
        }
    }
    let mut ping = voice_interaction(Some(command_data("ping", vec![])), None, true);
    ping.id = Id::new(50_000);
    sink.handle(
        &Event::InteractionCreate(Box::new(InteractionCreate(ping))),
        &cache,
    );
    yield_tasks().await;
    let completed = replies.completed.lock().unwrap();
    assert_eq!(
        completed.len(),
        capacity + 1,
        "a new interaction must run after expiry"
    );
    assert!(response_text(completed.last().unwrap()).starts_with("Pong!"));
    assert_eq!(sink.admission.permits.available_permits(), capacity);
}
