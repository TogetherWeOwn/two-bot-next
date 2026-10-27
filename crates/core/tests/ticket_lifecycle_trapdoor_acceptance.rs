//! Lifecycle trapdoor acceptance: unsupported label pairs must not lose counts.
//!
//! PROTECTED acceptance handoff for the review-fix round. The paired build
//! consumes this file's head and must not edit it; if a pin looks wrong, hand
//! the correction back to the acceptance author instead of changing the test.
//!
//! Contract pinned here (public `Metrics` API only, no DB, no Discord):
//!
//! - `Metrics::ticket_lifecycle(_n)` preserves every call in the rendered
//!   exposition. A normalized pair no emitter can produce must route to that
//!   event's rendered `query` trapdoor before incrementing, never to an
//!   unrendered cell.
//! - Unknown event + known outcome collapses the event to `close`, then routes
//!   to `close/query` when the collapsed pair is unrendered. Example:
//!   `("unexpected","retired")` must appear as `close/query`, not vanish in
//!   the unrendered `close/retired` cell.
//! - Known event + unsupported known outcome routes to that event's `query`.
//!   Example: `("retire","closed")` must appear as `retire/query`.
//! - Valid pairs stay in place: success outcomes, purge `db_cause` outcomes,
//!   every event's own `query`, and unknown+unknown (already `close/query`).
//! - Multiplicity is preserved through the trapdoor
//!   (`ticket_lifecycle_n` with count), zero stays a no-op, and the rendered
//!   family stays exactly the 22 per-event subset series (no new series).
//!
//! Today the mixed-label cases are RED: the count lands in an unrendered cell
//! and the scrape loses it. The valid-pair controls are GREEN before and
//! after the fix.

use two_bot_core::metrics::Metrics;

const SERIES: &str = "two_bot_ticket_lifecycle_total";

fn count(text: &str, event: &str, outcome: &str) -> u64 {
    let prefix = format!("{SERIES}{{event=\"{event}\",outcome=\"{outcome}\"}} ");
    text.lines()
        .find_map(|line| line.strip_prefix(prefix.as_str())?.parse::<u64>().ok())
        .unwrap_or_else(|| panic!("missing rendered series {event}/{outcome} in:\n{text}"))
}

fn total_lifecycle(text: &str) -> u64 {
    text.lines()
        .filter(|line| line.starts_with(SERIES))
        .map(|line| {
            line.rsplit_once(' ')
                .and_then(|(_, value)| value.parse::<u64>().ok())
                .unwrap_or_else(|| panic!("lifecycle sample is not a counter: {line:?}"))
        })
        .sum()
}

fn series_lines(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| line.starts_with(SERIES))
        .collect()
}

#[test]
fn unknown_event_with_known_outcome_routes_to_close_query() {
    let metrics = Metrics::default();
    metrics.ticket_lifecycle("unexpected", "retired");
    let text = metrics.render(None);
    assert_eq!(
        count(&text, "close", "query"),
        1,
        "unknown event must preserve its count in the close/query trapdoor:\n{text}"
    );
    assert_eq!(
        total_lifecycle(&text),
        1,
        "no count may vanish from the rendered exposition:\n{text}"
    );
}

#[test]
fn unknown_event_with_known_outcome_preserves_multiplicity() {
    let metrics = Metrics::default();
    metrics.ticket_lifecycle_n("unexpected", "retired", 3);
    metrics.ticket_lifecycle_n("unexpected", "retired", 0);
    let text = metrics.render(None);
    assert_eq!(
        count(&text, "close", "query"),
        3,
        "trapdoor must preserve the pass-level multiplicity:\n{text}"
    );
    assert_eq!(total_lifecycle(&text), 3);
}

#[test]
fn known_event_with_unsupported_known_outcome_routes_to_event_query() {
    // One unsupported known outcome per event row: a success token from
    // another event, plus db_cause tokens outside purge (the only
    // storage-failure emitter).
    let cases = [
        ("retire", "closed"),
        ("retire", "database"),
        ("abandon", "claimed"),
        ("abandon", "closed"),
        ("purge", "claimed"),
        ("purge", "retired"),
        ("claim", "retired"),
        ("claim", "pool_closed"),
        ("close", "retired"),
        ("close", "claimed"),
    ];
    for (event, outcome) in cases {
        let metrics = Metrics::default();
        metrics.ticket_lifecycle(event, outcome);
        let text = metrics.render(None);
        assert_eq!(
            count(&text, event, "query"),
            1,
            "unsupported pair {event}/{outcome} must route to {event}/query:\n{text}"
        );
        assert_eq!(
            total_lifecycle(&text),
            1,
            "unsupported pair {event}/{outcome} lost its count:\n{text}"
        );
    }
}

#[test]
fn trapdoor_preserves_n_multiplicity_for_known_event() {
    let metrics = Metrics::default();
    metrics.ticket_lifecycle_n("retire", "closed", 4);
    let text = metrics.render(None);
    assert_eq!(count(&text, "retire", "query"), 4);
    assert_eq!(total_lifecycle(&text), 4);
}

#[test]
fn valid_pairs_stay_in_place() {
    // Success outcomes, purge db_cause outcomes, and every event's own
    // query trapdoor: these render today and must not move.
    let valid = [
        ("retire", "retired"),
        ("abandon", "abandoned"),
        ("abandon", "already_gone"),
        ("purge", "deleted"),
        ("purge", "pool_closed"),
        ("purge", "database"),
        ("claim", "claimed"),
        ("close", "closed"),
        ("retire", "query"),
        ("abandon", "query"),
        ("purge", "query"),
        ("claim", "query"),
        ("close", "query"),
    ];
    for (event, outcome) in valid {
        let metrics = Metrics::default();
        metrics.ticket_lifecycle(event, outcome);
        let text = metrics.render(None);
        assert_eq!(
            count(&text, event, outcome),
            1,
            "valid pair {event}/{outcome} must stay in place:\n{text}"
        );
        assert_eq!(
            total_lifecycle(&text),
            1,
            "valid pair {event}/{outcome} leaked or duplicated:\n{text}"
        );
    }
}

#[test]
fn unknown_outcome_already_collapses_to_event_query() {
    // Control: unknown outcomes already fold to `query` via the trailing
    // slot, so these pass before and after the fix.
    for event in ["retire", "abandon", "purge", "claim", "close"] {
        let metrics = Metrics::default();
        metrics.ticket_lifecycle(event, "evil");
        let text = metrics.render(None);
        assert_eq!(count(&text, event, "query"), 1);
        assert_eq!(total_lifecycle(&text), 1);
    }
}

#[test]
fn unknown_event_and_outcome_stays_close_query() {
    // Control: the fully hostile pair already lands in the rendered
    // close/query cell (pinned by the builder's hostile-label unit test).
    let metrics = Metrics::default();
    metrics.ticket_lifecycle("unexpected", "evil");
    metrics.ticket_lifecycle_n("unexpected", "evil", 2);
    let text = metrics.render(None);
    assert_eq!(count(&text, "close", "query"), 3);
    assert_eq!(total_lifecycle(&text), 3);
}

#[test]
fn trapdoor_routing_adds_no_new_series() {
    let metrics = Metrics::default();
    metrics.ticket_lifecycle("unexpected", "retired");
    metrics.ticket_lifecycle("retire", "closed");
    metrics.ticket_lifecycle("claim", "pool_closed");
    metrics.ticket_lifecycle("purge", "claimed");
    metrics.ticket_lifecycle("retire", "retired");
    let text = metrics.render(None);
    let series = series_lines(&text);
    assert_eq!(
        series.len(),
        22,
        "trapdoor routing must not change the 22-series render subset:\n{text}"
    );
    // Every rendered line still carries exactly {event,outcome} from the
    // contract vocabularies.
    for line in &series {
        assert!(
            line.starts_with(&format!("{SERIES}{{")),
            "lifecycle series carries labels: {line:?}"
        );
    }
    // All five calls are preserved somewhere rendered.
    assert_eq!(total_lifecycle(&text), 5);
}
