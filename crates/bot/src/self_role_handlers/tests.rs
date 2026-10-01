use super::*;
use crate::command_runtime_tests::{message, slash};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use twilight_model::{
    application::interaction::message_component::MessageComponentInteractionData, id::Id,
};
use two_bot_core::self_roles::{self_role_custom_id, SelfRoleOption};
use two_bot_cutover::self_role_store::SelfRoleStore;
use two_bot_discord::ActionExecutor;

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

fn runtime() -> SelfRoleRuntime {
    let options = PgConnectOptions::new()
        .host("agent-testdb")
        .port(5432)
        .username("agent_test")
        .password("")
        .database("agent_test");
    let pool = PgPoolOptions::new().connect_lazy_with(options);
    SelfRoleRuntime {
        store: SelfRoleStore::new(pool),
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
