# Existing event reminders: no-new-preference baseline

Status: **revised proposal, design only**. Revised 2026-10-24; review by
**2026-10-31**. Recommendation: stop the new reminder-preference proposal and
retain the existing reminder behavior. No runtime, schema, command, endpoint,
permission, configuration or retention-policy change. Merge records this analysis,
not design acceptance, host alignment, activation or proof of production behavior.
This file is the sole editable spec; disposition records bind its commit and SHA-256.

## 1. Decision and evidence

**Choose no new preference.** Quiet hours were a hypothesis, not demonstrated TWO
member demand. The previous daily-rule/Off proposal is withdrawn from consideration:
no timezone/routine collection, 180-day expiry policy, indefinite member Off/cutoff
marker, or automatic marker on full erasure is proposed by this revision. Broadening
analytics opt-out into reminder-delivery policy is also withdrawn, not approved.
The earlier proposal remains in version history; it is not an alternative to build.

The reminder already posts once in the shared event channel, never a DM. Its
rendered member mentions do not ping under the closed allowed-mentions policy.
Omitting one member would remove their rendered mention, but not hide the message
sent for others or control Discord/client notifications. No Do Not Disturb or
silence guarantee follows from either omission design.

Roadmap-supplied [sesh](https://sesh.fyi/) optional-reminder and
[Dyno](https://docs.dyno.gg/en/modules/reminders) private-reminder references are
not independently reverified vendor capabilities or evidence of TWO demand for
shared-message omission. The community's low-bot-noise goal is a constraint, not
proof that omission solves a member problem. No existing authorized evidence in
this design establishes useful incremental omission value. No member recruitment,
new analytics collection, live send or production inspection was performed.

| Alternative | Benefit and cost supported by this analysis | Revised disposition |
| --- | --- | --- |
| No new preference | No additional personal data or mutations; current shared-channel limitations remain | Recommended baseline; stop preference work |
| Per-event omission | Could remove the actor's mention for one event, without timezone/DST storage; message remains visible and erasure/delayed-mutation safety still needs a policy | Not selected: member value is unproven and residual identity requirements are unresolved |
| Per-member quiet hours | Reusable daily suppression, but routine/timezone storage and new retention complexity for the same limited omission effect | Withdrawn; do not implement |

This is one baseline recommendation, not permission to implement both alternatives
or to substitute a smaller unaccepted feature. It does not amend the separate
[seat/waitlist proposal](rsvp-capacity-spec.md) or personal-reminder work.

## 2. Source baseline and what is not verified

Source baseline: `f2ca0f763c`; source inspection only, not a live runtime audit.

- [Current reminder help](event-reminders.md): Going/Interested only, final
  15 minutes, channel-only, two events/tick and best effort. Whole messages must
  fit 2,000 UTF-16 units; no partial roster or split burst.
- `crates/core/src/event_reminder_store.rs:15-59`: fixed lead, bounded scheduled
  future mirror rows, channel required and sorted bounded RSVP array.
- `event_reminder_store.rs:64-112`: event fields/time and the full attendee array
  revalidated under the guild admission lock; one permanent
  `event:reminder:<guild>:<event>` fence, action `rsvp.reminder_attempt`, NULL actor,
  event-only target, 10,000 retained attempts/guild. Only a known newly committed
  admission authorizes the wire attempt; at capacity admission stops.
- `crates/bot/src/scheduled_jobs.rs:195-275`: attempt precedes rendering/POST;
  invalid/oversized content consumes it without posting. Results are separate;
  missing result is not proof of non-delivery. Reminders do not change RSVP.
- [Privacy scope](privacy.md#member-opt-out-per-user-per-guild): analytics
  suppression is pair-scoped and distinct from full erasure. Existing exceptions
  are disclosed there; they do not authorize new reminder-related retention.

**Unverified policy seam:** reminder SQL does not read `privacy_opt_outs`, a
personal preference, or a dedicated live guild reminder-disable setting. The
existing scheduler is automation/activation-fenced, but that does not establish
what the host means by disabling reminders. This revision neither invents that
setting nor claims analytics opt-out currently suppresses attendee reminders.
It changes neither the existing opt-out scope nor its enforcement. No gap is
waived; actual policy mapping remains a prerequisite to any future change.

The mirror may be stale. An unobserved cancellation/access change, or one after
admission, cannot be promised to prevent a message already in flight. Nothing in
this document claims production health or that a merged spec remedies these limits.

## 3. Private controls and consent: no new surface now

No reminder-preference inspect/change/reset action, saved state, preview endpoint
or adapter is introduced. Existing event help and privacy controls remain as
currently documented; do not present hypothetical buttons as available commands.
No timezone is inferred from locale, IP, device settings or event time.

If a future independently accepted design reopens per-event omission, it must
satisfy all of these boundaries, not inherit the withdrawn quiet-hours contract:

- Derive actor and guild from authenticated context and authorize the event on
  every read/change. No target-member field, delegation, bulk update or host
  impersonation. Foreign/missing/denied events must not reveal a roster.
- Inspect/preview only the actor's state privately, with no write/enqueue; disclose
  that the channel message remains visible, a preview is not an admission, and a
  permanent attempt is not proof of delivery. Never DM or publish suppression data.
- Change must be explicit, pair/event-scoped and idempotent; reset must disclose
  future inclusion and cannot resend a decided reminder. Neither action changes
  RSVP, another member's state, host policy or analytics consent.
- A stale component grants no authority. Unknown authorization, policy, time,
  storage or commit state must fail closed, never fall back to an enabling default.
  These are reopening requirements, not claims that an unbuilt control exists.

## 4. Fences that any later design must preserve

No new scheduler or time policy is selected. Event time remains absolute UTC;
no member zone or daily interval exists. UTC offsets and DST therefore introduce
no new personal suppression boundary. The existing final-15-minute window and
best-effort restart behavior remain as documented.

A future accepted suppression-only change must preserve:

1. **Volume:** output recipients are a subset of the same valid baseline;
   <=1 message/event and <=2 events/tick, never more posts than the unfiltered
   baseline. Validate the unfiltered whole-message limit first: filtering an
   oversized baseline must not manufacture a new post. No replacement, split,
   acknowledgement, unsolicited DM or newly enabled ping.
2. **No defer/catch-up/replay:** a decided reminder stays decided. Restart,
   duplicate workers, late RSVP, preference reset/erasure, event move/channel
   change or mirror recreation must never reset the permanent guild/event fence.
   No occurrence-by-start key, expiring fence or history-absence resend heuristic.
3. **Admission:** applicable host disable and explicitly accepted privacy policy
   must be known; unknown required state fails closed. No personal action enables
   a disabled sender. Whether suppression needs a terminal no-send decision under
   the same permanent event fence must be resolved and proven before build;
   current code does not supply a new suppression branch.
4. **Races:** current snapshot/admission and preference/erasure changes must have
   a deterministic serialization contract, with no stale-cache authorization.
   Unknown commit outcome permits no POST; crash, timeout, refusal, rate limit,
   render failure or lost receipt after admission permits no repost.
5. **Disclosure:** a change committed after admission cannot recall an in-flight
   message. Moves/cancels observed before admission revalidate the mirror; a move
   after the fence never creates a second attempt. Do not promise live knowledge
   beyond the mirror or hold database locks over Discord HTTP.

These are fences on reopening, not authorization to alter baseline runtime now.

## 5. Retention and erasure: no new exception

The selected baseline adds **zero** member preference rows, identity markers,
mutation receipts, schedules, routine history or backup fields. There is no new
TTL, cleanup job or preference erasure/reset operation. Existing privacy intake,
manifest, receipts, backups and retention exceptions are unchanged; this is not
an assertion that existing erasure removes all personal data.

The permanent event-attempt fact has NULL actor and only the event target. Keep
it through member erasure, mirror replacement and backup/restore as currently
required. Do not repurpose it to retain member identities or delete it to resend.
Erasure cannot recall Discord messages, including ones already admitted/in flight.

**Why per-event omission is not selected:** even without a timezone it could need
`(guild, event, member)` suppression and mutation-order/replay state. Deleting that
state could silently resume inclusion or allow a delayed previously unseen action
to restore a deleted choice. Expiring it at the old start time is not a solution
when an event can move. Neither an identity-bearing cutoff, a hashed equivalent,
a full-erasure marker for members with no preference, nor weakened erasure/replay
protection is approved. Existing analytics tombstones must not be repurposed.

If reopened, any residual identity, retention duration, erasure/reset distinction,
delayed-action treatment and restore requirements must be explicit for independent
**non-author CISO disposition** before implementation. Do not silently inherit the
rejected exception. If no policy fits all fences, stop that alternative. This
baseline requests no new security/credential grant; the author grants none.

## 6. Offline design matrix and runnable check

These are source-review/scenario recipes, **not executed acceptance tests** and
not a request to create test/build subtasks. Synthetic IDs/clocks/stores only;
no real member data, Discord send or staging/production database test.

| ID | Deterministic review oracle |
| --- | --- |
| B01 | No new preference: same reminder inputs/output as baseline; zero new state or API/UX surface; RSVP/other actors unchanged |
| B02 | Equal UTC instants expressed with UTC/fractional offsets or across spring/fall DST boundaries produce no new local-time rule or stored zone |
| B03 | Restart/duplicate workers: <=1 permanent admission/event; after fence, unknown commit or past start => no new attempt |
| B04 | No defer/catch-up: closed window or decided attempt never becomes a later send through reset, erasure, restart or event move |
| B05 | Known effective scheduler disable prevents its work; hypothetical personal controls cannot enable it; actual host reminder-disable mapping remains unresolved, not assumed tested |
| B06 | Analytics opt-out: current documented analytics behavior is unchanged; no claim of a new delivery veto or weakened privacy protections |
| B07 | Full erasure: no new Off/cutoff marker, including with no prior preference; preserve existing manifest and NULL-actor event fences; no promise of future-delivery suppression added |
| B08 | Baseline oversized/invalid reminder remains no-send, no split/filter-to-fit; retained-attempt cap never prunes history for room |
| B09 | Failed required state/unknown admission outcome cannot authorize a new send; already-admitted wire effects have the disclosed in-flight limit |
| B10 | Any reopened omission adapter must specify stable policy/native scope IDs, reason codes/next_action, private read-only preview, idempotent mutation and delayed-action/erasure oracles before approval; none is shipped here |

Runnable documentation check now:

```sh
python3 scripts/check_docs_index.py --json
git diff --check
```

There is no new reducer, acceptance target or endpoint to execute. Existing tests
are not proof of a future omission feature. No performance or member-value
measurement is claimed. The discoverable source is this one indexed document.

## 7. Required disposition and stop rule

**Author privacy/consent analysis:** retain baseline and stop new preference work.
This avoids collecting additional personal data for unproven omission value; it
is not independent approval of the author's own new boundary. Existing privacy,
activation, guild-policy and permanent replay protections remain unchanged.

Request a fresh **revision-bound CEO design confirmation** of the baseline/stop
recommendation. Host intent alignment is separately unresolved: the recorded
verdict must establish actual reminder-disable policy and accepted member value,
not equate a roadmap, intake receipt, spec merge or CEO confirmation with alignment.
No claimed silence benefit may substitute for the shared-channel limitation.

No implementation or acceptance-test/build subtasks before **new design acceptance
AND recorded host alignment**, then separate COO admission, test-first execution,
independent non-author CISO disposition where required, exact-head `pr-lint`,
`ci-ok`, `gitleaks`, `CodeRabbit` and `Devin Review`. The baseline recommends
no implementation even if this design record is accepted. Any later feature needs
a new evidenced design disposition, not automatic continuation of either alternative.
No live send, preference/DB change, retention-policy change, access grant,
deployment, publication exception or new spend is authorized.

Review by **2026-10-31**: absent useful omission value within authorized evidence
and a deterministic erasure/replay-safe policy, stop the new preference proposal;
do not manufacture demand or relax fences to keep it alive. This recommendation
already selects that stop rather than waiting for the deadline to start a build.

ACT: revise one indexed design to baseline/stop. WHY: limited omission effect,
unproven demand and unresolved residual-identity trade-off. UNDO: reviewed docs
revert only; no operational state changes. Never undo by erasing replay facts or
sending missed reminders.
