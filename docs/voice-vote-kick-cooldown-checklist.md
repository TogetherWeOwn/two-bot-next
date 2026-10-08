# Vote-kick cooldown and spam-guard staging-readiness checklist

Staging-readiness leaf for V4 vote-kick cooldown behaviour. Pins the windows
and spam-guard thresholds from
[`voice-vote-cooldown-acceptance.md`](voice-vote-cooldown-acceptance.md) and
[`voice-rooms.md` §V4](voice-rooms.md#v4-vote-kick) against the producing code
(`crates/core/src/voice_vote_kick.rs`), with the hermetic suite
(`crates/core/tests/voice_vote_cooldown.rs`) as the executable pin.
Offline plus staging read-only; no writes were performed for this check.

## Pinned values

| # | Check | Pinned value | Code cite | Suite test | Result |
| --- | --- | --- | --- | --- | --- |
| 1 | Active-vote window length | `VOTE_KICK_TTL_MS = 120_000` ms (2 minutes, matches "expires after 2 minutes" in §V4) | `voice_vote_kick.rs:12`, deadline computed at `:224-226` | all tests via `DEADLINE_MS = START_MS + VOTE_KICK_TTL_MS` | Match |
| 2 | Re-raise inside the window refused | `Err(ActiveVoteExists)` for any initiator, from start through `deadline - 1`; guard keyed by guild + target (`reference.guild_id == facts.guild_id && reference.target_id == target_id`) | `voice_vote_kick.rs:234-240` | `defeated_vote_cannot_be_reraised_against_the_target_until_its_window_ends`, `initiator_spam_inside_the_window_is_refused_and_reserves_nothing` (10 refused starts, IDs stay `UnknownVote`) | Match |
| 3 | Replay ledger | Initiating interaction ID rejected forever with `ReusedVoteId`, even after expiry | `voice_vote_kick.rs:208-210` | expiry test asserts `start(100, …) == Err(ReusedVoteId)` after the fresh vote starts | Match |
| 4 | One ballot per member | Second button press from the same member fails with `RepeatedVote`, count unchanged; forged buttons across target/room/guild fail with `WrongVoteBoundary` | `voice_vote_kick.rs:308-310` (repeat), `:270-279` (boundary) | `initiator_spam_inside_the_window_is_refused_and_reserves_nothing`, `spam_cannot_be_laundered_through_another_target_or_room_boundary` | Match |
| 5 | Expiry boundary | `now_ms >= expires_at_ms` settles to `Expired`; `start` at exactly `deadline` succeeds with no prior `refresh` (start lazily expires elapsed votes, making no passing decision on that path) | `voice_vote_kick.rs:171-172`, `:229-233` | `window_expiry_re_enables_a_fresh_vote_at_the_exact_deadline` (refused at `deadline - 1`, succeeds at `deadline`, fresh vote opens its own 120 s window) | Match |
| 6 | Strict-majority thresholds | `required = total / 2 + 1`: totals 1–8 need 1, 2, 2, 3, 3, 4, 4, 5; shown as `required/total`; target excluded, roster deduped, initiator gets no implicit Yes, No/abstention never shrinks the denominator | `voice_vote_kick.rs:164` (recount on advance), `:255` (initial count), `:81-87` (`required_total_text`) | `strict_majority_threshold_for_each_electorate_size`, `electorate_excludes_target_dedupes_roster_and_counts_abstention_as_no`, `fresh_vote_after_the_window_counts_the_current_roster` | Match |
| 7 | Scope per target, released on departure | Guard keyed by guild + target (not room + target); `refresh` on target departure cancels with `TargetLeft` and room B can start at once; cancelled votes never revive | `voice_vote_kick.rs:234-238` (key), `:167-168` + `:316-328` (departure cancel via refresh) | `votes_in_room_a_never_block_other_targets_in_room_a_or_room_b`, `target_moving_to_room_b_is_released_by_room_a_departure_refresh` (room B refused before room A refreshes, starts at once after) | Match |
| 8 | No post-terminal cooldown (intentional spec gap) | After expiry, pass or cancellation a fresh interaction ID starts immediately; no `Cooldown` error variant exists in `VoteKickError` | `voice_vote_kick.rs:108-132` (full variant list has no cooldown) | expiry/pass/cancel tests each start a fresh vote in the same clock tick | Match — documented gap, not a mismatch |

No mismatches with the active-vote specification were found in this check.
That result does not establish security readiness: the
[vote-kick security gates](command-wiring-security.md#vote-kick) require a
post-terminal cooldown, a per-initiator limit across targets, protected staff
targets and bounded ledgers. A new cooldown needs a V4 spec change plus a core
change; implementing it remains outside this documentation check.

## Staging read-only check

There is no staging config surface for vote-kick: no `TWO_VOTE_*`,
`TWO_KICK_*` or `TWO_COOLDOWN_*` variable exists anywhere in `crates/` or
`docs/`, no vote-kick key exists in the guild config store, and the worker
layer adds no separate cooldown around the core. The window and thresholds
are code-pinned constants, so staging carries no divergent values to read.
No staging or production endpoint was called and no writes were made.

## Verification

- Threshold table verified by reading the producing code (cites above);
  the arithmetic `total / 2 + 1` reproduces 1, 2, 2, 3, 3, 4, 4, 5 for
  totals 1–8.
- A local run of `test -p two-bot-core --test voice_vote_cooldown` was
  attempted through the cache wrapper and refused (no idle, below-budget
  slot), so the suite did not run locally. Hosted CI on the PR runs it.
- `cargo fmt --all -- --check` passes locally.
