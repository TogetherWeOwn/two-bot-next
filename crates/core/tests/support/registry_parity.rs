use serde_json::{json, Value};
use std::collections::BTreeSet;
use two_bot_core::commands::{
    CommandDefinition, HELP_DESCRIPTION, MAX_RESOURCE_ID_CHARS, OCCURRENCE_ID_MAX_CHARS,
};
use two_bot_core::custom_commands::{
    MAX_COMMAND_NAME_CHARS, MAX_DESCRIPTION_CHARS, MAX_TEMPLATE_CHARS, MAX_TEXT_TRIGGER_CHARS,
};
use two_bot_core::feeds_http::MAX_FEED_SOURCE_BYTES;
use two_bot_core::lfg::{MAX_ROLE_SPEC_CHARS, MAX_TITLE_CHARS};
use two_bot_core::router::{InteractionRouter, RouterGates};
use two_bot_core::scheduled::MAX_BODY_CHARS;

pub const INTENTIONAL_DIFFERENCES: &[(&str, &str)] = &[
    ("rsvp-attendance", "docs/parity.md §1 #12 / #25"),
    ("rota-acknowledge", "docs/parity.md §1 #13 / §9 drop 1"),
    ("attendance", "docs/parity.md §1 #12 bound"),
    ("help", "docs/parity.md §1 help"),
    ("tempban", "docs/parity.md §1 #4 duration ceiling"),
    ("timeout", "docs/parity.md §1 #6 duration ceiling"),
    ("attendance", "docs/parity.md §1 #12 copy"),
    ("rsvp", "docs/parity.md §1 #24 copy"),
    ("rsvp-attendance", "docs/parity.md §1 #25 copy"),
    ("lfg", "docs/parity.md §1 #26 copy"),
    ("lfg-close", "docs/parity.md §1 #27 copy"),
    ("command", "docs/parity.md §1 #14 caps"),
    ("command-remove", "docs/parity.md §1 #15 caps"),
    ("schedule", "docs/parity.md §1 #17 caps"),
    ("schedule-remove", "docs/parity.md §1 #18 caps"),
    ("sticky", "docs/parity.md §1 #20 caps"),
    ("lfg", "docs/parity.md §1 #26 caps"),
    ("lfg-close", "docs/parity.md §1 #27 caps"),
    ("feed-add", "docs/parity.md §1 #28 caps"),
    ("feed-remove", "docs/parity.md §1 #29 caps"),
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
    // Duration exceptions add ONLY the service ceiling to each picker option;
    // preserve the frozen legacy payload and compare every other field strictly.
    for (name, max, reference) in [
        ("tempban", 31_536_000_i64, INTENTIONAL_DIFFERENCES[3].1),
        ("timeout", 2_419_200_i64, INTENTIONAL_DIFFERENCES[4].1),
    ] {
        let command = commands
            .iter_mut()
            .find(|c| c["name"] == name)
            .expect(reference);
        let duration = command["options"]
            .as_array_mut()
            .expect("legacy options array")
            .iter_mut()
            .find(|o| o["name"] == "duration_seconds")
            .expect(reference);
        assert_eq!(duration["min_value"], json!(60));
        assert!(duration["max_value"].is_null());
        duration["max_value"] = json!(max);
    }
    // Custom-command caps: advertise ONLY max_length matching the runtime
    // validators; every other field stays legacy-identical.
    for (name, option_name, max, reference) in [
        (
            "command",
            "name",
            MAX_COMMAND_NAME_CHARS,
            "docs/parity.md §1 #14 caps",
        ),
        (
            "command",
            "template",
            MAX_TEMPLATE_CHARS,
            "docs/parity.md §1 #14 caps",
        ),
        (
            "command",
            "description",
            MAX_DESCRIPTION_CHARS,
            "docs/parity.md §1 #14 caps",
        ),
        (
            "command",
            "text-trigger",
            MAX_TEXT_TRIGGER_CHARS,
            "docs/parity.md §1 #14 caps",
        ),
        (
            "command-remove",
            "name",
            MAX_COMMAND_NAME_CHARS,
            "docs/parity.md §1 #15 caps",
        ),
    ] {
        let command = commands
            .iter_mut()
            .find(|c| c["name"] == name)
            .expect(reference);
        let opt = command["options"]
            .as_array_mut()
            .expect("legacy options array")
            .iter_mut()
            .find(|o| o["name"] == option_name)
            .expect(reference);
        assert!(
            opt.get("max_length").is_none(),
            "legacy has no max_length on {name} {option_name}"
        );
        opt["max_length"] = json!(max);
    }
    let rota = commands
        .iter()
        .position(|c| c["name"] == INTENTIONAL_DIFFERENCES[1].0)
        .expect(INTENTIONAL_DIFFERENCES[1].1);
    commands.remove(rota);
    // Next-only `/help` discovery surface: no legacy counterpart. Insert at
    // the publish position (third, after the leveling core) as the exact
    // published definition, so parity pins its shape too.
    let help = serde_json::to_value(CommandDefinition::new("help", HELP_DESCRIPTION))
        .expect("help serializes");
    commands.insert(2, help);
    // Picker-copy exceptions (registry golden exceptions table): command and
    // option descriptions intentionally differ from legacy. Pointers mirror
    // the published option order.
    for (name, pointer, value) in [
        (
            "attendance",
            "/description",
            "Check in a verified human attendee for a scheduled event (scorecard)",
        ),
        (
            "attendance",
            "/options/0/description",
            "Event id (number in the event URL) or occurrence id, e.g. 12345 or weekly-standup-2026-10-03",
        ),
        (
            "rsvp",
            "/options/0/description",
            "Discord scheduled event id (number in the event URL), e.g. 12345",
        ),
        (
            "rsvp-attendance",
            "/description",
            "Show RSVP totals for a scheduled event",
        ),
        (
            "rsvp-attendance",
            "/options/0/description",
            "Discord scheduled event id (number in the event URL), e.g. 12345",
        ),
        (
            "lfg",
            "/options/1/description",
            "ISO-8601 start time, e.g. 2026-10-04T18:00:00Z",
        ),
        (
            "lfg",
            "/options/2/description",
            "Role slots as role:Label:count, comma-separated, e.g. tank:Tank:2,dps:DPS:6",
        ),
        (
            "lfg-close",
            "/options/0/description",
            "LFG id from the posted signup",
        ),
    ] {
        let command = commands
            .iter_mut()
            .find(|c| c["name"] == name)
            .expect("copy exception names a published command");
        *command.pointer_mut(pointer).expect("copy field exists") = json!(value);
    }
    // Further caps from the input-bound fix: advertise ONLY max_length
    // matching the runtime validators; the frozen legacy registry fixture
    // remains untouched. Custom-command caps are covered by the loop above.
    for (name, option_name, max_length, reference) in [
        (
            "schedule",
            "body",
            MAX_BODY_CHARS,
            "docs/parity.md §1 #17 caps",
        ),
        (
            "schedule-remove",
            "id",
            MAX_RESOURCE_ID_CHARS,
            "docs/parity.md §1 #18 caps",
        ),
        (
            "sticky",
            "body",
            MAX_BODY_CHARS,
            "docs/parity.md §1 #20 caps",
        ),
        (
            "lfg",
            "title",
            MAX_TITLE_CHARS,
            "docs/parity.md §1 #26 caps",
        ),
        (
            "lfg",
            "roles",
            MAX_ROLE_SPEC_CHARS,
            "docs/parity.md §1 #26 caps",
        ),
        (
            "lfg-close",
            "id",
            MAX_RESOURCE_ID_CHARS,
            "docs/parity.md §1 #27 caps",
        ),
        (
            "feed-add",
            "source",
            MAX_FEED_SOURCE_BYTES,
            "docs/parity.md §1 #28 caps",
        ),
        (
            "feed-remove",
            "id",
            MAX_RESOURCE_ID_CHARS,
            "docs/parity.md §1 #29 caps",
        ),
    ] {
        let _ = INTENTIONAL_DIFFERENCES
            .iter()
            .find(|(command, _)| *command == name)
            .expect("cap has a documented command exception");
        let command = commands
            .iter_mut()
            .find(|command| command["name"] == name)
            .expect(reference);
        let option = command["options"]
            .as_array_mut()
            .expect("command options array")
            .iter_mut()
            .find(|option| option["name"] == option_name)
            .expect(reference);
        assert!(
            option["max_length"].is_null(),
            "legacy {name}/{option_name}"
        );
        option["max_length"] = json!(max_length);
    }
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
