// Acceptance: voice authority warming-up reply for cold guild-role facts.
//
// When required guild-role facts have not loaded, a protected voice
// interaction must reply that the bot is warming up and ask for a retry,
// fail closed, and produce no command side effects. It must not report a
// permanent permission denial for that transient condition. Once facts are
// available, authorized interactions keep existing behavior and genuinely
// denied interactions keep permanent denials. Channel-scoped
// `member.permissions` must never authorize guild-wide actions, missing-
// member/malformed handling stays separate from a known cold state, and
// existing owner/timeout protections stay unchanged.
//
// Inventory of guild-authority seams covered or guarded:
// - dispatch gate: `handle_voice_interaction_full` builds `access_member`
//   from `guild_permissions` and runs `command_gate` before dispatch;
// - `/create`, `/textchannels`, `/position`, `/group`,
//   `/inheritpermissions`, `/defaultlimit`, `/alwaysprivate` check
//   `may_create` (Manage Channels);
// - `/access`, `/logging` and `/voice-orphans` require trusted Manage Channels
//   or Administrator authority before persistence (same admin helper);
// - `/export`, `/import` (upload + Confirm/Cancel) check
//   `may_manage_server` (Manage Guild).
// Representative protected commands below plus the cold-to-ready transition
// exercise the shared `guild_permissions: None` collapse point. Synthetic
// fixtures only; no Discord, database, or REST calls.

use super::*;

fn is_warming_up(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let warming = lower.contains("warm");
    let retry = lower.contains("try again") || lower.contains("retry") || lower.contains("moment");
    let permanent = text.contains("You need Manage Channels")
        || text.contains("You need Manage Server")
        || text.contains("You do not have a role")
        || text.contains("You need the server");
    warming && retry && !permanent
}

fn assert_warming_up(text: &str, what: &str) {
    assert!(
        is_warming_up(text),
        "{what}: expected warming-up + retry reply, got: {text:?}"
    );
}

fn assert_permanent_denial(text: &str, what: &str) {
    let lower = text.to_ascii_lowercase();
    assert!(
        !lower.contains("warm"),
        "{what}: permanent denial must not claim warming up, got: {text:?}"
    );
    assert!(
        text.contains("You need Manage Channels") || text.contains("You need Manage Server"),
        "{what}: expected permanent denial, got: {text:?}"
    );
}

/// Raw dispatch seam without auto-warming. `handle_capture` publishes a
/// snapshot when no actor exists; cold fixtures here deliberately skip it so
/// `guild_permissions` stays `None`.
async fn cold_handle<S, H>(
    runtime: &VoiceRuntime<S, H>,
    interaction: &Interaction,
) -> (bool, Option<InteractionResponse>)
where
    S: RoomPersistence + Send + 'static,
    H: RoomWrites + Send + 'static,
{
    let seen = std::sync::Arc::new(std::sync::Mutex::new(None::<InteractionResponse>));
    let writer = seen.clone();
    let owned = handle_voice_interaction(runtime, interaction, |response| {
        *writer.lock().unwrap() = Some(response);
        async {}
    })
    .await;
    let response = seen.lock().unwrap().clone();
    (owned, response)
}

#[tokio::test]
async fn cold_create_replies_warming_up_and_creates_nothing() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = voice_interaction(
        Some(command_data(
            "create",
            vec![command_option("name", "lobby")],
        )),
        Some(Permissions::MANAGE_CHANNELS),
        true,
    );
    let (owned, response) = cold_handle(&runtime, &interaction).await;
    assert!(
        owned,
        "cold /create must be answered, not left to another sink"
    );
    let text = response_text(&response.expect("cold /create must reply"));
    assert_warming_up(&text, "cold /create");
    assert!(
        trace.lock().unwrap().is_empty(),
        "cold /create must not create or persist, trace: {:?}",
        trace.lock().unwrap()
    );
}

#[tokio::test]
async fn cold_create_via_responder_defers_completes_and_creates_nothing() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let replies = Replies::new(trace.clone());
    VoiceResponder::respond_with(&runtime, &replies, &create_interaction(), None, None).await;
    assert_eq!(
        *trace.lock().unwrap(),
        ["defer", "complete"],
        "cold responder must defer once, complete once, and create nothing"
    );
    let completed = replies.completed.lock().unwrap();
    assert_eq!(completed.len(), 1);
    let text = response_text(&completed[0]);
    assert_warming_up(&text, "cold responder /create");
}

#[tokio::test]
async fn cold_access_replies_warming_up_without_side_effects() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = voice_interaction(
        Some(command_data("access", Vec::new())),
        Some(Permissions::MANAGE_CHANNELS),
        true,
    );
    let (owned, response) = cold_handle(&runtime, &interaction).await;
    assert!(owned);
    let text = response_text(&response.expect("cold /access must reply"));
    assert_warming_up(&text, "cold /access");
    assert!(
        trace.lock().unwrap().is_empty(),
        "cold /access must not read/write effects, trace: {:?}",
        trace.lock().unwrap()
    );
}

#[tokio::test]
async fn cold_logging_replies_warming_up_without_side_effects() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = voice_interaction(
        Some(command_data("logging", Vec::new())),
        Some(Permissions::MANAGE_CHANNELS),
        true,
    );
    let (owned, response) = cold_handle(&runtime, &interaction).await;
    assert!(owned);
    let text = response_text(&response.expect("cold /logging must reply"));
    assert_warming_up(&text, "cold /logging");
    assert!(
        trace.lock().unwrap().is_empty(),
        "cold /logging must not read/write effects, trace: {:?}",
        trace.lock().unwrap()
    );
}

#[tokio::test]
async fn cold_textchannels_replies_warming_up_without_side_effects() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = voice_interaction(
        Some(command_data(
            "textchannels",
            vec![channel_option("channel", CREATOR)],
        )),
        Some(Permissions::MANAGE_CHANNELS),
        true,
    );
    let (owned, response) = cold_handle(&runtime, &interaction).await;
    assert!(owned);
    let text = response_text(&response.expect("cold /textchannels must reply"));
    assert_warming_up(&text, "cold /textchannels");
    assert!(
        trace.lock().unwrap().is_empty(),
        "cold /textchannels must not read/write effects, trace: {:?}",
        trace.lock().unwrap()
    );
}

#[tokio::test]
async fn cold_export_replies_warming_up_without_side_effects() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = voice_interaction(
        Some(command_data("export", Vec::new())),
        Some(Permissions::MANAGE_CHANNELS),
        true,
    );
    let (owned, response) = cold_handle(&runtime, &interaction).await;
    assert!(owned);
    let text = response_text(&response.expect("cold /export must reply"));
    assert_warming_up(&text, "cold /export");
    assert!(
        trace.lock().unwrap().is_empty(),
        "cold /export must not send or persist, trace: {:?}",
        trace.lock().unwrap()
    );
}

#[tokio::test]
async fn cold_administrator_claim_still_warming_up() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = voice_interaction(
        Some(command_data(
            "create",
            vec![command_option("name", "lobby")],
        )),
        Some(Permissions::ADMINISTRATOR),
        true,
    );
    let (owned, response) = cold_handle(&runtime, &interaction).await;
    assert!(owned);
    let text = response_text(&response.expect("cold admin-claim /create must reply"));
    assert_warming_up(&text, "cold admin-claim /create");
    assert!(
        trace.lock().unwrap().is_empty(),
        "cold admin-claim must fail closed, trace: {:?}",
        trace.lock().unwrap()
    );
}

#[tokio::test]
async fn cold_to_ready_create_transition_succeeds_after_snapshot() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = voice_interaction(
        Some(command_data(
            "create",
            vec![command_option("name", "lobby")],
        )),
        Some(Permissions::MANAGE_CHANNELS),
        true,
    );
    let (owned_cold, cold_response) = cold_handle(&runtime, &interaction).await;
    assert!(owned_cold);
    let cold_text = response_text(&cold_response.expect("cold must reply first"));
    assert_warming_up(&cold_text, "cold phase");
    assert!(
        trace.lock().unwrap().is_empty(),
        "cold phase must not create, trace: {:?}",
        trace.lock().unwrap()
    );

    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    let (owned_ready, ready_response) = cold_handle(&runtime, &interaction).await;
    assert!(owned_ready);
    let ready_text = response_text(&ready_response.expect("ready must reply"));
    assert!(
        ready_text.contains("Created <#500>"),
        "ready authorized /create must succeed, got: {ready_text:?}"
    );
    assert!(
        !ready_text.to_ascii_lowercase().contains("warm"),
        "ready authorized /create must not claim warming up: {ready_text:?}"
    );
}

#[tokio::test]
async fn ready_denied_create_retains_permanent_denial() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    let interaction = voice_interaction(
        Some(command_data(
            "create",
            vec![command_option("name", "lobby")],
        )),
        None,
        true,
    );
    let (owned, response) = cold_handle(&runtime, &interaction).await;
    assert!(owned);
    let text = response_text(&response.expect("ready denied must reply"));
    assert_permanent_denial(&text, "ready denied /create");
    assert!(
        trace.lock().unwrap().is_empty(),
        "denied /create must not create, trace: {:?}",
        trace.lock().unwrap()
    );
}

#[tokio::test]
async fn ready_authorized_create_succeeds() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    let interaction = create_interaction();
    let (owned, response) = cold_handle(&runtime, &interaction).await;
    assert!(owned);
    let text = response_text(&response.expect("ready authorized must reply"));
    assert!(
        text.contains("Created <#500>"),
        "authorized /create must succeed, got: {text:?}"
    );
}

#[tokio::test]
async fn missing_guild_stays_unowned_and_separate_from_cold() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = voice_interaction(
        Some(command_data(
            "create",
            vec![command_option("name", "lobby")],
        )),
        Some(Permissions::MANAGE_CHANNELS),
        false,
    );
    let (owned, response) = cold_handle(&runtime, &interaction).await;
    assert!(!owned, "missing guild must stay unowned for other sinks");
    assert!(response.is_none(), "missing guild must not reply");
    assert!(trace.lock().unwrap().is_empty());
}

#[tokio::test]
async fn ready_owner_and_timeout_protections_unchanged() {
    use twilight_model::util::Timestamp;

    // Warm guild owner still resolves to all permissions.
    let trace = Trace::default();
    let runtime = test_runtime(trace);
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    let owner = voice_interaction_as(
        Some(command_data(
            "create",
            vec![command_option("name", "lobby")],
        )),
        None,
        998,
    );
    let (owned, response) = cold_handle(&runtime, &owner).await;
    assert!(owned);
    let text = response_text(&response.expect("owner must reply"));
    assert!(
        text.contains("Created <#500>"),
        "warm owner /create must succeed, got: {text:?}"
    );

    // Warm timed-out member keeps only readable bits, so the permanent
    // Manage Channels denial remains (not warming up).
    let trace2 = Trace::default();
    let runtime2 = test_runtime(trace2.clone());
    assert!(runtime2.publish_snapshot(GUILD, command_snapshot()));
    let mut timed_out = voice_interaction(
        Some(command_data(
            "create",
            vec![command_option("name", "lobby")],
        )),
        Some(Permissions::MANAGE_CHANNELS),
        true,
    );
    let until = time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000
        + 28 * 24 * 60 * 60 * 1_000_000;
    timed_out
        .member
        .as_mut()
        .unwrap()
        .communication_disabled_until =
        Some(Timestamp::from_micros(i64::try_from(until).unwrap()).unwrap());
    let (owned2, response2) = cold_handle(&runtime2, &timed_out).await;
    assert!(owned2);
    let text2 = response_text(&response2.expect("timed-out must reply"));
    assert_permanent_denial(&text2, "warm timed-out /create");
    assert!(
        trace2.lock().unwrap().is_empty(),
        "timed-out /create must not create, trace: {:?}",
        trace2.lock().unwrap()
    );
}

#[tokio::test]
async fn cold_import_confirm_replies_warming_up_and_retains_preview() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let hash = "0123456789abcdef";
    runtime.remember_pending_import(GUILD, super::UPLOADER, hash, super::full_config());
    let custom_id = two_bot_core::voice_custom_id::import_confirm_custom_id(super::UPLOADER, hash);
    let interaction = super::component_interaction(&custom_id, super::manager(), super::UPLOADER);
    let (owned, response) = cold_handle(&runtime, &interaction).await;
    assert!(owned, "cold Import Confirm must be answered");
    let response = response.expect("cold Import Confirm must reply");
    assert_eq!(
        response.data.as_ref().and_then(|data| data.flags),
        Some(twilight_model::channel::message::MessageFlags::EPHEMERAL),
        "cold Import Confirm must reply privately"
    );
    let text = response_text(&response);
    assert_eq!(
        text,
        super::super::WARMING_UP_REPLY,
        "cold Import Confirm must send the shared warming-up reply"
    );
    assert_warming_up(&text, "cold Import Confirm");
    assert!(
        trace.lock().unwrap().is_empty(),
        "cold Import Confirm must not write, trace: {:?}",
        trace.lock().unwrap()
    );
    assert!(
        runtime
            .pending_imports
            .lock()
            .expect("voice runtime lock")
            .contains_key(&(GUILD, super::UPLOADER, hash.to_owned())),
        "cold Import Confirm must retain the preview for retry"
    );
}

#[tokio::test]
async fn cold_import_cancel_replies_warming_up_and_retains_preview() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let hash = "0123456789abcdef";
    runtime.remember_pending_import(GUILD, super::UPLOADER, hash, super::full_config());
    let custom_id = two_bot_core::voice_custom_id::import_cancel_custom_id(super::UPLOADER, hash);
    let interaction = super::component_interaction(&custom_id, super::manager(), super::UPLOADER);
    let (owned, response) = cold_handle(&runtime, &interaction).await;
    assert!(owned, "cold Import Cancel must be answered");
    let response = response.expect("cold Import Cancel must reply");
    assert_eq!(
        response.data.as_ref().and_then(|data| data.flags),
        Some(twilight_model::channel::message::MessageFlags::EPHEMERAL),
        "cold Import Cancel must reply privately"
    );
    let text = response_text(&response);
    assert_eq!(
        text,
        super::super::WARMING_UP_REPLY,
        "cold Import Cancel must send the shared warming-up reply"
    );
    assert_warming_up(&text, "cold Import Cancel");
    assert!(
        trace.lock().unwrap().is_empty(),
        "cold Import Cancel must not write, trace: {:?}",
        trace.lock().unwrap()
    );
    assert!(
        runtime
            .pending_imports
            .lock()
            .expect("voice runtime lock")
            .contains_key(&(GUILD, super::UPLOADER, hash.to_owned())),
        "cold Import Cancel must retain the preview for retry"
    );
}

#[tokio::test]
async fn cold_position_replies_warming_up_without_side_effects() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = super::v8_interaction(
        "position",
        vec![super::typed_option(
            "channel",
            twilight_model::application::interaction::application_command::CommandOptionValue::Channel(
                twilight_model::id::Id::new(super::CREATOR),
            ),
        )],
    );
    let (owned, response) = cold_handle(&runtime, &interaction).await;
    assert!(owned);
    let text = response_text(&response.expect("cold /position must reply"));
    assert_warming_up(&text, "cold /position");
    assert!(
        trace.lock().unwrap().is_empty(),
        "cold /position must not read/write effects, trace: {:?}",
        trace.lock().unwrap()
    );
}

#[tokio::test]
async fn cold_group_replies_warming_up_without_side_effects() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = super::v8_interaction(
        "group",
        vec![
            super::typed_option(
                "channel",
                twilight_model::application::interaction::application_command::CommandOptionValue::Channel(
                    twilight_model::id::Id::new(super::CREATOR),
                ),
            ),
            super::typed_option(
                "enabled",
                twilight_model::application::interaction::application_command::CommandOptionValue::Boolean(
                    false,
                ),
            ),
        ],
    );
    let (owned, response) = cold_handle(&runtime, &interaction).await;
    assert!(owned);
    let text = response_text(&response.expect("cold /group must reply"));
    assert_warming_up(&text, "cold /group");
    assert!(
        trace.lock().unwrap().is_empty(),
        "cold /group must not read/write effects, trace: {:?}",
        trace.lock().unwrap()
    );
}

#[tokio::test]
async fn cold_inheritpermissions_replies_warming_up_without_side_effects() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = super::v8_interaction(
        "inheritpermissions",
        vec![
            super::typed_option(
                "channel",
                twilight_model::application::interaction::application_command::CommandOptionValue::Channel(
                    twilight_model::id::Id::new(super::CREATOR),
                ),
            ),
            super::typed_option(
                "source",
                twilight_model::application::interaction::application_command::CommandOptionValue::String(
                    "channel".to_owned(),
                ),
            ),
            super::typed_option(
                "source-channel",
                twilight_model::application::interaction::application_command::CommandOptionValue::Channel(
                    twilight_model::id::Id::new(super::CATEGORY),
                ),
            ),
        ],
    );
    let (owned, response) = cold_handle(&runtime, &interaction).await;
    assert!(owned);
    let text = response_text(&response.expect("cold /inheritpermissions must reply"));
    assert_warming_up(&text, "cold /inheritpermissions");
    assert!(
        trace.lock().unwrap().is_empty(),
        "cold /inheritpermissions must not read/write effects, trace: {:?}",
        trace.lock().unwrap()
    );
}

#[tokio::test]
async fn cold_defaultlimit_replies_warming_up_without_side_effects() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = super::v8_interaction(
        "defaultlimit",
        vec![
            super::typed_option(
                "channel",
                twilight_model::application::interaction::application_command::CommandOptionValue::Channel(
                    twilight_model::id::Id::new(super::CREATOR),
                ),
            ),
            super::typed_option(
                "limit",
                twilight_model::application::interaction::application_command::CommandOptionValue::Integer(
                    4,
                ),
            ),
        ],
    );
    let (owned, response) = cold_handle(&runtime, &interaction).await;
    assert!(owned);
    let text = response_text(&response.expect("cold /defaultlimit must reply"));
    assert_warming_up(&text, "cold /defaultlimit");
    assert!(
        trace.lock().unwrap().is_empty(),
        "cold /defaultlimit must not read/write effects, trace: {:?}",
        trace.lock().unwrap()
    );
}

#[tokio::test]
async fn cold_alwaysprivate_replies_warming_up_without_side_effects() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = super::v8_interaction(
        "alwaysprivate",
        vec![super::typed_option(
            "channel",
            twilight_model::application::interaction::application_command::CommandOptionValue::Channel(
                twilight_model::id::Id::new(super::CREATOR),
            ),
        )],
    );
    let (owned, response) = cold_handle(&runtime, &interaction).await;
    assert!(owned);
    let text = response_text(&response.expect("cold /alwaysprivate must reply"));
    assert_warming_up(&text, "cold /alwaysprivate");
    assert!(
        trace.lock().unwrap().is_empty(),
        "cold /alwaysprivate must not read/write effects, trace: {:?}",
        trace.lock().unwrap()
    );
}

#[tokio::test]
async fn cold_import_upload_replies_warming_up_without_side_effects() {
    let trace = Trace::default();
    let runtime = test_runtime(trace.clone());
    let interaction = super::import_interaction(10, super::manager(), super::UPLOADER);
    let (owned, response) = cold_handle(&runtime, &interaction).await;
    assert!(owned);
    let text = response_text(&response.expect("cold /import must reply"));
    assert_warming_up(&text, "cold /import");
    assert!(
        trace.lock().unwrap().is_empty(),
        "cold /import must not send or persist, trace: {:?}",
        trace.lock().unwrap()
    );
}

#[tokio::test]
async fn cold_to_ready_orphans_preserves_authority_states_before_store_construction() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let trace = Trace::default();
    let constructions = std::sync::Arc::new(AtomicUsize::new(0));
    let runtime = VoiceRuntime::new(
        {
            let trace = trace.clone();
            let constructions = constructions.clone();
            move || {
                constructions.fetch_add(1, Ordering::SeqCst);
                (Store::new(trace.clone()), Http::new(trace.clone()))
            }
        },
        Duration::from_millis(10),
        true,
    );
    let staff = voice_interaction(
        Some(command_data("voice-orphans", vec![])),
        Some(Permissions::MANAGE_CHANNELS),
        true,
    );
    let (owned, response) = cold_handle(&runtime, &staff).await;
    assert!(owned);
    let response = response.expect("cold orphan inspection replies");
    assert_warming_up(&response_text(&response), "cold /voice-orphans");
    assert_eq!(
        response.data.as_ref().and_then(|data| data.flags),
        Some(MessageFlags::EPHEMERAL)
    );
    assert_eq!(
        constructions.load(Ordering::SeqCst),
        0,
        "cold facts must not construct a store"
    );
    assert!(trace.lock().unwrap().is_empty());

    // Publish trusted facts without a background worker that could hide SQL or effects.
    let live = LiveGuild::new(GUILD);
    live.publish(command_snapshot());
    let (tx, _inbox) = mpsc::unbounded_channel();
    runtime.actors.lock().unwrap().insert(
        GUILD,
        GuildActor {
            live,
            tx,
            backpressure: ActorInboxBackpressure::default(),
        },
    );
    let (owned, response) = cold_handle(&runtime, &staff).await;
    assert!(owned);
    let text = response_text(&response.expect("ready staff replies"));
    // The memory adapter deliberately has no evidence store. Its explicit read
    // error proves authorization reached storage, rather than inventing success.
    assert!(
        text.contains("Could not read voice orphan evidence"),
        "{text}"
    );
    assert!(!text.contains("You need Manage Channels"));
    assert!(!text.to_ascii_lowercase().contains("warm"));
    assert!(constructions.load(Ordering::SeqCst) > 0);

    let before_denied = constructions.load(Ordering::SeqCst);
    let denied = voice_interaction(Some(command_data("voice-orphans", vec![])), None, true);
    let (owned, response) = cold_handle(&runtime, &denied).await;
    assert!(owned);
    assert_permanent_denial(
        &response_text(&response.expect("known denial replies")),
        "ready denied /voice-orphans",
    );
    assert_eq!(
        constructions.load(Ordering::SeqCst),
        before_denied,
        "known denial must not construct a store"
    );
    assert!(trace.lock().unwrap().is_empty());
}
