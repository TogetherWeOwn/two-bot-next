# V4 vote-kick decision core

`two_bot_core::voice_vote_kick` implements only the pure decisions from
[`voice-rooms.md` §V4](voice-rooms.md#v4-vote-kick). No Discord types, REST,
persistence, timer task or room-lifecycle runtime are introduced.

## Domain contract

- Supply authoritative `VoteRoomFacts` for the managed room, including its
  guild, owner, original creator and current occupant IDs. Duplicate IDs count
  once. The V4 electorate is the occupants other than the target; no extra
  role, administrator override or bot-specific moderation rule is added here.
- `start` accepts any occupant, rejects self-targeting, absent targets, the owner
  and original creator, and permits only one active vote per guild/target. Use
  the unique initiating interaction ID as the vote ID. Starting is not a Yes
  ballot: V4 says votes are cast with buttons.
- After `Passed`, `Expired` or either `Cancelled` outcome, a fresh vote against
  the same guild/target fails with `Cooldown` until `VOTE_KICK_COOLDOWN_MS`
  (300,000 ms) after the terminal transition; expiry counts from the deadline,
  not from a late observation. Each initiator may succeed at most
  `VOTE_KICK_INITIATOR_LIMIT` (3) starts per `VOTE_KICK_INITIATOR_WINDOW_MS`
  (600,000 ms) per guild, across targets and rooms, else `InitiatorLimited`.
  Only successful starts consume the cap. Refusal order is `ActiveVoteExists`,
  then `Cooldown`, then `InitiatorLimited`; every refusal creates no vote,
  ballot, enforcement decision or cap entry, and reuses no interaction ID.
- `cast` accepts one Yes or No ballot per current eligible occupant. Neither a
  repeated ballot nor a different button from the same member counts again.
  Both abstention and No leave the Yes count unchanged; the denominator is the
  entire eligible electorate, not the number of ballots cast.
- Every transition uses the supplied **current** occupant roster, not a frozen
  start roster. A departed voter's ballot is not counted while they are absent;
  a new occupant counts toward the denominator and can vote. Required Yes votes
  are `total / 2 + 1` (strict majority). `required_total_text` supplies the spec's
  required/total display; the Yes count is also available to the parent.
- Supply processing time through `VoteClock`, not a client-provided time. The
  deadline is fixed at start + 120,000 ms; at or after the deadline, even a
  threshold-reaching ballot expires without a passing decision.
- `refresh` must run on every member/ownership change and at the deadline.
  Target departure cancels permanently, even if the target later rejoins.
  Becoming the owner or original creator also prevents a pending vote passing.
- Carry the complete `VoteKickRef` through buttons, deriving the actual guild
  and room from trusted routing context. A changed ID/guild/room/target or
  mismatched facts cannot mutate another vote. A terminal vote never reopens.
- Only an Active-to-Passed transition returns `kick: Some(RoomKickDecision)`.
  Its scope is the target's disconnect and Connect denial on that room only,
  never a guild kick or ban. Replayed buttons/refreshes expose terminal status
  with `kick: None`; replayed start IDs are rejected, including after expiry or
  cancellation. A fresh vote starts with no inherited ballots.

## Residual parent integration (not parity evidence)

The parent still owns slash/button routing, ephemeral replies and reason text,
authoritative fact gathering, ordered event delivery, target-leave notifications,
scheduled expiry, durable/restart reconciliation and replay-ledger retention.
The in-memory core retains finished IDs for its lifetime; do not treat recreating
it as durable replay protection. Runtime wiring must preserve unique vote IDs and
reject unknown/stale buttons after restart, rather than reconstructing a vote
from button data.

The parent must independently gate and idempotently deliver permission-bearing
actions, check effective room permissions and target presence/protection again
before execution, and handle retries/failures. The one-shot decision is not a
transactional delivery guarantee. No live kick or permission write is performed
or verified by this component's tests.

## Hermetic verification

```sh
cargo fmt --all -- --check
cargo clippy -p two-bot-core --all-targets --locked -- -D warnings
cargo test -p two-bot-core voice_vote_kick --locked
```

The unit fixture uses a manually advanced clock and supplied ID lists. It pins
initiation/voter refusals, owner/creator protection, guild/room/target binding,
roster and ballot deduplication, strict-majority thresholds for odd/even totals,
abstention/No, current-membership changes, exact expiry boundaries, target
leave/rejoin cancellation, one room-scoped passing decision and terminal replay.
No network, Discord, database, credentials or sleep is needed.
