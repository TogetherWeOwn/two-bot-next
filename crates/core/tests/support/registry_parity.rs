use serde_json::{json, Value};
use std::collections::BTreeSet;
use two_bot_core::commands::OCCURRENCE_ID_MAX_CHARS;
use two_bot_core::router::{InteractionRouter, RouterGates};

pub const INTENTIONAL_DIFFERENCES: &[(&str, &str)] = &[
    ("rsvp-attendance", "docs/parity.md §1 #12 / #25"),
    ("rota-acknowledge", "docs/parity.md §1 #13 / §9 drop 1"),
    ("attendance", "docs/parity.md §1 #12 bound"),
];

pub fn all_on_router() -> InteractionRouter {
    InteractionRouter::new(RouterGates {
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
    })
}

pub fn legacy_snapshot() -> Value {
    serde_json::from_str(include_str!("../fixtures/legacy_registry.json"))
        .expect("frozen legacy registry JSON")
}

pub fn expected_registry() -> Value {
    let snapshot = legacy_snapshot();
    let mut commands = snapshot["commands"]
        .as_array()
        .expect("command array")
        .clone();
    assert_eq!(
        commands.len(),
        28,
        "full legacy registry, including collision"
    );
    assert_eq!(
        commands
            .iter()
            .filter(|c| c["name"] == "attendance")
            .count(),
        2,
        "do not silently dedupe the legacy attendance collision"
    );
    let rsvp = commands
        .iter_mut()
        .find(|c| c["name"] == "attendance" && c["options"][0]["name"] == "event-id")
        .expect(INTENTIONAL_DIFFERENCES[0].1);
    // The exception changes ONLY this name, never its options or permissions.
    rsvp["name"] = json!(INTENTIONAL_DIFFERENCES[0].0);
    // The bound exception advertises ONLY this max_length on the scorecard
    // `event-occurrence` option; every other field stays legacy-identical.
    let scorecard = commands
        .iter_mut()
        .find(|c| c["name"] == "attendance" && c["options"][0]["name"] == "event-occurrence")
        .expect(INTENTIONAL_DIFFERENCES[2].1);
    scorecard["options"][0]["max_length"] = json!(OCCURRENCE_ID_MAX_CHARS);
    let rota = commands
        .iter()
        .position(|c| c["name"] == INTENTIONAL_DIFFERENCES[1].0)
        .expect(INTENTIONAL_DIFFERENCES[1].1);
    commands.remove(rota);
    canonical_registry(json!(commands))
}

// Equivalent API defaults, not behavioural waivers. This is a GUILD bulk-set:
// absent dm_permission and false both remain guild-only. Preserve true so an
// accidental change is reported. Command order, option order, descriptions,
// all bounds, choices, permissions, and unknown fields are compared verbatim.
pub fn canonical_registry(mut commands: Value) -> Value {
    for command in commands
        .as_array_mut()
        .expect("publish JSON must be an array")
    {
        let obj = command.as_object_mut().expect("command object");
        obj.entry("type").or_insert(json!(1)); // Discord defaults to ChatInput.
        obj.entry("options").or_insert(json!([]));
        if obj.get("default_member_permissions") == Some(&Value::Null) {
            obj.remove("default_member_permissions");
        }
        if obj.get("dm_permission") == Some(&json!(false)) {
            obj.remove("dm_permission");
        }
        // Twilight sends a server-assigned version placeholder on bulk set.
        // No other non-default field (including a different version) is ignored.
        if obj.get("version") == Some(&json!("1")) {
            obj.remove("version");
        }
        for option in obj["options"].as_array_mut().expect("options array") {
            option
                .as_object_mut()
                .expect("option object")
                .entry("required")
                .or_insert(json!(false));
        }
    }
    commands
}

pub fn registry_diff(actual: Value) -> Vec<String> {
    let mut differences = Vec::new();
    diff(
        "/commands",
        Some(&expected_registry()),
        Some(&canonical_registry(actual)),
        &mut differences,
    );
    differences
}

pub fn assert_registry_parity(actual: Value) {
    let differences = registry_diff(actual);
    assert!(differences.is_empty(),
        "Unlisted registry drift (legacy - / next +). Only {INTENTIONAL_DIFFERENCES:?} are allowed:\n{}",
        differences.join("\n"));
}

fn diff(path: &str, expected: Option<&Value>, actual: Option<&Value>, out: &mut Vec<String>) {
    if expected == actual {
        return;
    }
    match (expected, actual) {
        (Some(Value::Object(left)), Some(Value::Object(right))) => {
            let keys: BTreeSet<_> = left.keys().chain(right.keys()).collect();
            for key in keys {
                diff(&format!("{path}/{key}"), left.get(key), right.get(key), out);
            }
        }
        (Some(Value::Array(left)), Some(Value::Array(right))) => {
            for i in 0..left.len().max(right.len()) {
                let label = left
                    .get(i)
                    .or_else(|| right.get(i))
                    .and_then(|v| v.get("name"))
                    .and_then(Value::as_str)
                    .map(|name| format!(" ({name})"))
                    .unwrap_or_default();
                diff(
                    &format!("{path}/{i}{label}"),
                    left.get(i),
                    right.get(i),
                    out,
                );
            }
        }
        _ => out.push(format!(
            "{path}\n  - {}\n  + {}",
            expected
                .map(Value::to_string)
                .unwrap_or_else(|| "<missing>".to_owned()),
            actual
                .map(Value::to_string)
                .unwrap_or_else(|| "<missing>".to_owned())
        )),
    }
}
