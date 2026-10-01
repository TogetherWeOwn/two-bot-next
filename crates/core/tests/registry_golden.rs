mod support {
    pub mod registry_parity;
}

use serde_json::{json, Value};
use support::registry_parity::{
    all_on_router, assert_registry_parity, canonical_registry, expected_registry, legacy_snapshot,
    registry_diff, INTENTIONAL_DIFFERENCES,
};

fn next_registry() -> Value {
    serde_json::to_value(all_on_router().publish_set(&[]).expect("publish set"))
        .expect("registry serializes")
}

#[test]
fn registry_matches_frozen_legacy_with_only_documented_exceptions() {
    assert_registry_parity(next_registry());
}

#[test]
fn fixture_and_exception_list_are_pinned_to_the_parity_matrix() {
    let snapshot = legacy_snapshot();
    let matrix = include_str!("../../../docs/parity.md");
    assert_eq!(snapshot["source"], "TogetherWeOwn/two-bot");
    assert_eq!(
        snapshot["revision"],
        "d5d1179348feb9157bcac8c875de9399d4f5c76a"
    );
    assert!(matrix.contains("`main` @ `d5d11793`"));
    let table = matrix
        .split("## Registry golden exceptions")
        .nth(1)
        .expect("documented exception table")
        .split("\n## ")
        .next()
        .unwrap();
    let rows: Vec<_> = table
        .lines()
        .filter(|line| line.starts_with("| `"))
        .collect();
    assert_eq!(rows.len(), INTENTIONAL_DIFFERENCES.len());
    for (name, reference) in INTENTIONAL_DIFFERENCES {
        assert!(
            rows.iter()
                .any(|row| row.contains(&format!("`{name}`")) && row.contains(reference)),
            "missing exception {name}: {reference}"
        );
    }
    assert!(matrix
        .lines()
        .any(|line| line.starts_with("| 13 |") && line.contains("**DROP**")));
    assert!(matrix
        .lines()
        .any(|line| line.starts_with("| 25 |") && line.contains("namespace")));
}

#[test]
fn diff_detects_each_kind_of_unlisted_drift_including_exception_bodies() {
    let mutations = [
        ("purge", "/options/0/min_value", json!(0)),
        ("purge", "/options/0/max_value", json!(101)),
        ("slowmode", "/options/0/min_value", json!(-1)),
        ("slowmode", "/options/0/max_value", json!(21601)),
        ("tempban", "/options/1/min_value", json!(59)),
        ("timeout", "/options/1/min_value", json!(59)),
        ("rank", "/options/0/name", json!("target")),
        ("rank", "/options/0/type", json!(3)),
        ("rank", "/options/0/required", json!(true)),
        ("ban", "/options/1/max_length", json!(513)),
        ("ban", "/default_member_permissions", json!("0")),
        ("rsvp", "/options/1/choices/0/value", json!("maybe")),
        ("feed-add", "/options/0/choices/0/name", json!("Atom")),
        ("attendance", "/default_member_permissions", json!("32")),
        ("rsvp-attendance", "/options/0/name", json!("event")),
        ("rsvp-attendance", "/name", json!("attendance")),
        ("leaderboard", "/description", json!("Changed copy")),
        ("leaderboard", "/dm_permission", json!(true)),
        ("leaderboard", "/type", json!(2)),
    ];
    for (name, pointer, replacement) in mutations {
        let mut actual = canonical_registry(next_registry());
        let command = actual
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|command| command["name"] == name)
            .unwrap();
        if pointer == "/dm_permission" {
            command["dm_permission"] = replacement;
        } else {
            *command.pointer_mut(pointer).expect("mutation field exists") = replacement;
        }
        let differences = registry_diff(actual);
        assert!(!differences.is_empty(), "missed {name}{pointer}");
        assert!(
            differences
                .join("\n")
                .contains(pointer.rsplit('/').next().unwrap()),
            "diff must identify the changed field: {differences:?}"
        );
    }

    let mut removed = next_registry();
    removed.as_array_mut().unwrap().remove(0);
    assert!(!registry_diff(removed).is_empty());
    let mut extra = next_registry();
    extra
        .as_array_mut()
        .unwrap()
        .push(json!({"name":"rota-acknowledge", "description":"revived"}));
    assert!(
        !registry_diff(extra).is_empty(),
        "the drop exception must not permit reintroduction"
    );
    let mut unknown = next_registry();
    unknown[0]["unexpected_field"] = json!(true);
    assert!(registry_diff(unknown)
        .join("\n")
        .contains("unexpected_field"));
    let mut reordered = next_registry();
    reordered[11]["options"].as_array_mut().unwrap().swap(0, 1); // /rsvp
    assert!(!registry_diff(reordered).is_empty());
}

#[test]
fn api_defaults_are_equivalent_but_non_default_values_are_not_discarded() {
    let mut actual = expected_registry();
    for command in actual.as_array_mut().unwrap() {
        command["version"] = json!("1");
        command["dm_permission"] = json!(false);
        if command.get("default_member_permissions").is_none() {
            command["default_member_permissions"] = Value::Null;
        }
    }
    assert_registry_parity(actual);
    let mut changed = next_registry();
    changed[0]["version"] = json!("2");
    assert!(registry_diff(changed).join("\n").contains("version"));
}
