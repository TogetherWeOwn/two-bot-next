//! V3 `/name` runtime tests. Discord and the database are faked behind
//! `RoomWrites` and `RoomPersistence`; nothing here touches a network.

use super::*;
use crate::voice_rooms::name_panel::{modal_response, panel_response, NameSettings};
use twilight_model::{
    application::interaction::modal::{
        ModalInteractionActionRow, ModalInteractionComponent, ModalInteractionData,
        ModalInteractionLabel, ModalInteractionTextInput,
    },
    channel::message::component::TextInput,
};
use two_bot_core::{
    voice_custom_id::{
        import_confirm_custom_id, join_custom_id, name_custom_custom_id, name_modal_custom_id,
        name_restore_custom_id, VOICE_CUSTOM_ID_PREFIX,
    },
    voice_private::JoinDecision,
    voice_room_name::{NameRefusal, MAX_CUSTOM_NAME_CHARS, NAME_INPUT_ID},
};

const ROOM: u64 = 500;
const OWNER: u64 = MEMBER;
const GUEST: u64 = 301;
const LOUNGE: u64 = 600;

fn directory() -> NameDirectory {
    let mut directory = NameDirectory::default();
    directory.insert(OWNER, "Ava".to_owned());
    directory.insert(GUEST, "Ben".to_owned());
    directory
}

fn policy(words: &[&str]) -> Arc<AutomodPolicy> {
    Arc::new(AutomodPolicy {
        bad_words: words.iter().map(ToString::to_string).collect(),
        ..AutomodPolicy::default()
    })
}

fn command(actor_id: u64, is_admin: bool, request: NameInteraction) -> NameCommand {
    NameCommand {
        actor_id,
        is_admin,
        request,
        settings: NameSettings::default(),
        directory: directory(),
        policy: policy(&[]),
    }
}

fn submit(text: &str) -> NameInteraction {
    NameInteraction::Submit {
        room_id: ROOM,
        text: text.to_owned(),
    }
}

fn restore() -> NameInteraction {
    NameInteraction::Restore { room_id: ROOM }
}

fn named_channel(id: u64, name: &str) -> Channel {
    let mut channel = channel(id, 2, Some(CATEGORY));
    channel.name = Some(name.to_owned());
    channel
}

fn occupant(member_id: u64, channel_id: u64) -> VoiceMember {
    VoiceMember {
        member_id,
        channel_id,
        bot: Some(false),
    }
}

/// One tracked room (500, owned by `OWNER`) holding `OWNER` and `GUEST`, a
/// second voice channel "Lounge", and a store holding the room's row.
async fn setup() -> (GuildRoomWorker<Store, Http>, Trace) {
    let trace = Trace::default();
    let live = LiveGuild::new(GUILD);
    live.publish(snapshot(
        &[ROOM],
        vec![occupant(OWNER, ROOM), occupant(GUEST, ROOM)],
    ));
    live.upsert_channel(named_channel(LOUNGE, "Lounge"));
    let store = Store::new(trace.clone());
    store.rooms.lock().unwrap().insert(ROOM, room(ROOM));
    let worker = GuildRoomWorker::load(live, store, Http::new(trace.clone()))
        .await
        .unwrap();
    (worker, trace)
}

fn applied(reply: NameReply) -> String {
    match reply {
        NameReply::Applied(text) => text,
        other => panic!("expected an applied reply, got {other:?}"),
    }
}

fn refused(reply: NameReply) -> String {
    match reply {
        NameReply::Refused(text) => text,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// Run queued writes until the worker has nothing due, `now` spaced past any
/// failure backoff.
async fn drain(worker: &mut GuildRoomWorker<Store, Http>) {
    for step in 0..8 {
        worker.dispatch_one(step * 1_000_000).await;
    }
}

fn channel_name(worker: &GuildRoomWorker<Store, Http>, id: u64) -> Option<String> {
    worker
        .live
        .inner
        .read()
        .unwrap()
        .channels
        .get(&id)
        .and_then(|channel| channel.name.clone())
}

fn panel_ids(response: &InteractionResponse) -> Vec<String> {
    let data = response.data.as_ref().expect("panel data");
    assert_eq!(data.flags, Some(MessageFlags::EPHEMERAL));
    let Some(Component::ActionRow(row)) = data.components.as_ref().and_then(|rows| rows.first())
    else {
        panic!("panel carries one action row");
    };
    row.components
        .iter()
        .map(|component| match component {
            Component::Button(button) => button.custom_id.clone().expect("button id"),
            other => panic!("panel holds buttons only, got {other:?}"),
        })
        .collect()
}

fn modal_input(response: &InteractionResponse) -> TextInput {
    let data = response.data.as_ref().expect("modal data");
    let Some(Component::Label(label)) = data.components.as_ref().and_then(|rows| rows.first())
    else {
        panic!("modal wraps its input in a label");
    };
    match &*label.component {
        Component::TextInput(input) => input.clone(),
        other => panic!("modal holds a text input, got {other:?}"),
    }
}

fn modal_interaction(custom_id: &str, components: Vec<ModalInteractionComponent>) -> Interaction {
    let mut interaction = with_user(voice_interaction(None, None, true), OWNER);
    interaction.kind = InteractionType::ModalSubmit;
    interaction.data = Some(InteractionData::ModalSubmit(Box::new(
        ModalInteractionData {
            components,
            custom_id: custom_id.to_owned(),
            resolved: None,
        },
    )));
    interaction
}

fn labelled_input(value: &str) -> Vec<ModalInteractionComponent> {
    vec![ModalInteractionComponent::Label(ModalInteractionLabel {
        id: 1,
        component: Box::new(ModalInteractionComponent::TextInput(
            ModalInteractionTextInput {
                custom_id: NAME_INPUT_ID.to_owned(),
                id: 2,
                value: value.to_owned(),
            },
        )),
    })]
}

// --- ids and responses --------------------------------------------------------

#[test]
fn panel_buttons_and_modal_ids_round_trip() {
    let panel = panel_response(ROOM, "state");
    assert_eq!(
        panel.kind,
        InteractionResponseType::ChannelMessageWithSource
    );
    let ids = panel_ids(&panel);
    assert_eq!(
        ids,
        [name_custom_custom_id(ROOM), name_restore_custom_id(ROOM)]
    );
    assert_eq!(
        parse_voice_custom_id(&ids[0]),
        Some(VoiceAction::NameCustom { room_id: ROOM })
    );
    assert_eq!(
        parse_voice_custom_id(&ids[1]),
        Some(VoiceAction::NameRestore { room_id: ROOM })
    );

    let modal = modal_response(ROOM, Some("@@owner@@'s base"));
    assert_eq!(modal.kind, InteractionResponseType::Modal);
    let custom_id = modal
        .data
        .as_ref()
        .and_then(|data| data.custom_id.clone())
        .expect("modal id");
    assert_eq!(
        parse_voice_custom_id(&custom_id),
        Some(VoiceAction::NameModal { room_id: ROOM })
    );
    let input = modal_input(&modal);
    assert_eq!(input.custom_id, NAME_INPUT_ID);
    assert_eq!(input.max_length, Some(MAX_CUSTOM_NAME_CHARS as u16));
    assert_eq!(input.required, Some(true));
    assert_eq!(input.value.as_deref(), Some("@@owner@@'s base"));
    assert_eq!(modal_input(&modal_response(ROOM, None)).value, None);
}

#[test]
fn clicks_and_submits_parse_and_everything_else_stays_silent() {
    let click = |id: &str| component_interaction(id, None, OWNER);
    assert_eq!(
        name_component_action(&click(&name_custom_custom_id(ROOM))),
        Some(NameInteraction::OpenModal { room_id: ROOM })
    );
    assert_eq!(
        name_component_action(&click(&name_restore_custom_id(ROOM))),
        Some(NameInteraction::Restore { room_id: ROOM })
    );
    // The modal id on a button, a button id on a modal, other voice ids, other
    // namespaces, zero ids and overlong ids are not ours.
    assert_eq!(
        name_component_action(&click(&name_modal_custom_id(ROOM))),
        None
    );
    assert_eq!(
        name_component_action(&click(&import_confirm_custom_id(OWNER, "0123456789abcdef"))),
        None
    );
    assert_eq!(
        name_component_action(&click(&join_custom_id(JoinDecision::Approve, ROOM, 1))),
        None
    );
    assert_eq!(
        name_component_action(&click("two:lfg:name-custom:500")),
        None
    );
    assert_eq!(
        name_component_action(&click("two:voice:name-custom:0")),
        None
    );
    assert_eq!(
        name_component_action(&click(&format!(
            "{VOICE_CUSTOM_ID_PREFIX}name-custom:{}",
            "9".repeat(100)
        ))),
        None
    );
    assert_eq!(
        name_component_action(&modal_interaction(&name_custom_custom_id(ROOM), Vec::new())),
        None
    );
    assert_eq!(
        name_component_action(&guildless_component_interaction(&name_custom_custom_id(
            ROOM
        ))),
        None
    );

    // The input is found inside a label (current clients) or an action row.
    assert_eq!(
        name_component_action(&modal_interaction(
            &name_modal_custom_id(ROOM),
            labelled_input("Den")
        )),
        Some(submit("Den"))
    );
    let row = vec![ModalInteractionComponent::ActionRow(
        ModalInteractionActionRow {
            id: 1,
            components: labelled_input("Den row"),
        },
    )];
    assert_eq!(
        name_component_action(&modal_interaction(&name_modal_custom_id(ROOM), row)),
        Some(submit("Den row"))
    );
    // A modal without the input still parses and later reads as empty.
    assert_eq!(
        name_component_action(&modal_interaction(&name_modal_custom_id(ROOM), Vec::new())),
        Some(submit(""))
    );
}

#[test]
fn hostile_name_ids_refuse_through_the_router() {
    use two_bot_core::voice_custom_id::MAX_VOICE_CUSTOM_ID_CHARS;
    let click = |id: &str| component_interaction(id, None, OWNER);
    let modal = |id: &str| modal_interaction(id, labelled_input("Den"));
    // Valid controls stay green through both the parser and the router.
    for room_id in [1, ROOM, u64::MAX] {
        assert_eq!(
            name_component_action(&click(&name_custom_custom_id(room_id))),
            Some(NameInteraction::OpenModal { room_id })
        );
        assert_eq!(
            name_component_action(&click(&name_restore_custom_id(room_id))),
            Some(NameInteraction::Restore { room_id })
        );
        assert_eq!(
            name_component_action(&modal(&name_modal_custom_id(room_id))),
            Some(NameInteraction::Submit {
                room_id,
                text: "Den".to_owned(),
            })
        );
        // A modal id on a button and button ids on a modal stay silent,
        // even at the largest encodable room.
        assert_eq!(
            name_component_action(&click(&name_modal_custom_id(room_id))),
            None
        );
        assert_eq!(
            name_component_action(&modal_interaction(
                &name_custom_custom_id(room_id),
                labelled_input("Den")
            )),
            None
        );
    }
    // Forged, truncated, separator-confused, non-numeric, control and
    // multibyte `two:voice:name-*` ids refuse through the real router on
    // both the button and the modal paths without panicking.
    let hostile = [
        // Forged verbs, prefixes and lookalikes.
        "two:voice:Name-Custom:500",
        "two:voice:NAME-CUSTOM:500",
        "TWO:VOICE:name-custom:500",
        "two:voice:name_custom:500",
        "two:voice:name custom:500",
        "two:voice:name-custom :500",
        "two:voice:name-custom:500 ",
        " two:voice:name-custom:500",
        "two:voice:name-custom:500\n",
        "two:lfg:name-custom:500",
        "two:self-role:name-custom:500",
        "two:voice:name-custom:500\u{200b}",
        "two:voice:name-restore:500\u{feff}",
        // Truncated shapes.
        "two:voice:name-",
        "two:voice:name-custom",
        "two:voice:name-restore",
        "two:voice:name-modal",
        "two:voice:name-custom:",
        "two:voice:name-restore:",
        "two:voice:name-modal:",
        // Separator confusion.
        "two:voice:name-custom:500:600",
        "two:voice:name-restore:500:600",
        "two:voice:name-modal:500:600",
        "two:voice:name-custom:500:",
        "two:voice:name-custom::500",
        "two:voice:name-custom:500::",
        "two::voice:name-custom:500",
        "two:voice::name-custom:500",
        "two;voice;name-custom;500",
        "two:voice:name-custom;500",
        "two:voice:name-custom/500",
        // Non-numeric, zero and overflow rooms.
        "two:voice:name-custom:0",
        "two:voice:name-restore:00",
        "two:voice:name-modal:000",
        "two:voice:name-custom:+500",
        "two:voice:name-custom:-500",
        "two:voice:name-custom: 500",
        "two:voice:name-custom:0x1F4",
        "two:voice:name-custom:1.5",
        "two:voice:name-custom:NaN",
        "two:voice:name-custom:abc",
        // Control bytes, NUL and non-ASCII rooms.
        "two:voice:name-custom:\u{0}",
        "two:voice:name-custom:500\u{0}",
        "two:voice:name-custom:500\u{7f}",
        "two:voice:name-restore:500\nmore",
        "two:voice:name-custom:\u{e9}",
        "two:voice:name-custom:\u{1f600}",
        "two:voice:name-custom:\u{ff15}\u{ff10}\u{ff10}",
        "two:voice:name-modal:\u{200b}",
    ];
    for id in hostile {
        assert_eq!(parse_voice_custom_id(id), None, "{id:?}");
        assert_eq!(name_component_action(&click(id)), None, "{id:?}");
        assert_eq!(name_component_action(&modal(id)), None, "{id:?}");
        assert_eq!(
            name_component_action(&modal_interaction(id, Vec::new())),
            None,
            "{id:?}"
        );
    }
    // 2^64 overflows `u64`: built arithmetically so the source holds no
    // 17-20 digit snowflake literal (snowflake gate).
    let overflow = format!("two:voice:name-custom:{}", (u64::MAX as u128) + 1);
    assert_eq!(parse_voice_custom_id(&overflow), None, "{overflow:?}");
    assert_eq!(
        name_component_action(&click(&overflow)),
        None,
        "{overflow:?}"
    );
    assert_eq!(
        name_component_action(&modal(&overflow)),
        None,
        "{overflow:?}"
    );
    assert_eq!(
        name_component_action(&modal_interaction(&overflow, Vec::new())),
        None,
        "{overflow:?}"
    );
    // Exact-boundary shape rejections refuse on both paths. The 100- and
    // 101-byte all-`1` custom ids overflow `u64`, so they refuse on shape at
    // any length. Lengths are pinned so the boundary cannot drift; the
    // zero-padded pair below pins the length gate itself.
    let custom_100 = format!("two:voice:name-custom:{}", "1".repeat(78));
    let custom_101 = format!("two:voice:name-custom:{}", "1".repeat(79));
    assert_eq!(custom_100.len(), MAX_VOICE_CUSTOM_ID_CHARS);
    assert_eq!(custom_101.len(), MAX_VOICE_CUSTOM_ID_CHARS + 1);
    for id in [&custom_100, &custom_101] {
        assert_eq!(parse_voice_custom_id(id), None, "{id:?}");
        assert_eq!(name_component_action(&click(id)), None, "{id:?}");
        assert_eq!(name_component_action(&modal(id)), None, "{id:?}");
    }
    // Zero-padded gate pin through the real router: `parse_id` accepts
    // leading zeros, so the exactly-100-byte room `000…0500` (value 500)
    // opens the modal while the 101-byte form refuses on both paths.
    // Deleting the length gate (or an off-by-one `>=`) flips these asserts.
    let zero_100 = format!("two:voice:name-custom:{}{}", "0".repeat(75), 500);
    let zero_101 = format!("two:voice:name-custom:{}{}", "0".repeat(76), 500);
    assert_eq!(zero_100.len(), MAX_VOICE_CUSTOM_ID_CHARS);
    assert_eq!(zero_101.len(), MAX_VOICE_CUSTOM_ID_CHARS + 1);
    assert_eq!(
        name_component_action(&click(&zero_100)),
        Some(NameInteraction::OpenModal { room_id: 500 })
    );
    assert_eq!(parse_voice_custom_id(&zero_101), None, "{zero_101:?}");
    assert_eq!(
        name_component_action(&click(&zero_101)),
        None,
        "{zero_101:?}"
    );
    assert_eq!(
        name_component_action(&modal(&zero_101)),
        None,
        "{zero_101:?}"
    );
    // Multibyte boundary: 39 `é` are 78 bytes, so with the 22-byte custom
    // prefix the id is exactly 100 bytes; 40 `é` are 102 bytes. Both refuse.
    // One emoji is one char but two UTF-16 units and four bytes.
    let multi_100 = format!("two:voice:name-custom:{}", "é".repeat(39));
    let multi_over = format!("two:voice:name-custom:{}", "é".repeat(40));
    assert_eq!(multi_100.len(), MAX_VOICE_CUSTOM_ID_CHARS);
    assert!(multi_over.len() > MAX_VOICE_CUSTOM_ID_CHARS);
    for id in [&multi_100, &multi_over] {
        assert_eq!(parse_voice_custom_id(id), None, "{id:?}");
        assert_eq!(name_component_action(&click(id)), None, "{id:?}");
        assert_eq!(name_component_action(&modal(id)), None, "{id:?}");
    }
    let emoji_over = format!("two:voice:name-custom:{}", "\u{1f600}".repeat(25));
    assert!(emoji_over.len() > MAX_VOICE_CUSTOM_ID_CHARS);
    assert!(emoji_over.chars().count() < MAX_VOICE_CUSTOM_ID_CHARS);
    assert_eq!(parse_voice_custom_id(&emoji_over), None);
    assert_eq!(name_component_action(&click(&emoji_over)), None);
}

#[tokio::test]
async fn hostile_clicks_refuse_at_the_router_and_valid_flow_still_applies() {
    // Hostile button ids never become a `NameInteraction` through the real
    // router, and the next valid step still applies, so a hostile battery
    // cannot confuse the state machine. This test never feeds the worker,
    // so it asserts no worker state here; hostile-input worker-write and
    // trace coverage lives in
    // `hostile_runtime_clicks_are_unowned_and_echo_nothing`, which routes
    // through the real runtime.
    let (mut worker, trace) = setup().await;
    let overlong = format!("two:voice:name-custom:{}", "1".repeat(79));
    // 2^64 overflows `u64`: built arithmetically so the source holds no
    // 17-20 digit snowflake literal (snowflake gate).
    let overflow = format!("two:voice:name-custom:{}", (u64::MAX as u128) + 1);
    let hostile: Vec<&str> = vec![
        "two:voice:Name-Custom:500",
        "two:voice:name-custom:500:600",
        "two:voice:name-custom:500:",
        "two:voice:name-custom::500",
        "two:voice:name-custom:0",
        "two:voice:name-custom:+500",
        "two:voice:name-custom:abc",
        &overflow,
        "two:voice:name-custom:\u{0}",
        "two:voice:name-custom:500\u{0}",
        "two:voice:name-custom:\u{1f600}",
        "two:voice:name-restore:500\nmore",
        &overlong,
    ];
    for id in hostile.iter().copied() {
        let click = component_interaction(id, None, OWNER);
        assert_eq!(name_component_action(&click), None, "{id:?}");
        let submitted = modal_interaction(id, labelled_input("Den"));
        assert_eq!(name_component_action(&submitted), None, "{id:?}");
    }
    // The valid panel flow still runs after the hostile battery.
    applied(worker.apply_name(command(OWNER, false, submit("Den")), 0));
    assert_eq!(
        worker.custom_names.get(&ROOM).map(String::as_str),
        Some("Den")
    );
    drain(&mut worker).await;
    let entries = trace.lock().unwrap().clone();
    assert!(
        entries
            .iter()
            .any(|entry| entry.contains("save_custom_name:500")),
        "{entries:?}"
    );
    assert!(
        !entries
            .iter()
            .any(|entry| hostile.iter().any(|id| entry.contains(*id))),
        "hostile input is never echoed: {entries:?}"
    );
}

#[tokio::test]
async fn hostile_runtime_clicks_are_unowned_and_echo_nothing() {
    // Through the real runtime, hostile name clicks are not owned and
    // produce no reply, so hostile bytes never reach a response. A valid
    // panel click immediately after still owns and answers.
    let trace = Trace::default();
    let runtime = ownership_room_runtime(trace.clone()).await;
    let names = directory();
    // `ownership_room_runtime` publishes the room through the real actor, so
    // the trace already holds its setup writes (`create`, `persist:500`,
    // `move:300:500`). Settle, then clear, so the empty-trace assertion below
    // isolates the hostile phase instead of the setup.
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    trace.lock().unwrap().clear();
    for id in [
        "two:voice:Name-Custom:500",
        "two:voice:name-custom:500:600",
        "two:voice:name-custom:0",
        "two:voice:name-custom:500\u{0}",
        "two:voice:name-custom:\u{1f600}",
    ] {
        let click = with_user(component_interaction(id, None, OWNER), OWNER);
        let (owned, response) = name_capture_full(&runtime, &click, &names).await;
        assert!(!owned, "{id:?}");
        assert!(response.is_none(), "{id:?}");
        let submitted = modal_interaction(id, labelled_input("Den"));
        let submitted = with_user(submitted, OWNER);
        let (owned, response) = name_capture_full(&runtime, &submitted, &names).await;
        assert!(!owned, "{id:?}");
        assert!(response.is_none(), "{id:?}");
    }
    // Hostile clicks reached the real runtime above, so an empty trace here
    // proves they queued no worker write and emitted no Discord call. The
    // actor runs concurrently, so settle before asserting.
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        trace.lock().unwrap().is_empty(),
        "hostile runtime clicks write nothing: {:?}",
        trace.lock().unwrap()
    );
    let click = component_interaction(&name_custom_custom_id(500), None, OWNER);
    let (owned, response) = name_capture_full(&runtime, &click, &names).await;
    assert!(owned);
    let modal = response.expect("valid custom button still opens the modal");
    assert_eq!(modal.kind, InteractionResponseType::Modal);
}

#[test]
fn name_is_a_gated_voice_command_named_name() {
    let interaction = voice_interaction(Some(command_data("name", Vec::new())), None, true);
    assert_eq!(parse_voice_command(&interaction), Some(VoiceCommand::Name));
    assert_eq!(VoiceCommand::Name.name(), "name");
}

// --- worker: authorization and stale ids --------------------------------------

#[tokio::test]
async fn panel_opens_for_the_owner_in_their_room() {
    let (mut worker, _) = setup().await;
    let NameReply::Panel { room_id, text } =
        worker.apply_name(command(OWNER, false, NameInteraction::Panel), 0)
    else {
        panic!("owner gets the panel");
    };
    assert_eq!(room_id, ROOM);
    assert!(text.contains("**room**"), "current name shown: {text}");
    assert!(text.contains("the template name"), "{text}");
    // The modal opens pre-filled with nothing, then with the override.
    assert_eq!(
        worker.apply_name(
            command(OWNER, false, NameInteraction::OpenModal { room_id: ROOM }),
            0
        ),
        NameReply::Modal {
            room_id: ROOM,
            prefill: None
        }
    );
    applied(worker.apply_name(command(OWNER, false, submit("Den")), 0));
    assert_eq!(
        worker.apply_name(
            command(OWNER, false, NameInteraction::OpenModal { room_id: ROOM }),
            0
        ),
        NameReply::Modal {
            room_id: ROOM,
            prefill: Some("Den".to_owned())
        }
    );
    let NameReply::Panel { text, .. } =
        worker.apply_name(command(OWNER, false, NameInteraction::Panel), 0)
    else {
        panic!("panel");
    };
    assert!(text.contains("custom name `Den`"), "{text}");
}

#[tokio::test]
async fn panel_needs_a_tracked_room_the_invoker_is_in() {
    let (mut worker, _) = setup().await;
    // Not in voice at all.
    let text = refused(worker.apply_name(command(999, true, NameInteraction::Panel), 0));
    assert!(
        text.contains("need to be in a temporary voice room"),
        "{text}"
    );
    // In an untracked channel (the creator channel).
    worker.live.voice_update(GUEST, Some(CREATOR), Some(false));
    let text = refused(worker.apply_name(command(GUEST, true, NameInteraction::Panel), 0));
    assert!(text.contains("isn't a temporary room"), "{text}");
}

#[tokio::test]
async fn non_owners_are_refused_and_admins_are_not() {
    let (mut worker, trace) = setup().await;
    for request in [
        NameInteraction::Panel,
        NameInteraction::OpenModal { room_id: ROOM },
        submit("Den"),
        restore(),
    ] {
        let text = refused(worker.apply_name(command(GUEST, false, request), 0));
        assert!(text.contains("owner"), "{text}");
    }
    assert!(worker.custom_names.is_empty());
    drain(&mut worker).await;
    assert!(trace.lock().unwrap().is_empty(), "a refusal writes nothing");
    // Admins may use it in any room.
    applied(worker.apply_name(command(GUEST, true, submit("Admin pick")), 0));
    assert_eq!(
        worker.custom_names.get(&ROOM).map(String::as_str),
        Some("Admin pick")
    );
}

#[tokio::test]
async fn a_stale_panel_from_the_previous_owner_is_refused() {
    let (mut worker, _) = setup().await;
    // `/transfer` moved the room to the guest after the panel was posted.
    worker.rooms.get_mut(&ROOM).unwrap().owner_id = GUEST;
    let text = refused(worker.apply_name(command(OWNER, false, submit("Den")), 0));
    assert!(text.contains("owner"), "{text}");
    assert!(worker.custom_names.is_empty());
    applied(worker.apply_name(command(GUEST, false, submit("Den")), 0));
}

#[tokio::test]
async fn stale_and_foreign_room_ids_are_answered() {
    let (mut worker, trace) = setup().await;
    for request in [
        NameInteraction::OpenModal { room_id: 999 },
        NameInteraction::Restore { room_id: 999 },
        NameInteraction::Submit {
            room_id: 999,
            text: "Den".to_owned(),
        },
        // A real voice channel the bot does not track.
        NameInteraction::Submit {
            room_id: LOUNGE,
            text: "Den".to_owned(),
        },
    ] {
        let text = refused(worker.apply_name(command(OWNER, true, request), 0));
        assert!(text.contains("no longer exists"), "{text}");
    }
    assert!(worker.custom_names.is_empty());
    drain(&mut worker).await;
    assert!(trace.lock().unwrap().is_empty());
}

#[tokio::test]
async fn paused_and_cold_workers_refuse() {
    let (mut worker, _) = setup().await;
    worker.live.disconnect();
    let text = refused(worker.apply_name(command(OWNER, false, submit("Den")), 0));
    assert!(text.contains("warmed up"), "{text}");
    let (mut worker, _) = setup().await;
    worker.halted = true;
    let text = refused(worker.apply_name(command(OWNER, false, submit("Den")), 0));
    assert!(text.contains("paused"), "{text}");
}

// --- worker: applying a custom name -------------------------------------------

#[tokio::test]
async fn custom_name_applies_through_the_coalescer_and_expands_tokens() {
    let (mut worker, trace) = setup().await;
    let text = applied(worker.apply_name(command(OWNER, false, submit("  @@owner@@'s base  ")), 0));
    assert!(text.contains("**Ava's base**"), "{text}");
    assert_eq!(
        worker.custom_names.get(&ROOM).map(String::as_str),
        Some("@@owner@@'s base"),
        "the override keeps the typed template, trimmed"
    );
    drain(&mut worker).await;
    assert_eq!(
        *trace.lock().unwrap(),
        [
            "save_custom_name:500:Some(\"@@owner@@'s base\")".to_owned(),
            "rename:500:Ava's base".to_owned()
        ]
    );
    assert_eq!(channel_name(&worker, ROOM).as_deref(), Some("Ava's base"));
    assert_eq!(
        worker
            .store
            .custom_names
            .lock()
            .unwrap()
            .get(&ROOM)
            .map(String::as_str),
        Some("@@owner@@'s base")
    );
}

#[tokio::test]
async fn tokens_read_the_rooms_live_facts() {
    let (mut worker, _) = setup().await;
    let mut settings = NameSettings::default();
    settings
        .lists
        .insert("pets".to_owned(), vec!["cat".to_owned()]);
    settings.no_game_label = "Chilling".to_owned();
    let mut cmd = command(
        OWNER,
        false,
        submit("@@game_name@@ ## @@num@@/@@limit@@ [[list:pets]]"),
    );
    cmd.settings = settings;
    // Room number 1 (first of its creator's rooms), two humans, limit 8
    // (the fixture channel's), the named list and the guild's no-game label.
    let text = applied(worker.apply_name(cmd, 0));
    assert!(text.contains("**Chilling #1 2/8 cat**"), "{text}");
}

#[tokio::test]
async fn unsafe_and_filtered_names_are_refused_without_a_write() {
    let (mut worker, trace) = setup().await;
    let refusal = |reason: NameRefusal| reason.to_string();
    let long = "a".repeat(101);
    let cases: [(&str, String); 6] = [
        ("   ", refusal(NameRefusal::Empty)),
        ("@@@@", refusal(NameRefusal::Empty)),
        (&long, refusal(NameRefusal::TooLong)),
        ("Blorp den", "not allowed".to_owned()),
        ("discord.gg/abc", "not allowed".to_owned()),
        // A blocked word in a branch that is not active yet.
        ("{{LIVE ??blorp//fine}}", "not allowed".to_owned()),
    ];
    for (text, expected) in cases {
        let mut cmd = command(OWNER, false, submit(text));
        cmd.policy = policy(&["blorp"]);
        let reply = refused(worker.apply_name(cmd, 0));
        assert!(reply.contains(&expected), "{text:?}: {reply}");
    }
    // A name built from a member's display name is filtered after expansion.
    let mut cmd = command(OWNER, false, submit("@@owner@@'s den"));
    cmd.policy = policy(&["ava"]);
    let reply = refused(worker.apply_name(cmd, 0));
    assert!(reply.contains("not allowed"), "{reply}");
    assert!(worker.custom_names.is_empty());
    drain(&mut worker).await;
    assert!(trace.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unique_names_conflict_only_when_the_setting_is_on() {
    let (mut worker, _) = setup().await;
    // Another voice channel is called "Lounge": a literal clash in any case,
    // spacing or full-width form is refused, but only with the setting on.
    for text in ["lounge", "  LOUNGE ", "Ｌｏｕｎｇｅ"] {
        let mut on = command(OWNER, false, submit(text));
        on.settings.unique_names = true;
        let reply = refused(worker.apply_name(on, 0));
        assert_eq!(reply, NameRefusal::Taken.to_string(), "{text:?}");
        assert!(worker.custom_names.is_empty());
    }
    let off = command(OWNER, false, submit("lounge"));
    applied(worker.apply_name(off, 0));
    // The room's own name never conflicts with itself.
    worker.live.upsert_channel(named_channel(ROOM, "Mine"));
    let mut own = command(OWNER, false, submit("mine"));
    own.settings.unique_names = true;
    applied(worker.apply_name(own, 0));
    // A name built from tokens is not a literal, so it is not compared.
    worker.live.upsert_channel(named_channel(LOUNGE, "Ava"));
    let mut tokens = command(OWNER, false, submit("@@owner@@"));
    tokens.settings.unique_names = true;
    applied(worker.apply_name(tokens, 0));
}

#[tokio::test]
async fn queued_rename_blocks_folded_duplicate_before_it_lands() {
    // Two tracked rooms with different owners: a folded duplicate of a
    // queued-but-unlanded rename is refused at submit (legacy 38041a1
    // siblingNames applies throttle.pending), and the first rename still lands.
    const SECOND_ROOM: u64 = 501;
    let trace = Trace::default();
    let live = LiveGuild::new(GUILD);
    live.publish(snapshot(
        &[ROOM, SECOND_ROOM],
        vec![occupant(OWNER, ROOM), occupant(GUEST, SECOND_ROOM)],
    ));
    live.upsert_channel(named_channel(ROOM, "Alpha"));
    live.upsert_channel(named_channel(SECOND_ROOM, "Beta"));
    let store = Store::new(trace.clone());
    store.rooms.lock().unwrap().insert(ROOM, room(ROOM));
    let mut second = room(SECOND_ROOM);
    second.owner_id = GUEST;
    store.rooms.lock().unwrap().insert(SECOND_ROOM, second);
    let mut worker = GuildRoomWorker::load(live, store, Http::new(trace.clone()))
        .await
        .unwrap();
    worker.name_settings.unique_names = true;

    let mut first = command(OWNER, false, submit("Squad"));
    first.settings.unique_names = true;
    applied(worker.apply_name(first, 0));

    let mut clash = command(
        GUEST,
        false,
        NameInteraction::Submit {
            room_id: SECOND_ROOM,
            text: "squad".to_owned(),
        },
    );
    clash.settings.unique_names = true;
    let reply = refused(worker.apply_name(clash, 0));
    assert_eq!(reply, NameRefusal::Taken.to_string(), "{reply}");

    drain(&mut worker).await;
    assert_eq!(channel_name(&worker, ROOM).as_deref(), Some("Squad"));
    assert_eq!(channel_name(&worker, SECOND_ROOM).as_deref(), Some("Beta"));
    let renames: Vec<String> = trace
        .lock()
        .unwrap()
        .iter()
        .filter_map(|entry| entry.strip_prefix("rename:").map(ToString::to_string))
        .collect();
    assert_eq!(renames, vec!["500:Squad".to_owned()]);
}

#[tokio::test]
async fn flush_recheck_drops_now_duplicate_rename_and_clears_override() {
    // A sibling channel landing the folded name after submit but before the
    // flush drops the queued rename (legacy 38041a1 drops a throttled rename
    // that would now duplicate): no rename issues, and the never-landed
    // override is cleared (memory plus store) so the room falls back to its
    // template instead of stranding a custom name the channel does not carry.
    const SECOND_ROOM: u64 = 501;
    let trace = Trace::default();
    let live = LiveGuild::new(GUILD);
    live.publish(snapshot(
        &[ROOM, SECOND_ROOM],
        vec![occupant(OWNER, ROOM), occupant(GUEST, SECOND_ROOM)],
    ));
    live.upsert_channel(named_channel(ROOM, "Alpha"));
    live.upsert_channel(named_channel(SECOND_ROOM, "Beta"));
    let store = Store::new(trace.clone());
    store.rooms.lock().unwrap().insert(ROOM, room(ROOM));
    let mut second = room(SECOND_ROOM);
    second.owner_id = GUEST;
    store.rooms.lock().unwrap().insert(SECOND_ROOM, second);
    let mut worker = GuildRoomWorker::load(live, store, Http::new(trace.clone()))
        .await
        .unwrap();
    worker.name_settings.unique_names = true;

    let mut first = command(OWNER, false, submit("Squad"));
    first.settings.unique_names = true;
    applied(worker.apply_name(first, 0));

    // The sibling lands the folded name before the queued rename flushes.
    worker
        .live
        .upsert_channel(named_channel(SECOND_ROOM, "squad"));
    drain(&mut worker).await;

    assert_eq!(channel_name(&worker, ROOM).as_deref(), Some("Alpha"));
    assert_eq!(channel_name(&worker, SECOND_ROOM).as_deref(), Some("squad"));
    let renames: Vec<String> = trace
        .lock()
        .unwrap()
        .iter()
        .filter_map(|entry| entry.strip_prefix("rename:").map(ToString::to_string))
        .collect();
    assert!(
        renames.iter().all(|entry| !entry.starts_with("500:")),
        "{renames:?}"
    );
    assert_eq!(worker.custom_names.get(&ROOM), None);
    assert_eq!(worker.desired_names.get(&ROOM), None);
    assert!(!worker
        .store
        .custom_names
        .lock()
        .unwrap()
        .contains_key(&ROOM));
}

#[tokio::test]
async fn token_built_name_is_not_dropped_by_the_flush_recheck() {
    // A token-built rename stays exempt at flush, matching submit time: even
    // when a sibling already shows the folded rendered name, the rename
    // lands and the override keeps the typed template.
    const SECOND_ROOM: u64 = 501;
    let trace = Trace::default();
    let live = LiveGuild::new(GUILD);
    live.publish(snapshot(
        &[ROOM, SECOND_ROOM],
        vec![occupant(OWNER, ROOM), occupant(GUEST, SECOND_ROOM)],
    ));
    live.upsert_channel(named_channel(ROOM, "Alpha"));
    live.upsert_channel(named_channel(SECOND_ROOM, "Ava"));
    let store = Store::new(trace.clone());
    store.rooms.lock().unwrap().insert(ROOM, room(ROOM));
    let mut second = room(SECOND_ROOM);
    second.owner_id = GUEST;
    store.rooms.lock().unwrap().insert(SECOND_ROOM, second);
    let mut worker = GuildRoomWorker::load(live, store, Http::new(trace.clone()))
        .await
        .unwrap();
    worker.name_settings.unique_names = true;

    let mut tokens = command(OWNER, false, submit("@@owner@@"));
    tokens.settings.unique_names = true;
    applied(worker.apply_name(tokens, 0));
    drain(&mut worker).await;

    assert_eq!(channel_name(&worker, ROOM).as_deref(), Some("Ava"));
    assert_eq!(
        worker.custom_names.get(&ROOM).map(String::as_str),
        Some("@@owner@@"),
        "the exempt override keeps the typed template"
    );
    let renames: Vec<String> = trace
        .lock()
        .unwrap()
        .iter()
        .filter_map(|entry| entry.strip_prefix("rename:").map(ToString::to_string))
        .collect();
    assert_eq!(renames, vec!["500:Ava".to_owned()]);
}

#[tokio::test]
async fn flush_recheck_is_skipped_when_unique_names_turns_off() {
    // A rename accepted while unique-names is on still lands when the setting
    // is turned off before the flush: the recheck is skipped, so no duplicate
    // drop clears the override.
    const SECOND_ROOM: u64 = 501;
    let trace = Trace::default();
    let live = LiveGuild::new(GUILD);
    live.publish(snapshot(
        &[ROOM, SECOND_ROOM],
        vec![occupant(OWNER, ROOM), occupant(GUEST, SECOND_ROOM)],
    ));
    live.upsert_channel(named_channel(ROOM, "Alpha"));
    live.upsert_channel(named_channel(SECOND_ROOM, "Beta"));
    let store = Store::new(trace.clone());
    store.rooms.lock().unwrap().insert(ROOM, room(ROOM));
    let mut second = room(SECOND_ROOM);
    second.owner_id = GUEST;
    store.rooms.lock().unwrap().insert(SECOND_ROOM, second);
    let mut worker = GuildRoomWorker::load(live, store, Http::new(trace.clone()))
        .await
        .unwrap();
    worker.name_settings.unique_names = true;

    let mut first = command(OWNER, false, submit("Squad"));
    first.settings.unique_names = true;
    applied(worker.apply_name(first, 0));

    // The sibling lands the folded name, then the setting is turned off.
    worker
        .live
        .upsert_channel(named_channel(SECOND_ROOM, "squad"));
    worker.name_settings.unique_names = false;
    drain(&mut worker).await;

    assert_eq!(channel_name(&worker, ROOM).as_deref(), Some("Squad"));
    assert_eq!(
        worker.custom_names.get(&ROOM).map(String::as_str),
        Some("Squad")
    );
}

#[tokio::test]
async fn stale_last_proposed_entry_never_holds_a_name() {
    // A landed rename that later diverges externally (gateway shows
    // Moderated while desired still says Squad) must not block a folded
    // duplicate from another room: only really-pending renames hold names.
    const SECOND_ROOM: u64 = 501;
    let trace = Trace::default();
    let live = LiveGuild::new(GUILD);
    live.publish(snapshot(
        &[ROOM, SECOND_ROOM],
        vec![occupant(OWNER, ROOM), occupant(GUEST, SECOND_ROOM)],
    ));
    live.upsert_channel(named_channel(ROOM, "Alpha"));
    live.upsert_channel(named_channel(SECOND_ROOM, "Beta"));
    let store = Store::new(trace.clone());
    store.rooms.lock().unwrap().insert(ROOM, room(ROOM));
    let mut second = room(SECOND_ROOM);
    second.owner_id = GUEST;
    store.rooms.lock().unwrap().insert(SECOND_ROOM, second);
    let mut worker = GuildRoomWorker::load(live, store, Http::new(trace.clone()))
        .await
        .unwrap();
    worker.name_settings.unique_names = true;

    let mut first = command(OWNER, false, submit("Squad"));
    first.settings.unique_names = true;
    applied(worker.apply_name(first, 0));
    drain(&mut worker).await;
    assert_eq!(channel_name(&worker, ROOM).as_deref(), Some("Squad"));

    // An external rename diverges the landed channel from the last proposal.
    worker.live.upsert_channel(named_channel(ROOM, "Moderated"));

    let mut clash = command(
        GUEST,
        false,
        NameInteraction::Submit {
            room_id: SECOND_ROOM,
            text: "squad".to_owned(),
        },
    );
    clash.settings.unique_names = true;
    applied(worker.apply_name(clash, 0));
}

#[tokio::test]
async fn queued_rename_holds_folded_name_before_dispatch() {
    // A rename drained from the coalescer but still queued behind the rate
    // limit blocks a folded duplicate at submit.
    const SECOND_ROOM: u64 = 501;
    let trace = Trace::default();
    let live = LiveGuild::new(GUILD);
    live.publish(snapshot(
        &[ROOM, SECOND_ROOM],
        vec![occupant(OWNER, ROOM), occupant(GUEST, SECOND_ROOM)],
    ));
    live.upsert_channel(named_channel(ROOM, "Alpha"));
    live.upsert_channel(named_channel(SECOND_ROOM, "Beta"));
    let store = Store::new(trace.clone());
    store.rooms.lock().unwrap().insert(ROOM, room(ROOM));
    let mut second = room(SECOND_ROOM);
    second.owner_id = GUEST;
    store.rooms.lock().unwrap().insert(SECOND_ROOM, second);
    let mut worker = GuildRoomWorker::load(live, store, Http::new(trace.clone()))
        .await
        .unwrap();
    worker.name_settings.unique_names = true;

    worker.queue.enqueue(
        GUILD,
        two_bot_core::voice_rooms::RoomAction::RenameRoom {
            channel_id: ROOM,
            name: "Squad".to_owned(),
        },
    );

    let mut clash = command(
        GUEST,
        false,
        NameInteraction::Submit {
            room_id: SECOND_ROOM,
            text: "squad".to_owned(),
        },
    );
    clash.settings.unique_names = true;
    let reply = refused(worker.apply_name(clash, 0));
    assert_eq!(reply, NameRefusal::Taken.to_string(), "{reply}");
}

#[tokio::test]
async fn in_flight_rename_holds_folded_name() {
    // A rename still running on its own task blocks a folded duplicate.
    const SECOND_ROOM: u64 = 501;
    let trace = Trace::default();
    let live = LiveGuild::new(GUILD);
    live.publish(snapshot(
        &[ROOM, SECOND_ROOM],
        vec![occupant(OWNER, ROOM), occupant(GUEST, SECOND_ROOM)],
    ));
    live.upsert_channel(named_channel(ROOM, "Alpha"));
    live.upsert_channel(named_channel(SECOND_ROOM, "Beta"));
    let store = Store::new(trace.clone());
    store.rooms.lock().unwrap().insert(ROOM, room(ROOM));
    let mut second = room(SECOND_ROOM);
    second.owner_id = GUEST;
    store.rooms.lock().unwrap().insert(SECOND_ROOM, second);
    let mut worker = GuildRoomWorker::load(live, store, Http::new(trace.clone()))
        .await
        .unwrap();
    worker.name_settings.unique_names = true;

    worker.renames_in_flight.insert(ROOM, "Squad".to_owned());

    let mut clash = command(
        GUEST,
        false,
        NameInteraction::Submit {
            room_id: SECOND_ROOM,
            text: "squad".to_owned(),
        },
    );
    clash.settings.unique_names = true;
    let reply = refused(worker.apply_name(clash, 0));
    assert_eq!(reply, NameRefusal::Taken.to_string(), "{reply}");
}

#[tokio::test]
async fn scheduled_retry_holds_folded_name() {
    // A rename waiting for its retry slot blocks a folded duplicate.
    const SECOND_ROOM: u64 = 501;
    let trace = Trace::default();
    let live = LiveGuild::new(GUILD);
    live.publish(snapshot(
        &[ROOM, SECOND_ROOM],
        vec![occupant(OWNER, ROOM), occupant(GUEST, SECOND_ROOM)],
    ));
    live.upsert_channel(named_channel(ROOM, "Alpha"));
    live.upsert_channel(named_channel(SECOND_ROOM, "Beta"));
    let store = Store::new(trace.clone());
    store.rooms.lock().unwrap().insert(ROOM, room(ROOM));
    let mut second = room(SECOND_ROOM);
    second.owner_id = GUEST;
    store.rooms.lock().unwrap().insert(SECOND_ROOM, second);
    let mut worker = GuildRoomWorker::load(live, store, Http::new(trace.clone()))
        .await
        .unwrap();
    worker.name_settings.unique_names = true;

    worker
        .rename_retries
        .insert(ROOM, ("Squad".to_owned(), 999_999));

    let mut clash = command(
        GUEST,
        false,
        NameInteraction::Submit {
            room_id: SECOND_ROOM,
            text: "squad".to_owned(),
        },
    );
    clash.settings.unique_names = true;
    let reply = refused(worker.apply_name(clash, 0));
    assert_eq!(reply, NameRefusal::Taken.to_string(), "{reply}");
}

#[tokio::test]
async fn an_unseen_channel_changes_nothing() {
    let (mut worker, trace) = setup().await;
    worker.live.remove_channel(ROOM);
    let text = refused(worker.apply_name(command(OWNER, false, submit("Den")), 0));
    assert!(text.contains("can't see that channel"), "{text}");
    assert!(worker.custom_names.is_empty());
    drain(&mut worker).await;
    assert!(trace.lock().unwrap().is_empty());
}

// --- worker: restore ----------------------------------------------------------

#[tokio::test]
async fn restore_clears_the_override_and_returns_the_template_name() {
    let (mut worker, trace) = setup().await;
    worker.creators.get_mut(&CREATOR).unwrap().name_template = "@@owner@@'s hub".to_owned();
    applied(worker.apply_name(command(OWNER, false, submit("Custom")), 0));
    drain(&mut worker).await;
    assert_eq!(channel_name(&worker, ROOM).as_deref(), Some("Custom"));
    trace.lock().unwrap().clear();

    let text = applied(worker.apply_name(command(OWNER, false, restore()), 0));
    assert!(text.contains("**Ava's hub**"), "{text}");
    assert!(worker.custom_names.is_empty());
    // Rename budget recovers after the interval; the override write is urgent.
    for step in 1..8u64 {
        worker.dispatch_one(step * 1_000_000).await;
    }
    assert_eq!(
        *trace.lock().unwrap(),
        [
            "save_custom_name:500:None".to_owned(),
            "rename:500:Ava's hub".to_owned()
        ]
    );
    assert!(worker.store.custom_names.lock().unwrap().is_empty());
    assert_eq!(channel_name(&worker, ROOM).as_deref(), Some("Ava's hub"));
}

#[tokio::test]
async fn restore_without_a_template_uses_the_default_room_name() {
    let (mut worker, _) = setup().await;
    let text = applied(worker.apply_name(command(OWNER, false, restore()), 0));
    assert!(text.contains("**Ava's room**"), "{text}");
    // Already restored: nothing is renamed twice.
    drain(&mut worker).await;
    let text = applied(worker.apply_name(command(OWNER, false, restore()), 0));
    assert!(text.contains("already uses its template name"), "{text}");
}

#[tokio::test]
async fn restore_refuses_a_template_the_filter_blocks() {
    let (mut worker, _) = setup().await;
    worker.creators.get_mut(&CREATOR).unwrap().name_template = "blorp hub".to_owned();
    applied(worker.apply_name(command(OWNER, false, submit("Custom")), 0));
    let mut cmd = command(OWNER, false, restore());
    cmd.policy = policy(&["blorp"]);
    let reply = refused(worker.apply_name(cmd, 0));
    assert_eq!(reply, NameRefusal::TemplateBlocked.to_string());
    assert_eq!(
        worker.custom_names.get(&ROOM).map(String::as_str),
        Some("Custom"),
        "a refused restore keeps the override"
    );
}

#[tokio::test]
async fn a_blocked_display_name_never_becomes_the_fallback_name() {
    let (mut worker, _) = setup().await;
    let mut cmd = command(OWNER, false, restore());
    cmd.policy = policy(&["ava"]);
    let text = applied(worker.apply_name(cmd, 0));
    assert!(text.contains("**member's room**"), "{text}");
}

// --- worker: persistence and lifecycle ----------------------------------------

#[tokio::test]
async fn a_stale_override_write_persists_nothing() {
    let (mut worker, trace) = setup().await;
    applied(worker.apply_name(command(OWNER, false, submit("First")), 0));
    applied(worker.apply_name(command(OWNER, false, restore()), 0));
    for step in 0..8u64 {
        worker.dispatch_one(step * 1_000_000).await;
    }
    let saves: Vec<String> = trace
        .lock()
        .unwrap()
        .iter()
        .filter(|entry| entry.starts_with("save_custom_name"))
        .cloned()
        .collect();
    assert_eq!(saves, ["save_custom_name:500:None"]);
}

#[tokio::test]
async fn override_write_retries_after_a_store_failure_and_halts_on_a_refused_credential() {
    let (mut worker, trace) = setup().await;
    worker
        .store
        .save_custom_name_errors
        .lock()
        .unwrap()
        .push_back(StoreError::Unavailable);
    applied(worker.apply_name(command(OWNER, false, submit("Den")), 0));
    worker.dispatch_one(0).await;
    assert!(worker.store.custom_names.lock().unwrap().is_empty());
    assert_eq!(worker.failures().len(), 1);
    drain(&mut worker).await;
    assert_eq!(
        worker
            .store
            .custom_names
            .lock()
            .unwrap()
            .get(&ROOM)
            .map(String::as_str),
        Some("Den")
    );
    let saves = trace
        .lock()
        .unwrap()
        .iter()
        .filter(|entry| entry.starts_with("save_custom_name"))
        .count();
    assert_eq!(saves, 2);

    let (mut worker, _) = setup().await;
    worker
        .store
        .save_custom_name_errors
        .lock()
        .unwrap()
        .push_back(StoreError::CredentialRefused);
    applied(worker.apply_name(command(OWNER, false, submit("Den")), 0));
    worker.dispatch_one(0).await;
    assert!(worker.halted());
}

#[tokio::test]
async fn overrides_load_survive_ownership_changes_and_go_with_the_room() {
    let trace = Trace::default();
    let live = LiveGuild::new(GUILD);
    // This scenario predates the empty-room grace and asserts immediate
    // deletion, so it shortens the grace; the grace itself is pinned by the
    // paused-time guard tests.
    live.set_empty_grace(Duration::ZERO);
    live.publish(snapshot(&[ROOM], vec![occupant(GUEST, ROOM)]));
    let store = Store::new(trace.clone());
    store.rooms.lock().unwrap().insert(ROOM, room(ROOM));
    store
        .custom_names
        .lock()
        .unwrap()
        .insert(ROOM, "Kept".to_owned());
    let mut worker = GuildRoomWorker::load(live, store, Http::new(trace.clone()))
        .await
        .unwrap();
    assert_eq!(
        worker.custom_names.get(&ROOM).map(String::as_str),
        Some("Kept")
    );
    // The owner is gone; succession hands the room to the guest and the
    // override stays with the room.
    worker.reconcile();
    assert_eq!(worker.rooms[&ROOM].owner_id, GUEST);
    assert_eq!(
        worker.custom_names.get(&ROOM).map(String::as_str),
        Some("Kept")
    );
    // Everyone leaves: the room is deleted and forgotten, override included.
    worker.live.voice_update(GUEST, None, Some(false));
    worker.reconcile();
    drain(&mut worker).await;
    assert!(worker.tracked().is_empty());
    assert!(worker.custom_names.is_empty());
}

// --- runtime: slash command, buttons and modal --------------------------------

async fn name_capture_full(
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

#[tokio::test]
async fn the_full_flow_runs_through_the_runtime() {
    let trace = Trace::default();
    let runtime = ownership_room_runtime(trace.clone()).await;
    let names = directory();
    let slash = with_user(
        voice_interaction(Some(command_data("name", Vec::new())), None, true),
        OWNER,
    );

    // `/name`: the panel, ephemeral, with request-bound buttons.
    let (owned, response) = name_capture_full(&runtime, &slash, &names).await;
    assert!(owned);
    let panel = response.expect("panel");
    assert_eq!(
        panel_ids(&panel),
        [name_custom_custom_id(500), name_restore_custom_id(500)]
    );

    // The Custom name button opens the modal for that room.
    let click = component_interaction(&name_custom_custom_id(500), None, OWNER);
    let (owned, response) = name_capture_full(&runtime, &click, &names).await;
    assert!(owned);
    let modal = response.expect("modal");
    assert_eq!(modal.kind, InteractionResponseType::Modal);
    assert_eq!(
        parse_voice_custom_id(modal.data.as_ref().unwrap().custom_id.as_deref().unwrap()),
        Some(VoiceAction::NameModal { room_id: 500 })
    );

    // Submitting it renames the room through the queue.
    let submitted = modal_interaction(
        &name_modal_custom_id(500),
        labelled_input("@@owner@@'s den"),
    );
    let (owned, response) = name_capture_full(&runtime, &submitted, &names).await;
    assert!(owned);
    assert!(response_text(&response.expect("reply")).contains("**Ava's den**"));
    wait_trace(&trace, "save_custom_name:500:Some(\"@@owner@@'s den\")").await;
    wait_trace(&trace, "rename:500:Ava's den").await;

    // Another occupant is refused with the same panel id.
    let intruder = with_user(
        component_interaction(&name_restore_custom_id(500), None, 301),
        301,
    );
    let (owned, response) = name_capture_full(&runtime, &intruder, &names).await;
    assert!(owned);
    assert!(response_text(&response.expect("reply")).contains("owner"));

    // Restore brings the template name back and clears the override.
    let restore = component_interaction(&name_restore_custom_id(500), None, OWNER);
    let (owned, response) = name_capture_full(&runtime, &restore, &names).await;
    assert!(owned);
    assert!(response_text(&response.expect("reply")).contains("Restored the template name"));
    wait_trace(&trace, "save_custom_name:500:None").await;
}

#[tokio::test]
async fn the_guild_role_gate_covers_every_name_step() {
    let trace = Trace::default();
    // `/name` is restricted to a role the owner does not hold.
    let controls = AccessControls {
        command_roles: [("name".to_owned(), vec![777])].into(),
        ..AccessControls::default()
    };
    let runtime = gated_runtime(trace.clone(), controls, None);
    let names = directory();
    for interaction in [
        with_user(
            voice_interaction(Some(command_data("name", Vec::new())), None, true),
            OWNER,
        ),
        component_interaction(&name_custom_custom_id(500), None, OWNER),
        component_interaction(&name_restore_custom_id(500), None, OWNER),
        modal_interaction(&name_modal_custom_id(500), labelled_input("Den")),
    ] {
        let (owned, response) = name_capture_full(&runtime, &interaction, &names).await;
        assert!(owned);
        assert_eq!(
            response_text(&response.expect("denial")),
            access_denied_text(AccessDenyReason::CommandRestricted)
        );
    }
    assert!(
        trace.lock().unwrap().is_empty(),
        "a gated step writes nothing"
    );
    // The role holder passes the gate; this runtime has no live guild actor,
    // so the worker step answers that it is not warmed up.
    let allowed = with_roles(
        component_interaction(&name_restore_custom_id(500), None, OWNER),
        &[777],
    );
    let (_, response) = name_capture_full(&runtime, &allowed, &names).await;
    assert!(response_text(&response.expect("reply")).contains("warmed up"));
}

#[tokio::test]
async fn an_unreadable_settings_store_refuses_a_name_instead_of_guessing() {
    let trace = Trace::default();
    let runtime = VoiceRuntime::new(
        {
            let trace = trace.clone();
            move || {
                let mut store = Store::new(trace.clone());
                store.config_error = Some(StoreError::Unavailable);
                (store, Http::new(trace.clone()))
            }
        },
        Duration::from_millis(10),
        true,
    );
    let submitted = modal_interaction(&name_modal_custom_id(500), labelled_input("Den"));
    let (owned, response) = name_capture_full(&runtime, &submitted, &directory()).await;
    assert!(owned);
    assert!(response_text(&response.expect("reply")).contains("Voice settings are unavailable"));
}

// --- custom-name re-render regression pins (review round 2, PR #1232) --------
// These pin the stale-directory fix and the new re-render gates. They live
// here (not in voice_name_rerender_tests.rs, which must stay verbatim).

/// A `/name` submit renders from the interaction's fresh directory while the
/// background re-render reads the worker's copy: the submit must copy the
/// fresh owner name over, so a later join keeps the promised name.
#[tokio::test]
async fn submit_syncs_fresh_owner_name_so_a_later_join_keeps_it() {
    let (mut worker, trace) = setup().await;
    worker.creators.get_mut(&CREATOR).unwrap().name_template = "Template @@num@@".to_owned();
    // The worker still holds a stale nickname for the owner.
    worker.name_directory.insert(OWNER, "Alice".to_owned());
    // The submit sees the fresh nickname.
    let mut cmd = command(OWNER, false, submit("@@owner@@'s den"));
    cmd.directory.insert(OWNER, "Bob".to_owned());
    let text = applied(worker.apply_name(cmd, 0));
    assert!(text.contains("Bob's den"), "{text}");
    drain(&mut worker).await;
    trace.lock().unwrap().clear();

    // A later occupant join re-renders the stored override: it must keep
    // the promised Bob render, not fall back to the stale Alice one.
    worker.live.voice_update(302, Some(ROOM), Some(false));
    worker.refresh_template_names(1);
    assert_eq!(worker.desired_names[&ROOM], "Bob's den");
    assert_eq!(
        worker.custom_names.get(&ROOM).map(String::as_str),
        Some("@@owner@@'s den")
    );
}

/// Same as above for restore: the restore reply promises a render from the
/// interaction's fresh facts, so it must sync them into the worker too.
#[tokio::test]
async fn restore_syncs_fresh_owner_name_so_a_later_join_keeps_it() {
    let (mut worker, _trace) = setup().await;
    worker.creators.get_mut(&CREATOR).unwrap().name_template = "@@owner@@'s hub".to_owned();
    worker.name_directory.insert(OWNER, "Alice".to_owned());
    let mut cmd = command(OWNER, false, restore());
    cmd.directory.insert(OWNER, "Bob".to_owned());
    let text = applied(worker.apply_name(cmd, 0));
    assert!(text.contains("Bob's hub"), "{text}");
    drain(&mut worker).await;

    worker.live.voice_update(302, Some(ROOM), Some(false));
    worker.refresh_template_names(1);
    assert_eq!(worker.desired_names[&ROOM], "Bob's hub");
    assert!(worker.custom_names.is_empty());
}

/// A literal override carries no template syntax under a templated creator,
/// so no fact change can alter its render: a join must not rename it.
#[tokio::test]
async fn literal_override_under_a_templated_creator_is_never_rerendered() {
    let (mut worker, trace) = setup().await;
    worker.creators.get_mut(&CREATOR).unwrap().name_template = "@@owner@@'s den".to_owned();
    worker.name_directory.insert(OWNER, "Alex".to_owned());
    worker.custom_names.insert(ROOM, "my room".to_owned());
    worker.refresh_template_names(0);
    assert!(!worker.dispatch_one(0).await);
    worker.live.voice_update(302, Some(ROOM), Some(false));
    worker.refresh_template_names(1);
    assert!(!worker.dispatch_one(1).await);
    assert!(trace.lock().unwrap().is_empty());
    assert_eq!(
        worker.custom_names.get(&ROOM).map(String::as_str),
        Some("my room")
    );
}

/// An occupancy-only override reads no display names, so an unknown owner
/// must not make the re-render wait.
#[tokio::test]
async fn occupancy_only_override_does_not_wait_for_an_unknown_owner() {
    let (mut worker, trace) = setup().await;
    worker.creators.get_mut(&CREATOR).unwrap().name_template = "Template @@num@@".to_owned();
    // The worker knows nobody: setup loads with an empty directory.
    worker.custom_names.insert(ROOM, "@@num@@ here".to_owned());
    worker.refresh_template_names(0);
    assert!(worker.name_waits.is_empty());
    assert!(worker.dispatch_one(0).await);
    assert_eq!(*trace.lock().unwrap(), ["rename:500:2 here".to_owned()]);
}

// --- custom-name re-render regression pins (review round 3, PR #1232) --------
// These pin the remainder of the same list: the blank-template gate, the
// override name wait, and the original-creator sync. They live here (not in
// voice_name_rerender_tests.rs, which must stay verbatim). Each fails under
// its own mutant and passes on the fixed head.

/// Under a blank creator template the room stays manually managed, override
/// or not: a tokened override must never spend a rename. Kills the
/// `(true || template.is_some())` mutant at the re-render gate.
#[tokio::test]
async fn blank_template_with_a_tokened_override_is_never_rerendered() {
    let (mut worker, trace) = setup().await;
    // The fixture creator template is blank by default: keep it blank so the
    // room is manually managed even though the override reads facts.
    assert!(
        worker.creators[&CREATOR].name_template.trim().is_empty(),
        "fixture creator must start blank"
    );
    worker.custom_names.insert(ROOM, "@@num@@ here".to_owned());
    worker.refresh_template_names(0);
    assert!(!worker.dispatch_one(0).await);
    worker.live.voice_update(302, Some(ROOM), Some(false));
    worker.refresh_template_names(1);
    assert!(!worker.dispatch_one(1).await);
    assert!(trace.lock().unwrap().is_empty());
}

/// An `@@owner@@` override with an unknown owner must wait for the name
/// instead of renaming the room to "member's den". Kills the
/// force-`unknown`-to-false mutant in `refresh_custom_name`.
#[tokio::test]
async fn owner_override_with_an_unknown_owner_waits_for_the_name() {
    let (mut worker, trace) = setup().await;
    worker.creators.get_mut(&CREATOR).unwrap().name_template = "Template @@num@@".to_owned();
    // The worker knows nobody: setup loads with an empty directory, so the
    // owner display name is unknown.
    worker
        .custom_names
        .insert(ROOM, "@@owner@@'s den".to_owned());
    worker.refresh_template_names(0);
    assert!(!worker.dispatch_one(0).await);
    assert!(
        worker.name_waits.contains_key(&ROOM),
        "the room must wait for its owner name"
    );
    worker.name_directory.insert(OWNER, "Alex".to_owned());
    worker.refresh_template_names(1);
    drain(&mut worker).await;
    assert_eq!(*trace.lock().unwrap(), ["rename:500:Alex's den".to_owned()]);
}

/// A `/name` submit must sync the fresh original-creator name (not just the
/// owner), so a later join keeps the promised render when the two differ.
/// Kills the `[owner_id, original_creator_id] -> [owner_id, owner_id]`
/// mutants on the submit/restore copy loops.
#[tokio::test]
async fn submit_syncs_differing_original_creator_name_so_a_later_join_keeps_it() {
    let (mut worker, trace) = setup().await;
    worker.creators.get_mut(&CREATOR).unwrap().name_template = "Template @@num@@".to_owned();
    worker.rooms.get_mut(&ROOM).unwrap().original_creator_id = GUEST;
    // The worker still holds a stale nickname for the original creator,
    // while the submit sees the fresh one (`directory()` maps GUEST to Ben).
    worker.name_directory.insert(GUEST, "Stale".to_owned());
    let text = applied(worker.apply_name(
        command(OWNER, false, submit("@@original_creator@@'s den")),
        0,
    ));
    assert!(text.contains("Ben's den"), "{text}");
    drain(&mut worker).await;
    trace.lock().unwrap().clear();

    worker.live.voice_update(302, Some(ROOM), Some(false));
    worker.refresh_template_names(1);
    assert_eq!(worker.desired_names[&ROOM], "Ben's den");
}

async fn lost_name_worker_reply(drop_reply: bool) {
    let (runtime, mut rx) = stalled_worker_runtime();
    let interaction = voice_interaction_as(Some(command_data("name", vec![])), None, OWNER);
    let names = directory();
    let call = name_capture_full(&runtime, &interaction, &names);
    tokio::pin!(call);
    let command = tokio::select! {
        command = rx.recv() => command.expect("accepted name command"),
        _ = &mut call => panic!("name handler did not reach its worker boundary"),
    };
    assert!(matches!(command, ActorCommand::Name { .. }));
    // Keep the inbox live in both cases; only the accepted reply is lost.
    let held = if drop_reply {
        drop(command);
        None
    } else {
        Some(command)
    };
    let (owned, response) = bounded_sibling_wait(&mut call).await;
    assert!(owned);
    assert_sibling_worker_lost_reply(&response.expect("one name failure reply"));
    drop(held);
}

#[tokio::test(start_paused = true)]
async fn stalled_name_worker_reply_returns_honest_user_copy() {
    lost_name_worker_reply(false).await;
}

#[tokio::test(start_paused = true)]
async fn dropped_name_worker_reply_returns_honest_user_copy() {
    lost_name_worker_reply(true).await;
}

#[tokio::test(start_paused = true)]
async fn name_wait_is_bounded_with_a_live_unanswered_inbox() {
    let (runtime, _rx) = stalled_worker_runtime();
    let command = NameCommand {
        actor_id: MEMBER,
        is_admin: false,
        request: restore(),
        settings: NameSettings::default(),
        directory: directory(),
        policy: policy(&[]),
    };
    assert!(bounded_sibling_wait(runtime.run_name(GUILD, command))
        .await
        .is_none());
}
