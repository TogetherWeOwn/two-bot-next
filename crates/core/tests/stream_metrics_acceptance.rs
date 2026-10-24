//! Acceptance contract for EventSub delivery metrics (paired test card for
//! the "emit EventSub delivery metrics" build slice). Test-first: this file
//! is compile-red until the Builder adds the counters below, and the Builder
//! must not need to edit it.
//!
//! Contract for `two_bot_core::metrics`, following the existing fixed-size,
//! allowlist pattern (`presence_capture`, `internal_action`):
//!
//! - `EVENTSUB_DELIVERY_KINDS` is exactly the five outcome kinds, then one
//!   snake_case label per `EventSubError` variant (13), then the trailing
//!   `other` absorbing unknown strings.
//! - `EVENTSUB_POST_OUTCOMES` is exactly
//!   `["attempted", "succeeded", "failed", "other"]`.
//! - `Metrics::eventsub_received()` increments the label-free counter
//!   `two_bot_eventsub_received_total` (one per delivery the receiver
//!   accepts for processing).
//! - `Metrics::eventsub_classified(kind)` increments
//!   `two_bot_eventsub_classified_total{kind}`; an unrecognised kind
//!   collapses to `other` and never becomes a label value.
//! - `Metrics::eventsub_post(outcome)` increments
//!   `two_bot_eventsub_posts_total{outcome}`; an unrecognised outcome
//!   collapses to `other`.
//! - Every series renders at zero from a fresh registry, like every other
//!   family, so a scrape before the first delivery still shows the series.
//! - No member/broadcaster ID, token, webhook secret, header value or body
//!   text may ever appear in a label value: the two allowlists above are
//!   the complete label vocabulary.
//!
//! The receiver wiring half of the contract (which delivery increments
//! which kind) is pinned in `stream_metrics_wiring_acceptance.rs`.

use std::collections::HashSet;
use two_bot_core::metrics::{
    Metrics, EVENTSUB_DELIVERY_KINDS, EVENTSUB_POST_OUTCOMES, EVENTSUB_RECONCILE_ACTIONS,
    EVENTSUB_RECONCILE_ERROR_CLASSES, EVENTSUB_RECONCILE_OUTCOMES,
};

#[test]
fn stream_metrics_eventsub_vocabularies_are_exactly_the_allowlists() {
    assert_eq!(
        EVENTSUB_DELIVERY_KINDS,
        [
            "challenge",
            "notification",
            "revocation",
            "tampered",
            "ignored",
            "invalid_secret",
            "invalid_header",
            "body_too_large",
            "bad_signature",
            "invalid_timestamp",
            "stale_timestamp",
            "invalid_body",
            "duplicate_keys",
            "wrong_subscription",
            "invalid_challenge",
            "invalid_callback",
            "invalid_broadcaster_id",
            "too_many_broadcasters",
            "other"
        ]
    );
    assert_eq!(
        EVENTSUB_POST_OUTCOMES,
        ["attempted", "succeeded", "failed", "other"]
    );
}

#[test]
fn stream_metrics_eventsub_series_render_at_zero_before_any_delivery() {
    let text = Metrics::default().render(None);
    assert!(
        text.contains("# TYPE two_bot_eventsub_received_total counter\n"),
        "received counter family must render"
    );
    assert!(text.contains("two_bot_eventsub_received_total 0\n"));
    assert!(!text.contains("two_bot_eventsub_received_total{"));
    assert!(text.contains("# TYPE two_bot_eventsub_classified_total counter\n"));
    for kind in EVENTSUB_DELIVERY_KINDS {
        assert!(
            text.contains(&format!(
                "two_bot_eventsub_classified_total{{kind=\"{kind}\"}} 0\n"
            )),
            "missing zero series for kind {kind}"
        );
    }
    assert!(text.contains("# TYPE two_bot_eventsub_posts_total counter\n"));
    for outcome in EVENTSUB_POST_OUTCOMES {
        assert!(
            text.contains(&format!(
                "two_bot_eventsub_posts_total{{outcome=\"{outcome}\"}} 0\n"
            )),
            "missing zero series for outcome {outcome}"
        );
    }
    // Fixed cardinality: one received sample, one sample per kind, one per
    // post outcome, plus the reconcile outcome families (passes by outcome,
    // actions by action, errors by error_class) — nothing else in the
    // eventsub namespace.
    let samples: Vec<&str> = text
        .lines()
        .filter(|line| line.starts_with("two_bot_eventsub_"))
        .collect();
    assert_eq!(
        samples.len(),
        1 + EVENTSUB_DELIVERY_KINDS.len()
            + EVENTSUB_POST_OUTCOMES.len()
            + EVENTSUB_RECONCILE_OUTCOMES.len()
            + EVENTSUB_RECONCILE_ACTIONS.len()
            + EVENTSUB_RECONCILE_ERROR_CLASSES.len(),
        "eventsub samples: {samples:?}"
    );
}

#[test]
fn stream_metrics_eventsub_counters_increment_on_each_delivery_kind() {
    let metrics = Metrics::default();
    // One delivery of each classified kind the receiver can produce today:
    // a webhook-callback verification challenge, a stream.online
    // notification, a subscription revocation, and a delivery whose
    // signature does not verify (tampered). Each is first a receipt.
    for kind in ["challenge", "notification", "revocation", "tampered"] {
        metrics.eventsub_received();
        metrics.eventsub_classified(kind);
    }
    // A second notification proves the counter increments rather than sets,
    // and the go-live post it owns is attempted and then succeeds.
    metrics.eventsub_received();
    metrics.eventsub_classified("notification");
    metrics.eventsub_post("attempted");
    metrics.eventsub_post("succeeded");

    let text = metrics.render(None);
    assert!(text.contains("two_bot_eventsub_received_total 5\n"));
    assert!(text.contains("two_bot_eventsub_classified_total{kind=\"challenge\"} 1\n"));
    assert!(text.contains("two_bot_eventsub_classified_total{kind=\"notification\"} 2\n"));
    assert!(text.contains("two_bot_eventsub_classified_total{kind=\"revocation\"} 1\n"));
    assert!(text.contains("two_bot_eventsub_classified_total{kind=\"tampered\"} 1\n"));
    assert!(text.contains("two_bot_eventsub_classified_total{kind=\"ignored\"} 0\n"));
    assert!(text.contains("two_bot_eventsub_classified_total{kind=\"other\"} 0\n"));
    assert!(text.contains("two_bot_eventsub_posts_total{outcome=\"attempted\"} 1\n"));
    assert!(text.contains("two_bot_eventsub_posts_total{outcome=\"succeeded\"} 1\n"));
    assert!(text.contains("two_bot_eventsub_posts_total{outcome=\"failed\"} 0\n"));
    assert!(text.contains("two_bot_eventsub_posts_total{outcome=\"other\"} 0\n"));

    // The whole exposition stays well-formed: unique series, numeric values.
    let mut series = HashSet::new();
    for line in text.lines().filter(|line| !line.starts_with('#')) {
        let (key, value) = line.rsplit_once(' ').unwrap();
        assert!(series.insert(key), "duplicate series: {key}");
        assert!(value.parse::<f64>().is_ok(), "bad sample: {line}");
    }
}

#[test]
fn stream_metrics_duplicate_keys_increment_without_collapsing_to_other() {
    let metrics = Metrics::default();
    metrics.eventsub_classified("duplicate_keys");
    metrics.eventsub_classified("duplicate_keys");
    let text = metrics.render(None);
    assert!(text.contains("two_bot_eventsub_classified_total{kind=\"duplicate_keys\"} 2\n"));
    assert!(text.contains("two_bot_eventsub_classified_total{kind=\"invalid_body\"} 0\n"));
    assert!(text.contains("two_bot_eventsub_classified_total{kind=\"ignored\"} 0\n"));
    assert!(text.contains("two_bot_eventsub_classified_total{kind=\"other\"} 0\n"));
}

#[test]
fn stream_metrics_eventsub_labels_never_carry_ids_tokens_or_secrets() {
    let metrics = Metrics::default();
    let before = Metrics::default().render(None).lines().count();
    // Hostile caller input shaped like the values that must never become
    // labels: a numeric broadcaster/member ID, an opaque token, a webhook
    // secret, and label-injection payloads. Every one collapses to `other`.
    let hostiles = [
        "123456789012345678",
        "broadcaster_user_id=123456",
        "oauth:synthetic-token-value",
        "synthetic-webhook-secret-value",
        "kind\"}\n two_bot_fictional_total 1 #",
    ];
    for round in 0..20 {
        for hostile in hostiles {
            let hostile = format!("{hostile}-{round}");
            metrics.eventsub_classified(&hostile);
            metrics.eventsub_post(&hostile);
        }
    }
    let text = metrics.render(None);
    assert_eq!(
        text.lines().count(),
        before,
        "hostile labels must not grow the exposition"
    );
    for forbidden in [
        "123456789012345678",
        "123456",
        "synthetic-token-value",
        "synthetic-webhook-secret-value",
        "two_bot_fictional_total 1",
    ] {
        assert!(
            !text.contains(forbidden),
            "exposition leaked {forbidden:?} into a label"
        );
    }
    let collapsed = 20 * hostiles.len();
    assert!(text.contains(&format!(
        "two_bot_eventsub_classified_total{{kind=\"other\"}} {collapsed}\n"
    )));
    assert!(text.contains(&format!(
        "two_bot_eventsub_posts_total{{outcome=\"other\"}} {collapsed}\n"
    )));
    // Every rendered eventsub label value is in its allowlist.
    for line in text.lines().filter(|line| line.contains('{')) {
        if let Some(sample) = line.strip_prefix("two_bot_eventsub_classified_total{kind=\"") {
            let value = sample.split('"').next().unwrap();
            assert!(
                EVENTSUB_DELIVERY_KINDS.contains(&value),
                "unbounded kind label {value:?}"
            );
        }
        if let Some(sample) = line.strip_prefix("two_bot_eventsub_posts_total{outcome=\"") {
            let value = sample.split('"').next().unwrap();
            assert!(
                EVENTSUB_POST_OUTCOMES.contains(&value),
                "unbounded outcome label {value:?}"
            );
        }
    }
}
