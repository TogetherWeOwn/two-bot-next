//! Acceptance tests: EventSub `classify` refuses duplicate JSON object keys.
//!
//! PROTECTED: the paired builder must satisfy these tests without editing
//! this file. Do not merge this file on its own while it is red.
//!
//! Contract for `classify(message_type, body)`: the HMAC-verified webhook
//! body is decoded with `serde_json::from_slice` into a `Value`, so repeated
//! object keys silently last-win today. After the fix, repeated keys at any
//! depth — conflicting values in both orders, identical values, nested
//! objects, escaped spellings of the same key, and unknown keys — are refused
//! with ONE fixed error whose `Display` contains `duplicate JSON object key`
//! (the Python refusal precedent: `production_deploy.py`,
//! `qa_cutover_probes.py`), and NO `Delivery` is produced: no challenge
//! echo, no go-live announce, no revocation reconcile. The refusal renders no
//! body bytes: neither `Display` nor `Debug` may contain hostile keys,
//! values, or bodies, and both stay bounded. Unique-key bodies classify
//! byte-identically, and pre-existing errors (`InvalidBody` for malformed
//! JSON, `WrongSubscription`) keep their own variants (refusal never
//! swallows a syntax error).
//!
//! Offline only: pure byte inputs, no network, no env, no credentials.
//! RED on the baseline: every refusal test fails with `Ok(delivery)` where
//! `Err` is expected (silent last-wins); the controls pass.

use two_bot_core::twitch_eventsub::{classify, Delivery, EventSubError, RevocationReason};

/// Fixed refusal vocabulary. The singular substring also matches a plural
/// message (`... object keys`), so either reads as the same contract.
const DUP_TOKEN: &str = "duplicate JSON object key";
/// Marker embedded in hostile keys/values; refusals must never echo it.
const SENTINEL: &str = "DUPKEY-SENTINEL-9c4e1a";
/// Login-shaped marker (`-` is not a valid login byte); same no-echo rule.
const SENTINEL_LOGIN: &str = "DUPKEY_SENTINEL_9c4e1a";
/// Refusal renderings stay bounded: a static description, never body bytes.
const MAX_RENDERED: usize = 1024;

fn valid_subscription() -> &'static str {
    r#"{"id":"sub-1","type":"stream.online","version":"1","status":"webhook_callback_verification_pending","condition":{"broadcaster_user_id":"1337"}}"#
}

fn valid_event() -> &'static str {
    r#"{"id":"9001","broadcaster_user_id":"1337","broadcaster_user_login":"cool_user","broadcaster_user_name":"Cool_User","type":"live","started_at":"2025-10-09T08:50:00Z"}"#
}

/// Assert `classify` refuses `body` for `label`'s reason; return the refusal
/// `Display` so callers can pin one fixed error across fixtures.
fn assert_refused(message_type: &str, body: &[u8], label: &str) -> String {
    match classify(message_type, body) {
        Ok(delivery) => {
            panic!("{label}: duplicate keys must be refused, got delivery: {delivery:?}")
        }
        Err(error) => {
            let display = format!("{error}");
            let debug = format!("{error:?}");
            assert!(
                display.contains(DUP_TOKEN),
                "{label}: refusal must carry the fixed vocabulary, got: {display}"
            );
            for rendered in [&display, &debug] {
                for marker in [SENTINEL, SENTINEL_LOGIN] {
                    assert!(
                        !rendered.contains(marker),
                        "{label}: refusal echoes hostile bytes: {rendered}"
                    );
                }
                assert!(
                    rendered.len() < MAX_RENDERED,
                    "{label}: refusal is not bounded: {rendered}"
                );
            }
            display
        }
    }
}

#[test]
fn challenge_duplicate_is_refused_in_both_orders() {
    let subscription = valid_subscription();
    let first_sentinel = format!(
        r#"{{"challenge":"{SENTINEL}","challenge":"pogchamp-kappa-360noscope-vohiyo","subscription":{subscription}}}"#
    );
    let first_valid = format!(
        r#"{{"challenge":"pogchamp-kappa-360noscope-vohiyo","challenge":"{SENTINEL}","subscription":{subscription}}}"#
    );
    let a = assert_refused(
        "webhook_callback_verification",
        first_sentinel.as_bytes(),
        "challenge duplicate, sentinel first",
    );
    let b = assert_refused(
        "webhook_callback_verification",
        first_valid.as_bytes(),
        "challenge duplicate, sentinel second",
    );
    assert_eq!(a, b, "both orders share one fixed refusal");
}

#[test]
fn challenge_identical_duplicate_is_refused() {
    let subscription = valid_subscription();
    let body = format!(
        r#"{{"challenge":"pogchamp-kappa-360noscope-vohiyo","challenge":"pogchamp-kappa-360noscope-vohiyo","subscription":{subscription}}}"#
    );
    assert_refused(
        "webhook_callback_verification",
        body.as_bytes(),
        "challenge repeated with the identical value",
    );
}

#[test]
fn notification_duplicate_subscription_is_refused_in_both_orders() {
    let event = valid_event();
    let sub_a = r#"{"id":"sub-1","type":"stream.online","version":"1","status":"enabled","condition":{"broadcaster_user_id":"1337"}}"#;
    let sub_b = r#"{"id":"sub-2","type":"stream.online","version":"1","status":"enabled","condition":{"broadcaster_user_id":"1337"}}"#;
    let a = assert_refused(
        "notification",
        format!(r#"{{"subscription":{sub_a},"subscription":{sub_b},"event":{event}}}"#).as_bytes(),
        "subscription duplicate, sub-1 first",
    );
    let b = assert_refused(
        "notification",
        format!(r#"{{"subscription":{sub_b},"subscription":{sub_a},"event":{event}}}"#).as_bytes(),
        "subscription duplicate, sub-2 first",
    );
    assert_eq!(a, b, "both orders share one fixed refusal");
}

#[test]
fn notification_nested_duplicate_is_refused() {
    let subscription = valid_subscription();
    // Nested conflicting values, both valid logins: last-wins picks one
    // silently today, so either order classifies.
    let nested_login = format!(
        r#"{{"subscription":{subscription},"event":{{"id":"9001","broadcaster_user_id":"1337","broadcaster_user_login":"cool_user","broadcaster_user_login":"{SENTINEL_LOGIN}","type":"live","started_at":"2025-10-09T08:50:00Z"}}}}"#
    );
    let nested_identical = format!(
        r#"{{"subscription":{{"id":"sub-1","type":"stream.online","version":"1","status":"enabled","condition":{{"broadcaster_user_id":"1337","broadcaster_user_id":"1337"}}}},"event":{event}}}"#,
        event = valid_event(),
    );
    let a = assert_refused(
        "notification",
        nested_login.as_bytes(),
        "nested conflicting broadcaster_user_login",
    );
    let b = assert_refused(
        "notification",
        nested_identical.as_bytes(),
        "nested identical condition id",
    );
    assert_eq!(a, b, "nested shapes share one fixed refusal");
}

#[test]
fn revocation_conflicting_status_is_refused_in_both_orders() {
    // Today each order classifies with a DIFFERENT reason: the refusal
    // removes an order-dependent reconcile input.
    let revoked_first = r#"{"subscription":{"id":"sub-1","type":"stream.online","version":"1","status":"authorization_revoked","status":"user_removed","condition":{"broadcaster_user_id":"1337"}}}"#;
    let removed_first = r#"{"subscription":{"id":"sub-1","type":"stream.online","version":"1","status":"user_removed","status":"authorization_revoked","condition":{"broadcaster_user_id":"1337"}}}"#;
    let a = assert_refused(
        "revocation",
        revoked_first.as_bytes(),
        "status duplicate, revoked first",
    );
    let b = assert_refused(
        "revocation",
        removed_first.as_bytes(),
        "status duplicate, removed first",
    );
    assert_eq!(a, b, "both orders share one fixed refusal");
}

#[test]
fn escaped_key_spelling_counts_as_duplicate() {
    // `\u0063` decodes to `c`: same key twice after decoding.
    let subscription = valid_subscription();
    let body = format!(
        "{{\"challenge\":\"a\",\"\\u0063hallenge\":\"b\",\"subscription\":{subscription}}}"
    );
    assert_refused(
        "webhook_callback_verification",
        body.as_bytes(),
        "escaped spelling of challenge",
    );
}

#[test]
fn unknown_duplicate_key_is_refused_without_echo() {
    // Unknown keys are ignored today, so the duplicate classifies; after the
    // fix it is refused, and the refusal must not name the hostile key.
    let subscription = valid_subscription();
    let body = format!(
        r#"{{"{SENTINEL}":1,"{SENTINEL}":2,"challenge":"pogchamp-kappa-360noscope-vohiyo","subscription":{subscription}}}"#
    );
    assert_refused(
        "webhook_callback_verification",
        body.as_bytes(),
        "unknown key repeated",
    );
}

#[test]
fn every_duplicate_shape_shares_one_fixed_error() {
    let subscription = valid_subscription();
    let event = valid_event();
    let challenge = assert_refused(
        "webhook_callback_verification",
        format!(r#"{{"challenge":"a","challenge":"b","subscription":{subscription}}}"#).as_bytes(),
        "challenge shape",
    );
    let sub_a = r#"{"id":"sub-1","type":"stream.online","version":"1","status":"enabled","condition":{"broadcaster_user_id":"1337"}}"#;
    let sub_b = r#"{"id":"sub-2","type":"stream.online","version":"1","status":"enabled","condition":{"broadcaster_user_id":"1337"}}"#;
    let notification = assert_refused(
        "notification",
        format!(r#"{{"subscription":{sub_a},"subscription":{sub_b},"event":{event}}}"#).as_bytes(),
        "subscription shape",
    );
    let nested = assert_refused(
        "notification",
        format!(
            r#"{{"subscription":{subscription},"event":{{"id":"9001","broadcaster_user_id":"1337","broadcaster_user_login":"cool_user","broadcaster_user_login":"other_user","type":"live","started_at":"2025-10-09T08:50:00Z"}}}}"#
        )
        .as_bytes(),
        "nested shape",
    );
    let revocation = assert_refused(
        "revocation",
        r#"{"subscription":{"id":"sub-1","type":"stream.online","version":"1","status":"user_removed","status":"user_removed","condition":{"broadcaster_user_id":"1337"}}}"#.as_bytes(),
        "revocation shape",
    );
    let escaped = assert_refused(
        "webhook_callback_verification",
        format!(
            "{{\"challenge\":\"a\",\"\\u0063hallenge\":\"b\",\"subscription\":{subscription}}}"
        )
        .as_bytes(),
        "escaped shape",
    );
    let unknown = assert_refused(
        "webhook_callback_verification",
        format!(
            r#"{{"{SENTINEL}":1,"{SENTINEL}":2,"challenge":"pogchamp-kappa-360noscope-vohiyo","subscription":{subscription}}}"#
        )
        .as_bytes(),
        "unknown-key shape",
    );
    for (shape, display) in [
        ("notification", notification),
        ("nested", nested),
        ("revocation", revocation),
        ("escaped", escaped),
        ("unknown-key", unknown),
    ] {
        assert_eq!(
            challenge, display,
            "{shape} refusal must equal the fixed error"
        );
    }
}

#[test]
fn refusal_is_deterministic() {
    let subscription = valid_subscription();
    let body = format!(r#"{{"challenge":"a","challenge":"b","subscription":{subscription}}}"#);
    let first = assert_refused(
        "webhook_callback_verification",
        body.as_bytes(),
        "repeat call one",
    );
    let second = assert_refused(
        "webhook_callback_verification",
        body.as_bytes(),
        "repeat call two",
    );
    assert_eq!(first, second, "the same body refuses identically");
}

#[test]
fn unique_bodies_classify_byte_identically() {
    let challenge = format!(
        r#"{{"challenge":"pogchamp-kappa-360noscope-vohiyo","subscription":{}}}"#,
        valid_subscription()
    );
    assert_eq!(
        classify("webhook_callback_verification", challenge.as_bytes()),
        Ok(Delivery::Challenge(
            "pogchamp-kappa-360noscope-vohiyo".to_owned()
        ))
    );

    let notification = format!(
        r#"{{"subscription":{{"id":"sub-1","type":"stream.online","version":"1","status":"enabled","condition":{{"broadcaster_user_id":"1337"}}}},"event":{}}}"#,
        valid_event()
    );
    assert!(
        matches!(
            classify("notification", notification.as_bytes()),
            Ok(Delivery::Notification(ref stream))
                if stream.broadcaster_user_id == "1337"
                    && stream.broadcaster_login == "cool_user"
                    && stream.started_at == "2025-10-09T08:50:00Z"
        ),
        "valid go-live classifies with exact fields"
    );

    let revocation = r#"{"subscription":{"id":"sub-1","type":"stream.online","version":"1","status":"authorization_revoked","condition":{"broadcaster_user_id":"1337"}}}"#;
    assert_eq!(
        classify("revocation", revocation.as_bytes()),
        Ok(Delivery::Revocation {
            subscription_id: "sub-1".to_owned(),
            reason: RevocationReason::AuthorizationRevoked,
        })
    );

    // Pre-existing error paths keep their own variants: refusal must never
    // swallow a syntax error or a subscription mismatch.
    assert_eq!(classify("something_new", b"{}"), Ok(Delivery::Ignored));
    assert_eq!(
        classify("notification", b"not json"),
        Err(EventSubError::InvalidBody)
    );
    assert_eq!(
        classify("notification", b"{}"),
        Err(EventSubError::InvalidBody)
    );
    let offline = r#"{"subscription":{"id":"s","type":"stream.offline","version":"1"},"event":{}}"#;
    assert_eq!(
        classify("notification", offline.as_bytes()),
        Err(EventSubError::WrongSubscription)
    );
}
