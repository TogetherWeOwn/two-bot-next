# Raid-watch and join-risk port boundaries

This TOG-9809 slice adds **framework-free decisions and staff-message proposals**
only. It does not subscribe to gateway events, load environment variables,
connect to storage, post to Discord, or activate anti-nuke. There are no member
mutation outcomes. Voice-room features belong to TOG-10091, and the other S4
feature handlers remain on TOG-10075–TOG-10089.

## Frozen source

All behavior below is grounded in `TogetherWeOwn/two-bot` revision
`d5d1179348feb9157bcac8c875de9399d4f5c76a`:

| Contract | Pinned source | Git blob SHA |
| --- | --- | --- |
| Burst, dedupe, cooldown and message text | [raidWatch.ts](https://github.com/TogetherWeOwn/two-bot/blob/d5d1179348feb9157bcac8c875de9399d4f5c76a/src/analytics/raidWatch.ts#L127-L220) | `438446aeeb12a6836ae746764da30374f63188f0` |
| Raid delivery | [raidAlert.ts](https://github.com/TogetherWeOwn/two-bot/blob/d5d1179348feb9157bcac8c875de9399d4f5c76a/src/discord/raidAlert.ts#L36-L75) | `bc6f8afd1ade23ecbe1396ebf975bd854ae9d810` |
| Account-age score, identity and bulk suppression | [containment.ts](https://github.com/TogetherWeOwn/two-bot/blob/d5d1179348feb9157bcac8c875de9399d4f5c76a/src/moderation/containment.ts#L203-L267) | `f3447f72d42eb8fb2a36be6ad90e46de5456f511` |
| Durable count and scoring transaction | [containmentStore.ts](https://github.com/TogetherWeOwn/two-bot/blob/d5d1179348feb9157bcac8c875de9399d4f5c76a/src/moderation/containmentStore.ts#L153-L205) | `283e877060ca4398c8f700a6752b00026c1e73b1` |
| Join-risk staff text and delivery | [containmentAlert.ts](https://github.com/TogetherWeOwn/two-bot/blob/d5d1179348feb9157bcac8c875de9399d4f5c76a/src/discord/containmentAlert.ts#L36-L65) | `b3a0e19e6a4230748459b962ae6efd5829005c78` |

The source blobs were downloaded by immutable SHA and their Git object hashes
verified before implementation. Legacy tests used as behavior references:
[raid watch](https://github.com/TogetherWeOwn/two-bot/blob/d5d1179348feb9157bcac8c875de9399d4f5c76a/test/unit.raidwatch.test.ts),
[join risk](https://github.com/TogetherWeOwn/two-bot/blob/d5d1179348feb9157bcac8c875de9399d4f5c76a/test/unit.containment.test.ts#L342-L418),
and [concurrent join scoring](https://github.com/TogetherWeOwn/two-bot/blob/d5d1179348feb9157bcac8c875de9399d4f5c76a/test/e2e.containment.test.ts#L89-L109).

## Raid-watch decisions

- Defaults: 5 joins / 60 seconds; 900-second cooldown; at most 50 IDs in a
  message. The ID cap does not cap the count or internal state.
- `RaidTuning` is one immutable snapshot supplied per observation. The next call
  can use updated window/threshold settings without rebuilding the watch.
- `RaidWatch` is volatile and isolated by guild. The adapter supplies non-bot
  joins, validated IDs and valid epoch-millisecond occurrence timestamps.
- Member dedupe happens **before** window pruning. A still-retained member's
  rejoin cannot advance the window; another member must first prune that entry.
- Sort by occurrence time, preserving insertion order on ties. Retain only
  timestamps strictly greater than `newest - window`; exact-window separation
  is excluded. Late arrivals use the newest occurrence, not the processing clock.
- Threshold equality alerts; cooldown equality releases. Cooldown is consumed
  when the proposal is returned, before any delivery. Missing destination,
  dry-run, unavailable permissions or send failure must not roll it back or retry.
- `repeat` means this guild has ever alerted in this watch instance, including
  after long quiet periods. Restart clears both the window and repeat/cooldown.
- Preserve legacy text quirks: first-alert text prints threshold **5** even when
  tuning differs; repeat text's “more joins” is the current count, not a delta.
  Span rounding is the legacy nonnegative `Math.round` behavior.

## Historical replay

`scan_joins_for_bursts` replays recorded joins through a **fresh** `RaidWatch`
per call, as legacy `scanJoinsForBursts` did
([raidWatch.ts](https://github.com/TogetherWeOwn/two-bot/blob/d5d1179348feb9157bcac8c875de9399d4f5c76a/src/analytics/raidWatch.ts#L170-L196)),
so a threshold can be checked against history before it ships. Options are one
fixed snapshot (`RaidScanOptions`, defaulting to the values above); nothing
carries between calls. Window, dedupe, cooldown and `repeat` stay per guild
inside one call, so one replay over two distant raids yields a second alert
marked `repeat`.

- Input need not be sorted. Joins are stably sorted by **parsed instant**, with
  input order on ties. Legacy sorted the raw strings (`localeCompare`), which
  only matches chronological order for uniform `toISOString` output; mixed
  offsets or precision now replay in true occurrence order (`docs/parity.md`).
- A timestamp that is not strict RFC 3339 (explicit offset required) is
  skipped, like legacy `Date.parse` NaN. Legacy also accepted non-RFC 3339
  shapes such as date-only or offset-less strings; the port skips those, so an
  export must emit RFC 3339. Sub-millisecond digits truncate.
- `crates/core/tests/raid_replay.rs` replays the legacy scenarios
  ([unit.raidwatch.test.ts](https://github.com/TogetherWeOwn/two-bot/blob/d5d1179348feb9157bcac8c875de9399d4f5c76a/test/unit.raidwatch.test.ts#L74-L130)):
  1,015 joins over 56 minutes give 3-6 cooldown-spaced alerts; each 15-join
  small raid alerts once at count 5; 7 joins 8 minutes apart never alert; a
  10-ID cap with a 10-second cooldown truncates the follow-up alert.
- The replay is pure: no database read or runtime wiring is added. The manual
  `raid-list` tool ([TOG-10867](/TOG/issues/TOG-10867)) does not call it in this
  slice.

## Join-risk decisions

`JoinRiskPolicy::prepare` ignores bots and other guilds. Joining members are not
exempted by executor trusted/protected lists. Account age is join occurrence minus
account creation time: `<24h` scores 3, `[24h,7d)` scores 1, and `>=7d` scores 0.
No stale/future rejection or negative-age clamp is introduced. An absent join
timestamp uses the explicit fallback clock.

The event identity is `guild:member:joinedAtISO` with millisecond precision.
Distinct rejoin timestamps produce distinct evidence. Without a join timestamp,
changing the fallback clock also changes identity, as in legacy.

The future store must serialize by guild, reject an existing event ID, count
previous rows by **processing-time `created_at`** in `(now-window, now]`, score
with that count plus the current join, and insert in the same transaction.
`count_recent_join_risks` models that WHERE clause for already-claimed rows; its
window must come from the validated observation. All observations count,
including unflagged and bulk-suppressed ones. The current join gains 2 points at
burst threshold equality; earlier joins are never retroactively flagged.

Flag when score is at least 3 unless `bulkJoinWindowUntil >= joinedAt`, including
expiry equality. Suppression uses occurrence time and has no source prerequisite
or start boundary; it preserves the score, reasons and attribution evidence.

`staff_message(persisted)` requires the actual successful-insert disposition;
false or unflagged returns no message. Persistence precedes delivery. A send
failure or crash is not repaired by duplicate replay: there is no outbox or
retry proposal in this contract. No evidence TTL/erasure policy is added here.

## Explicit port adjustments

- Numeric constructors accept finite positive thresholds/windows, including
  fractions; cooldown accepts finite nonnegative seconds. Legacy raid options
  did not validate malformed numbers. This pure API rejects them explicitly
  instead of producing NaN/empty-window behavior. No environment/settings parser
  is changed or activated in this slice.
- Replace the legacy alert's absent `node scripts/roster.ts 1` command with
  “Review join-attribution evidence to see which invite sent them.” Operator
  reporting scripts are not Rust runtime features (`docs/parity.md`). Other
  alert wording and flag-only semantics are preserved.

## Runtime obligations and verification boundary

A message proposal is **not** authorization to post. Both builders use the
existing `MentionPolicy::None`: the shared executor must send empty mention
parsing and no explicit member/role recipients. Log evidence first; only use the
configured staff destination, with a guild text-channel and bot View/Send check.
No DM, member ping, kick, ban, timeout or server-setting change belongs here.
Destination selection, permissions and real delivery are not tested by this slice.

Legacy raid watch is constructed independently of moderation/anti-nuke flags.
Join risk is constructed only under exact `TWO_ANTI_NUKE=1`, inside the staging
and verified identity fences described in `docs/containment.md`; it has no
independent enable switch. Anti-nuke dry-run does **not** suppress risk evidence
or staff messages. Preserve session-mode refusal of armed anti-nuke and the
adapter's contained-restart skips. Do not infer activation from a pure proposal.

Runtime integration remains on this parent's retained work: transactional store
and migrations, gateway calls after join recording, live raid tuning, startup
fences, log/delivery through the shared REST executor, and staging soak. The
executor seam is TOG-10076; no private production HTTP client is added here.

`raid.rs` unit tests cover strict clocks/age/window/cooldown boundaries, reverse
arrival, dedupe-before-prune, fractional/live tuning, guild isolation, payload
cap, repeat history, fallback identity, inclusive bulk expiry and flag-only text.
`raid_acceptance` uses sequential claimed-row fixtures and a scripted Discord
double for effect acceptance and failure consumption. Fixture reuse across scorer
recreation is **not** proof of durable restart dedupe or concurrent transactions.
No production/staging database or guild is used. Future persistence tests must
use agent-testdb/CI service containers only.
