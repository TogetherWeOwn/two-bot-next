#![no_main]

use std::collections::HashMap;

use libfuzzer_sys::fuzz_target;
use serde_json::{Map, Value};
use two_bot_core::internal_actions::{
    assert_allowed, assert_private_bind, build_channel_keys, build_role_keys, is_implemented,
    is_private_address, is_snowflake, new_request_id, normalise_bind_host, parse_body_object,
    parse_keys, require_reason, require_settings_key, require_snowflake, require_timestamp,
    valid_event_key, valid_idempotency_key, valid_nonce_format, valid_settings_key_shape,
    validate_event_key, validate_idempotency_key, validate_moderation_numbers, within_skew,
    InternalFlags, MAX_BODY_BYTES, MAX_SETTING_KEY_LEN, REQUEST_ID_LEN,
};
use two_bot_core::ModerationAction;

// Independent oracles, written from the documented grammar and bounds rather
// than by calling the production validators, so agreement proves the rule
// holds instead of restating the implementation.

fn oracle_key_charset(text: &str) -> bool {
    text.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

fn oracle_snowflake(text: &str) -> bool {
    (17..=20).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_digit())
}

fn oracle_settings_key_shape(text: &str) -> bool {
    let bytes = text.as_bytes();
    (2..=MAX_SETTING_KEY_LEN).contains(&bytes.len())
        && bytes[0].is_ascii_uppercase()
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_')
}

fn oracle_skew(timestamp: &str, skew: u64, now: u64) -> bool {
    if timestamp.is_empty()
        || timestamp.len() > 15
        || !timestamp.bytes().all(|b| b.is_ascii_digit())
    {
        return false;
    }
    let sent: u64 = timestamp.parse().expect("at most 15 digits fit u64");
    now.abs_diff(sent) <= skew
}

/// Bound for the four verbs that carry a required integer; `None` for verbs
/// with no numeric check at this layer.
fn oracle_numbers_ok(
    action: ModerationAction,
    duration: Option<&Value>,
    count: Option<&Value>,
    seconds: Option<&Value>,
) -> bool {
    let between = |value: Option<&Value>, min: i64, max: i64| matches!(value.and_then(Value::as_i64), Some(n) if (min..=max).contains(&n));
    match action {
        ModerationAction::TempBan => between(duration, 60, 365 * 24 * 60 * 60),
        ModerationAction::Timeout => between(duration, 60, 28 * 24 * 60 * 60),
        ModerationAction::Purge => between(count, 1, 100),
        ModerationAction::Slowmode => between(seconds, 0, 6 * 60 * 60),
        _ => true,
    }
}

fn check_string_grammars(text: &str) {
    assert_eq!(
        valid_nonce_format(text),
        text.len() == 32 && text.bytes().all(|b| b.is_ascii_hexdigit())
    );
    assert_eq!(
        valid_idempotency_key(text),
        (8..=200).contains(&text.len()) && oracle_key_charset(text)
    );
    assert_eq!(
        valid_event_key(text),
        (1..=200).contains(&text.len()) && oracle_key_charset(text)
    );
    assert_eq!(is_snowflake(text), oracle_snowflake(text));
    assert_eq!(
        valid_settings_key_shape(text),
        oracle_settings_key_shape(text)
    );

    // The header-shaped refusal must agree with the boolean grammar.
    assert_eq!(
        validate_idempotency_key(Some(text), "event.upsert").is_ok(),
        valid_idempotency_key(text)
    );
    assert!(validate_idempotency_key(None, "event.upsert").is_err());
}

fn check_freshness(text: &str, knob: u16) {
    let now = 1_700_000_000;
    let skew = u64::from(knob);
    assert_eq!(within_skew(text, skew, now), oracle_skew(text, skew, now));
}

fn check_bind_and_keys(text: &str) {
    // A successful bind is the normalised private address, never a wildcard.
    // Not asserted: that the result parses as an IP. `[[::1]]` normalises
    // once to `[::1]`, and the private check normalises again, so the bound
    // string can keep one bracket pair.
    if let Ok(bound) = assert_private_bind(text) {
        assert_eq!(bound, normalise_bind_host(text));
        assert!(is_private_address(&bound));
        assert!(!matches!(bound.as_str(), "" | "0.0.0.0" | "::" | "*"));
    }
    // A hostname or garbage is never private: only parsed IP literals count.
    if normalise_bind_host(text)
        .parse::<std::net::IpAddr>()
        .is_err()
    {
        assert!(!is_private_address(text));
    }

    // Key maps fail closed: every accepted entry is a trimmed key plus a
    // canonical snowflake.
    for built in [build_role_keys(text), build_channel_keys(text)] {
        if let Ok(map) = built {
            for (key, id) in &map {
                assert!(!key.is_empty() && key.trim() == key);
                assert!(oracle_snowflake(id));
            }
        }
    }
    // Signing-key specs: ids are unique and non-empty, and the ring is never
    // empty on success.
    if let Ok(keys) = parse_keys(text) {
        assert!(!keys.is_empty());
        let mut seen = std::collections::HashSet::new();
        for key in &keys {
            assert!(!key.id.is_empty());
            assert!(seen.insert(key.id.clone()), "duplicate key id accepted");
        }
    }
}

fn check_body_fields(map: &Map<String, Value>) {
    for field in ["starts_at", "ends_at"] {
        if let Ok(normal) = require_timestamp(map, field) {
            // `toISOString()` shape: YYYY-MM-DDTHH:MM:SS.mmmZ, and stable when
            // fed back through the validator.
            let bytes = normal.as_bytes();
            assert_eq!(bytes.len(), 24, "{normal}");
            assert!(bytes[4] == b'-' && bytes[7] == b'-' && bytes[10] == b'T');
            assert!(bytes[13] == b':' && bytes[16] == b':' && bytes[19] == b'.');
            assert_eq!(bytes[23], b'Z');
            let mut again = Map::new();
            again.insert(field.to_owned(), Value::from(normal.clone()));
            assert_eq!(
                require_timestamp(&again, field).as_deref().ok(),
                Some(normal.as_str())
            );
        }
    }

    for field in ["actor_id", "channel_id", "member_id", "role_id"] {
        if let Ok(id) = require_snowflake(map, field) {
            assert!(oracle_snowflake(id));
        }
    }

    if let Some(reason) = map.get("reason") {
        if let Ok(text) = require_reason(reason) {
            assert!(!text.is_empty() && text.trim() == text);
            assert!(text.encode_utf16().count() <= 512);
        }
    }

    for action in ModerationAction::ALL {
        let duration = map.get("duration_seconds");
        let count = map.get("count");
        let seconds = map.get("seconds");
        assert_eq!(
            validate_moderation_numbers(action, duration, count, seconds).is_ok(),
            oracle_numbers_ok(action, duration, count, seconds),
            "numeric bounds diverged for {}",
            action.action_name()
        );
    }

    if let Ok(key) = validate_event_key(map) {
        assert!(valid_event_key(key));
    }
    let _ = require_settings_key(map, "settings.set");
}

fn check_allowlist(text: &str) {
    let flags = InternalFlags::from_map(&HashMap::new());
    let verdict = assert_allowed(text, &flags, true, true);
    if !is_implemented(text) {
        // Unknown actions are never runnable, whatever the flags say.
        assert!(verdict.is_err(), "unknown action accepted: {text:?}");
    }
    // Acceptance needs both the allowlist and an enabled flag.
    if verdict.is_ok() {
        assert!(is_implemented(text) && flags.enabled.contains(text));
    }
}

fn check_request_id(data: &[u8]) {
    let mut now = [0u8; 8];
    let mut rand = [0u8; 16];
    for (slot, byte) in now.iter_mut().zip(data) {
        *slot = *byte;
    }
    for (slot, byte) in rand.iter_mut().zip(data.iter().skip(8)) {
        *slot = *byte;
    }
    let id = new_request_id(u64::from_le_bytes(now), &rand);
    assert_eq!(id.len(), REQUEST_ID_LEN);
    // Crockford base-32: no I, L, O or U, and uppercase only.
    assert!(id.bytes().all(|b| b.is_ascii_digit()
        || (b.is_ascii_uppercase() && !matches!(b, b'I' | b'L' | b'O' | b'U'))));
    assert_eq!(id, new_request_id(u64::from_le_bytes(now), &rand));
}

fuzz_target!(|data: &[u8]| {
    // Raw bytes into the strict body parser: the size ceiling is absolute,
    // success means a single JSON object, and parsing is deterministic.
    match parse_body_object(data) {
        Ok(map) => {
            assert!(data.len() <= MAX_BODY_BYTES);
            assert_eq!(
                serde_json::from_slice::<Value>(data).ok(),
                Some(Value::Object(map.clone()))
            );
            assert!(parse_body_object(data).is_ok());
            check_body_fields(&map);
        }
        Err(_) => assert!(parse_body_object(data).is_err()),
    }

    // Each NUL-separated field (and the whole input) through the string
    // grammars; invalid UTF-8 is replaced, never dropped, to keep the
    // hostile bytes reachable.
    let text = String::from_utf8_lossy(data);
    let knob = u16::from(data.first().copied().unwrap_or(0)) << 4;
    for field in text.split('\0').take(8) {
        check_string_grammars(field);
        check_freshness(field, knob);
        check_bind_and_keys(field);
        check_allowlist(field);
    }
    check_string_grammars(&text);
    check_request_id(data);
});
