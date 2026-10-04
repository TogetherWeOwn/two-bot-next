//! Offline pin of `docs/error-budget-alerts.md`.
//!
//! Every threshold number in the doc has exactly one assertion here: burn
//! rates, window lengths, per-window budget math, and the worked example at
//! 10,000 eligible events/hour. Changing the policy without updating this
//! file fails the suite. Nothing here touches IO, the clock, a database or
//! the network.

use super::burn_rate::*;

const EXAMPLE_EVENTS_PER_HOUR: f64 = 10_000.0;

fn example_window_events(window_secs: f64) -> u64 {
    (EXAMPLE_EVENTS_PER_HOUR * window_secs / 3600.0).round() as u64
}

fn assert_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-9,
        "expected {expected}, got {actual}"
    );
}

#[test]
fn fast_burn_windows_match_doc() {
    assert_close(FAST_BURN_LONG_WINDOW_SECS, 15.0 * 60.0);
    assert_close(FAST_BURN_SHORT_WINDOW_SECS, 2.0 * 60.0);
    assert_close(FAST_BURN_RATE, 12.0);
}

#[test]
fn slow_burn_windows_match_doc() {
    assert_close(SLOW_BURN_LONG_WINDOW_SECS, 2.0 * 3600.0);
    assert_close(SLOW_BURN_SHORT_WINDOW_SECS, 15.0 * 60.0);
    assert_close(SLOW_BURN_RATE, 3.0);
}

#[test]
fn budget_ratio_and_period_match_doc() {
    assert_close(BUDGET_ERROR_RATIO, 0.001);
    assert_close(WATCH_PERIOD_SECS, 172_800.0);
}

#[test]
fn burn_rate_is_ratio_over_budget() {
    assert_close(burn_rate(0.012, BUDGET_ERROR_RATIO), 12.0);
    assert_close(burn_rate(0.003, BUDGET_ERROR_RATIO), 3.0);
    assert_close(burn_rate(BUDGET_ERROR_RATIO, BUDGET_ERROR_RATIO), 1.0);
    assert_close(burn_rate(0.0, BUDGET_ERROR_RATIO), 0.0);
}

#[test]
fn per_window_budget_math_matches_doc() {
    assert_close(
        budget_fraction_consumed(
            FAST_BURN_RATE,
            FAST_BURN_LONG_WINDOW_SECS,
            WATCH_PERIOD_SECS,
        ),
        0.0625,
    );
    assert_close(
        budget_fraction_consumed(
            SLOW_BURN_RATE,
            SLOW_BURN_LONG_WINDOW_SECS,
            WATCH_PERIOD_SECS,
        ),
        0.125,
    );
}

#[test]
fn worked_example_page_and_ticket_counts() {
    // 10,000 events/hour: 2,500 per 15 min, 20,000 per 2 h.
    assert_eq!(example_window_events(FAST_BURN_LONG_WINDOW_SECS), 2_500);
    assert_eq!(example_window_events(SLOW_BURN_LONG_WINDOW_SECS), 20_000);
    assert_eq!(
        allowed_errors(2_500, BUDGET_ERROR_RATIO, FAST_BURN_RATE),
        30
    );
    assert_eq!(
        allowed_errors(20_000, BUDGET_ERROR_RATIO, SLOW_BURN_RATE),
        60
    );
}

#[test]
fn worked_example_short_window_confirmations() {
    // ~333 events per 2 min round up to a 4-error page confirmation;
    // 2,500 per 15 min round up to an 8-error ticket confirmation.
    assert_eq!(example_window_events(FAST_BURN_SHORT_WINDOW_SECS), 333);
    assert_eq!(allowed_errors(333, BUDGET_ERROR_RATIO, FAST_BURN_RATE), 4);
    assert_eq!(allowed_errors(2_500, BUDGET_ERROR_RATIO, SLOW_BURN_RATE), 8);
}

#[test]
fn edge_cases_stay_total() {
    assert_eq!(allowed_errors(0, BUDGET_ERROR_RATIO, FAST_BURN_RATE), 0);
    assert_eq!(allowed_errors(2_500, BUDGET_ERROR_RATIO, 0.0), 0);
    assert!(burn_rate(0.001, 0.0).is_infinite());
    assert_close(burn_rate(0.0, 0.0), 0.0);
}
