//! Audit mirror hourly recheck bounded-skip acceptance (TOG-12873).
//!
//! Tests-only slice against the public `audit_mirror` API
//! (`mirror_recheck_due`, `MirrorRecheckSkip`, `MIRROR_RECHECK_INTERVAL_MS`);
//! no REST, timers, SQL, or `audit_runtime` changes. Pins the house
//! bounded-skip pattern (TOG-12700 precedent): a not-due, malformed, or
//! failed recheck keeps the last good state, and only a good response replaces
//! it. The TOG-12240 runtime consumes this helper when it merges.

use two_bot_core::audit_mirror::{
    mirror_recheck_due, MirrorMessage, MirrorRecheckSkip, MIRROR_RECHECK_INTERVAL_MS,
};

const BOT: &str = "999";
const ENTRY: &str = "18446744073709551615";

fn message(id: &str) -> MirrorMessage {
    MirrorMessage {
        id: id.to_owned(),
        author_id: BOT.to_owned(),
        content: format!("audit-event:{ENTRY}; · something happened"),
    }
}

fn last_good_snapshot() -> Vec<MirrorMessage> {
    vec![message("300"), message("250")]
}

/// One history entry must carry identity (non-empty id, author, content); one
/// malformed row rejects the whole page, mirroring the `normalize_events`
/// whole-snapshot rejection in the scheduled-events precedent.
fn validate_page(page: &[MirrorMessage]) -> Option<Vec<MirrorMessage>> {
    if page.iter().all(|entry| {
        !entry.id.is_empty() && !entry.author_id.is_empty() && !entry.content.is_empty()
    }) {
        Some(page.to_vec())
    } else {
        None
    }
}

/// Scripted recheck tick mirroring the runtime recipe (due-gate → fetch →
/// validate → swap): only a good response replaces the snapshot and advances
/// the cursor. `Err` models a transport failure; `Ok` with an unvalidatable
/// page models a malformed response body.
fn apply_recheck(
    snapshot: &mut Vec<MirrorMessage>,
    last_recheck_ms: u64,
    now_ms: u64,
    fetch: Result<&[MirrorMessage], ()>,
) -> (Option<MirrorRecheckSkip>, u64) {
    if !mirror_recheck_due(last_recheck_ms, now_ms) {
        return (Some(MirrorRecheckSkip::NotDue), last_recheck_ms);
    }
    let payload = match fetch {
        Ok(payload) => payload,
        Err(()) => return (Some(MirrorRecheckSkip::DiscordReadFailed), last_recheck_ms),
    };
    match validate_page(payload) {
        Some(messages) => {
            *snapshot = messages;
            (None, now_ms)
        }
        None => (Some(MirrorRecheckSkip::InvalidResponse), last_recheck_ms),
    }
}

#[test]
fn interval_is_the_legacy_one_hour_cadence() {
    assert_eq!(MIRROR_RECHECK_INTERVAL_MS, 60 * 60 * 1000);
    assert_eq!(MIRROR_RECHECK_INTERVAL_MS, 3_600_000);
}

#[test]
fn due_gate_pins_the_one_hour_boundary() {
    let last = 1_000_000u64;
    assert!(!mirror_recheck_due(last, last));
    assert!(!mirror_recheck_due(
        last,
        last + MIRROR_RECHECK_INTERVAL_MS - 1
    ));
    assert!(mirror_recheck_due(last, last + MIRROR_RECHECK_INTERVAL_MS));
    assert!(mirror_recheck_due(
        last,
        last + MIRROR_RECHECK_INTERVAL_MS + 1
    ));
    // Clock skew or a persisted cursor ahead of now never underflows into a
    // catch-up burst: not due.
    assert!(!mirror_recheck_due(last, last - 1));
    assert!(!mirror_recheck_due(1, 0));
    assert!(!mirror_recheck_due(u64::MAX, 0));
}

#[test]
fn not_due_keeps_last_good_state_without_reading() {
    let last = 1_000_000u64;
    let mut snapshot = last_good_snapshot();
    let (skip, cursor) = apply_recheck(
        &mut snapshot,
        last,
        last + MIRROR_RECHECK_INTERVAL_MS - 1,
        Ok(&[message("400")]),
    );
    assert_eq!(skip, Some(MirrorRecheckSkip::NotDue));
    assert_eq!(cursor, last);
    assert_eq!(snapshot, last_good_snapshot());
}

#[test]
fn malformed_row_is_invalid_response_and_keeps_last_good_snapshot() {
    let last = 1_000_000u64;
    let now = last + MIRROR_RECHECK_INTERVAL_MS;
    let malformed = vec![
        message("400"),
        MirrorMessage {
            id: String::new(),
            author_id: BOT.to_owned(),
            content: "audit-event:broken; · x".to_owned(),
        },
    ];
    assert!(
        validate_page(&malformed).is_none(),
        "one malformed row rejects the page"
    );
    let mut snapshot = last_good_snapshot();
    let (skip, cursor) = apply_recheck(&mut snapshot, last, now, Ok(&malformed));
    assert_eq!(skip, Some(MirrorRecheckSkip::InvalidResponse));
    assert_eq!(cursor, last);
    assert_eq!(
        snapshot,
        last_good_snapshot(),
        "malformed reads never publish, not even an empty snapshot"
    );
}

#[test]
fn failed_fetch_is_discord_read_failed_and_keeps_last_good_snapshot() {
    let last = 1_000_000u64;
    let now = last + MIRROR_RECHECK_INTERVAL_MS;
    let mut snapshot = last_good_snapshot();
    let (skip, cursor) = apply_recheck(&mut snapshot, last, now, Err(()));
    assert_eq!(skip, Some(MirrorRecheckSkip::DiscordReadFailed));
    assert_eq!(cursor, last);
    assert_eq!(
        snapshot,
        last_good_snapshot(),
        "transport errors leave the last good snapshot in place"
    );
    assert_ne!(
        MirrorRecheckSkip::DiscordReadFailed,
        MirrorRecheckSkip::InvalidResponse,
        "transport failure and malformed body are distinct skip outcomes"
    );
    assert_ne!(
        MirrorRecheckSkip::DiscordReadFailed,
        MirrorRecheckSkip::NotDue,
        "transport failure and not-due are distinct skip outcomes"
    );
}

#[test]
fn good_response_replaces_snapshot_and_advances_cursor() {
    let last = 1_000_000u64;
    let now = last + MIRROR_RECHECK_INTERVAL_MS;
    let refreshed = vec![message("500")];
    let mut snapshot = last_good_snapshot();
    let (skip, cursor) = apply_recheck(&mut snapshot, last, now, Ok(&refreshed));
    assert_eq!(skip, None);
    assert_eq!(cursor, now);
    assert_eq!(snapshot, refreshed);
}

#[test]
fn mixed_failure_sequence_never_publishes_until_good_response() {
    let last = 1_000_000u64;
    let due = last + MIRROR_RECHECK_INTERVAL_MS;
    let malformed = vec![MirrorMessage {
        id: String::new(),
        author_id: BOT.to_owned(),
        content: "audit-event:broken; · x".to_owned(),
    }];
    let mut snapshot = last_good_snapshot();
    let mut cursor = last;
    let (skip, next) = apply_recheck(&mut snapshot, cursor, due - 1, Ok(&[message("400")]));
    assert_eq!(skip, Some(MirrorRecheckSkip::NotDue));
    cursor = next;
    let (skip, next) = apply_recheck(&mut snapshot, cursor, due, Err(()));
    assert_eq!(skip, Some(MirrorRecheckSkip::DiscordReadFailed));
    cursor = next;
    let (skip, next) = apply_recheck(&mut snapshot, cursor, due, Ok(&malformed));
    assert_eq!(skip, Some(MirrorRecheckSkip::InvalidResponse));
    cursor = next;
    assert_eq!(cursor, last);
    assert_eq!(
        snapshot,
        last_good_snapshot(),
        "failures in a row still keep the last good snapshot"
    );
    let refreshed = vec![message("600")];
    let (skip, next) = apply_recheck(&mut snapshot, cursor, due, Ok(&refreshed));
    assert_eq!(skip, None);
    assert_eq!(next, due);
    assert_eq!(snapshot, refreshed);
}
