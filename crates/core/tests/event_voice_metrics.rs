//! Builder-owned event ticker telemetry regressions.
use two_bot_core::metrics::Metrics;

#[test]
fn event_voice_creates_and_deletes_have_distinct_bounded_series() {
    let metrics = Metrics::default();
    for outcome in ["created", "created", "deleted", "unknown-event-id"] {
        metrics.event_voice_tick(outcome);
    }
    let rendered = metrics.render(None);
    for (outcome, count) in [("created", 2), ("deleted", 1), ("other", 1)] {
        assert!(
            rendered.contains(&format!(
                "two_bot_event_voice_tick_total{{outcome=\"{outcome}\"}} {count}\n"
            )),
            "missing {outcome}={count}"
        );
    }
    assert!(!rendered.contains("unknown-event-id"));
    assert_eq!(
        rendered
            .lines()
            .filter(|line| line.starts_with("two_bot_event_voice_tick_total{"))
            .count(),
        8
    );
}
