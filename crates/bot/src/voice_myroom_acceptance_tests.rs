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
async fn control_user_cap_refusal_record_stays_context_free_even_with_owned_rooms() {
    // The recorded refusal is an operator diagnostic: a gateway voice join has
    // no per-member reply, and member-specific text would split the failure
    // dedup key and notice ledger. Break caught: embedding any owned room id.
    let worker = refusal(RefusalReason::UserCap, true).await;
    assert_eq!(
        refusal_message(&worker, RefusalReason::UserCap),
        "You already have a temporary voice channel."
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
    live: LiveGuild,
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
    guild
        .channels
        .extend([500, 501, 502, 598, 701].into_iter().map(|id| {
            let mut room = channel(id, 2, Some(CATEGORY));
            if id == 501 {
                // Privacy is an observed Connect deny, not retained retry bookkeeping.
                room.permission_overwrites = Some(vec![PermissionOverwrite {
                    id: Id::new(GUILD),
                    kind: PermissionOverwriteType::Role,
                    allow: Permissions::empty(),
                    deny: Permissions::CONNECT,
                }]);
            }
            room
        }));
    // Neither caller has to be in voice to discover what they own.
    live.publish(guild);
    let worker = GuildRoomWorker::load(live.clone(), store.clone(), Http::new(trace.clone()))
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
    let durable_rooms = store.rooms.lock().unwrap().clone();
    let durable_privacy = store.privacy.lock().unwrap().clone();
    let live_privacy = worker.privacy.clone();
    let (runtime, task) = read_runtime(live.clone(), store.clone(), trace.clone(), worker);
    ReadFixture {
        runtime,
        live,
        store,
        task,
        trace,
        durable_rooms,
        durable_privacy,
        live_privacy,
    }
}

fn read_runtime(
    live: LiveGuild,
    store: Arc<Store>,
    trace: Trace,
    mut worker: Worker,
) -> (
    VoiceRuntime<Arc<Store>, Http>,
    tokio::task::JoinHandle<Worker>,
) {
    let (runtime, mut rx, backpressure) = controlled_read_runtime(live, store, trace);
    let task = tokio::spawn(async move {
        while let Some(command) = rx.recv().await {
            backpressure.release_for(&command);
            apply_command(&mut worker, command, 0, &backpressure);
        }
        worker
    });
    (runtime, task)
}

fn controlled_read_runtime(
    live: LiveGuild,
    store: Arc<Store>,
    trace: Trace,
) -> (
    VoiceRuntime<Arc<Store>, Http>,
    mpsc::UnboundedReceiver<ActorCommand>,
    ActorInboxBackpressure,
) {
    let runtime = VoiceRuntime::new(
        move || (store.clone(), Http::new(trace.clone())),
        Duration::from_secs(60),
        true,
    );
    let (tx, rx) = mpsc::unbounded_channel();
    let backpressure = ActorInboxBackpressure::default();
    runtime.actors.lock().unwrap().insert(
        GUILD,
        GuildActor {
            live,
            tx,
            backpressure: backpressure.clone(),
        },
    );
    (runtime, rx, backpressure)
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

async fn drain_read_worker(worker: &mut Worker) {
    for step in 1..=40u64 {
        if !worker.dispatch_one(step * 1_000_000).await {
            return;
        }
    }
    panic!("privacy setup queue did not drain");
}

#[tokio::test]
async fn myroom_partial_public_delete_refusal_reports_observed_open_acl_without_retiring_retry() {
    // Break caught: retry bookkeeping overrides Discord's accepted Connect-open
    // overwrite, so the member is told an actually open room is private.
    let (live, store, http, trace) = owned_room();
    let store = Arc::new(store);
    *http.next_id.lock().unwrap() = 700;
    let mut worker = GuildRoomWorker::load(live.clone(), store.clone(), http)
        .await
        .unwrap();
    worker.apply_privacy(MEMBER, false, "member", PrivacyCommand::Private);
    drain_read_worker(&mut worker).await;
    assert!(
        worker.privacy[&500].private,
        "precondition: room made private"
    );
    assert_eq!(
        store.privacy.lock().unwrap()[&500].join_channel_id,
        Some(700)
    );

    trace.lock().unwrap().clear();
    worker.http.written_overwrites.lock().unwrap().clear();
    worker
        .http
        .delete_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::AccessDenied);
    worker.apply_privacy(MEMBER, false, "member", PrivacyCommand::Public);
    drain_read_worker(&mut worker).await;
    assert_eq!(
        *trace.lock().unwrap(),
        ["overwrite:500:100:Role", "delete:700"]
    );
    assert!(
        worker.privacy[&500].private,
        "retry flag must survive refusal"
    );
    assert!(
        worker.join_deletable.contains(&700),
        "Join deletion remains retryable"
    );
    assert_eq!(
        store.privacy.lock().unwrap()[&500],
        PrivacyRecord {
            private: true,
            join_channel_id: Some(700),
            blocked: BTreeSet::new(),
        }
    );
    assert!(worker.failures().contains(&LifecycleFailure::Discord {
        channel_id: 500,
        error: RoomHttpError::AccessDenied,
    }));
    {
        let state = live.read_state();
        let everyone = state.channels[&500]
            .permission_overwrites
            .as_ref()
            .unwrap()
            .iter()
            .find(|entry| entry.kind == PermissionOverwriteType::Role && entry.id.get() == GUILD)
            .expect("accepted @everyone overwrite is projected into live evidence");
        assert!(!everyone.deny.contains(Permissions::CONNECT));
    }
    let live_rooms = worker.rooms.clone();
    let live_privacy = worker.privacy.clone();
    let join_deletable = worker.join_deletable.clone();
    let durable_rooms = store.rooms.lock().unwrap().clone();
    let durable_privacy = store.privacy.lock().unwrap().clone();
    let pending = worker.queue.pending_counts(GUILD);
    store
        .rooms_errors
        .lock()
        .unwrap()
        .push_back(StoreError::Unavailable);
    trace.lock().unwrap().clear();
    worker.http.written_overwrites.lock().unwrap().clear();
    let (runtime, task) = read_runtime(live, store.clone(), trace.clone(), worker);
    let rooms = runtime.run_myrooms(GUILD, MEMBER).await;
    let response = read(&runtime, MEMBER).await;
    let text = response_text(&response).to_ascii_lowercase();
    drop(runtime);
    let worker = task.await.unwrap();
    assert_eq!(worker.rooms, live_rooms);
    assert_eq!(worker.privacy, live_privacy);
    assert_eq!(worker.join_deletable, join_deletable);
    assert_eq!(worker.queue.pending_counts(GUILD), pending);
    assert_eq!(*store.rooms.lock().unwrap(), durable_rooms);
    assert_eq!(*store.privacy.lock().unwrap(), durable_privacy);
    assert_eq!(
        store.rooms_errors.lock().unwrap().len(),
        1,
        "no room-store read"
    );
    assert!(store.claims.lock().unwrap().is_empty());
    assert!(worker.http.written_overwrites.lock().unwrap().is_empty());
    assert!(
        trace.lock().unwrap().is_empty(),
        "myroom is read-only even after public refusal"
    );
    assert_eq!(
        rooms,
        Some(vec![(500, false)]),
        "ACL, not retry bookkeeping, is public"
    );
    let row = text
        .lines()
        .find(|line| line.contains("<#500>"))
        .expect("owned room row");
    assert!(row.contains("public") && !row.contains("private"), "{row}");
}

#[tokio::test]
async fn myroom_queued_read_disconnects_before_dispatch_then_reconnect_uses_new_acl() {
    // Deterministic actor-boundary fence: no sleeps or scheduling lottery.
    // This does not claim to interleave two synchronous read-lock acquisitions.
    let trace = Trace::default();
    let store = Arc::new(Store::new(trace.clone()));
    store
        .rooms
        .lock()
        .unwrap()
        .insert(500, owned(500, MEMBER, OTHER));
    let live = LiveGuild::new(GUILD);
    let mut closed = channel(500, 2, Some(CATEGORY));
    closed.permission_overwrites = Some(vec![PermissionOverwrite {
        id: Id::new(GUILD),
        kind: PermissionOverwriteType::Role,
        allow: Permissions::empty(),
        deny: Permissions::CONNECT,
    }]);
    let mut initial = command_snapshot();
    initial.channels.push(closed);
    live.publish(initial);
    let mut worker = GuildRoomWorker::load(live.clone(), store.clone(), Http::new(trace.clone()))
        .await
        .unwrap();
    let before_rooms = worker.rooms.clone();
    let before_privacy = worker.privacy.clone();
    let stored_rooms = store.rooms.lock().unwrap().clone();
    let stored_privacy = store.privacy.lock().unwrap().clone();
    store
        .rooms_errors
        .lock()
        .unwrap()
        .push_back(StoreError::Unavailable);
    let (runtime, mut rx, backpressure) =
        controlled_read_runtime(live.clone(), store.clone(), trace.clone());

    let (initial_rooms, ()) = tokio::join!(runtime.run_myrooms(GUILD, MEMBER), async {
        let command = rx.recv().await.expect("initial queued read");
        assert!(matches!(&command, ActorCommand::MyRooms { .. }));
        backpressure.release_for(&command);
        apply_command(&mut worker, command, 0, &backpressure);
    });
    assert_eq!(
        initial_rooms,
        Some(vec![(500, true)]),
        "closed initial generation"
    );

    let (disconnected_response, ()) = tokio::join!(read(&runtime, MEMBER), async {
        let command = rx.recv().await.expect("handler enqueued while live");
        assert!(matches!(&command, ActorCommand::MyRooms { .. }));
        runtime.disconnect();
        backpressure.release_for(&command);
        apply_command(&mut worker, command, 1, &backpressure);
    });
    let disconnected_text = response_text(&disconnected_response).to_ascii_lowercase();
    assert!(!disconnected_text.contains("<#500>"), "{disconnected_text}");
    assert!(
        disconnected_text.contains("try again") || disconnected_text.contains("unavailable"),
        "{disconnected_text}"
    );

    runtime.gateway_now_connected();
    // The new generation is explicitly open. The earlier Connect deny must
    // not be spliced into this ready/present channel's read result.
    let mut replacement = command_snapshot();
    replacement.channels.push(channel(500, 2, Some(CATEGORY)));
    live.publish(replacement);
    let (new_rooms, ()) = tokio::join!(runtime.run_myrooms(GUILD, MEMBER), async {
        let command = rx.recv().await.expect("fresh read after reconnect");
        backpressure.release_for(&command);
        apply_command(&mut worker, command, 2, &backpressure);
    });
    let (new_response, ()) = tokio::join!(read(&runtime, MEMBER), async {
        let command = rx.recv().await.expect("fresh handler read");
        backpressure.release_for(&command);
        apply_command(&mut worker, command, 3, &backpressure);
    });
    let new_text = response_text(&new_response).to_ascii_lowercase();
    assert_eq!(
        new_rooms,
        Some(vec![(500, false)]),
        "new ACL, never stale private"
    );
    let row = new_text
        .lines()
        .find(|line| line.contains("<#500>"))
        .unwrap();
    assert!(row.contains("public") && !row.contains("private"), "{row}");
    drop(runtime);
    assert!(
        rx.recv().await.is_none(),
        "no detached actor or queued mutation"
    );
    assert_eq!(worker.rooms, before_rooms);
    assert_eq!(worker.privacy, before_privacy);
    assert_eq!(*store.rooms.lock().unwrap(), stored_rooms);
    assert_eq!(*store.privacy.lock().unwrap(), stored_privacy);
    assert_eq!(store.rooms_errors.lock().unwrap().len(), 1);
    assert_eq!(worker.queue.pending_counts(GUILD), (0, 0));
    assert!(store.claims.lock().unwrap().is_empty());
    assert!(worker.http.written_overwrites.lock().unwrap().is_empty());
    assert!(trace.lock().unwrap().is_empty());
}

#[tokio::test]
async fn control_myroom_deleted_channel_is_omitted_without_reconciling_away_the_owned_row() {
    let fixture = read_fixture().await;
    fixture.live.remove_channel(501);
    let rooms = fixture.runtime.run_myrooms(GUILD, MEMBER).await;
    let response = read(&fixture.runtime, MEMBER).await;
    let text = response_text(&response).to_owned();
    assert_read_only(fixture).await;
    assert_eq!(rooms, Some(vec![(500, false)]));
    assert!(
        text.contains("<#500>") && !text.contains("<#501>"),
        "{text}"
    );
}

fn complete_private_rows(text: &str, expected: &BTreeSet<u64>) -> BTreeSet<u64> {
    let mut shown = BTreeSet::new();
    for line in text.lines().filter(|line| line.contains("<#")) {
        let (_, mention) = line.split_once("<#").unwrap();
        let (id, _) = mention.split_once('>').expect("no partial channel mention");
        let id = id.parse::<u64>().expect("complete decimal room id");
        assert!(expected.contains(&id), "only owned room ids: {line}");
        assert!(shown.insert(id), "no duplicate room: {line}");
        let lower = line.to_ascii_lowercase();
        assert!(
            lower.contains("private") && !lower.contains("public"),
            "no partial/mislabeled private row: {line}"
        );
    }
    shown
}

#[tokio::test]
async fn myroom_many_long_private_ids_complete_at_whole_row_boundary_with_omitted_accounting() {
    // The real completion adapter takes only 2000 scalars. The responder must
    // hand it an already bounded, complete message, not 2041 scalars whose
    // 60th room/privacy label will be cut mid-row. RoomHttp's private loopback
    // constructor is not accessible here: this pins its real responder input,
    // not an observed RoomHttp transport request.
    let ids: BTreeSet<u64> = (0..61)
        .map(|index| 1_000_000_000_000_000_000 + index)
        .collect();
    assert!(ids.iter().all(|id| id.to_string().len() == 19));
    let trace = Trace::default();
    let store = Arc::new(Store::new(trace.clone()));
    let live = LiveGuild::new(GUILD);
    let mut guild = command_snapshot();
    for id in &ids {
        store
            .rooms
            .lock()
            .unwrap()
            .insert(*id, owned(*id, MEMBER, OTHER));
        let mut observed = channel(*id, 2, Some(CATEGORY));
        observed.permission_overwrites = Some(vec![PermissionOverwrite {
            id: Id::new(GUILD),
            kind: PermissionOverwriteType::Role,
            allow: Permissions::empty(),
            deny: Permissions::CONNECT,
        }]);
        guild.channels.push(observed);
    }
    live.publish(guild);
    let worker = GuildRoomWorker::load(live.clone(), store.clone(), Http::new(trace.clone()))
        .await
        .unwrap();
    let before_rooms = worker.rooms.clone();
    let before_privacy = worker.privacy.clone();
    let stored_rooms = store.rooms.lock().unwrap().clone();
    let stored_privacy = store.privacy.lock().unwrap().clone();
    store
        .rooms_errors
        .lock()
        .unwrap()
        .push_back(StoreError::Unavailable);
    let (runtime, task) = read_runtime(live, store.clone(), trace.clone(), worker);
    let reply_trace = Trace::default();
    let replies = Replies::new(reply_trace.clone());
    VoiceResponder::respond_with(&runtime, &replies, &request(MEMBER), None, None).await;
    let completed = replies.completed.lock().unwrap().clone();
    drop(runtime);
    let worker = task.await.unwrap();
    assert_eq!(worker.rooms, before_rooms);
    assert_eq!(worker.privacy, before_privacy);
    assert_eq!(*store.rooms.lock().unwrap(), stored_rooms);
    assert_eq!(*store.privacy.lock().unwrap(), stored_privacy);
    assert_eq!(store.rooms_errors.lock().unwrap().len(), 1);
    assert_eq!(worker.queue.pending_counts(GUILD), (0, 0));
    assert!(store.claims.lock().unwrap().is_empty());
    assert!(store.bound.lock().unwrap().is_empty());
    assert!(store.settled.lock().unwrap().is_empty());
    assert!(worker.http.written_overwrites.lock().unwrap().is_empty());
    assert!(trace.lock().unwrap().is_empty());
    assert_eq!(*reply_trace.lock().unwrap(), ["defer", "complete"]);
    assert_eq!(
        completed.len(),
        1,
        "one private response, not unsolicited follow-ups"
    );
    let response = &completed[0];
    assert_eq!(
        response.data.as_ref().and_then(|data| data.flags),
        Some(MessageFlags::EPHEMERAL)
    );
    let text = response_text(response);
    assert!(
        text.chars().count() <= 2000,
        "completion must not silently clamp a row: {} scalars",
        text.chars().count()
    );
    let shown = complete_private_rows(text, &ids);
    let attachments = response
        .data
        .as_ref()
        .and_then(|data| data.attachments.as_deref())
        .unwrap_or(&[]);
    if let Some(attachment) = attachments.first() {
        assert_eq!(
            attachments.len(),
            1,
            "one complete bounded room-list attachment"
        );
        assert!(
            attachment.file.len() <= 64 * 1024,
            "room attachment is bounded"
        );
        let body = std::str::from_utf8(&attachment.file).expect("inspectable UTF-8 room list");
        assert_eq!(
            complete_private_rows(body, &ids),
            ids,
            "attachment contains every room/privacy row"
        );
        let lower = text.to_ascii_lowercase();
        assert!(
            lower.contains("attach") || lower.contains("file"),
            "complete list is discoverable: {text}"
        );
    } else {
        assert!(!shown.is_empty(), "at least one whole row fits");
        let omitted = ids.len() - shown.len();
        assert!(
            omitted > 0,
            "61 long private rows require bounded omission or attachment"
        );
        let footer = text
            .lines()
            .find(|line| {
                let lower = line.to_ascii_lowercase();
                lower.contains("omitt") || lower.contains("more")
            })
            .expect("explicit omitted-room count, not a misleading all-rooms header");
        let counts: Vec<usize> = footer
            .split(|ch: char| !ch.is_ascii_digit())
            .filter(|number| !number.is_empty())
            .map(|number| number.parse().unwrap())
            .collect();
        assert!(
            counts.contains(&omitted),
            "{} shown + {omitted} omitted = 61, footer was {footer}",
            shown.len()
        );
    }
}

#[tokio::test]
async fn myroom_unknown_overwrites_are_unavailable_through_facade_and_handler() {
    // Break caught: treating absent ACL evidence as an explicitly public room.
    let fixture = read_fixture().await;
    let mut unknown = channel(501, 2, Some(CATEGORY));
    unknown.permission_overwrites = None;
    fixture.live.upsert_channel(unknown);
    let rooms = fixture.runtime.run_myrooms(GUILD, MEMBER).await;
    let response = read(&fixture.runtime, MEMBER).await;
    let text = response_text(&response).to_ascii_lowercase();
    assert_read_only(fixture).await;
    assert_eq!(
        rooms, None,
        "a ready channel with unknown ACL is not public"
    );
    assert!(
        text.contains("try again") || text.contains("unavailable") || text.contains("unknown"),
        "missing unavailable/unknown read result: {text}"
    );
    assert!(
        !text.contains("<#501>"),
        "the bool-only room facade cannot label unknown evidence: {text}"
    );
}

#[tokio::test]
async fn control_myroom_explicit_empty_overwrites_are_known_public_evidence() {
    let fixture = read_fixture().await;
    let rooms = fixture.runtime.run_myrooms(GUILD, MEMBER).await;
    let response = read(&fixture.runtime, MEMBER).await;
    let text = response_text(&response).to_ascii_lowercase();
    assert_read_only(fixture).await;
    assert!(
        rooms
            .as_ref()
            .is_some_and(|rooms| rooms.contains(&(500, false))),
        "an explicitly empty overwrite vector is known, not unavailable: {rooms:?}"
    );
    let row = text.lines().find(|line| line.contains("<#500>")).unwrap();
    assert!(row.contains("public") && !row.contains("private"), "{row}");
}

#[tokio::test]
async fn control_myroom_disconnected_actor_never_claims_live_rooms() {
    let fixture = read_fixture().await;
    fixture.live.disconnect();
    let rooms = fixture.runtime.run_myrooms(GUILD, MEMBER).await;
    let response = read(&fixture.runtime, MEMBER).await;
    let text = response_text(&response).to_ascii_lowercase();
    assert_read_only(fixture).await;
    assert_eq!(rooms, None);
    assert!(
        !text.contains("<#500>") && !text.contains("<#501>"),
        "{text}"
    );
    assert!(
        text.contains("try again") || text.contains("unavailable"),
        "{text}"
    );
}
