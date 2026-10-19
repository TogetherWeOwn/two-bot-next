#![no_main]

use libfuzzer_sys::fuzz_target;
use serde_json::Value;
use two_bot_core::twitch_helix::{parse_token_response, HELIX_MAX_BODY_BYTES};

/// Keep each JSON decode bounded while allowing a body above the transport cap
/// so the oversized-response corpus seed exercises the parser itself.
const MAX_INPUT_BYTES: usize = HELIX_MAX_BODY_BYTES * 2;
const NOW_MS: i64 = 1_700_000_000_000;
/// Mirrors the fail-closed token contract in `twitch_helix::parse_token_response`
/// (kept as literals so oracle and parser drift loudly, not silently):
/// `access_token` is 1..=1024 bytes of `0x21..=0x7E`, `expires_in` is a JSON
/// integer in `1..=31_536_000` with no coercion, and every top-level string
/// value fits the same 1024-byte bound.
const MAX_TOKEN_EXPIRES_IN_SECS: u64 = 31_536_000;
const MAX_TOKEN_STRING_BYTES: usize = 1024;

/// Independent oracle for the narrowed token contract: object-only body,
/// printable-ASCII token, integer-only expiry range, per-field size cap.
fn expected_valid(body: &Value) -> bool {
    let Some(obj) = body.as_object() else {
        return false;
    };
    let token_ok = match obj.get("access_token").and_then(Value::as_str) {
        Some(token) => {
            !token.is_empty()
                && token.len() <= MAX_TOKEN_STRING_BYTES
                && token.bytes().all(|byte| (0x21..=0x7E).contains(&byte))
        }
        None => false,
    };
    if !token_ok {
        return false;
    }
    let expiry_ok = match obj.get("expires_in") {
        Some(Value::Number(n)) if n.is_i64() => {
            let secs = n.as_i64().unwrap_or(0);
            (1..=MAX_TOKEN_EXPIRES_IN_SECS as i64).contains(&secs)
        }
        Some(Value::Number(n)) if n.is_u64() => {
            let secs = n.as_u64().unwrap_or(u64::MAX);
            (1..=MAX_TOKEN_EXPIRES_IN_SECS).contains(&secs)
        }
        // Missing, string/float/bool/null/array/object: no coercion.
        _ => false,
    };
    if !expiry_ok {
        return false;
    }
    for value in obj.values() {
        if let Some(text) = value.as_str() {
            if text.len() > MAX_TOKEN_STRING_BYTES {
                return false;
            }
        }
    }
    true
}

fn expected_token_and_expiry(body: &Value) -> Option<(&str, i64)> {
    let obj = body.as_object()?;
    let token = obj.get("access_token")?.as_str()?;
    let secs = match obj.get("expires_in")? {
        Value::Number(n) if n.is_i64() => n.as_i64()?,
        Value::Number(n) if n.is_u64() => i64::try_from(n.as_u64()?).ok()?,
        _ => return None,
    };
    Some((token, secs))
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }

    let Ok(body) = serde_json::from_slice::<Value>(data) else {
        return;
    };

    let expected_valid = expected_valid(&body);
    let result = parse_token_response(&body, NOW_MS);
    assert_eq!(
        result.is_ok(),
        expected_valid,
        "parser acceptance differs from the token response contract"
    );

    if let Ok(token) = result {
        let (access_token, expires_in) =
            expected_token_and_expiry(&body).expect("valid token response has a token and expiry");
        let ttl_ms = expires_in.saturating_mul(1_000);
        assert_eq!(token.token.expose(), access_token);
        assert_eq!(token.expires_at_ms, NOW_MS.saturating_add(ttl_ms));
        assert!(token.needs_refresh(token.expires_at_ms));
        assert_eq!(format!("{:?}", token.token), "[REDACTED]");
    }
});
