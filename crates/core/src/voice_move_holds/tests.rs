use super::*;

const MEMBER: Snowflake = 7;
const TARGET: Snowflake = 11;
const OTHER: Snowflake = 12;

#[test]
fn pending_and_old_publications_cannot_settle_a_hold() {
    let mut holds = VoiceMoveHolds::default();
    let before = holds.begin_transition();
    assert_eq!(holds.reserve(MEMBER, TARGET), Ok(()));
    holds.observe(MEMBER, Some(TARGET), before);
    let pending = holds.begin_transition();
    holds.observe(MEMBER, Some(TARGET), pending);
    assert_eq!(holds.occupancy(MEMBER, TARGET, 0), Err(HoldRefused));
    holds.complete(MEMBER);
    holds.observe(MEMBER, Some(TARGET), pending);
    assert_eq!(holds.occupancy(8, TARGET, 1), Ok(2));
    let newer = holds.begin_transition();
    holds.observe(8, Some(TARGET), newer);
    holds.observe(MEMBER, Some(OTHER), newer);
    holds.observe(MEMBER, None, newer);
    assert_eq!(holds.occupancy(8, TARGET, 1), Ok(2));
    holds.observe(MEMBER, Some(TARGET), newer);
    assert_eq!(holds.occupancy(8, TARGET, 1), Ok(1));
    assert_eq!(holds.reserve(MEMBER, TARGET), Ok(()));
    holds.observe(MEMBER, Some(TARGET), newer);
    assert_eq!(holds.reserve(MEMBER, TARGET), Err(HoldRefused));
}

#[test]
fn completion_never_replays_early_arrival_or_moves_the_fence() {
    let mut holds = VoiceMoveHolds::default();
    holds.reserve(MEMBER, TARGET).unwrap();
    let early = holds.begin_transition();
    holds.observe(MEMBER, Some(TARGET), early);
    holds.complete(MEMBER);
    let newer = holds.begin_transition();
    holds.complete(MEMBER);
    holds.observe(MEMBER, Some(TARGET), early);
    assert_eq!(holds.reserve(MEMBER, TARGET), Err(HoldRefused));
    holds.observe(MEMBER, Some(TARGET), newer);
    assert_eq!(holds.reserve(MEMBER, TARGET), Ok(()));
}

#[test]
fn counts_all_holds_and_only_proven_no_effect_releases() {
    let mut holds = VoiceMoveHolds::default();
    holds.reserve(MEMBER, TARGET).unwrap();
    holds.reserve(8, TARGET).unwrap();
    holds.reserve(9, OTHER).unwrap();
    assert_eq!(holds.occupancy(10, TARGET, 3), Ok(5));
    assert_eq!(holds.occupancy(10, OTHER, u32::MAX), Ok(u32::MAX));
    assert_eq!(holds.occupancy(MEMBER, OTHER, 0), Err(HoldRefused));
    holds.release_no_effect(MEMBER);
    assert_eq!(holds.occupancy(10, TARGET, 3), Ok(4));
    assert_eq!(holds.reserve(MEMBER, TARGET), Ok(()));
}

#[test]
fn full_ledger_and_duplicate_fail_closed() {
    let mut holds = VoiceMoveHolds::default();
    for member in 1..=VoiceMoveHolds::LIMIT as u64 {
        assert_eq!(holds.reserve(member, TARGET), Ok(()));
    }
    assert_eq!(holds.reserve(2000, TARGET), Err(HoldRefused));
    holds.complete(1);
    let generation = holds.begin_transition();
    holds.observe(1, Some(OTHER), generation);
    assert_eq!(holds.reserve(2000, TARGET), Err(HoldRefused));
    holds.observe(1, Some(TARGET), generation);
    assert_eq!(holds.reserve(2000, TARGET), Ok(()));
    assert_eq!(holds.reserve(2000, OTHER), Err(HoldRefused));
}

#[test]
fn generation_exhaustion_fails_closed_without_wrapping() {
    let mut holds = VoiceMoveHolds {
        generation: u64::MAX - 1,
        ..VoiceMoveHolds::default()
    };
    holds.reserve(MEMBER, TARGET).unwrap();
    holds.complete(MEMBER);
    let last = holds.begin_transition();
    assert_eq!(last, u64::MAX);
    holds.observe(MEMBER, Some(TARGET), last);
    assert_eq!(holds.occupancy(8, TARGET, 0), Ok(0));
    assert_eq!(holds.reserve(MEMBER, TARGET), Err(HoldRefused));
    assert_eq!(holds.begin_transition(), u64::MAX);

    holds.generation = u64::MAX - 1;
    holds.reserve(MEMBER, TARGET).unwrap();
    assert_eq!(holds.begin_transition(), u64::MAX);
    holds.complete(MEMBER);
    let exhausted = holds.begin_transition();
    holds.observe(MEMBER, Some(TARGET), exhausted);
    assert_eq!(holds.occupancy(MEMBER, TARGET, 0), Err(HoldRefused));
}

#[test]
fn reserved_occupancy_counts_own_seat_only_for_the_matching_pending_hold() {
    let mut holds = VoiceMoveHolds::default();
    assert_eq!(holds.reserved_occupancy(1, 2, 0), None);
    holds.reserve(1, 2).unwrap();
    holds.reserve(3, 2).unwrap();
    holds.reserve(4, 5).unwrap();
    assert_eq!(holds.reserved_occupancy(1, 2, 4), Some(6));
    assert_eq!(holds.reserved_occupancy(1, 5, 0), None);
    holds.complete(1);
    assert_eq!(holds.reserved_occupancy(1, 2, 0), None);
    assert_eq!(holds.reserved_occupancy(3, 2, 0), Some(2));
}
