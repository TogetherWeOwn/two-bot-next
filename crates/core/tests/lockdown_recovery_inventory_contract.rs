//! Protected acceptance for the private saved-recovery inventory.
//! Missing publication/routing, a public/ungated command, or misleading help
//! must fail independently of the disposable-store/runtime tests.

use two_bot_core::{
    help_text, moderation::moderation_commands, CommandDefinition, InteractionRouter, RouterGates,
    SlashContext, SlashOutcome,
};

const COMMAND: &str = "lockdown-records";

fn router(enabled: bool) -> InteractionRouter {
    InteractionRouter::production_router(
        RouterGates {
            configured_guild: Some(111111111111111111),
            moderation: enabled,
            scorecard: false,
            automations: false,
            announcements: false,
            voice: false,
            voice_assistant: false,
            tickets: false,
            self_roles: false,
            onboarding_picker: false,
            session_picker: false,
        },
        true,
    )
}

fn definition() -> CommandDefinition {
    moderation_commands()
        .into_iter()
        .find(|def| def.name == COMMAND)
        .expect("missing private saved recovery inventory in command-owned moderation definitions")
}

#[test]
fn inventory_publishes_once_with_bounded_optional_page_and_no_reason() {
    let definition = definition();
    assert!(!definition.dm_permission);
    assert_eq!(definition.default_member_permissions.as_deref(), Some("16"));
    assert_eq!(definition.options.len(), 1, "reads need no audit reason");
    let page = &definition.options[0];
    assert_eq!(page.name, "page");
    assert_eq!(page.kind, 4);
    assert_ne!(page.required, Some(true), "page one must work by default");
    assert_eq!(page.min_value, Some(1));
    assert!(page.max_value.is_some_and(|max| (2..=1000).contains(&max)));
    let published = router(true).publish_set(&[]).expect("bounded publish set");
    assert_eq!(
        published.iter().filter(|def| def.name == COMMAND).count(),
        1
    );
    assert_eq!(
        published.iter().find(|def| def.name == COMMAND),
        Some(&definition)
    );
    assert!(!router(false)
        .publish_set(&[])
        .expect("disabled publish set")
        .iter()
        .any(|def| def.name == COMMAND));
}

#[test]
fn inventory_is_executable_only_in_configured_guild_with_manage_channels() {
    let router = router(true);
    let context = |guild_id, actor_permissions| SlashContext {
        name: COMMAND,
        guild_id,
        actor_permissions,
        custom_row: None,
    };
    let granted = router.route_slash(&context(Some(111111111111111111), Some(16)));
    let SlashOutcome::Handled { handler } = granted else {
        panic!("authorized inventory not routed: {granted:?}");
    };
    assert!(router.handler_for(&handler).is_some());
    for (guild, permissions) in [
        (None, Some(16)),
        (Some(999999999999999999), Some(16)),
        (Some(111111111111111111), None),
        (Some(111111111111111111), Some(0)),
        (Some(111111111111111111), Some(8192)),
    ] {
        assert!(matches!(
            router.route_slash(&context(guild, permissions)),
            SlashOutcome::Refuse { .. }
        ));
    }
    assert!(matches!(
        self::router(false).route_slash(&context(Some(111111111111111111), Some(16))),
        SlashOutcome::Refuse { .. }
    ));
}

#[test]
fn help_distinguishes_saved_records_from_live_lockdown_certification() {
    let defs = router(true).publish_set(&[]).expect("publish set");
    assert!(help_text(&defs).contains(&format!("/{COMMAND}")));
    let text = definition().help_detail_text().to_lowercase();
    assert!(text.contains("saved") && text.contains("recover"), "{text}");
    assert!(
        text.contains("page"),
        "pagination must be discoverable: {text}"
    );
    assert!(!text.contains("automatically unlock"));
}

#[test]
fn control_current_channel_inspect_and_mutating_gates_are_unchanged() {
    let definitions = moderation_commands();
    for name in ["channel-inspect", "lockdown", "unlock", "slowmode"] {
        let definition = definitions
            .iter()
            .find(|def| def.name == name)
            .expect("existing command");
        assert_eq!(definition.default_member_permissions.as_deref(), Some("16"));
        assert!(!definition.dm_permission);
        if name == "channel-inspect" {
            assert!(definition.options.is_empty());
        } else {
            assert!(definition
                .options
                .iter()
                .any(|opt| opt.name == "reason" && opt.required == Some(true)));
        }
        assert!(matches!(
            router(true).route_slash(&SlashContext {
                name,
                guild_id: Some(111111111111111111),
                actor_permissions: Some(16),
                custom_row: None,
            }),
            SlashOutcome::Handled { .. }
        ));
    }
}
