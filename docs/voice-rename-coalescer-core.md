# Rename-coalescer core

`two_bot_core::voice_rename_coalescer` is an original, pure implementation
derived only from [the approved voice-room specification](voice-rooms.md#discord-api-notes-all-slices).
It requires no `db` feature, Discord wire types, clock, store, timers or
external I/O.

## Backlog shape

`RenameCoalescer` keeps at most one pending name per channel ID (ascending
`BTreeMap` order), so depth is bounded at `MAX_PENDING_PER_CHANNEL` (1) per
channel by construction: no sequence of updates can deepen a channel's
backlog. There is no cap on the number of channels with pending renames, so a
backlog on one channel never refuses a rename for another.

| Limit | Value |
| --- | --- |
| `MAX_CHANNEL_NAME_CHARS` | 100 Unicode scalars |
| `MAX_PENDING_PER_CHANNEL` | 1 pending name per channel |

## Decisions

- `should_rename(current_name, desired_name) -> bool`: true exactly when the
  two names differ. Comparison is exact string inequality: case, spacing and
  normalisation forms are significant. The caller supplies the authoritative
  current name (from guild state or the last confirmed rename) and the
  freshly rendered desired name.
- `queue_rename(channel_id, desired_name)`: store the desired name,
  coalescing with any existing entry. A first update is `Queued`, a different
  second update overwrites and returns the replaced name as `Coalesced`, and
  an equal name is `Unchanged` with the map untouched. The name is stored
  exactly as given (no trimming or folding); the caller renders the final
  template output before queuing. Zero channel IDs, empty names and names
  over `MAX_CHANNEL_NAME_CHARS` scalars are typed refusals that echo no input
  and leave the map unchanged.
- `pending(channel_id)`, `contains`, `len`, `pending_slots`, `is_empty`:
  read-only views. `pending_slots` is the lifecycle slice's name for the same
  count as `len`: how many channel slots hold a pending rename.
- `take_pending(channel_id)`: remove and return the pending name when the
  runtime spends rename budget on the channel. If the send fails (for example
  a 429 the runtime must honour), the runtime re-queues the returned name.
- `observe_current(channel_id, current_name)`: drop the entry when the stored
  pending name already equals authoritative guild state (rename confirmed or
  made by hand). Never stores anything.
- `forget(channel_id)`: drop any entry for a deleted room. `clear` drops
  every entry, for example after a full reconciliation.

## Rename backlog never blocks create/delete

Creating a room never reads the map; deleting one only calls `forget`. The
runtime calls `should_rename` on each freshly rendered name, coalesces into
the backlog, and drains it only when rename budget allows, while
fast-changing information keeps flowing through the much less limited
voice-status path. Rate limiting, retry-after handling, per-guild ordering
and persistence stay on the parent runtime.

## Residual parent work

V1 room storage and lifecycle, V5 template rendering, the Discord send path
with retry-after and the ~2-per-10-minutes budget, per-guild queues and
startup reconciliation remain outside this slice. The parent must gather the
authoritative current name, render the desired name, and call
`queue_rename` / `take_pending` / `observe_current` / `forget` at the right
moments. Unit tests establish domain behaviour only, not runtime wiring or
staging readiness.

## Hermetic verification

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- clippy -p two-bot-core --all-targets -- -D warnings
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_rename_coalescer
```

The acceptance fixture covers `should_rename` identity and exactness, first
queue, second-update overwrite, identical-name no-op, the depth-1 bound over
repeated updates, per-channel independence, delete-path `forget`, queuing
under a 50-channel backlog, take/observe/clear transitions, and every typed
refusal with the map unchanged and no input echoed.
No tests in this fixture use a database, Redis, Discord, or a staging identity.
