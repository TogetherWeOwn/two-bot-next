//! Protected publication, authorization and discovery contract for saved-response cloning.
//! The runtime/store behavior lives in the companion Discord integration target.
use serde_json::{json, Value};
use two_bot_core::{
    command_permissions::{command_permission, CommandSurface},
    custom_commands::{
        builtin_command_names, reserved_command_names, validate_put_input, CommandError,
        PutCommandInput,
    },
    help::help_text,
    InteractionRouter, RouterGates,
};

fn gates(automations: bool) -> RouterGates {
    RouterGates {
        configured_guild: Some(2222),
        automations,
        moderation: false,
        voice: false,
        voice_assistant: false,
        scorecard: false,
        announcements: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    }
}

#[test]
fn clone_publishes_one_narrow_guild_only_management_shape() {
    let router = InteractionRouter::new(gates(true));
    let definitions = router.publish_set(&[]).expect("complete publication");
    let clones: Vec<_> = definitions
        .iter()
        .filter(|d| d.name == "command-clone")
        .collect();
    assert_eq!(
        clones.len(),
        1,
        "the public clone workflow must be published, not merely implemented as a helper"
    );
    let definition = serde_json::to_value(clones[0]).expect("compiled registry JSON");
    assert_eq!(
        definition,
        json!({
            "name": "command-clone",
            "description": "Clone a saved custom command under a new name",
            "dm_permission": false,
            "default_member_permissions": "32",
            "options": [
                {"name": "source", "description": "Saved source command name, a-z 0-9 _ -",
                 "type": 3, "required": true, "max_length": 32},
                {"name": "destination", "description": "New absent destination name, a-z 0-9 _ -",
                 "type": 3, "required": true, "max_length": 32}
            ]
        })
    );
    let names: Vec<_> = definitions.iter().map(|d| d.name.as_str()).collect();
    let inspect = names
        .iter()
        .position(|name| *name == "command-inspect")
        .expect("inspection control");
    assert_eq!(
        names[inspect + 1],
        "command-clone",
        "keep management workflows together"
    );
    let disabled = InteractionRouter::new(gates(false))
        .publish_set(&[])
        .expect("feature-off publication");
    assert!(!disabled.iter().any(|d| d.name == "command-clone"));
    assert!(disabled.iter().any(|d| d.name == "help"));
}

#[test]
fn clone_has_live_manage_guild_authority_and_cannot_be_shadowed() {
    let permission = command_permission("command-clone").expect("live runtime authorization row");
    assert_eq!(permission.required_permissions, 32);
    assert_eq!(permission.surface, CommandSurface::BuiltinSlash);
    assert!(builtin_command_names().contains("command-clone"));
    assert!(reserved_command_names().contains("command-clone"));
    assert!(matches!(
        validate_put_input(
            &PutCommandInput {
                name: "command-clone".to_owned(),
                description: "Never shadow management".to_owned(),
                template: "Safe template".to_owned(),
                text_trigger: None,
            },
            &reserved_command_names()
        ),
        Err(CommandError::ReservedName(_))
    ));
}

#[test]
fn published_clone_is_discoverable_once_under_manage_server_automations_not_custom() {
    let definitions = InteractionRouter::new(gates(true))
        .publish_set(&[])
        .expect("live snapshot");
    let text = help_text(&definitions);
    let automations = text
        .split("**Automations** — needs Manage Server\n")
        .nth(1)
        .expect("existing Manage Server discovery group")
        .split("\n\n")
        .next()
        .expect("group body");
    let tokens: Vec<_> = automations
        .split_whitespace()
        .filter(|word| word.starts_with('/'))
        .collect();
    assert_eq!(
        tokens
            .iter()
            .filter(|token| **token == "/command-clone")
            .count(),
        1,
        "clone must be discoverable beside existing management, not an ungrouped custom command"
    );
    for control in ["/command", "/command-list", "/command-inspect"] {
        assert!(tokens.contains(&control));
    }
    let all_tokens: Vec<_> = text
        .split_whitespace()
        .filter(|word| word.starts_with('/'))
        .collect();
    assert_eq!(
        all_tokens
            .iter()
            .filter(|token| **token == "/command-clone")
            .count(),
        1
    );
    let off = InteractionRouter::new(gates(false))
        .publish_set(&[])
        .expect("feature off");
    assert!(!help_text(&off).contains("/command-clone"));
    assert!(
        !help_text(&[]).contains("/command-clone"),
        "unconfirmed discovery is not a fresh DB view"
    );
}

#[test]
fn existing_create_options_remain_compatible_with_the_new_clone_workflow() {
    let definitions = InteractionRouter::new(gates(true))
        .publish_set(&[])
        .expect("registry");
    let create = definitions
        .iter()
        .find(|d| d.name == "command")
        .expect("existing management control");
    let value = serde_json::to_value(create).expect("create JSON");
    let options = value["options"]
        .as_array()
        .expect("existing create options");
    let names: Vec<_> = options
        .iter()
        .map(|v| v["name"].as_str().expect("option name"))
        .collect();
    assert_eq!(names, ["name", "template", "description", "text-trigger"]);
    let required: Vec<_> = options
        .iter()
        .filter(|v| v["required"] == Value::Bool(true))
        .map(|v| v["name"].as_str().expect("required name"))
        .collect();
    assert_eq!(required, ["name", "template"]);
    assert_eq!(value["default_member_permissions"], "32");
    assert_eq!(value["dm_permission"], false);
}
