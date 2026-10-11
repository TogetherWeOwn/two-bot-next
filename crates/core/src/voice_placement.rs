//! Pure V8b placement and numbering decisions, written from `docs/voice-rooms.md`
//! §V8 only.
//!
//! The caller supplies the category's current channel order, the triggering
//! creator's `/position` side and `/group` membership, the room numbers already
//! in use, and the creator's `/defaultlimit` and `/alwaysprivate` defaults.
//! This module performs no I/O, holds no Discord, store or clock types, and
//! never moves, renames, creates or deletes channels: it returns where the NEW
//! room goes and which number and initial state it takes. Creating the channel
//! at the index, enforcing the 50-channels-per-category limit, inheriting
//! permissions, and persisting settings belong to the parent runtime.

use std::collections::{BTreeMap, BTreeSet};

use crate::voice_config::RoomPosition;
use crate::Snowflake;

/// Highest user limit a new room may start with (`/limit` and `/defaultlimit`,
/// 0 means unlimited).
pub const MAX_ROOM_USER_LIMIT: u16 = 99;

/// Role of one channel in the category's current order, as classified by the
/// caller from authoritative guild state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CategoryEntryKind {
    /// A creator channel; rooms are never created from the other kinds.
    Creator,
    /// A temporary room previously created by a creator in the group.
    Room,
    /// Anything else (text channels, permanent voice, the category itself is
    /// not listed): never moved, never counted as a group room.
    Other,
}

/// One channel in the category, in any order. Only relative `position` order
/// matters; raw Discord positions need not be dense or start at zero. Ties
/// break by ascending channel ID so the order is deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CategoryChannel {
    pub id: Snowflake,
    pub position: i32,
    pub kind: CategoryEntryKind,
}

/// Placement input for exactly one new room.
#[derive(Debug, Clone, Copy)]
pub struct PlacementRequest<'a> {
    /// Creator channel the join (or `/create`) triggered on.
    pub creator_id: Snowflake,
    /// That creator's `/position` setting, as stored by the V11 codec: the new
    /// room goes directly above or directly below an anchor. Without grouping
    /// the anchor is the creator channel itself; with `/group` and existing
    /// group rooms the anchor is the corresponding edge of the group's room
    /// block, so the block stays contiguous without moving any existing
    /// channel.
    pub side: RoomPosition,
    /// That creator's `/group` (shared numbering and contiguous block) flag.
    pub grouped: bool,
    /// Existing rooms in the same numbering/placement group. With grouping,
    /// pass the rooms of every creator in the group, not just this creator's.
    /// Must be empty when `grouped` is false.
    pub group_room_ids: &'a [Snowflake],
    /// Every channel currently in the category, in any order.
    pub category_order: &'a [CategoryChannel],
}

/// Initial room state from per-creator defaults. Applies to new rooms only;
/// existing rooms keep their limit and privacy when defaults change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoomInitialState {
    /// Starting user limit, 0 means unlimited.
    pub user_limit: u8,
    pub private: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PlacementError {
    #[error("channel id must be nonzero")]
    InvalidChannelId,
    #[error("creator channel {0} is not in the category order")]
    UnknownCreator(Snowflake),
    #[error("channel {0} is not a creator channel")]
    NotACreator(Snowflake),
    #[error("channel {0} appears more than once in the category order")]
    DuplicateChannel(Snowflake),
    #[error("group room {0} is not in the category order")]
    UnknownGroupRoom(Snowflake),
    #[error("group entry {0} is not a room")]
    GroupEntryNotRoom(Snowflake),
    #[error("group rooms were supplied without grouping enabled")]
    UnexpectedGroupRooms,
    #[error("default limit {0} is outside 0..=99")]
    LimitOutOfRange(u16),
}

/// Lowest free room number at or above `first_number`.
///
/// Gaps are filled, never skipped: with existing numbers `[1, 2, 4]` and
/// `first_number` 1 the answer is 3, so a deleted room's number is reused by
/// the next room. Numbers below `first_number` are ignored, so rooms created
/// under an older, lower start keep their numbers after an admin raises it.
/// Duplicates and unsorted input are accepted. For `/group`, pass the union of
/// numbers in use across every creator in the group; this function is agnostic
/// to creators and needs no other grouping input.
///
/// `first_number` is the creator's validated setting (positive, see the V11
/// codec); the function itself treats any `u32` as a floor. Saturation at
/// `u32::MAX` is a defensive bound for an unreachable full range, not a free
/// number.
#[must_use]
pub fn next_room_number(existing_numbers: &[u32], first_number: u32) -> u32 {
    let mut taken: Vec<u32> = existing_numbers
        .iter()
        .copied()
        .filter(|&n| n >= first_number)
        .collect();
    taken.sort_unstable();
    let mut candidate = first_number;
    for number in taken {
        if number == candidate {
            candidate = match candidate.checked_add(1) {
                Some(next) => next,
                None => return u32::MAX,
            };
        } else if number > candidate {
            break;
        }
    }
    candidate
}

/// Insertion index for the new channel within the category's channels sorted
/// by `(position, id)`: inserting there leaves every existing channel's
/// relative order unchanged, so existing rooms are never moved. The result is
/// always in `0..=category_order.len()`.
///
/// Without grouping the new room lands directly above (`Above`) or below
/// (`Below`) its creator. Because existing rooms are not moved, the newest
/// ungrouped room always sits next to the creator: three rooms created one
/// after another read `[creator, r3, r2, r1]` for `Below` and
/// `[r1, r2, r3, creator]` for `Above`. Keeping rooms in creation order as a
/// contiguous block is the `/group` feature.
///
/// With grouping and existing group rooms the new room lands at the
/// corresponding edge of the group's room block (before the first group room
/// for `Above`, after the last for `Below`), keeping the block contiguous
/// going forward: three grouped rooms read `[creator, r1, r2, r3]` for `Below`
/// and `[r3, r2, r1, creator]` for `Above`. A block split by earlier moves is
/// not repaired: only the new channel's index is returned, never a reorder
/// plan. With grouping but no group rooms yet, placement falls back to
/// creator-adjacent, starting the block there. The runtime maps the index to a
/// concrete Discord position.
pub fn plan_placement(request: PlacementRequest<'_>) -> Result<usize, PlacementError> {
    let order = request.category_order;
    let mut seen = BTreeSet::new();
    for entry in order {
        if entry.id == 0 {
            return Err(PlacementError::InvalidChannelId);
        }
        if !seen.insert(entry.id) {
            return Err(PlacementError::DuplicateChannel(entry.id));
        }
    }
    if request.creator_id == 0 {
        return Err(PlacementError::InvalidChannelId);
    }
    let creator_index = order
        .iter()
        .position(|entry| entry.id == request.creator_id)
        .ok_or(PlacementError::UnknownCreator(request.creator_id))?;
    if order[creator_index].kind != CategoryEntryKind::Creator {
        return Err(PlacementError::NotACreator(request.creator_id));
    }
    if !request.grouped && !request.group_room_ids.is_empty() {
        return Err(PlacementError::UnexpectedGroupRooms);
    }
    // Display order is the rank of each input index in (position, id) order.
    let mut sorted: Vec<usize> = (0..order.len()).collect();
    sorted.sort_by_key(|&index| (order[index].position, order[index].id));
    let mut rank = vec![0usize; order.len()];
    for (ranked, &index) in sorted.iter().enumerate() {
        rank[index] = ranked;
    }
    let by_id: BTreeMap<Snowflake, usize> = order
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.id, index))
        .collect();
    let mut group = BTreeSet::new();
    for &id in request.group_room_ids {
        if id == 0 {
            return Err(PlacementError::InvalidChannelId);
        }
        if !group.insert(id) {
            continue;
        }
        let index = by_id
            .get(&id)
            .copied()
            .ok_or(PlacementError::UnknownGroupRoom(id))?;
        if order[index].kind != CategoryEntryKind::Room {
            return Err(PlacementError::GroupEntryNotRoom(id));
        }
    }

    if request.grouped && !group.is_empty() {
        let mut first = usize::MAX;
        let mut last = 0usize;
        for id in &group {
            let index = by_id
                .get(id)
                .copied()
                .ok_or(PlacementError::UnknownGroupRoom(*id))?;
            first = first.min(rank[index]);
            last = last.max(rank[index]);
        }
        return Ok(match request.side {
            RoomPosition::Above => first,
            RoomPosition::Below => last + 1,
        });
    }
    Ok(match request.side {
        RoomPosition::Above => rank[creator_index],
        RoomPosition::Below => rank[creator_index] + 1,
    })
}

/// Discord `position` to create the new channel with so it takes the slot at
/// `index` (as returned by [`plan_placement`]) among the category's channels.
///
/// The new channel asks for the slot of the channel currently at `index`, so
/// Discord inserts it there and pushes that channel and everything after it
/// down; no existing channel is patched. Appending (`index` at or past the
/// end) asks for one past the last channel's position. Raw positions may be
/// sparse or negative-free; ties break by ascending ID like `plan_placement`.
/// This is the single place that encodes the assumed create-time position
/// semantics, so a staging observation that disagrees changes only this
/// function.
#[must_use]
pub fn position_for_index(category_order: &[CategoryChannel], index: usize) -> u64 {
    let mut sorted: Vec<&CategoryChannel> = category_order.iter().collect();
    sorted.sort_by_key(|entry| (entry.position, entry.id));
    let clamp = |position: i32| u64::try_from(position).unwrap_or(0);
    match sorted.get(index) {
        Some(entry) => clamp(entry.position),
        None => sorted
            .last()
            .map_or(0, |last| clamp(last.position).saturating_add(1)),
    }
}

/// Gap left between channels by a re-space, so later creates find a free
/// position without another reorder (Auto-Voice uses the same step).
pub const POSITION_STEP: u64 = 16;

/// Where a new room is created so it lands at `index` of the category order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateSlot {
    /// Discord `position` for the create call. No other channel holds it once
    /// `respace` (if any) is applied.
    pub position: u64,
    /// Bulk reorder to apply before the create when no free position exists
    /// at `index`: every listed channel with its new position. Empty when the
    /// slot is free already.
    pub respace: Vec<(Snowflake, u64)>,
    /// Position to fall back to when the reorder is refused: the channel
    /// above the slot, so the tie (broken by the newer, larger id) still
    /// renders the room on the correct side of it. `None` when nothing is
    /// above the slot: a tie there would sort the room below its neighbour,
    /// so the room is created without a position instead.
    pub fallback: Option<u64>,
}

/// The create position for slot `index` (as returned by [`plan_placement`]).
///
/// Discord honours a create-time position exactly and shifts no other
/// channel, and a shared position renders in no reliable order. So the room
/// takes the first free integer below the channel above the slot when there
/// is one; otherwise the category is re-spaced at [`POSITION_STEP`] intervals
/// with a gap opened at `index`, in one bulk reorder before the create.
#[must_use]
pub fn create_slot(category_order: &[CategoryChannel], index: usize) -> CreateSlot {
    let mut sorted: Vec<&CategoryChannel> = category_order.iter().collect();
    sorted.sort_by_key(|entry| (entry.position, entry.id));
    let lower = index
        .checked_sub(1)
        .and_then(|above| sorted.get(above))
        .map_or(-1, |entry| i64::from(entry.position));
    let clamp = |position: i64| u64::try_from(position).unwrap_or(0);
    let Some(upper) = sorted.get(index).map(|entry| i64::from(entry.position)) else {
        let position = clamp((lower + 1).max(0));
        return CreateSlot {
            position,
            respace: Vec::new(),
            fallback: Some(position),
        };
    };
    if upper - lower > 1 {
        let position = clamp(lower + 1);
        return CreateSlot {
            position,
            respace: Vec::new(),
            fallback: Some(position),
        };
    }
    let respace = sorted
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let rank = if i < index { i + 1 } else { i + 2 };
            (entry.id, rank as u64 * POSITION_STEP)
        })
        .collect();
    CreateSlot {
        position: (index as u64 + 1) * POSITION_STEP,
        respace,
        fallback: (lower >= 0).then(|| clamp(lower)),
    }
}

/// Resolve a new room's starting limit and privacy from its creator's
/// `/defaultlimit` and `/alwaysprivate` defaults. Only the validated range
/// `0..=99` is accepted (0 means unlimited); anything else is refused rather
/// than clamped, so a misconfigured default surfaces instead of silently
/// changing room behaviour.
pub fn resolve_initial_state(
    default_limit: u16,
    always_private: bool,
) -> Result<RoomInitialState, PlacementError> {
    if default_limit > MAX_ROOM_USER_LIMIT {
        return Err(PlacementError::LimitOutOfRange(default_limit));
    }
    Ok(RoomInitialState {
        user_limit: u8::try_from(default_limit)
            .map_err(|_| PlacementError::LimitOutOfRange(default_limit))?,
        private: always_private,
    })
}
