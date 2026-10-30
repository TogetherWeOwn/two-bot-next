#![cfg(test)]

//! Protected acceptance: stale controls answer exactly one expired-control reply.
//!
//! Every test drives the production `CommandRuntime::on_interaction` dispatch
//! path against a loopback mock; nothing here touches a database, the network,
//! or live Discord. The RED tests fail on the current baseline (the runtime
//! silently drops every unknown component/modal and every rejected self-role
//! reference) and pass once the paired build connects the replies. The GREEN
//! tests pass before and after: they pin the silence, ownership, and
//! no-regression boundaries the build must preserve.
//!
//! Ownership / fallthrough contract (the paired build promotes this list to the
//! interaction-reply docs; it is the normative reference until then):
//!
//! - Genuinely unowned, in-scope controls — an unknown button, select menu, or
//!   modal submit in the configured guild and application — get exactly one
//!   ephemeral type-4 reply carrying `EXPIRED_COMPONENT_REPLY`, and no other
//!   REST or store effect.
//! - A `two:self-role:` reference that parses but names a removed panel, a
//!   replaced source message, or a removed option is stale, not foreign: same
//!   single expired reply, zero role or store mutation.
//! - Foreign applications and guilds, bot authors, and malformed payloads stay
//!   silent: zero requests.
//! - A malformed `two:self-role:` id or shape (unparseable id, values on a
//!   button, select id on a button) is malformed, not stale: silence.
//! - A well-formed `two:voice:` control (button or modal) is owned by the voice
//!   sink: this runtime stays silent and never double-acknowledges it.
//! - A `two:voice:`-prefixed id that fails `parse_voice_custom_id` is unowned:
//!   a prefix alone never claims ownership, so it gets the expired reply.
//! - Router-claimed controls on live surfaces (onboarding pickers, tickets) and
//!   any control on a disabled surface stay silent: owned-or-disabled controls
//!   are never double-acknowledged.
//! - Slash behavior is untouched: unknown names keep `UNKNOWN_COMMAND_REPLY`
//!   and live commands keep answering.
//!
//! Protected file: the paired build must not edit this module. Corrections go
//! back to its author. Valid self-role delivery (defer plus role mutation)
//! stays covered by the existing self-role suites, which need a database; this
//! module pins the offline dispatch boundary only.

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use twilight_model::application::interaction::message_component::MessageComponentInteractionData;
use twilight_model::application::interaction::modal::ModalInteractionData;
use twilight_model::application::interaction::{Interaction, InteractionData, InteractionType};
use twilight_model::channel::message::component::ComponentType;
use twilight_model::id::Id;
use two_bot_core::lfg::lfg_custom_id;
use two_bot_core::onboarding::{GAME_SELECT_ID, SESSION_SELECT_ID};
use two_bot_core::router::replies::{EXPIRED_COMPONENT_REPLY, UNKNOWN_COMMAND_REPLY};
use two_bot_core::self_roles::{
    parse_self_role_custom_id, self_role_custom_id, PanelMode, SelfRoleGates, SelfRoleOption,
    SelfRolePanel,
};
use two_bot_core::voice_custom_id::{join_custom_id, name_modal_custom_id, parse_voice_custom_id};
use two_bot_core::voice_private::JoinDecision;
use two_bot_core::{RouterGates, TICKET_OPEN_ID};
use two_bot_cutover::self_role_store::SelfRoleStore;
use two_bot_discord::ActionExecutor;

use crate::command_runtime::{router_with_commands, CommandRuntime};
use crate::command_runtime_tests::{message, slash};
use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};
use crate::self_role_handlers::SelfRoleService;
use crate::self_role_runtime::SelfRoleRuntime;

/// Guild and application the shared `slash` fixture speaks for.
const GUILD: u64 = 2222;
const APP_ID: u64 = 1111;

/// Self-role fixtures use realistic 17-20 digit snowflakes: service boot and
/// event ordering reject anything shorter.
const SR_GUILD: u64 = 100_000_000_000_000_001;
const SR_CHANNEL: u64 = 100_000_000_000_000_007;
const SR_MESSAGE: u64 = 100_000_000_000_000_008;
const SR_USER: u64 = 100_000_000_000_000_002;
const SR_BOT: u64 = 100_000_000_000_000_003;
const SR_EVENT: u64 = 100_000_000_000_000_011;

fn base_gates() -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD),
        scorecard: false,
        automations: true,
        announcements: true,
        moderation: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    }
}

fn sr_gates() -> RouterGates {
    RouterGates {
        configured_guild: Some(SR_GUILD),
        self_roles: true,
        ..base_gates()
    }
}

fn lazy_pool() -> PgPool {
    PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("lazy pool")
}

fn executor(origin: String) -> ActionExecutor {
    ActionExecutor::with_proxy("test-token".to_owned(), Some(origin)).expect("mock executor")
}

fn runtime(gates: RouterGates, origin: String) -> Arc<CommandRuntime> {
    let automations = gates.automations;
    CommandRuntime::new(
        lazy_pool(),
        executor(origin),
        router_with_commands(gates),
        gates.configured_guild.expect("configured guild"),
        automations,
    )
}

fn panel(id: &str, mode: PanelMode) -> SelfRolePanel {
    SelfRolePanel {
        id: id.to_owned(),
        channel_id: SR_CHANNEL.to_string(),
        message_id: SR_MESSAGE.to_string(),
        mode,
        exclusive: false,
        color: false,
        options: vec![SelfRoleOption {
            key: "new".to_owned(),
            label: "New".to_owned(),
            role_id: "100000000000000004".to_owned(),
            permissions: "0".to_owned(),
            emoji: None,
            description: None,
        }],
    }
}

/// Self-role service over the mock origin: any accidental role REST shows up in
/// the recorded requests and fails the zero-effect assertions.
fn service(origin: String) -> Arc<SelfRoleService> {
    let runtime = SelfRoleRuntime {
        store: SelfRoleStore::new(lazy_pool()),
        executor: executor(origin),
        guild_id: SR_GUILD.to_string(),
        bot_id: SR_BOT.to_string(),
    };
    let gates = SelfRoleGates {
        dry_run: false,
        panels: vec![
            panel("games", PanelMode::Button),
            panel("picks", PanelMode::Select),
        ],
    };
    let allowlist: HashSet<String> = [SR_GUILD.to_string()].into_iter().collect();
    Arc::new(SelfRoleService::new(runtime, gates, &allowlist).expect("self-role service"))
}

fn roles_runtime(gates: RouterGates, origin: String) -> Arc<CommandRuntime> {
    let automations = gates.automations;
    CommandRuntime::new_with_self_roles(
        lazy_pool(),
        executor(origin.clone()),
        router_with_commands(gates),
        SR_GUILD,
        automations,
        service(origin),
    )
}

/// Message-component interaction on the self-role source message.
fn sr_button(
    event_id: u64,
    custom_id: &str,
    component_type: ComponentType,
    values: Vec<String>,
) -> Interaction {
    let mut interaction = slash("unused", Some(SR_CHANNEL), Vec::new());
    interaction.guild_id = Some(Id::new(SR_GUILD));
    interaction.id = Id::new(event_id);
    interaction.kind = InteractionType::MessageComponent;
    interaction
        .member
        .as_mut()
        .expect("guild member")
        .user
        .as_mut()
        .expect("member user")
        .id = Id::new(SR_USER);
    let mut source = message(1, SR_CHANNEL, true, Some(SR_GUILD));
    source.id = Id::new(SR_MESSAGE);
    interaction.message = Some(source);
    interaction.data = Some(InteractionData::MessageComponent(Box::new(
        MessageComponentInteractionData {
            custom_id: custom_id.to_owned(),
            component_type,
            resolved: None,
            values,
        },
    )));
    interaction
}

fn unknown_component(
    custom_id: &str,
    component_type: ComponentType,
    values: Vec<String>,
) -> Interaction {
    let mut interaction = slash("unused", Some(3333), Vec::new());
    interaction.kind = InteractionType::MessageComponent;
    interaction.data = Some(InteractionData::MessageComponent(Box::new(
        MessageComponentInteractionData {
            custom_id: custom_id.to_owned(),
            component_type,
            resolved: None,
            values,
        },
    )));
    interaction
}

fn unknown_modal(custom_id: &str) -> Interaction {
    let mut interaction = slash("unused", Some(3333), Vec::new());
    interaction.kind = InteractionType::ModalSubmit;
    interaction.data = Some(InteractionData::ModalSubmit(Box::new(
        ModalInteractionData {
            components: Vec::new(),
            custom_id: custom_id.to_owned(),
            resolved: None,
        },
    )));
    interaction
}

async fn new_mock() -> MockRest {
    MockRest::start(Vec::new(), ScriptedResponse::json(200, json!({"id": "99"}))).await
}

/// Exactly one ephemeral type-4 callback with the expected copy, and no other
/// REST effect: one reply owner, no double acknowledgement, no mutation.
fn assert_single_callback(requests: &[RestRequest], expected: &str) {
    assert_eq!(requests.len(), 1, "one callback, zero other effects");
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    let path = request.path.as_str();
    assert!(path.ends_with("/callback"), "callback path: {path}");
    let reply: Value = serde_json::from_slice(&request.body).expect("reply json");
    assert_eq!(reply["type"], 4, "immediate response, not a defer");
    assert_eq!(reply["data"]["flags"], 64, "ephemeral");
    assert_eq!(reply["data"]["content"], expected);
    assert_eq!(
        reply["data"]["allowed_mentions"]["parse"],
        json!([]),
        "mention suppression"
    );
}

fn assert_expired_reply(requests: &[RestRequest]) {
    assert_single_callback(requests, EXPIRED_COMPONENT_REPLY);
}

// ---------------------------------------------------------------------------
// RED: stale controls get the expired reply (fail until the build lands)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_button_gets_one_expired_reply() {
    let mock = new_mock().await;
    let runtime = runtime(base_gates(), mock.origin());
    let interaction = unknown_component(
        "two:stale-widget:deadbeef",
        ComponentType::Button,
        Vec::new(),
    );
    runtime.on_interaction(&interaction).await;
    assert_expired_reply(&mock.requests());
    mock.shutdown().await;
}

#[tokio::test]
async fn unknown_menu_gets_one_expired_reply() {
    let mock = new_mock().await;
    let runtime = runtime(base_gates(), mock.origin());
    let interaction = unknown_component(
        "two:old-menu:xyz",
        ComponentType::TextSelectMenu,
        vec!["stale-value".to_owned()],
    );
    runtime.on_interaction(&interaction).await;
    assert_expired_reply(&mock.requests());
    mock.shutdown().await;
}

#[tokio::test]
async fn unknown_modal_gets_one_expired_reply() {
    let mock = new_mock().await;
    let runtime = runtime(base_gates(), mock.origin());
    runtime
        .on_interaction(&unknown_modal("two:old-modal:xyz"))
        .await;
    assert_expired_reply(&mock.requests());
    mock.shutdown().await;
}

#[tokio::test]
async fn removed_panel_gets_expired_reply_without_mutation() {
    let custom_id = "two:self-role:retired:new";
    assert!(
        parse_self_role_custom_id(custom_id).is_some(),
        "well-formed stale reference"
    );
    let mock = new_mock().await;
    let runtime = roles_runtime(sr_gates(), mock.origin());
    let interaction = sr_button(SR_EVENT, custom_id, ComponentType::Button, Vec::new());
    runtime.on_interaction(&interaction).await;
    assert_expired_reply(&mock.requests());
    mock.shutdown().await;
}

#[tokio::test]
async fn replaced_source_message_gets_expired_reply_without_mutation() {
    let custom_id = self_role_custom_id("games", Some("new"));
    let mock = new_mock().await;
    let runtime = roles_runtime(sr_gates(), mock.origin());
    let mut interaction = sr_button(SR_EVENT + 1, &custom_id, ComponentType::Button, Vec::new());
    interaction.message.as_mut().expect("source message").id = Id::new(SR_MESSAGE + 1);
    runtime.on_interaction(&interaction).await;
    assert_expired_reply(&mock.requests());
    mock.shutdown().await;
}

#[tokio::test]
async fn removed_button_option_gets_expired_reply_without_mutation() {
    let custom_id = "two:self-role:games:retired";
    assert!(
        parse_self_role_custom_id(custom_id).is_some(),
        "well-formed stale reference"
    );
    let mock = new_mock().await;
    let runtime = roles_runtime(sr_gates(), mock.origin());
    let interaction = sr_button(SR_EVENT + 2, custom_id, ComponentType::Button, Vec::new());
    runtime.on_interaction(&interaction).await;
    assert_expired_reply(&mock.requests());
    mock.shutdown().await;
}

#[tokio::test]
async fn removed_select_values_get_expired_reply_without_mutation() {
    let custom_id = self_role_custom_id("picks", None);
    assert!(
        parse_self_role_custom_id(&custom_id).is_some(),
        "well-formed stale reference"
    );
    let mock = new_mock().await;
    let runtime = roles_runtime(sr_gates(), mock.origin());
    let interaction = sr_button(
        SR_EVENT + 3,
        &custom_id,
        ComponentType::TextSelectMenu,
        vec!["retired".to_owned()],
    );
    runtime.on_interaction(&interaction).await;
    assert_expired_reply(&mock.requests());
    mock.shutdown().await;
}

#[tokio::test]
async fn voice_prefix_alone_does_not_claim_ownership() {
    let custom_id = "two:voice:exploded";
    assert!(
        parse_voice_custom_id(custom_id).is_none(),
        "prefix without a voice action is unowned"
    );
    let mock = new_mock().await;
    let runtime = runtime(base_gates(), mock.origin());
    let interaction = unknown_component(custom_id, ComponentType::Button, Vec::new());
    runtime.on_interaction(&interaction).await;
    assert_expired_reply(&mock.requests());
    mock.shutdown().await;
}

#[tokio::test]
async fn armed_matching_application_gets_expired_reply() {
    let mock = new_mock().await;
    let runtime = runtime(base_gates(), mock.origin());
    runtime.set_identity(APP_ID, APP_ID);
    let interaction = unknown_component(
        "two:stale-widget:deadbeef",
        ComponentType::Button,
        Vec::new(),
    );
    assert_eq!(
        interaction.application_id.get(),
        APP_ID,
        "fixture speaks for the configured application"
    );
    runtime.on_interaction(&interaction).await;
    assert_expired_reply(&mock.requests());
    mock.shutdown().await;
}

// ---------------------------------------------------------------------------
// GREEN: silence, ownership, and no-regression pins (pass before and after)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn foreign_application_stays_silent() {
    let mock = new_mock().await;
    let runtime = runtime(base_gates(), mock.origin());
    runtime.set_identity(APP_ID, APP_ID);
    let mut button = unknown_component(
        "two:stale-widget:deadbeef",
        ComponentType::Button,
        Vec::new(),
    );
    button.application_id = Id::new(9999);
    runtime.on_interaction(&button).await;
    let mut modal = unknown_modal("two:old-modal:xyz");
    modal.application_id = Id::new(9999);
    runtime.on_interaction(&modal).await;
    assert!(
        mock.requests().is_empty(),
        "foreign application stays silent"
    );
    mock.shutdown().await;

    let mock_two = new_mock().await;
    let runtime = roles_runtime(sr_gates(), mock_two.origin());
    runtime.set_identity(APP_ID, APP_ID);
    let mut stale = sr_button(
        SR_EVENT + 4,
        "two:self-role:retired:new",
        ComponentType::Button,
        Vec::new(),
    );
    stale.application_id = Id::new(9999);
    runtime.on_interaction(&stale).await;
    assert!(
        mock_two.requests().is_empty(),
        "stale reference under a foreign application stays silent"
    );
    mock_two.shutdown().await;
}

#[tokio::test]
async fn foreign_and_missing_guild_stay_silent() {
    let mock = new_mock().await;
    let runtime = runtime(base_gates(), mock.origin());
    for guild in [Some(Id::new(9999)), None] {
        let mut button = unknown_component(
            "two:stale-widget:deadbeef",
            ComponentType::Button,
            Vec::new(),
        );
        button.guild_id = guild;
        runtime.on_interaction(&button).await;
        let mut modal = unknown_modal("two:old-modal:xyz");
        modal.guild_id = guild;
        runtime.on_interaction(&modal).await;
    }
    assert!(
        mock.requests().is_empty(),
        "foreign and missing guilds stay silent"
    );
    mock.shutdown().await;

    let mock_two = new_mock().await;
    let runtime = roles_runtime(sr_gates(), mock_two.origin());
    for (index, guild) in [Some(Id::new(9999)), None].into_iter().enumerate() {
        let mut stale = sr_button(
            SR_EVENT + 10 + index as u64,
            "two:self-role:retired:new",
            ComponentType::Button,
            Vec::new(),
        );
        stale.guild_id = guild;
        runtime.on_interaction(&stale).await;
    }
    assert!(
        mock_two.requests().is_empty(),
        "stale reference outside the configured guild stays silent"
    );
    mock_two.shutdown().await;
}

#[tokio::test]
async fn malformed_self_role_reference_stays_silent() {
    let live_button = self_role_custom_id("games", Some("new"));
    let live_select = self_role_custom_id("games", None);
    let cases: [(&str, ComponentType, Vec<String>); 5] = [
        ("two:self-role:", ComponentType::Button, Vec::new()),
        ("two:self-role:Games", ComponentType::Button, Vec::new()),
        (
            "two:self-role:games:new:extra",
            ComponentType::Button,
            Vec::new(),
        ),
        // Values on a button, and a select id on a button: well-formed ids in
        // shapes the panel never serves.
        (
            live_button.as_str(),
            ComponentType::Button,
            vec!["new".to_owned()],
        ),
        (live_select.as_str(), ComponentType::Button, Vec::new()),
    ];
    for (custom_id, _, _) in &cases[..3] {
        assert!(
            parse_self_role_custom_id(custom_id).is_none(),
            "malformed id parses to nothing: {custom_id}"
        );
    }
    let mock = new_mock().await;
    let runtime = roles_runtime(sr_gates(), mock.origin());
    for (index, (custom_id, component_type, values)) in cases.into_iter().enumerate() {
        let interaction = sr_button(
            SR_EVENT + 20 + index as u64,
            custom_id,
            component_type,
            values,
        );
        runtime.on_interaction(&interaction).await;
    }
    assert!(
        mock.requests().is_empty(),
        "malformed input is not stale: silence"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn bot_member_stays_silent() {
    let mock = new_mock().await;
    let runtime = runtime(base_gates(), mock.origin());
    let mut button = unknown_component(
        "two:stale-widget:deadbeef",
        ComponentType::Button,
        Vec::new(),
    );
    button
        .member
        .as_mut()
        .expect("guild member")
        .user
        .as_mut()
        .expect("member user")
        .bot = true;
    button.user.as_mut().expect("top-level user").bot = true;
    runtime.on_interaction(&button).await;
    assert!(
        mock.requests().is_empty(),
        "bot-authored unknown control stays silent"
    );
    mock.shutdown().await;

    let mock_two = new_mock().await;
    let runtime = roles_runtime(sr_gates(), mock_two.origin());
    let mut stale = sr_button(
        SR_EVENT + 5,
        "two:self-role:retired:new",
        ComponentType::Button,
        Vec::new(),
    );
    stale
        .member
        .as_mut()
        .expect("guild member")
        .user
        .as_mut()
        .expect("member user")
        .bot = true;
    runtime.on_interaction(&stale).await;
    assert!(
        mock_two.requests().is_empty(),
        "bot-authored stale reference stays silent"
    );
    mock_two.shutdown().await;
}

#[tokio::test]
async fn disabled_self_role_surface_stays_silent() {
    let gates = RouterGates {
        self_roles: false,
        ..sr_gates()
    };
    let custom_id = self_role_custom_id("games", Some("new"));
    let mock = new_mock().await;
    let origin = mock.origin();
    let service = service(origin.clone());
    let automations = gates.automations;
    let runtime = CommandRuntime::new_with_self_roles(
        lazy_pool(),
        executor(origin),
        router_with_commands(gates),
        SR_GUILD,
        automations,
        Arc::clone(&service),
    );
    let interaction = sr_button(SR_EVENT + 6, &custom_id, ComponentType::Button, Vec::new());
    assert!(
        service.component_input(&interaction).is_some(),
        "fixture is the live control; only the surface gate is off"
    );
    runtime.on_interaction(&interaction).await;
    assert!(
        mock.requests().is_empty(),
        "disabled surface stays silent even with the service attached"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn voice_owned_controls_stay_silent() {
    let room = 777_000_111_222_333_444u64;
    let request = 888_000_111_222_333_444u64;
    let button_id = join_custom_id(JoinDecision::Approve, room, request);
    let modal_id = name_modal_custom_id(room);
    assert!(
        parse_voice_custom_id(&button_id).is_some(),
        "voice button fixture parses as voice-owned"
    );
    assert!(
        parse_voice_custom_id(&modal_id).is_some(),
        "voice modal fixture parses as voice-owned"
    );
    let mock = new_mock().await;
    let runtime = runtime(base_gates(), mock.origin());
    let button = unknown_component(&button_id, ComponentType::Button, Vec::new());
    runtime.on_interaction(&button).await;
    runtime.on_interaction(&unknown_modal(&modal_id)).await;
    assert!(
        mock.requests().is_empty(),
        "voice-owned controls stay silent: no double acknowledgement"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn onboarding_picker_controls_stay_silent() {
    let gates = RouterGates {
        onboarding_picker: true,
        session_picker: true,
        ..base_gates()
    };
    let mock = new_mock().await;
    let runtime = runtime(gates, mock.origin());
    let game = unknown_component(GAME_SELECT_ID, ComponentType::Button, Vec::new());
    runtime.on_interaction(&game).await;
    let session = unknown_component(
        SESSION_SELECT_ID,
        ComponentType::TextSelectMenu,
        vec!["mage".to_owned()],
    );
    runtime.on_interaction(&session).await;
    assert!(
        mock.requests().is_empty(),
        "onboarding-owned controls stay silent"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn ticket_controls_without_service_stay_silent() {
    let gates = RouterGates {
        tickets: true,
        ..base_gates()
    };
    let mock = new_mock().await;
    let runtime = runtime(gates, mock.origin());
    let ticket = unknown_component(TICKET_OPEN_ID, ComponentType::Button, Vec::new());
    runtime.on_interaction(&ticket).await;
    assert!(
        mock.requests().is_empty(),
        "recognized ticket route without a service stays silent"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn disabled_surfaces_stay_silent() {
    let gates = RouterGates {
        configured_guild: Some(GUILD),
        scorecard: false,
        automations: false,
        announcements: false,
        moderation: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    };
    let mock = new_mock().await;
    let runtime = runtime(gates, mock.origin());
    let game = unknown_component(GAME_SELECT_ID, ComponentType::Button, Vec::new());
    runtime.on_interaction(&game).await;
    let session = unknown_component(
        SESSION_SELECT_ID,
        ComponentType::TextSelectMenu,
        vec!["mage".to_owned()],
    );
    runtime.on_interaction(&session).await;
    let signup = unknown_component(
        &lfg_custom_id("post"),
        ComponentType::TextSelectMenu,
        Vec::new(),
    );
    runtime.on_interaction(&signup).await;
    assert!(
        mock.requests().is_empty(),
        "controls on disabled surfaces stay silent"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn invalid_component_payload_stays_silent() {
    let mock = new_mock().await;
    let runtime = runtime(base_gates(), mock.origin());
    let mut dataless = slash("unused", Some(3333), Vec::new());
    dataless.kind = InteractionType::MessageComponent;
    dataless.data = None;
    runtime.on_interaction(&dataless).await;
    let mut ping = slash("unused", Some(3333), Vec::new());
    ping.kind = InteractionType::Ping;
    ping.data = None;
    runtime.on_interaction(&ping).await;
    assert!(
        mock.requests().is_empty(),
        "unroutable payloads stay silent"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn unknown_slash_keeps_unknown_command_copy() {
    let mock = new_mock().await;
    let runtime = runtime(base_gates(), mock.origin());
    runtime
        .on_interaction(&slash("not-a-command", Some(3333), Vec::new()))
        .await;
    assert_single_callback(&mock.requests(), UNKNOWN_COMMAND_REPLY);
    mock.shutdown().await;
}

#[tokio::test]
async fn help_stays_functional() {
    let mock = new_mock().await;
    let runtime = runtime(base_gates(), mock.origin());
    runtime
        .on_interaction(&slash("help", Some(3333), Vec::new()))
        .await;
    assert_single_callback(
        &mock.requests(),
        "The command list is still refreshing. Try /help again shortly.",
    );
    mock.shutdown().await;
}
