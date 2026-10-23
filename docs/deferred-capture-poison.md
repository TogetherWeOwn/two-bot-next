# Deferred capture poison contract

A poisoned `DeferredLeveling` or `DeferredCommunityFacts` mutex is a
supervised failure, never a panic or a silent success. A previous holder
panicked while holding the buffer lock, so the buffered entries are
untrusted. The buffers never heal in place: no `into_inner`,
`clear_poison`, default replacement or retry.

## Reason codes

| Buffer | Reason | Next action |
|--------|--------|-------------|
| DeferredLeveling | `deferred_leveling_poisoned` | `supervisor_restart_inspection` |
| DeferredCommunityFacts | `deferred_community_facts_poisoned` | `supervisor_restart_inspection` |

The typed errors (`LevelingRuntimeError::LevelingPoisoned`,
`LevelingRuntimeError::CommunityFactsPoisoned`,
`CommunityFactsError::Poisoned`) carry the reason and the action in a
bounded Display (<= 256 bytes). The Display holds no panic payload, SQL
or connection detail, member or message content, and no per-member
labels. A poison refusal is distinct from an empty batch, a disabled
sink (`Ok(0)`), and a store or Discord failure.

## Gateway behavior

- Funnel collection refuses typed on either poisoned buffer; the worker
  logs the sanitized refusal and holds the checkpoint cursor.
- The leveling drain refuses before any store or Discord work, even for
  an empty batch.
- The facts drain refuses before any store work and keeps every family
  queued: never a partial batch, never a false zero. Unlike an ordinary
  write failure, poison holds the cursor — warn-and-continue would recur
  forever on a buffer that never heals.
- A held cursor stops the dispatch worker and surfaces through the
  existing `gateway_checkpoint_failures` rule (`stage="pre_commit"` in
  `docs/gateway-failure-classification.md`); the supervisor restarts and
  the fresh process owns fresh buffers.

## Offline fault check

```sh
PAPERCLIP_RUN_SCRATCH_DIR="$PWD/.scratch" \
  python3 crates/discord/tests/run_deferred_capture_poison.py
```

The runner copies the workspace into run-owned scratch, registers the
protected suite only there if the source lacks it, and runs the focused
library suite through the bounded cache wrapper. The ordinary
`db`-feature library CI lane runs the same registered module directly.
