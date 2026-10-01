//! Shared store contract. See docs/membership-contract.md for source mapping
//! and the sqlx hook. Every factory call must return an isolated, empty store.

use two_bot_core::membership::{
    member_observation, normalize_timestamp, Membership, MembershipClock, MembershipStore,
};
use two_bot_core::{EventType, FunnelEvent, StoredRow};

const G: u64 = 1;
const M: u64 = 2;
const FIRST: &str = "2026-09-29T00:00:00.000Z";
const REJOIN: &str = "2026-09-30T00:30:00.000Z";
const RESUMED: &str = "2026-09-30T01:00:00.000Z";

fn event(kind: EventType, at: &str, source: &str) -> FunnelEvent {
    FunnelEvent {
        guild_id: G,
        member_id: Some(M),
        event_type: kind,
        occurred_at: at.into(),
        source: source.into(),
        metadata: None,
        dedupe_token: None,
    }
}

fn join(at: &str, source: &str) -> FunnelEvent {
    event(EventType::MemberJoin, at, source)
}
fn leave(at: &str) -> FunnelEvent {
    event(EventType::MemberLeave, at, "gateway")
}
fn flag(at: &str) -> FunnelEvent {
    event(EventType::MemberInactive, at, "job:inactivity")
}
fn utc(at: &str) -> String {
    normalize_timestamp(at).expect("fixture timestamp")
}
fn state<S: MembershipStore>(s: &S) -> Membership {
    s.membership(G, M).expect("member")
}
fn expect<S: MembershipStore>(
    s: &S,
    joined: Option<&str>,
    source: Option<&str>,
    left: Option<&str>,
    inactive: Option<&str>,
) {
    assert_eq!(
        state(s),
        Membership {
            joined_at: joined.map(utc),
            join_source: source.map(str::to_owned),
            left_at: left.map(utc),
            inactive_flagged_at: inactive.map(utc),
        }
    );
}
fn row<S: MembershipStore>(s: &S, kind: EventType) -> StoredRow {
    s.membership_rows(G, M)
        .into_iter()
        .find(|r| r.event_type == kind)
        .expect("event")
}
fn observed<S: MembershipStore>(s: &S, kind: EventType) -> String {
    row(s, kind).metadata.expect("metadata")["membershipObservedAt"]
        .as_str()
        .expect("observation")
        .into()
}

fn permutations(events: &[FunnelEvent]) -> Vec<Vec<FunnelEvent>> {
    if events.is_empty() {
        return vec![vec![]];
    }
    let mut result = Vec::new();
    for i in 0..events.len() {
        let mut rest = events.to_vec();
        let first = rest.remove(i);
        for mut tail in permutations(&rest) {
            tail.insert(0, first.clone());
            result.push(tail);
        }
    }
    result
}

/// Source: two-bot@bffccf3 test/unit.membership-chronology.test.ts:45 and test/unit.membership-precision.test.ts:55.
pub fn permutations_contract<S: MembershipStore>(make: &impl Fn() -> S) {
    for stamps in [
        [
            "2026-08-01T00:00:00.000Z",
            "2026-08-02T00:00:00.000Z",
            "2026-08-03T00:00:00.000Z",
            "2026-08-04T00:00:00.000Z",
        ],
        [
            "2026-08-01T00:00:00.000001Z",
            "2026-08-01T00:00:00.000002Z",
            "2026-08-01T00:00:00.000003Z",
            "2026-08-01T00:00:00.000004Z",
        ],
        [
            "2026-08-01T00:00:00.001Z",
            "2026-08-01T00:00:00.002Z",
            "2026-08-01T00:00:00.003Z",
            "2026-08-01T00:00:00.004Z",
        ],
    ] {
        let journey = [
            join(stamps[0], "invite:first"),
            leave(stamps[1]),
            join(stamps[2], "invite:latest"),
            leave(stamps[3]),
        ];
        for size in [3, 4] {
            let orders = permutations(&journey[..size]);
            assert_eq!(orders.len(), if size == 3 { 6 } else { 24 });
            for order in orders {
                let s = make();
                let mut seen = Vec::<FunnelEvent>::new();
                for e in &order {
                    assert!(s.record(e.clone()).inserted);
                    seen.push(e.clone());
                    let latest_join = seen
                        .iter()
                        .filter(|e| e.event_type == EventType::MemberJoin)
                        .max_by_key(|e| utc(&e.occurred_at));
                    let latest = seen
                        .iter()
                        .max_by_key(|e| {
                            (utc(&e.occurred_at), e.event_type == EventType::MemberLeave)
                        })
                        .unwrap();
                    expect(
                        &s,
                        latest_join.map(|e| e.occurred_at.as_str()),
                        latest_join.map(|e| e.source.as_str()),
                        (latest.event_type == EventType::MemberLeave)
                            .then_some(latest.occurred_at.as_str()),
                        None,
                    );
                }
                for e in &order {
                    assert!(!s.record(e.clone()).inserted);
                }
                let mut history = s.membership_rows(G, M);
                history.sort_by_key(|r| utc(&r.occurred_at));
                assert_eq!(history.len(), size);
                for (actual, original) in history.iter().zip(&journey) {
                    assert_eq!(utc(&actual.occurred_at), utc(&original.occurred_at));
                    assert_eq!(actual.source, original.source);
                    assert_eq!(actual.event_type, original.event_type);
                }
            }
        }
    }
}

pub fn inactivity<S: MembershipStore>(make: &impl Fn() -> S) {
    let j1 = "2026-08-01T00:00:00.000Z";
    let l1 = "2026-08-02T00:00:00.000Z";
    let j2 = "2026-08-03T00:00:00.000Z";
    let l2 = "2026-08-04T00:00:00.000Z";
    // Source: two-bot@bffccf3 test/unit.membership-chronology.test.ts:79.
    let s = make();
    s.record(join(j2, "invite:latest"));
    s.record(flag(l2));
    let before = state(&s);
    s.record(join(j1, "invite:first"));
    assert_eq!(state(&s), before);
    expect(&s, Some(j2), Some("invite:latest"), None, Some(l2));

    // Source: two-bot@bffccf3 test/unit.membership-chronology.test.ts:89.
    let s = make();
    s.record(join(j1, "invite:first"));
    let milestone = event(
        EventType::FirstMessage,
        "2026-08-01T12:00:00.000Z",
        "channel:10",
    );
    s.record(milestone.clone());
    s.record(flag("2026-08-01T18:00:00.000Z"));
    s.record(leave(l1));
    s.record(join(j2, "invite:latest"));
    expect(&s, Some(j2), Some("invite:latest"), None, None);
    assert_eq!(
        row(&s, EventType::FirstMessage).occurred_at,
        milestone.occurred_at
    );
    assert!(!s.record(milestone).inserted);

    // Source: two-bot@bffccf3 test/unit.membership-chronology.test.ts:105.
    for order in permutations(&[join(j2, "invite:latest"), leave(l2)]) {
        let s = make();
        s.record(join(j1, "invite:first"));
        s.record(flag(l1));
        for e in order {
            assert!(s.record(e).inserted);
        }
        expect(&s, Some(j2), Some("invite:latest"), Some(l2), None);
    }
    // Source: two-bot@bffccf3 test/unit.membership-chronology.test.ts:124.
    let s = make();
    let original = join(j1, "invite:first");
    s.record_observed(original.clone(), Some(j1));
    s.record(flag(l1));
    let key = row(&s, EventType::MemberJoin).idempotency_key;
    assert!(!s.record_observed(join(j1, "unknown"), Some(l2)).inserted);
    expect(&s, Some(j1), Some("invite:first"), None, Some(l1));
    assert_eq!(row(&s, EventType::MemberJoin).idempotency_key, key);
    assert_eq!(s.membership_rows(G, M).len(), 2);

    // Source: two-bot@bffccf3 test/unit.membership-precision.test.ts:92.
    for (at, remains) in [
        ("2026-08-01T00:00:00.000002Z", false),
        ("2026-08-01T00:00:00.000004Z", true),
    ] {
        let s = make();
        s.record(join("2026-08-01T00:00:00.000001Z", "invite:first"));
        s.record(flag(at));
        s.record(join("2026-08-01T00:00:00.000003Z", "invite:latest"));
        expect(
            &s,
            Some("2026-08-01T00:00:00.000003Z"),
            Some("invite:latest"),
            None,
            remains.then_some(at),
        );
    }
}

pub fn precision<S: MembershipStore>(make: &impl Fn() -> S) {
    // Source: two-bot@bffccf3 test/unit.membership-precision.test.ts:109 (database session offset represented at store text boundary).
    let s = make();
    for e in [
        join("2026-07-31 20:00:00.000003-04", "invite:latest"),
        leave("2026-07-31 20:00:00.000002-04"),
        join("2026-07-31 20:00:00.000001-04", "invite:first"),
    ] {
        assert!(s.record(e).inserted);
        expect(
            &s,
            Some("2026-08-01T00:00:00.000003Z"),
            Some("invite:latest"),
            None,
            None,
        );
    }
    // Source: two-bot@bffccf3 test/unit.membership-precision.test.ts:122.
    let s = make();
    for (e, at) in [
        (
            join("2026-08-01T00:00:00.000001Z", "invite:first"),
            "2026-08-01T00:00:01.000001Z",
        ),
        (
            join("2026-08-01T00:00:00.000003Z", "invite:latest"),
            "2026-08-01T00:00:01.000002Z",
        ),
        (
            leave("2026-08-01T00:00:00.000002Z"),
            "2026-08-01T00:00:01.000003Z",
        ),
    ] {
        s.record_observed(e, Some(at));
    }
    expect(
        &s,
        Some("2026-08-01T00:00:00.000003Z"),
        Some("invite:latest"),
        Some("2026-08-01T00:00:00.000002Z"),
        None,
    );
}

pub fn replay<S: MembershipStore>(make: &impl Fn() -> S) {
    // Source: two-bot@bffccf3 test/unit.membership-replay.test.ts:36 (both completion orders).
    for leave_first in [true, false] {
        let s = make();
        s.record(join(FIRST, "invite:first"));
        let mut writes = vec![
            (leave(RESUMED), "2026-09-30T01:00:00.000001Z"),
            (join(REJOIN, "invite:latest"), "2026-09-30T01:00:00.000002Z"),
        ];
        if !leave_first {
            writes.reverse();
        }
        for (e, at) in writes {
            assert!(s.record_observed(e, Some(at)).inserted);
        }
        expect(&s, Some(REJOIN), Some("invite:latest"), None, None);
        assert_eq!(s.membership_rows(G, M).len(), 3);
        s.record(event(
            EventType::MemberLeave,
            "2026-09-30T00:45:00.000Z",
            "backfill:member_list",
        ));
        assert!(!s.record(join(FIRST, "unknown")).inserted);
        expect(&s, Some(REJOIN), Some("invite:latest"), None, None);
    }
    // Source: two-bot@bffccf3 test/unit.membership-replay.test.ts:66.
    let s = make();
    let clock = MembershipClock::default();
    let wall = two_bot_core::parse_iso_millis(RESUMED).unwrap();
    for e in [
        join(REJOIN, "invite:latest"),
        leave(RESUMED),
        join(REJOIN, "unknown"),
    ] {
        s.record_observed(e, Some(&clock.next_at(wall)));
    }
    expect(&s, Some(REJOIN), Some("invite:latest"), None, None);
    assert_eq!(s.membership_rows(G, M).len(), 2);

    // Source: two-bot@bffccf3 test/unit.membership-replay.test.ts:81.
    let s = make();
    let actual = "2026-09-01T00:00:00.000Z";
    let mut original = join(actual, "invite:first");
    original.metadata = Some(serde_json::json!({"inviterId":"original-inviter"}));
    s.record(original.clone());
    s.record(flag("2026-09-20T00:00:00.000Z"));
    s.record(leave("2026-09-25T00:00:00.000Z"));
    let key = row(&s, EventType::MemberJoin).idempotency_key;
    assert!(
        !s.record_observed(join(actual, "unknown"), Some("2026-09-30T00:00:00.000Z"))
            .inserted
    );
    expect(
        &s,
        Some(actual),
        Some("invite:first"),
        None,
        Some("2026-09-20T00:00:00.000Z"),
    );
    let saved = row(&s, EventType::MemberJoin);
    assert_eq!(saved.idempotency_key, key);
    assert_eq!(saved.source, original.source);
    assert_eq!(saved.occurred_at, original.occurred_at);
    assert_eq!(
        saved.metadata.unwrap(),
        serde_json::json!({"inviterId":"original-inviter","membershipObservedAt":"2026-09-30T00:00:00.000Z"})
    );
    s.record_observed(join(REJOIN, "invite:latest"), Some(RESUMED));
    expect(&s, Some(REJOIN), Some("invite:latest"), None, None);
    assert_eq!(
        s.membership_rows(G, M)
            .iter()
            .filter(|r| r.event_type == EventType::MemberJoin)
            .count(),
        2
    );
}

pub fn concurrent<S: MembershipStore>(make: &impl Fn() -> S) {
    // Source: two-bot@bffccf3 test/unit.membership-chronology.test.ts:142 and test/unit.membership-precision.test.ts:81.
    for stamps in [
        [
            "2026-08-01T00:00:00.000Z",
            "2026-08-02T00:00:00.000Z",
            "2026-08-03T00:00:00.000Z",
            "2026-08-04T00:00:00.000Z",
        ],
        [
            "2026-08-01T00:00:00.000001Z",
            "2026-08-01T00:00:00.000002Z",
            "2026-08-01T00:00:00.000003Z",
            "2026-08-01T00:00:00.000004Z",
        ],
    ] {
        for size in [3, 4] {
            let s = make();
            let barrier = std::sync::Barrier::new(size);
            let events = [
                join(stamps[0], "invite:first"),
                leave(stamps[1]),
                join(stamps[2], "invite:latest"),
                leave(stamps[3]),
            ];
            std::thread::scope(|scope| {
                let handles: Vec<_> = events[..size]
                    .iter()
                    .map(|e| {
                        let s = &s;
                        let barrier = &barrier;
                        scope.spawn(move || {
                            barrier.wait();
                            s.record(e.clone())
                        })
                    })
                    .collect();
                for h in handles {
                    assert!(h.join().unwrap().inserted);
                }
            });
            expect(
                &s,
                Some(stamps[2]),
                Some("invite:latest"),
                (size == 4).then_some(stamps[3]),
                None,
            );
            assert_eq!(s.membership_rows(G, M).len(), size);
        }
    }
    // Source: two-bot@bffccf3 test/unit.membership-replay.test.ts:121.
    let s = make();
    let mut original = join(REJOIN, "invite:latest");
    original.metadata = Some(serde_json::json!({"inviterId":"inviter"}));
    std::thread::scope(|scope| {
        scope.spawn(|| s.record_observed(original, Some("2026-09-30T01:00:00.000002Z")));
        scope.spawn(|| s.record_observed(leave(RESUMED), Some("2026-09-30T01:00:00.000001Z")));
    });
    expect(&s, Some(REJOIN), Some("invite:latest"), None, None);
    s.record_observed(leave(RESUMED), Some("2026-09-30T01:00:00.000003Z"));
    std::thread::scope(|scope| {
        scope.spawn(|| {
            s.record_observed(join(REJOIN, "unknown"), Some("2026-09-30T01:00:00.000001Z"))
        });
        scope.spawn(|| s.record_observed(leave(RESUMED), Some("2026-09-30T01:00:00.000001Z")));
    });
    expect(&s, Some(REJOIN), Some("invite:latest"), Some(RESUMED), None);
    assert_eq!(s.membership_rows(G, M).len(), 2);
    assert_eq!(
        row(&s, EventType::MemberJoin).metadata.unwrap()["inviterId"],
        "inviter"
    );
    assert_eq!(
        observed(&s, EventType::MemberJoin),
        "2026-09-30T01:00:00.000002Z"
    );
    assert_eq!(
        observed(&s, EventType::MemberLeave),
        "2026-09-30T01:00:00.000003Z"
    );

    // Source: two-bot@bffccf3 test/unit.membership-replay.test.ts:160 (delayed old duplicate); :217 (five collisions, store-independent maximum guarantee).
    let s = make();
    s.record_observed(
        join(FIRST, "invite:original"),
        Some("2026-09-30T01:00:00.000000Z"),
    );
    s.record_observed(leave(RESUMED), Some("2026-09-30T01:00:00.000010Z"));
    let key = row(&s, EventType::MemberJoin).idempotency_key;
    let (release, wait) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let store = &s;
        let old = scope.spawn(move || {
            wait.recv().unwrap();
            store.record_observed(join(FIRST, "unknown"), Some("2026-09-30T01:00:00.000001Z"))
        });
        let peers: Vec<_> = (2..=5)
            .map(|n| {
                let s = &s;
                scope.spawn(move || {
                    s.record_observed(
                        join(FIRST, "unknown"),
                        Some(&format!("2026-09-30T01:00:00.{n:06}Z")),
                    )
                })
            })
            .collect();
        assert!(
            !s.record_observed(join(FIRST, "unknown"), Some("2026-09-30T01:00:00.000050Z"))
                .inserted
        );
        for peer in peers {
            assert!(!peer.join().unwrap().inserted);
        }
        release.send(()).unwrap();
        assert!(!old.join().unwrap().inserted);
    });
    expect(&s, Some(FIRST), Some("invite:original"), None, None);
    assert_eq!(row(&s, EventType::MemberJoin).idempotency_key, key);
    assert_eq!(
        observed(&s, EventType::MemberJoin),
        "2026-09-30T01:00:00.000050Z"
    );
    assert_eq!(s.membership_rows(G, M).len(), 2);
}

pub fn dispatch_and_rest<S: MembershipStore>(make: &impl Fn() -> S) {
    // Source: two-bot@bffccf3 test/unit.membership-replay.test.ts:282 (delayed leave/add).
    for delayed_leave in [true, false] {
        let s = make();
        let clock = MembershipClock::default();
        let wall = two_bot_core::parse_iso_millis(RESUMED).unwrap();
        let first_stamp = clock.next_at(wall);
        let second_stamp = clock.next_at(wall);
        let (first, second) = if delayed_leave {
            (leave(RESUMED), join(REJOIN, "invite:latest"))
        } else {
            (join(REJOIN, "invite:latest"), leave(RESUMED))
        };
        s.record_observed(second, Some(&second_stamp));
        s.record_observed(first, Some(&first_stamp));
        expect(
            &s,
            Some(REJOIN),
            Some("invite:latest"),
            (!delayed_leave).then_some(RESUMED),
            None,
        );
    }
    const START: &str = "2026-09-30T10:00:00.000Z";
    const LATER: &str = "2026-09-30T10:01:00.000Z";
    const FINISHED: &str = "2026-09-30T10:02:00.000Z";
    // Source: two-bot@bffccf3 test/unit.membership-rest-observation.test.ts:9 (request-start evidence despite later headers/body).
    // The store boundary receives the page's successful attempt start. HTTP
    // transport is intentionally not emulated by this generic store contract.
    let s = make();
    s.record(leave(LATER));
    let page_start = member_observation(START, FIRST).unwrap();
    assert!(![utc(LATER), utc(FINISHED)].contains(&page_start));
    s.record_observed(join(FIRST, "backfill:member_list"), Some(&page_start));
    expect(
        &s,
        Some(FIRST),
        Some("backfill:member_list"),
        Some(LATER),
        None,
    );
    // Source: two-bot@bffccf3 test/unit.membership-rest-observation.test.ts:31 (successful retry start replaces failed attempt start).
    let s = make();
    s.record(leave("2026-09-30T10:00:30.000Z"));
    let successful_start = member_observation(LATER, FIRST).unwrap();
    s.record_observed(join(FIRST, "backfill:member_list"), Some(&successful_start));
    expect(&s, Some(FIRST), Some("backfill:member_list"), None, None);
    // Failed-page/data-only behavior is exercised against the real executor
    // by crates/discord/tests/membership_observation.rs, not a fake store no-op.
}

pub fn observation_boundaries<S: MembershipStore>(make: &impl Fn() -> S) {
    // Source: two-bot@bffccf3 src/store/eventStore.ts:91-176 (explicit hints, metadata-only duplicate maximum).
    const BEFORE: &str = "2026-09-30T00:00:00.000001Z";
    const BETWEEN: &str = "2026-09-30T00:15:00.000001Z";
    for hint in [BEFORE, REJOIN] {
        for duplicate in [false, true] {
            let s = make();
            s.record(leave(BETWEEN));
            let original = join(REJOIN, "invite:original");
            if duplicate {
                assert!(s.record(original.clone()).inserted);
            }
            assert_eq!(s.record_observed(original, Some(hint)).inserted, !duplicate);
            assert_eq!(observed(&s, EventType::MemberJoin), hint);
            expect(
                &s,
                Some(REJOIN),
                Some("invite:original"),
                (hint == BEFORE).then_some(BETWEEN),
                None,
            );
            let original_row = row(&s, EventType::MemberJoin);
            let mut replay = join(REJOIN, "unknown");
            replay.metadata = Some(serde_json::json!({
                "membershipObservedAt": RESUMED,
                "unrelated": "must not replace original metadata"
            }));
            assert!(!s.record_observed(replay.clone(), None).inserted);
            assert_eq!(
                row(&s, EventType::MemberJoin).metadata,
                original_row.metadata
            );
            assert!(!s.record_observed(replay, Some(BEFORE)).inserted);
            assert_eq!(observed(&s, EventType::MemberJoin), hint);
            let current = row(&s, EventType::MemberJoin);
            assert_eq!(current.idempotency_key, original_row.idempotency_key);
            assert_eq!(current.source, original_row.source);
            assert_eq!(current.occurred_at, original_row.occurred_at);
        }
    }

    let s = make();
    let original = join(REJOIN, "invite:original");
    s.record(original.clone());
    let mut replay = original.clone();
    replay.metadata = Some(serde_json::json!({"membershipObservedAt": RESUMED}));
    s.record_observed(replay, None);
    assert!(row(&s, EventType::MemberJoin).metadata.is_none());
    s.record_observed(original.clone(), Some(BEFORE));
    s.record_observed(original, Some(BETWEEN));
    assert_eq!(observed(&s, EventType::MemberJoin), BETWEEN);
}

pub fn clock() {
    // Source: two-bot@bffccf3 test/unit.membership-clock.test.ts:5.
    let clock = MembershipClock::default();
    let wall = two_bot_core::parse_iso_millis(RESUMED).unwrap();
    assert_eq!(clock.next_at(wall), "2026-09-30T01:00:00.000000Z");
    assert_eq!(clock.next_at(wall), "2026-09-30T01:00:00.000001Z");
    assert_eq!(clock.next_at(wall + 1), "2026-09-30T01:00:00.001000Z");
    assert_eq!(clock.next_at(wall), "2026-09-30T01:00:00.001001Z");
    assert_eq!(two_bot_core::format_iso_millis(wall), RESUMED);
}

/// Single reusable entry point for a future durable store adapter.
#[allow(dead_code)]
pub fn run<S: MembershipStore>(make: impl Fn() -> S) {
    permutations_contract(&make);
    inactivity(&make);
    precision(&make);
    replay(&make);
    concurrent(&make);
    dispatch_and_rest(&make);
    observation_boundaries(&make);
    clock();
}
