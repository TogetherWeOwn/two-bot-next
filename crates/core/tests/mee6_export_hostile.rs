//! MEE6 export hostile-corpus acceptance for `parse_mee6_entry` (pure,
//! offline).
//!
//! Contract: an export row is untrusted input. `parse_mee6_entry` must
//! fail closed with `None` — never a half record — for wrong types,
//! missing or extra fields, ambiguous duplicate field spellings, huge
//! strings/arrays/objects, deeply nested values, empty required
//! strings, and non-object entries. A rejection carries no row content:
//! the `None` result must not echo any marker planted in the row.
//! The known field spellings (`command`/`name`,
//! `response`/`message`/`content`, optional `description`) keep parsing
//! exactly as today, including a missing or non-string `description`
//! defaulting to empty.
//!
//! Test files only; no source is touched here. Tests named `accept_*`
//! and the already-fail-closed rejection tests pass today and must stay
//! green. Tests named `reject_*` for extra fields, duplicate spellings,
//! huge values, oversized containers, empty required strings, and the
//! no-echo marker FAIL today: the parser currently returns `Some(..)`
//! for those rows. The builder makes them pass without editing this
//! file.
//!
//! Builder freedom, pinned by the gap between accept and reject sizes:
//! strings at downstream sizes must parse (command 32 chars, response
//! 2000 chars, description 100 chars) and strings of 1,000,000 chars
//! must not; any per-field bound inside that gap satisfies this corpus.
//! Whitespace around otherwise non-empty command text is not pinned.
//!
//! Scope note: the API takes a `serde_json::Value`, so duplicate keys
//! in raw JSON text are already collapsed by the JSON parser before
//! this function runs and cannot be pinned at this layer. Deeply
//! nested *syntactic* JSON text is likewise bounded by the JSON
//! parser; nesting here is built programmatically as `Value`s.

use serde_json::{json, Map, Value};
use two_bot_core::{parse_mee6_entry, Mee6CommandInput};

const HUGE: usize = 1_000_000;

fn expected(command: &str, description: &str, response: &str) -> Option<Mee6CommandInput> {
    Some(Mee6CommandInput {
        command: command.to_owned(),
        description: description.to_owned(),
        response: response.to_owned(),
    })
}

/// Assert rejection and that nothing in the result echoes `marker`.
fn assert_none_no_echo(value: &Value, marker: &str) {
    let result = parse_mee6_entry(value);
    assert_eq!(result, None, "hostile row must fail closed: {value:.80}");
    assert!(
        !format!("{result:?}").contains(marker),
        "rejection must not echo row content"
    );
}

fn nest_arrays(depth: usize, leaf: Value) -> Value {
    let mut value = leaf;
    for _ in 0..depth {
        value = Value::Array(vec![value]);
    }
    value
}

fn nest_objects(depth: usize, leaf: Value) -> Value {
    let mut value = leaf;
    for _ in 0..depth {
        let mut map = Map::new();
        map.insert("a".to_owned(), value);
        value = Value::Object(map);
    }
    value
}

// ---------------------------------------------------------------------
// Accept shapes: green today, must stay green.
// ---------------------------------------------------------------------

#[test]
fn accept_known_spellings_all_combinations() {
    for name_key in ["command", "name"] {
        for response_key in ["response", "message", "content"] {
            let mut row = Map::new();
            row.insert(name_key.to_owned(), json!("faq"));
            row.insert(response_key.to_owned(), json!("read the docs"));
            assert_eq!(
                parse_mee6_entry(&Value::Object(row)),
                expected("faq", "", "read the docs"),
                "{name_key}/{response_key} must parse as today"
            );
        }
    }
}

#[test]
fn accept_description_variants_as_today() {
    // Present string description is kept.
    assert_eq!(
        parse_mee6_entry(&json!({"command": "faq", "response": "a", "description": "about"})),
        expected("faq", "about", "a")
    );
    // Empty string description stays empty.
    assert_eq!(
        parse_mee6_entry(&json!({"command": "faq", "response": "a", "description": ""})),
        expected("faq", "", "a")
    );
    // Missing, null, and non-string descriptions default to empty
    // (translation substitutes the standard default downstream).
    for description in [
        json!(null),
        json!(7),
        json!(true),
        json!(["x"]),
        json!({"x": 1}),
    ] {
        assert_eq!(
            parse_mee6_entry(
                &json!({"command": "faq", "response": "a", "description": description})
            ),
            expected("faq", "", "a"),
            "non-string description must keep defaulting to empty"
        );
    }
}

#[test]
fn accept_preserves_text_verbatim() {
    // Parsing does not clean, case-fold, trim inner text, or translate
    // placeholders; those steps belong to later stages.
    assert_eq!(
        parse_mee6_entry(&json!({
            "command": "Rules & FAQ!",
            "response": "  Olá 👋 {user} — 東京 {server}  ",
            "description": "MiXeD Case"
        })),
        expected(
            "Rules & FAQ!",
            "MiXeD Case",
            "  Olá 👋 {user} — 東京 {server}  "
        )
    );
}

#[test]
fn accept_at_downstream_sizes() {
    // The largest sizes the downstream stages keep: a 32-char command
    // name, a 2000-char template, a 100-char description.
    let row = json!({
        "command": "a".repeat(32),
        "response": "b".repeat(2000),
        "description": "c".repeat(100),
    });
    assert_eq!(
        parse_mee6_entry(&row),
        expected(&"a".repeat(32), &"c".repeat(100), &"b".repeat(2000))
    );
}

// ---------------------------------------------------------------------
// Already fail-closed today: regression pins, green throughout.
// ---------------------------------------------------------------------

#[test]
fn reject_non_object_entries() {
    for value in [
        json!(null),
        json!(true),
        json!(false),
        json!(0),
        json!(-1),
        json!(1.5),
        json!("faq"),
        json!(""),
        json!([]),
        json!([{"command": "faq", "response": "a"}]),
        json!([["command", "faq"]]),
    ] {
        assert_eq!(parse_mee6_entry(&value), None, "non-object: {value}");
    }
}

#[test]
fn reject_missing_required_fields() {
    for value in [
        json!({}),
        json!({"command": "faq"}),
        json!({"name": "faq"}),
        json!({"response": "a"}),
        json!({"message": "a"}),
        json!({"content": "a"}),
        json!({"description": "about"}),
        json!({"command": "faq", "description": "about"}),
        json!({"response": "a", "description": "about"}),
    ] {
        assert_eq!(parse_mee6_entry(&value), None, "missing field: {value}");
    }
}

#[test]
fn reject_wrong_type_required_fields() {
    let wrong = [
        json!(1),
        json!(1.5),
        json!(true),
        json!(null),
        json!([]),
        json!({}),
    ];
    for name_key in ["command", "name"] {
        for bad in &wrong {
            let mut row = Map::new();
            row.insert(name_key.to_owned(), bad.clone());
            row.insert("response".to_owned(), json!("a"));
            assert_eq!(
                parse_mee6_entry(&Value::Object(row)),
                None,
                "{name_key}={bad} must be None"
            );
        }
    }
    for response_key in ["response", "message", "content"] {
        for bad in &wrong {
            let mut row = Map::new();
            row.insert("command".to_owned(), json!("faq"));
            row.insert(response_key.to_owned(), bad.clone());
            assert_eq!(
                parse_mee6_entry(&Value::Object(row)),
                None,
                "{response_key}={bad} must be None"
            );
        }
    }
}

#[test]
fn reject_case_variant_and_padded_keys_as_only_spellings() {
    for value in [
        json!({"Command": "faq", "response": "a"}),
        json!({"COMMAND": "faq", "response": "a"}),
        json!({"Name": "faq", "response": "a"}),
        json!({"command ": "faq", "response": "a"}),
        json!({" command": "faq", "response": "a"}),
        json!({"command": "faq", "Response": "a"}),
        json!({"command": "faq", "MESSAGE": "a"}),
        json!({"command": "faq", "content\n": "a"}),
    ] {
        assert_eq!(parse_mee6_entry(&value), None, "lookalike key: {value}");
    }
}

#[test]
fn reject_null_spelling_does_not_fall_through_to_alias() {
    // A present-but-null spelling is a malformed row, not a cue to use
    // the alias spelling instead.
    assert_eq!(
        parse_mee6_entry(&json!({"command": null, "name": "faq", "response": "a"})),
        None
    );
    assert_eq!(
        parse_mee6_entry(&json!({"command": "faq", "response": null, "message": "a"})),
        None
    );
    assert_eq!(
        parse_mee6_entry(&json!({"command": "faq", "message": null, "content": "a"})),
        None
    );
}

#[test]
fn reject_deeply_nested_required_field_values() {
    assert_eq!(
        parse_mee6_entry(&json!({
            "command": nest_arrays(128, json!("faq")),
            "response": "a"
        })),
        None
    );
    assert_eq!(
        parse_mee6_entry(&json!({
            "command": "faq",
            "response": nest_objects(128, json!("a"))
        })),
        None
    );
    // The whole entry wrapped in layers is not a command-shaped object.
    assert_eq!(
        parse_mee6_entry(&nest_arrays(64, json!({"command": "faq", "response": "a"}))),
        None
    );
    assert_eq!(
        parse_mee6_entry(&nest_objects(
            64,
            json!({"command": "faq", "response": "a"})
        )),
        None
    );
}

// ---------------------------------------------------------------------
// RED: each test below fails today (parser returns Some) and must pass
// after the hardening fix, without editing this file.
// ---------------------------------------------------------------------

#[test]
fn reject_extra_unknown_fields() {
    for value in [
        json!({"command": "faq", "response": "a", "id": 1}),
        json!({"command": "faq", "response": "a", "enabled": true}),
        json!({"command": "faq", "response": "a", "extra": "x"}),
        json!({"name": "faq", "message": "a", "guild_id": "123"}),
        json!({"command": "faq", "content": "a", "description": "d", "aliases": []}),
        // A lookalike key next to the real spelling is an extra field,
        // not a harmless duplicate.
        json!({"command": "faq", "Command": "evil", "response": "a"}),
        json!({"command": "faq", "command ": "evil", "response": "a"}),
        json!({"command": "faq", "response": "a", "Response": "evil"}),
    ] {
        assert_eq!(parse_mee6_entry(&value), None, "extra field: {value}");
    }
}

#[test]
fn reject_duplicate_spellings_even_when_values_agree() {
    for value in [
        json!({"command": "faq", "name": "other", "response": "a"}),
        json!({"command": "faq", "name": "faq", "response": "a"}),
        json!({"command": "faq", "response": "a", "message": "b"}),
        json!({"command": "faq", "response": "a", "content": "b"}),
        json!({"command": "faq", "message": "a", "content": "b"}),
        json!({"command": "faq", "response": "a", "message": "a", "content": "a"}),
        json!({"command": "faq", "name": "faq", "response": "a", "message": "a"}),
    ] {
        assert_eq!(
            parse_mee6_entry(&value),
            None,
            "ambiguous spellings: {value}"
        );
    }
}

#[test]
fn reject_huge_strings() {
    let huge = "x".repeat(HUGE);
    for name_key in ["command", "name"] {
        let mut row = Map::new();
        row.insert(name_key.to_owned(), json!(huge.clone()));
        row.insert("response".to_owned(), json!("a"));
        assert!(
            parse_mee6_entry(&Value::Object(row)).is_none(),
            "huge {name_key} must be None"
        );
    }
    for response_key in ["response", "message", "content"] {
        let mut row = Map::new();
        row.insert("command".to_owned(), json!("faq"));
        row.insert(response_key.to_owned(), json!(huge.clone()));
        assert!(
            parse_mee6_entry(&Value::Object(row)).is_none(),
            "huge {response_key} must be None"
        );
    }
    assert!(
        parse_mee6_entry(&json!({
            "command": "faq", "response": "a", "description": huge
        }))
        .is_none(),
        "huge description must be None, not truncated into a record"
    );
}

#[test]
fn reject_oversized_containers() {
    // A huge array riding in an extra field.
    let big_array = Value::Array((0..10_000).map(|i| json!(i)).collect());
    assert_eq!(
        parse_mee6_entry(&json!({
            "command": "faq", "response": "a", "payload": big_array
        })),
        None
    );
    // Deeply nested value riding in an extra field.
    assert_eq!(
        parse_mee6_entry(&json!({
            "command": "faq",
            "response": "a",
            "payload": nest_arrays(256, json!("leaf"))
        })),
        None
    );
    // Thousands of extra keys around an otherwise valid row.
    let mut row = Map::new();
    row.insert("command".to_owned(), json!("faq"));
    row.insert("response".to_owned(), json!("a"));
    for i in 0..10_000 {
        row.insert(format!("k{i}"), json!(i));
    }
    assert_eq!(parse_mee6_entry(&Value::Object(row)), None);
    // A huge extra key name.
    let mut row = Map::new();
    row.insert("command".to_owned(), json!("faq"));
    row.insert("response".to_owned(), json!("a"));
    row.insert("k".repeat(HUGE), json!(1));
    assert_eq!(parse_mee6_entry(&Value::Object(row)), None);
}

#[test]
fn reject_empty_required_strings() {
    for value in [
        json!({"command": "", "response": "a"}),
        json!({"name": "", "response": "a"}),
        json!({"command": "faq", "response": ""}),
        json!({"command": "faq", "message": ""}),
        json!({"command": "faq", "content": ""}),
        // A whitespace-only command cleans to no invokable name; it is
        // a half record, not an entry.
        json!({"command": " \t\n", "response": "a"}),
    ] {
        assert_eq!(parse_mee6_entry(&value), None, "empty required: {value}");
    }
}

#[test]
fn reject_no_row_content_echo() {
    let marker = "HOSTILE-MARKER-7f3a9c";
    // Marker in a huge response.
    assert_none_no_echo(
        &json!({"command": "faq", "response": format!("{marker}{}", "x".repeat(HUGE))}),
        marker,
    );
    // Marker in an extra field.
    assert_none_no_echo(
        &json!({"command": "faq", "response": "a", "note": marker}),
        marker,
    );
    // Marker in the losing half of a duplicate-spelling row.
    assert_none_no_echo(
        &json!({"command": "faq", "name": marker, "response": "a"}),
        marker,
    );
    // Marker as a deeply nested extra-field leaf.
    assert_none_no_echo(
        &json!({
            "command": "faq",
            "response": "a",
            "payload": nest_objects(64, json!(marker))
        }),
        marker,
    );
}
