//! Acceptance tests: EventSub reconcile outcome gauges.
//!
//! The reconcile executor records each pass that reaches Helix in the
//! existing metrics registry: one `passes_total{outcome}` sample per pass,
//! `actions_total{action}` for successful 2xx creates/deletes, and
//! `errors_total{error_class}` per failure site. Parked or refused startups
//! stay log-only.
//!
//! These tests pin the contract at the registry/exposition layer, offline:
//! no database, no live Twitch, no production mutation. Fixtures carry no
//! secrets, tokens, broadcaster IDs or response bytes as labels — hostile
//! strings must fold into the bounded trailing slot, never grow the series.
//!
//! Each test uses a runtime-local [`Metrics`], never the process-global, so
//! parallel suites cannot flake the exact-value asserts.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use two_bot_core::metrics::{
    Metrics, EVENTSUB_RECONCILE_ACTIONS, EVENTSUB_RECONCILE_ERROR_CLASSES,
    EVENTSUB_RECONCILE_OUTCOMES,
};

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Sample lines for one metric family: `(labels, value)` pairs.
fn family_samples(text: &str, family: &str) -> Vec<(String, f64)> {
    text.lines()
        .filter(|line| line.starts_with(family) && line.contains('{'))
        .map(|line| {
            let (series, value) = line
                .rsplit_once(' ')
                .unwrap_or_else(|| panic!("bad sample line: {line}"));
            let labels = series
                .split_once('{')
                .map(|(_, rest)| rest.to_owned())
                .unwrap_or_default();
            (
                labels,
                value.parse::<f64>().expect("sample value must parse"),
            )
        })
        .collect()
}

fn count_family_lines(text: &str, family: &str) -> usize {
    text.lines()
        .filter(|line| line.starts_with(family) && line.contains('{'))
        .count()
}

/// Every reconcile scenario records exactly one pass sample.
#[test]
fn passes_increment_once_per_reconcile_scenario() {
    let metrics = Metrics::default();
    let text = metrics.render(None);
    for outcome in ["healthy", "pending", "failed"] {
        assert!(
            text.contains(&format!(
                "two_bot_eventsub_reconcile_passes_total{{outcome=\"{outcome}\"}} 0\n"
            )),
            "fresh registry must expose a zero {outcome} pass series"
        );
    }

    metrics.eventsub_reconcile_pass("healthy");
    metrics.eventsub_reconcile_pass("pending");
    metrics.eventsub_reconcile_pass("failed");

    let text = metrics.render(None);
    for outcome in ["healthy", "pending", "failed"] {
        assert!(
            text.contains(&format!(
                "two_bot_eventsub_reconcile_passes_total{{outcome=\"{outcome}\"}} 1\n"
            )),
            "{outcome} pass must increment exactly once"
        );
    }
}

/// Successful mutations count by bounded action; a zero count is a no-op.
#[test]
fn actions_count_successful_mutations_and_zero_is_noop() {
    let metrics = Metrics::default();
    metrics.eventsub_reconcile_action("create", 2);
    metrics.eventsub_reconcile_action("delete", 1);
    metrics.eventsub_reconcile_action("create", 0);
    metrics.eventsub_reconcile_action("delete", 0);

    let text = metrics.render(None);
    assert!(text.contains("two_bot_eventsub_reconcile_actions_total{action=\"create\"} 2\n"));
    assert!(text.contains("two_bot_eventsub_reconcile_actions_total{action=\"delete\"} 1\n"));
}

/// Each failure site records under its log `error_class`.
#[test]
fn errors_record_failure_site_per_pass() {
    let metrics = Metrics::default();
    for error_class in [
        "reconcile_action_failed",
        "reconcile_prior_failed",
        "reconcile_unhealthy",
        "reconcile_failed",
    ] {
        metrics.eventsub_reconcile_error(error_class);
    }

    let text = metrics.render(None);
    for error_class in [
        "reconcile_action_failed",
        "reconcile_prior_failed",
        "reconcile_unhealthy",
        "reconcile_failed",
        "other",
    ] {
        let want = if error_class == "other" { 0 } else { 1 };
        assert!(
            text.contains(&format!(
                "two_bot_eventsub_reconcile_errors_total{{error_class=\"{error_class}\"}} {want}\n"
            )),
            "{error_class} must read {want}"
        );
    }
}

/// Unknown outcomes fail closed into the bounded trailing slot.
#[test]
fn unknown_outcomes_fail_closed_into_bounded_slot() {
    let metrics = Metrics::default();
    metrics.eventsub_reconcile_pass("bogus-outcome");
    metrics.eventsub_reconcile_error("bogus-class");

    let text = metrics.render(None);
    assert!(
        text.contains("two_bot_eventsub_reconcile_passes_total{outcome=\"failed\"} 1\n"),
        "unknown pass outcome must fold to failed, never grow the series"
    );
    assert!(
        text.contains("two_bot_eventsub_reconcile_errors_total{error_class=\"other\"} 1\n"),
        "unknown error class must fold to other"
    );
    assert!(
        !text.contains("bogus"),
        "untrusted input must never reach exposition"
    );
}

/// Names, label keys and exposition types follow the repo's conventions.
#[test]
fn exposition_names_labels_and_types_follow_conventions() {
    assert_eq!(
        EVENTSUB_RECONCILE_OUTCOMES,
        &["healthy", "pending", "failed"]
    );
    assert_eq!(EVENTSUB_RECONCILE_ACTIONS, &["create", "delete"]);
    assert_eq!(
        EVENTSUB_RECONCILE_ERROR_CLASSES,
        &[
            "reconcile_action_failed",
            "reconcile_prior_failed",
            "reconcile_unhealthy",
            "reconcile_failed",
            "other",
        ]
    );

    let metrics = Metrics::default();
    metrics.eventsub_reconcile_pass("healthy");
    metrics.eventsub_reconcile_action("create", 1);
    metrics.eventsub_reconcile_error("reconcile_failed");
    let text = metrics.render(None);

    for (family, label) in [
        ("two_bot_eventsub_reconcile_passes_total", "outcome"),
        ("two_bot_eventsub_reconcile_actions_total", "action"),
        ("two_bot_eventsub_reconcile_errors_total", "error_class"),
    ] {
        assert!(
            text.contains(&format!("# TYPE {family} counter\n")),
            "{family} must render as a counter"
        );
        assert!(
            text.contains(&format!("# HELP {family} ")),
            "{family} must carry HELP text"
        );
        for (labels, _) in family_samples(&text, family) {
            assert!(
                labels.starts_with(&format!("{label}=\"")) && labels.ends_with("\"}"),
                "{family} sample must carry exactly one {label} label: {labels}"
            );
        }
    }

    // No duplicate series and every sample value parses.
    let mut seen = HashSet::new();
    for line in text
        .lines()
        .filter(|line| line.starts_with("two_bot_eventsub_reconcile_") && !line.starts_with('#'))
    {
        let (series, value) = line.rsplit_once(' ').expect("sample must have a value");
        assert!(seen.insert(series.to_owned()), "duplicate series: {series}");
        assert!(value.parse::<f64>().is_ok(), "bad sample value: {line}");
    }
}

/// Cardinality is bounded per outcome, never per subscription: hostile
/// broadcaster IDs, subscription IDs and response bytes cannot grow the
/// series or leak into exposition.
#[test]
fn cardinality_is_bounded_per_outcome_not_per_subscription() {
    let metrics = Metrics::default();
    let before = metrics.render(None);
    let before_lines = before.lines().count();
    assert_eq!(
        count_family_lines(&before, "two_bot_eventsub_reconcile_passes_total"),
        3
    );
    assert_eq!(
        count_family_lines(&before, "two_bot_eventsub_reconcile_actions_total"),
        2
    );
    assert_eq!(
        count_family_lines(&before, "two_bot_eventsub_reconcile_errors_total"),
        5
    );

    for id in 0..100 {
        let hostile = format!("broadcaster-{id}\"\nsubscription-{id}=secret-{id}");
        metrics.eventsub_reconcile_pass(&hostile);
        metrics.eventsub_reconcile_action(&hostile, 1);
        metrics.eventsub_reconcile_error(&hostile);
    }

    let text = metrics.render(None);
    assert_eq!(
        text.lines().count(),
        before_lines,
        "hostile input must not grow the exposition line count"
    );
    assert_eq!(
        count_family_lines(&text, "two_bot_eventsub_reconcile_passes_total"),
        3
    );
    assert_eq!(
        count_family_lines(&text, "two_bot_eventsub_reconcile_actions_total"),
        2
    );
    assert_eq!(
        count_family_lines(&text, "two_bot_eventsub_reconcile_errors_total"),
        5
    );
    assert!(
        !text.contains("broadcaster-"),
        "broadcaster IDs must never reach labels"
    );
    assert!(
        !text.contains("subscription-"),
        "subscription IDs must never reach labels"
    );
    assert!(
        !text.contains("secret-"),
        "response bytes must never reach labels"
    );
    assert!(text.contains("two_bot_eventsub_reconcile_passes_total{outcome=\"failed\"} 100\n"));
    // Unknown actions fold into the trailing bounded slot.
    let delete_total: u64 = family_samples(&text, "two_bot_eventsub_reconcile_actions_total")
        .into_iter()
        .map(|(_, value)| value as u64)
        .sum();
    assert_eq!(
        delete_total, 100,
        "hostile actions must fold into the bounded slot"
    );
    assert!(text.contains("two_bot_eventsub_reconcile_errors_total{error_class=\"other\"} 100\n"));
}

/// The operator docs name all three reconcile series (docs/exposition parity).
#[test]
fn docs_name_the_reconcile_series() {
    let doc = std::fs::read_to_string(repository_root().join("docs/metrics.md"))
        .expect("docs/metrics.md must be readable");
    for series in [
        "two_bot_eventsub_reconcile_passes_total",
        "two_bot_eventsub_reconcile_actions_total",
        "two_bot_eventsub_reconcile_errors_total",
    ] {
        assert!(
            doc.contains(series),
            "docs/metrics.md must document {series}"
        );
    }
}
