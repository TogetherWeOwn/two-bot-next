#![no_main]

use libfuzzer_sys::fuzz_target;
use two_bot_core::automation_transfer::{
    max_import_entries, parse_import_document, ImportParseError,
};

/// Raw entry count behind the import envelope, when the envelope carries a
/// `commands` array at all. Envelope problems (bad version, wrong shape) have
/// no entry count: they fail the whole import before any planning.
fn raw_entry_count(body: &serde_json::Value) -> Option<usize> {
    match body {
        serde_json::Value::Array(entries) => Some(entries.len()),
        serde_json::Value::Object(map) => map
            .get("commands")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len),
        _ => None,
    }
}

fuzz_target!(|data: &[u8]| {
    // Arbitrary bytes: invalid UTF-8, truncated input and scalar JSON must
    // never panic; only a decoded JSON value reaches the import parser. The
    // decoder's default recursion limit stays enabled, so hostile nesting
    // fails closed here instead of exhausting the stack below.
    let body: serde_json::Value = match serde_json::from_slice(data) {
        Ok(body) => body,
        Err(_) => return,
    };
    let max = max_import_entries();
    assert!(max > 0, "import budget must leave room for custom commands");

    let parsed = parse_import_document(&body, max);
    // Deterministic: the same bytes parse the same way twice.
    assert_eq!(
        parse_import_document(&body, max).as_ref().ok(),
        parsed.as_ref().ok()
    );

    let Ok(import) = parsed else { return };
    let Some(raw) = raw_entry_count(&body) else {
        panic!("successful parse without a commands array");
    };
    // The envelope gate fires before any per-entry allocation, so a parsed
    // import never holds more entries than the budget allows.
    assert!(raw <= max, "oversized list parsed without TooManyEntries");
    // Entry-level problems are counted, never fatal: every raw entry is
    // either imported or counted invalid. MEE6 rows whose names clean to
    // nothing are dropped by translation on top, so only the upper bound
    // holds for that shape.
    assert!(import.entries.len() + import.invalid_entries <= raw);
    let is_own_format = matches!(
        &body,
        serde_json::Value::Object(map) if map.contains_key("version")
    );
    if is_own_format {
        // Own-format entries are kept one-for-one: valid or counted, with no
        // translation conflicts.
        assert_eq!(import.entries.len() + import.invalid_entries, raw);
        assert!(import.translation_conflicts.is_empty());
    }
    // Oversized entry lists fail bounded: one under the raw count refuses
    // with the exact count and bound, and never plans anything.
    if raw > 0 {
        assert!(
            matches!(
                parse_import_document(&body, raw - 1),
                Err(ImportParseError::TooManyEntries { count, max: bound })
                if count == raw && bound == raw - 1
            ),
            "raw {raw} entries accepted below their own bound"
        );
    }
});
