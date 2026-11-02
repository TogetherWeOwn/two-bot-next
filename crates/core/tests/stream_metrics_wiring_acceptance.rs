//! Wiring half of the EventSub delivery-metrics acceptance contract (see
//! `stream_metrics_acceptance.rs` for the counter API). Split into its own
//! test binary so these pins compile and fail at runtime before the build
//! lands, while the API pins are compile-red.
//!
//! Contract for the Builder, in the private receiver
//! (`crates/bot/src/eventsub_http.rs`, non-test code):
//!
//! - `eventsub_received()` once per delivery accepted for processing.
//! - `eventsub_classified("challenge")` when a verified challenge is
//!   answered, `"notification"` for a verified stream.online notification,
//!   `"revocation"` for a verified revocation, `"ignored"` for deliveries
//!   acknowledged without effect (unknown type, non-signature verification
//!   failures), the variant label for classify errors (see
//!   `eventsub_error_label`), and `"tampered"` when signature verification
//!   fails.
//! - The kind is a fixed literal chosen by the receiver branch, never
//!   formatted from a header, body, ID or secret.
//!
//! And in `docs/metrics.md`: one row per new counter, in the same
//! `` `two_bot_..._total{label}` `` shape the metrics-docs conformance pin
//! parses, so `/metrics` consumers can discover the series.

use std::path::{Path, PathBuf};

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The receiver source with its `#[cfg(test)]` module removed, so literals
/// inside the module's own tests cannot satisfy the wiring pins.
fn receiver_non_test_source() -> String {
    let source = std::fs::read_to_string(repository_root().join("crates/bot/src/eventsub_http.rs"))
        .expect("receiver source");
    match source.find("#[cfg(test)]") {
        Some(cut) => source[..cut].to_owned(),
        None => source,
    }
}

#[test]
fn stream_metrics_receiver_counts_receipts() {
    let source = receiver_non_test_source();
    assert!(
        source.contains("eventsub_received()"),
        "receiver must count each accepted delivery via eventsub_received()"
    );
}

#[test]
fn stream_metrics_receiver_classifies_each_delivery_kind() {
    let source = receiver_non_test_source();
    for kind in [
        "challenge",
        "notification",
        "revocation",
        "tampered",
        "ignored",
    ] {
        assert!(
            source.contains(&format!("eventsub_classified(\"{kind}\")")),
            "receiver must classify {kind} deliveries with the fixed literal"
        );
    }
}

#[test]
fn stream_metrics_receiver_never_formats_a_label() {
    let source = receiver_non_test_source();
    // The classified/post label must be a fixed literal per branch, not a
    // formatted string that could carry an ID, token or secret into the
    // exposition. (The core counter also collapses unknown values, but the
    // call site must not even attempt a dynamic label.)
    for needle in ["eventsub_classified(&format!", "eventsub_post(&format!"] {
        assert!(
            !source.contains(needle),
            "receiver formats a dynamic metric label: {needle}"
        );
    }
}

#[test]
fn stream_metrics_eventsub_series_are_documented() {
    let doc =
        std::fs::read_to_string(repository_root().join("docs/metrics.md")).expect("metrics doc");
    assert!(
        doc.contains("`two_bot_eventsub_received_total`"),
        "docs/metrics.md must document two_bot_eventsub_received_total"
    );
    assert!(
        doc.contains("`two_bot_eventsub_classified_total{kind}`"),
        "docs/metrics.md must document two_bot_eventsub_classified_total{{kind}}"
    );
    assert!(
        doc.contains("`two_bot_eventsub_posts_total{outcome}`"),
        "docs/metrics.md must document two_bot_eventsub_posts_total{{outcome}}"
    );
}
