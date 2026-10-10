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
use two_bot_core::ModerationAction;
use two_bot_discord::internal_channel_moderation::InternalChannelRequest;

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
    check_channel_moderation(body);
}

/// Channel-moderation verbs (`moderation.purge`, `moderation.slowmode`,
/// `moderation.lockdown`, `moderation.unlock`): snowflake actor/channel
/// identities, audit reason, purge `count` 1–100 and slowmode `seconds`
/// 0–6h. The receiver refuses before any channel effect unless every clause
/// holds, so the real receiver verdict (`InternalChannelRequest::from_body`)
/// must equal the independent oracle on every arbitrary body; any divergence
/// (or panic) is a fuzz failure, filed as a fix card per the out-of-scope rule.
const CHANNEL_ACTIONS: [ModerationAction; 4] = [
    ModerationAction::Purge,
    ModerationAction::Slowmode,
    ModerationAction::Lockdown,
    ModerationAction::Unlock,
];

/// Present numerics must be JSON integers: `"10"`, `1.5` and booleans are
/// malformed, never coerced. Oracle-only: the production verdict below calls
/// the real receiver, so this helper must not run on the production side
/// (it would make the `assert_eq!` vacuous).
fn numeric_fields_are_integers(body: &Map<String, Value>) -> bool {
    ["duration_seconds", "count", "seconds"]
        .iter()
        .all(|field| match body.get(*field) {
            None => true,
            Some(value) => value.as_i64().is_some(),
        })
}

/// Production verdict for one channel verb: the real receiver seam. Any
/// regression in `InternalChannelRequest::from_body` (canonical-id
/// tightening, integer coercion, new clauses) fails this fuzzer.
fn channel_body_accepts(action: ModerationAction, body: &Map<String, Value>) -> bool {
    InternalChannelRequest::from_body(action.action_name(), body).is_ok()
}

fn oracle_canonical_id(value: &Value) -> bool {
    match value.as_str() {
        Some(text) => {
            let bytes = text.as_bytes();
            (17..=20).contains(&bytes.len())
                && bytes.iter().all(|b| b.is_ascii_digit())
                && text
                    .parse::<u64>()
                    .is_ok_and(|id| id != 0 && id.to_string() == text)
        }
        None => false,
    }
}

fn oracle_reason_ok(value: &Value) -> bool {
    match value.as_str() {
        // Trimmed, non-empty, at most 512 UTF-16 code units (astral
        // characters cost 2, exactly like the production rule).
        Some(text) => {
            let trimmed = text.trim();
            !trimmed.is_empty() && trimmed.encode_utf16().count() <= 512
        }
        None => false,
    }
}

/// Independent refusal-vs-accept oracle: rebuilt from JSON primitives and
/// the documented bounds, never by calling the production validators, so
/// agreement with `channel_body_accepts` proves the bounds hold rather
/// than restating the implementation.
fn expect_channel_accept(action: ModerationAction, body: &Map<String, Value>) -> bool {
    let numbers_ok = match action {
        ModerationAction::Purge => {
            matches!(body.get("count").and_then(Value::as_i64), Some(n) if (1..=100).contains(&n))
        }
        ModerationAction::Slowmode => {
            matches!(body.get("seconds").and_then(Value::as_i64), Some(n) if (0..=21_600).contains(&n))
        }
        _ => true,
    };
    oracle_canonical_id(body.get("actor_id").unwrap_or(&Value::Null))
        && oracle_canonical_id(body.get("channel_id").unwrap_or(&Value::Null))
        && oracle_reason_ok(body.get("reason").unwrap_or(&Value::Null))
        && numbers_ok
        && numeric_fields_are_integers(body)
}

fn check_channel_moderation(body: &Map<String, Value>) {
    for action in CHANNEL_ACTIONS {
        assert_eq!(
            channel_body_accepts(action, body),
            expect_channel_accept(action, body),
            "channel-moderation verdict diverged for {}",
            action.action_name()
        );
    }
}

fn happy_channel_body(action: ModerationAction) -> Map<String, Value> {
    let mut body = Map::new();
    body.insert("actor_id".to_owned(), Value::from("111111111111111111"));
    body.insert("channel_id".to_owned(), Value::from("222222222222222222"));
    body.insert("reason".to_owned(), Value::from("spam"));
    match action {
        ModerationAction::Purge => {
            body.insert("count".to_owned(), Value::from(10));
        }
        ModerationAction::Slowmode => {
            body.insert("seconds".to_owned(), Value::from(30));
        }
        _ => {}
    }
    body
}

/// Fixed happy/bad-key matrix from the channel-moderation receiver tests:
/// every happy body accepts, each hostile key refuses, and a purge without
/// its required count refuses (no silent default). Runs once per campaign;
/// the seed corpus replays the same cases as fuzzer inputs every iteration.
fn check_fixed_channel_matrix() {
    for action in CHANNEL_ACTIONS {
        assert!(
            channel_body_accepts(action, &happy_channel_body(action)),
            "happy {} must accept",
            action.action_name()
        );
        assert!(
            expect_channel_accept(action, &happy_channel_body(action)),
            "oracle must agree on happy {}",
            action.action_name()
        );
    }
    let bad: [(ModerationAction, &str, Value); 12] = [
        (
            ModerationAction::Purge,
            "channel_id",
            Value::from("not-a-snowflake"),
        ),
        (
            ModerationAction::Slowmode,
            "channel_id",
            Value::from("00000000000000000"),
        ),
        (
            ModerationAction::Lockdown,
            "channel_id",
            Value::from("99999999999999999999"),
        ),
        (
            ModerationAction::Unlock,
            "channel_id",
            Value::from("022222222222222222"),
        ),
        (
            ModerationAction::Purge,
            "actor_id",
            Value::from("not-a-snowflake"),
        ),
        (ModerationAction::Purge, "count", Value::from(0)),
        (ModerationAction::Purge, "count", Value::from(101)),
        (ModerationAction::Purge, "count", Value::from("10")),
        (ModerationAction::Slowmode, "seconds", Value::from(21601)),
        (ModerationAction::Slowmode, "seconds", Value::from("30")),
        (ModerationAction::Lockdown, "reason", Value::from("")),
        (
            ModerationAction::Unlock,
            "reason",
            Value::from("x".repeat(513)),
        ),
    ];
    for (action, field, value) in bad {
        let mut body = happy_channel_body(action);
        body.insert(field.to_owned(), value);
        assert!(
            !channel_body_accepts(action, &body),
            "bad {} {field} must refuse",
            action.action_name()
        );
        assert!(
            !expect_channel_accept(action, &body),
            "oracle must agree on bad {} {field}",
            action.action_name()
        );
    }
    let mut missing = happy_channel_body(ModerationAction::Purge);
    missing.remove("count");
    assert!(
        !channel_body_accepts(ModerationAction::Purge, &missing),
        "purge without count must refuse"
    );
    assert!(
        !expect_channel_accept(ModerationAction::Purge, &missing),
        "oracle must agree on purge without count"
    );
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
    static MATRIX: std::sync::Once = std::sync::Once::new();
    MATRIX.call_once(check_fixed_channel_matrix);
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
