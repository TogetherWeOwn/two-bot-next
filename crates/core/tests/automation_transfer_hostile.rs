//! Hostile-input holds for the automations import parser.
//!
//! Pins the fuzzer's contract for [`two_bot_core::automation_transfer`] in
//! the normal test suite: arbitrary bytes never panic, hang, or over-allocate
//! through [`parse_import_document`], envelope problems fail the whole import
//! before any planning, entry-level problems are counted and never fatal, and
//! oversized entry lists fail bounded with the exact count and bound.
//! Synthetic inputs only: no Discord, network, database, or subprocess.

use serde_json::{json, Value};
use two_bot_core::automation_transfer::{
    max_import_entries, parse_import_document, ImportParseError,
};
use two_bot_core::custom_commands::{max_custom_commands, reserved_command_names};

/// Drive one decoded body through the parser and assert the fuzzer's
/// accounting invariants: determinism, the envelope gate before planning, and
/// entry problems counted rather than fatal.
fn assert_parser_holds(body: &Value) {
    let max = max_import_entries();
    let first = parse_import_document(body, max);
    // Deterministic: the same bytes parse the same way twice.
    assert_eq!(
        parse_import_document(body, max).as_ref().ok(),
        first.as_ref().ok()
    );
    let Ok(import) = first else { return };
    let raw = match body {
        Value::Array(entries) => entries.len(),
        Value::Object(map) => match map.get("commands").and_then(Value::as_array) {
            Some(entries) => entries.len(),
            None => panic!("successful parse without a commands array: {body:?}"),
        },
        _ => panic!("successful parse of a non-document: {body:?}"),
    };
    // The envelope gate fires before any per-entry allocation.
    assert!(raw <= max, "oversized list parsed without TooManyEntries");
    assert!(import.entries.len() + import.invalid_entries <= raw);
    if matches!(body, Value::Object(map) if map.contains_key("version")) {
        // Own-format entries are kept one-for-one: valid or counted invalid,
        // with no translation conflicts.
        assert_eq!(import.entries.len() + import.invalid_entries, raw);
        assert!(import.translation_conflicts.is_empty());
    }
    // Oversized entry lists fail bounded with the exact count and bound.
    if raw > 0 {
        assert!(
            matches!(
                parse_import_document(body, raw - 1),
                Err(ImportParseError::TooManyEntries { count, max: bound })
                if count == raw && bound == raw - 1
            ),
            "raw {raw} entries accepted below their own bound"
        );
    }
}

#[test]
fn hostile_byte_inputs_never_panic() {
    // Truncated, over-nested, scalar, and wrong-typed JSON: the decoder or the
    // envelope gate fails each one closed, and whatever decodes holds.
    let hostile: &[&[u8]] = &[
        b"",
        b"\x00\x01\x02\xff\xfe",
        b"{",
        b"[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]",
        b"null",
        b"42",
        b"\"just a string\"",
        b"true",
        b"[]",
        b"{}",
        br#"{"version":null,"commands":null}"#,
        br#"{"version":[],"commands":{}}"#,
        br#"{"version":1.5,"commands":[]}"#,
        br#"{"version":18446744073709551616,"commands":[]}"#,
        br#"{"version":"1","commands":[]}"#,
        br#"{"version":true,"commands":[]}"#,
        br#"{"version":1,"commands":null,"overwrite":[1]}"#,
        br#"{"version":1,"commands":[{"name":null}],"schedules":{}}"#,
        br#"[[[[]]],{"command":[]}]"#,
    ];
    for bytes in hostile {
        let Ok(body) = serde_json::from_slice::<Value>(bytes) else {
            continue;
        };
        assert_parser_holds(&body);
    }
    // Invalid UTF-8 never reaches the parser: decoding fails closed.
    assert!(serde_json::from_slice::<Value>(&[0xff, 0xfe, 0x00]).is_err());
}

#[test]
fn seed_corpus_inputs_hold() {
    // Every committed seed starts from a shape the parser must already hold.
    for seed in [
        json!({"version":1,"commands":[{"name":"faq","description":"MEE6 FAQ","template":"Read {server} rules","text_trigger":"!faq"},{"name":"welcome","description":"The welcome command","template":"hi {user}","text_trigger":null}],"overwrite":false}),
        json!([{"command":"faq","response":"Read {server} rules"},{"command":"FAQ","response":"second faq"},{"command":"welcome","description":"says hi","response":"hi {user}"},""]),
        json!({"commands":[{"command":"live","description":"MEE6 FAQ","response":"imported answer"}],"overwrite":true}),
        json!({"version":2,"commands":[]}),
        json!({"nope":true}),
        json!({"commands":[],"overwrite":"yes"}),
        json!({"version":1,"commands":[{"name":7,"template":null},{"name":"ok","template":"hi {user}","text_trigger":123},[1,2],"str",null,{"name":"","template":""},{"name":"café 👋","description":12345,"template":"{user} {bogus} {{nested}} {oops"}],"schedules":null}),
    ] {
        assert_parser_holds(&seed);
    }
    // The oversized seed (200 own-format entries) exceeds any guild budget:
    // guild commands cap at 100 total, so 200 entries always refuse bounded.
    let oversized: Value = serde_json::from_str(&format!(
        "{{\"version\":1,\"commands\":[{}]}}",
        (0..200)
            .map(|i| format!(
                "{{\"name\":\"bulk-{i:03}\",\"description\":\"Bulk\",\"template\":\"hi {{user}}\"}}"
            ))
            .collect::<Vec<_>>()
            .join(",")
    ))
    .expect("oversized seed decodes");
    assert!(matches!(
        parse_import_document(&oversized, max_import_entries()),
        Err(ImportParseError::TooManyEntries { count: 200, .. })
    ));
}

#[test]
fn envelope_problems_fail_before_planning() {
    let max = max_import_entries();
    // Bad version in every JSON shape refuses the whole import.
    for version in [
        json!(2),
        json!(0),
        json!("1"),
        json!(1.5),
        json!(true),
        json!(null),
        json!([]),
        json!({}),
    ] {
        let rendered = match &version {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        assert_eq!(
            parse_import_document(&json!({"version": version.clone(), "commands": []}), max),
            Err(ImportParseError::UnsupportedVersion(rendered)),
            "version shape {version} must fail the envelope"
        );
    }
    // Wrong envelope shapes refuse before any entry is touched, even when the
    // entries themselves would parse.
    let good_entry = json!({"name":"faq","description":"d","template":"t"});
    for body in [
        json!({"nope": true}),
        json!({"commands": {}}),
        json!({"commands": "faq"}),
        json!({"commands": [good_entry.clone()], "overwrite": "yes"}),
        json!({"commands": [good_entry.clone()], "overwrite": 1}),
        json!({"commands": [good_entry.clone()], "overwrite": null}),
        json!({"version": 1, "commands": {}}),
        json!({"version": 1, "commands": [good_entry.clone()], "schedules": [{"at": "soon"}]}),
        json!("faq"),
        json!(42),
        json!(null),
    ] {
        assert!(
            parse_import_document(&body, max).is_err(),
            "envelope must refuse: {body:?}"
        );
    }
    // Null and empty schedules are not schedules: commands still import.
    for schedules in [json!(null), json!([])] {
        let parsed = parse_import_document(
            &json!({"version": 1, "commands": [good_entry.clone()], "schedules": schedules}),
            max,
        )
        .expect("null/empty schedules import commands only");
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.invalid_entries, 0);
    }
}

#[test]
fn entry_problems_are_counted_never_fatal() {
    let max = max_import_entries();
    // Own format: seven hostile entries, one valid survivor; the count, not a
    // failure, carries the damage.
    let parsed = parse_import_document(
        &json!({"version": 1, "commands": [
            {"name": 7, "template": null},
            {"name": "ok", "template": "hi {user}", "text_trigger": 123},
            [1, 2],
            "str",
            null,
            {"name": "", "template": null},
            {"name": "fine", "template": "kept", "text_trigger": null},
        ]}),
        max,
    )
    .expect("entry problems never fail the import");
    assert_eq!(parsed.invalid_entries, 6);
    assert_eq!(
        parsed
            .entries
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>(),
        ["fine"]
    );
    // Missing optional fields default; explicit null trigger stays slash-only.
    assert_eq!(
        parsed.entries[0].description,
        "Imported via automations.import"
    );
    assert_eq!(parsed.entries[0].text_trigger, None);

    // MEE6 rows whose names clean to nothing are dropped by translation
    // without failing the import or counting as invalid entries.
    let parsed = parse_import_document(
        &json!([{"command": "!!!", "response": "a"}, {"command": "faq", "response": "b"}]),
        max,
    )
    .expect("uncleanable MEE6 names never fail the import");
    assert_eq!(parsed.invalid_entries, 0);
    assert_eq!(
        parsed
            .entries
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>(),
        ["faq"]
    );

    // MEE6 collision suffixes stay deterministic and slash-only past the first.
    let parsed = parse_import_document(
        &json!([{"command": "faq", "response": "one"}, {"command": "FAQ!", "response": "two"}]),
        max,
    )
    .expect("collisions translate");
    assert_eq!(parsed.translation_conflicts, ["faq"]);
    assert_eq!(parsed.entries[1].text_trigger, None);
}

#[test]
fn oversized_entry_lists_fail_bounded() {
    let max = max_import_entries();
    assert!(
        max > 0 && max < 100,
        "budget leaves room under the guild cap"
    );
    let entry =
        |i: usize| json!({"name": format!("bulk-{i:04}"), "description": "Bulk", "template": "t"});
    // Exactly the budget parses; one more refuses with the exact count.
    let fitting: Vec<Value> = (0..max).map(entry).collect();
    let parsed = parse_import_document(&json!({"version": 1, "commands": fitting}), max)
        .expect("exactly the budget fits");
    assert_eq!(parsed.entries.len() + parsed.invalid_entries, max);
    let overflowing: Vec<Value> = (0..max + 1).map(entry).collect();
    assert_eq!(
        parse_import_document(&json!({"version": 1, "commands": overflowing}), max),
        Err(ImportParseError::TooManyEntries {
            count: max + 1,
            max
        })
    );
    // The bound is the caller's, not the guild's: a zero budget refuses any entry.
    assert_eq!(
        parse_import_document(&json!([{"command": "faq", "response": "a"}]), 0),
        Err(ImportParseError::TooManyEntries { count: 1, max: 0 })
    );
    // Only published definitions consume slots; the unpublished assistant
    // still reserves its name without taking the legal 33rd custom slot.
    let published = two_bot_core::custom_commands::published_builtin_count();
    assert_eq!(published, 67);
    assert_eq!(max, 33, "67 published +33 custom =100");
    assert_eq!(max, max_custom_commands(published));
    assert!(reserved_command_names().contains("templateassistant"));
}

#[test]
fn hostile_strings_stay_bounded_and_deterministic() {
    let max = max_import_entries();
    // A megabyte template and a megabyte version string return promptly with
    // memory proportional to the input, never hanging or over-allocating.
    let big = "x".repeat(1 << 20);
    let parsed = parse_import_document(
        &json!({"version": 1, "commands": [{"name": "big", "template": big}]}),
        max,
    )
    .expect("large strings parse");
    assert_eq!(parsed.entries.len(), 1);
    assert_eq!(parsed.invalid_entries, 0);
    assert_eq!(
        parse_import_document(
            &json!({"version": 1, "commands": [{"name": "big", "template": big}]}),
            max
        )
        .expect("large strings re-parse")
        .entries,
        parsed.entries
    );
    // Deeply nested JSON fails closed at the decoder's recursion limit.
    let mut deep = String::from("null");
    for _ in 0..1024 {
        deep = format!("[{deep}]");
    }
    assert!(serde_json::from_str::<Value>(&deep).is_err());
    // Unicode survives the hostile matrix byte-for-byte where kept.
    let parsed = parse_import_document(
        &json!({"version": 1, "commands": [{"name": "uni", "template": "Olá 👋 {user} — 東京"}]}),
        max,
    )
    .expect("unicode parses");
    assert_eq!(parsed.entries[0].template, "Olá 👋 {user} — 東京");
}
