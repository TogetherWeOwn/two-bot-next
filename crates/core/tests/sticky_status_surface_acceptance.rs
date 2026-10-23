//! Protected public command contract; literal permission bits are independent
//! of production definitions. Rendering/store behavior is tested at the runtime.
use two_bot_core::{
    HandlerId, InteractionRouter, RouterGates, RouterRefusal, SlashContext, SlashOutcome,
};

fn router(automations: bool) -> InteractionRouter {
    InteractionRouter::new(RouterGates {
        configured_guild: Some(2222),
        scorecard: false,
        automations,
        announcements: false,
        moderation: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    })
}

#[test]
fn status_is_guild_only_manage_server_and_has_no_target_or_mutation_options() {
    let published = router(true).publish_set(&[]).expect("valid registry");
    let status = published
        .iter()
        .find(|c| c.name == "sticky-status")
        .expect("read-only current-channel inspect command must be discoverable");
    assert!(!status.dm_permission);
    assert_eq!(status.default_member_permissions.as_deref(), Some("32"));
    assert_eq!(
        status.description,
        "Inspect this channel's saved sticky and delivery state"
    );
    assert_eq!(
        status.options.len(),
        1,
        "no remote channel/guild or reset options"
    );
    let view = &status.options[0];
    assert_eq!(view.name, "view");
    assert_eq!(view.kind, 3);
    assert!(!view.required.unwrap_or(false));
    assert_eq!(view.max_length, Some(8));
    assert_eq!(
        view.choices
            .iter()
            .map(|c| (c.name.as_str(), c.value.as_str()))
            .collect::<Vec<_>>(),
        [("Metadata", "metadata"), ("Saved body", "body")]
    );
}

#[test]
fn status_rechecks_management_authority_instead_of_trusting_published_permissions() {
    let router = router(true);
    for (bits, outcome) in [
        (
            Some(32),
            SlashOutcome::Handled {
                handler: HandlerId::AutomationAdmin,
            },
        ),
        (
            Some(u64::MAX),
            SlashOutcome::Handled {
                handler: HandlerId::AutomationAdmin,
            },
        ),
        (
            None,
            SlashOutcome::Refuse {
                refusal: RouterRefusal::ManageServerRequired,
            },
        ),
        (
            Some(0),
            SlashOutcome::Refuse {
                refusal: RouterRefusal::ManageServerRequired,
            },
        ),
        (
            Some(8),
            SlashOutcome::Refuse {
                refusal: RouterRefusal::ManageServerRequired,
            },
        ),
    ] {
        assert_eq!(
            router.route_slash(&SlashContext {
                name: "sticky-status",
                guild_id: Some(2222),
                actor_permissions: bits,
                custom_row: None,
            }),
            outcome,
            "same resolved-ManageGuild contract as sticky set/remove; bits={bits:?}"
        );
    }
}

#[test]
fn status_cannot_bypass_automation_gate_or_publish_a_shadowing_dynamic_row() {
    let off = router(false);
    assert!(!off
        .publish_set(&[])
        .expect("off registry")
        .iter()
        .any(|c| c.name == "sticky-status"));
    assert_eq!(
        off.route_slash(&SlashContext {
            name: "sticky-status",
            guild_id: Some(2222),
            actor_permissions: Some(32),
            custom_row: None,
        }),
        SlashOutcome::Refuse {
            refusal: RouterRefusal::AutomationsDisabled
        }
    );
    let custom = two_bot_core::commands::CustomCommand {
        name: "sticky-status".into(),
        description: "Must not shadow management".into(),
        enabled: true,
    };
    let commands = router(true)
        .publish_set(&[custom])
        .expect("deduplicated surface");
    let matches: Vec<_> = commands
        .iter()
        .filter(|c| c.name == "sticky-status")
        .collect();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].default_member_permissions.as_deref(), Some("32"));
}

#[test]
fn status_keeps_the_existing_core_guild_fence_before_any_store_dispatch() {
    for guild in [None, Some(9999)] {
        assert_eq!(
            router(true).route_slash(&SlashContext {
                name: "sticky-status",
                guild_id: guild,
                actor_permissions: Some(u64::MAX),
                custom_row: None,
            }),
            SlashOutcome::Ignore
        );
    }
}
