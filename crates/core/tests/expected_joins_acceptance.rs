//! Expected-joins TTL/source acceptance (TOG-12703).
//!
//! Pure, offline: pins the public `two_bot_core::expected_joins` API only —
//! no gateway, no SQL, no Discord client. The clock is injectable
//! (`ExpectedJoins::with_clock`) so TTL boundaries are deterministic.
//!
//! Parity: one-click join attribution notes — 30 s TTL, `web:one_click`
//! source, guild+member keying; dying between the add call and the gateway
//! event falls back to the honest `unknown` the invite tracker reports
//! (`consume` returning `None` means attribution proceeds by invite diff).

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use two_bot_core::{ExpectedJoins, EXPECTED_JOIN_TTL_SECONDS, WEB_ONE_CLICK_SOURCE};

const GUILD_A: u64 = 11;
const GUILD_B: u64 = 22;
const MEMBER: u64 = 7;
const OTHER_MEMBER: u64 = 8;

fn clock(start_ms: i64) -> (Arc<AtomicI64>, impl Fn() -> i64 + Send + Sync + 'static) {
    let t = Arc::new(AtomicI64::new(start_ms));
    let c = Arc::clone(&t);
    (t, move || c.load(Ordering::SeqCst))
}

fn tracker(start_ms: i64) -> (Arc<AtomicI64>, ExpectedJoins) {
    let (t, now) = clock(start_ms);
    (t, ExpectedJoins::with_clock(EXPECTED_JOIN_TTL_SECONDS, now))
}

/// (1) A note taken before the add call is consumed by the matching join
/// within the 30 s TTL, stamped with the `web:one_click` source.
#[test]
fn note_before_add_is_consumed_by_matching_join_within_ttl() {
    let (t, mut joins) = tracker(1_000_000);
    joins.expect(GUILD_A, MEMBER, WEB_ONE_CLICK_SOURCE.to_owned());
    // The gateway event lands just inside the TTL window.
    t.store(1_000_000 + 29_999, Ordering::SeqCst);
    assert_eq!(
        joins.consume(GUILD_A, MEMBER).as_deref(),
        Some(WEB_ONE_CLICK_SOURCE)
    );
    // Consuming clears the note: no double attribution.
    assert!(joins.consume(GUILD_A, MEMBER).is_none());
}

/// (2) Expired notes are dropped: the join falls back to the honest
/// `unknown` the tracker already reports (`consume` returns `None`, so
/// attribution proceeds by invite diff as normal).
#[test]
fn expired_note_is_dropped_and_join_falls_back_to_unknown() {
    let (t, mut joins) = tracker(0);
    joins.expect(GUILD_A, MEMBER, WEB_ONE_CLICK_SOURCE.to_owned());
    // Exactly at the TTL boundary the note is already stale (`t - at < ttl`).
    t.store(EXPECTED_JOIN_TTL_SECONDS as i64 * 1000, Ordering::SeqCst);
    assert!(joins.consume(GUILD_A, MEMBER).is_none());
    assert!(joins.is_empty());
}

/// (3) Notes are keyed by guild+member: a note in one guild never credits
/// another guild, and never credits a different member.
#[test]
fn notes_are_keyed_by_guild_and_member() {
    let (_t, mut joins) = tracker(0);
    joins.expect(GUILD_A, MEMBER, WEB_ONE_CLICK_SOURCE.to_owned());
    // Same member, other guild: no credit.
    assert!(joins.consume(GUILD_B, MEMBER).is_none());
    // Same guild, other member: no credit.
    assert!(joins.consume(GUILD_A, OTHER_MEMBER).is_none());
    // The real guild+member still holds its note.
    assert_eq!(
        joins.consume(GUILD_A, MEMBER).as_deref(),
        Some(WEB_ONE_CLICK_SOURCE)
    );
}

/// (4) Dying between the add call and the event (note present, no consumer
/// survives) still reports unknown: a fresh tracker — what a restarted
/// process sees — attributes nothing, never a fabricated source.
#[test]
fn dying_between_add_and_event_reports_unknown_never_fabricated() {
    let (_t, mut before_crash) = tracker(0);
    before_crash.expect(GUILD_A, MEMBER, WEB_ONE_CLICK_SOURCE.to_owned());
    assert_eq!(before_crash.len(), 1);
    // The process dies with its in-memory notes; nothing is consumed.
    drop(before_crash);

    let (_t, mut after_restart) = tracker(5_000);
    assert!(after_restart.is_empty());
    assert!(after_restart.consume(GUILD_A, MEMBER).is_none());
}

/// (5) The TTL and source constants equal their legacy values
/// (`DEFAULT_TTL_SECONDS`, `WEB_ONE_CLICK_SOURCE`).
#[test]
fn constants_match_legacy_values() {
    assert_eq!(EXPECTED_JOIN_TTL_SECONDS, 30);
    assert_eq!(WEB_ONE_CLICK_SOURCE, "web:one_click");
}
