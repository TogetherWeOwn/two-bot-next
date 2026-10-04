//! Pure error-budget burn math for the 48h-watch alerts in
//! `docs/error-budget-alerts.md`.
//!
//! No IO, no clock, no database, no network: every function maps numbers to
//! numbers so the alert thresholds stay testable offline. The alert policy
//! (burn rates, windows, worked example) is pinned by
//! `burn_rate_tests.rs`; keep the two in sync.

/// Error budget ratio of the 48h watch: 99.9% SLO leaves 0.1% spendable.
pub const BUDGET_ERROR_RATIO: f64 = 0.001;

/// Watch period: 48 hours in seconds.
pub const WATCH_PERIOD_SECS: f64 = 48.0 * 3600.0;

/// Fast-burn page threshold: burn rate 12 over a 15-minute long window,
/// confirmed by a 2-minute short window.
pub const FAST_BURN_RATE: f64 = 12.0;
/// Fast-burn long window in seconds.
pub const FAST_BURN_LONG_WINDOW_SECS: f64 = 15.0 * 60.0;
/// Fast-burn short confirmation window in seconds.
pub const FAST_BURN_SHORT_WINDOW_SECS: f64 = 2.0 * 60.0;

/// Slow-burn ticket threshold: burn rate 3 over a 2-hour long window,
/// confirmed by a 15-minute short window.
pub const SLOW_BURN_RATE: f64 = 3.0;
/// Slow-burn long window in seconds.
pub const SLOW_BURN_LONG_WINDOW_SECS: f64 = 2.0 * 3600.0;
/// Slow-burn short confirmation window in seconds.
pub const SLOW_BURN_SHORT_WINDOW_SECS: f64 = 15.0 * 60.0;

/// Burn rate: how fast the budget is spent relative to an exactly-on-SLO
/// service. Returns `observed_error_ratio / budget_error_ratio`; a service
/// exactly at SLO burns at 1.0.
pub fn burn_rate(observed_error_ratio: f64, budget_error_ratio: f64) -> f64 {
    if budget_error_ratio <= 0.0 {
        if observed_error_ratio > 0.0 {
            return f64::INFINITY;
        }
        return 0.0;
    }
    observed_error_ratio / budget_error_ratio
}

/// Fraction of the whole budget period spent if `burn` is sustained for
/// `window_secs` of a `period_secs` period: `burn * window / period`.
pub fn budget_fraction_consumed(burn: f64, window_secs: f64, period_secs: f64) -> f64 {
    burn * window_secs / period_secs
}

/// Error count that trips a `(burn, window)` alert over `window_events`
/// eligible events, rounded up so a fractional allowance still needs a
/// whole extra error to fire.
pub fn allowed_errors(window_events: u64, budget_error_ratio: f64, burn: f64) -> u64 {
    (burn * budget_error_ratio * window_events as f64).ceil() as u64
}
