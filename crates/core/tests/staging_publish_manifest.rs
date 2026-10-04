//! Staging publish manifest drift test (offline).
//!
//! Pins the command set expected on the staging guild — the all-gates-on
//! [`InteractionRouter::publish_set`] output in legacy publish order — plus
//! the automation-gating contract for dynamic DB-backed custom commands.
//! Code-derived: the fixture (`fixtures/staging_published_commands.json`)
//! is generated from the router, and this test fails on any drift between
//! the router-advertised set and the manifest.
//!
//! Offline carve only: no guild touch, no staging secrets, no database.
//! The live diff/publish workflow stays in `docs/command-publish.md`.
//! Distinct from `registry_golden.rs` (legacy parity axis: next vs the frozen
//! legacy snapshot) — this is the staging publish axis (router vs manifest).

use two_bot_core::commands::CustomCommand;
use two_bot_core::router::{InteractionRouter, RouterGates};

use serde_json::Value;

fn manifest() -> Value {
    serde_json::from_str(include_str!("fixtures/staging_published_commands.json"))
        .expect("staging publish manifest is valid JSON")
}

fn manifest_names(key: &str) -> Vec<String> {
    manifest()[key]
        .as_array()
        .unwrap_or_else(|| panic!("manifest must list {key}"))
        .iter()
        .map(|name| {
            name.as_str()
                .unwrap_or_else(|| panic!("manifest {key} entries are command names"))
                .to_owned()
        })
        .collect()
}

/// Staging enables every publish gate; the guild id here is a placeholder —
/// the publish set does not vary by guild, and no guild is ever contacted.
fn staging_gates() -> RouterGates {
    RouterGates {
        configured_guild: Some(1),
        scorecard: true,
        automations: true,
        announcements: true,
        moderation: true,
        voice: false,
        voice_assistant: false,
        tickets: true,
        self_roles: true,
        onboarding_picker: true,
        session_picker: true,
    }
}

fn published_names(router: &InteractionRouter, custom: &[CustomCommand]) -> Vec<String> {
    router
        .publish_set(custom)
        .expect("staging publish set assembles")
        .iter()
        .map(|def| def.name.clone())
        .collect()
}

#[test]
fn staging_builtin_set_matches_manifest_in_publish_order() {
    let router = InteractionRouter::new(staging_gates());
    assert_eq!(
        published_names(&router, &[]),
        manifest_names("builtins_in_publish_order"),
        "router-advertised staging builtins drifted from the manifest: \
         update the fixture from publish_set output, never hand-edit names",
    );
}

#[test]
fn example_dynamic_command_appends_last_while_automations_on() {
    let router = InteractionRouter::new(staging_gates());
    let custom: Vec<CustomCommand> = manifest()["example_dynamic"]
        .as_array()
        .expect("manifest example_dynamic array")
        .iter()
        .map(|row| CustomCommand {
            name: row["name"].as_str().expect("name").to_owned(),
            description: row["description"].as_str().expect("description").to_owned(),
            enabled: row["enabled"].as_bool().unwrap_or(true),
        })
        .collect();
    assert!(
        !custom.is_empty(),
        "manifest must pin at least one representative dynamic command"
    );
    assert_eq!(
        published_names(&router, &custom),
        manifest_names("expected_with_example_dynamic_in_publish_order"),
        "dynamic custom commands must append after builtins in publish order",
    );
}

#[test]
fn automations_off_withholds_automation_builtins_and_all_dynamics() {
    let gates = RouterGates {
        automations: false,
        ..staging_gates()
    };
    let router = InteractionRouter::new(gates);
    let faq = CustomCommand {
        name: "faq".to_owned(),
        description: "example".to_owned(),
        enabled: true,
    };
    let names = published_names(&router, &[faq]);
    // 27 all-on builtins minus the 8 automation admin commands, and the
    // stored dynamic row stays unpublished: every such invocation refuses
    // at dispatch while gated off, so publishing would burn the ceiling.
    assert_eq!(names.len(), 19, "automations-off staging set: {names:?}");
    assert!(
        !names.iter().any(|name| name == "faq"),
        "dynamic rows must not publish while automations are off: {names:?}"
    );
    for gated in [
        "command",
        "command-remove",
        "command-list",
        "schedule",
        "schedule-remove",
        "schedule-list",
        "sticky",
        "sticky-remove",
    ] {
        assert!(
            !names.contains(&gated.to_owned()),
            "{gated} must not publish while automations are off"
        );
    }
}

#[test]
fn dynamic_rows_never_shadow_builtin_names() {
    // Dispatch matches builtins first and refuses the disabled row, so a
    // same-named dynamic row could never execute — publish withholds it,
    // matching dispatch precedence.
    let router = InteractionRouter::new(staging_gates());
    let shadow = CustomCommand {
        name: "ban".to_owned(),
        description: "shadow attempt".to_owned(),
        enabled: true,
    };
    let names = published_names(&router, &[shadow]);
    assert_eq!(
        names.iter().filter(|name| *name == "ban").count(),
        1,
        "the builtin keeps the name; the dynamic shadow stays out: {names:?}"
    );
}

fn voice_manifest() -> Value {
    manifest()["voice"].clone()
}

/// A manifest permission cell: JSON null is "no default permission gate".
fn permission_cell(cell: &Value) -> Option<String> {
    cell.as_str().map(str::to_owned)
}

fn voice_router(voice: bool, assistant: bool) -> InteractionRouter {
    InteractionRouter::new(RouterGates {
        voice,
        voice_assistant: assistant,
        ..staging_gates()
    })
}

#[test]
fn voice_set_matches_manifest_names_and_permissions() {
    let voice = voice_manifest();
    let rows = voice["published_after_builtins"]
        .as_array()
        .expect("manifest voice rows");
    let mut expected = manifest_names("builtins_in_publish_order");
    expected.extend(
        rows.iter()
            .map(|row| row["name"].as_str().expect("voice row name").to_owned()),
    );
    let set = voice_router(true, false)
        .publish_set(&[])
        .expect("staging voice set assembles");
    let names: Vec<String> = set.iter().map(|def| def.name.clone()).collect();
    assert_eq!(
        names, expected,
        "router-advertised voice set drifted from the manifest: update the fixture \
         from publish_set output, never hand-edit names",
    );
    for row in rows {
        let name = row["name"].as_str().expect("voice row name");
        let def = set
            .iter()
            .find(|def| def.name == name)
            .unwrap_or_else(|| panic!("/{name} missing from the voice publish set"));
        assert_eq!(
            def.default_member_permissions,
            permission_cell(&row["default_member_permissions"]),
            "/{name} default permissions drifted from the manifest",
        );
        assert!(!def.dm_permission, "/{name} must stay guild-only");
    }
}

#[test]
fn voice_kick_is_shadowed_by_moderation_kick() {
    let voice = voice_manifest();
    let shadowed: Vec<&str> = voice["shadowed_by_first_wins"]
        .as_array()
        .expect("manifest shadowed list")
        .iter()
        .map(|name| name.as_str().expect("shadowed name"))
        .collect();
    assert_eq!(shadowed, ["kick"]);
    let set = voice_router(true, true)
        .publish_set(&[])
        .expect("staging voice set assembles");
    for name in shadowed {
        let defs: Vec<_> = set.iter().filter(|def| def.name == name).collect();
        assert_eq!(defs.len(), 1, "/{name} must publish exactly once");
        assert_eq!(
            defs[0].default_member_permissions,
            permission_cell(&voice["shadow_winner_default_member_permissions"]),
            "/{name} must keep the moderation shape, not the voice one",
        );
        assert!(defs[0].options.iter().any(|option| option.name == "target"));
    }
}

#[test]
fn templateassistant_publishes_last_only_with_both_gates() {
    let voice = voice_manifest();
    let assistant = &voice["assistant"];
    let name = assistant["name"].as_str().expect("assistant name");
    for (voice_gate, assistant_gate) in [(false, false), (false, true), (true, false)] {
        let set = voice_router(voice_gate, assistant_gate)
            .publish_set(&[])
            .expect("staging set assembles");
        assert!(
            !set.iter().any(|def| def.name == name),
            "/{name} must not publish with voice={voice_gate} assistant={assistant_gate}",
        );
    }
    let set = voice_router(true, true)
        .publish_set(&[])
        .expect("staging set assembles");
    let last = set.last().expect("non-empty publish set");
    assert_eq!(last.name, name, "/{name} publishes after the voice set");
    assert_eq!(
        last.default_member_permissions,
        permission_cell(&assistant["default_member_permissions"]),
    );
}

#[test]
fn voice_gate_off_keeps_the_voice_names_out_of_the_staging_set() {
    let set = voice_router(false, true)
        .publish_set(&[])
        .expect("staging set assembles");
    let names: Vec<String> = set.iter().map(|def| def.name.clone()).collect();
    assert_eq!(names, manifest_names("builtins_in_publish_order"));
}
