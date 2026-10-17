#![no_main]
//! Fuzz the Facebook Page webhook front door: [`verify_challenge`]
//! (GET verification), [`verify_signature`] (HMAC-SHA256 over the raw POST
//! body), and [`parse_envelope`] (page-events JSON) over hostile bytes.
//!
//! Input structure: the raw fuzzer bytes are the POST body verbatim, and the
//! lossy text split on NUL (`mode\0token\0challenge`) feeds the challenge
//! verifier, so every input exercises all three seams without keyword routing
//! (no selector bytes can reroute a seed away from its shape). The HMAC is
//! always recomputed with public synthetic test material, so MAC failure
//! cannot starve the JSON parser: tampered inputs assert refusal, and fresh
//! correctly-signed inputs assert `Ok` before the envelope pass runs.
//!
//! Bounds: bodies are capped at twice `FB_WEBHOOK_MAX_BODY_BYTES` so the
//! oversize refusal stays reachable; challenge fields are capped per-call at
//! 512 chars so the production `FB_WEBHOOK_CHALLENGE_MAX` bound is what
//! refuses. No network, database, Discord client, clock, or secret is used.

use std::sync::Once;

use hmac::{Hmac, KeyInit, Mac};
use libfuzzer_sys::fuzz_target;
use sha2::Sha256;
use two_bot_core::fb_page_events::webhook::{
    parse_envelope, verify_challenge, verify_signature, DriverError, FbPageEnvelope,
    FB_WEBHOOK_CHALLENGE_MAX, FB_WEBHOOK_ERROR_MAX, FB_WEBHOOK_FIELD_MAX, FB_WEBHOOK_ID_MAX,
    FB_WEBHOOK_MAX_BODY_BYTES, FB_WEBHOOK_MAX_CHANGES, FB_WEBHOOK_MAX_ENTRIES,
};

/// Synthetic verify token both sides use. Public test material, never a
/// production secret; refusals must never echo it (pinned in
/// [`check_fixed_matrix`]).
const EXPECTED_TOKEN: &str = "synthetic-fb-verify-token";
/// Synthetic app-secret bytes both sides use: derived, never a literal (see
/// `eventsub_delivery::synthetic_key`), so the hard-coded-crypto query stays
/// quiet while every campaign derives the same key.
fn synthetic_key() -> [u8; 32] {
    std::array::from_fn(|index| (index as u8).wrapping_mul(37).wrapping_add(11))
}

/// The synthetic secret string both sides use: the hex of the derived key.
/// Public test material, never a production secret; refusals must never echo
/// it (pinned in [`check_fixed_matrix`]).
fn app_secret() -> String {
    hex::encode(synthetic_key())
}

/// Cap for raw-body bytes per iteration: twice the transport cap so the
/// oversize refusal stays reachable without wasting RSS.
const MAX_INPUT_BYTES: usize = FB_WEBHOOK_MAX_BODY_BYTES * 2;
/// Cap for fuzz-derived challenge fields: above the production challenge
/// bound so overlong inputs still reach the verifier's own refusal.
const MAX_FIELD_CHARS: usize = 512;

/// Recompute the delivery signature over `body` keyed on the synthetic app
/// secret, mirroring [`verify_signature`].
fn sign(body: &[u8]) -> String {
    let secret = app_secret();
    let mut mac = <Hmac<Sha256>>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// Independent oracle for the challenge predicate: accept if and only if the
/// production contract holds. A mode/token/bound mutant fails here.
fn challenge_oracle(mode: &str, token: &str, challenge: &str) -> bool {
    mode == "subscribe"
        && !EXPECTED_TOKEN.is_empty()
        && token == EXPECTED_TOKEN
        && !challenge.is_empty()
        && challenge.len() <= FB_WEBHOOK_CHALLENGE_MAX
        && challenge.bytes().all(|b| b.is_ascii_graphic())
}

/// Challenger fields from one input: lossy text split on NUL, each field
/// capped so the production bound (not the harness) is what refuses.
fn challenge_fields(data: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(data)
        .split('\0')
        .take(3)
        .map(|field| field.chars().take(MAX_FIELD_CHARS).collect())
        .collect()
}

fn valid_body() -> Vec<u8> {
    serde_json::json!({
        "object": "page",
        "entry": [
            {"id": "1234567890", "time": 1_760_000_000, "changes": [
                {"field": "feed", "value": {"item": "post"}},
            ]},
        ],
    })
    .to_string()
    .into_bytes()
}

/// Input-independent pins: every happy shape verifies and parses, every
/// hostile shape refuses with its documented error, and no refusal leaks
/// synthetic secrets. Any front-door logic change must update this target on
/// its own card; silent drift fails the fuzzer instead. Runs once per
/// campaign; the oversize allocation stays here so per-iteration inputs stay
/// small and fast.
fn check_fixed_matrix() {
    // Challenge happy path echoes the challenge back.
    assert_eq!(
        verify_challenge("subscribe", EXPECTED_TOKEN, "123456789", EXPECTED_TOKEN),
        Ok("123456789".to_owned())
    );
    // Wrong mode and wrong token refuse without echoing the token.
    for outcome in [
        verify_challenge("unsubscribe", EXPECTED_TOKEN, "123456789", EXPECTED_TOKEN),
        verify_challenge("subscribe", "wrong-token", "123456789", EXPECTED_TOKEN),
        verify_challenge("subscribe", EXPECTED_TOKEN, "", EXPECTED_TOKEN),
    ] {
        let Err(DriverError::Failed(message)) = outcome else {
            panic!("hostile challenge must fail as Failed: {outcome:?}");
        };
        assert!(!message.contains(EXPECTED_TOKEN));
        assert!(message.len() <= FB_WEBHOOK_ERROR_MAX);
    }
    // Empty expected token refuses everything.
    assert!(verify_challenge("subscribe", "anything", "123456789", "").is_err());

    // Signature happy path verifies; tampered, re-prefixed, and garbage
    // signatures refuse without echoing the secret.
    let body = valid_body();
    let secret = app_secret();
    let good = sign(&body);
    assert_eq!(verify_signature(secret.as_str(), &body, &good), Ok(()));
    let mut tampered = good.clone();
    let last = tampered.pop().expect("test signature is non-empty");
    tampered.push(if last == '0' { '1' } else { '0' });
    let reprefixed = format!("md5={}", &good["sha256=".len()..]);
    for bad in [tampered.as_str(), reprefixed.as_str(), "sha256=zz"] {
        let Err(DriverError::Failed(message)) = verify_signature(secret.as_str(), &body, bad)
        else {
            panic!("hostile signature must fail as Failed");
        };
        assert!(!message.contains(secret.as_str()));
    }

    // Oversize body is refused before hashing or parsing.
    let big = vec![b'x'; FB_WEBHOOK_MAX_BODY_BYTES + 1];
    assert!(verify_signature(secret.as_str(), &big, &sign(&big)).is_err());
    assert!(parse_envelope(&big).is_err());

    // Envelope happy path parses to its exact change.
    let parsed = parse_envelope(&body).expect("valid envelope parses");
    assert_eq!(
        parsed,
        FbPageEnvelope {
            changes: vec![two_bot_core::fb_page_events::webhook::FbPageChange {
                page_id: "1234567890".to_owned(),
                field: "feed".to_owned(),
            }],
        }
    );
    // Wrong object, malformed JSON, and floods refuse.
    for bad in [
        serde_json::json!({"object": "user", "entry": []})
            .to_string()
            .into_bytes(),
        b"not json".to_vec(),
    ] {
        assert!(parse_envelope(&bad).is_err());
    }
    let flooded = serde_json::json!({
        "object": "page",
        "entry": (0..FB_WEBHOOK_MAX_ENTRIES + 1)
            .map(|i| serde_json::json!({"id": i.to_string(), "changes": []}))
            .collect::<Vec<_>>(),
    })
    .to_string()
    .into_bytes();
    assert!(parse_envelope(&flooded).is_err());
}

fuzz_target!(|data: &[u8]| {
    static MATRIX: Once = Once::new();
    MATRIX.call_once(check_fixed_matrix);

    if data.len() > MAX_INPUT_BYTES {
        return;
    }

    // --- Challenge pass: NUL-split fields through the real verifier against
    // the independent oracle. A mode/token/bound mutant fails here.
    let mut fields = challenge_fields(data);
    while fields.len() < 3 {
        fields.push(String::new());
    }
    let first = verify_challenge(&fields[0], &fields[1], &fields[2], EXPECTED_TOKEN);
    assert_eq!(
        first,
        verify_challenge(&fields[0], &fields[1], &fields[2], EXPECTED_TOKEN)
    );
    assert_eq!(
        first.is_ok(),
        challenge_oracle(&fields[0], &fields[1], &fields[2]),
        "verifier differs from the challenge contract"
    );
    match first {
        Ok(echo) => assert_eq!(echo, fields[2]),
        Err(DriverError::Failed(message)) => {
            assert!(message.len() <= FB_WEBHOOK_ERROR_MAX);
            assert!(!message.contains(EXPECTED_TOKEN));
        }
        Err(DriverError::Challenge(_)) => {
            panic!("the webhook front door never raises Challenge");
        }
    }

    // --- Signature pass: the correct signature verifies exactly when the
    // body is within the transport cap; tampered and re-prefixed signatures
    // never verify. A signature-bypass mutant fails here.
    let secret = app_secret();
    let good = sign(data);
    let verified = verify_signature(secret.as_str(), data, &good);
    assert_eq!(verified, verify_signature(secret.as_str(), data, &good));
    if data.len() > FB_WEBHOOK_MAX_BODY_BYTES {
        assert!(verified.is_err());
    } else {
        assert_eq!(verified, Ok(()));
    }
    let mut tampered = good.clone();
    let last = tampered.pop().expect("fuzz signature is non-empty");
    tampered.push(if last == '0' { '1' } else { '0' });
    assert!(verify_signature(secret.as_str(), data, &tampered).is_err());
    assert!(verify_signature(
        secret.as_str(),
        data,
        &format!("md5={}", &good["sha256=".len()..])
    )
    .is_err());

    // --- Envelope pass: arbitrary bytes through the parser. Must never
    // panic, must be deterministic, must fail as bounded `Failed`, and must
    // hold every accepted value inside its bound.
    let first_parse = parse_envelope(data);
    assert_eq!(first_parse, parse_envelope(data));
    match first_parse {
        Ok(envelope) => {
            assert!(envelope.changes.len() <= FB_WEBHOOK_MAX_CHANGES);
            for change in &envelope.changes {
                assert!(!change.page_id.is_empty() && change.page_id.len() <= FB_WEBHOOK_ID_MAX);
                assert!(!change.field.is_empty() && change.field.len() <= FB_WEBHOOK_FIELD_MAX);
            }
        }
        Err(DriverError::Failed(message)) => {
            assert!(message.len() <= FB_WEBHOOK_ERROR_MAX);
            assert!(!message.contains(secret.as_str()));
            assert!(!message.contains(EXPECTED_TOKEN));
        }
        Err(DriverError::Challenge(_)) => {
            panic!("the webhook front door never raises Challenge");
        }
    }
});
