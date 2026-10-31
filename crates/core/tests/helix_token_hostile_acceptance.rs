//! Acceptance tests: hostile corpus for Helix token-response parsing.
//!
//! PROTECTED: the paired builder must satisfy these tests without editing
//! this file. Do not merge this file on its own while it is red.
//!
//! Contract for `parse_token_response(body, now_ms)`: the Twitch
//! client-credentials token endpoint is an untrusted parser seam. Its 2xx
//! body is small and fixed-shape, so anything outside that shape is
//! refused fail-closed with the existing fixed-vocabulary
//! `HelixError::BadResponse(&'static str)` — never coerced, never
//! echoed. Concretely, the builder must enforce:
//!
//! 1. The body is a JSON object (not null/array/string/number/bool).
//! 2. `access_token` is present, a string, 1..=1024 bytes, and every byte
//!    is printable non-space ASCII (`0x21..=0x7E`). Empty, whitespace-only,
//!    control/whitespace-bearing (Authorization-header injection shapes),
//!    DEL and non-ASCII tokens are refused; no stringification of numbers,
//!    bools, arrays or objects; field names are case-sensitive.
//! 3. `expires_in` is present and a JSON integer (no string/float/bool
//!    coercion, including integral floats like `3600.0`) in
//!    `1..=31_536_000` seconds (365 days; real Twitch app tokens live far
//!    below this). Zero, negatives and absurd values are refused.
//! 4. Every top-level string value in the object is at most 1024 bytes
//!    (covers `access_token`, `token_type` and any unknown field); other
//!    unknown fields of ordinary size stay ignored for forward
//!    compatibility.
//! 5. `expires_at_ms = now_ms + expires_in * 1000` never wraps or panics:
//!    on overflow the parse either refuses with `BadResponse` or
//!    saturates, but never yields an expiry before `now_ms` from valid
//!    positive input.
//! 6. No refusal (Display or Debug) contains any body content: no token
//!    material, no sentinel field values, and a bounded length. A parsed
//!    token stays inside `Secret` (`[REDACTED]` under Debug/Display).
//! 7. Valid behavior is byte-identical: a well-formed body parses to the
//!    same token and `expires_at_ms`, and `needs_refresh` flips exactly
//!    at `expires_at_ms - TOKEN_REFRESH_SKEW_MS`.
//!
//! Offline only: pure `serde_json::Value` inputs, no network, no env, no
//! credentials. Duplicate JSON keys are resolved by the JSON decoder
//! before this `Value`-level seam and are out of scope here.

use serde_json::{json, Value};
use two_bot_core::twitch_helix::{
    parse_token_response, AppToken, HelixError, TOKEN_REFRESH_SKEW_MS,
};
use two_bot_core::Secret;

/// Marker embedded in hostile payloads; errors must never echo it.
const SENTINEL: &str = "PAYLOAD-SENTINEL-7f3a9c";
/// Pinned clock for every parse, so expiry math is exact.
const NOW_MS: i64 = 1_700_000_000_000;
/// Contract bound from the module header: 365 days, in seconds.
const MAX_EXPIRES_IN_S: i64 = 31_536_000;
/// Contract bound from the module header: longest accepted string field.
const MAX_STRING_BYTES: usize = 1024;

/// A runtime-built token in the usual OAuth shape (no literal credential).
fn valid_token() -> String {
    format!("tok-{}-B7._~+/=", "a".repeat(24))
}

/// A well-formed token response carrying the usual ignored field.
fn valid_body() -> Value {
    json!({
        "access_token": valid_token(),
        "expires_in": 3600,
        "token_type": "bearer",
    })
}

/// Assert refusal with the fixed-vocabulary error and no body echo.
fn assert_bad_response(body: &Value, label: &str) {
    match parse_token_response(body, NOW_MS) {
        Err(err @ HelixError::BadResponse(_)) => {
            for rendered in [format!("{err}"), format!("{err:?}")] {
                assert!(
                    !rendered.contains(SENTINEL),
                    "{label}: error echoes body content: {rendered}"
                );
                assert!(
                    rendered.len() < 300,
                    "{label}: error is not bounded ({} bytes)",
                    rendered.len()
                );
            }
        }
        Err(other) => panic!("{label}: expected BadResponse, got {other:?}"),
        Ok(_) => panic!("{label}: hostile body was accepted"),
    }
}

// --- Valid behavior stays byte-identical ---------------------------------

#[test]
fn valid_token_response_parses_to_same_token_and_expiry() {
    let token = parse_token_response(&valid_body(), NOW_MS).expect("valid body must parse");
    assert_eq!(token.token.expose(), &valid_token());
    assert_eq!(token.expires_at_ms, NOW_MS + 3_600_000);
}

#[test]
fn valid_expiry_bounds_and_unknown_fields_are_accepted() {
    // Smallest positive expiry.
    let body = json!({"access_token": valid_token(), "expires_in": 1});
    let token = parse_token_response(&body, NOW_MS).expect("expires_in=1 must parse");
    assert_eq!(token.expires_at_ms, NOW_MS + 1_000);

    // Largest in-contract expiry (365 days).
    let body = json!({"access_token": valid_token(), "expires_in": MAX_EXPIRES_IN_S});
    let token = parse_token_response(&body, NOW_MS).expect("max expiry must parse");
    assert_eq!(token.expires_at_ms, NOW_MS + MAX_EXPIRES_IN_S * 1_000);

    // Longest in-contract token (exactly 1024 bytes).
    let longest = "b".repeat(MAX_STRING_BYTES);
    let body = json!({"access_token": longest, "expires_in": 3600});
    let token = parse_token_response(&body, NOW_MS).expect("1024-byte token must parse");
    assert_eq!(token.token.expose().len(), MAX_STRING_BYTES);

    // Unknown fields of ordinary size/shape stay ignored.
    let body = json!({
        "access_token": valid_token(),
        "expires_in": 3600,
        "token_type": "bearer",
        "scope": ["channel:read"],
        "future_field": {"nested": [1, 2, 3]},
        "another": null,
    });
    let token = parse_token_response(&body, NOW_MS).expect("extra fields must be ignored");
    assert_eq!(token.token.expose(), &valid_token());
}

#[test]
fn needs_refresh_flips_exactly_at_the_skew_boundary() {
    let token = parse_token_response(&valid_body(), NOW_MS).expect("valid body must parse");
    let exp = token.expires_at_ms;
    assert!(!token.needs_refresh(NOW_MS), "fresh token must not refresh");
    assert!(
        !token.needs_refresh(exp - TOKEN_REFRESH_SKEW_MS - 1),
        "one ms before the skew window must not refresh"
    );
    assert!(
        token.needs_refresh(exp - TOKEN_REFRESH_SKEW_MS),
        "entering the skew window must refresh"
    );
    assert!(token.needs_refresh(exp), "expired token must refresh");
    assert!(
        token.needs_refresh(exp + 60_000),
        "past expiry must refresh"
    );

    // Same boundary on a directly built token, independent of parsing.
    let manual = AppToken {
        token: Secret::new(valid_token()),
        expires_at_ms: 1_000_000,
    };
    assert!(!manual.needs_refresh(1_000_000 - TOKEN_REFRESH_SKEW_MS - 1));
    assert!(manual.needs_refresh(1_000_000 - TOKEN_REFRESH_SKEW_MS));

    // A 1-second token is always inside the 60s skew window.
    let body = json!({"access_token": valid_token(), "expires_in": 1});
    let short = parse_token_response(&body, NOW_MS).expect("expires_in=1 must parse");
    assert!(short.needs_refresh(NOW_MS));
}

#[test]
fn secret_hygiene_keeps_token_material_out_of_formatting() {
    let token = parse_token_response(&valid_body(), NOW_MS).expect("valid body must parse");
    let material = valid_token();
    for rendered in [
        format!("{:?}", token.token),
        format!("{}", token.token),
        format!("{token:?}"),
    ] {
        assert!(
            !rendered.contains(&material),
            "token material leaked into formatting: {rendered}"
        );
        assert!(
            rendered.contains("[REDACTED]"),
            "Secret must render as [REDACTED], got: {rendered}"
        );
    }
    assert_eq!(token.token.expose(), &material, "expose stays explicit");
}

// --- Hostile corpus: every case refused, nothing echoed -------------------

#[test]
fn non_object_bodies_are_refused() {
    for (label, body) in [
        ("null body", Value::Null),
        ("string body", json!(SENTINEL)),
        ("empty string body", json!("")),
        ("number body", json!(3600)),
        ("bool body", json!(true)),
        ("array body with sentinel", json!([SENTINEL])),
        (
            "array wrapping a valid object",
            json!([{"access_token": valid_token(), "expires_in": 3600}]),
        ),
    ] {
        assert_bad_response(&body, label);
    }
}

#[test]
fn missing_fields_are_refused() {
    for (label, body) in [
        ("empty object", json!({})),
        ("missing access_token", json!({"expires_in": 3600})),
        ("missing expires_in", json!({"access_token": valid_token()})),
        (
            "only an unrelated sentinel field",
            json!({"note": SENTINEL}),
        ),
        (
            "case-variant field names do not count",
            json!({"Access_Token": valid_token(), "Expires_In": 3600}),
        ),
        (
            "upper-case field names do not count",
            json!({"ACCESS_TOKEN": valid_token(), "EXPIRES_IN": 3600}),
        ),
    ] {
        assert_bad_response(&body, label);
    }
}

#[test]
fn wrong_typed_access_token_is_refused_without_coercion() {
    for (label, value) in [
        ("null token", Value::Null),
        ("numeric token", json!(123_456)),
        ("zero token", json!(0)),
        ("bool token", json!(true)),
        ("array token", json!([SENTINEL])),
        ("object token", json!({"value": SENTINEL})),
    ] {
        let body = json!({"access_token": value, "expires_in": 3600});
        assert_bad_response(&body, label);
    }
}

#[test]
fn empty_and_whitespace_tokens_are_refused() {
    for (label, tok) in [
        ("empty token", ""),
        ("spaces only", "   "),
        ("tabs/newlines only", "\t\r\n"),
        ("leading space", " abcdef"),
        ("trailing space", "abcdef "),
    ] {
        let body = json!({"access_token": tok, "expires_in": 3600});
        assert_bad_response(&body, label);
    }
}

#[test]
fn tokens_with_header_hostile_bytes_are_refused() {
    for (label, tok) in [
        ("inner space", "abc def".to_owned()),
        ("tab", "abc\tdef".to_owned()),
        ("CRLF injection", "abc\r\ndef".to_owned()),
        ("bare LF", "abc\ndef".to_owned()),
        ("NUL byte", "abc\u{0}def".to_owned()),
        ("DEL byte", "abc\u{7f}def".to_owned()),
        ("non-ASCII letter", "abc\u{e9}def".to_owned()),
        ("non-ASCII symbol", "abc\u{1f600}def".to_owned()),
    ] {
        let body = json!({"access_token": tok, "expires_in": 3600});
        assert_bad_response(&body, label);
    }
}

#[test]
fn oversized_access_token_is_refused_without_echo() {
    for (label, tok) in [
        ("1025-byte token", "c".repeat(MAX_STRING_BYTES + 1)),
        ("8192-byte token", "c".repeat(8192)),
        (
            "sentinel-prefixed oversized token",
            format!("{SENTINEL}{}", "c".repeat(4096)),
        ),
    ] {
        let body = json!({"access_token": tok, "expires_in": 3600});
        assert_bad_response(&body, label);
    }
}

#[test]
fn oversized_other_string_fields_are_refused() {
    let huge = "d".repeat(MAX_STRING_BYTES + 1);
    for (label, body) in [
        (
            "oversized token_type",
            json!({
                "access_token": valid_token(),
                "expires_in": 3600,
                "token_type": huge,
            }),
        ),
        (
            "oversized unknown string field",
            json!({
                "access_token": valid_token(),
                "expires_in": 3600,
                "comment": format!("{SENTINEL}{}", "d".repeat(4096)),
            }),
        ),
    ] {
        assert_bad_response(&body, label);
    }
}

#[test]
fn wrong_typed_expires_in_is_refused_without_coercion() {
    for (label, value) in [
        ("null expiry", Value::Null),
        ("string expiry", json!("3600")),
        ("sentinel string expiry", json!(SENTINEL)),
        ("integral float expiry", json!(3600.0_f64)),
        ("fractional float expiry", json!(3600.5_f64)),
        ("negative float expiry", json!(-1.0_f64)),
        ("bool expiry", json!(true)),
        ("array expiry", json!([3600])),
        ("object expiry", json!({"seconds": 3600})),
    ] {
        let body = json!({"access_token": valid_token(), "expires_in": value});
        assert_bad_response(&body, label);
    }
}

#[test]
fn degenerate_and_absurd_expiry_values_are_refused() {
    for (label, value) in [
        ("zero expiry", json!(0)),
        ("negative one", json!(-1)),
        ("negative hour", json!(-3600)),
        ("i64::MIN expiry", json!(i64::MIN)),
        ("one second over the cap", json!(MAX_EXPIRES_IN_S + 1)),
        ("ten-year expiry", json!(315_360_000)),
        ("i64::MAX expiry", json!(i64::MAX)),
        ("u64 beyond i64::MAX", json!(u64::MAX)),
    ] {
        let body = json!({"access_token": valid_token(), "expires_in": value});
        assert_bad_response(&body, label);
    }
}

#[test]
fn expiry_overflow_never_wraps_or_panics() {
    // Valid positive expiry plus a clock near i64::MAX: refuse or
    // saturate, but never wrap to an expiry in the past (and no panic).
    let body = json!({"access_token": valid_token(), "expires_in": 3600});
    let now = i64::MAX - 1_000;
    match parse_token_response(&body, now) {
        Err(HelixError::BadResponse(_)) => {}
        Ok(token) => assert!(
            token.expires_at_ms >= now,
            "expiry wrapped into the past: {}",
            token.expires_at_ms
        ),
        Err(other) => panic!("expected BadResponse or saturated Ok, got {other:?}"),
    }
}

#[test]
fn refusal_never_echoes_a_valid_token_from_the_same_body() {
    // The token is well-formed but the expiry is hostile: the error must
    // still not carry the (real, usable) token material anywhere.
    let material = format!("{SENTINEL}-{}", "e".repeat(24));
    let body = json!({"access_token": material, "expires_in": -5});
    match parse_token_response(&body, NOW_MS) {
        Err(err @ HelixError::BadResponse(_)) => {
            for rendered in [format!("{err}"), format!("{err:?}")] {
                assert!(
                    !rendered.contains(SENTINEL),
                    "error echoes token material: {rendered}"
                );
            }
        }
        Err(other) => panic!("expected BadResponse, got {other:?}"),
        Ok(_) => panic!("negative expiry was accepted"),
    }
}

#[test]
fn sentinel_bearing_bodies_are_refused_without_echo_in_every_field() {
    // Sentinel rides along in an unrelated field of otherwise varied
    // hostile shapes; assert_bad_response checks both renderings.
    for (label, body) in [
        (
            "hostile token plus sentinel note",
            json!({
                "access_token": "bad token",
                "expires_in": 3600,
                "note": SENTINEL,
            }),
        ),
        (
            "hostile expiry plus sentinel note",
            json!({
                "access_token": valid_token(),
                "expires_in": "3600",
                "note": SENTINEL,
            }),
        ),
        (
            "sentinel as the only extra alongside missing fields",
            json!({"note": SENTINEL, "expires_in": -1}),
        ),
    ] {
        assert_bad_response(&body, label);
    }
}
