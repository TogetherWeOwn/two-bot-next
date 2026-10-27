//! Protected acceptance for member-owned live-room discovery. The paired
//! builder must not edit this file. Only persistence/Discord are doubles.
//! The read tests drive real parsing, gates, actor dispatch and replies; the
//! command-only actor driver excludes unrelated periodic maintenance writes.
#![cfg(test)]

use super::*;

const OTHER: u64 = 301;
type Worker = GuildRoomWorker<Arc<Store>, Http>;

fn owned(channel_id: u64, owner: u64, original: u64) -> VoiceRoom {
    let mut row = room(channel_id);
    row.owner_id = owner;
    row.original_creator_id = original;
    row
}

async fn refusal(reason: RefusalReason, known_rooms: bool) -> GuildRoomWorker<Store, Http> {
    let (live, mut store, http, _) = fixture();
    store.trace_claims = true;
    if known_rooms {
        store.rooms.lock().unwrap().extend([
            (500, owned(500, MEMBER, OTHER)),
            (501, owned(501, MEMBER, MEMBER)),
            // Original creation is not current ownership.
            (502, owned(502, OTHER, MEMBER)),
        ]);
        live.publish(snapshot(
            &[500, 501, 502],
            vec![VoiceMember {
                member_id: OTHER,
                channel_id: 502,
                bot: Some(false),
            }],
        ));
    }
    store
        .claim_script
        .lock()
        .unwrap()
        .push_back(Ok(CreateClaim::Refused(reason)));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.set_admission_config(CreateAdmissionConfig::default());
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    worker
}

fn refusal_message(worker: &GuildRoomWorker<Store, Http>, reason: RefusalReason) -> String {
    match worker.failures().back().expect("one admission refusal") {
        LifecycleFailure::CreateRefused {
            creator_id,
            reason: actual,
            message,
        } => {
            assert_eq!(*creator_id, CREATOR);
            assert_eq!(*actual, reason, "reason identity must not change");
            message.clone()
        }
        other => panic!("expected admission refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn user_cap_refusal_links_every_known_currently_owned_room_only() {
    // Break caught: rendering the count alone, using original_creator_id, or
    // listing every guild room instead of the refused member's owned rooms.
    let worker = refusal(RefusalReason::UserCap, true).await;
    let text = refusal_message(&worker, RefusalReason::UserCap);
    assert!(text.contains("<#500>"), "missing owned room: {text}");
    assert!(text.contains("<#501>"), "missing second owned room: {text}");
    assert!(
        !text.contains("<#502>"),
        "another owner's room leaked: {text}"
    );
}

#[tokio::test]
async fn control_user_cap_without_known_rooms_never_invents_a_channel() {
    let worker = refusal(RefusalReason::UserCap, false).await;
    assert_eq!(
        refusal_message(&worker, RefusalReason::UserCap),
        "You already have a temporary voice channel."
    );
}

#[tokio::test]
async fn control_non_cap_refusals_stay_byte_identical_even_with_owned_rooms() {
    for (reason, expected) in [
        (
            RefusalReason::GuildCap,
            "This server has reached its temporary voice channel limit. Try again shortly.",
        ),
        (
            RefusalReason::Cooldown,
            "Please wait 30 seconds between creating channels.",
        ),
        (
            RefusalReason::UserBurst,
            "Temporary voice rate limit: 3 creates per 60 seconds. Please try again shortly.",
        ),
        (
            RefusalReason::GuildBurst,
            "This server's temporary voice rate limit is 10 creates per 60 seconds. Please try again shortly.",
        ),
    ] {
        let worker = refusal(reason, true).await;
        assert_eq!(refusal_message(&worker, reason), expected);
    }
}

#[tokio::test]
async fn control_refusals_claim_once_before_any_discord_write_and_never_retry() {
    for reason in [
        RefusalReason::UserCap,
        RefusalReason::GuildCap,
        RefusalReason::Cooldown,
        RefusalReason::UserBurst,
        RefusalReason::GuildBurst,
    ] {
        let mut worker = refusal(reason, true).await;
        assert_eq!(*worker.store.trace.lock().unwrap(), ["claim"]);
        assert_eq!(worker.store.claims.lock().unwrap().len(), 1);
        assert_eq!(worker.store.claims.lock().unwrap()[0].0, MEMBER);
        assert!(worker.store.bound.lock().unwrap().is_empty());
        assert!(worker.store.settled.lock().unwrap().is_empty());
        assert_eq!(worker.tracked().len(), 3);
        assert!(!worker.dispatch_one(60_000).await);
        assert_eq!(*worker.store.trace.lock().unwrap(), ["claim"]);
    }
}

#[test]
fn control_pure_admission_codes_and_messages_remain_context_free() {
    let config = CreateAdmissionConfig::default();
    for (reason, code, message) in [
        (RefusalReason::UserCap, "user_cap", "You already have a temporary voice channel."),
        (RefusalReason::GuildCap, "guild_cap", "This server has reached its temporary voice channel limit. Try again shortly."),
        (RefusalReason::Cooldown, "cooldown", "Please wait 30 seconds between creating channels."),
        (RefusalReason::UserBurst, "user_burst", "Temporary voice rate limit: 3 creates per 60 seconds. Please try again shortly."),
        (RefusalReason::GuildBurst, "guild_burst", "This server's temporary voice rate limit is 10 creates per 60 seconds. Please try again shortly."),
    ] {
        assert_eq!(reason.code(), code);
        assert_eq!(reason.user_message(&config), message);
    }
}

struct ReadFixture {
    runtime: VoiceRuntime<Arc<Store>, Http>,
    store: Arc<Store>,
    task: tokio::task::JoinHandle<Worker>,
    trace: Trace,
    durable_rooms: HashMap<u64, VoiceRoom>,
    durable_privacy: BTreeMap<u64, PrivacyRecord>,
    live_privacy: HashMap<u64, PrivateRoom>,
}

async fn read_fixture() -> ReadFixture {
    let trace = Trace::default();
    let store = Arc::new(Store::new(trace.clone()));
    store.rooms.lock().unwrap().extend([
        (500, owned(500, MEMBER, OTHER)),
        (501, owned(501, MEMBER, MEMBER)),
        (502, owned(502, OTHER, MEMBER)),
    ]);
    store.privacy.lock().unwrap().insert(
        501,
        PrivacyRecord {
            private: true,
            join_channel_id: Some(701),
            blocked: BTreeSet::new(),
        },
    );
    let live = LiveGuild::new(GUILD);
    let mut guild = command_snapshot();
    guild.channels.extend(
        [500, 501, 502, 598, 701]
            .into_iter()
            .map(|id| channel(id, 2, Some(CATEGORY))),
    );
    // Neither caller has to be in voice to discover what they own.
    live.publish(guild);
    let mut worker = GuildRoomWorker::load(live.clone(), store.clone(), Http::new(trace.clone()))
        .await
        .unwrap();

    // Persistence now contradicts the loaded live worker. A database census
    // would omit a live room, invent room 599 and report the wrong privacy.
    store.rooms.lock().unwrap().remove(&500);
    store
        .rooms
        .lock()
        .unwrap()
        .insert(599, owned(599, MEMBER, MEMBER));
    store.privacy.lock().unwrap().get_mut(&501).unwrap().private = false;
    store
        .rooms_errors
        .lock()
        .unwrap()
        .push_back(StoreError::Unavailable);
    let factory_store = store.clone();
    let factory_trace = trace.clone();
    let runtime = VoiceRuntime::new(
        move || (factory_store.clone(), Http::new(factory_trace.clone())),
        Duration::from_secs(60),
        true,
    );
    let (tx, mut rx) = mpsc::unbounded_channel();
    let backpressure = ActorInboxBackpressure::default();
    runtime.actors.lock().unwrap().insert(
        GUILD,
        GuildActor {
            live,
            tx,
            backpressure: backpressure.clone(),
        },
    );
    let durable_rooms = store.rooms.lock().unwrap().clone();
    let durable_privacy = store.privacy.lock().unwrap().clone();
    let live_privacy = worker.privacy.clone();
    let task = tokio::spawn(async move {
        while let Some(command) = rx.recv().await {
            backpressure.release_for(&command);
            apply_command(&mut worker, command, 0, &backpressure);
        }
        worker
    });
    ReadFixture {
        runtime,
        store,
        task,
        trace,
        durable_rooms,
        durable_privacy,
        live_privacy,
    }
}

fn request(member: u64) -> Interaction {
    voice_interaction_as(
        Some(command_data("myroom", vec![])),
        Some(Permissions::empty()),
        member,
    )
}

async fn read(runtime: &VoiceRuntime<Arc<Store>, Http>, member: u64) -> InteractionResponse {
    let replies = Arc::new(Mutex::new(Vec::new()));
    let capture = replies.clone();
    let handled = handle_voice_interaction(runtime, &request(member), move |response| {
        capture.lock().unwrap().push(response);
        async {}
    })
    .await;
    assert!(handled, "/myroom must be claimed by the real voice handler");
    let mut replies = replies.lock().unwrap();
    assert_eq!(replies.len(), 1, "one read produces exactly one reply");
    let response = replies.remove(0);
    assert!(
        response
            .data
            .as_ref()
            .and_then(|data| data.flags)
            .is_some_and(|flags| flags.contains(MessageFlags::EPHEMERAL)),
        "owned-room information must remain private"
    );
    response
}

async fn assert_read_only(fixture: ReadFixture) {
    drop(fixture.runtime);
    let worker = fixture.task.await.expect("command-only actor exits");
    assert_eq!(worker.rooms[&500], owned(500, MEMBER, OTHER));
    assert_eq!(worker.rooms[&501], owned(501, MEMBER, MEMBER));
    assert_eq!(worker.rooms[&502], owned(502, OTHER, MEMBER));
    assert_eq!(worker.rooms.len(), 3, "no room mutation");
    assert_eq!(worker.privacy, fixture.live_privacy, "no privacy mutation");
    assert_eq!(
        worker.queue.pending_counts(GUILD),
        (0, 0),
        "no queued mutation"
    );
    assert_eq!(*fixture.store.rooms.lock().unwrap(), fixture.durable_rooms);
    assert_eq!(
        *fixture.store.privacy.lock().unwrap(),
        fixture.durable_privacy
    );
    assert_eq!(
        fixture.store.rooms_errors.lock().unwrap().len(),
        1,
        "no room-store read"
    );
    assert!(
        fixture.store.claims.lock().unwrap().is_empty(),
        "no create claim"
    );
    assert!(worker.http.written_overwrites.lock().unwrap().is_empty());
    assert!(
        fixture.trace.lock().unwrap().is_empty(),
        "no persistence/Discord write"
    );
}

#[tokio::test]
async fn myroom_lists_all_owned_live_channels_with_their_runtime_privacy_outside_voice() {
    // Break caught: current-channel-only lookup, persistence census, omitted
    // privacy, or using original creation rather than current ownership.
    let fixture = read_fixture().await;
    let response = read(&fixture.runtime, MEMBER).await;
    let text = response_text(&response).to_ascii_lowercase();
    for (mention, state) in [("<#500>", "public"), ("<#501>", "private")] {
        let row = text
            .lines()
            .find(|line| line.contains(mention))
            .unwrap_or_else(|| panic!("missing owned channel {mention}: {text}"));
        assert!(
            row.contains(state),
            "missing runtime privacy {state}: {row}"
        );
    }
    for excluded in ["<#502>", "<#598>", "<#599>", "<#701>"] {
        assert!(
            !text.contains(excluded),
            "not an owned tracked room: {text}"
        );
    }
    assert_read_only(fixture).await;
}

#[tokio::test]
async fn myroom_uses_authenticated_current_owner_not_original_creator_or_another_member() {
    let fixture = read_fixture().await;
    let response = read(&fixture.runtime, OTHER).await;
    let text = response_text(&response);
    assert!(
        text.contains("<#502>"),
        "current owner must find their room: {text}"
    );
    for excluded in ["<#500>", "<#501>", "<#599>"] {
        assert!(
            !text.contains(excluded),
            "another member's room leaked: {text}"
        );
    }
    assert_read_only(fixture).await;
}

#[tokio::test]
async fn myroom_no_owned_live_room_is_truthful_and_points_to_joining_a_creator_lobby() {
    let fixture = read_fixture().await;
    let response = read(&fixture.runtime, 399).await;
    let text = response_text(&response).to_ascii_lowercase();
    assert!(
        text.contains("no") && text.contains("own"),
        "must say no owned room: {text}"
    );
    assert!(
        text.contains("room") || text.contains("voice channel"),
        "{text}"
    );
    assert!(
        text.contains("creator") && text.contains("join"),
        "next action is join a creator lobby: {text}"
    );
    assert!(
        !text.contains("<#5"),
        "must not show someone else's or store-only rooms: {text}"
    );
    assert_read_only(fixture).await;
}

#[tokio::test]
async fn myroom_deferred_responder_completes_one_private_read_without_effects() {
    let fixture = read_fixture().await;
    let reply_trace = Trace::default();
    let replies = Replies::new(reply_trace.clone());
    VoiceResponder::respond_with(&fixture.runtime, &replies, &request(MEMBER), None, None).await;
    let completed = replies.completed.lock().unwrap().clone();
    assert_eq!(
        completed.len(),
        1,
        "responder must actually complete /myroom"
    );
    assert!(response_text(&completed[0]).contains("<#500>"));
    assert!(completed[0]
        .data
        .as_ref()
        .and_then(|data| data.flags)
        .is_some_and(|flags| flags.contains(MessageFlags::EPHEMERAL)));
    assert_eq!(*reply_trace.lock().unwrap(), ["defer", "complete"]);
    assert_read_only(fixture).await;
}
