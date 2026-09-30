# V2 ownership core integration seam

`two_bot_core::voice_ownership` is an original, pure implementation derived only
from [the approved voice-room specification](voice-rooms.md#v2-ownership).
It requires no `db` feature, Discord wire types, clock, or external I/O.

## Inputs and decisions

- `RoomOwnership`: current owner and remembered original creator, both nonzero
  member IDs. Either may be absent from the room. An absent owner is a legitimate
  reconciliation input, not a corrupt state.
- `RoomMember`: one row per current occupant, including bots, with the start of
  their **current continuous stay** in comparable milliseconds. Leave/rejoin must
  reset this time. The caller supplies a complete snapshot for exactly one room.
- `RoomActor`: command member and effective Manage Channels permission for the
  room. An admin may act from outside the room. Non-admin actors must be current
  human occupants. The runtime must authenticate actor identity/permission and
  validate room/guild routing; these are supplied facts, not computed here.
- `decide_ownership`: returns `Unchanged`, `Changed { previous, next, reason }`,
  `EmptyRoom`, or a typed refusal. It never mutates the supplied state.
- `require_room_owner`: the same owner/admin authorization gate used by transfer,
  exposed for future owner-only controls. The runtime still enforces lifecycle
  and any guild/command restrictions outside this V2 slice.

`Reconcile` keeps a present owner. Otherwise it selects the earliest-joined human
as caretaker without replacing the remembered creator. Equal join times use
ascending member ID, an explicit deterministic tie-break for the spec's otherwise
unspecified tie. No humans (including bot-only rooms) returns `EmptyRoom` for V1
cleanup; no deletion is performed here.

`Reclaim` requires a current human occupant who is the original creator **or**
whose room owner is absent. It changes only the owner. The spec's explicit reclaim
eligibility rule is used even if the claimant happens to be an admin; reclaim is
not an owner-only command. Returning creators are not promoted automatically by
reconciliation: they must reclaim.

`Transfer` requires the owner or an admin and a current human recipient. It changes
both owner and original creator. A self-transfer by a caretaker also changes the
remembered creator; a self-transfer by an owner who is already creator is unchanged.
Bots cannot own, reclaim, or receive transfers. Duplicate members, zero IDs, or a
snapshot identifying the owner/creator as a bot are invalid and never silently
repaired. Commands cannot revive an empty room.

## Replay and concurrency contract

The same inputs always produce the same decision. Reconciliation after applying
caretaker succession is unchanged. A repeated creator reclaim is unchanged. A
repeated transfer is unchanged **only if the current actor is still authorized**;
a former owner cannot replay it to bypass authorization. Similarly, a noncreator
claimant cannot claim again while the new owner (themself) is present. Refusals
leave state unchanged.

The runtime must serialize per-room membership and commands, persist `Changed`
atomically against the supplied `previous` state, and deduplicate interaction/event
IDs. The comparison must cover membership too (or use a room revision/transaction),
not just the owner fields: a recipient can leave without ownership changing.
Stale snapshots and delayed event replays are **not** detected by this pure core.
No exactly-once Discord effects or crash recovery are claimed by these tests.

## Residual parent work

V1 room storage, membership/join-time tracking, startup reconciliation, empty-room
cleanup and runtime wiring remain outside this slice. The V2 parent must route
`/transfer` and `/reclaim`, authenticate permissions, persist both ownership fields,
translate typed refusals into ephemeral replies, deduplicate/serialize writes, and
execute any ownership-dependent permission, private Join-channel, and name updates.
Privacy and other room settings are not part of this component and must be preserved.
Unit tests establish domain behavior only, not runtime parity or staging readiness.

## Hermetic verification

```sh
cargo fmt --all -- --check
cargo clippy -p two-bot-core --all-targets --locked -- -D warnings
cargo test -p two-bot-core --test voice_ownership --locked
```

The acceptance fixture covers transfer authorization/recipients, departure and
caretaker ordering, reclaim, replay, empty/bot-only rooms, and invalid snapshots.
No tests in this fixture use a database, Redis, Discord, or a staging identity.
