//! Protected acceptance: published settings must reach real self-role inputs
//! and CommandRuntime::on_interaction, including stale-control classification.
//! No helper-only catalogue calls, environment writes, or database connections.
//!
//! Builder seam: per-instance `set_live_settings(&self, LiveSettings)` on
//! SelfRoleService and CommandRuntime. The baseline adapters do nothing; the
//! real boot-only entry points therefore compile and fail behavioral assertions.
//! Inherent methods supersede these adapters without changing this corpus.
//! Mutants: use boot panels in component_input/reaction_input/stale_component,
//! or resolve admission and stale classification from different catalogues.

#![cfg(test)]

use super::*;
use serde_json::{json, Value};
use two_bot_core::self_roles::{
    parse_self_role_panels, self_role_custom_id, PanelMode, SelfRoleGates, SelfRolePanel,
};
use two_bot_core::settings::{LiveSettings, LiveSettingsWriter};
use two_bot_cutover::self_role_store::SelfRoleStore;

use crate::self_role_handlers::SelfRoleService;
use crate::self_role_runtime::{Selection, SelfRoleRuntime};

const SR_GUILD: u64 = 100_000_000_000_000_001;
const SR_BOT: u64 = 100_000_000_000_000_003;
const SR_CHANNEL: u64 = 100_000_000_000_000_007;
const SR_MESSAGE: u64 = 100_000_000_000_000_008;
const SR_MOVED: u64 = 100_000_000_000_000_018;
const SR_ROLE: &str = "100000000000000004";
const SR_LIVE_ROLE: &str = "100000000000000014";
const PANELS: &str = "TWO_SELF_ROLE_PANELS";

trait BaselineLiveSettings {
    fn set_live_settings(&self, live: LiveSettings);
}

impl BaselineLiveSettings for SelfRoleService {
    fn set_live_settings(&self, _live: LiveSettings) {}
}

impl BaselineLiveSettings for CommandRuntime {
    fn set_live_settings(&self, _live: LiveSettings) {}
}

fn catalogue(mode: PanelMode, message: u64, key: &str, role: &str) -> Value {
    json!([{
        "id": "games", "channelId": SR_CHANNEL.to_string(),
        "messageId": message.to_string(),
        "mode": match mode {
            PanelMode::Button => "button",
            PanelMode::Select => "select",
            PanelMode::Reaction => "reaction",
        },
        "exclusive": false,
        "options": [{
            "key": key, "label": "Chess", "roleId": role,
            "permissions": "0", "emoji": "b"
        }]
    }])
}

fn panel(value: &Value) -> SelfRolePanel {
    parse_self_role_panels(&value.to_string())
        .expect("handwritten valid catalogue")
        .remove(0)
}

fn publish(writer: &mut LiveSettingsWriter, revision: i64, guild: u64, value: Option<Value>) {
    writer.publish(&SettingsSnapshot {
        revision,
        rows: value
            .map(|value| SettingRow {
                guild_id: guild.to_string(),
                key: PANELS.into(),
                value,
                version: revision,
            })
            .into_iter()
            .collect(),
    });
}

async fn fixture(
    mode: PanelMode,
) -> (
    Arc<CommandRuntime>,
    Arc<SelfRoleService>,
    MockRest,
    SelfRolePanel,
) {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let options = PgConnectOptions::new()
        .host("agent-testdb")
        .port(5432)
        .username("agent_test")
        .password("")
        .database("two_bot_test_live_settings_acceptance");
    let pool = PgPoolOptions::new().connect_lazy_with(options);
    // A mistaken admission cannot connect anywhere or mutate the database.
    // It instead produces the ordinary deferred error reply, which fails the
    // exact one-immediate-expired-callback assertion below.
    pool.close().await;
    let executor = ActionExecutor::with_proxy("fixture-token".into(), Some(origin))
        .expect("loopback mock executor");
    let boot = panel(&catalogue(mode, SR_MESSAGE, "new", SR_ROLE));
    let service = Arc::new(
        SelfRoleService::new(
            SelfRoleRuntime {
                store: SelfRoleStore::new(pool.clone()),
                executor: executor.clone(),
                guild_id: SR_GUILD.to_string(),
                bot_id: SR_BOT.to_string(),
            },
            SelfRoleGates {
                panels: vec![boot.clone()],
                dry_run: false,
            },
            &[SR_GUILD.to_string()].into_iter().collect(),
        )
        .expect("valid boot panel"),
    );
    let mut router_gates = gates(false, false);
    router_gates.configured_guild = Some(SR_GUILD);
    router_gates.self_roles = true;
    let runtime = CommandRuntime::new_with_self_roles(
        pool,
        executor,
        router_with_commands(router_gates),
        SR_GUILD,
        false,
        service.clone(),
    );
    (runtime, service, mock, boot)
}

fn press(source: &SelfRolePanel, option: &str) -> Interaction {
    let mut interaction =
        crate::self_role_handlers::tests::component(source, 100_000_000_000_000_020, Vec::new());
    let Some(InteractionData::MessageComponent(data)) = &mut interaction.data else {
        unreachable!("component fixture")
    };
    data.custom_id = self_role_custom_id(&source.id, Some(option));
    interaction
}

fn reaction_on(source: &SelfRolePanel) -> twilight_model::gateway::GatewayReaction {
    let mut reaction = crate::self_role_handlers::tests::reaction();
    reaction.channel_id = Id::new(source.channel_id.parse().expect("channel id"));
    reaction.message_id = Id::new(source.message_id.parse().expect("message id"));
    reaction
}

#[tokio::test]
async fn component_entry_observes_off_moved_foreign_and_deleted_snapshots() {
    let (_, service, mock, boot) = fixture(PanelMode::Button).await;
    let (mut writer, live) = live_channel();
    service.set_live_settings(live);
    let old = press(&boot, "new");
    assert!(
        service.component_input(&old).is_some(),
        "pre-first-poll boot fallback"
    );
    publish(&mut writer, 1, SR_GUILD, Some(json!([])));
    assert!(
        service.component_input(&old).is_none(),
        "published off must reach component_input"
    );

    let moved = catalogue(PanelMode::Button, SR_MOVED, "new", SR_LIVE_ROLE);
    publish(&mut writer, 2, SR_GUILD, Some(moved.clone()));
    assert!(
        service.component_input(&old).is_none(),
        "old source is retired"
    );
    let admitted = service
        .component_input(&press(&panel(&moved), "new"))
        .expect("published new source is admitted");
    assert_eq!(admitted.panel.message_id, "100000000000000018");
    assert_eq!(admitted.panel.options[0].role_id, SR_LIVE_ROLE);
    assert!(
        matches!(admitted.request.selection, Selection::Button { ref option_key } if option_key == "new")
    );

    publish(&mut writer, 3, SR_GUILD + 1, Some(json!([])));
    assert!(
        service.component_input(&old).is_some(),
        "foreign rows cannot disable this guild"
    );
    publish(&mut writer, 4, SR_GUILD, Some(json!([])));
    assert!(service.component_input(&old).is_none());
    publish(&mut writer, 5, SR_GUILD, None);
    let restored = service
        .component_input(&old)
        .expect("delete restores deployment");
    assert_eq!(restored.panel.options[0].role_id, SR_ROLE);
    assert!(
        mock.requests().is_empty(),
        "input admission performs no REST"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn reaction_entry_observes_off_moved_foreign_and_deleted_snapshots() {
    let (_, service, mock, boot) = fixture(PanelMode::Reaction).await;
    let (mut writer, live) = live_channel();
    service.set_live_settings(live);
    let old = reaction_on(&boot);
    assert!(service.reaction_input(&old, false).is_some());
    publish(&mut writer, 1, SR_GUILD, Some(json!([])));
    assert!(
        service.reaction_input(&old, false).is_none(),
        "published off must reach reaction_input"
    );
    assert!(
        service.reaction_input(&old, true).is_none(),
        "removal uses the same live source"
    );

    let moved = catalogue(PanelMode::Reaction, SR_MOVED, "new", SR_LIVE_ROLE);
    publish(&mut writer, 2, SR_GUILD, Some(moved.clone()));
    assert!(service.reaction_input(&old, false).is_none());
    for remove in [false, true] {
        let input = service
            .reaction_input(&reaction_on(&panel(&moved)), remove)
            .expect("new reaction source");
        assert_eq!(input.panel.message_id, "100000000000000018");
        assert_eq!(input.panel.options[0].role_id, SR_LIVE_ROLE);
        assert!(
            matches!(input.request.selection, Selection::Reaction { ref option_key, remove: got } if option_key == "new" && got == remove)
        );
    }
    publish(&mut writer, 3, SR_GUILD + 1, Some(json!([])));
    assert!(service.reaction_input(&old, false).is_some());
    publish(&mut writer, 4, SR_GUILD, Some(json!([])));
    assert!(service.reaction_input(&old, false).is_none());
    publish(&mut writer, 5, SR_GUILD, None);
    assert!(
        service.reaction_input(&old, true).is_some(),
        "delete restores boot"
    );
    let mut bot = old.clone();
    bot.user_id = Id::new(SR_BOT);
    assert!(service.reaction_input(&bot, false).is_none());
    let mut foreign = old;
    foreign.guild_id = Some(Id::new(SR_GUILD + 1));
    assert!(service.reaction_input(&foreign, false).is_none());
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
}

async fn assert_live_stale_reply(value: Value) {
    let (runtime, service, mock, boot) = fixture(PanelMode::Button).await;
    let (mut writer, live) = live_channel();
    runtime.set_live_settings(live.clone());
    service.set_live_settings(live);
    publish(&mut writer, 1, SR_GUILD, Some(value));
    runtime.on_interaction(&press(&boot, "new")).await;
    assert_one_expired_reply(&mock.requests());
    let requests = mock.requests();
    let body: Value = serde_json::from_slice(&requests[0].body).expect("callback JSON");
    assert_eq!(body["data"]["allowed_mentions"]["parse"], json!([]));
    // requests.len()==1 above also forbids every role mutation, message edit,
    // member/role fetch and completion callback at the remote boundary.
    mock.shutdown().await;
}

#[tokio::test]
async fn moved_live_message_gets_one_expired_callback_through_command_runtime() {
    assert_live_stale_reply(catalogue(PanelMode::Button, SR_MOVED, "new", SR_ROLE)).await;
}

#[tokio::test]
async fn removed_live_option_gets_one_expired_callback_through_command_runtime() {
    assert_live_stale_reply(catalogue(
        PanelMode::Button,
        SR_MESSAGE,
        "other",
        SR_LIVE_ROLE,
    ))
    .await;
}

#[tokio::test]
async fn removed_live_panel_gets_one_expired_callback_through_command_runtime() {
    assert_live_stale_reply(json!([])).await;
}

#[tokio::test]
async fn malformed_foreign_and_bot_controls_stay_silent_after_live_option_removal() {
    let (runtime, service, mock, boot) = fixture(PanelMode::Button).await;
    let (mut writer, live) = live_channel();
    runtime.set_live_settings(live.clone());
    service.set_live_settings(live);
    publish(
        &mut writer,
        1,
        SR_GUILD,
        Some(catalogue(
            PanelMode::Button,
            SR_MESSAGE,
            "other",
            SR_LIVE_ROLE,
        )),
    );
    for fault in [
        "foreign",
        "bot",
        "missing-member",
        "missing-source",
        "values",
        "surface",
    ] {
        let mut bad = press(&boot, "new");
        match fault {
            "foreign" => bad.guild_id = Some(Id::new(SR_GUILD + 1)),
            "bot" => {
                bad.member
                    .as_mut()
                    .expect("member")
                    .user
                    .as_mut()
                    .expect("user")
                    .bot = true
            }
            "missing-member" => bad.member = None,
            "missing-source" => bad.message = None,
            other => {
                let Some(InteractionData::MessageComponent(data)) = &mut bad.data else {
                    unreachable!()
                };
                if other == "values" {
                    data.values = vec!["new".into()];
                } else {
                    data.component_type = ComponentType::TextSelectMenu;
                }
            }
        }
        runtime.on_interaction(&bad).await;
        assert!(
            mock.requests().is_empty(),
            "{fault}: no callback or mutation"
        );
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn independent_services_do_not_share_a_process_global_catalogue() {
    let (_, disabled, first_mock, boot) = fixture(PanelMode::Button).await;
    let (_, enabled, second_mock, _) = fixture(PanelMode::Button).await;
    let (mut off_writer, off) = live_channel();
    let (_boot_writer, boot_live) = live_channel();
    disabled.set_live_settings(off);
    enabled.set_live_settings(boot_live);
    publish(&mut off_writer, 1, SR_GUILD, Some(json!([])));
    assert!(
        disabled.component_input(&press(&boot, "new")).is_none(),
        "one instance live-off"
    );
    assert!(
        enabled.component_input(&press(&boot, "new")).is_some(),
        "peer retains boot fallback"
    );
    assert!(first_mock.requests().is_empty());
    assert!(second_mock.requests().is_empty());
    first_mock.shutdown().await;
    second_mock.shutdown().await;
}

#[tokio::test]
async fn explicit_baseline_adapters_keep_boot_entry_points_callable() {
    let (runtime, service, mock, boot) = fixture(PanelMode::Button).await;
    let (_writer, live) = live_channel();
    <SelfRoleService as BaselineLiveSettings>::set_live_settings(&service, live.clone());
    <CommandRuntime as BaselineLiveSettings>::set_live_settings(&runtime, live);
    assert!(service.component_input(&press(&boot, "new")).is_some());
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
}
