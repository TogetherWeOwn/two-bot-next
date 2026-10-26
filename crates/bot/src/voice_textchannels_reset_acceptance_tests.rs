//! Protected reset contract through real slash dispatch, persisted creator rows,
//! emitted worker commands and the existing companion planner. In-memory I/O
//! doubles only. The cases below are the test matrix; no runtime API is invented.
#![cfg(test)]

use super::*;
use two_bot_core::voice_text_channel::{text_channel_plan, OverwriteTarget, VoiceRoomFacts};

const EXTRA_ROLE: u64 = 42;

fn configured_creator(enabled: bool) -> CreatorChannel {
    let mut creator = CreatorChannel::new(GUILD, CREATOR);
    creator.text_channels = enabled;
    creator.text_channel_name = Some("Lounge".to_owned());
    creator.text_viewer_role_id = Some(EXTRA_ROLE);
    creator.name_template = "Private @@owner@@".to_owned();
    creator.default_limit = Some(7);
    creator.private_default = true;
    creator.position = RoomPosition::Below;
    creator.first_room_number = 9;
    creator.group_by_category = true;
    creator.permission_source = PermissionSource::Channel(CATEGORY);
    creator.permission_channel_id = Some(CATEGORY);
    creator
}

fn snapshot_creator() -> CreatorChannel {
    let mut creator = text_creator();
    creator.text_channel_name = Some("Lounge".to_owned());
    creator.text_viewer_role_id = Some(EXTRA_ROLE);
    creator
}

fn reset(name: &str, value: bool) -> CommandDataOption {
    typed_option(name, CommandOptionValue::Boolean(value))
}

fn replacement(name: &str) -> CommandDataOption {
    typed_option("name", CommandOptionValue::String(name.to_owned()))
}

fn viewer(role: u64) -> CommandDataOption {
    typed_option("viewer-role", CommandOptionValue::Role(Id::new(role)))
}

fn options(mut args: Vec<CommandDataOption>) -> Vec<CommandDataOption> {
    args.insert(
        0,
        typed_option("channel", CommandOptionValue::Channel(Id::new(CREATOR))),
    );
    args
}

struct Harness {
    runtime: VoiceRuntime<Store, Http>,
    rows: Arc<Mutex<Vec<CreatorChannel>>>,
    trace: Trace,
    inbox: mpsc::UnboundedReceiver<ActorCommand>,
}

impl Harness {
    fn new(creator: CreatorChannel, read: Option<StoreError>, write: Option<StoreError>) -> Self {
        // The second creator detects accidental guild-wide replacement/reset.
        let rows = Arc::new(Mutex::new(vec![creator, configured_sibling()]));
        let trace = Trace::default();
        let runtime = VoiceRuntime::new(
            {
                let rows = rows.clone();
                let trace = trace.clone();
                move || {
                    let mut store = Store::new(trace.clone());
                    store.creators = rows.clone();
                    store.creator_for_error = read;
                    *store.add_creator_error.lock().unwrap() = write;
                    (store, Http::new(trace.clone()))
                }
            },
            Duration::from_millis(10),
            true,
        );
        // Hold the real actor's input boundary rather than starting a timed
        // worker. We observe the actual saved notification deterministically.
        let live = LiveGuild::new(GUILD);
        live.publish(command_snapshot());
        let (tx, inbox) = mpsc::unbounded_channel();
        runtime.actors.lock().unwrap().insert(
            GUILD,
            GuildActor {
                live,
                tx,
                backpressure: ActorInboxBackpressure::default(),
            },
        );
        Self {
            runtime,
            rows,
            trace,
            inbox,
        }
    }

    async fn send(&self, args: Vec<CommandDataOption>) -> InteractionResponse {
        self.send_interaction(textchannels_interaction(
            options(args),
            Some(Permissions::MANAGE_CHANNELS),
        ))
        .await
    }

    async fn send_interaction(&self, interaction: Interaction) -> InteractionResponse {
        let (owned, response) = handle_capture(&self.runtime, &interaction).await;
        assert!(
            owned,
            "malformed textchannels commands must still receive a private refusal"
        );
        let response = response.expect("textchannels must reply");
        let data = response.data.as_ref().expect("response data");
        assert_eq!(data.flags, Some(MessageFlags::EPHEMERAL));
        let text = response_text(&response);
        assert!(!text.is_empty());
        assert!(
            text.encode_utf16().count() <= 2000,
            "unbounded reply: {text}"
        );
        response
    }

    fn saved(&self) -> CreatorChannel {
        self.rows
            .lock()
            .unwrap()
            .iter()
            .find(|row| row.channel_id == CREATOR)
            .expect("creator preserved")
            .clone()
    }

    fn only_creator_writes(&self) -> usize {
        let trace = self.trace.lock().unwrap();
        assert!(trace.iter().all(|entry| entry == &format!("add_creator:{CREATOR}")),
            "settings must not mutate Discord, companion rows, audit policy or unrelated state: {trace:?}");
        trace.len()
    }

    fn unchanged(&mut self, before: &[CreatorChannel]) {
        assert_eq!(self.rows.lock().unwrap().as_slice(), before);
        assert_eq!(self.only_creator_writes(), 0);
        assert!(
            self.inbox.try_recv().is_err(),
            "refusal must not notify the worker"
        );
    }
}

fn configured_sibling() -> CreatorChannel {
    let mut creator = configured_creator(false);
    creator.channel_id = CREATOR + 1;
    creator.text_channel_name = Some("Sibling".to_owned());
    creator
}

fn assert_mention_safe(response: &InteractionResponse) {
    let text = response_text(response);
    if text.contains("<@") || text.contains("@everyone") || text.contains("@here") {
        let mentions = response
            .data
            .as_ref()
            .expect("response data")
            .allowed_mentions
            .as_ref()
            .expect("disable mentions in settings replies");
        assert!(mentions.parse.is_empty());
        assert!(mentions.users.is_empty());
        assert!(mentions.roles.is_empty());
        assert!(!mentions.replied_user);
    }
}

fn assert_future_notice(text: &str) {
    let lower = text.to_lowercase();
    assert!(
        lower.contains("future") || lower.contains("new companion") || lower.contains("new room"),
        "explain when saved settings apply: {text}"
    );
    assert!(
        lower.contains("existing") || lower.contains("creation-time") || lower.contains("snapshot"),
        "explain the existing-companion snapshot: {text}"
    );
    assert!(
        !lower.contains("renamed existing") && !lower.contains("revoked access"),
        "a config save must not claim a live Discord change: {text}"
    );
}

async fn saved_case(
    args: Vec<CommandDataOption>,
    enabled: bool,
    name: Option<&str>,
    role: Option<u64>,
) {
    let original = configured_creator(enabled);
    let mut harness = Harness::new(original.clone(), None, None);
    let response = harness.send(args).await;
    let mut expected = original;
    expected.text_channel_name = name.map(str::to_owned);
    expected.text_viewer_role_id = role;
    assert_eq!(
        harness.saved(),
        expected,
        "nullable reset must preserve every unrelated creator field"
    );
    assert_eq!(
        harness
            .rows
            .lock()
            .unwrap()
            .iter()
            .find(|row| row.channel_id == CREATOR + 1),
        Some(&configured_sibling())
    );
    assert_eq!(harness.only_creator_writes(), 1);
    match harness
        .inbox
        .try_recv()
        .expect("notify worker after successful save")
    {
        ActorCommand::CreatorAdded(saved) => assert_eq!(saved, expected),
        _ => panic!("expected the saved creator notification"),
    }
    assert!(
        harness.inbox.try_recv().is_err(),
        "one successful save, one notification"
    );
    let text = response_text(&response);
    assert_mention_safe(&response);
    assert_future_notice(&text);
    if name.is_none() {
        assert!(
            text.contains("voice-chat"),
            "show actual saved default name even when off: {text}"
        );
    }
    if role.is_none() {
        let lower = text.to_lowercase();
        assert!(
            lower.contains("occupant") && lower.contains("admin"),
            "show default audience even when off: {text}"
        );
    }
    if !enabled {
        let lower = text.to_lowercase();
        assert!(
            lower.contains("off") || lower.contains("disabled"),
            "must remain disabled: {text}"
        );
    }
}

#[tokio::test]
async fn reset_name_preserves_viewer_role_and_other_settings() {
    saved_case(
        vec![reset("reset-name", true)],
        true,
        None,
        Some(EXTRA_ROLE),
    )
    .await;
}

#[tokio::test]
async fn reset_viewer_role_preserves_name_and_other_settings() {
    saved_case(
        vec![reset("reset-viewer-role", true)],
        true,
        Some("Lounge"),
        None,
    )
    .await;
}

#[tokio::test]
async fn reset_both_stores_nullable_defaults() {
    saved_case(
        vec![reset("reset-name", true), reset("reset-viewer-role", true)],
        true,
        None,
        None,
    )
    .await;
}

#[tokio::test]
async fn reset_both_while_disabled_does_not_revive_companions() {
    saved_case(
        vec![reset("reset-name", true), reset("reset-viewer-role", true)],
        false,
        None,
        None,
    )
    .await;
}

#[tokio::test]
async fn reset_one_field_can_replace_the_other() {
    saved_case(
        vec![reset("reset-name", true), viewer(GUILD)],
        true,
        None,
        Some(GUILD),
    )
    .await;
    saved_case(
        vec![
            reset("reset-viewer-role", true),
            replacement("  New lounge  "),
        ],
        true,
        Some("New lounge"),
        None,
    )
    .await;
}

#[tokio::test]
async fn control_omitted_and_false_controls_preserve_existing_values() {
    for args in [
        vec![],
        vec![reset("reset-name", false)],
        vec![reset("reset-viewer-role", false)],
        vec![
            reset("reset-name", false),
            reset("reset-viewer-role", false),
        ],
    ] {
        let original = configured_creator(false);
        let mut harness = Harness::new(original.clone(), None, None);
        harness.send(args).await;
        assert_eq!(harness.saved(), original);
        let writes = harness.only_creator_writes();
        assert!(writes <= 1);
        let mut notifications = 0;
        while let Ok(command) = harness.inbox.try_recv() {
            match command {
                ActorCommand::CreatorAdded(saved) => assert_eq!(saved, original),
                _ => panic!("unexpected notification"),
            }
            notifications += 1;
        }
        assert_eq!(notifications, writes, "only committed state is notified");
    }
}

#[tokio::test]
async fn false_reset_does_not_conflict_with_replacement() {
    saved_case(
        vec![
            reset("reset-name", false),
            reset("reset-viewer-role", false),
            replacement("Other"),
            viewer(GUILD),
        ],
        true,
        Some("Other"),
        Some(GUILD),
    )
    .await;
}

#[tokio::test]
async fn resetting_already_default_reports_no_change_without_reenabling() {
    let original = CreatorChannel::new(GUILD, CREATOR);
    let mut harness = Harness::new(original.clone(), None, None);
    let response = harness
        .send(vec![
            reset("reset-name", true),
            reset("reset-viewer-role", true),
        ])
        .await;
    assert_eq!(harness.saved(), original);
    let text = response_text(&response).to_lowercase();
    assert!(
        text.contains("already") || text.contains("unchanged") || text.contains("no change"),
        "{text}"
    );
    assert!(
        text.contains("voice-chat") && text.contains("occupant") && text.contains("admin"),
        "{text}"
    );
    assert!(harness.only_creator_writes() <= 1);
    while let Ok(command) = harness.inbox.try_recv() {
        match command {
            ActorCommand::CreatorAdded(saved) => assert_eq!(saved, original),
            _ => panic!("unexpected notification"),
        }
    }
}

async fn refused(args: Vec<CommandDataOption>, field: &str) {
    let mut harness = Harness::new(configured_creator(true), None, None);
    let before = harness.rows.lock().unwrap().clone();
    let response = harness.send(args).await;
    harness.unchanged(&before);
    let text = response_text(&response).to_lowercase();
    assert!(
        text.encode_utf16().count() <= 500,
        "refusal must be bounded: {text}"
    );
    assert!(
        text.contains(field),
        "identify the invalid/conflicting field {field}: {text}"
    );
    assert!(
        text.contains("/textchannels")
            || text.contains("try again")
            || text.contains("choose")
            || text.contains("use "),
        "give actionable correction guidance: {text}"
    );
}

#[tokio::test]
async fn reset_and_replacement_conflict_is_atomic_in_both_orders() {
    for mut pair in [
        vec![reset("reset-name", true), replacement("Other")],
        vec![reset("reset-viewer-role", true), viewer(EXTRA_ROLE)],
    ] {
        let field = if pair[0].name == "reset-name" {
            "name"
        } else {
            "role"
        };
        refused(pair.clone(), field).await;
        pair.reverse();
        refused(pair, field).await;
    }
}

#[tokio::test]
async fn duplicate_reset_controls_are_refused_even_when_equal_or_false() {
    for field in ["reset-name", "reset-viewer-role"] {
        for (first, second) in [(true, false), (false, true), (true, true), (false, false)] {
            refused(vec![reset(field, first), reset(field, second)], field).await;
        }
    }
}

#[tokio::test]
async fn wrong_typed_reset_controls_are_not_silently_ignored() {
    for field in ["reset-name", "reset-viewer-role"] {
        for value in [
            CommandOptionValue::String("true".to_owned()),
            CommandOptionValue::Integer(1),
            CommandOptionValue::Role(Id::new(EXTRA_ROLE)),
            CommandOptionValue::Channel(Id::new(CREATOR)),
            CommandOptionValue::SubCommand(Vec::new()),
        ] {
            refused(vec![typed_option(field, value)], field).await;
        }
    }
}

#[tokio::test]
async fn duplicate_wrong_typed_and_unknown_old_options_refuse_before_partial_reset() {
    let bad = [
        (vec![replacement("One"), replacement("Two")], "name"),
        (vec![viewer(EXTRA_ROLE), viewer(GUILD)], "role"),
        (
            vec![reset("enabled", true), reset("enabled", false)],
            "enabled",
        ),
        (
            vec![typed_option(
                "channel",
                CommandOptionValue::Channel(Id::new(CREATOR)),
            )],
            "channel",
        ),
        (
            vec![typed_option("name", CommandOptionValue::Boolean(true))],
            "name",
        ),
        (
            vec![typed_option(
                "viewer-role",
                CommandOptionValue::String("42".to_owned()),
            )],
            "role",
        ),
        (
            vec![typed_option("enabled", CommandOptionValue::Integer(1))],
            "enabled",
        ),
        (vec![reset("unknown-setting", true)], "option"),
    ];
    for (mut args, field) in bad {
        args.insert(0, reset("reset-name", true));
        refused(args, field).await;
    }
}

#[tokio::test]
async fn invalid_replacement_does_not_partially_reset_the_other_field() {
    for name in [
        String::new(),
        "   ".to_owned(),
        "x".repeat(MAX_TEXT_CHANNEL_NAME_CHARS + 1),
    ] {
        refused(
            vec![reset("reset-viewer-role", true), replacement(&name)],
            "name",
        )
        .await;
    }
}

#[tokio::test]
async fn control_manage_channels_and_current_guild_creator_fences_remain() {
    for permissions in [
        None,
        Some(Permissions::VIEW_CHANNEL),
        Some(Permissions::MANAGE_GUILD),
    ] {
        let mut harness = Harness::new(configured_creator(true), None, None);
        let before = harness.rows.lock().unwrap().clone();
        let response = harness
            .send_interaction(textchannels_interaction(
                options(vec![reset("reset-name", true)]),
                permissions,
            ))
            .await;
        harness.unchanged(&before);
        assert!(response_text(&response).contains("Manage Channels"));
    }
    for channel_id in [CREATOR + 1, 999_999] {
        let mut harness = Harness::new(configured_creator(true), None, None);
        // A real same-guild channel that is not configured as a creator.
        harness
            .rows
            .lock()
            .unwrap()
            .retain(|row| row.channel_id == CREATOR);
        let before = harness.rows.lock().unwrap().clone();
        let response = harness
            .send_interaction(textchannels_interaction(
                vec![
                    typed_option("channel", CommandOptionValue::Channel(Id::new(channel_id))),
                    reset("reset-name", true),
                ],
                Some(Permissions::MANAGE_CHANNELS),
            ))
            .await;
        harness.unchanged(&before);
        assert!(response_text(&response).contains("/create"));
    }
    // No guild: the existing dispatch contract is to not own the
    // interaction at all (no reply, no write), not a private refusal.
    {
        let mut harness = Harness::new(configured_creator(true), None, None);
        let before = harness.rows.lock().unwrap().clone();
        let mut interaction = textchannels_interaction(
            options(vec![reset("reset-name", true)]),
            Some(Permissions::MANAGE_CHANNELS),
        );
        interaction.guild_id = None;
        let (owned, response) = handle_capture(&harness.runtime, &interaction).await;
        assert!(!owned, "DM interactions must stay unowned");
        assert!(response.is_none());
        harness.unchanged(&before);
    }
    // A different guild has no live authority facts here, so it fails
    // closed with a private refusal and no write.
    {
        let mut harness = Harness::new(configured_creator(true), None, None);
        let before = harness.rows.lock().unwrap().clone();
        let mut interaction = textchannels_interaction(
            options(vec![reset("reset-name", true)]),
            Some(Permissions::MANAGE_CHANNELS),
        );
        interaction.guild_id = Some(Id::new(GUILD + 1));
        harness.send_interaction(interaction).await;
        harness.unchanged(&before);
    }
}

#[tokio::test]
async fn control_read_and_write_failures_do_not_save_or_notify() {
    for (read, write, guidance) in [
        (Some(StoreError::Unavailable), None, "read"),
        (None, Some(StoreError::Unavailable), "save"),
        (None, Some(StoreError::CredentialRefused), "credential"),
    ] {
        let mut harness = Harness::new(configured_creator(true), read, write);
        let before = harness.rows.lock().unwrap().clone();
        let response = harness
            .send(vec![
                reset("reset-name", true),
                reset("reset-viewer-role", true),
            ])
            .await;
        harness.unchanged(&before);
        let text = response_text(&response).to_lowercase();
        assert!(
            text.contains(guidance),
            "honest failure classification: {text}"
        );
        assert!(
            text.contains("try again") || text.contains("admin") || text.contains("restart"),
            "{text}"
        );
        assert!(
            !text.contains("saved"),
            "failure must not claim success: {text}"
        );
    }
}

/// A configured name/viewer role queues more join work than the plain
/// fixture drains. Run the worker to a genuinely idle queue (bounded)
/// so "no pending mutation" measures the reset, not leftover join work.
async fn drain_worker(worker: &mut GuildRoomWorker<Store, Http>, now: &mut u64) {
    for _ in 0..32 {
        if !worker.dispatch_one(*now).await {
            return;
        }
        *now += 1;
    }
    panic!("worker did not reach an idle queue");
}

#[tokio::test]
async fn control_existing_companion_fixture_has_no_pending_live_mutation() {
    let (live, store, http, _) = companion_fixture();
    store.creators.lock().unwrap()[0] = snapshot_creator();
    let mut worker = GuildRoomWorker::load(live, store, http)
        .await
        .expect("load worker");
    let mut now = 0;
    join_with_companion(&mut worker, MEMBER, &mut now).await;
    assert_eq!(
        worker.companions[&500].settings.configured_name.as_deref(),
        Some("Lounge")
    );
    assert_eq!(
        worker.companions[&500].settings.viewer_role_id,
        Some(EXTRA_ROLE)
    );
    drain_worker(&mut worker, &mut now).await;
    assert!(
        !worker.dispatch_one(now).await,
        "snapshot fixture must start idle"
    );
}

#[tokio::test]
async fn saved_reset_notification_preserves_existing_companion_but_changes_future_plan() {
    let original = snapshot_creator();
    let mut harness = Harness::new(original.clone(), None, None);
    harness
        .send(vec![
            reset("reset-name", true),
            reset("reset-viewer-role", true),
        ])
        .await;
    let command = harness
        .inbox
        .try_recv()
        .expect("actual handler notification");
    let saved = match &command {
        ActorCommand::CreatorAdded(creator) => creator.clone(),
        _ => panic!("expected creator update"),
    };
    assert_eq!(saved.text_channel_name, None);
    assert_eq!(saved.text_viewer_role_id, None);
    let (live, store, http, trace) = companion_fixture();
    store.creators.lock().unwrap()[0] = original;
    let mut worker = GuildRoomWorker::load(live, store, http)
        .await
        .expect("load worker");
    let mut now = 0;
    join_with_companion(&mut worker, MEMBER, &mut now).await;
    drain_worker(&mut worker, &mut now).await;
    let existing = worker.companions[&500].clone();
    assert_eq!(existing.settings.configured_name.as_deref(), Some("Lounge"));
    assert_eq!(existing.settings.viewer_role_id, Some(EXTRA_ROLE));
    let existing_rows = worker.store.companions.lock().unwrap().clone();
    let calls = trace.lock().unwrap().clone();
    apply_command(
        &mut worker,
        command,
        now,
        &ActorInboxBackpressure::default(),
    );
    assert!(
        !worker.dispatch_one(now).await,
        "reset must not queue a live Discord mutation"
    );
    assert_eq!(worker.companions[&500], existing);
    assert_eq!(*worker.store.companions.lock().unwrap(), existing_rows);
    assert_eq!(
        *trace.lock().unwrap(),
        calls,
        "save/notification must not edit existing channels or overwrites"
    );
    assert_eq!(worker.creators[&CREATOR], saved);
    let plan = text_channel_plan(
        &worker.creators[&CREATOR].text_channel_settings(),
        &VoiceRoomFacts {
            guild_id: GUILD,
            room_id: 501,
            category_id: CATEGORY,
            occupants: &[MEMBER + 1],
            admin_ids: &[998],
            admin_role_ids: &[900],
        },
    )
    .expect("subsequently planned enabled companion");
    assert_eq!(plan.name, "voice-chat");
    assert_eq!(plan.settings.configured_name, None);
    assert_eq!(plan.settings.viewer_role_id, None);
    assert!(plan
        .overwrites
        .iter()
        .any(|o| o.target == OverwriteTarget::Everyone && o.deny_view && !o.allow_view));
    for target in [
        OverwriteTarget::Member(MEMBER + 1),
        OverwriteTarget::Member(998),
        OverwriteTarget::Role(900),
    ] {
        assert!(plan
            .overwrites
            .iter()
            .any(|o| o.target == target && o.allow_view && !o.deny_view));
    }
    assert!(!plan
        .overwrites
        .iter()
        .any(|o| o.target == OverwriteTarget::Role(EXTRA_ROLE)));
    assert!(!plan.visible_to_all());
}
