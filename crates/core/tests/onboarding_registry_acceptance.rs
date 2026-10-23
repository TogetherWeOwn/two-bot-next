//! Onboarding admin publication must not depend on feature enablement or
//! weaken the guild/Manage Server boundary. No guild, network or database.

use serde_json::{json, Value};
use two_bot_core::{HandlerId, InteractionRouter, RouterGates, SlashContext, SlashOutcome};

const GUILD: u64 = 2222;

fn gates(enabled: bool, voice: bool) -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD),
        scorecard: enabled,
        automations: enabled,
        announcements: enabled,
        moderation: enabled,
        voice,
        voice_assistant: voice,
        tickets: enabled,
        self_roles: enabled,
        onboarding_picker: enabled,
        session_picker: enabled,
    }
}

#[test]
fn onboarding_is_always_published_once_with_live_authorization_required() {
    for (enabled, voice, count) in [(false, false, 5), (true, false, 30), (true, true, 51)] {
        let router = InteractionRouter::new(gates(enabled, voice));
        let definitions = router.publish_set(&[]).expect("publish set assembles");
        assert_eq!(definitions.len(), count);
        let matching: Vec<_> = definitions
            .iter()
            .filter(|d| d.name == "onboarding")
            .collect();
        assert_eq!(matching.len(), 1);
        let definition = matching[0];
        assert!(!definition.dm_permission);
        assert_eq!(definition.default_member_permissions.as_deref(), Some("32"));
        for permissions in [None, Some(0), Some(8), Some(!32)] {
            assert!(matches!(
                router.route_slash(&SlashContext {
                    name: "onboarding",
                    guild_id: Some(GUILD),
                    actor_permissions: permissions,
                    custom_row: Some(true),
                }),
                SlashOutcome::Refuse { .. }
            ));
        }
        assert_eq!(
            router.route_slash(&SlashContext {
                name: "onboarding",
                guild_id: Some(GUILD),
                actor_permissions: Some(32),
                custom_row: None,
            }),
            SlashOutcome::Handled {
                handler: HandlerId::Onboarding
            }
        );
        for guild_id in [None, Some(GUILD + 1)] {
            assert_eq!(
                router.route_slash(&SlashContext {
                    name: "onboarding",
                    guild_id,
                    actor_permissions: Some(u64::MAX),
                    custom_row: Some(true),
                }),
                SlashOutcome::Ignore
            );
        }
    }
}

#[test]
fn onboarding_publishes_only_the_five_approved_admin_subcommands_and_bounded_inputs() {
    let definitions = InteractionRouter::new(gates(false, false))
        .publish_set(&[])
        .expect("publish set assembles");
    let definition = definitions
        .iter()
        .find(|d| d.name == "onboarding")
        .expect("onboarding");
    let command = serde_json::to_value(definition).expect("definition serializes");
    let options = command["options"].as_array().expect("subcommands");
    let names: Vec<_> = options
        .iter()
        .map(|option| option["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "join-roles",
            "member-role",
            "clear-join",
            "clear-member",
            "panel"
        ]
    );
    assert!(options.iter().all(|option| option["type"] == json!(1)));
    for (index, name, kind) in [(0, "roles", 3), (1, "role", 8)] {
        let inputs = options[index]["options"]
            .as_array()
            .expect("subcommand inputs");
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0]["name"], name);
        assert_eq!(inputs[0]["type"], kind);
        assert_eq!(inputs[0]["required"], true);
    }
    assert_eq!(options[0]["options"][0]["max_length"], 209);
    for option in &options[2..] {
        assert!(option
            .get("options")
            .is_none_or(|inputs| inputs == &Value::Null || inputs == &json!([])));
    }
}
