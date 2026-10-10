# V4 vote-kick cooldown and spam-guard acceptance

`crates/core/tests/voice_vote_cooldown.rs` is a hermetic acceptance suite for
the re-raise and ballot-spam guards of
[`two_bot_core::voice_vote_kick`](voice-vote-kick-core.md), written against
[`voice-rooms.md` §V4](voice-rooms.md#v4-vote-kick). It uses only the public
`VoteKickCore` API (`start`, `cast`, `refresh`, `VOTE_KICK_TTL_MS`) and does
not change the core.

## What the "cooldown" is

V4 stacks two guards plus the ledgers, and this suite treats them together as
the cooldown and spam guard:

- **Active-vote window.** One active vote per guild and target ("Only one
  active vote per target"). The window lasts `VOTE_KICK_TTL_MS` (120,000 ms)
  from that vote's start. Inside it, any `start` against the same target fails
  with `ActiveVoteExists`, whoever the initiator is.
- **Post-terminal cooldown (VK-02).** After a vote passes, expires or is
  cancelled, any `start` against the same guild + target fails with `Cooldown`
  until `VOTE_KICK_COOLDOWN_MS` (300,000 ms) after the terminal transition. A
  new interaction ID or a different room does not evade it. Expiry counts from
  the deadline even when the core observes it late.
- **Per-initiator limit (VK-02).** Each initiator may succeed at most
  `VOTE_KICK_INITIATOR_LIMIT` (3) starts per `VOTE_KICK_INITIATOR_WINDOW_MS`
  (600,000 ms) per guild, across targets and rooms; other guilds are separate.
  Further starts fail with `InitiatorLimited`. Only successful starts count.
- **Replay ledger.** A vote's initiating interaction ID never starts another
  vote (`ReusedVoteId`), even after the vote expires.
- **One ballot per member.** A second button press from the same member,
  whether Yes or No, fails with `RepeatedVote`.

Refusal precedence after target validation is `ActiveVoteExists`, then
`Cooldown`, then `InitiatorLimited`. Every refusal creates no vote, ballot,
enforcement decision or cap entry.

## Criterion mapping

| # | Card criterion | Pinned behaviour | Test |
| --- | --- | --- | --- |
| 1 | A failed vote cannot be immediately re-raised against the same target/room | A defeated vote (every eligible member voted No) stays Active until its deadline. V4 settles a vote only on pass, expiry or cancellation. Re-raises by any occupant fail with `ActiveVoteExists` from the start time through `deadline - 1`. The original ID always fails with `ReusedVoteId`. | `defeated_vote_cannot_be_reraised_against_the_target_until_its_window_ends` |
| 2 | Repeated attempts from the same initiator inside the window refuse with a cooldown error | Ten repeated `/kick` starts get `ActiveVoteExists`. None of them creates a vote; their IDs stay `UnknownVote`. Repeated buttons get `RepeatedVote`, and the count stays 1. Forged buttons that change the target, room or guild fail with `WrongVoteBoundary` and cannot add a second ballot. | `initiator_spam_inside_the_window_is_refused_and_reserves_nothing`, `spam_cannot_be_laundered_through_another_target_or_room_boundary` |
| 3 | Expiry ends the window; the post-terminal cooldown gates re-entry | A start at `deadline - 1` is refused with `ActiveVoteExists`; at exactly `deadline` the elapsed vote settles as Expired but the fresh ID is refused with `Cooldown` and reserves nothing (`UnknownVote`). A start at exactly `deadline + 300,000` succeeds without a prior refresh. Old buttons then report Expired with no decision. The fresh vote inherits no ballots and opens its own 120 s window, whose expiry cools down again. | `window_expiry_re_enables_a_fresh_vote_only_after_the_cooldown` |
| 4 | Cooldown is scoped per target member, not per room | Votes against different targets, in the same room or in room B, never block each other. Buttons and facts cannot cross rooms. Room B's decision names room B only. The same member IDs in another guild are independent. See the scope note below. | `votes_in_room_a_never_block_other_targets_in_room_a_or_room_b`, `target_moving_to_room_b_is_released_by_room_a_departure_refresh` |
| 6 | Post-terminal cooldown after pass and cancellation | A pass cools the guild + target down for 300,000 ms: fresh IDs get `Cooldown` and reserve nothing, replays emit no new enforcement, other targets are unaffected, and exactly at `terminal + 300,000` a fresh vote opens with no inherited ballots. A protective cancellation behaves the same; target validation (`ProtectedTarget`) still precedes the cooldown. | `cooldown_after_a_pass_blocks_the_same_target_but_not_other_targets`, `cooldown_after_a_protective_cancellation_with_validation_precedence` |
| 7 | Per-initiator limit across targets, rooms and guilds | Three starts succeed; a fourth target in the same room or in room B gets `InitiatorLimited` and reserves nothing, while another guild is unaffected. The oldest start drops off exactly at `start + 600,000`. A target in cooldown reports `Cooldown` even when the initiator is also capped. | `initiator_cap_spans_targets_and_rooms_but_not_guilds`, `target_cooldown_precedes_the_initiator_cap` |
| 5 | Eligible-voter counting and thresholds are unchanged | Required Yes votes for eligible totals 1–8 are 1, 2, 2, 3, 3, 4, 4, 5 (strict majority), with `required/total` text. The target is excluded and duplicate occupants count once. The initiator gets no implicit Yes. No votes and abstentions do not reduce the denominator. A refused re-raise changes nothing. A fresh vote counts the current roster. | `strict_majority_threshold_for_each_electorate_size`, `electorate_excludes_target_dedupes_roster_and_counts_abstention_as_no`, `fresh_vote_after_the_window_counts_the_current_roster` |

### Scope note for criterion 4

The guard is keyed by **guild and target**, not room and target. That follows
the spec's "one active vote per target", and the existing unit test
`one_active_vote_per_target_with_guild_isolation` pins it. A Discord member is
in one voice channel at a time, so the same target can only appear in room B
by leaving room A. The parent must call `refresh` on that departure. The
refresh cancels room A's vote (`TargetLeft`), releasing the active-vote window.
The cancellation then starts the post-terminal cooldown keyed by guild +
target, so room B is refused with `Cooldown` (not `ActiveVoteExists`) until
exactly `cancel + 300,000`. The suite pins both sides: the window release and
the cooldown. A Discord member is in one voice channel at a time, so room B
can only matter after the departure refresh the core contract already
requires on every roster change.

## Not provided by the core (not parity evidence)

- Coordinated retention bounding of all vote state (VK-03) remains a separate
  card. Staff-permission protected targets (VK-01) and mention-safe reason
  rendering (VK-04) have landed on main and compose with this slice.
- Slash/button routing, ephemeral reply text, durable replay retention across
  restarts and permission-bearing delivery remain parent obligations, as
  listed in [the core document](voice-vote-kick-core.md#residual-parent-integration-not-parity-evidence).

## Hermetic verification

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_vote_cooldown
```

The suite uses a manually advanced clock and fixed ID lists. It needs no
network, Discord, database, credentials or sleep.
