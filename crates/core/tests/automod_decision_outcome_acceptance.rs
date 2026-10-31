//! Acceptance tests: automod decision outcome gauges.
//!
//! Automod inspections end in a terminal [`CompletionKind`] but dashboards
//! cannot break down accept, dry-run, protected, ladder and refusal decisions
//! without log sampling. The shared activation records each committed receipt
//! once in the existing metrics registry as
//! `two_bot_automod_decisions_total{outcome}`.
//!
//! These tests pin the contract at the registry/exposition layer, offline:
//! no database, no Discord gateway, no production mutation. Fixtures carry no
//! secrets, tokens, user IDs or message text as labels — hostile strings must
//! fold into the bounded trailing slot, never grow the series or leak into
//! exposition.
//!
//! Each test uses a runtime-local [`Metrics`], never the process-global, so
//! parallel suites cannot flake the exact-value asserts.
//!
//! Failing-first note: every test here binds to builder
//! (`metrics: emit automod decision outcome gauges`) APIs —
//! [`Metrics::automod_decision`], `AUTOMOD_DECISION_OUTCOMES` and
//! [`CompletionKind::as_str`] — none of which exist on the pre-builder head,
//! so this file does not compile there and goes green on the builder head.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use two_bot_core::automod_runtime::CompletionKind;
use two_bot_core::metrics::{Metrics, AUTOMOD_DECISION_OUTCOMES};

const FAMILY: &str = "two_bot_automod_decisions_total";

const ALL_KINDS: [CompletionKind; 8] = [
    CompletionKind::Accepted,
    CompletionKind::AlreadyProcessed,
    CompletionKind::DryRun,
    CompletionKind::Protected,
    CompletionKind::Deleted,
    CompletionKind::Warned,
    CompletionKind::TimedOut,
    CompletionKind::SanctionRefused,
];

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

/// Every terminal outcome records exactly one sample per committed receipt.
#[test]
fn decisions_increment_once_per_completion_kind() {
    let metrics = Metrics::default();
    let text = metrics.render(None);
    for outcome in AUTOMOD_DECISION_OUTCOMES {
        assert!(
            text.contains(&format!("{FAMILY}{{outcome=\"{outcome}\"}} 0\n")),
            "fresh registry must expose a zero {outcome} decision series"
        );
    }

    for kind in ALL_KINDS {
        metrics.automod_decision(kind.as_str());
    }

    let text = metrics.render(None);
    for kind in ALL_KINDS {
        let outcome = kind.as_str();
        assert!(
            text.contains(&format!("{FAMILY}{{outcome=\"{outcome}\"}} 1\n")),
            "{outcome} decision must increment exactly once"
        );
    }
}

/// The runtime vocabulary is exactly the fixed eight-outcome allowlist, in
/// declaration order, so no caller can smuggle a ninth label value.
#[test]
fn completion_kind_vocabulary_is_the_fixed_allowlist() {
    assert_eq!(
        AUTOMOD_DECISION_OUTCOMES,
        &[
            "accepted",
            "already_processed",
            "dry_run",
            "protected",
            "deleted",
            "warned",
            "timed_out",
            "sanction_refused",
        ]
    );
    let kinds: Vec<&str> = ALL_KINDS.map(CompletionKind::as_str).to_vec();
    assert_eq!(
        kinds.as_slice(),
        AUTOMOD_DECISION_OUTCOMES,
        "CompletionKind::as_str must mirror the metrics allowlist in order"
    );
}

/// Unknown outcomes fail closed into the bounded trailing slot.
#[test]
fn unknown_outcomes_fail_closed_into_bounded_slot() {
    let metrics = Metrics::default();
    metrics.automod_decision("bogus-outcome");

    let text = metrics.render(None);
    assert!(
        text.contains(&format!("{FAMILY}{{outcome=\"sanction_refused\"}} 1\n")),
        "unknown decision outcome must fold to sanction_refused, never grow the series"
    );
    assert_eq!(
        count_family_lines(&text, FAMILY),
        8,
        "an unknown outcome must not add a series"
    );
    assert!(
        !text.contains("bogus"),
        "untrusted input must never reach exposition"
    );
}

/// Names, label keys and exposition types follow the repo's conventions.
#[test]
fn exposition_names_labels_and_types_follow_conventions() {
    let metrics = Metrics::default();
    for kind in ALL_KINDS {
        metrics.automod_decision(kind.as_str());
    }
    let text = metrics.render(None);

    assert!(
        text.contains(&format!("# TYPE {FAMILY} counter\n")),
        "{FAMILY} must render as a counter"
    );
    assert!(
        text.contains(&format!("# HELP {FAMILY} ")),
        "{FAMILY} must carry HELP text"
    );
    for (labels, _) in family_samples(&text, FAMILY) {
        assert!(
            labels.starts_with("outcome=\"") && labels.ends_with("\"}"),
            "{FAMILY} sample must carry exactly one outcome label: {labels}"
        );
    }

    // No duplicate series and every sample value parses.
    let mut seen = HashSet::new();
    for line in text
        .lines()
        .filter(|line| line.starts_with(FAMILY) && !line.starts_with('#'))
    {
        let (series, value) = line.rsplit_once(' ').expect("sample must have a value");
        assert!(seen.insert(series.to_owned()), "duplicate series: {series}");
        assert!(value.parse::<f64>().is_ok(), "bad sample value: {line}");
    }
}

/// Cardinality is bounded per outcome, never per message: hostile message
/// text, user IDs, secret-looking tokens and exposition-injection metachar
/// sequences cannot grow the series or leak into exposition.
#[test]
fn cardinality_is_bounded_per_outcome_not_per_message() {
    let metrics = Metrics::default();
    let before = metrics.render(None);
    let before_lines = before.lines().count();
    assert_eq!(count_family_lines(&before, FAMILY), 8);

    for id in 0..100 {
        let hostile =
            format!("user-{id} posted <message-{id}> bearer secret-{id} \"}}\nFAMILY{{x=\"y");
        metrics.automod_decision(&hostile);
    }

    let text = metrics.render(None);
    assert_eq!(
        text.lines().count(),
        before_lines,
        "hostile input must not grow the exposition line count"
    );
    assert_eq!(
        count_family_lines(&text, FAMILY),
        8,
        "hostile input must not add decision series"
    );
    assert!(
        !text.contains("posted <message"),
        "message text must never reach labels"
    );
    assert!(
        !text.contains("secret-"),
        "secret-looking tokens must never reach labels"
    );
    assert!(
        !text.contains("FAMILY{"),
        "injection metacharacters must never reach exposition"
    );
    assert!(
        text.contains(&format!("{FAMILY}{{outcome=\"sanction_refused\"}} 100\n")),
        "hostile outcomes must fold into the bounded trailing slot"
    );
    // Every other outcome stays untouched: hostile input moves no real count.
    let real_total: u64 = family_samples(&text, FAMILY)
        .into_iter()
        .filter(|(labels, _)| !labels.contains("sanction_refused"))
        .map(|(_, value)| value as u64)
        .sum();
    assert_eq!(
        real_total, 0,
        "hostile input must not inflate any real outcome"
    );
}

/// The operator docs name the decision series (docs/exposition parity).
#[test]
fn docs_name_the_decision_series() {
    let doc = std::fs::read_to_string(repository_root().join("docs/metrics.md"))
        .expect("docs/metrics.md must be readable");
    assert!(
        doc.contains(FAMILY),
        "docs/metrics.md must document {FAMILY}"
    );
    for outcome in AUTOMOD_DECISION_OUTCOMES {
        assert!(
            doc.contains(outcome),
            "docs/metrics.md must name the {outcome} outcome"
        );
    }
}
