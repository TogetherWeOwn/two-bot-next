//! Independent fixed-clock acceptance for persisted absolute schedule times.
//! Breaks caught: wall-clock parsing of offsets, inclusive-now admission,
//! exclusive horizon admission, rounding instead of truncating fractions, and
//! accepting malformed or unbounded input. No store, runtime clock or Discord.

use two_bot_core::scheduled::validate_schedule_at;

// 2026-01-01T00:00:00.000Z; expectations are hand-calculated Unix milliseconds.
const NOW: u64 = 1_767_225_600_000;

#[test]
fn equivalent_offsets_across_calendar_dates_preserve_the_same_millisecond() {
    for at in [
        "2026-01-02T00:00:00.123Z",
        "2026-01-02T00:00:00.123+00:00",
        "2026-01-02T05:30:00.123+05:30",
        "2026-01-01T17:00:00.123-07:00",
    ] {
        assert_eq!(
            validate_schedule_at(at, NOW).expect("future explicit-offset instant"),
            1_767_312_000_123,
            "offset normalization must not reinterpret local calendar fields: {at}"
        );
    }
}

#[test]
fn same_millisecond_and_past_refuse_but_one_millisecond_ahead_is_future() {
    let now = NOW + 123;
    for at in [
        "2026-01-01T00:00:00.123Z",
        "2026-01-01T00:00:00.122Z",
        "2025-12-31T23:59:59.999Z",
        "2026-01-01T05:30:00.123+05:30",
    ] {
        assert!(validate_schedule_at(at, now).is_err(), "not future: {at}");
    }
    assert_eq!(
        validate_schedule_at("2026-01-01T00:00:00.124Z", now)
            .expect("exactly one persisted millisecond ahead"),
        1_767_225_600_124
    );
}

#[test]
fn the_525600_minute_horizon_is_inclusive_to_the_millisecond() {
    assert_eq!(525_600_u64 * 60_000, 31_536_000_000);
    assert_eq!(
        validate_schedule_at("2027-01-01T00:00:00.000Z", NOW)
            .expect("exact existing one-year horizon"),
        1_798_761_600_000
    );
    assert_eq!(
        validate_schedule_at("2027-01-01T05:30:00.000+05:30", NOW)
            .expect("inclusive horizon after offset normalization"),
        1_798_761_600_000
    );
    assert!(
        validate_schedule_at("2027-01-01T00:00:00.001Z", NOW).is_err(),
        "one millisecond beyond the horizon must not be admitted"
    );
    assert!(
        validate_schedule_at("2027-01-01T05:30:00.001+05:30", NOW).is_err(),
        "the horizon applies to the normalized instant, not offset wall time"
    );
}

#[test]
fn fractions_truncate_to_milliseconds_without_rounding_up() {
    for (at, expected) in [
        ("2026-01-01T00:00:00.001000001Z", 1_767_225_600_001),
        ("2026-01-01T00:00:00.001999999Z", 1_767_225_600_001),
        ("2026-01-01T00:00:00.999999999Z", 1_767_225_600_999),
        ("2026-01-01T05:30:00.123456789+05:30", 1_767_225_600_123),
    ] {
        assert_eq!(
            validate_schedule_at(at, NOW).expect("future persisted millisecond"),
            expected,
            "fractional digits beyond milliseconds are discarded: {at}"
        );
    }
}

#[test]
fn fractional_future_within_the_current_millisecond_still_refuses() {
    for (at, now) in [
        ("2026-01-01T00:00:00.000000001Z", NOW),
        ("2026-01-01T00:00:00.000999999Z", NOW),
        ("2026-01-01T00:00:00.123999999Z", NOW + 123),
        ("2026-01-01T05:30:00.123999999+05:30", NOW + 123),
    ] {
        assert!(
            validate_schedule_at(at, now).is_err(),
            "strict future comparison uses the persisted millisecond: {at}"
        );
    }
}

#[test]
fn malformed_offset_free_impossible_calendar_and_extreme_year_inputs_refuse() {
    for at in [
        "",
        "not-a-time @everyone",
        "2026-01-02T12:30:00",
        "2026-02-30T12:30:00Z",
        "2026-02-29T12:30:00+00:00",
        "2026-13-01T12:30:00Z",
        "2026-01-02T24:00:00Z",
        "2026-01-02T12:30:00+25:00",
        "2026-01-02T12:30:00+01:60",
        "9999-12-31T23:59:59Z",
        "+999999999999-01-01T00:00:00Z",
        "-999999999999-01-01T00:00:00Z",
    ] {
        assert!(validate_schedule_at(at, NOW).is_err(), "refuse {at:?}");
    }
}

#[test]
fn overlong_even_syntactically_valid_fractional_inputs_refuse() {
    // RFC3339 permits a fractional second of arbitrary precision. Its
    // persisted value would be tomorrow + 123 ms, but the input is over cap.
    let fractional = format!("2026-01-02T00:00:00.123{}Z", "0".repeat(41));
    assert_eq!(fractional.encode_utf16().count(), 65);
    for at in [fractional, "x".repeat(65), "😀".repeat(33)] {
        assert!(at.encode_utf16().count() > 64);
        assert!(
            validate_schedule_at(&at, NOW).is_err(),
            "overlong at refuses"
        );
    }
}
