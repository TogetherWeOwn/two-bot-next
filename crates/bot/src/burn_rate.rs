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

/// Minimum eligible events per fast-burn window: quiet traffic never pages.
/// Must match `ERROR_BUDGET_FAST_MIN_EVENTS` in `wrangler/src/alert-rules.ts`.
pub const FAST_BURN_MIN_EVENTS: u64 = 10;
/// Minimum budget errors per fast-burn window: one bad minute never pages.
/// Must match `ERROR_BUDGET_FAST_MIN_ERRORS` in `wrangler/src/alert-rules.ts`.
pub const FAST_BURN_MIN_ERRORS: u64 = 2;

/// Fast-burn page decision over the two policy windows: page only when the
/// long window (15 min of eligible events) and the short confirmation window
/// (2 min) both burn at or above `FAST_BURN_RATE`. One bad window alone never
/// pages, and a window below either volume floor stays silent instead of
/// dividing by zero or paging on a trickle. The Worker
/// (`error_budget_fast_burn` in `wrangler/src/alert-rules.ts`) evaluates this
/// policy on its keepalive schedule against cumulative REST counters; this
/// function pins that decision exactly, including the per-window floors. The
/// only intentional divergence is the Worker's ~1% scrape-quantization
/// allowance, which exists because window edges land on whole scrapes.
pub fn fast_burn_pages(
    long_errors: u64,
    long_events: u64,
    short_errors: u64,
    short_events: u64,
) -> bool {
    fn burns(errors: u64, events: u64) -> bool {
        if events < FAST_BURN_MIN_EVENTS || errors < FAST_BURN_MIN_ERRORS {
            return false;
        }
        burn_rate(errors as f64 / events as f64, BUDGET_ERROR_RATIO) >= FAST_BURN_RATE
    }
    burns(long_errors, long_events) && burns(short_errors, short_events)
}

/// Minimum eligible events per slow-burn window: quiet traffic never tickets.
/// Must match `ERROR_BUDGET_SLOW_MIN_EVENTS` in `wrangler/src/alert-rules.ts`.
pub const SLOW_BURN_MIN_EVENTS: u64 = 10;
/// Minimum budget errors per slow-burn window: one bad event never tickets.
/// Must match `ERROR_BUDGET_SLOW_MIN_ERRORS` in `wrangler/src/alert-rules.ts`.
pub const SLOW_BURN_MIN_ERRORS: u64 = 2;

/// Slow-burn ticket decision over the two policy windows: ticket only when the
/// long window (2 h of eligible events) and the short confirmation window
/// (15 min) both burn at or above `SLOW_BURN_RATE`. One bad window alone never
/// tickets, and a window below either volume floor stays silent instead of
/// dividing by zero or ticketing on a trickle. The Worker
/// (`error_budget_slow_burn` in `wrangler/src/alert-rules.ts`) evaluates this
/// policy on its keepalive schedule against cumulative REST counters; this
/// function pins that decision exactly, including the per-window floors. The
/// only intentional divergence is the Worker's ~1% scrape-quantization
/// allowance, which exists because window edges land on whole scrapes.
pub fn slow_burn_tickets(
    long_errors: u64,
    long_events: u64,
    short_errors: u64,
    short_events: u64,
) -> bool {
    fn burns(errors: u64, events: u64) -> bool {
        if events < SLOW_BURN_MIN_EVENTS || errors < SLOW_BURN_MIN_ERRORS {
            return false;
        }
        burn_rate(errors as f64 / events as f64, BUDGET_ERROR_RATIO) >= SLOW_BURN_RATE
    }
    burns(long_errors, long_events) && burns(short_errors, short_events)
}

/// Fast-burn page message. Carries the container build SHA and the public
/// log-query URL so the responder can jump from the page to the failing
/// build and its logs; names the fast-burn alert for the runbook lookup.
pub fn fast_burn_page_message(
    build_sha: &str,
    log_query_url: &str,
    long_burn: f64,
    short_burn: f64,
) -> String {
    format!(
        "two-bot-next fast-burn page: error budget burning at {long_burn:.1}x \
         over 15 min and {short_burn:.1}x over 2 min (threshold {FAST_BURN_RATE:.0}x). \
         Build: {build_sha}. Logs: {log_query_url}"
    )
}
