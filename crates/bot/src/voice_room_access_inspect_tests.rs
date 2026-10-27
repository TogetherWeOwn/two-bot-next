//! Builder-owned pins for `/room-access-inspect` handler behavior outside the
//! protected acceptance module: unusable `page` shapes refuse through real
//! slash parsing and dispatch (never silently serving page 1), past-end pages
//! name the real total, and a missing or dead guild actor reports warming-up
//! or worker-lost instead of an empty room. No protected file is touched here.
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
    let owned = handle_voice_interaction(&runtime, &interaction, move |response| {
        captured.lock().unwrap().push(response);
        async {}
    })
    .await;
    assert!(owned, "inspect must be handled, not silently dropped");
    let responses = responses.lock().unwrap();
    assert_eq!(responses.len(), 1, "exactly one reply");
    response_text(&responses[0])
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
