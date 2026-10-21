#![cfg(test)]
//! Protected, settings-only acceptance. No environment, database or Discord IO.
//!
//! The fallback transparently wraps the baseline reader's real output in Ok;
//! an inherent `ContainmentSettings::try_from_vars` supersedes it. The ID-list
//! adapter similarly accepts the legacy HashSet or a fallible replacement,
//! allowing behavioral RED without inventing an error or a missing-API failure.

use std::collections::{HashMap, HashSet};
use std::fmt::{Debug, Display};

use crate::containment_runtime::{id_list, ContainmentSettings};

const WINDOW: &str = "TWO_ANTI_NUKE_WINDOW_SECONDS";
const MAX_AGE: &str = "TWO_ANTI_NUKE_EVENT_MAX_AGE_SECONDS";
const HEAT: &str = "TWO_ANTI_NUKE_HEAT_THRESHOLD";
const CHANNEL: &str = "DISCORD_STAFF_ALERT_CHANNEL_ID";
const TRUSTED: &str = "TWO_ANTI_NUKE_TRUSTED_USER_IDS";
const PROTECTED: &str = "TWO_ANTI_NUKE_PROTECTED_USER_IDS";

trait BaselineReader: Sized {
    fn try_from_vars(vars: &HashMap<String, String>) -> Result<Self, String>;
}

impl BaselineReader for ContainmentSettings {
    fn try_from_vars(vars: &HashMap<String, String>) -> Result<Self, String> {
        Ok(Self::from_vars(vars))
    }
}

trait IdListResult {
    fn acceptance_result(self) -> Result<HashSet<String>, String>;
}

impl IdListResult for HashSet<String> {
    fn acceptance_result(self) -> Result<HashSet<String>, String> {
        Ok(self)
    }
}

impl<E: Debug + Display> IdListResult for Result<HashSet<String>, E> {
    fn acceptance_result(self) -> Result<HashSet<String>, String> {
        self.map_err(|error| format!("{error}; {error:?}"))
    }
}

fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn refuses(key: &str, values: &[&str]) {
    for raw in values {
        let result = ContainmentSettings::try_from_vars(&vars(&[(key, raw)]));
        let error =
            result.expect_err("present malformed containment input must refuse the whole read");
        let message = error.to_string();
        assert!(
            message.contains(key),
            "refusal must identify {key}: {message}"
        );
        assert!(
            message.to_ascii_lowercase().contains("refus"),
            "operator-visible refusal, not silent substitution: {message}"
        );
    }
}

#[test]
fn containment_tuning_refusal_acceptance_heat_syntax() {
    refuses(HEAT, &["eight", "8oops", "1.5", "18446744073709551616"]);
}

#[test]
fn containment_tuning_refusal_acceptance_heat_domain() {
    refuses(HEAT, &["0", "-3", "NaN", "inf"]);
}

#[test]
fn containment_tuning_refusal_acceptance_window() {
    refuses(
        WINDOW,
        &["thirty", "0", "-3", "NaN", "inf", "0.0004", "1e308"],
    );
}

#[test]
fn containment_tuning_refusal_acceptance_max_age() {
    refuses(
        MAX_AGE,
        &["ninety", "0", "-3", "NaN", "inf", "0.0004", "1e308"],
    );
}

#[test]
fn containment_tuning_refusal_acceptance_staff_channel() {
    refuses(
        CHANNEL,
        &[
            "not-a-channel",
            "1234567890123456",
            "123456789012345678901",
            "12345678901234567x",
            "<#123456789012345678>",
            "123456789012345678,223456789012345678",
        ],
    );
}

#[test]
fn containment_tuning_refusal_acceptance_diagnostic_does_not_echo_input() {
    let marker = "containment-invalid-private-value-sentinel";
    for key in [WINDOW, MAX_AGE, HEAT, CHANNEL] {
        let error = ContainmentSettings::try_from_vars(&vars(&[(key, marker)]))
            .expect_err("malformed input must produce a refusal");
        assert!(error.to_string().contains(key));
        assert!(!error.to_string().contains(marker));
        assert!(!format!("{error:?}").contains(marker));
    }
}

fn refuses_ids(key: &str) {
    for raw in [
        "not-a-user",
        "1234567890123456",
        "123456789012345678901",
        "12345678901234567x",
        "<@123456789012345678>",
        "123456789012345678,not-a-user,223456789012345678",
    ] {
        let result = id_list(&vars(&[(key, raw)]), key).acceptance_result();
        let message = result.expect_err("one malformed ID must refuse the entire list");
        assert!(
            message.contains(key),
            "refusal must identify {key}: {message}"
        );
        assert!(message.to_ascii_lowercase().contains("refus"), "{message}");
    }
}

#[test]
fn containment_tuning_refusal_acceptance_trusted_ids() {
    refuses_ids(TRUSTED);
}

#[test]
fn containment_tuning_refusal_acceptance_protected_ids() {
    refuses_ids(PROTECTED);
}

#[test]
fn containment_tuning_refusal_acceptance_id_diagnostic_does_not_echo_input() {
    let marker = "user-list-invalid-private-value-sentinel";
    for key in [TRUSTED, PROTECTED] {
        let message = id_list(&vars(&[(key, marker)]), key)
            .acceptance_result()
            .expect_err("malformed ID must produce a refusal");
        assert!(message.contains(key));
        assert!(
            !message.contains(marker),
            "Display and Debug must redact values"
        );
    }
}

#[test]
fn containment_tuning_refusal_acceptance_missing_and_empty_keep_defaults_log_only() {
    for input in [
        vars(&[]),
        vars(&[(WINDOW, ""), (MAX_AGE, ""), (HEAT, ""), (CHANNEL, "")]),
        vars(&[
            (WINDOW, " \t"),
            (MAX_AGE, "\n"),
            (HEAT, " "),
            (CHANNEL, "\t"),
        ]),
    ] {
        let parsed =
            ContainmentSettings::try_from_vars(&input).expect("empty keys retain defaults");
        assert_eq!(parsed.window_ms, 60_000);
        assert_eq!(parsed.max_age_ms, 120_000);
        assert_eq!(parsed.heat_threshold, 5);
        assert_eq!(parsed.staff_channel, None, "no channel may be guessed");
        assert_eq!(
            parsed,
            <ContainmentSettings as BaselineReader>::try_from_vars(&input).expect("legacy read")
        );
    }
}

#[test]
fn containment_tuning_refusal_acceptance_valid_config_is_byte_identical() {
    let input = vars(&[
        (WINDOW, " 30.5 "),
        (MAX_AGE, " 90 "),
        (HEAT, " 8 "),
        (CHANNEL, " 123456789012345678 "),
    ]);
    let parsed = ContainmentSettings::try_from_vars(&input).expect("valid tuning is accepted");
    assert_eq!(parsed.window_ms, 30_500);
    assert_eq!(parsed.max_age_ms, 90_000);
    assert_eq!(parsed.heat_threshold, 8);
    assert_eq!(
        parsed.staff_channel.as_deref().map(str::as_bytes),
        Some(b"123456789012345678".as_slice())
    );
    assert_eq!(
        parsed,
        <ContainmentSettings as BaselineReader>::try_from_vars(&input).expect("legacy read")
    );
}

#[test]
fn containment_tuning_refusal_acceptance_valid_fractional_ms_still_truncate() {
    for (raw, expected) in [("0.001", 1), ("0.0015", 1), ("1.0005", 1_000)] {
        let parsed = ContainmentSettings::try_from_vars(&vars(&[(WINDOW, raw), (MAX_AGE, raw)]))
            .expect("existing positive millisecond conversion remains valid");
        assert_eq!(parsed.window_ms, expected);
        assert_eq!(parsed.max_age_ms, expected);
        assert_eq!(parsed.heat_threshold, 5);
        assert_eq!(parsed.staff_channel, None);
    }
}

#[test]
fn containment_tuning_refusal_acceptance_empty_lists_remain_empty() {
    for key in [TRUSTED, PROTECTED] {
        for input in [vars(&[]), vars(&[(key, "")]), vars(&[(key, " \t, ,\n")])] {
            assert!(id_list(&input, key)
                .acceptance_result()
                .expect("missing and empty ID entries retain an empty set")
                .is_empty());
        }
    }
}

#[test]
fn containment_tuning_refusal_acceptance_valid_lists_keep_trimmed_bytes_and_deduplicate() {
    for key in [TRUSTED, PROTECTED] {
        let parsed = id_list(
            &vars(&[(
                key,
                " 123456789012345678, ,223456789012345678,123456789012345678 ",
            )]),
            key,
        )
        .acceptance_result()
        .expect("valid IDs and empty CSV entries retain existing semantics");
        assert_eq!(
            parsed,
            ["123456789012345678", "223456789012345678"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
    }
}
