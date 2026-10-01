#[path = "support/membership_contract.rs"]
mod contract;

use two_bot_core::membership::{normalize_timestamp, MembershipStore};
use two_bot_core::MemStore;
use two_bot_core::{EventType, FunnelEvent};

#[test]
fn every_chronology_and_precision_permutation_and_prefix() {
    contract::permutations_contract(&MemStore::new);
}
#[test]
fn inactivity_is_tied_to_actual_join_not_reconfirmation() {
    contract::inactivity(&MemStore::new);
}
#[test]
fn precision_survives_offsets_and_independent_observation_order() {
    contract::precision(&MemStore::new);
}
#[test]
fn replay_preserves_identity_attribution_metadata_and_presence() {
    contract::replay(&MemStore::new);
}
#[test]
fn concurrent_writes_and_delayed_stale_duplicates_converge() {
    contract::concurrent(&MemStore::new);
}
#[test]
fn dispatch_and_rest_completion_do_not_reverse_observation_order() {
    contract::dispatch_and_rest(&MemStore::new);
}
#[test]
fn explicit_observation_maximum_is_independent_of_occurrence() {
    contract::observation_boundaries(&MemStore::new);
}
#[test]
fn same_tick_and_backward_clock_keep_occurrence_and_observation_separate() {
    contract::clock();
}

fn event(kind: EventType, at: &str) -> FunnelEvent {
    FunnelEvent {
        guild_id: 1,
        member_id: Some(2),
        event_type: kind,
        occurred_at: at.into(),
        source: "gateway".into(),
        metadata: None,
        dedupe_token: None,
    }
}

#[test]
fn equal_observation_favors_leave_and_equal_join_clears_inactivity() {
    // Additional boundaries from legacy src/store/eventStore.ts:288-306.
    let at = "2026-08-01T00:00:00.000001Z";
    for reverse in [false, true] {
        let s = MemStore::new();
        let mut events = vec![
            event(EventType::MemberJoin, at),
            event(EventType::MemberLeave, at),
            event(EventType::MemberInactive, at),
        ];
        if reverse {
            events.reverse();
        }
        for e in events {
            s.record_observed(e, Some(at));
        }
        let m = s.membership(1, 2).unwrap();
        assert_eq!(m.joined_at.as_deref(), Some(at));
        assert_eq!(m.left_at.as_deref(), Some(at));
        assert!(m.inactive_flagged_at.is_none());
    }
}

#[test]
fn invalid_hints_do_not_change_presence_or_non_membership_metadata() {
    // Additional validation boundaries from legacy src/store/eventStore.ts:91-106.
    let s = MemStore::new();
    let join = event(EventType::MemberJoin, "2026-08-01T00:00:00.000001Z");
    s.record_observed(join.clone(), None);
    s.record_observed(
        event(EventType::MemberLeave, "2026-08-01T00:00:00.000002Z"),
        None,
    );
    for invalid in [
        "bad",
        "2026-09-01T00:00:00.0000011Z",
        "2026-09-01T00:00:00.00Z",
        "2026-09-01T00:00:00.000+00:00",
        "2026-02-30T00:00:00.000Z",
    ] {
        assert!(!s.record_observed(join.clone(), Some(invalid)).inserted);
    }
    let before = s.membership(1, 2).unwrap();
    assert_eq!(
        before.left_at.as_deref(),
        Some("2026-08-01T00:00:00.000002Z")
    );
    let mut message = event(EventType::FirstMessage, "2026-08-02T00:00:00.000Z");
    message.metadata = Some(serde_json::json!({"keep":true}));
    s.record_observed(message.clone(), Some("2026-09-01T00:00:00.000Z"));
    s.record_observed(message, Some("2026-10-01T00:00:00.000Z"));
    assert_eq!(s.membership(1, 2).unwrap(), before);
    assert_eq!(
        s.membership_rows(1, 2)
            .iter()
            .find(|r| r.event_type == EventType::FirstMessage)
            .unwrap()
            .metadata,
        Some(serde_json::json!({"keep":true}))
    );
    assert!(s.membership(3, 2).is_none());
    assert!(s.membership(1, 4).is_none());
}

#[test]
fn timestamp_normalization_retains_microseconds_without_rounding() {
    assert_eq!(
        normalize_timestamp("2026-07-31 20:00:00.000001-04"),
        Some("2026-08-01T00:00:00.000001Z".into())
    );
    assert_eq!(
        normalize_timestamp("2026-08-01T02:00:00.001+02:00"),
        Some("2026-08-01T00:00:00.001000Z".into())
    );
    assert!(normalize_timestamp("2026-02-30T00:00:00.000Z").is_none());
    assert!(normalize_timestamp("2026-08-01T00:00:00.0000001Z").is_none());
    assert!(normalize_timestamp("2026-08-01T00:00:00+0é0").is_none());
}
