//! Acceptance tests: moderation enforcement outcome gauges.
//!
//! Protected test file for TOG-28492 (test-author card for TOG-28482).
//! The Builder builds on this card's branch and never edits this file.
//! If a pinned name below cannot be implemented as written, stop and
//! escalate instead of changing the tests.
//!
//! Contract pinned here (TOG-28482 implements exactly this):
//! - New `two_bot_moderation_enforcements_total{verb,outcome}` counter with
//!   exactly nine verbs pinned to `ModerationAction::command_name`
//!   (`ban`, `tempban`, `kick`, `timeout`, `warn`, `purge`, `slowmode`,
//!   `lockdown`, `unlock`) and exactly five fixed outcomes (`success`,
//!   `refused`, `discord_error`, `storage_error`, `other`).
//! - Unknown verbs/outcomes collapse to the trailing allowlist slot, never
//!   to a dynamic label. No user id, reason text, message text, secret,
//!   token, or label-injection payload ever becomes a label or series.
//! - Offline only: `Metrics::default()` setter plus `render`, no network,
//!   no database, no global state.
//!
//! These tests fail before TOG-28482 (the `moderation_enforcement` setter
//! and the `MODERATION_VERBS` / `MODERATION_ENFORCEMENT_OUTCOMES`
//! allowlists do not exist) and pass against the builder head.

use std::collections::HashSet;

use two_bot_core::metrics::{Metrics, MODERATION_ENFORCEMENT_OUTCOMES, MODERATION_VERBS};
use two_bot_core::moderation::ModerationAction;

const SERIES_PREFIX: &str = "two_bot_moderation_enforcements_total{";

fn enforcement_lines(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| line.starts_with(SERIES_PREFIX))
        .collect()
}

fn count_for(text: &str, verb: &str, outcome: &str) -> u64 {
    let needle = format!("{SERIES_PREFIX}verb=\"{verb}\",outcome=\"{outcome}\"}} ");
    let line = text
        .lines()
        .find(|line| line.starts_with(&needle))
        .unwrap_or_else(|| panic!("missing series {verb}/{outcome}"));
    line.rsplit_once(' ')
        .expect("series line has a sample value")
        .1
        .parse::<u64>()
        .expect("sample value parses as u64")
}

#[test]
fn verbs_match_the_nine_moderation_command_names() {
    let names: Vec<&str> = ModerationAction::ALL
        .iter()
        .map(|action| action.command_name())
        .collect();
    assert_eq!(
        names, MODERATION_VERBS,
        "verb vocabulary must stay pinned to ModerationAction::command_name"
    );
    assert_eq!(MODERATION_VERBS.len(), 9, "exactly nine verbs");
    assert_eq!(
        MODERATION_VERBS,
        &["ban", "tempban", "kick", "timeout", "warn", "purge", "slowmode", "lockdown", "unlock",],
        "verb order follows legacy publish order"
    );
}

#[test]
fn outcomes_are_the_five_fixed_terminal_outcomes() {
    assert_eq!(
        MODERATION_ENFORCEMENT_OUTCOMES,
        &[
            "success",
            "refused",
            "discord_error",
            "storage_error",
            "other"
        ],
        "outcome vocabulary is fixed; add a new outcome only with a docs + budget row"
    );
    assert_eq!(
        MODERATION_ENFORCEMENT_OUTCOMES.last(),
        Some(&"other"),
        "unknown outcomes collapse to the trailing other slot"
    );
}

#[test]
fn every_verb_outcome_pair_records_and_renders_exactly_once() {
    // Runtime-local instance: no global state, no cross-test bleed.
    let metrics = Metrics::default();
    for verb in MODERATION_VERBS {
        for outcome in MODERATION_ENFORCEMENT_OUTCOMES {
            metrics.moderation_enforcement(verb, outcome);
        }
    }
    let text = metrics.render(None);
    for verb in MODERATION_VERBS {
        for outcome in MODERATION_ENFORCEMENT_OUTCOMES {
            assert_eq!(
                count_for(&text, verb, outcome),
                1,
                "missing or double-counted {verb}/{outcome}"
            );
        }
    }
    // Fixed cardinality: 9 verbs x 5 outcomes, no more, no fewer.
    assert_eq!(
        enforcement_lines(&text).len(),
        MODERATION_VERBS.len() * MODERATION_ENFORCEMENT_OUTCOMES.len(),
        "fixed 9x5 series cardinality"
    );
    // Every series key is unique and every sample is numeric.
    let mut seen = HashSet::new();
    for line in enforcement_lines(&text) {
        let (key, value) = line.rsplit_once(' ').expect("series line has a value");
        assert!(seen.insert(key.to_owned()), "duplicate series: {key}");
        assert!(value.parse::<u64>().is_ok(), "bad sample value: {line}");
    }
}

#[test]
fn hostile_labels_collapse_without_new_series_or_secret_capture() {
    let metrics = Metrics::default();
    let before = enforcement_lines(&metrics.render(None)).len();
    let baseline_unlock_success = count_for(&metrics.render(None), "unlock", "success");
    let baseline_ban_other = count_for(&metrics.render(None), "ban", "other");
    let baseline_unlock_other = count_for(&metrics.render(None), "unlock", "other");

    // Unbounded inputs that must never become labels: snowflake user ids,
    // free-form reason/message text, secrets/tokens, and label-injection
    // payloads with quotes, braces, commas, and newlines.
    let hostile_verbs = [
        "111111111111111111",
        "general",
        "being rude in chat",
        "sk-live-secret-token-value",
        "ban\",\nsecret=\"leaked",
        "kick\",outcome=\"success",
        "{verb=\"ban\"}",
        "",
    ];
    let hostile_outcomes = [
        "user was warned for spam",
        "122333444455555555",
        "ghp_hostiletoken12345",
        "success\",\nsecret=\"leaked",
        "other\",verb=\"ban",
        "",
    ];
    for verb in hostile_verbs {
        metrics.moderation_enforcement(verb, "success");
    }
    for outcome in hostile_outcomes {
        metrics.moderation_enforcement("ban", outcome);
    }
    // Hostile pairs on both axes at once.
    metrics.moderation_enforcement("999999999999999999", "some reason text");

    let text = metrics.render(None);
    // No new series: unknown labels collapse into the trailing slots.
    assert_eq!(
        enforcement_lines(&text).len(),
        before,
        "hostile labels must not create new series"
    );
    // Nothing hostile leaks into exposition.
    for leaked in [
        "111111111111111111",
        "being rude",
        "sk-live-secret",
        "ghp_hostiletoken",
        "secret=\"leaked",
        "some reason text",
        "general",
    ] {
        assert!(
            !text.contains(leaked),
            "hostile payload leaked into exposition: {leaked:?}"
        );
    }
    // Collapse destinations: unknown verbs land on the trailing verb slot,
    // unknown outcomes on the trailing outcome slot. The dual-hostile pair
    // lands on exactly one cell (unlock/other), so the single-axis counts
    // exclude it.
    let expected_unlock_success = baseline_unlock_success + hostile_verbs.len() as u64;
    assert_eq!(
        count_for(&text, "unlock", "success"),
        expected_unlock_success,
        "unknown verbs collapse to unlock"
    );
    assert_eq!(
        count_for(&text, "ban", "other"),
        baseline_ban_other + hostile_outcomes.len() as u64,
        "unknown outcomes collapse to other"
    );
    assert_eq!(
        count_for(&text, "unlock", "other"),
        baseline_unlock_other + 1,
        "a hostile verb with a hostile outcome collapses to unlock/other once"
    );
}

#[test]
fn metrics_instances_are_isolated_per_runtime() {
    let first = Metrics::default();
    let second = Metrics::default();
    first.moderation_enforcement("ban", "success");
    first.moderation_enforcement("ban", "success");
    assert_eq!(
        count_for(&first.render(None), "ban", "success"),
        2,
        "recording runtime counts its own samples"
    );
    assert_eq!(
        count_for(&second.render(None), "ban", "success"),
        0,
        "a fresh runtime starts at zero; no global bleed"
    );
}

#[test]
fn exposition_labels_are_only_verb_and_outcome_from_the_allowlist() {
    let metrics = Metrics::default();
    metrics.moderation_enforcement("timeout", "refused");
    metrics.moderation_enforcement("purge", "discord_error");
    let text = metrics.render(None);
    let verb_set: HashSet<&str> = MODERATION_VERBS.iter().copied().collect();
    let outcome_set: HashSet<&str> = MODERATION_ENFORCEMENT_OUTCOMES.iter().copied().collect();
    assert!(!verb_set.is_empty() && !outcome_set.is_empty());
    for line in enforcement_lines(&text) {
        let labels = line
            .split_once('{')
            .expect("series has labels")
            .1
            .rsplit_once('}')
            .expect("labels close")
            .0;
        let mut keys = Vec::new();
        for pair in labels.split(',') {
            let (key, value) = pair.split_once('=').expect("label is k=v");
            keys.push(key);
            let unquoted = value.trim_matches('"');
            match key {
                "verb" => assert!(
                    verb_set.contains(unquoted),
                    "verb label outside fixed vocabulary: {unquoted:?}"
                ),
                "outcome" => assert!(
                    outcome_set.contains(unquoted),
                    "outcome label outside fixed vocabulary: {unquoted:?}"
                ),
                _ => panic!("unexpected label key on enforcement series: {key:?}"),
            }
            // No unbounded content hides inside an allowlisted value either:
            // every rendered value is exactly one allowlist member.
            assert!(
                verb_set.contains(unquoted) || outcome_set.contains(unquoted),
                "label value outside fixed vocabulary: {unquoted:?}"
            );
        }
        assert_eq!(
            keys,
            vec!["verb", "outcome"],
            "enforcement series carries exactly verb + outcome"
        );
    }
    // Spot-check the two recorded samples render with their exact counts.
    assert!(text.contains(&format!(
        "{SERIES_PREFIX}verb=\"timeout\",outcome=\"refused\"}} 1\n"
    )));
    assert!(text.contains(&format!(
        "{SERIES_PREFIX}verb=\"purge\",outcome=\"discord_error\"}} 1\n"
    )));
}
