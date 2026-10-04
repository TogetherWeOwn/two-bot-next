# Ordinary voice-room delete guards

The worker shares delete evidence with gateway publication. Every ordinary
room delete checks authoritative readiness, tracked provenance, protected IDs,
human occupancy, a continuous 60-second human-empty grace, and current
permissions. The HTTP adapter re-evaluates the same live guard immediately before
sending, including after rate-limit waits. A human join cancels the marker even
if they leave again before the actor next reconciles. Unknown bot identity counts
as human; known bot-only occupancy still permits cleanup after the grace.

A complete reconnect snapshot starts a new grace. This deliberately takes the
safer path than the legacy boot-time immediate-empty delete. Missing channels
need no Discord delete; forgetting their row remains idempotent. Exact-result
failed-create compensation bypasses only grace, not protected IDs or humans.

## Legacy obligation map

The six mutation obligations come from the
[legacy guard contract](https://github.com/TogetherWeOwn/two-bot/commit/681dc291bca04c91b0e0631dea962ccf30ea5aa5).
Next uses worker fixtures, not the legacy TypeScript mutation runner.

| Legacy obligation | Worker fixture in `crates/bot/src/voice_delete_guard_tests.rs` (unless noted) |
| --- | --- |
| M1: Lobby, generator and category excluded by ID, even with claimed provenance | `protected_infrastructure_with_claimed_provenance_never_deletes`; `companion_provenance_cannot_delete_infrastructure`; `queued_delete_observes_new_protection_at_send_time` |
| M2: Never trust a caller's no-row delete request | `direct_untracked_delete_cannot_trust_queued_provenance` |
| M3: Absent provenance must fail closed, not invert into a delete | Same direct untracked fixture; `untracked_live_channels_enqueue_zero_deletes` in `voice_rooms_tests.rs` pins reconcile independently |
| M4: No ordinary delete before the grace deadline | `ordinary_empty_room_survives_until_exact_grace_deadline`; `human_join_and_leave_between_ticks_restart_queued_delete_grace` |
| M5: Re-read occupancy instead of trusting an old empty marker | `delayed_delete_rechecks_grace_after_transient_human_occupancy`; `occupants_arriving_during_delete_backoff_cancel_the_write` in `voice_rooms_tests.rs` |
| M6: Re-adopt occupied tracked rooms on reconnect | `reconnect_only_prunes_tracked_empty_channels_and_counts_unknown_members_as_human` in `voice_rooms_tests.rs` covers human/unknown occupancy, bot-only cleanup, missing tracked channels and untouched untracked channels |

The existing readiness/disconnect, failed-move compensation, permission-loss,
403 suspension/no-retry-storm and SQL-only forget retry fixtures remain in the
same worker suite. Boot-ID parsing and Worker-to-Container forwarding have
separate synthetic fixtures. Ordinary provenance is the worker's loaded
`voice_rooms` set; compensation has the exact successful create response. These
fixtures do not claim a per-delete database lookup or cross-process ownership.

## Verification and evidence boundary

Run from an isolated checkout through the bounded controller wrapper:

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot --lib voice_rooms::tests:: -- --test-threads=2
```

The targeted new suite is `voice_rooms::tests::delete_guards::`. A separate local
mutation receipt must show an assertion failure when removing the protected-ID
membership check and when ignoring the grace comparison, one at a time, followed
by restoration and a green original suite. A compilation error or wrapper
admission refusal is **not** a killed mutation. No automated CI mutation harness
is introduced here.

At the initial implementation checkpoint, compiling tests and mutations had not
run: the bounded pool refused admission with `no idle, below-budget slot`.
Therefore the parity row remains **carded**. Fixture locations alone are not a
passing receipt, independent review, staging waiver acceptance or voice-cutover
approval. The separate voice-receipt gate remains required.
