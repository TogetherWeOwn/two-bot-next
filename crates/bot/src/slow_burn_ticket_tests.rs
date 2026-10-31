//! Acceptance tests: error-budget slow burn files a ticket (never a page).
//!
//! Offline pin of `docs/error-budget-alerts.md` slow-burn behavior with
//! synthetic window data: burn rate 3 over the 2 h long window, confirmed by
//! the 15 min short window. The constants (`SLOW_BURN_*`) and burn math
//! already exist in `burn_rate.rs`; the ticket decision does not, so these
//! tests are red until it lands. The builder must not edit this file:
//! implement `slow_burn_tickets` in `burn_rate.rs` (mirroring
//! `fast_burn_pages`, including per-window volume floors so quiet traffic
//! and a single error never ticket) and keep these fixtures green
//! unmodified.

use super::burn_rate::*;

#[test]
fn worked_example_slow_burn_tickets() {
    // Worked example from the doc at 10,000 eligible events/hour: 20,000 per
    // 2 h with 60 errors and 2,500 per 15 min with 8 errors. The long window
    // burns at exactly 3x and the short window confirms above it.
    let long_burn = burn_rate(60.0 / 20_000.0, BUDGET_ERROR_RATIO);
    let short_burn = burn_rate(8.0 / 2_500.0, BUDGET_ERROR_RATIO);
    assert!(
        (long_burn - SLOW_BURN_RATE).abs() < 0.01,
        "synthetic long window should burn at 3x, got {long_burn}"
    );
    assert!(
        short_burn >= SLOW_BURN_RATE,
        "synthetic short window should confirm at 3x+, got {short_burn}"
    );
    assert!(
        slow_burn_tickets(60, 20_000, 8, 2_500),
        "worked-example slow burn (both windows >= 3x) must ticket"
    );
    // Sustained burn well above the threshold tickets too (6x in both).
    assert!(
        slow_burn_tickets(120, 20_000, 15, 2_500),
        "sustained 6x slow burn must ticket"
    );
}

#[test]
fn just_below_threshold_in_either_window_stays_silent() {
    // One error short of the worked example in either window drops that
    // window below 3x: 59/20,000 burns 2.95x and 7/2,500 burns 2.8x.
    assert!(
        !slow_burn_tickets(59, 20_000, 8, 2_500),
        "long window just below 3x must not ticket"
    );
    assert!(
        !slow_burn_tickets(60, 20_000, 7, 2_500),
        "short window just below 3x must not ticket"
    );
}

#[test]
fn one_window_alone_never_tickets() {
    // A burning long window without short-window confirmation is history,
    // not an ongoing burn.
    assert!(
        !slow_burn_tickets(60, 20_000, 0, 2_500),
        "long-window burn without short confirmation must not ticket"
    );
    // A short spike against a healthy long window (1x over 2 h) is one bad
    // quarter hour, not a slow burn, even at 10x in the short window.
    assert!(
        !slow_burn_tickets(20, 20_000, 25, 2_500),
        "short-window spike without long burn must not ticket"
    );
    assert!(
        !slow_burn_tickets(0, 20_000, 8, 2_500),
        "error-free long window must not ticket"
    );
}

#[test]
fn healthy_burn_stays_silent() {
    // Traffic at the SLO itself (1x in both windows) spends budget exactly
    // as allowed and must never ticket.
    assert!(
        !slow_burn_tickets(20, 20_000, 3, 2_500),
        "healthy burn at ~1x must not ticket"
    );
    assert!(
        !slow_burn_tickets(0, 20_000, 0, 2_500),
        "error-free traffic must not ticket"
    );
}

#[test]
fn volume_floors_keep_quiet_traffic_silent() {
    // A trickle where every request fails has an enormous ratio but almost
    // no eligible events: per-window volume floors keep it silent instead of
    // ticketing on a handful of requests (or dividing by zero).
    assert!(
        !slow_burn_tickets(3, 9, 3, 9),
        "single-digit eligible events per window must not ticket"
    );
    // A single error never tickets, even when its window's ratio is 10x and
    // the other window burns at the worked-example rate.
    assert!(
        !slow_burn_tickets(1, 100, 8, 2_500),
        "one long-window error must not ticket"
    );
    assert!(
        !slow_burn_tickets(60, 20_000, 1, 100),
        "one short-window error must not ticket"
    );
    // Empty windows are unknown/quiet, never a ticket and never a panic.
    assert!(
        !slow_burn_tickets(0, 0, 0, 0),
        "empty windows must not ticket"
    );
}
