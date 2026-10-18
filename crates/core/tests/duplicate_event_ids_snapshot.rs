//! Acceptance: a website-mirror snapshot with duplicate event IDs is refused
//! as a whole before storage replacement. Identical duplicates and
//! conflicting duplicates (either order) share one unambiguous refusal;
//! unique and empty snapshots keep the existing replacement semantics.

use two_bot_core::{normalize_events, EventStatus, RawScheduledEvent};

fn raw(id: &str, name: &str, start: &str, status: i64) -> RawScheduledEvent {
    RawScheduledEvent {
        id: Some(id.to_owned()),
        name: Some(name.to_owned()),
        scheduled_start_time: Some(start.to_owned()),
        channel_id: Some("voice-1".to_owned()),
        description: Some("Weekly games night.".to_owned()),
        status: Some(status),
    }
}

fn event(id: &str) -> RawScheduledEvent {
    raw(id, "Test night", "2026-10-01T19:00:00Z", 1)
}

#[test]
fn unique_ids_normalize_in_snapshot_order() {
    let normalized = normalize_events(&[event("3002"), event("3000"), event("3001")])
        .expect("unique ids are a valid snapshot");
    let ids: Vec<&str> = normalized.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(ids, ["3002", "3000", "3001"]);
    assert!(normalized
        .iter()
        .all(|row| row.status == EventStatus::Scheduled));
}

#[test]
fn empty_snapshot_is_valid_not_malformed() {
    let normalized = normalize_events(&[]).expect("empty snapshot is valid");
    assert!(normalized.is_empty());
}

#[test]
fn identical_duplicate_ids_refuse_the_whole_snapshot() {
    assert!(
        normalize_events(&[event("3000"), event("3000")]).is_none(),
        "identical duplicate ids must refuse, not silently keep one row"
    );
}

#[test]
fn conflicting_duplicate_ids_refuse_in_either_order() {
    let first = raw("3000", "First name", "2026-10-01T19:00:00Z", 1);
    let second = raw("3000", "Second name", "2026-10-02T19:00:00Z", 2);
    assert!(
        normalize_events(&[first.clone(), second.clone()]).is_none(),
        "conflicting duplicates must refuse (first, second)"
    );
    assert!(
        normalize_events(&[second, first]).is_none(),
        "conflicting duplicates must refuse (second, first); order must not pick a winner"
    );
}

#[test]
fn duplicate_id_alongside_unique_rows_refuses_the_whole_snapshot() {
    assert!(normalize_events(&[event("3001"), event("3000"), event("3000")]).is_none());
    assert!(normalize_events(&[event("3000"), event("3000"), event("3001")]).is_none());
}

#[test]
fn non_adjacent_duplicate_ids_refuse_the_whole_snapshot() {
    assert!(
        normalize_events(&[event("3000"), event("3001"), event("3002"), event("3000")]).is_none()
    );
}

#[test]
fn tripled_id_refuses_the_whole_snapshot() {
    assert!(normalize_events(&[event("3000"), event("3000"), event("3000")]).is_none());
}

#[test]
fn duplicate_refusal_does_not_change_single_row_semantics() {
    // Control: the same row alone is valid, so refusal above keys on the
    // repeated identity within the snapshot, not on row content.
    let normalized = normalize_events(&[event("3000")]).expect("single row is valid");
    assert_eq!(normalized.len(), 1);
    assert_eq!(normalized[0].id, "3000");
    assert_eq!(normalized[0].starts_at, "2026-10-01T19:00:00.000Z");
}
