//! Acceptance tests: fast-burn page carries build SHA and log link.
//!
//! Offline pin of `docs/error-budget-alerts.md` fast-burn behavior with
//! synthetic window data. The burn math (`burn_rate`) already exists; the page
//! decision and message do not yet have a runtime caller, so these tests are
//! red until the fast-burn page lands. The builder must not edit this file:
//! implement `fast_burn_pages` and `fast_burn_page_message` in `burn_rate.rs`
//! (plus the runtime caller and Worker history) and keep these fixtures green
//! unmodified.

use super::burn_rate::*;

const SYNTHETIC_BUILD_SHA: &str = "abc123def456abc123def456abc123def456abcd";
const SYNTHETIC_LOG_URL: &str =
    "https://developers.cloudflare.com/workers/observability/logs/workers-logs/";

#[test]
fn forced_fast_burn_pages_with_synthetic_window_data() {
    // Worked example from the doc at 10,000 eligible events/hour: 2,500 per
    // 15 min with 30 errors and ~333 per 2 min with 4 errors both burn at 12x.
    let long_burn = burn_rate(30.0 / 2_500.0, BUDGET_ERROR_RATIO);
    let short_burn = burn_rate(4.0 / 333.0, BUDGET_ERROR_RATIO);
    assert!(
        (long_burn - FAST_BURN_RATE).abs() < 0.5,
        "synthetic long window should burn near 12x, got {long_burn}"
    );
    assert!(
        short_burn >= FAST_BURN_RATE,
        "synthetic short window should confirm at 12x+, got {short_burn}"
    );
    assert!(
        fast_burn_pages(30, 2_500, 4, 333),
        "forced fast burn (both windows >= 12x) must page"
    );
}

#[test]
fn healthy_burn_stays_silent() {
    // Healthy traffic at a fifth of budget: 1,000 eligible events with a
    // single error burns near 1x and must not page.
    assert!(
        !fast_burn_pages(1, 1_000, 0, 133),
        "healthy burn must not page"
    );
    // One bad window alone is not a burn trigger: a long-window spike without
    // short-window confirmation stays silent, and vice versa.
    assert!(
        !fast_burn_pages(30, 2_500, 0, 333),
        "long-window spike without short confirmation must not page"
    );
    assert!(
        !fast_burn_pages(0, 2_500, 4, 333),
        "short-window spike without long burn must not page"
    );
}

#[test]
fn fast_burn_page_carries_build_sha_and_log_link() {
    let message = fast_burn_page_message(
        SYNTHETIC_BUILD_SHA,
        SYNTHETIC_LOG_URL,
        FAST_BURN_RATE,
        FAST_BURN_RATE,
    );
    assert!(
        message.contains(SYNTHETIC_BUILD_SHA),
        "page must carry the build SHA, got: {message}"
    );
    assert!(
        message.contains(SYNTHETIC_LOG_URL),
        "page must carry the log-query URL, got: {message}"
    );
    assert!(
        message.to_ascii_lowercase().contains("fast"),
        "page must name the fast-burn alert, got: {message}"
    );
}
