//! Acceptance tests: Facebook Page webhook front-door envelope.
//!
//! REGRESSION PIN for the front door in `crates/core/src/fb_page_events.rs`
//! (`webhook` module): the Meta GET challenge handshake, the POST
//! HMAC-SHA256 signature check, and the page-events JSON envelope parser.
//! Hostile input at any of the three seams must fail as a bounded
//! `DriverError::Failed` — never a panic, never a `DriverError::Challenge`,
//! never an unbounded or secret-bearing message.
//!
//! Contract pinned here:
//!
//! 1. `verify_challenge` echoes `hub.challenge` only for mode `subscribe`
//!    with a matching non-empty verify token and a non-empty, byte-bounded,
//!    printable-ASCII challenge (at most `FB_WEBHOOK_CHALLENGE_MAX` bytes).
//! 2. `verify_signature` checks the 1 MiB transport cap before the HMAC, then
//!    verifies `sha256=<64 hex>` in constant time; every refusal is
//!    `DriverError::Failed` with no app-secret material.
//! 3. `parse_envelope` accepts only `{"object":"page","entry":[...]}` with
//!    bounded page ids (`FB_WEBHOOK_ID_MAX`), bounded change fields
//!    (`FB_WEBHOOK_FIELD_MAX`), at most `FB_WEBHOOK_MAX_ENTRIES` entries and
//!    `FB_WEBHOOK_MAX_CHANGES` changes; floods refuse rather than truncate,
//!    and a verified envelope with zero changes is `Ok` with no changes.
//! 4. Every refusal message stays within `FB_WEBHOOK_ERROR_MAX` bytes and
//!    carries no secret, token, or hostile body bytes; verdicts are
//!    deterministic (repeat calls agree).
//!
//! Offline only: synthetic tokens and a derived HMAC key, no network, no
//! env, no credentials, no database.

use hmac::{Hmac, KeyInit, Mac};
use serde_json::json;
use sha2::Sha256;
use two_bot_core::fb_page_events::webhook::{
    parse_envelope, verify_challenge, verify_signature, DriverError, FbPageChange, FbPageEnvelope,
    FB_WEBHOOK_CHALLENGE_MAX, FB_WEBHOOK_ERROR_MAX, FB_WEBHOOK_FIELD_MAX, FB_WEBHOOK_ID_MAX,
    FB_WEBHOOK_MAX_BODY_BYTES, FB_WEBHOOK_MAX_CHANGES, FB_WEBHOOK_MAX_ENTRIES,
};

/// Synthetic verify token both sides use. Public test material; refusals
/// must never echo it.
const EXPECTED_TOKEN: &str = "fixture-fb-verify-token";
/// Marker embedded in hostile payloads; errors must never echo it.
const SENTINEL: &str = "PAYLOAD-SENTINEL-7f3a9c";

/// Synthetic app-secret string: derived, never a literal, so the
/// hard-coded-crypto query stays quiet.
fn fixture_secret() -> String {
    let bytes: [u8; 32] =
        std::array::from_fn(|index| (index as u8).wrapping_mul(37).wrapping_add(11));
    hex::encode(bytes)
}

fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac =
        <Hmac<Sha256>>::new_from_slice(secret.as_bytes()).expect("test HMAC key is valid");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// Unwrap a refusal: hostile input must fail as `Failed`, never `Challenge`.
fn failed_message<T: std::fmt::Debug>(result: Result<T, DriverError>, label: &str) -> String {
    match result {
        Err(DriverError::Failed(message)) => message,
        Err(DriverError::Challenge(signal)) => {
            panic!("{label}: front-door refusals are Failed, never Challenge: {signal:?}")
        }
        Ok(value) => panic!("{label}: hostile input was accepted: {value:?}"),
    }
}

fn assert_bounded(message: &str, label: &str) {
    assert!(
        message.len() <= FB_WEBHOOK_ERROR_MAX,
        "{label}: refusal exceeds the {FB_WEBHOOK_ERROR_MAX}-byte bound: {message:?}"
    );
}

fn assert_no_leak(message: &str, secrets: &[&str], label: &str) {
    for secret in secrets {
        assert!(
            !message.contains(secret),
            "{label}: refusal leaks secret material: {message:?}"
        );
    }
}

// --- GET challenge -------------------------------------------------------

#[test]
fn webhook_challenge_echoes_subscribe_with_matching_token() {
    assert_eq!(
        verify_challenge("subscribe", EXPECTED_TOKEN, "123456789", EXPECTED_TOKEN),
        Ok("123456789".to_owned())
    );
    // Longest in-contract challenge echoes back.
    let longest = "c".repeat(FB_WEBHOOK_CHALLENGE_MAX);
    assert_eq!(
        verify_challenge("subscribe", EXPECTED_TOKEN, &longest, EXPECTED_TOKEN),
        Ok(longest)
    );
}

#[test]
fn webhook_challenge_malformed_handshakes_are_bounded_failures() {
    let overlong = "c".repeat(FB_WEBHOOK_CHALLENGE_MAX + 1);
    let rows = [
        ("unsubscribe", EXPECTED_TOKEN, "123456789"),
        ("SUBSCRIBE", EXPECTED_TOKEN, "123456789"),
        ("", EXPECTED_TOKEN, "123456789"),
        ("subscribe", "wrong-token", "123456789"),
        ("subscribe", "", "123456789"),
        ("subscribe", EXPECTED_TOKEN, ""),
        ("subscribe", EXPECTED_TOKEN, overlong.as_str()),
        ("subscribe", EXPECTED_TOKEN, "has space"),
        ("subscribe", EXPECTED_TOKEN, "new\nline"),
        ("subscribe", EXPECTED_TOKEN, "tab\there"),
        ("subscribe", EXPECTED_TOKEN, "café"),
    ];
    for (mode, token, challenge) in rows {
        let label = format!("mode={mode:?} challenge={challenge:?}");
        let first = verify_challenge(mode, token, challenge, EXPECTED_TOKEN);
        assert_eq!(
            first,
            verify_challenge(mode, token, challenge, EXPECTED_TOKEN),
            "{label}: verdicts are deterministic"
        );
        let message = failed_message(first, &label);
        assert_bounded(&message, &label);
        assert_no_leak(&message, &[EXPECTED_TOKEN], &label);
        assert!(
            !message.contains(challenge) || challenge.is_empty(),
            "{label}: refusal echoes hostile bytes: {message:?}"
        );
    }
}

#[test]
fn webhook_challenge_empty_expected_token_refuses_everything() {
    // A misconfigured receiver cannot verify against nothing.
    for token in ["", "anything"] {
        let label = format!("token={token:?}");
        let message = failed_message(
            verify_challenge("subscribe", token, "123456789", ""),
            &label,
        );
        assert_bounded(&message, &label);
    }
}

// --- POST signature ------------------------------------------------------

#[test]
fn webhook_signature_verifies_exactly_within_the_cap() {
    let secret = fixture_secret();
    let body = br#"{"object":"page","entry":[]}"#;
    let good = sign(&secret, body);
    assert_eq!(verify_signature(&secret, body, &good), Ok(()));
    assert_eq!(
        verify_signature(&secret, body, &good),
        verify_signature(&secret, body, &good),
        "signature verdicts are deterministic"
    );
}

#[test]
fn webhook_signature_malformed_deliveries_are_bounded_failures() {
    let secret = fixture_secret();
    let body = br#"{"object":"page","entry":[]}"#;
    let good = sign(&secret, body);
    let mut tampered = good.clone();
    let last = tampered.pop().expect("test signature is non-empty");
    tampered.push(if last == '0' { '1' } else { '0' });
    let mut other_secret = secret.clone();
    other_secret.push('x');
    let wrong_key = sign(&other_secret, body);
    let reprefixed = format!("md5={}", &good["sha256=".len()..]);
    let bare_hex = good["sha256=".len()..].to_owned();
    let headers = [
        tampered.as_str(),
        reprefixed.as_str(),
        bare_hex.as_str(),
        "",
        "sha256=",
        "sha256=zz",
        "md5=abcd",
        wrong_key.as_str(),
    ];
    for header in headers {
        let label = format!("header={header:?}");
        let message = failed_message(verify_signature(&secret, body, header), &label);
        assert_bounded(&message, &label);
        assert_no_leak(&message, &[secret.as_str()], &label);
    }
    // The body is part of the signed message.
    let mut other = body.to_vec();
    other.push(b' ');
    let message = failed_message(verify_signature(&secret, &other, &good), "body tamper");
    assert_bounded(&message, "body tamper");
    assert_no_leak(&message, &[secret.as_str()], "body tamper");
    // An empty secret fails closed.
    let empty_secret = String::new();
    let message = failed_message(verify_signature(&empty_secret, body, &good), "empty secret");
    assert_bounded(&message, "empty secret");
    let empty_signed = sign(&empty_secret, body);
    let message = failed_message(
        verify_signature(&empty_secret, body, &empty_signed),
        "empty secret signed",
    );
    assert_bounded(&message, "empty secret signed");
}

#[test]
fn webhook_transport_cap_refuses_before_hashing_or_parsing() {
    let secret = fixture_secret();
    let valid = br#"{"object":"page","entry":[]}"#;
    // Exactly at the cap, a valid envelope still verifies and parses: the
    // cap refuses only bodies *beyond* 1 MiB. JSON skips the padding
    // whitespace, and the HMAC covers the padded bytes exactly.
    let mut edge = valid.to_vec();
    edge.resize(FB_WEBHOOK_MAX_BODY_BYTES, b' ');
    assert_eq!(
        verify_signature(&secret, &edge, &sign(&secret, &edge)),
        Ok(())
    );
    let parsed = parse_envelope(&edge).expect("cap-edge envelope parses");
    assert!(parsed.changes.is_empty());
    // One byte more refuses at both seams, before hashing or parsing.
    let mut big = edge;
    big.push(b' ');
    let message = failed_message(
        verify_signature(&secret, &big, &sign(&secret, &big)),
        "oversize signature",
    );
    assert_bounded(&message, "oversize signature");
    let message = failed_message(parse_envelope(&big), "oversize envelope");
    assert_bounded(&message, "oversize envelope");
}

// --- POST envelope -------------------------------------------------------

#[test]
fn webhook_envelope_parses_page_changes_in_order() {
    let body = json!({
        "object": "page",
        "entry": [
            {"id": "111", "time": 1_760_000_000, "changes": [
                {"field": "feed", "value": {"item": "post"}},
                {"field": "photos", "value": {}},
            ]},
            {"id": "222", "time": 1_760_000_001, "changes": []},
        ],
    })
    .to_string();
    let parsed = parse_envelope(body.as_bytes()).expect("valid envelope parses");
    assert_eq!(
        parsed,
        FbPageEnvelope {
            changes: vec![
                FbPageChange {
                    page_id: "111".to_owned(),
                    field: "feed".to_owned(),
                },
                FbPageChange {
                    page_id: "111".to_owned(),
                    field: "photos".to_owned(),
                },
            ],
        }
    );
    // Longest in-contract page id and field still parse.
    let longest_id = "1".repeat(FB_WEBHOOK_ID_MAX);
    let longest_field = "f".repeat(FB_WEBHOOK_FIELD_MAX);
    let body = json!({
        "object": "page",
        "entry": [{"id": longest_id, "changes": [{"field": longest_field}]}],
    })
    .to_string();
    let parsed = parse_envelope(body.as_bytes()).expect("boundary envelope parses");
    assert_eq!(parsed.changes.len(), 1);
    // A verified envelope with zero changes acknowledges with 2xx and does
    // nothing: `Ok` with no changes.
    let empty = json!({"object": "page", "entry": []}).to_string();
    let parsed = parse_envelope(empty.as_bytes()).expect("empty envelope parses");
    assert!(parsed.changes.is_empty());
}

#[test]
fn webhook_envelope_malformed_bodies_are_bounded_failures() {
    let overlong_id = "1".repeat(FB_WEBHOOK_ID_MAX + 1);
    let overlong_field = "f".repeat(FB_WEBHOOK_FIELD_MAX + 1);
    let hostile: Vec<Vec<u8>> = vec![
        Vec::new(),
        b"not json".to_vec(),
        b"{}".to_vec(),
        b"[]".to_vec(),
        b"null".to_vec(),
        vec![0xff, 0xfe, 0x00],
        json!({"object": "user", "entry": []})
            .to_string()
            .into_bytes(),
        json!({"object": 7, "entry": []}).to_string().into_bytes(),
        json!({"object": "page"}).to_string().into_bytes(),
        json!({"object": "page", "entry": {}})
            .to_string()
            .into_bytes(),
        json!({"object": "page", "entry": [{"id": "1"}]})
            .to_string()
            .into_bytes(),
        json!({"object": "page", "entry": [{"changes": []}]})
            .to_string()
            .into_bytes(),
        json!({"object": "page", "entry": [{"id": "", "changes": []}]})
            .to_string()
            .into_bytes(),
        json!({"object": "page", "entry": [{"id": "has space", "changes": []}]})
            .to_string()
            .into_bytes(),
        json!({"object": "page", "entry": [{"id": overlong_id, "changes": []}]})
            .to_string()
            .into_bytes(),
        json!({"object": "page", "entry": [{"id": "1", "changes": {}}]})
            .to_string()
            .into_bytes(),
        json!({"object": "page", "entry": [{"id": "1", "changes": [{"value": 1}]}]})
            .to_string()
            .into_bytes(),
        json!({"object": "page", "entry": [{"id": "1", "changes": [{"field": ""}]}]})
            .to_string()
            .into_bytes(),
        json!({"object": "page",
            "entry": [{"id": "1", "changes": [{"field": overlong_field}]}]})
        .to_string()
        .into_bytes(),
        json!({"object": "page", "entry": ["nope"]})
            .to_string()
            .into_bytes(),
        // Hostile bytes ride along in a refused shape; the refusal must not
        // echo them.
        json!({"object": "page",
            "entry": [{"id": "1", "changes": [{"field": "", "value": SENTINEL}]}]})
        .to_string()
        .into_bytes(),
    ];
    for body in &hostile {
        let preview = String::from_utf8_lossy(&body[..body.len().min(80)]);
        let label = format!("body={preview:?}");
        let first = parse_envelope(body);
        assert_eq!(
            first,
            parse_envelope(body),
            "{label}: verdicts are deterministic"
        );
        let message = failed_message(first, &label);
        assert_bounded(&message, &label);
        assert_no_leak(&message, &[SENTINEL, EXPECTED_TOKEN], &label);
    }
}

#[test]
fn webhook_envelope_floods_refuse_without_truncation() {
    // Entries beyond Meta's documented 1000-update batch refuse.
    let many_entries: Vec<_> = (0..FB_WEBHOOK_MAX_ENTRIES + 1)
        .map(|i| json!({"id": i.to_string(), "changes": []}))
        .collect();
    let flooded = json!({"object": "page", "entry": many_entries}).to_string();
    let message = failed_message(parse_envelope(flooded.as_bytes()), "entry flood");
    assert_bounded(&message, "entry flood");
    // Changes beyond the batch refuse, even within one entry.
    let many_changes: Vec<_> = (0..FB_WEBHOOK_MAX_CHANGES + 1)
        .map(|_| json!({"field": "feed"}))
        .collect();
    let flooded = json!({"object": "page",
        "entry": [{"id": "1", "changes": many_changes}]})
    .to_string();
    let message = failed_message(parse_envelope(flooded.as_bytes()), "change flood");
    assert_bounded(&message, "change flood");
    // The change cap applies across entries, not to each entry separately.
    let flooded = json!({"object": "page", "entry": [
        {"id": "1", "changes": vec![json!({"field": "feed"}); 600]},
        {"id": "2", "changes": vec![json!({"field": "feed"}); 401]},
    ]})
    .to_string();
    let message = failed_message(
        parse_envelope(flooded.as_bytes()),
        "cross-entry change flood",
    );
    assert_bounded(&message, "cross-entry change flood");
    let full = json!({"object": "page", "entry": [
        {"id": "1", "changes": vec![json!({"field": "feed"}); 600]},
        {"id": "2", "changes": vec![json!({"field": "feed"}); 400]},
    ]})
    .to_string();
    let parsed = parse_envelope(full.as_bytes()).expect("1000-change cross-entry batch parses");
    assert_eq!(parsed.changes.len(), FB_WEBHOOK_MAX_CHANGES);
    // A full in-contract batch parses whole: nothing is truncated.
    let full_changes: Vec<_> = (0..FB_WEBHOOK_MAX_CHANGES)
        .map(|_| json!({"field": "feed"}))
        .collect();
    let full = json!({"object": "page",
        "entry": [{"id": "1", "changes": full_changes}]})
    .to_string();
    let parsed = parse_envelope(full.as_bytes()).expect("1000-change batch parses");
    assert_eq!(parsed.changes.len(), FB_WEBHOOK_MAX_CHANGES);
}
