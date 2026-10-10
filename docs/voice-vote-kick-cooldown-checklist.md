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
| 5 | Expiry boundary | `now_ms >= expires_at_ms` settles to `Expired`; `start` at exactly `deadline` lazily expires the elapsed vote (no passing decision on that path) with terminal time backdated to the deadline, then refuses the fresh ID with `Cooldown`; the fresh vote succeeds exactly at `deadline + 300,000` and opens its own 120 s window | `voice_vote_kick.rs` expiry branch in `advance` (backdated terminal time) and lazy sweep in `start` | `window_expiry_re_enables_a_fresh_vote_only_after_the_cooldown` (refused at `deadline - 1` with `ActiveVoteExists`, refused at `deadline` with `Cooldown`, succeeds at `deadline + 300,000`) | Match |
| 6 | Strict-majority thresholds | `required = total / 2 + 1`: totals 1–8 need 1, 2, 2, 3, 3, 4, 4, 5; shown as `required/total`; target excluded, roster deduped, initiator gets no implicit Yes, No/abstention never shrinks the denominator | `voice_vote_kick.rs:164` (recount on advance), `:255` (initial count), `:81-87` (`required_total_text`) | `strict_majority_threshold_for_each_electorate_size`, `electorate_excludes_target_dedupes_roster_and_counts_abstention_as_no`, `fresh_vote_after_the_window_counts_the_current_roster` | Match |
| 7 | Scope per target, released on departure | Active guard and cooldown both keyed by guild + target (not room + target); `refresh` on target departure cancels with `TargetLeft`, releasing the active guard, and the cancellation starts the 300,000 ms cooldown, so room B is refused with `Cooldown` (not `ActiveVoteExists`) until exactly `cancel + 300,000`; cancelled votes never revive | `voice_vote_kick.rs` active-guard key, departure cancel in `advance`, guild + target cooldown check in `start` | `votes_in_room_a_never_block_other_targets_in_room_a_or_room_b`, `target_moving_to_room_b_is_released_by_room_a_departure_refresh` (room B refused with `ActiveVoteExists` before room A refreshes, with `Cooldown` after, starts at `cancel + 300,000`) | Match |
| 8 | Post-terminal cooldown (VK-02) | `VOTE_KICK_COOLDOWN_MS = 300_000` ms per guild + target after pass, expiry or cancellation; fresh IDs and room changes refused with `Cooldown`; expiry counts from the deadline even when observed late; refused attempts reserve nothing | `voice_vote_kick.rs` cooldown const, `Cooldown` variant, terminal-time tracking in `advance`/lazy sweep, guild + target check in `start` | `window_expiry_re_enables_a_fresh_vote_only_after_the_cooldown`, `cooldown_after_a_pass_blocks_the_same_target_but_not_other_targets`, `cooldown_after_a_protective_cancellation_with_validation_precedence`, `target_moving_to_room_b_is_released_by_room_a_departure_refresh` (boundary just-before/exactly-at in each) | Match |
| 9 | Per-initiator limit (VK-02) | `VOTE_KICK_INITIATOR_LIMIT = 3` starts per `VOTE_KICK_INITIATOR_WINDOW_MS = 600_000` ms per guild + initiator, across targets and rooms; other guilds separate; only successful starts count; refused with `InitiatorLimited`; precedence `ActiveVoteExists` > `Cooldown` > `InitiatorLimited` | `voice_vote_kick.rs` initiator consts, `initiator_starts` map with sliding-window prune in `start` | `initiator_cap_spans_targets_and_rooms_but_not_guilds`, `target_cooldown_precedes_the_initiator_cap` (window boundary just-before/exactly-at) | Match |
| 10 | Out-of-scope gates stated | VK-01 (staff-permission protected targets), VK-03 (bounded retention), VK-04 (mention-safe reason) remain open on separate cards | [security gates](command-wiring-security.md#vote-kick) | n/a (no tests added for these gates) | Open — see below |

No mismatches with the V4 cooldown specification were found in this check.
That result does not establish security readiness: the
[vote-kick security gates](command-wiring-security.md#vote-kick) additionally
require VK-01 (targets with effective Kick Members or Administrator permission),
VK-03 (bounded retention across all vote stores) and VK-04 (mention-safe
reason rendering). Those three gates are out of scope for this slice and remain
open; this PR implements VK-02 only.

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
