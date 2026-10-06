//! V3 `/limit` and `/unlimit` runtime tests. Discord is faked behind
//! `RoomWrites`; nothing here touches the network, a token or a database.

use super::super::limit::{decide_limit, outcome_text, LimitAck, LimitReply};
use super::*;
use two_bot_core::voice_room_controls::RoomLimit;

const OUTSIDER: u64 = 555;

fn int_option(name: &str, value: i64) -> CommandDataOption {
    CommandDataOption {
        name: name.to_owned(),
        value: CommandOptionValue::Integer(value),
    }
}

fn limit_interaction(user_id: u64, options: Vec<CommandDataOption>) -> Interaction {
    voice_interaction_as(Some(command_data("limit", options)), None, user_id)
}

/// Run one command on the worker: queue it, dispatch the write once and
/// return the reply text plus the acknowledgement the caller would see.
async fn run_once(
    worker: &mut GuildRoomWorker<Store, Http>,
    actor: u64,
    is_admin: bool,
    command: LimitCommand,
) -> (String, Option<LimitAck>) {
    match worker.apply_limit(actor, is_admin, command) {
        LimitReply::Done(text) => (text, None),
        LimitReply::Queued { limit, ack } => {
            assert!(worker.dispatch_one(0).await, "the queued write is due");
            let outcome = ack.await.ok();
            (outcome_text(command, limit, outcome), outcome)
        }
    }
}

fn limit_writes(trace: &Trace) -> Vec<String> {
    trace
        .lock()
        .unwrap()
        .iter()
        .filter(|entry| entry.starts_with("limit:"))
        .cloned()
        .collect()
}

// --- parsing and gate names ---------------------------------------------------

#[test]
fn parse_limit_reads_the_optional_count() {
    let bare = voice_interaction(Some(command_data("limit", Vec::new())), None, true);
    assert_eq!(
        parse_voice_command(&bare),
        Some(VoiceCommand::Limit(LimitArg::Absent))
    );
    let counted = voice_interaction(
        Some(command_data("limit", vec![int_option("count", 5)])),
        None,
        true,
    );
    assert_eq!(
        parse_voice_command(&counted),
        Some(VoiceCommand::Limit(LimitArg::Value(5)))
    );
    // A wrongly typed count is refused later, never read as "lock the room".
    let malformed = voice_interaction(
        Some(command_data("limit", vec![command_option("count", "five")])),
        None,
        true,
    );
    assert_eq!(
        parse_voice_command(&malformed),
        Some(VoiceCommand::Limit(LimitArg::Malformed))
    );
}

#[test]
fn parse_unlimit_takes_no_options() {
    let interaction = voice_interaction(Some(command_data("unlimit", Vec::new())), None, true);
    assert_eq!(
        parse_voice_command(&interaction),
        Some(VoiceCommand::Unlimit)
    );
    let guildless = voice_interaction(Some(command_data("unlimit", Vec::new())), None, false);
    assert_eq!(parse_voice_command(&guildless), None);
}

#[test]
fn limit_command_names_key_the_role_restrictions() {
    let commands = [VoiceCommand::Limit(LimitArg::Absent), VoiceCommand::Unlimit];
    for command in &commands {
        assert!(
            two_bot_core::voice_access::VOICE_COMMANDS.contains(&command.name()),
            "{} is not a restrictable voice command",
            command.name()
        );
    }
    assert_eq!(commands[0].name(), "limit");
    assert_eq!(commands[1].name(), "unlimit");
    assert_eq!(LimitCommand::Limit(LimitArg::Value(3)).name(), "limit");
    assert_eq!(LimitCommand::Unlimit.name(), "unlimit");
}

// --- the pure decision, as the worker feeds it ----------------------------------

#[test]
fn decide_limit_maps_the_documented_edge_table() {
    let arg = |value| LimitCommand::Limit(LimitArg::Value(value));
    assert_eq!(decide_limit(arg(0), 4), Ok(RoomLimit::Unlimited));
    assert_eq!(decide_limit(arg(1), 4), Ok(RoomLimit::Limited(1)));
    assert_eq!(decide_limit(arg(99), 4), Ok(RoomLimit::Limited(99)));
    for refused in [arg(100), arg(-1), arg(i64::MAX), arg(i64::MIN)] {
        let text = decide_limit(refused, 4).expect_err("out of range");
        assert!(text.contains("0 to 99"), "{text}");
        assert!(text.contains("Nothing was changed"), "{text}");
    }
    let malformed = decide_limit(LimitCommand::Limit(LimitArg::Malformed), 4);
    assert!(malformed.expect_err("malformed").contains("0 to 99"));
    // No argument locks at the headcount: an empty room is unlimited and a
    // crowd above 99 clamps to 99.
    let lock = LimitCommand::Limit(LimitArg::Absent);
    assert_eq!(decide_limit(lock, 0), Ok(RoomLimit::Unlimited));
    assert_eq!(decide_limit(lock, 3), Ok(RoomLimit::Limited(3)));
    assert_eq!(decide_limit(lock, 99), Ok(RoomLimit::Limited(99)));
    assert_eq!(decide_limit(lock, 100), Ok(RoomLimit::Limited(99)));
    assert_eq!(decide_limit(lock, u32::MAX), Ok(RoomLimit::Limited(99)));
    assert_eq!(
        decide_limit(LimitCommand::Unlimit, 7),
        Ok(RoomLimit::Unlimited)
    );
}

// --- worker: decisions, gates and writes ----------------------------------------

#[tokio::test]
async fn limit_sets_the_room_limit_through_the_queue() {
    let (live, store, http, trace) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let (text, ack) = run_once(
        &mut worker,
        MEMBER,
        false,
        LimitCommand::Limit(LimitArg::Value(5)),
    )
    .await;
    assert_eq!(ack, Some(LimitAck::Applied));
    assert!(text.contains("limited to 5 people"), "{text}");
    assert_eq!(limit_writes(&trace), ["limit:500:5"]);
    let live = worker.live.inner.read().unwrap();
    assert_eq!(live.channels.get(&500).unwrap().user_limit, Some(5));
}

#[tokio::test]
async fn limit_zero_means_unlimited() {
    let (live, store, http, trace) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let (text, ack) = run_once(
        &mut worker,
        MEMBER,
        false,
        LimitCommand::Limit(LimitArg::Value(0)),
    )
    .await;
    assert_eq!(ack, Some(LimitAck::Applied));
    assert!(text.contains("no longer has a user limit"), "{text}");
    assert_eq!(limit_writes(&trace), ["limit:500:0"]);
}

#[tokio::test]
async fn limit_above_the_range_is_refused_before_anything_is_queued() {
    let (live, store, http, trace) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    for arg in [
        LimitArg::Value(100),
        LimitArg::Value(-3),
        LimitArg::Malformed,
    ] {
        let reply = worker.apply_limit(MEMBER, false, LimitCommand::Limit(arg));
        let LimitReply::Done(text) = reply else {
            panic!("{arg:?} must be refused, not queued");
        };
        assert!(text.contains("0 to 99"), "{text}");
    }
    assert_eq!(worker.queue.pending_counts(GUILD), (0, 0));
    assert!(limit_writes(&trace).is_empty());
}

#[tokio::test]
async fn limit_without_a_count_locks_at_the_human_headcount() {
    let (live, store, http, trace) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    // A bot in the room is not counted: two humans, so the lock is 2.
    worker
        .live
        .voice_update_at(777, Some(500), Some(true), 1_000);
    let (text, ack) = run_once(
        &mut worker,
        MEMBER,
        false,
        LimitCommand::Limit(LimitArg::Absent),
    )
    .await;
    assert_eq!(ack, Some(LimitAck::Applied));
    assert!(text.contains("Locked this room at 2 people"), "{text}");
    assert_eq!(limit_writes(&trace), ["limit:500:2"]);
}

#[tokio::test]
async fn unlimit_clears_the_limit() {
    let (live, store, http, trace) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let (text, ack) = run_once(&mut worker, MEMBER, false, LimitCommand::Unlimit).await;
    assert_eq!(ack, Some(LimitAck::Applied));
    assert!(text.contains("no longer has a user limit"), "{text}");
    assert_eq!(limit_writes(&trace), ["limit:500:0"]);
    let live = worker.live.inner.read().unwrap();
    assert_eq!(live.channels.get(&500).unwrap().user_limit, Some(0));
}

#[tokio::test]
async fn a_limit_discord_already_shows_writes_nothing() {
    let (live, store, http, trace) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    // The fixture room already carries a limit of 8.
    let (text, ack) = run_once(
        &mut worker,
        MEMBER,
        false,
        LimitCommand::Limit(LimitArg::Value(8)),
    )
    .await;
    assert_eq!(ack, Some(LimitAck::Applied));
    assert!(text.contains("limited to 8 people"), "{text}");
    assert!(limit_writes(&trace).is_empty());
}

#[tokio::test]
async fn limit_refuses_a_non_owner_and_changes_nothing() {
    let (live, store, http, trace) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    for command in [
        LimitCommand::Limit(LimitArg::Value(3)),
        LimitCommand::Unlimit,
    ] {
        let (text, ack) = run_once(&mut worker, 301, false, command).await;
        assert_eq!(ack, None);
        assert!(text.contains("Only the room owner or an admin"), "{text}");
    }
    assert_eq!(worker.queue.pending_counts(GUILD), (0, 0));
    assert!(limit_writes(&trace).is_empty());
}

#[tokio::test]
async fn limit_refuses_a_caller_who_is_not_in_voice() {
    let (live, store, http, trace) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let (text, _) = run_once(
        &mut worker,
        OUTSIDER,
        false,
        LimitCommand::Limit(LimitArg::Value(3)),
    )
    .await;
    assert_eq!(text, "You need to be in a voice room to use /limit.");
    let (text, _) = run_once(&mut worker, OUTSIDER, false, LimitCommand::Unlimit).await;
    assert_eq!(text, "You need to be in a voice room to use /unlimit.");
    // Admins have no room to name either: the commands act on the room they
    // are standing in.
    let (text, _) = run_once(&mut worker, OUTSIDER, true, LimitCommand::Unlimit).await;
    assert!(text.contains("need to be in a voice room"), "{text}");
    assert!(limit_writes(&trace).is_empty());
}

#[tokio::test]
async fn limit_refuses_an_unmanaged_voice_channel() {
    let (live, store, http, trace) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker
        .live
        .voice_update_at(OUTSIDER, Some(600), Some(false), 1_000);
    for is_admin in [false, true] {
        let (text, _) = run_once(
            &mut worker,
            OUTSIDER,
            is_admin,
            LimitCommand::Limit(LimitArg::Value(3)),
        )
        .await;
        assert!(text.contains("isn't a temporary room I manage"), "{text}");
    }
    assert!(limit_writes(&trace).is_empty());
}

#[tokio::test]
async fn an_admin_in_the_room_may_limit_it_without_owning_it() {
    let (live, store, http, trace) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    // 301 is a plain occupant; the admin flag is the override.
    let (text, ack) = run_once(
        &mut worker,
        301,
        true,
        LimitCommand::Limit(LimitArg::Value(4)),
    )
    .await;
    assert_eq!(ack, Some(LimitAck::Applied));
    assert!(text.contains("limited to 4 people"), "{text}");
    assert_eq!(limit_writes(&trace), ["limit:500:4"]);
}

#[tokio::test]
async fn limit_while_halted_refuses_as_paused() {
    let (live, store, http, trace) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.halted = true;
    let (text, _) = run_once(&mut worker, MEMBER, false, LimitCommand::Unlimit).await;
    assert!(text.contains("paused"), "{text}");
    assert!(limit_writes(&trace).is_empty());
}

#[tokio::test]
async fn limit_without_manage_channels_refuses_before_queueing() {
    let (live, store, http, trace) = owned_room();
    let mut snap = snapshot(
        &[500],
        vec![
            VoiceMember {
                member_id: MEMBER,
                channel_id: 500,
                bot: Some(false),
            },
            VoiceMember {
                member_id: 301,
                channel_id: 500,
                bot: Some(false),
            },
        ],
    );
    snap.bot.roles = vec![role(Permissions::VIEW_CHANNEL | Permissions::CONNECT)];
    live.publish(snap);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let (text, _) = run_once(&mut worker, MEMBER, false, LimitCommand::Unlimit).await;
    assert!(text.contains("Manage Channels"), "{text}");
    assert_eq!(worker.queue.pending_counts(GUILD), (0, 0));
    assert!(limit_writes(&trace).is_empty());
}

#[tokio::test]
async fn a_rate_limited_write_goes_back_on_the_queue_with_its_retry_after() {
    let (live, store, http, trace) = owned_room();
    http.limit_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::RateLimited {
            retry_after_ms: 1_500,
            global: false,
        });
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let (text, ack) = run_once(
        &mut worker,
        MEMBER,
        false,
        LimitCommand::Limit(LimitArg::Value(3)),
    )
    .await;
    assert_eq!(ack, Some(LimitAck::Delayed));
    assert!(text.contains("Discord is busy"), "{text}");
    // Re-queued, not dropped and not recorded as a failure.
    assert_eq!(worker.queue.pending_counts(GUILD), (1, 0));
    assert!(worker.failures().is_empty());
    assert!(worker.queue.pop_due(GUILD, 1_000).is_none());
    assert!(!worker.dispatch_one(1_000).await, "nothing is due yet");
    assert_eq!(limit_writes(&trace), ["limit:500:3"], "no early retry");
    assert!(worker.dispatch_one(10_000).await);
    assert_eq!(limit_writes(&trace), ["limit:500:3", "limit:500:3"]);
    assert_eq!(worker.queue.pending_counts(GUILD), (0, 0));
}

#[tokio::test]
async fn a_terminal_discord_refusal_is_reported_and_recorded() {
    let (live, store, http, trace) = owned_room();
    http.limit_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::AccessDenied);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let (text, ack) = run_once(
        &mut worker,
        MEMBER,
        false,
        LimitCommand::Limit(LimitArg::Value(3)),
    )
    .await;
    assert_eq!(ack, Some(LimitAck::Refused(RoomHttpError::AccessDenied)));
    assert!(text.contains("Manage Channels"), "{text}");
    assert!(text.contains("Nothing was changed"), "{text}");
    // The write left the queue for good and surfaces under `/setup`.
    assert_eq!(worker.queue.pending_counts(GUILD), (0, 0));
    assert_eq!(limit_writes(&trace), ["limit:500:3"]);
    assert!(worker.failures().contains(&LifecycleFailure::Discord {
        channel_id: 500,
        error: RoomHttpError::AccessDenied,
    }));
    // The live limit still shows what Discord has.
    let live = worker.live.inner.read().unwrap();
    assert_eq!(live.channels.get(&500).unwrap().user_limit, Some(8));
}

#[tokio::test]
async fn an_unknown_outcome_retries_then_dead_letters_in_the_limit_family() {
    let (live, store, http, _trace) = owned_room();
    for _ in 0..QUEUE_MAX_ATTEMPTS {
        http.limit_errors
            .lock()
            .unwrap()
            .push_back(RoomHttpError::UnknownOutcome);
    }
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let series = "two_bot_voice_dead_letters_total{action=\"limit\"}";
    let before = global_series(series);
    let LimitReply::Queued { ack, .. } =
        worker.apply_limit(MEMBER, false, LimitCommand::Limit(LimitArg::Value(3)))
    else {
        panic!("the limit must queue");
    };
    for attempt in 0..QUEUE_MAX_ATTEMPTS {
        assert!(worker.dispatch_one(u64::from(attempt) * 120_000).await);
    }
    // The caller was told it is queued on the first attempt.
    assert_eq!(ack.await.ok(), Some(LimitAck::Delayed));
    assert_eq!(worker.queue.pending_counts(GUILD), (0, 0));
    let failed = worker.queue.failed();
    assert_eq!(failed.len(), 1);
    assert_eq!(
        failed[0].action.action,
        RoomAction::SetUserLimit {
            channel_id: 500,
            user_limit: 3,
        }
    );
    assert_eq!(global_series(series), before + 1);
}

#[tokio::test]
async fn a_write_for_a_room_that_is_gone_is_dropped() {
    let (live, store, http, trace) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let LimitReply::Queued { ack, .. } = worker.apply_limit(MEMBER, false, LimitCommand::Unlimit)
    else {
        panic!("the limit must queue");
    };
    worker.rooms.remove(&500);
    assert!(worker.dispatch_one(0).await);
    assert_eq!(ack.await.ok(), Some(LimitAck::Obsolete));
    assert!(limit_writes(&trace).is_empty());
    assert_eq!(worker.queue.pending_counts(GUILD), (0, 0));
}

#[tokio::test]
async fn closed_acknowledgements_are_pruned_on_the_next_command() {
    let (live, store, http, _trace) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    for _ in 0..3 {
        // The caller gives up at once: the receiver is dropped.
        drop(worker.apply_limit(MEMBER, false, LimitCommand::Unlimit));
    }
    let LimitReply::Queued { ack, .. } = worker.apply_limit(MEMBER, false, LimitCommand::Unlimit)
    else {
        panic!("the limit must queue");
    };
    assert_eq!(worker.limit_acks.len(), 1, "only the live waiter remains");
    drop(ack);
}

// --- interaction path through the real actor ------------------------------------

/// Runtime whose single guild actor shares `access` and scripts `errors` for
/// the limit write, warmed up like `ownership_room_runtime`: one tracked room
/// (channel 500, owner `MEMBER`) with a second occupant 301.
async fn limit_room_runtime(
    trace: Trace,
    access: Arc<Mutex<AccessControls>>,
    errors: Vec<RoomHttpError>,
) -> VoiceRuntime<Store, Http> {
    let make_trace = trace.clone();
    let runtime = VoiceRuntime::new(
        move || {
            let mut store = Store::new(make_trace.clone());
            store.access = access.clone();
            let http = Http::new(make_trace.clone());
            http.limit_errors.lock().unwrap().extend(errors.clone());
            (store, http)
        },
        Duration::from_millis(10),
        true,
    );
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    assert!(runtime.voice_frame(
        GUILD,
        MEMBER,
        Some(CREATOR),
        Some(false),
        "ava's room".to_owned(),
    ));
    wait_trace(&trace, "persist:500").await;
    assert!(runtime.voice_frame(GUILD, MEMBER, Some(500), Some(false), "x".to_owned()));
    assert!(runtime.voice_frame(GUILD, 301, Some(500), Some(false), "x".to_owned()));
    runtime
}

fn open_access() -> Arc<Mutex<AccessControls>> {
    Arc::new(Mutex::new(AccessControls::default()))
}

#[tokio::test]
async fn handle_limit_replies_after_the_write_lands() {
    let trace = Trace::default();
    let runtime = limit_room_runtime(trace.clone(), open_access(), Vec::new()).await;
    let interaction = limit_interaction(MEMBER, vec![int_option("count", 5)]);
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    let response = response.expect("reply");
    let text = response_text(&response);
    assert!(text.contains("limited to 5 people"), "{text}");
    // Ephemeral, like every voice reply.
    assert_eq!(
        response.data.as_ref().and_then(|data| data.flags),
        Some(twilight_model::channel::message::MessageFlags::EPHEMERAL)
    );
    assert_eq!(limit_writes(&trace), ["limit:500:5"]);
}

#[tokio::test]
async fn handle_unlimit_by_a_non_owner_refuses() {
    let trace = Trace::default();
    let runtime = limit_room_runtime(trace.clone(), open_access(), Vec::new()).await;
    let interaction = voice_interaction_as(Some(command_data("unlimit", Vec::new())), None, 301);
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    let text = response_text(response.as_ref().expect("reply"));
    assert!(text.contains("Only the room owner or an admin"), "{text}");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(limit_writes(&trace).is_empty());
}

#[tokio::test]
async fn handle_limit_reports_a_terminal_discord_refusal() {
    let trace = Trace::default();
    let runtime = limit_room_runtime(
        trace.clone(),
        open_access(),
        vec![RoomHttpError::AccessDenied],
    )
    .await;
    let interaction = limit_interaction(MEMBER, vec![int_option("count", 5)]);
    let (_, response) = handle_capture(&runtime, &interaction).await;
    let text = response_text(response.as_ref().expect("reply"));
    assert!(text.contains("Manage Channels"), "{text}");
    assert!(text.contains("Nothing was changed"), "{text}");
}

#[tokio::test]
async fn handle_limit_reports_a_rate_limited_write_as_queued_then_applies_it() {
    let trace = Trace::default();
    let runtime = limit_room_runtime(
        trace.clone(),
        open_access(),
        vec![RoomHttpError::RateLimited {
            retry_after_ms: 50,
            global: false,
        }],
    )
    .await;
    let interaction = limit_interaction(MEMBER, vec![int_option("count", 5)]);
    let (_, response) = handle_capture(&runtime, &interaction).await;
    let text = response_text(response.as_ref().expect("reply"));
    assert!(text.contains("Discord is busy"), "{text}");
    // The retry after the 429 lands on a later tick.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while limit_writes(&trace).len() < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "retry never ran: {:?}",
            trace.lock().unwrap()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(limit_writes(&trace), ["limit:500:5", "limit:500:5"]);
}

#[tokio::test]
async fn the_per_command_role_gate_covers_limit_and_unlimit() {
    let trace = Trace::default();
    let access = open_access();
    {
        let mut controls = access.lock().unwrap();
        controls.command_roles.insert("limit".to_owned(), vec![7]);
        controls.command_roles.insert("unlimit".to_owned(), vec![7]);
    }
    let runtime = limit_room_runtime(trace.clone(), access, Vec::new()).await;
    // The owner lacks role 7: both commands are refused before the worker.
    for interaction in [
        limit_interaction(MEMBER, vec![int_option("count", 5)]),
        voice_interaction_as(Some(command_data("unlimit", Vec::new())), None, MEMBER),
    ] {
        let (owned, response) = handle_capture(&runtime, &interaction).await;
        assert!(owned);
        let text = response_text(response.as_ref().expect("reply"));
        assert!(!text.is_empty() && !text.contains("limited to"), "{text}");
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(limit_writes(&trace).is_empty());
    // With the role the same command goes through.
    let mut allowed = limit_interaction(MEMBER, vec![int_option("count", 5)]);
    allowed.member.as_mut().expect("member").roles = vec![Id::new(7)];
    let (_, response) = handle_capture(&runtime, &allowed).await;
    let text = response_text(response.as_ref().expect("reply"));
    assert!(text.contains("limited to 5 people"), "{text}");
    assert_eq!(limit_writes(&trace), ["limit:500:5"]);
}

// --- concurrency regressions (independent review) -------------------------------

fn room_members() -> Vec<VoiceMember> {
    vec![
        VoiceMember {
            member_id: MEMBER,
            channel_id: 500,
            bot: Some(false),
        },
        VoiceMember {
            member_id: 301,
            channel_id: 500,
            bot: Some(false),
        },
    ]
}

#[tokio::test]
async fn a_newer_gateway_update_survives_a_late_rest_completion() {
    let (live, store, mut http, trace) = owned_room();
    // The gateway republishes the room (still limit 8) while the PATCH for 5
    // is in flight: the revision moves, so the completion must not roll the
    // cache back to the just-written value.
    let gateway = live.clone();
    http.before_limit = Some(Arc::new(move || {
        gateway.upsert_channel(channel(500, 2, Some(CATEGORY)));
    }));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let (text, ack) = run_once(
        &mut worker,
        MEMBER,
        false,
        LimitCommand::Limit(LimitArg::Value(5)),
    )
    .await;
    assert_eq!(ack, Some(LimitAck::Applied));
    assert!(text.contains("limited to 5 people"), "{text}");
    {
        let live = worker.live.inner.read().unwrap();
        assert_eq!(live.channels.get(&500).unwrap().user_limit, Some(8));
    }
    // ...so asking for 5 again still writes instead of falsely skipping.
    let (text, ack) = run_once(
        &mut worker,
        MEMBER,
        false,
        LimitCommand::Limit(LimitArg::Value(5)),
    )
    .await;
    assert_eq!(ack, Some(LimitAck::Applied));
    assert!(text.contains("limited to 5 people"), "{text}");
    assert_eq!(limit_writes(&trace), ["limit:500:5", "limit:500:5"]);
}

#[tokio::test]
async fn a_disconnect_republication_survives_a_late_rest_completion() {
    let (live, store, mut http, trace) = owned_room();
    // A disconnect plus a fresh snapshot lands while the PATCH for 5 is in
    // flight: the generation moves, so the completion must not overwrite the
    // republished evidence.
    let gateway = live.clone();
    http.before_limit = Some(Arc::new(move || {
        gateway.disconnect();
        assert!(gateway.publish(snapshot(&[500], room_members())));
    }));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let (text, ack) = run_once(
        &mut worker,
        MEMBER,
        false,
        LimitCommand::Limit(LimitArg::Value(5)),
    )
    .await;
    assert_eq!(ack, Some(LimitAck::Applied));
    assert!(text.contains("limited to 5 people"), "{text}");
    assert_eq!(limit_writes(&trace), ["limit:500:5"]);
    let live = worker.live.inner.read().unwrap();
    assert_eq!(live.channels.get(&500).unwrap().user_limit, Some(8));
}

#[tokio::test]
async fn a_transient_guard_loss_requeues_instead_of_retiring() {
    let (live, store, mut http, trace) = owned_room();
    // The disconnect lands after the pre-write state check but before the
    // adapter's final guard: the room and channel are still tracked.
    let gateway = live.clone();
    http.before_limit = Some(Arc::new(move || {
        gateway.disconnect();
    }));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let LimitReply::Queued { ack, .. } =
        worker.apply_limit(MEMBER, false, LimitCommand::Limit(LimitArg::Value(5)))
    else {
        panic!("the limit must queue");
    };
    assert!(worker.dispatch_one(0).await);
    // Resumable, not retired: the caller is told it is queued, nothing was
    // written, and the action waits for the reconnect without a failure.
    assert_eq!(ack.await.ok(), Some(LimitAck::Delayed));
    assert!(limit_writes(&trace).is_empty());
    assert_eq!(worker.queue.pending_counts(GUILD), (1, 0));
    assert!(worker.failures().is_empty());
    // The reconnect retries the same write to completion.
    worker.http.before_limit = None;
    worker.live.publish(snapshot(&[500], room_members()));
    assert!(worker.dispatch_one(0).await);
    assert_eq!(limit_writes(&trace), ["limit:500:5"]);
    assert_eq!(worker.queue.pending_counts(GUILD), (0, 0));
    let live = worker.live.inner.read().unwrap();
    assert_eq!(live.channels.get(&500).unwrap().user_limit, Some(5));
}

#[tokio::test]
async fn a_not_found_write_invalidates_the_cached_channel() {
    let (live, store, http, trace) = owned_room();
    http.limit_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::NotFound);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    let (text, ack) = run_once(
        &mut worker,
        MEMBER,
        false,
        LimitCommand::Limit(LimitArg::Value(3)),
    )
    .await;
    assert_eq!(ack, Some(LimitAck::Refused(RoomHttpError::NotFound)));
    assert!(text.contains("no longer exists"), "{text}");
    assert_eq!(limit_writes(&trace), ["limit:500:3"]);
    assert!(worker.failures().contains(&LifecycleFailure::Discord {
        channel_id: 500,
        error: RoomHttpError::NotFound,
    }));
    // The cached channel is gone: a command matching the old cached value
    // must refuse, never report Applied without a write.
    assert!(!worker
        .live
        .inner
        .read()
        .unwrap()
        .channels
        .contains_key(&500));
    let (text, ack) = run_once(
        &mut worker,
        MEMBER,
        false,
        LimitCommand::Limit(LimitArg::Value(8)),
    )
    .await;
    assert_eq!(ack, None);
    assert!(!text.contains("limited to 8"), "{text}");
    assert_eq!(limit_writes(&trace), ["limit:500:3"], "no second write");
}

#[tokio::test]
async fn a_delete_overtaking_a_queued_limit_settles_its_waiter() {
    let (live, store, http, trace) = owned_room();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    // This scenario predates the ordinary empty-room grace and asserts the
    // delete runs first: shorten the grace to zero via the fixture-only seam
    // so reconcile queues the delete immediately. Production keeps the default.
    worker.live.set_empty_grace(Duration::ZERO);
    // Everyone leaves: reconcile queues the delete ahead of any new command.
    worker
        .live
        .voice_update_at(MEMBER, None, Some(false), 1_000);
    worker.live.voice_update_at(301, None, Some(false), 1_000);
    worker.reconcile();
    // The owner briefly rejoins and queues a limit behind the delete, then
    // leaves before anything dispatches.
    worker
        .live
        .voice_update_at(MEMBER, Some(500), Some(false), 2_000);
    let LimitReply::Queued { limit, ack } =
        worker.apply_limit(MEMBER, false, LimitCommand::Limit(LimitArg::Value(5)))
    else {
        panic!("the limit must queue behind the delete");
    };
    worker
        .live
        .voice_update_at(MEMBER, None, Some(false), 3_000);
    assert!(worker.dispatch_one(4_000).await, "the delete runs first");
    // The delete discarded the limit: its waiter ends obsolete with no
    // queued-success wording, and nothing is retained.
    assert_eq!(ack.await.ok(), Some(LimitAck::Obsolete));
    assert!(worker.limit_acks.is_empty());
    assert_eq!(worker.queue.pending_counts(GUILD), (0, 0));
    assert_eq!(
        outcome_text(
            LimitCommand::Limit(LimitArg::Value(5)),
            limit,
            Some(LimitAck::Obsolete)
        ),
        "That room is gone or has changed, so nothing was changed."
    );
    assert!(limit_writes(&trace).is_empty());
}

#[tokio::test]
async fn a_held_actor_still_answers_within_the_reply_budget() {
    let trace = Trace::default();
    let gate = Arc::new(LimitGate::default());
    let make_trace = trace.clone();
    let factory_gate = gate.clone();
    let access = open_access();
    let runtime = VoiceRuntime::new(
        move || {
            let mut store = Store::new(make_trace.clone());
            store.access = access.clone();
            let mut http = Http::new(make_trace.clone());
            http.limit_gate = Some(factory_gate.clone());
            (store, http)
        },
        Duration::from_millis(10),
        true,
    );
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    assert!(runtime.voice_frame(
        GUILD,
        MEMBER,
        Some(CREATOR),
        Some(false),
        "ava's room".to_owned(),
    ));
    wait_trace(&trace, "persist:500").await;
    assert!(runtime.voice_frame(GUILD, MEMBER, Some(500), Some(false), "x".to_owned()));
    assert!(runtime.voice_frame(GUILD, 301, Some(500), Some(false), "x".to_owned()));
    // The first command queues and its write blocks in the fake, holding the
    // actor across ticks. The second command must answer unconfirmed within
    // the reply budget instead of waiting on actor acceptance.
    let interaction = limit_interaction(MEMBER, vec![int_option("count", 5)]);
    let first = handle_capture(&runtime, &interaction);
    let second = async {
        gate.sent.notified().await;
        runtime
            .run_limit(
                GUILD,
                MEMBER,
                false,
                LimitCommand::Limit(LimitArg::Value(3)),
            )
            .await
    };
    let ((owned, response), unconfirmed) = tokio::join!(first, second);
    assert!(owned);
    let queued = response_text(response.as_ref().expect("reply"));
    assert!(queued.contains("Discord is busy"), "{queued}");
    let unconfirmed = unconfirmed.expect("reply");
    assert!(unconfirmed.contains("couldn't confirm"), "{unconfirmed}");
    // Releasing the held write still lands it: the budget never cancels work.
    gate.release.notify_one();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while limit_writes(&trace).is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "held write never ran: {:?}",
            trace.lock().unwrap()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(limit_writes(&trace), ["limit:500:5"]);
}
