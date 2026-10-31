#![no_main]

use libfuzzer_sys::fuzz_target;
use two_bot_core::self_roles::{parse_self_role_custom_id, self_role_custom_id, ParsedCustomId};

/// The wire namespace is a contract: live panels already carry these strings
/// in their components, so a rename strands every open panel. Pinned as a
/// literal rather than through any constant so drift fails the fuzzer.
const WIRE_PREFIX: &str = "two:self-role:";
/// Per-key bound mirrored from the private `valid_key` in `self_roles.rs`:
/// keys are 1..=60 bytes, first byte lower/digit, rest lower/digit/`_`/`-`.
const MAX_KEY_BYTES: usize = 60;
/// Discord caps real component ids at 100 chars; the harness deliberately
/// goes beyond it because the parser itself only enforces the per-key bound.
const MAX_INPUT_CHARS: usize = 512;

/// Independent copy of the private `valid_key`: a key the parser accepts
/// must satisfy this, and anything this rejects must parse to `None`.
fn expected_valid_key(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_KEY_BYTES {
        return false;
    }
    let first = bytes[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
}

/// Independent oracle for `parse_self_role_custom_id`: the exact namespace
/// prefix, then one or two colon-separated keys, each held to `valid_key`.
fn expected_parse(input: &str) -> Option<ParsedCustomId> {
    let rest = input.strip_prefix(WIRE_PREFIX)?;
    let mut pieces = rest.split(':');
    let panel_id = pieces.next()?;
    let option_key = pieces.next();
    if pieces.next().is_some() {
        return None;
    }
    if !expected_valid_key(panel_id) {
        return None;
    }
    if let Some(key) = option_key {
        if !expected_valid_key(key) {
            return None;
        }
    }
    Some(ParsedCustomId {
        panel_id: panel_id.to_owned(),
        option_key: option_key.map(str::to_owned),
    })
}

fn truncate_chars(s: &str, cap: usize) -> String {
    s.chars().take(cap).collect()
}

fuzz_target!(|data: &[u8]| {
    // Input-independent base cases. Any parser behavior change must update
    // this target on its own card; silent drift fails the fuzzer instead.
    assert_eq!(
        self_role_custom_id("games", Some("chess")),
        "two:self-role:games:chess"
    );
    assert_eq!(self_role_custom_id("games", None), "two:self-role:games");
    assert_eq!(
        parse_self_role_custom_id("two:self-role:games:chess"),
        Some(ParsedCustomId {
            panel_id: "games".to_owned(),
            option_key: Some("chess".to_owned()),
        })
    );
    assert_eq!(
        parse_self_role_custom_id("two:self-role:games"),
        Some(ParsedCustomId {
            panel_id: "games".to_owned(),
            option_key: None,
        })
    );
    // Round trip through the real builder for boundary keys.
    let panel60 = "p".repeat(MAX_KEY_BYTES);
    let key60 = "k".repeat(MAX_KEY_BYTES);
    assert_eq!(
        parse_self_role_custom_id(&self_role_custom_id(&panel60, Some(&key60))),
        Some(ParsedCustomId {
            panel_id: panel60.clone(),
            option_key: Some(key60.clone()),
        })
    );
    assert_eq!(
        parse_self_role_custom_id(&self_role_custom_id(&panel60, None)),
        Some(ParsedCustomId {
            panel_id: panel60.clone(),
            option_key: None,
        })
    );
    // Over-long (61 bytes) is rejected per `valid_key`, on either side.
    let long61 = "k".repeat(MAX_KEY_BYTES + 1);
    assert_eq!(
        parse_self_role_custom_id(&self_role_custom_id(&long61, Some("ok"))),
        None
    );
    assert_eq!(
        parse_self_role_custom_id(&self_role_custom_id("ok", Some(&long61))),
        None
    );
    assert_eq!(
        parse_self_role_custom_id(&format!("{WIRE_PREFIX}{long61}")),
        None
    );
    // Foreign namespaces, wrong case, leading space and multi-colon refuse.
    for forged in [
        "",
        "two:self-role:",
        "two:self-role:Games",
        "two:self-role:games:CHESS!",
        "two:self-role:a:b:c",
        "two:self-role:games:chess:",
        "two:self-role::chess",
        "two:self-role:games:",
        "other:games:chess",
        "two:self-role",
        "TWO:SELF-ROLE:games",
        " two:self-role:games",
        "two:lfg:abc123",
        "two:voice:kick-yes:99",
    ] {
        assert_eq!(parse_self_role_custom_id(forged), None, "{forged:?}");
    }

    // Discord strings are UTF-8. Invalid UTF-8 is discarded, matching the
    // other `&str` harnesses.
    let Ok(raw) = std::str::from_utf8(data) else {
        return;
    };
    // Bound the allocation to a function of the (libFuzzer-capped) input;
    // 512 chars still reaches every over-long and multi-colon shape.
    let input = truncate_chars(raw, MAX_INPUT_CHARS);

    // No database, Discord client, network or secret is touched below; only
    // the pure builder and parser run.
    assert_eq!(
        parse_self_role_custom_id(&input),
        parse_self_role_custom_id(&input),
        "parse is deterministic"
    );
    let parsed = parse_self_role_custom_id(&input);
    assert_eq!(
        parsed,
        expected_parse(&input),
        "oracle agrees with the parser"
    );
    // Anything outside the exact namespace returns `None`.
    if !input.as_bytes().starts_with(WIRE_PREFIX.as_bytes()) {
        assert_eq!(parsed, None, "foreign namespace is refused");
    }
    if let Some(rest) = input.strip_prefix(WIRE_PREFIX) {
        // Multi-colon inputs are rejected: at most one `:` may follow the
        // prefix (panel, plus one optional option key).
        if rest.split(':').count() > 2 {
            assert_eq!(parsed, None, "multi-colon input is refused");
        }
        // Over-long sides are rejected per `valid_key` (byte length, not
        // chars: multibyte UTF-8 only makes the byte count larger).
        let mut pieces = rest.split(':');
        let panel_id = pieces.next().unwrap_or("");
        let option_key = pieces.next();
        if panel_id.as_bytes().len() > MAX_KEY_BYTES {
            assert_eq!(parsed, None, "over-long panel id is refused");
        }
        if option_key.is_some_and(|k| k.as_bytes().len() > MAX_KEY_BYTES) {
            assert_eq!(parsed, None, "over-long option key is refused");
        }
    }
});
