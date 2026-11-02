#![cfg(test)]

use super::*;
use crate::command_runtime_tests::{message, slash};
use crate::discord_test_common::{MockRest, ScriptedResponse};
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use twilight_model::{
    application::interaction::message_component::MessageComponentInteractionData, id::Id,
};
use two_bot_core::self_roles::{self_role_custom_id, SelfRoleOption};
use two_bot_core::settings::{live_channel, SettingRow, SettingsSnapshot};

pub(crate) const GUILD: u64 = 100_000_000_000_000_001;
const USER: u64 = 100_000_000_000_000_002;
const BOT: u64 = 100_000_000_000_000_003;
const CHANNEL: u64 = 100_000_000_000_000_007;
const MESSAGE: u64 = 100_000_000_000_000_008;

pub(crate) fn component(panel: &SelfRolePanel, event_id: u64, values: Vec<String>) -> Interaction {
    let mut interaction = slash("unused", Some(panel.channel_id.parse().unwrap()), vec![]);
    interaction.guild_id = Some(Id::new(GUILD));
    interaction.id = Id::new(event_id);
    interaction.kind = InteractionType::MessageComponent;
    interaction
        .member
        .as_mut()
        .unwrap()
        .user
        .as_mut()
        .unwrap()
        .id = Id::new(USER);
    let mut source = message(1, panel.channel_id.parse().unwrap(), true, Some(GUILD));
    source.id = Id::new(panel.message_id.parse().unwrap());
    interaction.message = Some(source);
    interaction.data = Some(InteractionData::MessageComponent(Box::new(
        MessageComponentInteractionData {
            custom_id: self_role_custom_id(
                &panel.id,
                (panel.mode == PanelMode::Button).then_some("new"),
            ),
            component_type: if panel.mode == PanelMode::Button {
                ComponentType::Button
            } else {
                ComponentType::TextSelectMenu
            },
            resolved: None,
            values,
        },
    )));
    interaction
}

pub(crate) fn reaction() -> GatewayReaction {
    GatewayReaction {
        burst: false,
        burst_colors: vec![],
        channel_id: Id::new(CHANNEL),
        emoji: EmojiReactionType::Unicode { name: "b".into() },
        guild_id: Some(Id::new(GUILD)),
        member: None,
        message_author_id: None,
        message_id: Id::new(MESSAGE),
        user_id: Id::new(USER),
    }
}

/// Lazy agent-testdb pool: these fixtures never perform database I/O.
fn lazy_store() -> SelfRoleStore {
    let options = PgConnectOptions::new()
        .host("agent-testdb")
        .port(5432)
        .username("agent_test")
        .password("")
        .database("agent_test");
    SelfRoleStore::new(PgPoolOptions::new().connect_lazy_with(options))
}

fn runtime() -> SelfRoleRuntime {
    SelfRoleRuntime {
        store: lazy_store(),
        executor: ActionExecutor::with_proxy(
            "fixture-token".into(),
            Some("http://127.0.0.1:9".into()),
        )
        .unwrap(),
        guild_id: GUILD.to_string(),
        bot_id: BOT.to_string(),
    }
}

fn gates(mode: PanelMode) -> SelfRoleGates {
    SelfRoleGates {
        dry_run: false,
        panels: vec![SelfRolePanel {
            id: "games".into(),
            channel_id: CHANNEL.to_string(),
            message_id: MESSAGE.to_string(),
            mode,
            exclusive: true,
            color: false,
            options: vec![SelfRoleOption {
                key: "new".into(),
                label: "New".into(),
                role_id: "100000000000000004".into(),
                permissions: "0".into(),
                emoji: Some("b".into()),
                description: None,
            }],
        }],
    }
}

fn service(mode: PanelMode) -> SelfRoleService {
    SelfRoleService::new(
        runtime(),
        gates(mode),
        &[GUILD.to_string()].into_iter().collect(),
    )
    .unwrap()
}

#[tokio::test]
async fn admission_terminal_rejection_never_reports_unresolved() {
    let service = service(PanelMode::Select);
    let panel = &gates(PanelMode::Select).panels[0];
    for code in ["unsafe_live_roles", "disallowed_role_permission", "unknown"] {
        let result = service
            .handle_admission(Ok(Admission::Rejected(code)), panel, "fixture-event")
            .await;
        assert_eq!(result, SelfRoleResult::Rejected, "code={code}");
        for color in [false, true] {
            let reply = result.reply(color);
            assert_eq!(reply, "This role selection could not be applied. Try again later, or contact a server admin to check this panel.");
            assert!(!reply.contains("unresolved"));
        }
    }
}

#[tokio::test]
async fn admission_unavailable_evidence_never_reports_terminal_rejection() {
    let service = service(PanelMode::Select);
    let panel = &gates(PanelMode::Select).panels[0];
    for error in [
        RuntimeError::InvalidSnapshot,
        RuntimeError::Store(StoreErrorKind::Database),
        RuntimeError::Store(StoreErrorKind::Timeout),
    ] {
        let result = service
            .handle_admission(Err(error), panel, "fixture-event")
            .await;
        assert_eq!(result, SelfRoleResult::Pending);
        for color in [false, true] {
            let reply = result.reply(color);
            assert_eq!(reply, "This role selection is unresolved; no successful change has been confirmed. Please wait a moment and try again; if it keeps failing, contact a server admin.");
            assert!(!reply.contains("could not be applied"));
        }
    }
}

/// A valid stored catalogue with a distinct panel id, proving the live row
/// wins over (rather than merges into) the boot catalogue.
fn live_catalogue() -> serde_json::Value {
    json!([{
        "id": "live-games",
        "channelId": "111111111111111111",
        "messageId": "555555555555555555",
        "mode": "button",
        "options": [{
            "key": "chess",
            "label": "Chess",
            "roleId": "222222222222222222",
            "permissions": "0"
        }]
    }])
}

#[test]
fn live_panels_resolve_store_first_with_boot_fallback() {
    let boot = gates(PanelMode::Button).panels;
    assert_eq!(boot.len(), 1);
    assert_eq!(boot[0].id, "games");
    let guild = GUILD.to_string();
    // No snapshot (poller parked or pre-first-poll) reads boot.
    assert_eq!(live_panels_with(&guild, &boot, None), boot);
    let (mut writer, live) = live_channel();
    assert_eq!(live_panels_with(&guild, &boot, Some(&live)), boot);
    // Every publish moves the revision: the channel only republishes on
    // moved marks, so a repeated revision would read back stale.
    let mut revision = 0;
    let mut publish = |rows: Vec<SettingRow>| {
        revision += 1;
        writer.publish(&SettingsSnapshot { revision, rows })
    };
    let row = |guild: &str, value: serde_json::Value| SettingRow {
        guild_id: guild.to_owned(),
        key: PANELS_KEY.to_owned(),
        value,
        version: 1,
    };
    // A stored catalogue wins over the boot one.
    publish(vec![row(&guild, live_catalogue())]);
    let resolved = live_panels_with(&guild, &boot, Some(&live));
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].id, "live-games");
    // A stored empty array resolves to no panels (live disable).
    publish(vec![row(&guild, json!([]))]);
    assert!(live_panels_with(&guild, &boot, Some(&live)).is_empty());
    // Unparsable rows fall back to boot: a bypassed write guard must not
    // blank live pickers.
    for stored in [json!("not-json"), json!({"panels": []}), json!(7)] {
        publish(vec![row(&guild, stored.clone())]);
        assert_eq!(
            live_panels_with(&guild, &boot, Some(&live)),
            boot,
            "{stored} must fall back to boot"
        );
    }
    // Another guild's row never leaks across.
    publish(vec![row("other-guild", live_catalogue())]);
    assert_eq!(live_panels_with(&guild, &boot, Some(&live)), boot);
    // Deleting the row hands the catalogue back to boot.
    publish(vec![]);
    assert_eq!(live_panels_with(&guild, &boot, Some(&live)), boot);
}

#[tokio::test]
async fn recover_panels_empty_sweeps_nothing() {
    let service = service(PanelMode::Button);
    assert_eq!(service.recover_panels(&[]).await.unwrap(), 0);
}

#[tokio::test]
async fn recovery_job_has_bounded_cadence_jitter_and_owned_service() {
    let service = Arc::new(service(PanelMode::Button));
    let job = service.recovery_job();
    assert_eq!(job.name, RECOVERY_JOB_NAME);
    assert_eq!(job.cadence, Duration::from_secs(30));
    assert_eq!(job.timeout, Duration::from_secs(25));
    assert!(job.startup_jitter <= Duration::from_secs(5));
    assert_eq!(Arc::strong_count(&service), 2);
    drop(job);
    assert_eq!(Arc::strong_count(&service), 1);
}

#[test]
fn recovery_error_class_keeps_each_store_kind_distinct() {
    use two_bot_discord::executor::self_roles::SelfRoleRestError;

    assert_eq!(
        recovery_error_class(RuntimeError::Store(StoreErrorKind::Timeout)),
        ErrorClass::Timeout
    );
    assert_eq!(
        recovery_error_class(RuntimeError::Store(StoreErrorKind::Database)),
        ErrorClass::Database
    );
    // A zero-row receipt completion answered successfully, so it must not
    // report as a database failure. State mismatches share the
    // configuration class without becoming `Stale`.
    assert_eq!(
        recovery_error_class(RuntimeError::Store(StoreErrorKind::UnmatchedReceipt)),
        ErrorClass::Configuration
    );
    assert_eq!(
        recovery_error_class(RuntimeError::Rest(SelfRoleRestError::RateLimited)),
        ErrorClass::Rest
    );
    assert_eq!(
        recovery_error_class(RuntimeError::Stale),
        ErrorClass::Configuration
    );
}

#[tokio::test]
async fn empty_or_denied_catalogue_never_registers_a_surface() {
    let allowlist = [GUILD.to_string()].into_iter().collect();
    assert!(SelfRoleService::new(
        runtime(),
        SelfRoleGates {
            panels: vec![],
            dry_run: false
        },
        &allowlist
    )
    .is_none());
    assert!(SelfRoleService::new(runtime(), gates(PanelMode::Button), &HashSet::new()).is_none());
}

#[tokio::test]
async fn invalid_catalogue_parks_service_at_boot() {
    // A catalogue the strict parser refuses (21 reactions exceeds Discord's
    // 20-reaction bound) must park `from_env` before any REST or store use.
    let key = "TWO_SELF_ROLE_PANELS";
    let prior = std::env::var(key).ok();
    let options = (0..21)
        .map(|i| {
            format!(
                "{{\"key\":\"k{i:02}\",\"label\":\"Chess\",\"roleId\":\"10000000000000{:04}\",\"permissions\":\"0\",\"emoji\":\"e{i:02}\"}}",
                100 + i,
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    std::env::set_var(
        key,
        format!(
            "[{{\"id\":\"react\",\"channelId\":\"{CHANNEL}\",\"messageId\":\"{MESSAGE}\",\"mode\":\"reaction\",\"options\":[{options}]}}]"
        ),
    );
    let options = PgConnectOptions::new()
        .host("agent-testdb")
        .port(5432)
        .username("agent_test")
        .password("")
        .database("agent_test");
    let pool = PgPoolOptions::new().connect_lazy_with(options);
    let parked = SelfRoleService::from_env(pool, "fixture-token", GUILD).await;
    assert!(parked.is_none());
    match prior {
        Some(value) => std::env::set_var(key, value),
        None => std::env::remove_var(key),
    }
}

#[test]
fn boot_allowlist_is_only_the_pinned_staging_guild() {
    let allowlist = staging_allowlist();
    assert_eq!(allowlist.len(), 1);
    assert!(allowlist.contains(TWO_STAGING_GUILD_ID));
    assert!(!allowlist.contains(two_bot_core::TWO_GUILD_ID));
}

#[tokio::test]
async fn boot_checks_catalogue_and_staging_guild_before_one_identity_read() {
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(200, json!({"id": BOT.to_string(), "bot": true})),
    )
    .await;
    let executor =
        || ActionExecutor::with_proxy("fixture-token".into(), Some(mock.origin())).unwrap();
    let allowlist: HashSet<String> = [GUILD.to_string()].into_iter().collect();
    let empty = SelfRoleGates {
        panels: vec![],
        dry_run: false,
    };
    assert!(
        SelfRoleService::boot(empty, lazy_store(), executor(), GUILD, &allowlist)
            .await
            .is_none()
    );
    let denied = GUILD + 1;
    assert!(SelfRoleService::boot(
        gates(PanelMode::Button),
        lazy_store(),
        executor(),
        denied,
        &allowlist
    )
    .await
    .is_none());
    assert!(mock.requests().is_empty());

    let service = SelfRoleService::boot(
        gates(PanelMode::Button),
        lazy_store(),
        executor(),
        GUILD,
        &allowlist,
    )
    .await
    .expect("approved staging catalogue");
    assert_eq!(service.runtime.guild_id, GUILD.to_string());
    assert_eq!(service.runtime.bot_id, BOT.to_string());
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/api/v10/users/@me");
}

#[tokio::test]
async fn boot_parks_without_a_verified_bot_identity() {
    for response in [
        ScriptedResponse::status(500),
        ScriptedResponse::json(200, json!({"id": BOT.to_string(), "bot": false})),
    ] {
        let mock = MockRest::start(vec![], response).await;
        let executor =
            ActionExecutor::with_proxy("fixture-token".into(), Some(mock.origin())).unwrap();
        let allowlist = [GUILD.to_string()].into_iter().collect();
        assert!(SelfRoleService::boot(
            gates(PanelMode::Select),
            lazy_store(),
            executor,
            GUILD,
            &allowlist
        )
        .await
        .is_none());
        assert_eq!(mock.requests().len(), 1);
    }
}

#[tokio::test]
async fn components_require_guild_member_source_and_matching_surface() {
    let service = service(PanelMode::Button);
    let good = component(&service.panels[0], 100_000_000_000_000_020, vec![]);
    assert!(service.component_input(&good).is_some());
    for fault in [
        "guild",
        "message",
        "channel",
        "member",
        "bot",
        "modal",
        "surface",
        "values",
        "custom_id",
    ] {
        let mut bad = good.clone();
        match fault {
            "guild" => bad.guild_id = Some(Id::new(GUILD + 1)),
            "message" => bad.message.as_mut().unwrap().id = Id::new(MESSAGE + 1),
            "channel" => bad.message.as_mut().unwrap().channel_id = Id::new(CHANNEL + 1),
            "member" => bad.member = None,
            "bot" => bad.member.as_mut().unwrap().user.as_mut().unwrap().bot = true,
            "modal" => bad.kind = InteractionType::ModalSubmit,
            other => {
                let Some(InteractionData::MessageComponent(data)) = &mut bad.data else {
                    unreachable!()
                };
                match other {
                    "surface" => data.component_type = ComponentType::TextSelectMenu,
                    "values" => data.values = vec!["new".into()],
                    "custom_id" => data.custom_id = "two:self-role:games:new:extra".into(),
                    _ => unreachable!(),
                }
            }
        }
        assert!(service.component_input(&bad).is_none(), "{fault}");
    }
}

#[tokio::test]
async fn selects_accept_empty_but_refuse_duplicate_unknown_or_excess_options() {
    let mut service = service(PanelMode::Select);
    let mut old = service.panels[0].options[0].clone();
    old.key = "old".into();
    old.role_id = "100000000000000009".into();
    service.panels[0].options.push(old);
    let id = 100_000_000_000_000_021;
    let empty = component(&service.panels[0], id, vec![]);
    assert!(service.component_input(&empty).is_some());
    assert!(service
        .component_input(&component(&service.panels[0], id, vec!["new".into()]))
        .is_some());
    for values in [
        vec!["new".into(), "new".into()],
        vec!["unknown".into()],
        vec!["new".into(), "old".into()],
    ] {
        assert!(service
            .component_input(&component(&service.panels[0], id, values))
            .is_none());
    }
}

#[tokio::test]
async fn reaction_partials_mint_unique_identity_before_work_and_match_deleted_custom_emoji() {
    let mut service = service(PanelMode::Reaction);
    let event = reaction();
    let first = service.reaction_input(&event, false).unwrap();
    let second = service.reaction_input(&event, true).unwrap();
    assert_ne!(first.request.event_id, second.request.event_id);
    assert_ne!(first.request.event_order, second.request.event_order);
    assert!(matches!(
        second.request.selection,
        Selection::Reaction { remove: true, .. }
    ));
    let mut bad = event.clone();
    bad.guild_id = None;
    assert!(service.reaction_input(&bad, false).is_none());
    bad = event.clone();
    bad.user_id = Id::new(BOT);
    assert!(service.reaction_input(&bad, false).is_none());
    service.panels[0].options[0].emoji = Some("<:chess:100000000000000010>".into());
    let mut custom = event;
    custom.emoji = EmojiReactionType::Custom {
        animated: false,
        id: Id::new(100_000_000_000_000_010),
        name: None,
    };
    assert!(service.reaction_input(&custom, false).is_some());
}

#[test]
fn every_result_reply_pins_its_exact_text_in_both_color_modes() {
    for color in [false, true] {
        assert_eq!(
            SelfRoleResult::Settled(SettledOutcome::Assigned).reply(color),
            "Role added.",
            "color={color}"
        );
        assert_eq!(
            SelfRoleResult::Settled(SettledOutcome::Removed).reply(color),
            "Role removed.",
            "color={color}"
        );
        assert_eq!(
            SelfRoleResult::Settled(SettledOutcome::Switched).reply(color),
            if color {
                "Color updated."
            } else {
                "Role updated."
            },
            "color={color}"
        );
        assert_eq!(
            SelfRoleResult::Settled(SettledOutcome::AlreadyHeld).reply(color),
            "No role changes were needed.",
            "color={color}"
        );
        assert_eq!(
            SelfRoleResult::Settled(SettledOutcome::AlreadyAbsent).reply(color),
            "No role changes were needed.",
            "color={color}"
        );
        assert_eq!(
            SelfRoleResult::Settled(SettledOutcome::Rejected).reply(color),
            "I could not change that role. Staff have been notified in the logs.",
            "color={color}"
        );
        assert_eq!(
            SelfRoleResult::DryRun.reply(color),
            "Dry run recorded; no roles were changed.",
            "color={color}"
        );
        assert_eq!(
            SelfRoleResult::Duplicate.reply(color),
            "This selection is already being handled or was handled earlier.",
            "color={color}"
        );
        assert_eq!(
            SelfRoleResult::Ignored.reply(color),
            "This role selection could not be applied.",
            "color={color}"
        );
        assert_eq!(
            SelfRoleResult::Rejected.reply(color),
            "This role selection could not be applied. Try again later, or contact a server admin to check this panel.",
            "color={color}"
        );
        for refusal in [
            SafeRefusal::MissingManageRoles,
            SafeRefusal::MissingRole,
            SafeRefusal::RoleHierarchy,
        ] {
            assert_eq!(
                SelfRoleResult::Refused(refusal).reply(color),
                refusal.reply(),
                "color={color}"
            );
        }
        assert_eq!(
            SelfRoleResult::Pending.reply(color),
            "This role selection is unresolved; no successful change has been confirmed. Please wait a moment and try again; if it keeps failing, contact a server admin.",
            "color={color}"
        );
    }
}
