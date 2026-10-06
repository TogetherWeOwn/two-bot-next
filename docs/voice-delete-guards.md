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
Real-time lifecycle-race fixtures that predate the grace shorten it through
`VoiceRuntime::with_empty_grace`; no production path calls it, and the paused-time
guard tests pin the 60-second default.

## Legacy obligation map

The six mutation obligations come from the
[legacy guard contract](https://github.com/TogetherWeOwn/two-bot/commit/681dc291bca04c91b0e0631dea962ccf30ea5aa5).
Next uses worker fixtures, not the legacy TypeScript mutation runner.

| Legacy obligation | Worker fixture in `crates/bot/src/voice_delete_guard_tests.rs` (unless noted) |
| --- | --- |
| M1: Lobby, generator and category excluded by ID, even with claimed provenance | `protected_infrastructure_with_claimed_provenance_never_deletes`; `companion_provenance_cannot_delete_infrastructure`; `queued_delete_observes_new_protection_at_send_time` |
| M2: Never trust a caller's no-row delete request | `direct_untracked_delete_cannot_trust_queued_provenance`; `stale_loaded_provenance_never_authorizes_an_ordinary_delete` removes the durable row before reconcile and after enqueue |
| M3: Absent provenance must fail closed, not invert into a delete | Same direct/stale provenance fixtures; `unavailable_provenance_fails_closed_and_honors_backoff_or_credential_halt` pins failed reads; `untracked_live_channels_enqueue_zero_deletes` in `voice_rooms_tests.rs` pins reconcile independently |
| M4: No ordinary delete before the grace deadline | `ordinary_empty_room_survives_until_exact_grace_deadline`; `human_join_and_leave_between_ticks_restart_queued_delete_grace` |
| M5: Re-read occupancy instead of trusting an old empty marker | `delayed_delete_rechecks_grace_after_transient_human_occupancy`; `occupants_arriving_during_delete_backoff_cancel_the_write` in `voice_rooms_tests.rs` |
| M6: Re-adopt occupied tracked rooms on reconnect | `reconnect_only_prunes_tracked_empty_channels_and_counts_unknown_members_as_human` in `voice_rooms_tests.rs` covers human/unknown occupancy, bot-only cleanup, missing tracked channels and untouched untracked channels |

The existing readiness/disconnect, failed-move compensation, permission-loss,
403 suspension/no-retry-storm and SQL-only forget retry fixtures remain in the
same worker suite. Boot-ID parsing and Worker-to-Container forwarding have
separate synthetic fixtures. Ordinary deletes require both worker tracking and
an authoritative `RoomPersistence::rooms` read before deletion. If its durable
row disappeared, the worker stops managing that channel without deleting it or
its companion. Failed reads back off, and credential refusal halts writes. This
adds one room-inventory query per ordinary delete; compensation instead has the
exact successful create response. The live guard is built and evaluated after
the query (`delete_rechecks_live_grace_after_the_provenance_read`) and again at
send time. This is not a cross-process ownership lock or a transaction spanning
SQL and Discord.

## Verification and evidence boundary

Run from an isolated checkout through the bounded controller wrapper:

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot --lib voice_rooms::tests:: -- --test-threads=2
```

The targeted new suite is `voice_rooms::tests::delete_guards::`.

## Mutation receipts

The controller's bounded Cargo pool refused every local compile (`no idle,
below-budget slot`), so both mutations ran as one-off hosted `check.yml`
dispatches on throwaway probe branches cut from the tested code (`6164d9d`),
which were deleted afterwards. This is a manual receipt, not an automated CI
mutation harness. Each mutation was applied alone, compiled, and failed
assertions (not a compile error or a refused wrapper):

| Mutation | Hosted run | Failing worker fixtures (`cargo test` unit step) |
| --- | --- | --- |
| `delete_protected` always returns `false` (protected-ID and category check removed) | [run 37209631466](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/37209631466) | 5 failed, 248 passed: `boot_configured_infrastructure_is_protected_in_each_actor`, `companion_provenance_cannot_delete_infrastructure`, `newly_added_creator_cancels_a_preexisting_delete`, `queued_delete_observes_new_protection_at_send_time`, `protected_infrastructure_with_claimed_provenance_never_deletes` |
| `empty_grace_elapsed` always returns `true` (grace comparison removed) | [run 37211554475, attempt 2](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/37211554475) | 6 failed, 247 passed: `bot_only_occupancy_is_empty_but_unknown_identity_is_human`, `delayed_delete_rechecks_grace_after_transient_human_occupancy`, `delete_rechecks_live_grace_after_the_provenance_read`, `human_join_and_leave_between_ticks_restart_queued_delete_grace`, `ordinary_empty_room_survives_until_exact_grace_deadline`, `reconnect_restarts_grace_instead_of_counting_disconnected_time` |

The unmutated head ran green in the same workflow (run 37210684046, tested head
`6e52179`): 253 unit tests, the bin target, the integration step and the
ignored-database gateway reconcile fixture all passed. Two earlier probe
attempts failed for unrelated reasons (a rustup component conflict before
`cargo fmt`, and dead-code warnings under `clippy -D warnings`) and are not
counted. The first hosted unit run on the PR also caught four real-time sink
fixtures and one uncataloged log event; both are fixed above and in the PR
history.

With the protected-ID and grace checks each shown to fail assertions, and every
M1-M6 obligation mapped to a worker fixture, the legacy mutation-guard row is
promoted. Fixture evidence is not staging acceptance, independent review or
voice-cutover approval. The separate voice-receipt gate remains required.
