//! Hermetic V8b acceptance cases against the public placement core API.

use proptest::prelude::*;
use two_bot_core::voice_config::RoomPosition;
use two_bot_core::voice_placement::{
    next_room_number, plan_placement, position_for_index, resolve_initial_state, CategoryChannel,
    CategoryEntryKind, PlacementError, PlacementRequest, RoomInitialState, MAX_ROOM_USER_LIMIT,
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
    side: RoomPosition,
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
        plan_placement(request(10, RoomPosition::Above, false, &[], &category)),
        Ok(0)
    );
    assert_eq!(
        plan_placement(request(10, RoomPosition::Below, false, &[], &category)),
        Ok(1)
    );
}

#[test]
fn creator_at_top_places_without_moving_others() {
    let category = [creator(10, 0), other(20, 1), room(30, 2)];
    assert_eq!(
        plan_placement(request(10, RoomPosition::Above, false, &[], &category)),
        Ok(0)
    );
    assert_eq!(
        plan_placement(request(10, RoomPosition::Below, false, &[], &category)),
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
        plan_placement(request(10, RoomPosition::Above, false, &[], &category)),
        Ok(2)
    );
    assert_eq!(
        plan_placement(request(10, RoomPosition::Below, false, &[], &category)),
        Ok(3)
    );
}

#[test]
fn placement_uses_display_order_not_input_order() {
    // Input in any order; positions may be sparse and out of sequence.
    let category = [other(20, 100), creator(10, 0), room(30, 50)];
    assert_eq!(display_order(&category), vec![10, 30, 20]);
    assert_eq!(
        plan_placement(request(10, RoomPosition::Below, false, &[], &category)),
        Ok(1)
    );
    assert_eq!(
        plan_placement(request(10, RoomPosition::Above, false, &[], &category)),
        Ok(0)
    );
}

#[test]
fn position_ties_break_by_channel_id() {
    let category = [creator(30, 0), other(10, 0), other(20, 0)];
    assert_eq!(display_order(&category), vec![10, 20, 30]);
    assert_eq!(
        plan_placement(request(30, RoomPosition::Above, false, &[], &category)),
        Ok(2)
    );
    assert_eq!(
        plan_placement(request(30, RoomPosition::Below, false, &[], &category)),
        Ok(3)
    );
}

// ---- plan_placement: grouped ----

#[test]
fn group_with_no_rooms_yet_starts_beside_the_creator() {
    let category = [creator(10, 0), creator(20, 1), other(30, 2)];
    assert_eq!(
        plan_placement(request(10, RoomPosition::Below, true, &[], &category)),
        Ok(1)
    );
    assert_eq!(
        plan_placement(request(10, RoomPosition::Above, true, &[], &category)),
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
        plan_placement(request(10, RoomPosition::Below, true, &group, &category)),
        Ok(4)
    );
    // Above: before the first group room.
    assert_eq!(
        plan_placement(request(20, RoomPosition::Above, true, &group, &category)),
        Ok(1)
    );
    // Triggering creator does not matter, only the side and the group set.
    assert_eq!(
        plan_placement(request(20, RoomPosition::Below, true, &group, &category)),
        Ok(4)
    );
    assert_eq!(
        plan_placement(request(10, RoomPosition::Above, true, &group, &category)),
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
        plan_placement(request(10, RoomPosition::Below, true, &group, &category)),
        Ok(5)
    );
    assert_eq!(
        plan_placement(request(20, RoomPosition::Above, true, &group, &category)),
        Ok(2)
    );
}

#[test]
fn group_room_duplicates_are_deduplicated() {
    let category = [creator(10, 0), room(11, 1), other(30, 2)];
    let group = [11, 11, 11];
    assert_eq!(
        plan_placement(request(10, RoomPosition::Below, true, &group, &category)),
        Ok(2)
    );
    assert_eq!(
        plan_placement(request(10, RoomPosition::Above, true, &group, &category)),
        Ok(1)
    );
}

// ---- plan_placement: successive rooms ----

/// Creates `count` rooms one after another from creator 10, inserting each at
/// its planned index and renumbering positions densely as Discord would, and
/// returns the final display order. Rooms are 101, 102, ... in creation order.
fn create_rooms_in_sequence(side: RoomPosition, grouped: bool, count: u64) -> Vec<u64> {
    let mut category = vec![creator(10, 0)];
    let mut rooms: Vec<u64> = Vec::new();
    for new_id in (1..=count).map(|n| 100 + n) {
        let group: &[u64] = if grouped { &rooms } else { &[] };
        let index = plan_placement(request(10, side, grouped, group, &category))
            .expect("room plans in a valid category");
        category = with_insertion(&category, index, new_id)
            .into_iter()
            .enumerate()
            .map(|(position, id)| {
                let position = i32::try_from(position).expect("small category");
                if id == 10 {
                    creator(id, position)
                } else {
                    room(id, position)
                }
            })
            .collect();
        rooms.push(new_id);
    }
    display_order(&category)
}

#[test]
fn ungrouped_rooms_keep_the_newest_next_to_the_creator() {
    // Existing rooms are not moved, so each new room takes the slot beside
    // the creator and pushes older rooms outward.
    assert_eq!(
        create_rooms_in_sequence(RoomPosition::Below, false, 3),
        vec![10, 103, 102, 101]
    );
    assert_eq!(
        create_rooms_in_sequence(RoomPosition::Above, false, 3),
        vec![101, 102, 103, 10]
    );
}

#[test]
fn grouped_rooms_extend_the_block_in_creation_order() {
    // `/group` keeps one contiguous block and appends at its outer edge.
    assert_eq!(
        create_rooms_in_sequence(RoomPosition::Below, true, 3),
        vec![10, 101, 102, 103]
    );
    assert_eq!(
        create_rooms_in_sequence(RoomPosition::Above, true, 3),
        vec![103, 102, 101, 10]
    );
}

// ---- plan_placement: refusals ----

#[test]
fn placement_refusals_name_the_bad_entry() {
    let category = [creator(10, 0), room(11, 1), other(30, 2)];
    assert_eq!(
        plan_placement(request(99, RoomPosition::Below, false, &[], &category)),
        Err(PlacementError::UnknownCreator(99))
    );
    assert_eq!(
        plan_placement(request(11, RoomPosition::Below, false, &[], &category)),
        Err(PlacementError::NotACreator(11))
    );
    assert_eq!(
        plan_placement(request(30, RoomPosition::Below, false, &[], &category)),
        Err(PlacementError::NotACreator(30))
    );
    assert_eq!(
        plan_placement(request(0, RoomPosition::Below, false, &[], &category)),
        Err(PlacementError::InvalidChannelId)
    );
    assert_eq!(
        plan_placement(request(10, RoomPosition::Below, false, &[11], &category)),
        Err(PlacementError::UnexpectedGroupRooms)
    );
    assert_eq!(
        plan_placement(request(10, RoomPosition::Below, true, &[99], &category)),
        Err(PlacementError::UnknownGroupRoom(99))
    );
    assert_eq!(
        plan_placement(request(10, RoomPosition::Below, true, &[10], &category)),
        Err(PlacementError::GroupEntryNotRoom(10))
    );
    assert_eq!(
        plan_placement(request(10, RoomPosition::Below, true, &[30], &category)),
        Err(PlacementError::GroupEntryNotRoom(30))
    );
    assert_eq!(
        plan_placement(request(10, RoomPosition::Below, true, &[0], &category)),
        Err(PlacementError::InvalidChannelId)
    );
    let duplicate = [creator(10, 0), room(11, 1), room(11, 2)];
    assert_eq!(
        plan_placement(request(10, RoomPosition::Below, false, &[], &duplicate)),
        Err(PlacementError::DuplicateChannel(11))
    );
    let zero = [creator(10, 0), room(0, 1)];
    assert_eq!(
        plan_placement(request(10, RoomPosition::Below, false, &[], &zero)),
        Err(PlacementError::InvalidChannelId)
    );
    let empty: [CategoryChannel; 0] = [];
    assert_eq!(
        plan_placement(request(10, RoomPosition::Below, false, &[], &empty)),
        Err(PlacementError::UnknownCreator(10))
    );
}

#[test]
fn placement_refusals_leave_input_unchanged() {
    let category = [creator(10, 0), room(11, 1)];
    let before = category;
    assert!(plan_placement(request(99, RoomPosition::Below, false, &[], &category)).is_err());
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

// ---- property: adjacency ----

/// Stand-in ID for the new room; synthetic channel IDs stay far below it.
const NEW_ROOM: u64 = u64::MAX;

/// A synthetic category in display order: `lead` unrelated channels, then the
/// creator, a gap of `gap` unrelated channels and a contiguous block of
/// `block` rooms (or the block, the gap and the creator when `block_first`),
/// then `tail` unrelated channels.
#[derive(Debug, Clone)]
struct AdjacencyCase {
    /// Every channel, in shuffled input order with sparse positions.
    category: Vec<CategoryChannel>,
    creator_id: u64,
    /// The room block in display order.
    block: Vec<u64>,
}

fn adjacency_case() -> impl Strategy<Value = AdjacencyCase> {
    (0usize..4, 0usize..3, 0usize..4, 0usize..4, any::<bool>()).prop_flat_map(
        |(lead, gap, block, tail, block_first)| {
            let len = lead + 1 + gap + block + tail;
            (
                proptest::collection::btree_set(1u64..1_000_000, len)
                    .prop_map(|ids| ids.into_iter().collect::<Vec<_>>())
                    .prop_shuffle(),
                proptest::collection::vec(1i32..6, len),
                -40i32..40,
            )
                .prop_flat_map(move |(ids, steps, start)| {
                    let mut kinds = vec![CategoryEntryKind::Other; lead];
                    let mut middle = vec![CategoryEntryKind::Creator];
                    middle.extend(vec![CategoryEntryKind::Other; gap]);
                    middle.extend(vec![CategoryEntryKind::Room; block]);
                    if block_first {
                        middle.reverse();
                    }
                    kinds.extend(middle);
                    kinds.extend(vec![CategoryEntryKind::Other; tail]);
                    // Strictly increasing but sparse positions: the layout
                    // above is the display order regardless of channel IDs.
                    let mut position = start;
                    let mut category = Vec::with_capacity(len);
                    for ((&id, &kind), &step) in ids.iter().zip(&kinds).zip(&steps) {
                        position += step;
                        category.push(CategoryChannel { id, position, kind });
                    }
                    let creator_id = category
                        .iter()
                        .find(|entry| entry.kind == CategoryEntryKind::Creator)
                        .map(|entry| entry.id)
                        .expect("layout has one creator");
                    let block: Vec<u64> = category
                        .iter()
                        .filter(|entry| entry.kind == CategoryEntryKind::Room)
                        .map(|entry| entry.id)
                        .collect();
                    Just(category)
                        .prop_shuffle()
                        .prop_map(move |category| AdjacencyCase {
                            category,
                            creator_id,
                            block: block.clone(),
                        })
                })
        },
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// The new room lands beside its anchor: the outer edge of the group's
    /// room block when grouped with rooms (keeping block plus new room
    /// contiguous), otherwise the creator channel itself.
    #[test]
    fn property_new_room_is_adjacent_to_its_anchor(
        case in adjacency_case(),
        above in any::<bool>(),
        grouped in any::<bool>(),
    ) {
        let side = if above { RoomPosition::Above } else { RoomPosition::Below };
        let group_room_ids: &[u64] = if grouped { &case.block } else { &[] };
        let index = plan_placement(request(
            case.creator_id,
            side,
            grouped,
            group_room_ids,
            &case.category,
        ))
        .expect("synthetic category must plan");
        prop_assert!(index <= case.category.len());
        let after = with_insertion(&case.category, index, NEW_ROOM);
        let neighbour = match side {
            RoomPosition::Above => after.get(index + 1),
            RoomPosition::Below => index.checked_sub(1).and_then(|before| after.get(before)),
        };
        if grouped && !case.block.is_empty() {
            let edge = match side {
                RoomPosition::Above => case.block.first(),
                RoomPosition::Below => case.block.last(),
            };
            prop_assert_eq!(neighbour, edge);
            // Slots are collected in display order, so first and last bound
            // the block; no outsider fits between them when the span matches.
            let slots: Vec<usize> = after
                .iter()
                .enumerate()
                .filter(|(_, id)| **id == NEW_ROOM || case.block.contains(id))
                .map(|(slot, _)| slot)
                .collect();
            prop_assert_eq!(slots.len(), case.block.len() + 1);
            prop_assert_eq!(slots[slots.len() - 1] - slots[0], case.block.len());
        } else {
            prop_assert_eq!(neighbour, Some(&case.creator_id));
        }
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

#[test]
fn position_for_index_takes_the_slot_of_the_channel_it_displaces() {
    // Input order is irrelevant: the slot comes from (position, id) order.
    let order = [other(9, 7), creator(5, 2), room(6, 4)];
    assert_eq!(position_for_index(&order, 0), 2);
    assert_eq!(position_for_index(&order, 1), 4);
    assert_eq!(position_for_index(&order, 2), 7);
}

#[test]
fn position_for_index_appends_one_past_the_last_channel() {
    let order = [creator(5, 0), room(6, 3)];
    assert_eq!(position_for_index(&order, 2), 4);
    // Past-the-end indexes behave like appending; an empty category starts at 0.
    assert_eq!(position_for_index(&order, 9), 4);
    assert_eq!(position_for_index(&[], 0), 0);
}

#[test]
fn position_for_index_never_goes_negative() {
    let order = [creator(5, -3), room(6, -1)];
    assert_eq!(position_for_index(&order, 0), 0);
    // Appending still lands one past the clamped last position.
    assert_eq!(position_for_index(&order, 2), 1);
}
