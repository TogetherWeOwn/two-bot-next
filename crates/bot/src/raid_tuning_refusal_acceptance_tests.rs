#![cfg(test)]
//! Protected, settings-only acceptance. No environment, database or Discord IO.
//!
//! The test-only fallback calls the baseline reader and returns its real output
//! as Ok. An inherent `RaidSettings::try_from_vars` takes precedence once added.
//! Thus the unpatched baseline compiles and fails on accepted malformed input,
//! not on a missing API. Keep the legacy reader for compatibility controls.

use std::collections::HashMap;

use crate::raid_runtime::RaidSettings;

const WINDOW: &str = "TWO_RAID_WINDOW_SECONDS";
const THRESHOLD: &str = "TWO_RAID_JOIN_THRESHOLD";
const CHANNEL: &str = "DISCORD_STAFF_ALERT_CHANNEL_ID";

trait BaselineReader: Sized {
    fn try_from_vars(vars: &HashMap<String, String>) -> Result<Self, String>;
}

impl BaselineReader for RaidSettings {
    fn try_from_vars(vars: &HashMap<String, String>) -> Result<Self, String> {
        Ok(Self::from_vars(vars))
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
        let result = RaidSettings::try_from_vars(&vars(&[(key, raw)]));
        let error = result.expect_err("present malformed raid input must refuse the whole read");
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
fn raid_tuning_refusal_acceptance_threshold_syntax() {
    refuses(THRESHOLD, &["eight", "8oops", "1,5"]);
}

#[test]
fn raid_tuning_refusal_acceptance_threshold_domain() {
    refuses(THRESHOLD, &["0", "-3", "NaN", "inf", "-inf", "1e309"]);
}

#[test]
fn raid_tuning_refusal_acceptance_window_syntax() {
    refuses(WINDOW, &["thirty", "30seconds", "1,5"]);
}

#[test]
fn raid_tuning_refusal_acceptance_window_domain() {
    refuses(WINDOW, &["0", "-3", "NaN", "inf", "-inf", "1e309"]);
}

#[test]
fn raid_tuning_refusal_acceptance_staff_channel() {
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
fn raid_tuning_refusal_acceptance_diagnostic_does_not_echo_input() {
    let marker = "raid-invalid-private-value-sentinel";
    for key in [WINDOW, THRESHOLD, CHANNEL] {
        let error = RaidSettings::try_from_vars(&vars(&[(key, marker)]))
            .expect_err("malformed input must produce a refusal");
        assert!(error.to_string().contains(key));
        assert!(!error.to_string().contains(marker));
        assert!(!format!("{error:?}").contains(marker));
    }
}

#[test]
fn raid_tuning_refusal_acceptance_missing_and_empty_keep_defaults_log_only() {
    for input in [
        vars(&[]),
        vars(&[(WINDOW, ""), (THRESHOLD, ""), (CHANNEL, "")]),
        vars(&[(WINDOW, " \t"), (THRESHOLD, "\n"), (CHANNEL, " \t\n")]),
    ] {
        let parsed = RaidSettings::try_from_vars(&input).expect("empty keys retain defaults");
        assert_eq!(parsed.tuning.window_seconds().to_bits(), 60.0_f64.to_bits());
        assert_eq!(parsed.tuning.threshold().to_bits(), 5.0_f64.to_bits());
        assert_eq!(parsed.staff_channel, None, "no channel may be guessed");
        assert_eq!(
            parsed,
            <RaidSettings as BaselineReader>::try_from_vars(&input).expect("legacy read")
        );
    }
}

#[test]
fn raid_tuning_refusal_acceptance_valid_config_is_byte_identical() {
    let input = vars(&[
        (WINDOW, " 30.5 "),
        (THRESHOLD, " 8.25 "),
        (CHANNEL, " 123456789012345678 "),
    ]);
    let parsed = RaidSettings::try_from_vars(&input).expect("valid tuning is accepted");
    assert_eq!(parsed.tuning.window_seconds().to_bits(), 30.5_f64.to_bits());
    assert_eq!(parsed.tuning.threshold().to_bits(), 8.25_f64.to_bits());
    assert_eq!(
        parsed.staff_channel.as_deref().map(str::as_bytes),
        Some(b"123456789012345678".as_slice())
    );
    assert_eq!(
        parsed,
        <RaidSettings as BaselineReader>::try_from_vars(&input).expect("legacy read")
    );
}

#[test]
fn raid_tuning_refusal_acceptance_empty_channel_keeps_valid_tuning() {
    let parsed = RaidSettings::try_from_vars(&vars(&[
        (WINDOW, "1e1"),
        (THRESHOLD, "2.5"),
        (CHANNEL, " "),
    ]))
    .expect("an empty optional channel is not malformed");
    assert_eq!(parsed.tuning.window_seconds().to_bits(), 10.0_f64.to_bits());
    assert_eq!(parsed.tuning.threshold().to_bits(), 2.5_f64.to_bits());
    assert_eq!(parsed.staff_channel, None);
}
