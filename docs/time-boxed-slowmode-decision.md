# Time-boxed slowmode: retain manual reset

## Disposition and evidence boundary

**No build: keep existing authorized manual `/slowmode` and private
`/channel-inspect`. Do not add automatic expiry or a private reminder job.**
This is a design outcome, not a command, capability grant, runtime acceptance or
activation. Review target: **2026-11-02**, not a release promise or default-fire
date. No runtime, schema, migration, host timer, service, credential or spend
change follows. Documentation publication changes no channel setting.

Consumer: staff handling a brief traffic burst might avoid leaving slowmode on
longer than needed. That benefit is a **hypothesis**. No first-party demand,
frequency of forgotten resets, or incremental benefit over manual follow-up was
established by this source review. The [Wrenchy feature list](https://wrenchy.org/features)
describes moderation/crisis controls, not measured demand for slowmode expiry.
Do not infer host consent from a vendor feature list or a generated proposal.

Source baseline: `5db6ecccd7c67202ecea6d76f9ae65ceab2e3654`.
The following are repository/API-documentation observations, not deployed-state
claims. No live guild, channel, database, member or permissions were inspected.

- `crates/core/src/moderation.rs:191-202` defines `seconds:0..21600` (zero
  disables) and reason, with Manage Channels. There is no duration option.
  `:488-525` enables moderation only for `TWO_MODERATION=1`, with a valid
  `TWO_OWEN_USER_ID` while enabled; no deployed flag value is asserted here.
- `crates/core/src/router.rs:578-614` fences configured guild, moderation
  feature and actor permission. `crates/discord/src/channel_moderation.rs:263-289`
  binds the request to its invoking guild/channel/member. These existing gates
  are not a grant for unattended future action.
- `crates/discord/src/channel_moderation.rs:469-477` prepares slowmode directly,
  with no saved restoration seed or prior-delay observation. Manual inspection
  before resetting is operator guidance, not an enforced preflight in that
  branch. `:740-759` audits actor/channel/reason/requested seconds; request
  history does not prove who owns the live setting now.
- `crates/discord/src/channel_moderation_reads.rs:141-165` validates same-guild
  text/announcement identity for inspection. Missing/zero delay is off;
  malformed values or read failure are unknown. Setter/time are explicitly
  unknown. Saved lockdown metadata is not slowmode provenance.
- `crates/discord/src/executor.rs:2449-2472,2575-2597` GETs a channel document
  and PATCHes the requested numeric delay. The slowmode write accepts no
  expected-state/generation predicate. `crates/core/src/channel_moderation_store.rs:293-473`
  provides durable request/channel exclusion and atomic settlement, not a
  Discord setting-intent revision or an expiry. An uncertain lane has no TTL.
- The signed internal/website writer also reaches the same slowmode adapter
  under its own enablement/resolved-actor/policy gates
  (`crates/discord/src/internal_channel_moderation.rs:213-232,321-375`). A
  proposed bot generation must cover that writer too, without pretending it
  covers direct Discord staff edits. No new API surface is proposed.

The [Discord Channel resource and Modify Channel contract](https://docs.discord.com/developers/resources/channel)
documents `rate_limit_per_user` as a numeric delay, and a PATCH setting that
value. The inspected contract supplies no documented per-setting intent revision
or conditional expected-generation write. This is **no sufficient witness found
in the inspected contract**, not proof that every Discord surface lacks one.
Audit history, cached gateway events and a bot-owned database generation are not
an atomic external-effect ownership predicate.

## Alternatives and distinction from lockdown

| Alternative | Benefit and cost | Outcome |
| --- | --- | --- |
| No build; inspect and manually set the desired delay | No new background authority or retained reminder data; staff must remember follow-up | Choose now |
| Private reminder only, with no automatic channel action | Could prompt inspection without resetting; still needs demand, recipient/authority checks, bounded delivery and lifecycle/privacy design | Defer; do not implement or send from this decision |
| Automatic time-boxed slowmode | Could avoid a manual reset, but can overwrite a later staff decision or restore an inappropriate prior value | Refuse under the inspected provenance contract |

Slowmode changes message pacing, not channel permission overwrites. It does not
promise to stop all posting or restore channel access. It is not a weaker form
of permission restoration that can inherit permission to automate it. The
[time-boxed lockdown decision](time-boxed-lockdown-decision.md) remains intact:
manual authorized `/unlock`, no automatic restoration and no dispatched reminder.
This record neither supersedes nor generalizes that decision.

## Why an equal delay is not ownership

Suppose the observed prior delay is 5 seconds and the bot sets 30 seconds:

1. No later edit: current delay is 30.
2. Staff intentionally reassert 30: current delay is also 30.
3. Staff set 60, then 30 (ABA): current delay is again 30.

A channel ID plus `current == 30` cannot distinguish these histories. Restoring
5 would overwrite the later decision in cases 2 and 3. Restoring zero would also
invent a prior state. Even a successful GET followed by PATCH has a read/write
race: staff can change the delay after GET and before PATCH. Serializing bot
commands does not serialize direct Discord writers. A deadline is not renewed
authority; a permission check is not atomic with the later PATCH and cannot
prove the initiating actor still has authority at that effect.

No automatic expiry is proposed. Any materially new proposal would need **all**
of the following before it could even be considered for implementation:

- Durable guild/channel identity and a non-reused setting-intent generation for
  every participating writer, including same-value renewals; a new intervention
  invalidates the old generation. A lane/claim generation alone is not this.
- A proven observation of the actual prior delay, the explicitly requested
  delay, and one finite absolute UTC deadline. Missing prior state is unknown,
  not zero. Do not borrow a lockdown recovery seed for slowmode.
- Current-state comparison and an independently accepted external witness or
  equivalent design that protects later human intent **at the effect boundary**.
  The same-value and GET/PATCH counterexamples must be solved, not waived.
- Current actor/bot authority, configured guild, feature/activation, drain and
  security holds; no impersonation, replacement moderator or background grant
  inferred from the original command. Human edits win, including same-value edits.
- Durable uncertainty before sending, bounded retry with a finite budget and no
  blind mutation replay. A confirmed effect with failed settlement permits only
  safe settlement of that exact effect. Unknown outcome stays fenced for recovery.
- Private inspect/cancel UX, no-effect dry-run/cancel and exact-generation stale
  rejection. Saved intent, pending work, observed setting and unavailable reads
  must be distinct; none is a synonym for applied restoration.

If any condition cannot be met, keep no build or reconsider reminder-only on
its own merits. Do not create an executor, lease expiry, audit polling system or
new authority merely to make a timer proposal appear viable.

## State and tabletop matrix

These are obligations for reconsideration and manual inspection guidance, **not
implemented timer states**. Every row currently implies zero automatic setting
writes and zero reminder sends. Existing manual commands retain their own gates.

| Case | Required safe disposition / evidence |
| --- | --- |
| Restart or replay of a delivery | Do not create a timer from an old command/audit row; retain existing idempotency/uncertainty fences |
| Restart after deadline / missed deadline | No catch-up reset or notification; elapsed time does not establish current intent |
| Two overlapping interventions | An older generation must never restore over the newer one; equal requested delays do not merge intent |
| Later staff edit to another delay | Preserve that decision; inspect manually, do not restore the recorded prior delay |
| Identical-value edit or 30 → 60 → 30 | Numeric equality is insufficient; no automatic restoration |
| Staff edit after GET but before PATCH | Preflight equality cannot protect the effect; refuse the automatic design |
| Actor leaves, authority changes, bot permission lost | No delegation to the background job or another actor; inspection/refusal, not retry-until-authorized |
| Guild/feature/activation off, drain or security hold | No deadline exception to existing gates |
| Missing channel / Discord 404 | No guessed replacement or automatic write; inaccessible is not affirmative proof of deletion |
| 403 / forbidden | Refuse; unknown is not off/deleted. No background retry waiting for restored authority; only a new authorized human action may proceed, with no credential fallback |
| Transport/read timeout, malformed response | Unavailable/unknown, not off, deleted or safe to reset; no read or write retry is commissioned by this decision; future proposals need an explicit finite budget |
| Ambiguous manual mutation, crash or lost response | Preserve that manual command's existing durable lane/effect fence; reconciliation, not another PATCH; never create a timer fence |
| Confirmed effect; settlement failed | Retry only settlement under the exact existing claim; do not repeat the effect |
| Cancel, duplicate cancel or dry-run | No Discord setting write; never claim cancellation undid an accepted/uncertain effect |
| Delete/recreate channel or intent record | Never reuse old identity/generation, infer a replacement channel, or revive stale work |

## Human and agent handoff

No new UI or agent endpoint is created. Existing private inspection distinguishes
saved lockdown evidence from observed slowmode; it must not be described as a
saved slowmode expiry or setter record. An unavailable read is not zero/off.
For future reminder-only reconsideration, require private, bounded, mention-safe
plain text, no public ping or DM fallback, authenticated current recipient and
channel authority, and no reset button that smuggles in automatic restoration.
Suggested wording is **not shipped copy**: “Slowmode needs review. Inspect the
current channel setting; no automatic reset was scheduled.” Never say “timer
saved,” “cancelled,” or “restored” without the corresponding evidence.

Stable design identifier: `slowmode-expiry-v1`. An agent can discover this record
through the [documentation map](README.md). Example **design handoff JSON**, not
a runtime response or a new executable operation:

```json
{"decision_id":"slowmode-expiry-v1","outcome":"no_build","reason":"setting_intent_unproven","next_action":"use_existing_authorized_manual_inspection","automatic_writes":0,"reminder_sends":0}
```

Any future exact-spec proposal would need to define guild/channel, request and
non-reused intent IDs, observed timestamp and spec commit; this record defines
or authorizes no such operation. Missing IDs or observations must be
null/unavailable, never invented. Do not publish real IDs,
reasons, member names, interaction tokens or remote error bodies in a decision
report. Fixed reason/outcome codes are preferable to free-text audit payloads.

**Retention/erasure:** this no-build outcome adds no personal data, durable job,
notification recipient, retention horizon or erasure exception. Existing audit,
claim and erasure rules in [privacy](privacy.md) remain unchanged; in particular,
do not erase an uncertain-effect replay fence to make a command retryable. A
future reminder design must specify minimal actor/guild/channel identity, finite
expiry and removal after completion/cancellation, per-member/guild erasure and
unresolved-effect handling before build admission. This document grants no new
retention period. Synthetic tabletop histories contain no member data.

## Runnable offline check

From the repo root, run `python3 scripts/check_docs_index.py --json`; require
exit 0 and empty `missing`, `stale`, `oversize` arrays on the publication head.
Otherwise repair the index/budget before publication. Then run this tabletop:

```sh
python3 - <<'PY'
import json
histories = {
    "untouched": [5, 30],
    "same_value_human_intent": [5, 30, 30],
    "aba_human_intent": [5, 30, 60, 30],
}
observations = {name: states[-1] for name, states in histories.items()}
assert set(observations.values()) == {30}
assert len({tuple(h) for h in histories.values()}) == 3
# The channel/value observation is identical despite different later intent.
assert observations["untouched"] == observations["aba_human_intent"]
read_value, human_edit_after_read = 30, 60
assert read_value == 30 and human_edit_after_read != read_value
print(json.dumps({
    "decision_id": "slowmode-expiry-v1",
    "outcome": "no_build",
    "reason": "numeric_equality_does_not_prove_intent",
    "next_action": "use_existing_authorized_manual_inspection",
    "indistinguishable_histories": len(histories),
    "automatic_writes": 0,
    "reminder_sends": 0,
}, sort_keys=True))
PY
```

This proves only the model's indistinguishable histories and a read/use gap;
it is not protected acceptance testing, Discord/DB behavior, real permission
verification or demand evidence. No network, database, flags or effect executor
is involved. No Cargo build is needed for this documentation-only outcome.

## Independent boundary and reconsideration

The author recommends **no new automatic authority** and cannot independently
approve an authored capability request. Independent review must assess these
exact document bytes and the no-build boundary, not approve a hypothetical
executor. Normal exact-head CI, CodeRabbit and Devin Review remain required
for publication; they do not constitute new guild-host intent or activation.

Only reconsider on materially new evidence: demonstrated incremental value,
authenticated actual guild-host intent bound to an exact spec, and independent
security disposition solving the provenance/current-authority gaps. If a build
is then recommended, route exact-spec intent through the existing executive
process; no host or owner approval question is manufactured for this no-build
outcome. Review date expiry never authorizes a timer. Until evidence changes,
kill/defer automatic expiry and keep reminder-only uncommissioned. No runtime
rollback is needed; supersede this record explicitly if the decision changes.
