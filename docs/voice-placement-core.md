# V8b placement and numbering core

`two_bot_core::voice_placement` is an original, pure implementation derived only
from [the approved voice-room specification](voice-rooms.md#v8-placement-permissions-per-creator-defaults).
It requires no `db` feature, Discord wire types, clock, store or external I/O.

## Inputs and decisions

- `next_room_number(existing_numbers, first_number) -> u32`: the lowest free
  number at or above `first_number`. Gaps are filled, never skipped, so a
  deleted room's number is reused by the next room. Numbers below the start are
  ignored, so rooms created under an older, lower start keep their numbers after
  an admin raises it. Duplicates and unsorted input are accepted. For `/group`,
  pass the union of numbers in use across every creator in the group.
- `plan_placement(request) -> Result<usize, PlacementError>`: the insertion
  index for the NEW channel within the category's channels sorted by
  `(position, id)`. Only the new channel's position is decided; existing
  channels are never moved and their relative order is unchanged.
- `resolve_initial_state(default_limit, always_private)`: the new room's
  starting user limit (`0..=99`, 0 means unlimited) and privacy flag from the
  triggering creator's `/defaultlimit` and `/alwaysprivate` defaults. Out of
  range limits are refused, never clamped. Existing rooms are unaffected when
  defaults change.

Without grouping the room lands directly above (`Above`) or below (`Below`) its
creator channel (`/position`, the V11 codec's `RoomPosition`). Because existing
rooms are not moved, the newest ungrouped room always sits next to the creator:
three rooms created in turn read `[creator, r3, r2, r1]` for `Below` and
`[r1, r2, r3, creator]` for `Above`. Keeping rooms in creation order is the
`/group` feature. With grouping and existing group rooms the room lands at the
matching edge of the group's room block, keeping the block contiguous going
forward (`[creator, r1, r2, r3]` for `Below`, `[r3, r2, r1, creator]` for
`Above`); a block split by earlier manual moves is not repaired. With
grouping but no group rooms yet, placement falls back to creator-adjacent,
starting the block there. A contiguous block under a category's 50-channel limit
is the runtime's concern: this core returns an index, not a Discord position.

## Validation boundary

The caller classifies each category channel as `Creator`, `Room` or `Other`
from authoritative guild state. The creator must be present and be a creator;
group rooms must be present and be rooms; group rooms without grouping enabled
are refused. Zero IDs, duplicate category entries and a non-creator anchor are
typed refusals, safe to surface as ephemeral command errors. The runtime still
owns slash routing, Move Members/permission checks, channel creation with
overrides included, the category capacity check, and persisting the chosen
number, position, limit and privacy.

## Residual parent work

V1 room storage and lifecycle, V5 numbering-token rendering, `/inheritpermissions`
sources, `/textchannels` companions, and all runtime wiring remain outside this
slice. The V8 parent must gather the category order, the union of in-use group
numbers, and the triggering creator's validated settings (positive first number
per the V11 codec), then create the channel at the planned index and move the
member in. Unit tests establish domain behavior only, not runtime parity or
staging readiness.

## Hermetic verification

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- clippy -p two-bot-core --all-targets -- -D warnings
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_placement
```

The acceptance fixture covers lowest-free numbering (gaps, raised starts,
unsorted/duplicated input, start above one, `u32::MAX` saturation), empty and
edge creator placement, two-creator group blocks, creator-adjacent group starts,
the pinned three-room orderings for both sides with and without grouping, every
typed refusal, limit/privacy defaults, and proptest properties proving the new
room lands beside its anchor (the creator, or the group block's outer edge with
block and new room contiguous) under shuffled input order and sparse positions,
and that the chosen number is free and stable.
No tests in this fixture use a database, Redis, Discord, or a staging identity.
