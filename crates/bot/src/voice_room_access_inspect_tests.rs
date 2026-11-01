//! Builder-owned pins for `/room-access-inspect` handler behavior outside the
//! protected acceptance module: unusable `page` shapes refuse through real
//! slash parsing and dispatch (never silently serving page 1), past-end pages
//! name the real total, and a missing, dead or stalled guild actor reports
//! warming-up or worker-lost instead of an empty room. An open-but-stalled
//! inbox must time out within the acknowledgement bound, never hang.
//! No protected file is touched here.
#![cfg(test)]

use super::*;
use two_bot_core::voice_private::{ChannelId, JoinRequest, MemberId, RequestId};

const ROOM: u64 = 500;
type Worker = GuildRoomWorker<Store, Http>;

async fn owned_worker() -> Worker {
    let (live, store, http, _) = owned_room();
    GuildRoomWorker::load(live, store, http)
        .await
        .expect("load owner fixture")
}

fn seed_access(worker: &mut Worker, blocked: &[u64], granted: &[u64], pending: &[u64]) {
    let mut room = PrivateRoom::new(ChannelId(ROOM), MemberId(MEMBER), "owner");
    room.blocked = blocked.iter().map(|id| MemberId(*id)).collect();
    room.granted = granted.iter().map(|id| MemberId(*id)).collect();
    for (index, member) in pending.iter().enumerate() {
        room.pending.insert(
            MemberId(*member),
            JoinRequest {
                id: RequestId(index as u64 + 1),
                member_id: MemberId(*member),
                owner_id: MemberId(MEMBER),
            },
        );
    }
    room.next_request_id = pending.len() as u64 + 1;
    room.validate()
        .expect("internally consistent access fixture");
    worker.privacy.insert(ROOM, room);
}

fn page_option(value: CommandOptionValue) -> CommandDataOption {
    CommandDataOption {
        name: "page".to_owned(),
        value,
    }
}

fn inspect_command(options: Vec<CommandDataOption>) -> VoiceCommand {
    let interaction = voice_interaction_as(
        Some(command_data("room-access-inspect", options)),
        None,
        MEMBER,
    );
    parse_voice_command(&interaction).expect("inspect parses")
}

async fn invoke(runtime: &VoiceRuntime<Store, Http>, options: Vec<CommandDataOption>) -> String {
    let interaction = voice_interaction_as(
        Some(command_data("room-access-inspect", options)),
        None,
        MEMBER,
    );
    let responses = Arc::new(Mutex::new(Vec::new()));
    let captured = responses.clone();
    let owned = handle_voice_interaction(runtime, &interaction, move |response| {
        captured.lock().unwrap().push(response);
        async {}
    })
    .await;
    assert!(owned, "inspect must be handled, not silently dropped");
    let responses = responses.lock().unwrap();
    assert_eq!(responses.len(), 1, "exactly one reply");
    assert_private_plain_reply(&responses[0])
}

/// Every reply, fallbacks included, is ephemeral plain text within Discord's
/// content bound and offers no component that could mutate access.
fn assert_private_plain_reply(response: &InteractionResponse) -> String {
    let data = response.data.as_ref().expect("reply data");
    assert!(
        data.flags
            .unwrap_or(MessageFlags::empty())
            .contains(MessageFlags::EPHEMERAL),
        "room access replies must not be public"
    );
    assert!(
        data.components.as_ref().is_none_or(Vec::is_empty),
        "a room access reply must not carry components"
    );
    let text = response_text(response);
    assert!(!text.trim().is_empty(), "reply cannot be empty");
    assert!(text.encode_utf16().count() <= 2000, "reply over bound");
    text
}

/// A runtime whose guild actor is a real worker on the owner fixture, so a
/// dispatched read reaches `apply_room_access`.
fn live_runtime(mut worker: Worker) -> VoiceRuntime<Store, Http> {
    let runtime = test_runtime(worker.store.trace.clone());
    let (tx, rx) = mpsc::unbounded_channel();
    runtime.actors.lock().unwrap().insert(
        GUILD,
        GuildActor {
            live: worker.live.clone(),
            tx,
            backpressure: ActorInboxBackpressure::default(),
        },
    );
    tokio::spawn(async move {
        run_actor(&mut worker, rx, Duration::from_secs(3600)).await;
    });
    runtime
}

fn assert_no_access_sections(text: &str) {
    for heading in ["**Blocked", "**Granted", "**Pending"] {
        assert!(
            !text.contains(heading),
            "a fallback must never look like a read: {text}"
        );
    }
}

#[test]
fn decode_rejects_unusable_page_shapes() {
    for options in [
        vec![page_option(CommandOptionValue::Integer(0))],
        vec![page_option(CommandOptionValue::Integer(-1))],
        vec![page_option(CommandOptionValue::String("2".to_owned()))],
        vec![
            page_option(CommandOptionValue::Integer(1)),
            page_option(CommandOptionValue::Integer(2)),
        ],
    ] {
        assert!(
            matches!(
                inspect_command(options),
                VoiceCommand::RoomAccessInspect(PageArg::Invalid)
            ),
            "unusable page shapes must decode Invalid, never page 1"
        );
    }
}

#[test]
fn decode_keeps_huge_page_as_past_end_candidate() {
    let huge = usize::try_from(i64::MAX).expect("64-bit page index");
    assert!(
        matches!(
            inspect_command(vec![page_option(CommandOptionValue::Integer(i64::MAX))]),
            VoiceCommand::RoomAccessInspect(PageArg::Page(number)) if number == huge
        ),
        "a huge well-typed page stays Page and refuses past-end at dispatch"
    );
}

#[tokio::test]
async fn invalid_page_refuses_without_listing() {
    let worker = owned_worker().await;
    assert_eq!(
        worker.apply_room_access(MEMBER, false, PageArg::Invalid),
        "Unknown page. Use /room-access-inspect page:1 to see the first page."
    );
}

#[tokio::test]
async fn past_end_page_names_the_real_total() {
    let mut worker = owned_worker().await;
    seed_access(&mut worker, &[81_000_000_000_000_001], &[], &[]);
    assert_eq!(
        worker.apply_room_access(MEMBER, false, PageArg::Page(2)),
        "Unknown page. /room-access-inspect has 1 page; \
         use /room-access-inspect page:1 to start over."
    );
    let huge = usize::try_from(i64::MAX).expect("64-bit page index");
    let text = worker.apply_room_access(MEMBER, false, PageArg::Page(huge));
    assert!(
        text.starts_with("Unknown page. /room-access-inspect has 1 page;"),
        "huge pages refuse past-end, never wrap: {text}"
    );
}

#[tokio::test]
async fn past_end_page_names_the_real_total_on_long_lists() {
    let mut worker = owned_worker().await;
    let blocked: Vec<u64> = (0..300).map(|n| 81_000_000_000_000_000 + n).collect();
    seed_access(&mut worker, &blocked, &[], &[]);
    let first = worker.apply_room_access(MEMBER, false, PageArg::Missing);
    let total: usize = first
        .rsplit_once("Page 1 of ")
        .and_then(|(_, tail)| tail.lines().next()?.parse().ok())
        .expect("multi-page footer");
    assert!(total > 1, "fixture must span pages: {first}");
    let text = worker.apply_room_access(MEMBER, false, PageArg::Page(total + 1));
    assert!(
        text.contains(&format!("has {total} pages;")),
        "past-end must name the real total: {text}"
    );
}

#[tokio::test]
async fn inspect_without_actor_reports_warming_up() {
    let runtime = test_runtime(Trace::default());
    let text = invoke(&runtime, vec![]).await;
    assert_eq!(
        text,
        "The voice worker isn't warmed up yet — try again in a moment."
    );
    assert_no_access_sections(&text);
}

#[tokio::test]
async fn inspect_with_dead_actor_reports_worker_lost() {
    let runtime = test_runtime(Trace::default());
    let (live, _, _, _) = fixture();
    let (tx, rx) = mpsc::unbounded_channel();
    drop(rx);
    runtime.actors.lock().unwrap().insert(
        GUILD,
        GuildActor {
            live: live.clone(),
            tx,
            backpressure: ActorInboxBackpressure::default(),
        },
    );
    let text = invoke(&runtime, vec![]).await;
    assert_eq!(
        text,
        "The voice worker stopped answering — try again in a moment."
    );
    assert_no_access_sections(&text);
}

#[tokio::test(start_paused = true)]
async fn inspect_with_stalled_actor_reports_worker_lost_within_ack_wait() {
    let (runtime, mut rx) = stalled_worker_runtime();
    let interaction = voice_interaction_as(
        Some(command_data("room-access-inspect", vec![])),
        None,
        MEMBER,
    );
    let responses = Arc::new(Mutex::new(Vec::new()));
    let captured = responses.clone();
    let call = handle_voice_interaction(&runtime, &interaction, move |response| {
        captured.lock().unwrap().push(response);
        async {}
    });
    tokio::pin!(call);
    let command = tokio::select! {
        command = rx.recv() => command.expect("accepted inspect command"),
        _ = &mut call => panic!("inspect handler did not reach its worker boundary"),
    };
    assert!(
        matches!(command, ActorCommand::RoomAccess { .. }),
        "the stalled inbox must hold the inspect read"
    );
    // Hold the accepted command without answering: the inbox stays open but
    // stalled, so the read must time out instead of hanging.
    let held = Some(command);
    let owned = bounded_sibling_wait(&mut call).await;
    assert!(owned, "inspect must be handled, not silently dropped");
    let responses = responses.lock().unwrap();
    assert_eq!(responses.len(), 1, "exactly one reply");
    let text = assert_private_plain_reply(&responses[0]);
    assert_eq!(
        text,
        "The voice worker stopped answering — try again in a moment."
    );
    assert_no_access_sections(&text);
    drop(held);
}

#[tokio::test]
async fn unusable_page_refuses_through_dispatch() {
    let runtime = live_runtime(owned_worker().await);
    for options in [
        vec![page_option(CommandOptionValue::Integer(0))],
        vec![page_option(CommandOptionValue::String("2".to_owned()))],
        vec![
            page_option(CommandOptionValue::Integer(1)),
            page_option(CommandOptionValue::Integer(2)),
        ],
    ] {
        assert_eq!(
            invoke(&runtime, options).await,
            "Unknown page. Use /room-access-inspect page:1 to see the first page."
        );
    }
    let text = invoke(&runtime, vec![page_option(CommandOptionValue::Integer(2))]).await;
    assert_eq!(
        text,
        "Unknown page. /room-access-inspect has 1 page; \
         use /room-access-inspect page:1 to start over."
    );
}

#[tokio::test]
async fn non_owner_page_errors_stay_behind_the_owner_gate() {
    let mut worker = owned_worker().await;
    let blocked: Vec<u64> = (0..300).map(|n| 81_000_000_000_000_000 + n).collect();
    seed_access(&mut worker, &blocked, &[], &[]);
    let refusal = worker.apply_room_access(301, false, PageArg::Missing);
    assert_eq!(
        refusal,
        "Only the room owner or an admin can use /room-access-inspect."
    );
    for page in [PageArg::Page(99_999), PageArg::Invalid] {
        assert_eq!(
            worker.apply_room_access(301, false, page),
            refusal,
            "a non-owner must not learn the page count"
        );
    }
}
