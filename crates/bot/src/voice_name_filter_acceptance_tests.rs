//! Acceptance tests: member-requested renames pass the same name filter as
//! every other naming path.
//!
//! The fixture blocked term is the invented word `blorp`; no real word list,
//! database, or Discord call is involved. Every hostile case drives the real
//! dispatch path (`RenameRoom`, `SetCustomName`, and the runtime modal
//! submit) and must end with a `name_blocked` refusal and zero Discord
//! rename calls. Allowed controls must keep the exact pre-existing
//! coalescer/reply behaviour.

use super::*;
use twilight_model::application::interaction::modal::{
    ModalInteractionComponent, ModalInteractionData, ModalInteractionLabel,
    ModalInteractionTextInput,
};
use two_bot_core::{voice_custom_id::name_modal_custom_id, voice_room_name::NAME_INPUT_ID};

/// Blocked-term evasions the core filter is documented to see through:
/// the plain token, mixed case, and separator confusion (inner spaces,
/// zero-width gaps, full-width forms, trailing punctuation), plus an
/// invite link.
const HOSTILE_NAMES: &[&str] = &[
    "blorp den",
    "BlOrP den",
    "b l o r p den",
    "b\u{200B}l\u{200C}orp den",
    "\u{FF22}\u{FF2C}\u{FF2F}\u{FF32}\u{FF30} den",
    "blorp! den",
    "join discord.gg/abc123",
];

/// Names that must keep renaming exactly as before, including the word
/// boundary: a longer word merely containing the blocked term passes.
const ALLOWED_NAMES: &[&str] = &["Cozy Den", "blorpish den"];

fn voice_names_outcome(outcome: &str) -> u64 {
    global_series(&format!(
        "two_bot_voice_names_total{{outcome=\"{outcome}\"}}"
    ))
}

/// A refused rename must be observable under the stable `name_blocked`
/// audit label: in the tracing output emitted while dispatching, in the
/// worker failure list, or as a voice-names metric outcome (`name_blocked`
/// or the pre-existing template `blocked` outcome). Counters are
/// process-global, so metric checks are lower bounds.
fn name_blocked_recorded(
    worker: &GuildRoomWorker<Store, Http>,
    log_text: &str,
    blocked_before: u64,
    named_before: u64,
) -> bool {
    log_text.contains("name_blocked")
        || worker
            .failures()
            .iter()
            .any(|failure| failure_line(failure).contains("name_blocked"))
        || voice_names_outcome("blocked") > blocked_before
        || voice_names_outcome("name_blocked") > named_before
}

async fn rename_worker() -> (GuildRoomWorker<Store, Http>, Trace) {
    let (live, store, http, trace) = fixture();
    slow_rename_room(&live, &store);
    let worker = GuildRoomWorker::load(live, store, http)
        .await
        .unwrap()
        .with_name_policy(Arc::new(fixture_policy(&["blorp"])));
    (worker, trace)
}

#[tokio::test]
async fn rename_room_dispatch_refuses_hostile_names_with_zero_discord_calls() {
    for hostile in HOSTILE_NAMES {
        let (mut worker, trace) = rename_worker().await;
        worker.desired_names.insert(700, (*hostile).to_owned());
        worker.queue.enqueue(
            GUILD,
            RoomAction::RenameRoom {
                channel_id: 700,
                name: (*hostile).to_owned(),
            },
        );
        let blocked_before = voice_names_outcome("blocked");
        let named_before = voice_names_outcome("name_blocked");
        let global = super::tracing_capture::global();
        let log_before = global.text().len();
        dispatch(&mut worker, 0).await;
        let log_text = global.text()[log_before..].to_owned();
        assert!(
            renames_sent(&trace).is_empty(),
            "{hostile:?}: a blocked rename must send no Discord rename, trace: {:?}",
            trace.lock().unwrap()
        );
        assert!(
            name_blocked_recorded(&worker, &log_text, blocked_before, named_before),
            "{hostile:?}: refusal must be recorded as name_blocked"
        );
    }
}

#[tokio::test]
async fn rename_room_dispatch_still_sends_allowed_names() {
    for allowed in ALLOWED_NAMES {
        let (mut worker, trace) = rename_worker().await;
        worker.desired_names.insert(700, (*allowed).to_owned());
        worker.queue.enqueue(
            GUILD,
            RoomAction::RenameRoom {
                channel_id: 700,
                name: (*allowed).to_owned(),
            },
        );
        dispatch(&mut worker, 0).await;
        assert_eq!(
            renames_sent(&trace),
            [format!("rename:700:{allowed}")],
            "{allowed:?}: an allowed rename behaves exactly as before"
        );
    }
}

async fn custom_name_worker() -> (GuildRoomWorker<Store, Http>, Trace) {
    let (live, store, http, trace) = owned_room();
    let worker = GuildRoomWorker::load(live, store, http)
        .await
        .unwrap()
        .with_name_policy(Arc::new(fixture_policy(&["blorp"])));
    (worker, trace)
}

async fn drain_custom(worker: &mut GuildRoomWorker<Store, Http>) {
    for step in 0..8 {
        worker.dispatch_one(step * 1_000_000).await;
    }
}

#[tokio::test]
async fn set_custom_name_dispatch_refuses_hostile_names_with_zero_discord_calls() {
    for hostile in HOSTILE_NAMES {
        let (mut worker, trace) = custom_name_worker().await;
        worker.queue.enqueue(
            GUILD,
            RoomAction::SetCustomName {
                channel_id: 500,
                custom_name: Some((*hostile).to_owned()),
            },
        );
        let blocked_before = voice_names_outcome("blocked");
        let named_before = voice_names_outcome("name_blocked");
        let global = super::tracing_capture::global();
        let log_before = global.text().len();
        drain_custom(&mut worker).await;
        let log_text = global.text()[log_before..].to_owned();
        assert!(
            renames_sent(&trace).is_empty(),
            "{hostile:?}: a blocked custom name must send no Discord rename, trace: {:?}",
            trace.lock().unwrap()
        );
        assert!(
            !worker
                .store
                .custom_names
                .lock()
                .unwrap()
                .values()
                .any(|saved| saved == hostile),
            "{hostile:?}: a blocked custom name must not be persisted"
        );
        assert!(
            name_blocked_recorded(&worker, &log_text, blocked_before, named_before),
            "{hostile:?}: refusal must be recorded as name_blocked"
        );
    }
}

#[tokio::test]
async fn set_custom_name_dispatch_still_applies_allowed_names() {
    for allowed in ALLOWED_NAMES {
        let (mut worker, trace) = custom_name_worker().await;
        worker.queue.enqueue(
            GUILD,
            RoomAction::SetCustomName {
                channel_id: 500,
                custom_name: Some((*allowed).to_owned()),
            },
        );
        drain_custom(&mut worker).await;
        assert_eq!(
            renames_sent(&trace),
            [format!("rename:500:{allowed}")],
            "{allowed:?}: an allowed custom name behaves exactly as before"
        );
        assert_eq!(
            worker
                .store
                .custom_names
                .lock()
                .unwrap()
                .get(&500)
                .map(String::as_str),
            Some(*allowed),
            "{allowed:?}: an allowed custom name is still persisted"
        );
    }
}

// --- runtime: the member-facing modal submit --------------------------------

fn name_directory() -> NameDirectory {
    let mut directory = NameDirectory::default();
    directory.insert(MEMBER, "Ava".to_owned());
    directory.insert(301, "Ben".to_owned());
    directory
}

fn modal_submit(text: &str) -> Interaction {
    let mut interaction = with_user(voice_interaction(None, None, true), MEMBER);
    interaction.kind = InteractionType::ModalSubmit;
    interaction.data = Some(InteractionData::ModalSubmit(Box::new(
        ModalInteractionData {
            components: vec![ModalInteractionComponent::Label(ModalInteractionLabel {
                id: 1,
                component: Box::new(ModalInteractionComponent::TextInput(
                    ModalInteractionTextInput {
                        custom_id: NAME_INPUT_ID.to_owned(),
                        id: 2,
                        value: text.to_owned(),
                    },
                )),
            })],
            custom_id: name_modal_custom_id(500),
            resolved: None,
        },
    )));
    interaction
}

async fn capture_name(
    runtime: &VoiceRuntime<Store, Http>,
    interaction: &Interaction,
    names: &NameDirectory,
) -> (bool, Option<InteractionResponse>) {
    let seen = Arc::new(Mutex::new(None::<InteractionResponse>));
    let writer = seen.clone();
    let owned =
        handle_voice_interaction_full(runtime, interaction, None, None, names, |response| {
            *writer.lock().unwrap() = Some(response);
            async {}
        })
        .await;
    let response = seen.lock().unwrap().clone();
    (owned, response)
}

/// `ownership_room_runtime` with the fixture name policy installed: room
/// 500 exists, owned by `MEMBER`, with `MEMBER` and 301 inside it.
async fn filtering_room_runtime(trace: Trace) -> VoiceRuntime<Store, Http> {
    let runtime = test_runtime(trace.clone()).with_name_policy(fixture_policy(&["blorp"]));
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

#[tokio::test]
async fn custom_name_modal_refuses_hostile_names_and_sends_no_rename() {
    for hostile in HOSTILE_NAMES {
        let trace = Trace::default();
        let runtime = filtering_room_runtime(trace.clone()).await;
        let names = name_directory();
        let settled = trace.lock().unwrap().len();
        let (owned, response) = capture_name(&runtime, &modal_submit(hostile), &names).await;
        assert!(owned, "{hostile:?}");
        let text = response_text(response.as_ref().expect("reply"));
        assert!(
            text.contains("not allowed"),
            "{hostile:?}: the member gets the not-allowed refusal, got: {text}"
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        let after = trace.lock().unwrap()[settled..].to_vec();
        assert!(
            !after
                .iter()
                .any(|entry| entry.starts_with("rename:") || entry.starts_with("save_custom_name")),
            "{hostile:?}: a refused submit writes nothing and renames nothing, trace: {after:?}"
        );
    }
}

#[tokio::test]
async fn custom_name_modal_still_applies_allowed_names_with_the_same_reply() {
    for allowed in ALLOWED_NAMES {
        let trace = Trace::default();
        let runtime = filtering_room_runtime(trace.clone()).await;
        let names = name_directory();
        let (owned, response) = capture_name(&runtime, &modal_submit(allowed), &names).await;
        assert!(owned, "{allowed:?}");
        let text = response_text(response.as_ref().expect("reply"));
        assert!(
            text.contains(&format!("**{allowed}**")),
            "{allowed:?}: the applied reply is unchanged, got: {text}"
        );
        wait_trace(&trace, &format!("rename:500:{allowed}")).await;
    }
}
