# V4 vote-kick cooldown and spam-guard acceptance

`crates/core/tests/voice_vote_cooldown.rs` is a hermetic acceptance suite for
the re-raise and ballot-spam guards of
[`two_bot_core::voice_vote_kick`](voice-vote-kick-core.md), written against
[`voice-rooms.md` §V4](voice-rooms.md#v4-vote-kick). It uses only the public
`VoteKickCore` API (`start`, `cast`, `refresh`, `VOTE_KICK_TTL_MS`) and does
not change the core.

## What the "cooldown" is

V4 defines no separate post-failure cooldown. The existing core has exactly
three guards, and this suite treats them as the cooldown and spam guard:

- **Active-vote window.** One active vote per guild and target ("Only one
  active vote per target"). The window lasts `VOTE_KICK_TTL_MS` (120,000 ms)
  from that vote's start. Inside it, any `start` against the same target fails
  with `ActiveVoteExists`, whoever the initiator is.
- **Replay ledger.** A vote's initiating interaction ID never starts another
  vote (`ReusedVoteId`), even after the vote expires.
- **One ballot per member.** A second button press from the same member,
  whether Yes or No, fails with `RepeatedVote`.

The core has no `Cooldown` error variant. The cooldown errors referred to below
are `ActiveVoteExists` (re-raise) and `RepeatedVote` (ballot).

## Criterion mapping

| # | Card criterion | Pinned behaviour | Test |
| --- | --- | --- | --- |
| 1 | A failed vote cannot be immediately re-raised against the same target/room | A defeated vote (every eligible member voted No) stays Active until its deadline. V4 settles a vote only on pass, expiry or cancellation. Re-raises by any occupant fail with `ActiveVoteExists` from the start time through `deadline - 1`. The original ID always fails with `ReusedVoteId`. | `defeated_vote_cannot_be_reraised_against_the_target_until_its_window_ends` |
| 2 | Repeated attempts from the same initiator inside the window refuse with a cooldown error | Ten repeated `/kick` starts get `ActiveVoteExists`. None of them creates a vote; their IDs stay `UnknownVote`. Repeated buttons get `RepeatedVote`, and the count stays 1. Forged buttons that change the target, room or guild fail with `WrongVoteBoundary` and cannot add a second ballot. | `initiator_spam_inside_the_window_is_refused_and_reserves_nothing`, `spam_cannot_be_laundered_through_another_target_or_room_boundary` |
| 3 | Expiry of the cooldown re-enables a fresh vote | A start at `deadline - 1` is refused; a start at exactly `deadline` succeeds without a prior refresh. Old buttons then report Expired with no decision. The fresh vote inherits no ballots and opens its own 120 s window. | `window_expiry_re_enables_a_fresh_vote_at_the_exact_deadline` |
| 4 | Cooldown is scoped per target member and room | Votes against different targets, in the same room or in room B, never block each other. Buttons and facts cannot cross rooms. Room B's decision names room B only. The same member IDs in another guild are independent. See the scope note below. | `votes_in_room_a_never_block_other_targets_in_room_a_or_room_b`, `target_moving_to_room_b_is_released_by_room_a_departure_refresh` |
| 5 | Eligible-voter counting and thresholds are unchanged | Required Yes votes for eligible totals 1–8 are 1, 2, 2, 3, 3, 4, 4, 5 (strict majority), with `required/total` text. The target is excluded and duplicate occupants count once. The initiator gets no implicit Yes. No votes and abstentions do not reduce the denominator. A refused re-raise changes nothing. A fresh vote counts the current roster. | `strict_majority_threshold_for_each_electorate_size`, `electorate_excludes_target_dedupes_roster_and_counts_abstention_as_no`, `fresh_vote_after_the_window_counts_the_current_roster` |

### Scope note for criterion 4

The guard is keyed by **guild and target**, not room and target. That follows
the spec's "one active vote per target", and the existing unit test
`one_active_vote_per_target_with_guild_isolation` pins it. A Discord member is
in one voice channel at a time, so the same target can only appear in room B
by leaving room A. The parent must call `refresh` on that departure. The
refresh cancels room A's vote (`TargetLeft`), and room B can then start at
once without waiting out room A's window. Before that refresh, room B is
refused with `ActiveVoteExists`. The suite pins both sides. Room A therefore
never blocks room B as long as the parent refreshes on every roster change,
as the core contract requires.

## Not provided by the core (spec gap, not parity evidence)

- **No post-terminal cooldown.** After a vote expires, passes or is
  cancelled, a fresh interaction ID can start a new vote against the same
  target immediately. Criterion 3 depends on this. A longer cooldown after a
  failed vote, or a per-initiator rate limit across targets, would need a V4
  spec change plus a core change. Both are outside this tests-only slice.
- Slash/button routing, ephemeral reply text, durable replay retention across
  restarts and permission-bearing delivery remain parent obligations, as
  listed in [the core document](voice-vote-kick-core.md#residual-parent-integration-not-parity-evidence).

## Hermetic verification

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_vote_cooldown
```

The suite uses a manually advanced clock and fixed ID lists. It needs no
network, Discord, database, credentials or sleep.
