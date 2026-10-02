//! Hermetic V8b acceptance cases against the public placement core API.

use proptest::prelude::*;
use two_bot_core::voice_placement::{
    next_room_number, plan_placement, resolve_initial_state, CategoryChannel, CategoryEntryKind,
    PlacementError, PlacementRequest, RoomInitialState, RoomSide, MAX_ROOM_USER_LIMIT,
};

fn creator(id: u64, position: i32) -> CategoryChannel {
    CategoryChannel {
        id,
        position,
        kind: CategoryEntryKind::Creator,
    }
}

fn room(id: u64, position: i32) -> CategoryChannel {
    CategoryChannel {
        id,
        position,
        kind: CategoryEntryKind::Room,
    }
}

fn other(id: u64, position: i32) -> CategoryChannel {
    CategoryChannel {
        id,
        position,
        kind: CategoryEntryKind::Other,
    }
}

fn request<'a>(
    creator_id: u64,
    side: RoomSide,
    grouped: bool,
    group_room_ids: &'a [u64],
    category_order: &'a [CategoryChannel],
) -> PlacementRequest<'a> {
    PlacementRequest {
        creator_id,
        side,
        grouped,
        group_room_ids,
        category_order,
    }
}

/// Display order the runtime sees: `(position, id)` ascending.
fn display_order(category: &[CategoryChannel]) -> Vec<u64> {
    let mut sorted = category.to_vec();
    sorted.sort_by_key(|entry| (entry.position, entry.id));
    sorted.into_iter().map(|entry| entry.id).collect()
}

fn with_insertion(category: &[CategoryChannel], index: usize, new_id: u64) -> Vec<u64> {
    let mut order = display_order(category);
    order.insert(index, new_id);
    order
}

// ---- next_room_number ----

#[test]
fn numbering_starts_at_first_number_when_empty() {
    assert_eq!(next_room_number(&[], 1), 1);
    assert_eq!(next_room_number(&[], 5), 5);
}

#[test]
fn numbering_fills_gaps_instead_of_appending() {
    // A deleted room's number is reused by the next room.
    assert_eq!(next_room_number(&[1, 2, 4], 1), 3);
    assert_eq!(next_room_number(&[2, 3, 4], 2), 5);
    assert_eq!(next_room_number(&[1, 3, 5], 1), 2);
}

#[test]
fn numbering_ignores_numbers_below_first_number() {
    // Rooms created under an older, lower start keep their numbers after an
    // admin raises the start; they never shift the next number down.
    assert_eq!(next_room_number(&[1, 2, 3], 5), 5);
    assert_eq!(next_room_number(&[1, 2, 5, 6], 5), 7);
    assert_eq!(next_room_number(&[1, 4, 6, 7], 5), 5);
}

#[test]
fn numbering_accepts_unsorted_input_and_duplicates() {
    assert_eq!(next_room_number(&[4, 2, 2, 1, 3, 3], 1), 5);
    assert_eq!(next_room_number(&[7, 5, 6], 5), 8);
    assert_eq!(next_room_number(&[3, 1, 1], 1), 2);
}

#[test]
fn numbering_supports_first_number_above_one() {
    assert_eq!(next_room_number(&[], 10), 10);
    assert_eq!(next_room_number(&[10, 11], 10), 12);
    assert_eq!(next_room_number(&[10, 12], 10), 11);
    assert_eq!(next_room_number(&[9, 10, 11], 10), 12);
}

#[test]
fn numbering_saturates_at_u32_max() {
    assert_eq!(next_room_number(&[], u32::MAX), u32::MAX);
    assert_eq!(next_room_number(&[u32::MAX], u32::MAX), u32::MAX);
    assert_eq!(
        next_room_number(&[u32::MAX - 1, u32::MAX], u32::MAX - 1),
        u32::MAX
    );
    assert_eq!(next_room_number(&[u32::MAX - 1], u32::MAX - 1), u32::MAX);
}

// ---- plan_placement: ungrouped ----

#[test]
fn creator_only_category_places_adjacent_to_creator() {
    let category = [creator(10, 0)];
    assert_eq!(
        plan_placement(request(10, RoomSide::Above, false, &[], &category)),
        Ok(0)
    );
    assert_eq!(
        plan_placement(request(10, RoomSide::Below, false, &[], &category)),
        Ok(1)
    );
}

#[test]
fn creator_at_top_places_without_moving_others() {
    let category = [creator(10, 0), other(20, 1), room(30, 2)];
    assert_eq!(
        plan_placement(request(10, RoomSide::Above, false, &[], &category)),
        Ok(0)
    );
    assert_eq!(
        plan_placement(request(10, RoomSide::Below, false, &[], &category)),
        Ok(1)
    );
    // Existing relative order is untouched by the decision itself.
    assert_eq!(display_order(&category), vec![10, 20, 30]);
    assert_eq!(with_insertion(&category, 1, 99), vec![10, 99, 20, 30]);
}

#[test]
fn creator_at_bottom_places_without_moving_others() {
    let category = [other(20, 0), room(30, 1), creator(10, 2)];
    assert_eq!(
        plan_placement(request(10, RoomSide::Above, false, &[], &category)),
        Ok(2)
    );
    assert_eq!(
        plan_placement(request(10, RoomSide::Below, false, &[], &category)),
        Ok(3)
    );
}

#[test]
fn placement_uses_display_order_not_input_order() {
    // Input in any order; positions may be sparse and out of sequence.
    let category = [other(20, 100), creator(10, 0), room(30, 50)];
    assert_eq!(display_order(&category), vec![10, 30, 20]);
    assert_eq!(
        plan_placement(request(10, RoomSide::Below, false, &[], &category)),
        Ok(1)
    );
    assert_eq!(
        plan_placement(request(10, RoomSide::Above, false, &[], &category)),
        Ok(0)
    );
}

#[test]
fn position_ties_break_by_channel_id() {
    let category = [creator(30, 0), other(10, 0), other(20, 0)];
    assert_eq!(display_order(&category), vec![10, 20, 30]);
    assert_eq!(
        plan_placement(request(30, RoomSide::Above, false, &[], &category)),
        Ok(2)
    );
    assert_eq!(
        plan_placement(request(30, RoomSide::Below, false, &[], &category)),
        Ok(3)
    );
}

// ---- plan_placement: grouped ----

#[test]
fn group_with_no_rooms_yet_starts_beside_the_creator() {
    let category = [creator(10, 0), creator(20, 1), other(30, 2)];
    assert_eq!(
        plan_placement(request(10, RoomSide::Below, true, &[], &category)),
        Ok(1)
    );
    assert_eq!(
        plan_placement(request(10, RoomSide::Above, true, &[], &category)),
        Ok(0)
    );
}

#[test]
fn group_block_spanning_two_creators_stays_contiguous() {
    // Category: creator A, room of A, creator B, room of B, unrelated tail.
    let category = [
        creator(10, 0),
        room(11, 1),
        creator(20, 2),
        room(21, 3),
        other(30, 4),
    ];
    let group = [11, 21];
    // Below: after the last group room, not beside the triggering creator.
    assert_eq!(
        plan_placement(request(10, RoomSide::Below, true, &group, &category)),
        Ok(4)
    );
    // Above: before the first group room.
    assert_eq!(
        plan_placement(request(20, RoomSide::Above, true, &group, &category)),
        Ok(1)
    );
    // Triggering creator does not matter, only the side and the group set.
    assert_eq!(
        plan_placement(request(20, RoomSide::Below, true, &group, &category)),
        Ok(4)
    );
    assert_eq!(
        plan_placement(request(10, RoomSide::Above, true, &group, &category)),
        Ok(1)
    );
}

#[test]
fn group_split_block_places_at_its_outer_edges() {
    // Channel 40 was moved between the group rooms; the split is not repaired
    // and the new room still lands at the outer edge of the group rooms.
    let category = [
        other(1, 0),
        creator(10, 1),
        room(11, 2),
        other(40, 3),
        room(12, 4),
        creator(20, 5),
    ];
    let group = [12, 11];
    assert_eq!(
        plan_placement(request(10, RoomSide::Below, true, &group, &category)),
        Ok(5)
    );
    assert_eq!(
        plan_placement(request(20, RoomSide::Above, true, &group, &category)),
        Ok(2)
    );
}

#[test]
fn group_room_duplicates_are_deduplicated() {
    let category = [creator(10, 0), room(11, 1), other(30, 2)];
    let group = [11, 11, 11];
    assert_eq!(
        plan_placement(request(10, RoomSide::Below, true, &group, &category)),
        Ok(2)
    );
    assert_eq!(
        plan_placement(request(10, RoomSide::Above, true, &group, &category)),
        Ok(1)
    );
}

// ---- plan_placement: refusals ----

#[test]
fn placement_refusals_name_the_bad_entry() {
    let category = [creator(10, 0), room(11, 1), other(30, 2)];
    assert_eq!(
        plan_placement(request(99, RoomSide::Below, false, &[], &category)),
        Err(PlacementError::UnknownCreator(99))
    );
    assert_eq!(
        plan_placement(request(11, RoomSide::Below, false, &[], &category)),
        Err(PlacementError::NotACreator(11))
    );
    assert_eq!(
        plan_placement(request(30, RoomSide::Below, false, &[], &category)),
        Err(PlacementError::NotACreator(30))
    );
    assert_eq!(
        plan_placement(request(0, RoomSide::Below, false, &[], &category)),
        Err(PlacementError::InvalidChannelId)
    );
    assert_eq!(
        plan_placement(request(10, RoomSide::Below, false, &[11], &category)),
        Err(PlacementError::UnexpectedGroupRooms)
    );
    assert_eq!(
        plan_placement(request(10, RoomSide::Below, true, &[99], &category)),
        Err(PlacementError::UnknownGroupRoom(99))
    );
    assert_eq!(
        plan_placement(request(10, RoomSide::Below, true, &[10], &category)),
        Err(PlacementError::GroupEntryNotRoom(10))
    );
    assert_eq!(
        plan_placement(request(10, RoomSide::Below, true, &[30], &category)),
        Err(PlacementError::GroupEntryNotRoom(30))
    );
    assert_eq!(
        plan_placement(request(10, RoomSide::Below, true, &[0], &category)),
        Err(PlacementError::InvalidChannelId)
    );
    let duplicate = [creator(10, 0), room(11, 1), room(11, 2)];
    assert_eq!(
        plan_placement(request(10, RoomSide::Below, false, &[], &duplicate)),
        Err(PlacementError::DuplicateChannel(11))
    );
    let zero = [creator(10, 0), room(0, 1)];
    assert_eq!(
        plan_placement(request(10, RoomSide::Below, false, &[], &zero)),
        Err(PlacementError::InvalidChannelId)
    );
    let empty: [CategoryChannel; 0] = [];
    assert_eq!(
        plan_placement(request(10, RoomSide::Below, false, &[], &empty)),
        Err(PlacementError::UnknownCreator(10))
    );
}

#[test]
fn placement_refusals_leave_input_unchanged() {
    let category = [creator(10, 0), room(11, 1)];
    let before = category;
    assert!(plan_placement(request(99, RoomSide::Below, false, &[], &category)).is_err());
    assert_eq!(category, before);
}

// ---- resolve_initial_state ----

#[test]
fn initial_state_passes_through_validated_defaults() {
    assert_eq!(
        resolve_initial_state(0, false),
        Ok(RoomInitialState {
            user_limit: 0,
            private: false,
        })
    );
    assert_eq!(
        resolve_initial_state(1, true),
        Ok(RoomInitialState {
            user_limit: 1,
            private: true,
        })
    );
    assert_eq!(
        resolve_initial_state(MAX_ROOM_USER_LIMIT, true),
        Ok(RoomInitialState {
            user_limit: 99,
            private: true,
        })
    );
    assert_eq!(MAX_ROOM_USER_LIMIT, 99);
}

#[test]
fn initial_state_refuses_out_of_range_limits_instead_of_clamping() {
    for limit in [100, 101, 500, u16::MAX] {
        assert_eq!(
            resolve_initial_state(limit, false),
            Err(PlacementError::LimitOutOfRange(limit))
        );
    }
}

// ---- property: existing channels never move ----

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Inserting the new channel at the planned index preserves the relative
    /// order of every existing channel, for arbitrary categories and inputs.
    #[test]
    fn property_planned_insertion_preserves_existing_order(
        positions in proptest::collection::vec(-8..8i32, 0..8),
        side in any::<bool>(),
        grouped in any::<bool>(),
    ) {
        // Deterministic synthetic category: channel 1 is the creator, even
        // IDs below 20 are rooms, the rest are other channels.
        let category: Vec<CategoryChannel> = positions
            .iter()
            .enumerate()
            .map(|(index, &position)| {
                let id = index as u64 + 1;
                let kind = if id == 1 {
                    CategoryEntryKind::Creator
                } else if id < 20 && id.is_multiple_of(2) {
                    CategoryEntryKind::Room
                } else {
                    CategoryEntryKind::Other
                };
                CategoryChannel { id, position, kind }
            })
            .collect();
        let group_rooms: Vec<u64> = category
            .iter()
            .filter(|entry| entry.kind == CategoryEntryKind::Room)
            .map(|entry| entry.id)
            .collect();
        let side = if side { RoomSide::Above } else { RoomSide::Below };
        let request = PlacementRequest {
            creator_id: 1,
            side,
            grouped,
            group_room_ids: if grouped { &group_rooms } else { &[] },
            category_order: &category,
        };
        let before = display_order(&category);
        let index = match plan_placement(request) {
            Ok(index) => index,
            // Empty category has no creator entry; anything else is a bug in
            // the generator, not a silent pass.
            Err(PlacementError::UnknownCreator(1)) => {
                prop_assert!(category.is_empty());
                return Ok(());
            }
            Err(error) => panic!("synthetic category must plan: {error:?}"),
        };
        prop_assert!(index <= category.len());
        let after = with_insertion(&category, index, u64::MAX);
        let retained: Vec<u64> = after.into_iter().filter(|id| *id != u64::MAX).collect();
        prop_assert_eq!(retained, before);
        // The new channel is exactly where the decision says, nowhere else.
        let placed: Vec<u64> = with_insertion(&category, index, u64::MAX);
        prop_assert_eq!(placed[index], u64::MAX);
    }

    /// Lowest-free-number is stable: it never collides and never shifts when
    /// unrelated lower numbers appear.
    #[test]
    fn property_next_number_is_free_and_stable(
        taken in proptest::collection::vec(1u32..32, 0..16),
        first_number in 1u32..8,
    ) {
        let next = next_room_number(&taken, first_number);
        prop_assert!(next >= first_number);
        prop_assert!(!taken.iter().any(|&n| n == next && n >= first_number));
        // Filling a gap or appending: nothing between first and next is free.
        for candidate in first_number..next {
            prop_assert!(taken.contains(&candidate));
        }
        // Lower numbers and duplicates never change the answer.
        let mut padded = taken.clone();
        for noise in [0u32, 1, first_number.saturating_sub(1)] {
            if noise < first_number {
                padded.push(noise);
            }
        }
        padded.extend(taken.iter().copied());
        prop_assert_eq!(next_room_number(&padded, first_number), next);
    }
}
