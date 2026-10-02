//! Scheduled-events skip-path acceptance (TOG-12700).
//!
//! Tests-only slice against the existing public `scheduled_events` API
//! (`normalize_event` / `normalize_events`, `EventStatus`,
//! `ScheduledEventsSkip`, `SCHEDULED_EVENTS_INTERVAL_MS`); no REST, timer,
//! or SQL. Pins the schema tripwire: a failed or malformed read keeps the
//! last good snapshot in place and never publishes an empty snapshot, while
//! a successful empty response *is* a valid empty mirror. See
//! `docs/scheduled-events-skip-acceptance.md`.

use two_bot_core::{
    normalize_event, normalize_events, EventStatus, RawScheduledEvent, ScheduledEvent,
    ScheduledEventsSkip, SCHEDULED_EVENTS_INTERVAL_MS,
};

const START: &str = "2026-09-06T18:00:00.000Z";

const EMPTY: &[RawScheduledEvent] = &[];

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
/// `Err` models a transport failure; `Ok` with an unnormalizable payload
/// models a malformed response body.
fn apply_poll(
    fetch: Result<&[RawScheduledEvent], ()>,
    snapshot: &mut Vec<ScheduledEvent>,
) -> Option<ScheduledEventsSkip> {
    let payload = match fetch {
        Ok(payload) => payload,
        Err(()) => return Some(ScheduledEventsSkip::DiscordReadFailed),
    };
    match normalize_events(payload) {
        Some(events) => {
            *snapshot = events;
            None
        }
        None => Some(ScheduledEventsSkip::InvalidResponse),
    }
}

#[test]
fn malformed_payload_is_invalid_response_and_keeps_last_good_snapshot() {
    let malformed_rows = [
        RawScheduledEvent {
            scheduled_start_time: Some("not-a-date".to_owned()),
            ..raw("bad-1", "Bad start", START, 1)
        },
        RawScheduledEvent {
            status: Some(99),
            ..raw("bad-2", "Bad status", START, 1)
        },
        RawScheduledEvent {
            id: None,
            ..raw("bad-3", "Missing id", START, 1)
        },
    ];
    for broken in &malformed_rows {
        let response = [raw("event-1", "Sunday Squad", START, 1), broken.clone()];
        assert!(
            normalize_events(&response).is_none(),
            "one malformed event rejects the snapshot: {broken:?}"
        );
        let mut snapshot = last_good_snapshot();
        assert_eq!(
            apply_poll(Ok(&response), &mut snapshot),
            Some(ScheduledEventsSkip::InvalidResponse)
        );
        assert_eq!(
            snapshot,
            last_good_snapshot(),
            "malformed reads never publish, not even an empty snapshot"
        );
    }
}

#[test]
fn failed_fetch_is_discord_read_failed_and_keeps_last_good_snapshot() {
    let mut snapshot = last_good_snapshot();
    assert_eq!(
        apply_poll(Err(()), &mut snapshot),
        Some(ScheduledEventsSkip::DiscordReadFailed)
    );
    assert_eq!(
        snapshot,
        last_good_snapshot(),
        "transport errors leave the last good snapshot in place"
    );
    assert_ne!(
        ScheduledEventsSkip::DiscordReadFailed,
        ScheduledEventsSkip::InvalidResponse,
        "transport failure and malformed body are distinct skip outcomes"
    );
}

#[test]
fn successful_empty_response_is_a_valid_empty_mirror() {
    assert_eq!(
        normalize_events(&[]),
        Some(Vec::<ScheduledEvent>::new()),
        "a successful empty response normalizes, unlike a transport error"
    );
    let mut snapshot = last_good_snapshot();
    assert_eq!(apply_poll(Ok(EMPTY), &mut snapshot), None);
    assert!(
        snapshot.is_empty(),
        "a successful empty response deletes the last event"
    );
}

#[test]
fn status_maps_the_four_known_statuses_and_refuses_unknown() {
    assert_eq!(EventStatus::from_api(1), Some(EventStatus::Scheduled));
    assert_eq!(EventStatus::from_api(2), Some(EventStatus::Active));
    assert_eq!(EventStatus::from_api(3), Some(EventStatus::Completed));
    assert_eq!(EventStatus::from_api(4), Some(EventStatus::Cancelled));
    for unknown in [0, 5, -1, 99, i64::MIN, i64::MAX] {
        assert_eq!(EventStatus::from_api(unknown), None, "{unknown}");
    }
    for (word, status) in [
        ("scheduled", EventStatus::Scheduled),
        ("active", EventStatus::Active),
        ("completed", EventStatus::Completed),
        ("cancelled", EventStatus::Cancelled),
    ] {
        assert_eq!(EventStatus::parse(word), Some(status));
        assert_eq!(status.as_str(), word);
    }
    assert_eq!(EventStatus::parse("bogus"), None);
    assert!(
        normalize_event(&raw("bad", "Unknown status", START, 99)).is_none(),
        "unknown statuses are refused per row"
    );
    assert!(
        normalize_events(&[
            raw("event-1", "Sunday Squad", START, 1),
            raw("bad", "Unknown status", START, 99),
        ])
        .is_none(),
        "an unknown status rejects the whole snapshot"
    );
}

#[test]
fn interval_is_the_legacy_ten_minute_cadence() {
    assert_eq!(SCHEDULED_EVENTS_INTERVAL_MS, 10 * 60 * 1000);
    assert_eq!(SCHEDULED_EVENTS_INTERVAL_MS, 600_000);
}

#[test]
fn mixed_failure_sequence_never_publishes_an_empty_snapshot() {
    let mut snapshot = last_good_snapshot();
    let malformed = [
        raw("event-1", "Sunday Squad", START, 1),
        RawScheduledEvent {
            status: Some(7),
            ..raw("bad", "Unknown status", START, 1)
        },
    ];
    assert_eq!(
        apply_poll(Err(()), &mut snapshot),
        Some(ScheduledEventsSkip::DiscordReadFailed)
    );
    assert_eq!(
        apply_poll(Ok(&malformed), &mut snapshot),
        Some(ScheduledEventsSkip::InvalidResponse)
    );
    assert_eq!(
        snapshot,
        last_good_snapshot(),
        "failures in a row still keep the last good snapshot"
    );
    let refreshed = [raw("event-9", "Friday Fights", START, 2)];
    assert_eq!(apply_poll(Ok(&refreshed), &mut snapshot), None);
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].status, EventStatus::Active);
}
