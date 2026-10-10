# V4 vote-kick decision core

`two_bot_core::voice_vote_kick` implements only the pure decisions from
[`voice-rooms.md` §V4](voice-rooms.md#v4-vote-kick). No Discord types, REST,
persistence, timer task or room-lifecycle runtime are introduced.

## Domain contract

- Supply authoritative `VoteRoomFacts` for the managed room, including its
  guild, owner, original creator and current occupant IDs, plus the target's
  guild-scoped privilege (`target_privileged`): `Some(true)` when the target
  holds effective Kick Members or Administrator in that guild, `Some(false)`
  for an ordinary target, and `None` when the guild-authority lookup is
  unavailable. Duplicate IDs count once. The V4 electorate is the occupants
  other than the target; no extra role, administrator override or bot-specific
  moderation rule is added here.
- `start` accepts any occupant, rejects self-targeting, absent targets, the owner
  and original creator (`ProtectedTarget`), privileged Kick Members /
  Administrator holders (`PrivilegedTarget`), and unavailable authority lookups
  (`AuthorityUnavailable`, fail closed with no vote, ballot, or enforcement
  effect), and permits only one active vote per guild/target. Use
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
- `refresh` must run on every member/ownership/role change and at the deadline.
  Target departure cancels permanently, even if the target later rejoins.
  Becoming the owner or original creator also prevents a pending vote passing.
  A mid-vote grant of Kick Members / Administrator, or a lost authority lookup
  (`None`), cancels as `TargetProtected` with no kick decision: the recheck
  runs on every `cast`/`refresh` transition immediately before a pass, so a
  promotion granted mid-vote cannot be bypassed. The runtime rechecks guild
  authority a final time immediately before the Discord deny/disconnect writes
  and writes nothing when the target is privileged or the lookup is
  unavailable.
- Carry the complete `VoteKickRef` through buttons, deriving the actual guild
  and room from trusted routing context. A changed ID/guild/room/target or
  mismatched facts cannot mutate another vote. A terminal vote never reopens.
- Only an Active-to-Passed transition returns `kick: Some(RoomKickDecision)`.
  Its scope is the target's disconnect and Connect denial on that room only,
  never a guild kick or ban. Replayed buttons/refreshes expose terminal status
  with `kick: None` while the vote is retained; replayed start IDs are rejected
  through the post-terminal cooldown horizon inclusive. Past that horizon
  `VoteKickCore::prune` reaps the vote and the ID may start a new vote (VK-03;
  safe because Discord interaction IDs are unique per interaction). A fresh
  vote starts with no inherited ballots.

## Residual parent integration (not parity evidence)

The parent still owns slash/button routing, ephemeral replies and reason text,
authoritative fact gathering, ordered event delivery, target-leave notifications,
scheduled expiry, durable/restart reconciliation and replay-ledger retention.
The in-memory core retains finished votes (and their IDs) only through the
post-terminal cooldown horizon inclusive, and initiator starts only through the
10-minute sliding window. The parent timer must call `VoteKickCore::prune` so
expired entries are reaped even with no new starts, and must collect evicted
IDs after every core call (`drain_evicted`; `prune` returns them for the timer
pass, while `start`/`cast`/`refresh` report only through the drain) and drop
its own per-vote maps for them; do not treat recreating the core as durable
replay protection. The timer must reap only when it can also settle — gate on
authoritative evidence, settle every live vote first, then prune — and must
keep a vote's initiator while its enforcement is still queued, so pruning never
drops an unaudited terminal or an unresolved enforcement fence. Runtime wiring
must preserve unique vote IDs and reject unknown/stale buttons after restart,
rather than reconstructing a vote from button data.

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
initiation/voter refusals, owner/creator/privileged/unavailable protection,
guild/room/target binding,
roster and ballot deduplication, strict-majority thresholds for odd/even totals,
abstention/No, current-membership changes, exact expiry boundaries, target
leave/rejoin cancellation, one room-scoped passing decision and terminal replay.
Bounded retention (VK-03) is pinned by `expired_cooldown_entries_reaped_without_new_starts`
and `retained_state_stays_bounded_across_sustained_churn`: sustained starts and
terminal transitions keep memory proportional to the live window, and expired
entries are reaped by `prune` with no new start.
No network, Discord, database, credentials or sleep is needed.
