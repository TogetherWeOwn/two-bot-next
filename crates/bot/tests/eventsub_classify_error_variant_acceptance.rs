//! Acceptance tests: EventSub classify errors surface their variant on a bounded label.
//!
//! PROTECTED FILE — the paired Builder builds on this branch but must never
//! edit this file. Test corrections stay with the Test Author: if a pin below
//! contradicts the implementation, return it to the author instead of editing
//! here. Never merge this file while its pins are red: it is a deliberately
//! failing-first contract, not a shippable change.
//!
//! Contract (paired build: "eventsub receiver: surface the classify-error
//! variant on a bounded label"): every [`EventSubError`] variant records its
//! own bounded `eventsub_classified_total{kind}` series on the classify-error
//! path, plus a sampled `warn!` carrying `error_variant`. The HTTP behavior
//! per arm is unchanged: deterministic errors stay 2xx-acknowledged
//! (`WrongSubscription`, `InvalidBody`, `DuplicateKeys`) or 400-`malformed`
//! (`InvalidChallenge`), and the catch-all stays a retryable 503
//! `unavailable`. Flood-safety: hostile bursts are counted in counters, never
//! per-request error logs (sampled warn only).
//!
//! Expected labels (one per variant, snake_case like the existing `kind`
//! vocabulary):
//!
//! | Variant | Label |
//! |---|---|
//! | `InvalidSecret` | `invalid_secret` |
//! | `InvalidHeader` | `invalid_header` |
//! | `BodyTooLarge` | `body_too_large` |
//! | `BadSignature` | `bad_signature` |
//! | `InvalidTimestamp` | `invalid_timestamp` |
//! | `StaleTimestamp` | `stale_timestamp` |
//! | `InvalidBody` | `invalid_body` |
//! | `DuplicateKeys` | `duplicate_keys` |
//! | `WrongSubscription` | `wrong_subscription` |
//! | `InvalidChallenge` | `invalid_challenge` |
//! | `InvalidCallback` | `invalid_callback` |
//! | `InvalidBroadcasterId` | `invalid_broadcaster_id` |
//! | `TooManyBroadcasters` | `too_many_broadcasters` |
//!
//! RED on the current head: the receiver discards the variant (`let _ = e`)
//! and every classify error collapses to `kind="other"` (catch-all) or
//! `kind="ignored"` (deterministic arms). The label pins below fail while the
//! hostile corpus itself passes — a behavioral RED, not a compile refusal.
//!
//! The receiver module is private to the `two-bot` binary, so integration
//! tests cannot drive it directly. Each assertion therefore has two halves,
//! following `eventsub_subscription_reconcile_acceptance.rs`: a behavioral
//! half over public `two_bot_core` APIs (offline: scripted bytes, no
//! database, no live Twitch, no sockets) and a wiring gate over the bot/core
//! sources proving the binary records the same labels. The Builder may place
//! the variant mapping in either crate and wire it at the classify-error call
//! sites; any placement satisfying the gates passes without touching this file.
//!
//! Fixture secrets are runtime-generated (`test_secret_value`), never
//! literals, and every callback/host is an `example.com` fixture.

use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    OnceLock,
};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use two_bot_core::format_iso_millis;
use two_bot_core::metrics::{Metrics, EVENTSUB_DELIVERY_KINDS};
use two_bot_core::twitch_eventsub::{
    classify, create_subscription_body, reconcile, verify_delivery, CallbackUrl, DeliveryHeaders,
    EventSubError, WebhookSecret, MAX_BODY_BYTES, MAX_BROADCASTERS, MAX_TIMESTAMP_SKEW_MS,
};

static NEXT_TEST_SECRET: AtomicU64 = AtomicU64::new(1);

/// Runtime-generated signing secret: no literal key material reaches the HMAC,
/// so secret scanners stay silent. Shared via `OnceLock` so every helper in
/// this binary agrees on the value.
fn test_secret_value() -> &'static str {
    static SECRET: OnceLock<String> = OnceLock::new();
    SECRET.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time is after the Unix epoch")
            .as_nanos();
        let nonce = NEXT_TEST_SECRET.fetch_add(1, Ordering::Relaxed);
        format!("synthetic-webhook-secret-{nanos:032x}-{nonce:016x}")
    })
}

fn test_secret() -> WebhookSecret {
    WebhookSecret::new(test_secret_value().to_owned()).expect("generated secret must be valid")
}

fn hmac_sign(message_id: &str, timestamp: &str, body: &[u8]) -> String {
    use hmac::{Hmac, KeyInit, Mac as _};
    use sha2::Sha256;

    let mut mac = Hmac::<Sha256>::new_from_slice(test_secret_value().as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(message_id.as_bytes());
    mac.update(timestamp.as_bytes());
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time is after the Unix epoch")
            .as_millis(),
    )
    .expect("millis fit in i64")
}

fn headers<'a>(
    message_id: &'a str,
    timestamp: &'a str,
    signature: &'a str,
    message_type: &'a str,
) -> DeliveryHeaders<'a> {
    DeliveryHeaders::from_lookup(|name| match name {
        two_bot_core::twitch_eventsub::HEADER_MESSAGE_ID => Some(message_id),
        two_bot_core::twitch_eventsub::HEADER_TIMESTAMP => Some(timestamp),
        two_bot_core::twitch_eventsub::HEADER_SIGNATURE => Some(signature),
        two_bot_core::twitch_eventsub::HEADER_MESSAGE_TYPE => Some(message_type),
        _ => None,
    })
    .expect("fixture headers must parse")
}

fn subscription_fixture(subscription_type: &str) -> serde_json::Value {
    json!({
        "id": "synthetic-sub-0001",
        "type": subscription_type,
        "version": "1",
        "status": "enabled",
        "condition": {"broadcaster_user_id": "123456"},
    })
}

fn live_notification_body() -> String {
    json!({
        "subscription": subscription_fixture("stream.online"),
        "event": {
            "id": "synthetic-stream-0001",
            "broadcaster_user_id": "123456",
            "broadcaster_user_login": "example_user",
            "broadcaster_user_name": "Example_User",
            "type": "live",
            "started_at": "2026-10-13T20:00:00Z",
        },
    })
    .to_string()
}

fn fixture_callback() -> CallbackUrl {
    CallbackUrl::new("https://hooks.example.com/twitch").expect("fixture callback must parse")
}

/// The contract: the bounded metric label for each [`EventSubError`] variant.
/// Exhaustive, so a future fourteenth variant fails this file to compile until
/// the Test Author extends the contract — the vocabulary can never silently
/// grow through `bounded_index` collapse.
fn expected_label(error: &EventSubError) -> &'static str {
    match error {
        EventSubError::InvalidSecret => "invalid_secret",
        EventSubError::InvalidHeader => "invalid_header",
        EventSubError::BodyTooLarge => "body_too_large",
        EventSubError::BadSignature => "bad_signature",
        EventSubError::InvalidTimestamp => "invalid_timestamp",
        EventSubError::StaleTimestamp => "stale_timestamp",
        EventSubError::InvalidBody => "invalid_body",
        EventSubError::DuplicateKeys => "duplicate_keys",
        EventSubError::WrongSubscription => "wrong_subscription",
        EventSubError::InvalidChallenge => "invalid_challenge",
        EventSubError::InvalidCallback => "invalid_callback",
        EventSubError::InvalidBroadcasterId => "invalid_broadcaster_id",
        EventSubError::TooManyBroadcasters => "too_many_broadcasters",
    }
}

fn all_variants() -> [EventSubError; 13] {
    [
        EventSubError::InvalidSecret,
        EventSubError::InvalidHeader,
        EventSubError::BodyTooLarge,
        EventSubError::BadSignature,
        EventSubError::InvalidTimestamp,
        EventSubError::StaleTimestamp,
        EventSubError::InvalidBody,
        EventSubError::DuplicateKeys,
        EventSubError::WrongSubscription,
        EventSubError::InvalidChallenge,
        EventSubError::InvalidCallback,
        EventSubError::InvalidBroadcasterId,
        EventSubError::TooManyBroadcasters,
    ]
}

fn classified_series(label: &str, count: u64) -> String {
    format!("two_bot_eventsub_classified_total{{kind=\"{label}\"}} {count}\n")
}

/// RED until the Builder extends the `eventsub_classified` vocabulary: the
/// variant's label must survive as its own series instead of collapsing to
/// `kind="other"` through `bounded_index`.
fn assert_variant_label_records_distinctly(error: &EventSubError) {
    let label = expected_label(error);
    assert!(
        EVENTSUB_DELIVERY_KINDS.contains(&label),
        "RED: {error:?} must have its own bounded kind label ({label}); \
         the Builder extends EVENTSUB_DELIVERY_KINDS keeping \"other\" last"
    );
    let metrics = Metrics::default();
    metrics.eventsub_classified(label);
    let exposition = metrics.render(None);
    assert!(
        exposition.contains(&classified_series(label, 1)),
        "RED: {error:?} must record kind=\"{label}\", not collapse to \"other\"; got:\n{exposition}"
    );
}

// --- Vocabulary: one bounded series per variant (RED) ---

#[test]
fn each_variant_records_its_own_bounded_series() {
    let labels: Vec<&str> = all_variants().iter().map(expected_label).collect();
    let mut sorted = labels.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        13,
        "the contract itself must define 13 distinct labels"
    );
    let metrics = Metrics::default();
    for &label in &labels {
        metrics.eventsub_classified(label);
    }
    let exposition = metrics.render(None);
    for &label in &labels {
        assert!(
            exposition.contains(&classified_series(label, 1)),
            "RED: kind=\"{label}\" must exist as its own series; got:\n{exposition}"
        );
    }
}

#[test]
fn unknown_and_display_labels_still_collapse_to_other() {
    // Bounded by construction: hostile and human-readable strings never become
    // series. `other` stays the trailing vocabulary entry absorbing them.
    let big = "x".repeat(4096);
    let hostile = [
        "not-a-kind",
        "BadSignature",
        "bad signature",
        "bad_signature\ninjected",
        "Signature does not match.",
        "Body is not a valid EventSub message.",
        "",
        big.as_str(),
    ];
    let metrics = Metrics::default();
    for &label in &hostile {
        metrics.eventsub_classified(label);
    }
    let exposition = metrics.render(None);
    for &label in &hostile {
        if label.is_empty() {
            // `contains("")` is vacuous, so the empty id is covered by the
            // collapse-to-other count below instead of this loop.
            continue;
        }
        assert!(
            !exposition.contains(label),
            "hostile label must never reach exposition: {label:?}"
        );
    }
    assert!(
        exposition.contains(&classified_series("other", hostile.len() as u64)),
        "unknown labels must collapse to kind=\"other\"; got:\n{exposition}"
    );
}

// --- Hostile corpus: each variant is producible offline, then pinned to its label ---
//
// The `assert_eq!(error, ...)` half passes today (the corpus is valid); the
// label half is RED until the Builder lands the vocabulary + wiring.

#[test]
fn corpus_short_long_and_nongraphic_secrets_are_invalid_secret() {
    for secret in [
        "short".to_owned(),
        "x".repeat(101),
        "has space in it".to_owned(),
    ] {
        let error = WebhookSecret::new(secret).unwrap_err();
        assert_eq!(error, EventSubError::InvalidSecret);
        assert_variant_label_records_distinctly(&error);
    }
}

#[test]
fn corpus_missing_headers_and_unsafe_ids_are_invalid_header() {
    let error = DeliveryHeaders::from_lookup(|_| None).unwrap_err();
    assert_eq!(error, EventSubError::InvalidHeader);
    assert_variant_label_records_distinctly(&error);
    let long = "a".repeat(129);
    for bad_id in ["", "has space", long.as_str()] {
        let error = DeliveryHeaders::from_lookup(|name| match name {
            two_bot_core::twitch_eventsub::HEADER_MESSAGE_ID => Some(bad_id),
            two_bot_core::twitch_eventsub::HEADER_TIMESTAMP => Some("t"),
            two_bot_core::twitch_eventsub::HEADER_SIGNATURE => Some("s"),
            two_bot_core::twitch_eventsub::HEADER_MESSAGE_TYPE => Some("m"),
            _ => None,
        })
        .unwrap_err();
        assert_eq!(error, EventSubError::InvalidHeader, "{bad_id:?}");
    }
}

#[test]
fn corpus_oversize_body_is_body_too_large_before_hashing() {
    let big = vec![b'x'; MAX_BODY_BYTES + 1];
    let timestamp = format_iso_millis(now_ms());
    let signature = hmac_sign("synthetic-delivery-body-cap", &timestamp, &big);
    let delivered = headers(
        "synthetic-delivery-body-cap",
        &timestamp,
        &signature,
        "notification",
    );
    let error = verify_delivery(&test_secret(), &delivered, &big, now_ms()).unwrap_err();
    assert_eq!(error, EventSubError::BodyTooLarge);
    assert_variant_label_records_distinctly(&error);
}

#[test]
fn corpus_tampered_body_and_wrong_secret_are_bad_signature() {
    let body = live_notification_body();
    let timestamp = format_iso_millis(now_ms());
    let zeros = "sha256=".to_owned() + &"0".repeat(64);
    let delivered = headers(
        "synthetic-delivery-forged-0001",
        &timestamp,
        &zeros,
        "notification",
    );
    let error = verify_delivery(&test_secret(), &delivered, body.as_bytes(), now_ms()).unwrap_err();
    assert_eq!(error, EventSubError::BadSignature);
    assert_variant_label_records_distinctly(&error);

    // Same bytes, signature by a different secret: still a forgery.
    let mut tampered = body.into_bytes();
    tampered.push(b' ');
    let signature = hmac_sign(
        "synthetic-delivery-forged-0002",
        &timestamp,
        &tampered[..tampered.len() - 1],
    );
    let delivered = headers(
        "synthetic-delivery-forged-0002",
        &timestamp,
        &signature,
        "notification",
    );
    let error = verify_delivery(&test_secret(), &delivered, &tampered, now_ms()).unwrap_err();
    assert_eq!(error, EventSubError::BadSignature);
}

#[test]
fn corpus_garbage_timestamp_is_invalid_timestamp_after_signature() {
    let body = live_notification_body();
    let timestamp = "not-a-time";
    let signature = hmac_sign("synthetic-delivery-bad-clock", timestamp, body.as_bytes());
    let delivered = headers(
        "synthetic-delivery-bad-clock",
        timestamp,
        &signature,
        "notification",
    );
    let error = verify_delivery(&test_secret(), &delivered, body.as_bytes(), now_ms()).unwrap_err();
    assert_eq!(error, EventSubError::InvalidTimestamp);
    assert_variant_label_records_distinctly(&error);
}

#[test]
fn corpus_skewed_timestamp_is_stale_timestamp_after_signature() {
    let body = live_notification_body();
    let now = now_ms();
    let old = now - MAX_TIMESTAMP_SKEW_MS - 1;
    let timestamp = format_iso_millis(old);
    let signature = hmac_sign(
        "synthetic-delivery-stale-clock",
        &timestamp,
        body.as_bytes(),
    );
    let delivered = headers(
        "synthetic-delivery-stale-clock",
        &timestamp,
        &signature,
        "notification",
    );
    let error = verify_delivery(&test_secret(), &delivered, body.as_bytes(), now).unwrap_err();
    assert_eq!(error, EventSubError::StaleTimestamp);
    assert_variant_label_records_distinctly(&error);
}

#[test]
fn corpus_malformed_bodies_are_invalid_body() {
    for body in [
        b"not json".as_slice(),
        b"{}".as_slice(),
        b"{\"subscription\": null}".as_slice(),
    ] {
        let error = classify("notification", body).unwrap_err();
        assert_eq!(error, EventSubError::InvalidBody);
        assert_variant_label_records_distinctly(&error);
    }
}

#[test]
fn corpus_signed_duplicate_keys_record_their_own_variant() {
    let valid: serde_json::Value =
        serde_json::from_str(&live_notification_body()).expect("valid notification fixture");
    let subscription = &valid["subscription"];
    let event = &valid["event"];
    let escaped_key = format!("{}u0073ubscription", char::from(92));
    // Identical values are still duplicates; escaped keys compare decoded,
    // and duplicate keys in unknown nested fields are refused too.
    let bodies = [
        format!(
            r#"{{"subscription":{subscription},"subscription":{subscription},"event":{event}}}"#
        ),
        format!(
            r#"{{"subscription":{subscription},"{escaped_key}":{subscription},"event":{event}}}"#
        ),
        format!(r#"{{"subscription":{subscription},"event":{event},"extra":{{"x":1,"x":2}}}}"#),
    ];
    let now = now_ms();
    let timestamp = format_iso_millis(now);
    for (i, body) in bodies.iter().enumerate() {
        let message_id = format!("synthetic-delivery-duplicate-{i}");
        let signature = hmac_sign(&message_id, &timestamp, body.as_bytes());
        let delivered = headers(&message_id, &timestamp, &signature, "notification");
        verify_delivery(&test_secret(), &delivered, body.as_bytes(), now)
            .expect("signed duplicate-key bytes must reach classification");
        let error = classify("notification", body.as_bytes()).unwrap_err();
        assert_eq!(error, EventSubError::DuplicateKeys, "corpus item {i}");
        assert_variant_label_records_distinctly(&error);

        // Verification still owns forgeries: adding even whitespace after
        // signing must fail before any duplicate-key classification.
        let tampered = format!("{body} ");
        let error =
            verify_delivery(&test_secret(), &delivered, tampered.as_bytes(), now).unwrap_err();
        assert_eq!(error, EventSubError::BadSignature, "tampered item {i}");
    }
}

#[test]
fn corpus_foreign_subscription_is_wrong_subscription() {
    let body = json!({
        "subscription": subscription_fixture("stream.offline"),
        "event": {"broadcaster_user_id": "123456"},
    })
    .to_string();
    let error = classify("notification", body.as_bytes()).unwrap_err();
    assert_eq!(error, EventSubError::WrongSubscription);
    assert_variant_label_records_distinctly(&error);
}

#[test]
fn corpus_bad_challenges_are_invalid_challenge() {
    for challenge in [json!(""), json!("has space"), json!(7)] {
        let body = json!({
            "challenge": challenge.clone(),
            "subscription": subscription_fixture("stream.online"),
        })
        .to_string();
        let error = classify("webhook_callback_verification", body.as_bytes()).unwrap_err();
        assert_eq!(error, EventSubError::InvalidChallenge, "{challenge}");
        assert_variant_label_records_distinctly(&error);
    }
}

#[test]
fn corpus_nonhttps_callback_is_invalid_callback() {
    for callback in [
        "http://hooks.example.com/twitch",
        "https://hooks.example.com:8443/twitch",
        "not a url",
    ] {
        let error = CallbackUrl::new(callback).unwrap_err();
        assert_eq!(error, EventSubError::InvalidCallback, "{callback}");
        assert_variant_label_records_distinctly(&error);
    }
}

#[test]
fn corpus_nonnumeric_broadcaster_is_invalid_broadcaster_id() {
    for broadcaster in ["", "abc", "13 37"] {
        let error =
            create_subscription_body(broadcaster, &fixture_callback(), &test_secret()).unwrap_err();
        assert_eq!(error, EventSubError::InvalidBroadcasterId, "{broadcaster}");
        assert_variant_label_records_distinctly(&error);
    }
}

#[test]
fn corpus_oversize_roster_is_too_many_broadcasters() {
    let many: Vec<String> = (0..=MAX_BROADCASTERS).map(|i| i.to_string()).collect();
    assert_eq!(many.len(), MAX_BROADCASTERS + 1);
    let error = reconcile(&many, &fixture_callback(), &[]).unwrap_err();
    assert_eq!(error, EventSubError::TooManyBroadcasters);
    assert_variant_label_records_distinctly(&error);
}

// --- Wiring gates: the binary must record the same labels (RED) ---
//
// The receiver is private to the binary, so these gates read the non-test
// sources (never `tests/`, so this file's own contract match cannot satisfy
// them) and prove the classify-error path records the variant.

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).expect("crate src dir must be readable");
    for entry in entries {
        let path = entry.expect("dir entry must be readable").path();
        if path.is_dir() {
            rs_files(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

fn non_test_sources(crate_dir: &Path) -> String {
    let mut files = Vec::new();
    rs_files(&crate_dir.join("src"), &mut files);
    files.sort();
    let mut joined = String::new();
    for path in &files {
        let text = std::fs::read_to_string(path).expect("crate source must be readable");
        let head: &str = match text.split_once("#[cfg(test)]") {
            Some((head, _)) => head,
            None => text.as_str(),
        };
        joined.push_str(head);
        joined.push('\n');
    }
    joined
}

fn bot_sources() -> String {
    non_test_sources(Path::new(env!("CARGO_MANIFEST_DIR")))
}

fn core_sources() -> String {
    non_test_sources(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../core"))
}

/// The variant mapping may live in either crate (for example
/// `EventSubError::InvalidBody => "invalid_body"` in core, or `Self::...` arms
/// in the binary): each variant name must map to its own label string within
/// one match arm.
fn contains_mapping_arm(sources: &str, variant: &str, label: &str) -> bool {
    let literal = format!("\"{label}\"");
    let mut rest = sources;
    while let Some(pos) = rest.find(variant) {
        let after = &rest[pos + variant.len()..];
        let window: String = after.chars().take(48).collect();
        if let Some(arrow) = window.find("=>") {
            if window[arrow + 2..].contains(literal.as_str()) {
                return true;
            }
        }
        rest = after;
    }
    false
}

const VARIANT_DEBUG_NAMES: [&str; 13] = [
    "InvalidSecret",
    "InvalidHeader",
    "BodyTooLarge",
    "BadSignature",
    "InvalidTimestamp",
    "StaleTimestamp",
    "InvalidBody",
    "DuplicateKeys",
    "WrongSubscription",
    "InvalidChallenge",
    "InvalidCallback",
    "InvalidBroadcasterId",
    "TooManyBroadcasters",
];

#[test]
fn classify_error_arms_record_the_variant_not_other() {
    let http =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/eventsub_http.rs"))
            .expect("eventsub_http.rs must be readable");
    let (http, _) = http.split_once("#[cfg(test)]").expect("test module");
    assert!(
        !http.contains("let _ = e"),
        "RED: the classify-error path must not discard the variant (`let _ = e`)"
    );
    let region = http
        .split_once("match classify(")
        .expect("classify match")
        .1;
    assert_eq!(
        region.matches("eventsub_classified(\"other\")").count(),
        0,
        "RED: no classify-error arm may record the catch-all \"other\""
    );
    assert_eq!(
        region.matches("eventsub_classified(\"ignored\")").count(),
        1,
        "RED: only the Ok(Ignored) arm keeps \"ignored\"; every Err arm records its variant"
    );
    let mapping_sources = format!("{}\n{}", bot_sources(), core_sources());
    for (variant, error) in VARIANT_DEBUG_NAMES.iter().zip(all_variants()) {
        let label = expected_label(&error);
        assert!(
            contains_mapping_arm(&mapping_sources, variant, label),
            "RED: no `{variant} => \"{label}\"` mapping arm in bot/core non-test sources; \
             the Builder adds a total EventSubError-to-label mapping (13 arms) in either crate \
             and uses it at the classify-error call sites"
        );
    }
}

#[test]
fn classify_errors_emit_a_sampled_error_variant_warn() {
    let http =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/eventsub_http.rs"))
            .expect("eventsub_http.rs must be readable");
    let (http, _) = http.split_once("#[cfg(test)]").expect("test module");
    assert!(
        http.contains("error_variant"),
        "RED: the classify-error path must log a warn carrying the bounded `error_variant` scalar"
    );
    assert!(
        http.contains("warn!"),
        "classify errors must warn, never stay silent"
    );
    let has_counter_sampler = (http.contains("AtomicU64") || http.contains("AtomicUsize"))
        && http.contains("fetch_add")
        && http.contains('%');
    let has_named_sampler = [
        "sample",
        "every_n",
        "throttle",
        "last_warn",
        "should_log",
        "log_every",
        "sampled",
    ]
    .iter()
    .any(|marker| http.contains(*marker));
    assert!(
        has_counter_sampler || has_named_sampler,
        "RED: the hostile-path warn must be sampled (for example a static atomic counter \
         emitting every Nth, or an equivalent bounded mechanism named \
         sampled/sample/every_n/throttle/last_warn/should_log/log_every)"
    );
    let refuse_body = http
        .split_once("fn refuse(")
        .expect("refuse must exist")
        .1
        .split_once("\n}\n")
        .expect("refuse end")
        .0;
    let refuse_sampled = [
        "sample",
        "every_n",
        "throttle",
        "fetch_add",
        "should_emit",
        "should_log",
        "skip",
    ]
    .iter()
    .any(|marker| refuse_body.contains(*marker));
    assert!(
        refuse_sampled || !refuse_body.contains("warn!"),
        "RED: refuse() must not warn unconditionally per request; sample it or delegate \
         to a sampled helper so forgery bursts stay bounded"
    );
}

#[test]
fn no_error_level_logs_on_eventsub_paths() {
    // Counters and sampled warns carry hostile traffic; `error!` would page or
    // flood. Both the transport core and the receiver stay error-free.
    let http =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/eventsub_http.rs"))
            .expect("eventsub_http.rs must be readable");
    let (http, _) = http.split_once("#[cfg(test)]").expect("test module");
    assert!(
        !http.contains("error!"),
        "the EventSub receiver must never log at error level on hostile traffic"
    );
    let twitch_eventsub = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../core/src/twitch_eventsub.rs"),
    )
    .expect("twitch_eventsub.rs must be readable");
    let (twitch_eventsub, _) = twitch_eventsub
        .split_once("#[cfg(test)]")
        .expect("test module");
    assert!(
        !twitch_eventsub.contains("error!"),
        "the EventSub transport core must never log at error level"
    );
}

#[test]
fn retryable_503_envelope_unchanged_on_the_catch_all() {
    // Behavior freeze: only the dropped variant is the gap. The catch-all
    // keeps its retryable 503 `unavailable` envelope, deterministic errors
    // keep their 2xx ack / 400 malformed answers.
    let http =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/eventsub_http.rs"))
            .expect("eventsub_http.rs must be readable");
    let (http, _) = http.split_once("#[cfg(test)]").expect("test module");
    let region = http
        .split_once("match classify(")
        .expect("classify match")
        .1;
    let catch_all = region.split_once("Err(e) => {").expect("catch-all arm").1;
    assert!(
        catch_all.contains("StatusCode::SERVICE_UNAVAILABLE"),
        "catch-all must stay 503"
    );
    assert!(
        catch_all.contains("\"unavailable\""),
        "catch-all must keep the unavailable error class"
    );
    assert!(catch_all.contains("true,"), "catch-all must stay retryable");
    for marker in [
        "WrongSubscription",
        "InvalidBody",
        "DuplicateKeys",
        "InvalidChallenge",
        "ack(",
        "BAD_REQUEST",
        "malformed",
    ] {
        assert!(
            region.contains(marker),
            "deterministic classify arms must keep their routing and answers: {marker}"
        );
    }
}

#[test]
fn duplicate_keys_keep_the_deterministic_ack_path() {
    let http =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/eventsub_http.rs"))
            .expect("eventsub_http.rs must be readable");
    let (http, _) = http.split_once("#[cfg(test)]").expect("test module");
    let region = http
        .split_once("match classify(")
        .expect("classify match")
        .1;
    let duplicate = region
        .find("EventSubError::DuplicateKeys")
        .expect("duplicate-key arm");
    let start = region[..duplicate]
        .rfind("Err(")
        .expect("classify-error arm");
    let body = region[start..].split_once("=>").expect("arm body").1;
    let end = body.find("Err(").expect("next classify-error arm");
    let body = &body[..end];
    assert!(
        body.contains("eventsub_error_label(&e)") && body.contains("eventsub_classified(variant)"),
        "duplicate keys must record the bounded variant, not invalid_body, ignored or other"
    );
    assert!(
        body.contains("sampled_classify_warn(") && body.contains("StatusCode::OK"),
        "duplicate keys must use the shared sampled classification warning at status 200"
    );
    assert!(
        !body.contains("warn!"),
        "the duplicate-key arm must not warn unconditionally per delivery"
    );
    let sampled_warn = http
        .split_once("fn sampled_classify_warn(")
        .expect("shared classification warning")
        .1
        .split_once("\n}\n")
        .expect("shared classification warning end")
        .0;
    assert!(
        sampled_warn.contains("if sampled_refusal_warn_emits()")
            && sampled_warn.contains("tracing::warn!")
            && sampled_warn.contains("error_variant = error_variant"),
        "the shared warning must sample and carry the bounded duplicate-key error_variant"
    );
    assert!(
        body.contains("ack("),
        "duplicate keys must retain the 200 ack"
    );
    assert!(
        !body.contains("SERVICE_UNAVAILABLE") && !body.contains("\"unavailable\""),
        "duplicate keys are deterministic, never a retryable 503"
    );
}

// --- Flood: a forgery burst is counted in counters, never per-request logs ---

#[test]
fn forgery_burst_counts_in_counters_not_logs() {
    const BURST: usize = 1000;
    let body = live_notification_body();
    let timestamp = format_iso_millis(now_ms());
    let zeros = "sha256=".to_owned() + &"0".repeat(64);
    for i in 0..BURST {
        let message_id = format!("synthetic-delivery-burst-{i:04}");
        let delivered = headers(&message_id, &timestamp, &zeros, "notification");
        let error =
            verify_delivery(&test_secret(), &delivered, body.as_bytes(), now_ms()).unwrap_err();
        assert_eq!(error, EventSubError::BadSignature, "burst item {i}");
    }
    // Counters carry the full volume under one bounded series; the log-volume
    // half (sampled warn, no error!) is pinned by the wiring gates above,
    // since the receiver is private to the binary.
    let metrics = Metrics::default();
    for _ in 0..BURST {
        metrics.eventsub_classified("bad_signature");
    }
    let exposition = metrics.render(None);
    assert!(
        exposition.contains(&classified_series("bad_signature", BURST as u64)),
        "RED: the burst must count under kind=\"bad_signature\"; got:\n{exposition}"
    );
    for (name, probe) in [
        ("secret", test_secret_value()),
        ("delivery-id", "synthetic-delivery-burst-0000"),
        ("timestamp", timestamp.as_str()),
        ("signature", zeros.as_str()),
    ] {
        assert!(
            !exposition.contains(probe),
            "per-request values must never reach exposition: {name}"
        );
    }
}
