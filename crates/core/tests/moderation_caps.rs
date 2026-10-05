//! Dedicated cap-refusal enforcement for moderation durations and
//! schedule/sticky bounds (TOG-11718).
//!
//! Builders advertise the same duration minima and maxima enforced by the
//! validators in `crate::moderation`. This suite pins every inclusive edge
//! plus the adjacent refusal,
//! missing/non-integer refusal, and the no-echo error-text rule. Synthetic
//! fixtures only: no Discord, network, or database.

use serde_json::{json, Value};
use two_bot_core::commands::{
    SCHEDULE_EVERY_MINUTES_MAX, SCHEDULE_EVERY_MINUTES_MIN, SCHEDULE_IN_MINUTES_MAX,
    SCHEDULE_IN_MINUTES_MIN, STICKY_DEBOUNCE_MAX_SECONDS, STICKY_DEBOUNCE_MIN_SECONDS,
    TEMPBAN_DURATION_MAX_SECONDS, TEMPBAN_DURATION_MIN_SECONDS, TIMEOUT_DURATION_MAX_SECONDS,
    TIMEOUT_DURATION_MIN_SECONDS,
};
use two_bot_core::moderation::{
    moderation_commands, validate_schedule_every_minutes, validate_schedule_in_minutes,
    validate_sticky_debounce, validate_tempban_duration, validate_timeout_duration,
};

fn assert_edges(
    validate: fn(Option<&Value>) -> Result<i64, two_bot_core::moderation::ModerationCapError>,
    min: i64,
    max: i64,
) {
    // Inclusive edges accept.
    for n in [min, min + 1, max - 1, max] {
        let value = json!(n);
        assert_eq!(
            validate(Some(&value)),
            Ok(n),
            "inclusive value {n} in [{min}, {max}] must accept"
        );
    }
    // Adjacent outsiders refuse.
    for n in [min - 1, max + 1] {
        let value = json!(n);
        assert!(
            validate(Some(&value)).is_err(),
            "adjacent value {n} outside [{min}, {max}] must refuse"
        );
    }
}

fn assert_refuses_missing_and_non_integer(
    validate: fn(Option<&Value>) -> Result<i64, two_bot_core::moderation::ModerationCapError>,
    field: &str,
    min: i64,
    max: i64,
) {
    // Missing refuses.
    let err = validate(None).expect_err("missing field must refuse");
    assert_eq!(err.field, field);
    assert_eq!((err.min, err.max), (min, max));

    // Non-integer JSON refuses: string, float, bool, null, array, object.
    for invalid in [
        json!("not-a-number"),
        json!(1.5),
        json!(true),
        Value::Null,
        json!([1]),
        json!({"n": 1}),
    ] {
        assert!(
            validate(Some(&invalid)).is_err(),
            "non-integer {invalid} must refuse for {field}"
        );
    }

    // Wire strings never coerce the way legacy `Number()` did.
    let coerced = json!(min.to_string());
    assert!(
        validate(Some(&coerced)).is_err(),
        "numeric string must refuse for {field}"
    );
}

fn assert_error_names_bound_without_echo(
    validate: fn(Option<&Value>) -> Result<i64, two_bot_core::moderation::ModerationCapError>,
    field: &str,
    min: i64,
    max: i64,
) {
    // A hostile payload must not be reflected in the error text.
    let hostile = json!("evil-input-987654321");
    let err = validate(Some(&hostile)).expect_err("hostile input must refuse");
    let text = err.to_string();
    assert!(text.contains(field), "error must name the field: {text}");
    assert!(
        text.contains(&min.to_string()) && text.contains(&max.to_string()),
        "error must name both bounds: {text}"
    );
    assert!(
        !text.contains("evil-input-987654321"),
        "error must not echo user input: {text}"
    );

    // Same rule for an out-of-range integer: bounds named, input echoed nowhere
    // beyond the bound digits themselves.
    let over = json!(max + 1);
    let err = validate(Some(&over)).expect_err("max+1 must refuse");
    let text = err.to_string();
    assert!(text.contains(field));
    assert!(text.contains(&min.to_string()) && text.contains(&max.to_string()));
}

#[test]
fn tempban_caps_at_365_days() {
    assert_eq!(TEMPBAN_DURATION_MIN_SECONDS, 60);
    assert_eq!(TEMPBAN_DURATION_MAX_SECONDS, 365 * 24 * 60 * 60);
    assert_eq!(TEMPBAN_DURATION_MAX_SECONDS, 31_536_000);
    assert_edges(
        validate_tempban_duration,
        TEMPBAN_DURATION_MIN_SECONDS,
        TEMPBAN_DURATION_MAX_SECONDS,
    );
    assert_refuses_missing_and_non_integer(
        validate_tempban_duration,
        "duration_seconds",
        TEMPBAN_DURATION_MIN_SECONDS,
        TEMPBAN_DURATION_MAX_SECONDS,
    );
    assert_error_names_bound_without_echo(
        validate_tempban_duration,
        "duration_seconds",
        TEMPBAN_DURATION_MIN_SECONDS,
        TEMPBAN_DURATION_MAX_SECONDS,
    );
    // Exact acceptance pins: 365d ok, 365d+1 refuses.
    assert_eq!(
        validate_tempban_duration(Some(&json!(31_536_000))),
        Ok(31_536_000)
    );
    assert!(validate_tempban_duration(Some(&json!(31_536_001))).is_err());
}

#[test]
fn timeout_caps_at_28_days() {
    assert_eq!(TIMEOUT_DURATION_MIN_SECONDS, 60);
    assert_eq!(TIMEOUT_DURATION_MAX_SECONDS, 28 * 24 * 60 * 60);
    assert_eq!(TIMEOUT_DURATION_MAX_SECONDS, 2_419_200);
    assert_edges(
        validate_timeout_duration,
        TIMEOUT_DURATION_MIN_SECONDS,
        TIMEOUT_DURATION_MAX_SECONDS,
    );
    assert_refuses_missing_and_non_integer(
        validate_timeout_duration,
        "duration_seconds",
        TIMEOUT_DURATION_MIN_SECONDS,
        TIMEOUT_DURATION_MAX_SECONDS,
    );
    assert_error_names_bound_without_echo(
        validate_timeout_duration,
        "duration_seconds",
        TIMEOUT_DURATION_MIN_SECONDS,
        TIMEOUT_DURATION_MAX_SECONDS,
    );
    // Exact acceptance pins: 28d ok, 28d+1 refuses.
    assert_eq!(
        validate_timeout_duration(Some(&json!(2_419_200))),
        Ok(2_419_200)
    );
    assert!(validate_timeout_duration(Some(&json!(2_419_201))).is_err());
}

#[test]
fn schedule_windows_enforce_inclusive_edges() {
    assert_eq!(
        (SCHEDULE_IN_MINUTES_MIN, SCHEDULE_IN_MINUTES_MAX),
        (1, 525_600)
    );
    assert_eq!(
        (SCHEDULE_EVERY_MINUTES_MIN, SCHEDULE_EVERY_MINUTES_MAX),
        (60, 525_600)
    );
    assert_edges(
        validate_schedule_in_minutes,
        SCHEDULE_IN_MINUTES_MIN,
        SCHEDULE_IN_MINUTES_MAX,
    );
    assert_edges(
        validate_schedule_every_minutes,
        SCHEDULE_EVERY_MINUTES_MIN,
        SCHEDULE_EVERY_MINUTES_MAX,
    );
    assert_refuses_missing_and_non_integer(
        validate_schedule_in_minutes,
        "in-minutes",
        SCHEDULE_IN_MINUTES_MIN,
        SCHEDULE_IN_MINUTES_MAX,
    );
    assert_refuses_missing_and_non_integer(
        validate_schedule_every_minutes,
        "every-minutes",
        SCHEDULE_EVERY_MINUTES_MIN,
        SCHEDULE_EVERY_MINUTES_MAX,
    );
    assert_error_names_bound_without_echo(
        validate_schedule_in_minutes,
        "in-minutes",
        SCHEDULE_IN_MINUTES_MIN,
        SCHEDULE_IN_MINUTES_MAX,
    );
    assert_error_names_bound_without_echo(
        validate_schedule_every_minutes,
        "every-minutes",
        SCHEDULE_EVERY_MINUTES_MIN,
        SCHEDULE_EVERY_MINUTES_MAX,
    );
}

#[test]
fn sticky_debounce_enforces_inclusive_edges() {
    assert_eq!(
        (STICKY_DEBOUNCE_MIN_SECONDS, STICKY_DEBOUNCE_MAX_SECONDS),
        (1, 300)
    );
    assert_edges(
        validate_sticky_debounce,
        STICKY_DEBOUNCE_MIN_SECONDS,
        STICKY_DEBOUNCE_MAX_SECONDS,
    );
    assert_refuses_missing_and_non_integer(
        validate_sticky_debounce,
        "debounce",
        STICKY_DEBOUNCE_MIN_SECONDS,
        STICKY_DEBOUNCE_MAX_SECONDS,
    );
    assert_error_names_bound_without_echo(
        validate_sticky_debounce,
        "debounce",
        STICKY_DEBOUNCE_MIN_SECONDS,
        STICKY_DEBOUNCE_MAX_SECONDS,
    );
}

#[test]
fn builders_advertise_the_documented_parity_bounds() {
    let definitions = moderation_commands();
    for (name, max) in [
        ("tempban", TEMPBAN_DURATION_MAX_SECONDS),
        ("timeout", TIMEOUT_DURATION_MAX_SECONDS),
    ] {
        let definition = definitions
            .iter()
            .find(|d| d.name == name)
            .expect("verb published");
        let duration = definition
            .options
            .iter()
            .find(|o| o.name == "duration_seconds")
            .expect("duration option published");
        // Next's picker mirrors the service ceiling as well as the legacy floor.
        assert_eq!(
            (duration.min_value, duration.max_value),
            (Some(60), Some(max)),
            "{name} builder must advertise inclusive bounds 60..={max}"
        );
    }

    let automation = two_bot_core::automation_commands();
    let schedule = automation
        .iter()
        .find(|d| d.name == "schedule")
        .expect("schedule published");
    for (option, min, max) in [
        (
            "in-minutes",
            SCHEDULE_IN_MINUTES_MIN,
            SCHEDULE_IN_MINUTES_MAX,
        ),
        (
            "every-minutes",
            SCHEDULE_EVERY_MINUTES_MIN,
            SCHEDULE_EVERY_MINUTES_MAX,
        ),
    ] {
        let published = schedule
            .options
            .iter()
            .find(|o| o.name == option)
            .expect("timing option published");
        assert_eq!(
            (published.min_value, published.max_value),
            (Some(min), Some(max)),
            "schedule {option} must publish [{min}, {max}]"
        );
    }
    let sticky = automation
        .iter()
        .find(|d| d.name == "sticky")
        .expect("sticky published");
    let debounce = sticky
        .options
        .iter()
        .find(|o| o.name == "debounce")
        .expect("debounce option published");
    assert_eq!(
        (debounce.min_value, debounce.max_value),
        (
            Some(STICKY_DEBOUNCE_MIN_SECONDS),
            Some(STICKY_DEBOUNCE_MAX_SECONDS)
        )
    );
}
