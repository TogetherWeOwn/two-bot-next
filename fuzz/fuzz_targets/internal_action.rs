#![no_main]

use std::collections::HashMap;
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use serde_json::{Map, Value};
use two_bot_core::internal_actions::{
    authorize, body_hash, check_setting_value_size, require_settings_key, sign,
    validate_announcement, validate_event_input, validate_guild_add_member, validate_role_assign,
    AuthHeaders, InternalFlags, KeyRing, NonceCache, SigningKey, TokenBuckets, NONCE_TTL_SECONDS,
    SKEW_SECONDS,
};

fn keys() -> &'static KeyRing {
    static KEYS: OnceLock<KeyRing> = OnceLock::new();
    KEYS.get_or_init(|| {
        KeyRing::new(vec![SigningKey {
            id: "fuzz".to_owned(),
            // Derived public test material, never an environment credential.
            secret: body_hash(b"public internal-action fuzz fixture")
                .into_bytes()
                .into(),
        }])
    })
}

fn validate_fields(body: &Map<String, Value>) {
    let ids = HashMap::from([("fuzz".to_owned(), "12345678901234567".to_owned())]);
    let _ = validate_event_input(body, &ids);
    let _ = validate_role_assign(body, &ids);
    let _ = validate_announcement(body, &ids);
    let _ = validate_guild_add_member(body);
    let _ = require_settings_key(body, "settings.set");
    if let Some(value) = body.get("value") {
        let _ = check_setting_value_size(value);
    }
}

fn check_request(headers: &AuthHeaders<'_>, raw: &[u8]) {
    let flags = InternalFlags::from_map(&HashMap::new());
    // Fresh state per request: accumulated nonce/rate-limit state must not
    // prevent the fuzzer reaching body decoding after the first iteration.
    let _ = authorize(
        headers,
        raw,
        keys(),
        &flags,
        true,
        true,
        SKEW_SECONDS,
        1_700_000_000_000,
        &mut NonceCache::new(NONCE_TTL_SECONDS),
        &mut TokenBuckets::new(),
        &mut two_bot_core::ClockGuard::new(),
    );
}

fuzz_target!(|data: &[u8]| {
    let timestamp = "1700000000";
    let digest = body_hash(data);
    let nonce = &digest[..32];
    let secret = body_hash(b"public internal-action fuzz fixture").into_bytes();
    let signature = sign(&secret, timestamp, nonce, data);
    assert!(keys().verify("fuzz", &signature, timestamp, nonce, data));
    assert!(!keys().verify("unknown", &signature, timestamp, nonce, data));
    let mut changed = data.to_vec();
    changed.push(0);
    assert!(!keys().verify("fuzz", &signature, timestamp, nonce, &changed));

    // Signing arbitrary bytes is essential: unsigned mutations would stop at
    // the MAC check and never exercise the production JSON/object parser.
    check_request(
        &AuthHeaders {
            key_id: "fuzz",
            timestamp,
            nonce,
            signature: &signature,
        },
        data,
    );
    if let Ok(Value::Object(body)) = serde_json::from_slice::<Value>(data) {
        validate_fields(&body);
    }

    let text = String::from_utf8_lossy(data);
    let mut fields = text.splitn(4, '\n');
    // Fall back to fuzzer-derived input, never a constant: a hardcoded nonce
    // both trips the nonce-reuse scanner and wastes the fuzzer on one value.
    let fuzzer_nonce: &str = &text;
    check_request(
        &AuthHeaders {
            key_id: fields.next().unwrap_or(""),
            timestamp: fields.next().unwrap_or(""),
            nonce: fields.next().unwrap_or(fuzzer_nonce),
            signature: fields.next().unwrap_or(""),
        },
        data,
    );
    // A correctly signed but arbitrary timestamp reaches freshness parsing.
    let signature = sign(&secret, &text, nonce, data);
    check_request(
        &AuthHeaders {
            key_id: "fuzz",
            timestamp: &text,
            nonce,
            signature: &signature,
        },
        data,
    );
});
