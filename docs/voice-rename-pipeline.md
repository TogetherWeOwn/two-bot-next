# Rename pipeline: render to coalescer

The naming engine (`two_bot_core::voice_naming`) and the rename coalescer
(`two_bot_core::voice_rename_coalescer`) are tested separately in
`voice_naming_wiring.rs` (render rows) and `voice_rename_coalescer.rs`
(backlog rows). This slice pins the seam between them, derived only from
[the voice-room specification](voice-rooms.md#v5-naming-template-engine-core-library)
(V5/V10 renames) and the [Discord API notes](voice-rooms.md#discord-api-notes-all-slices).

## Call path

The room-lifecycle runtime (V1) does not exist yet. Its future rename path,
modelled by every case in `voice_rename_pipeline.rs`, is:

1. Re-render the room name with `resolve_room_name` on each membership change.
2. Compare against the authoritative current name with `should_rename`.
3. On difference, coalesce with `queue_rename` (one pending name per channel).
4. Drain only when rename budget (~2 per 10 minutes per channel) allows, via
   `take_pending`, and reconcile with `observe_current` as guild state
   confirms the rename.

Creating a room never reads the backlog; the tests drive only the public
core APIs listed above.

## Cases

- Differing re-render queues exactly one pending name; an identical
  re-render queues nothing (`should_rename` identity at the seam).
- A burst of renders before delivery collapses to the latest name only;
  depth stays bounded at `MAX_PENDING_PER_CHANNEL` (1) per channel.
- Seeded random picks (`@@random_emoji@@`, `[[a/b/c]]`) stay stable across
  re-renders through the queue: membership change updates the counts
  without re-rolling the emoji/list prefix.
- Delivery plus `observe_current` sync clears the slot; a stale observation
  keeps a still-needed rename; channels stay independent end to end.

## Non-goals

Isolated engine rows (golden corpus, parser properties) and isolated
coalescer rows (refusals, `forget`, `clear`, backlog shape) are covered by
the sibling fixtures and are not re-asserted here.

## Hermetic verification

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_rename_pipeline
```

No test in this fixture uses a database, Redis, Discord, or a staging identity.
