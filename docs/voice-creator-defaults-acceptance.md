# V8 per-creator defaults acceptance (tests-only)

`crates/core/tests/voice_creator_defaults.rs` pins the V8 per-creator
defaults wiring contract against the existing public API only, derived from
[the voice-room specification](voice-rooms.md#v8-placement-permissions-per-creator-defaults)
with the placement and permission cores
([placement](voice-placement-core.md),
[permissions](voice-permissions-core.md)) already merged.

## Contract

Each creator channel carries its own validated settings (`first_number`,
`/position`, `/group`, `/defaultlimit`, `/alwaysprivate`, channel bitrate).
The runtime routes the *triggering* creator's settings into the pure cores;
every case below calls those cores with one creator's fields at a time and
requires no `db` feature, Discord wire types, clock, store, or external I/O.

1. **Numbering starts from the triggering creator's `first_number`:**
   `next_room_number(&[5, 6, 8], 5)` is `7` (gap filled, never skipped),
   numbers below the start are ignored after an admin raises it, an empty
   room list starts exactly at the creator's first number, and two creators
   with different starts number independently from the same taken set.
2. **Placement uses the triggering creator's category/position:**
   `plan_placement` with the creator's own `/position` lands adjacent to
   its channel without moving existing channels; swapping configs swaps
   the answers, so the side follows the creator, not the channel id. The
   `/group` flag likewise comes from the triggering creator: a grouped
   creator extends the shared block edge while an ungrouped creator in the
   same category stays adjacent to its own channel.
3. **Initial state applies creator limit/privacy/bitrate within tier max:**
   `resolve_initial_state(default_limit, always_private)` passes the
   creator's values through (`4/private`, `0/public`), accepts `99`, and
   refuses `100` instead of clamping. `validate_bitrate_preference` accepts
   the creator bitrate inside the tier maximum and refuses `<= 8000` and
   `> tier_max`; with no member preferences `room_bitrate` falls back to
   the creator default, averages member preferences with floor division
   otherwise, and always clamps the result into `[8001, tier_max]`.
4. **A creator without defaults falls back to guild defaults:**
   a neutral creator (`first_number 1`, limit `0`, public, `Below`,
   ungrouped, guild bitrate) resolves identically to the guild defaults
   through the same pure calls for numbering, limits, bitrate fallback,
   and placement.
5. **Per-creator defaults never leak across creators:** interleaved calls
   for creators A and B return each creator's own limit/privacy, number,
   bitrate fallback, and placement anchor, and re-reading A after B is
   unchanged. A V11 codec export/import round-trip of a two-creator
   configuration keeps every per-creator field separate, and the decoded
   values still resolve independently.

## Hermetic verification

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- clippy -p two-bot-core --all-targets -- -D warnings
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_creator_defaults
```

No test in this fixture uses a database, Redis, Discord, or a staging
identity. Unit tests establish domain behavior only, not runtime parity or
staging readiness; the V8 parent still owns slash routing, authoritative
fact gathering, permission gates, persistence, channel creation with
overrides included, and the Join-channel flow.
