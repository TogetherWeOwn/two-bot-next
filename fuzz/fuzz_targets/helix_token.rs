#![no_main]

use libfuzzer_sys::fuzz_target;
use serde_json::Value;
use two_bot_core::twitch_helix::{parse_token_response, HELIX_MAX_BODY_BYTES};

/// Keep each JSON decode bounded while allowing a body above the transport cap
/// so the oversized-response corpus seed exercises the parser itself.
const MAX_INPUT_BYTES: usize = HELIX_MAX_BODY_BYTES * 2;
const NOW_MS: i64 = 1_700_000_000_000;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }

    let Ok(body) = serde_json::from_slice::<Value>(data) else {
        return;
    };

    let access_token = body
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let expires_in = body
        .get("expires_in")
        .and_then(Value::as_u64)
        .filter(|seconds| *seconds > 0);
    let expected_valid = access_token.is_some() && expires_in.is_some();
    let result = parse_token_response(&body, NOW_MS);
    assert_eq!(
        result.is_ok(),
        expected_valid,
        "parser acceptance differs from the token response contract"
    );

    if let Ok(token) = result {
        let access_token = access_token.expect("valid token response has a token");
        let expires_in = expires_in.expect("valid token response has an expiry");
        let ttl_ms = i64::try_from(expires_in.saturating_mul(1_000)).unwrap_or(i64::MAX);
        assert_eq!(token.token.expose(), access_token);
        assert_eq!(token.expires_at_ms, NOW_MS.saturating_add(ttl_ms));
        assert!(token.needs_refresh(token.expires_at_ms));
        assert_eq!(format!("{:?}", token.token), "[REDACTED]");
    }
});
