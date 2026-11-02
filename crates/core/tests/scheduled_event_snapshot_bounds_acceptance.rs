//! Scheduled-event snapshot-bounds acceptance (bounded-claim decision, caps item).
//!
//! Tests-only slice against the existing public `scheduled_events` API
//! (`normalize_event` / `normalize_events`, `ScheduledEvent`,
//! `ScheduledEventsSkip`); no REST, timer, or SQL. Pins the decision caps:
//! max 100 rows per snapshot and per-field `name` <= 100 / `description` <=
//! 1000 / `channel_id` <= 32 chars, every violation refusing the whole
//! snapshot as `InvalidResponse` — never silently truncated, never partially
//! applied — while in-bounds snapshots normalize byte-identically.
//!
//! Bound split across suites, by seam:
//! - rows + fields: here, through `normalize_events` (pure, offline).
//! - raw-body 1 MiB cap: the bot `website_snapshot_tests` tick suite, through
//!   `run_once` with a scripted mock body and a poison pool. `normalize_*`
//!   never sees body bytes, so a pure test here could not pin that bound.
//! - snapshot age (5 min `StaleSnapshot` skip): not pinned here. The tick
//!   stamps `observed_at` from the real clock with no injectable seam and the
//!   client aborts reads long before the bound, so no baseline-observable
//!   fixture can make a snapshot stale; that bound needs its own seam.

use two_bot_core::{
    normalize_event, normalize_events, RawScheduledEvent, ScheduledEvent, ScheduledEventsSkip,
};

const START: &str = "2026-09-06T18:00:00.000Z";

const MAX_ROWS: usize = 100;
const MAX_NAME_CHARS: usize = 100;
const MAX_DESCRIPTION_CHARS: usize = 1000;
const MAX_CHANNEL_CHARS: usize = 32;

fn raw(id: &str, name: &str, start: &str, status: i64) -> RawScheduledEvent {
    RawScheduledEvent {
        id: Some(id.to_owned()),
        name: Some(name.to_owned()),
        scheduled_start_time: Some(start.to_owned()),
        channel_id: Some("voice-1".to_owned()),
        description: Some("Join the weekly games night.".to_owned()),
        status: Some(status),
    }
}

fn last_good_snapshot() -> Vec<ScheduledEvent> {
    normalize_events(&[raw("event-1", "Sunday Squad", START, 1)]).expect("fixture normalizes")
}

/// Scripted tick outcome mirroring the documented poller recipe (fetch →
/// `normalize_events` → swap): only `Some(events)` replaces the snapshot.
/// A refused snapshot keeps the last good rows, including keeping them at
/// zero rows; production maps the refusal to `InvalidResponse`.
fn apply_poll(
    payload: &[RawScheduledEvent],
    snapshot: &mut Vec<ScheduledEvent>,
) -> Option<ScheduledEventsSkip> {
    match normalize_events(payload) {
        Some(events) => {
            *snapshot = events;
            None
        }
        None => Some(ScheduledEventsSkip::InvalidResponse),
    }
}

fn numbered_rows(count: usize) -> Vec<RawScheduledEvent> {
    (0..count)
        .map(|i| {
            raw(
                &format!("event-{i}"),
                &format!("Event number {i}"),
                START,
                1,
            )
        })
        .collect()
}

#[test]
fn rows_101_refuses_the_whole_snapshot_and_keeps_last_good() {
    let response = numbered_rows(MAX_ROWS + 1);
    assert!(
        normalize_events(&response).is_none(),
        "101 rows must refuse the whole snapshot, not apply the first 100"
    );
    let mut snapshot = last_good_snapshot();
    assert_eq!(
        apply_poll(&response, &mut snapshot),
        Some(ScheduledEventsSkip::InvalidResponse)
    );
    assert_eq!(
        snapshot,
        last_good_snapshot(),
        "an over-cap snapshot publishes nothing, not even a partial prefix"
    );
}

#[test]
fn rows_100_applies_byte_identically() {
    let response = numbered_rows(MAX_ROWS);
    let events = normalize_events(&response).expect("exactly 100 rows is in bounds");
    assert_eq!(events.len(), MAX_ROWS);
    for (event, index) in events.iter().zip(0..MAX_ROWS) {
        assert_eq!(event.id, format!("event-{index}"));
        assert_eq!(event.name, format!("Event number {index}"));
        assert_eq!(event.starts_at, "2026-09-06T18:00:00.000Z");
    }
    let mut snapshot = last_good_snapshot();
    assert_eq!(apply_poll(&response, &mut snapshot), None);
    assert_eq!(snapshot, events);
}

#[test]
fn name_101_chars_refuses_while_100_chars_is_byte_identical() {
    let fitting_str = "n".repeat(MAX_NAME_CHARS);
    let over = "n".repeat(MAX_NAME_CHARS + 1);
    let fitting = normalize_event(&raw("ok", &fitting_str, START, 1)).expect("100-char name fits");
    assert_eq!(
        fitting.name, fitting_str,
        "an in-bounds name keeps every char"
    );
    assert!(
        normalize_event(&raw("over", &over, START, 1)).is_none(),
        "a 101-char name must refuse, not clip to 100"
    );
    let response = [
        raw("ok", &fitting_str, START, 1),
        raw("over", &over, START, 1),
    ];
    assert!(
        normalize_events(&response).is_none(),
        "one over-length name rejects the whole snapshot"
    );
    let mut snapshot = last_good_snapshot();
    assert_eq!(
        apply_poll(&response, &mut snapshot),
        Some(ScheduledEventsSkip::InvalidResponse)
    );
    assert_eq!(snapshot, last_good_snapshot());
}

#[test]
fn name_limit_counts_chars_not_bytes() {
    let fitting_str = "é".repeat(MAX_NAME_CHARS);
    assert_eq!(fitting_str.chars().count(), MAX_NAME_CHARS);
    assert!(
        fitting_str.len() > MAX_NAME_CHARS,
        "fixture must exceed the limit in bytes while fitting in chars"
    );
    let event =
        normalize_event(&raw("ok", &fitting_str, START, 1)).expect("100 chars fit in any encoding");
    assert_eq!(event.name, fitting_str);
    let over = "é".repeat(MAX_NAME_CHARS + 1);
    assert!(
        normalize_event(&raw("over", &over, START, 1)).is_none(),
        "101 chars refuse even though the byte count also exceeds"
    );
}

#[test]
fn description_1001_chars_refuses_while_1000_chars_is_byte_identical() {
    let fitting_str = "d".repeat(MAX_DESCRIPTION_CHARS);
    let over = "d".repeat(MAX_DESCRIPTION_CHARS + 1);
    let fitting_row = RawScheduledEvent {
        description: Some(fitting_str.clone()),
        ..raw("ok", "Sunday Squad", START, 1)
    };
    let fitting = normalize_event(&fitting_row).expect("1000-char description fits");
    assert_eq!(
        fitting.description.as_deref(),
        Some(fitting_str.as_str()),
        "an in-bounds description keeps every char"
    );
    let over_row = RawScheduledEvent {
        description: Some(over),
        ..raw("over", "Sunday Squad", START, 1)
    };
    assert!(
        normalize_event(&over_row).is_none(),
        "a 1001-char description must refuse, not clip to 1000"
    );
    assert!(
        normalize_events(&[fitting_row, over_row]).is_none(),
        "one over-length description rejects the whole snapshot"
    );
}

#[test]
fn description_limit_counts_chars_not_bytes() {
    let fitting_str = "あ".repeat(MAX_DESCRIPTION_CHARS);
    assert_eq!(fitting_str.chars().count(), MAX_DESCRIPTION_CHARS);
    assert!(
        fitting_str.len() > MAX_DESCRIPTION_CHARS,
        "fixture must exceed the limit in bytes while fitting in chars"
    );
    let row = RawScheduledEvent {
        description: Some(fitting_str.clone()),
        ..raw("ok", "Sunday Squad", START, 1)
    };
    let event = normalize_event(&row).expect("1000 chars fit in any encoding");
    assert_eq!(event.description.as_deref(), Some(fitting_str.as_str()));
    let over = "あ".repeat(MAX_DESCRIPTION_CHARS + 1);
    let over_row = RawScheduledEvent {
        description: Some(over),
        ..raw("over", "Sunday Squad", START, 1)
    };
    assert!(
        normalize_event(&over_row).is_none(),
        "1001 chars refuse even though the byte count also exceeds"
    );
}

#[test]
fn channel_id_33_chars_refuses_while_32_chars_is_byte_identical() {
    let fitting_str = "1".repeat(MAX_CHANNEL_CHARS);
    let over = "1".repeat(MAX_CHANNEL_CHARS + 1);
    let fitting_row = RawScheduledEvent {
        channel_id: Some(fitting_str.clone()),
        ..raw("ok", "Sunday Squad", START, 1)
    };
    let fitting = normalize_event(&fitting_row).expect("32-char channel id fits");
    assert_eq!(
        fitting.channel_id.as_deref(),
        Some(fitting_str.as_str()),
        "an in-bounds channel id keeps every char"
    );
    let over_row = RawScheduledEvent {
        channel_id: Some(over),
        ..raw("over", "Sunday Squad", START, 1)
    };
    assert!(
        normalize_event(&over_row).is_none(),
        "a 33-char channel id must refuse, not clip to 32"
    );
    assert!(
        normalize_events(&[fitting_row, over_row]).is_none(),
        "one over-length channel id rejects the whole snapshot"
    );
}

fn assert_whole_snapshot_refusal(label: &str, response: &[RawScheduledEvent]) {
    assert!(
        normalize_events(response).is_none(),
        "{label} must refuse the whole snapshot"
    );
    let mut snapshot = last_good_snapshot();
    assert_eq!(
        apply_poll(response, &mut snapshot),
        Some(ScheduledEventsSkip::InvalidResponse),
        "{label} keeps the last good snapshot"
    );
    assert_eq!(
        snapshot,
        last_good_snapshot(),
        "{label} publishes nothing, not even the valid rows"
    );
}

#[test]
fn over_cap_rows_never_partially_apply() {
    assert_whole_snapshot_refusal("101 rows", &numbered_rows(MAX_ROWS + 1));
}

#[test]
fn over_cap_name_never_partially_applies() {
    // The valid sibling proves the refusal is about the bound, not about a
    // malformed row.
    let response = [
        raw("event-1", "Sunday Squad", START, 1),
        raw("over", &"n".repeat(MAX_NAME_CHARS + 1), START, 1),
    ];
    assert_whole_snapshot_refusal("a 101-char name", &response);
}

#[test]
fn over_cap_description_never_partially_applies() {
    let response = [
        raw("event-1", "Sunday Squad", START, 1),
        RawScheduledEvent {
            description: Some("d".repeat(MAX_DESCRIPTION_CHARS + 1)),
            ..raw("over", "Sunday Squad", START, 1)
        },
    ];
    assert_whole_snapshot_refusal("a 1001-char description", &response);
}

#[test]
fn over_cap_channel_id_never_partially_applies() {
    let response = [
        raw("event-1", "Sunday Squad", START, 1),
        RawScheduledEvent {
            channel_id: Some("1".repeat(MAX_CHANNEL_CHARS + 1)),
            ..raw("over", "Sunday Squad", START, 1)
        },
    ];
    assert_whole_snapshot_refusal("a 33-char channel id", &response);
}

#[test]
fn in_bounds_snapshot_with_at_cap_values_is_byte_identical() {
    let response = [RawScheduledEvent {
        name: Some("n".repeat(MAX_NAME_CHARS)),
        description: Some("d".repeat(MAX_DESCRIPTION_CHARS)),
        channel_id: Some("1".repeat(MAX_CHANNEL_CHARS)),
        ..raw("event-1", "Sunday Squad", START, 1)
    }];
    let events = normalize_events(&response).expect("at-cap values are in bounds");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].name, "n".repeat(MAX_NAME_CHARS));
    assert_eq!(
        events[0].description.as_deref(),
        Some("d".repeat(MAX_DESCRIPTION_CHARS).as_str())
    );
    assert_eq!(
        events[0].channel_id.as_deref(),
        Some("1".repeat(MAX_CHANNEL_CHARS).as_str())
    );
    assert_eq!(events[0].starts_at, "2026-09-06T18:00:00.000Z");
    let mut snapshot = Vec::new();
    assert_eq!(apply_poll(&response, &mut snapshot), None);
    assert_eq!(snapshot, events);
}
