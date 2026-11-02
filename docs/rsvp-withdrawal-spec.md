# RSVP: withdraw my saved response

Status: **Merged into [RSVP capacity](rsvp-capacity-spec.md)**, deferred to
2026-12 (owner decision 2026-11-01). That doc is now the single RSVP-policy
spec; this one stays as the detailed record of its slice. Edit policy there,
not here: the "single editable repository contract" wording in the original
proposal text below is superseded by this merge. Original proposal:
written 2026-10-26. This is the single editable repository contract; the task
spec mirrors its exact commit and SHA-256. Merge is not acceptance, host
alignment, build approval or activation. No runtime, acceptance-test, migration,
permission, credential, publication, deployment or spending change is included.

## 1. Decision and baseline

Consumer: a member who wants no saved response rather than a saved Declined.
Hypothesis: that distinction is useful to members; demand is **unproven**.
The [sesh manual](https://sesh.fyi/manual/), read 2026-10-26, says members can
remove/change an RSVP before an event starts. That is comparison, not TWO demand
or a grant to reproduce its other controls. No member outreach or live audit ran.

| Alternative | Member effect / cost | Proposed disposition |
| --- | --- | --- |
| A. No build; current Declined | A saved Declined remains visible in personal status and declined totals; excluded from future reminder rosters. No new schema/UX | Standing fallback until acceptance; choose it if value or safe bounds fail |
| B. Self-only withdrawal | Remove one saved Going, Interested or Declined; personal state becomes absent and that status total falls by one; needs stale-write fencing, atomic audit and shared admission order | Submit for exact intent/security review, not implementation |
| C. Host/bulk/historical clear | Broader authority and historical/privacy implications | Excluded; not a hidden fallback |

Source baseline: `e1868d97d3519cae3f2ed91726e83e98f77dff0d`; source inspection
only, not deployed-state evidence:

- `crates/discord/src/rsvp.rs:330-367,374-448`: totals distinguish saved choices
  from absent and allow historical reads. Saves verify current membership/event
  before and after commit, with fenced compensation. Current live-event check
  accepts Scheduled, Active and Completed, refuses Cancelled; no start cutoff.
- `crates/cutover/migrations/0160_rsvp.sql:24-34`: one response per
  `(guild_id,event_id,user_id)`, no occurrence column, version or event cascade.
- `crates/core/src/rsvp_store.rs:112-161,503-643`: save locks event, member-minute
  budget, then full identity; compensation uses only the identity lock and
  audit/timestamp/xmin fencing. It is rollback repair, **not a public clear API**.
  Audit deduplication alone does not prevent a preceding save upsert.
- `crates/core/src/event_reminder_store.rs:64-112`: admission has a separate
  `event_reminders:<guild>` lock. These paths do **not** share one ordering today.
- [Reminder contract](event-reminders.md): final 15 minutes, channel-only,
  suppressed pings, bounded full-roster or count-only fallback; permanent
  guild/event attempt fence, including uncertain delivery. Withdrawal must not
  restore the older oversized-message no-send behavior in other proposed docs.

Existing [RA-01/RA-03 controls](command-wiring-security.md)
remain build gates; this proposal does not certify a new path against them.
Attendance/host check-ins, Discord-native event subscriptions and LFG are distinct.

## 2. Proposed private UX and identity

Proposed entry only: `/rsvp-withdraw event-id:<id-or-same-guild-event-link>`.
No member, occurrence, force, bulk or admin override option. Strictly validate
one bounded option through the shared router and existing event-link normalizer;
never match by name, start time, channel title or a guessed recurring occurrence.

Identity is the authenticated guild + Discord event snowflake + authenticated
actor, exactly the existing response key. For recurring events it means the
stored event-level response, **not** a guessed occurrence or all future events.
Attendance's `event:label` occurrence IDs are invalid here. Rescheduling the same
event preserves identity and reminder fence; a new Discord event ID is distinct
and never inherits this withdrawal or response. Do not change attendance facts.

1. Preserve application/configured-guild, Announcements feature/activation,
   admission and reply fences. Successful ephemeral defer precedes work; failed
   defer writes nothing. Actor/guild come only from authenticated context.
2. Verify current actor membership before event access. Resolve a same-guild
   Scheduled event with an absolute future start (`DB now < start`); require a
   bound same-guild channel and fresh evidence of actor View Channel through the
   shared executor. Bot visibility is not proof of member visibility. Unknown
   permission/thread evidence, 403/404, foreign/DM context or channel-less events
   are unavailable without revealing saved status, totals or event details.
3. Active, Completed, Cancelled, deleted and start-at-or-before-now events are
   ineligible, even if a saved row remains. This deliberately narrower rule does
   **not** change legacy saves or historical `/rsvp-attendance`. Historical
   withdrawal is not a privacy shortcut; use the existing privacy intake.
4. Authorized preview is read-only. Absent => no-op, no control/audit. Otherwise:
   “Withdraw your saved response for event `<id>`? It will no longer count in
   saved RSVP totals. An already queued reminder may still include you.” Offer
   **Withdraw my response** / **Cancel**. No status, member list or event name
   needs to be echoed. All replies ephemeral, <=400 UTF-16 units, no pings/DMs.
5. Confirmation binds exactly the preview's event and random response version,
   expiry and runtime boot nonce, never silently the newest response. Require
   authenticated bot-authored ephemeral message metadata whose originating user
   matches the actor, and correct application/guild. Missing evidence refuses.
   Expire at DB UTC preview time +300 seconds, equality expired. Every restart
   creates a fresh 128-bit boot nonce; controls from another runtime refuse.
   Entropy failure leaves withdrawal unavailable. This is not a bearer capability.
   One possible fixed encoding: `two:rw1:` + unpadded canonical base64url of
   event u64 + version 16 bytes + expiry u64 + boot nonce 16 bytes (network byte
   order), 48 payload bytes / 64 encoded characters, 72 ASCII characters total.
   Reject malformed length, invalid/zero snowflake, invalid version or expiry;
   never log tokens, raw component IDs or reply bodies.

Withdraw is not Declined, cancellation, unsubscribe, notification silence,
attendance reversal or privacy erasure. Shared-channel messages remain visible;
previous messages and admitted reminders cannot be recalled. Saving again is an
explicit new response, not undo of a delivery attempt. No public marker is sent.

## 3. Proposed store order, races and bounded work

These are required **new seams**, not existing functionality. Smallest safe build
would add a database-generated UUIDv4 response version (122 random bits) to the
existing row. Every successful save, including same-choice saves and restored
predecessors, rotates it; versions are never timestamps, xmin or status. Use
Postgres's built-in `gen_random_uuid()`, not a new extension/grant. A database-owned,
security-invoker insert/update trigger covers old-style writers too: INSERT must
omit version; UPDATE must leave the old version untouched as input; the trigger
assigns a fresh version and rejects caller assignments and immutable-key updates.
Verify function availability before migration; unavailable means A/no rollout,
never activate a trigger that breaks existing saves. The build must specify/test
expand-backfill-validate and import compatibility before issuing controls; no
migration number or executable DDL is assigned here. Deleting the row removes its
version: **no member tombstone or permanent withdrawal ledger**. Collision risk is
negligible, not zero (birthday bound n²/2^123); intent must accept that assumption.

New global order for save, withdrawal, save compensation and reminder admission:
existing reminder guild advisory lock -> existing event lock -> existing member
minute-budget lock -> full response identity lock -> row. Reminder need only the
guild lock; never take it after a lower lock. Every participating writer must join
before enabling withdrawal; a process-local lane or one changed DELETE is not
sufficient. Save compensation must require its exact current version, rotate any
restored version, and never resurrect a response deleted/replaced by withdrawal.
The save store returns its committed `RETURNING version` with the transition;
compensation receives that exact written version, not the predecessor's version.
Mismatch or absent => no response restore/delete; the originating attempt remains
refused. Restoration omits a version assignment so the trigger creates a fresh one.
Roster erasure/prune and import writers also need this ordering, taking any
existing erasure-compatible table locks before advisory locks. Bulk restore/import
runs only with withdrawal dark and old workers drained. No new authority to run
those operations is granted; if their lock order cannot be reconciled, choose A.

Withdrawal uses one transaction: acquire those locks, check current policy and
same identity/version, remove <=1 row, insert exactly one required audit, then
commit. A newer version => stale, no deletion/audit; absent => no-op, never proof
that this request performed a prior clear. Repeated confirmations while absent
are harmless; replay after a new save is stale. No automatic mutation retry.

**Proposed RA-03 consistency choice:** preflight fresh live membership/event/channel
proof, then revalidate the same proof after the uncommitted DELETE/audit and before
COMMIT; any observed loss, lookup failure, start cutoff or changed local policy
rolls the transaction back. No post-commit resurrection/compensation for withdrawal.
This requires bounded read-only Discord I/O while transaction locks are held:
whole transaction admission deadline <=5 seconds including lock waits and reads,
each external read <=1 second, no blocked retry or lane wait inside it. On deadline,
rollback and discard the connection if safe rollback cannot be confirmed; never
claim a hard server-lock release time from a client timeout. Reserve required
executor read capacity before opening the transaction: this means a permit for
the complete fixed read sequence, held until commit/rollback, with no acquisition
inside the transaction. No such reservation seam is asserted to exist today.
Unknown/deadlocking admission keeps the feature dark. This deliberate cost/
consistency trade-off needs independent boundary review and contention tests;
if rejected, choose A rather than quietly substituting preflight-only validation
or the current post-commit helper. Existing save behavior is not weakened.
Discord changes after the last external proof cannot be made atomic with Postgres;
no promise of perfect real-time visibility is made. Failed pre-commit validation
must leave response and audit unchanged, satisfying the refusal oracle.

Both saves and successful withdrawals share the existing **20 writes per member/
guild/UTC minute** budget. Use retained save + withdraw audit actions to count it
under the same budget lock; only committed audited mutations count. A rolled-back
withdrawal consumes no budget. Absent/stale/cancel/denied/preview consume no writes.
Do not prune audit to make room. Withdrawal remains possible at the **1,000 total
responses/event** ceiling; do not treat it as a new participant. No background
sweep, queue, per-refusal audit, synthetic response or new reminder enqueue.

Reminder order is defined at **admission**, not scan or wire time:

- Withdrawal commits first => stale candidate roster/count fails revalidation;
  a fresh later scan omits the member (or skips the empty event). No guarantee
  another scheduler tick occurs before start. Unadmitted empty events retain
  existing behavior: a later explicit save can make them eligible in-window.
- Admission commits first => it owns that immutable roster/count; withdrawal may
  commit but cannot edit/cancel it. Private reply discloses possible inclusion.
- Save/clear first lock winner establishes order: save first invalidates old
  preview; clear first removes the previewed row, then a later save is a new
  response/version. Independent connections and restarts must produce this order.

Never erase/reset `event:reminder:<guild>:<event>`, `rsvp.reminder_attempt`, result
facts or any delivery/recovery evidence, even on re-save, reschedule, mirror
replacement, audit retention, member erasure or unknown outcome. Preserve NULL
system actors, whole-message fallback, <=1 attempt/event and <=2 events/tick.
No filtering-to-fit change, partial roster, extra post, ping or catch-up.

## 4. Audit, privacy and outcome contract

Atomic changed audit: `action=rsvp.withdraw`, existing guild, actor=user,
target=event; deterministic ID `rsvp:withdraw:<interaction-id>`, outcome `ok`,
constant reason `withdrawn`, timestamp only. No previous status/version, roster,
name, channel, raw payload/token or free text in metadata. Exactly one inserted
row is required; audit conflict/zero insert/failure rolls back the DELETE. It is
not a sufficient replay fence (the expected version is required too); a collision
is an additional fail-closed guard, never a reason to adopt an earlier result.
Unknown outcomes never retry the old interaction; inspect first and create a fresh
preview/confirmation with a new interaction ID if still needed. Use the existing
transaction-aware RSVP audit seam, not a second best-effort call or public mirror.

Member linkage in this audit is personal data. Preserve the **90-day RSVP /
365-day audit floors** (`crates/core/src/rsvp.rs:510-516`); current joint purge
uses the stricter 365-day cutoff (`crates/core/src/rsvp_store.rs:675-714`).
Explicit user withdrawal is a new purpose-specific response deletion, not an
audit-retention exemption: independent disposition must accept that distinction.
Apply existing actor erasure predicates and explicitly include the new action
in purge/count/erasure/restore tests before build. No new retention
exception, hashed identity marker, consent policy or indefinitely retained version.
Withdrawal removes the saved response now but not existing audit, attendance or
messages; a retained audit can still link the member to this event. Disclose this
in private preview help; never describe withdrawal as erasure. Member erasure
deletes actor-attributed audit using the existing predicate, not a new nulling or
hashing transform to preserve a budget history; it remains separate and must not
reset NULL-actor delivery fences.
Any restore/history rewind keeps withdrawal dark until old runtimes are drained,
new boot nonces invalidate old controls, versions are valid and erasure reapplied.
Unknown restore evidence means unavailable, never reuse an old nonce or fence.

Proposed internal diagnostic shape (not an agent impersonation endpoint):
`schema=rsvp.withdraw.v1`, `mode=preview|confirm`, `outcome`, `reason`,
`retryable`, `next_action`; authorized context may carry guild/event and expected
version, otherwise omit identity fields. Human replies remain plain language.

| Outcome / reason | Bounded private example | Retry / next action |
| --- | --- | --- |
| `preview / confirmation_required` | Preview in §2 | false / confirm same version or cancel |
| `saved / withdrawn` | “Your saved response was withdrawn. An already queued reminder may still include you.” | false / none; confirmed commit only |
| `no_op / absent` | “You have no saved response for this event. Nothing changed.” | false / none; authorized current read only |
| `no_op / cancelled` | “Cancelled. Nothing changed.” | false / none; no store call |
| `stale / response_changed_or_expired` | “This control is out of date. Refresh before withdrawing your response.” | false / fresh private preview |
| `denied / actor_or_context_mismatch` | “You can withdraw only your own response in this server.” | false / invoke privately as yourself |
| `unavailable / event_or_access_unavailable` | “Withdrawal is unavailable for that event. No change was saved.” | false / verify current event/access; no existence oracle |
| `unavailable / rate_limited` | “Your RSVP write limit is reached. Try again next minute.” | true / fresh preview next UTC minute |
| `unavailable / store_or_validation_failed` | “Withdrawal could not be saved. Refresh and try again.” | true / fresh preview; proven rollback only |
| `unavailable / commit_unknown` | “The withdrawal result is unconfirmed. Check your saved response before trying again.” | false / inspect personal state, never blindly retry |

Post-commit reply failure cannot undo/repeat the mutation. Lost COMMIT acknowledgement
is unknown, not a claimed rollback or success; later absence is not causal proof.
No row/status details are revealed before authorization, including on no-op.

## 5. Deterministic future scenario matrix

These are **planned oracles, not executed tests**. Fixture: guild G, member A,
other member B, Scheduled event E in visible channel C, future start, A Going
at version V; reset per row. All cases preserve attendance, other members/guilds,
event identity, delivery facts and zero public sends/DMs/pings. Disposable DBs,
independent connections/barriers, injected clocks/commit failures and the shared
Discord double only; never test on staging/production databases.

| ID | Controlled order / required outcome |
| --- | --- |
| W01 | Withdraw Going, Interested, Declined separately -> absent; only respective total -1, one audit; future roster omits A |
| W02 | Authorized absent preview; repeated confirmation after clear -> no_op, zero new audit/write; no-op does not confirm who cleared |
| W03 | DM/foreign guild/link/event, missing membership or actor/message mismatch -> denied/unavailable; no saved-state disclosure or writes |
| W04 | Bot can see C but A cannot; channel-less/thread-access-unknown/403 -> unavailable, unchanged |
| W05 | Deleted/Cancelled/Active/Completed/past event and exact start equality -> unavailable; historical totals unchanged/readable |
| W06 | Pause clear V; save (including same status) commits V2; clear V -> stale, V2 unchanged |
| W07 | Clear V commits; new save V2; replay V -> stale, V2 survives; no old audit deletion |
| W08 | A's two devices/independent connections confirm V at barrier -> one saved, one no_op; exactly one DELETE/audit; B always denied before row outcome |
| W09 | Clear commits before reminder admission of old roster/count -> no admission from stale snapshot; next eligible scan omits A |
| W10 | Reminder admission commits first, then clear -> immutable roster/count/fence; possible in-flight inclusion, no second attempt |
| W11 | Clear last attendee before admission -> empty skip; later explicit save may qualify only if no prior permanent attempt and still in-window |
| W12 | Force audit failure/collision/zero insert -> rollback with original V/response and no new audit; no success |
| W13 | Disconnect COMMIT acknowledgement -> commit_unknown; no automatic retry/compensation, inspect first |
| W14 | Fail private reply after confirmed commit -> absent + one audit, no restore; old control harmless |
| W15 | Pause before/after uncommitted deletion; remove membership/event/access or fail lookup -> rollback, no associated audit |
| W16 | Hold locks/read admission beyond bounds, move start to now, policy disable -> rollback/unavailable; <=5s transaction, no retry loop |
| W17 | 20 combined saves/withdrawals in same UTC minute -> next mutation rate-limited; new minute eligible; no-op/preview do not count; deletion allowed at event cap |
| W18 | Save compensation races clear/new save -> exact-version guard never restores over absent/newer state; every writer uses canonical lock order |
| W19 | Reschedule/mirror delete-recreate/re-save/restart after admitted or uncertain attempt -> same permanent fence, zero catch-up |
| W20 | Erase A / purge expired RSVP audit -> permitted personal rows removed, NULL-actor attempt preserved; B/G2 unchanged; stale controls cannot clear a new version |
| W21 | Restart, expired/forged/noncanonical control or old DB restore -> stale/dark; valid versions/new nonces required, no clock-rewind revival |
| W22 | Full-roster/count-only fallback and over-bound roster -> unchanged bounded one-message policy; withdrawal never manufactures extra sends |

Run now from repository root:

```sh
python3 scripts/check_docs_index.py --json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_index.py' -v
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
git diff --check
```

These validate discovery, paths and whitespace only, not W01-W22, SQL safety,
member value, deployment or authority. No new runtime tests are authored here.

## 6. Smallest future slice, acceptance gates and kill rule

One possible later slice: one private preview/confirm handler via existing router,
`PreparedRsvp`/`ActionExecutor`, new version-aware audited store deletion, shared
locks across save/compensation/reminder, and minimum additive schema/compatibility
work. Reuse existing rate limits, audit transaction and reminder executor; do not
reuse the compensation DELETE as a command. No companion UI, endpoint, reminder
preference, waitlist, role/seat, calendar, historical clear or follow-on build card.
If the version/trigger, bounded RA-03 read admission or compatibility scope cannot
fit a small reviewed slice, choose A; never cut a fence to make it fit.

Before **any build or acceptance-test allocation**, require exact commit/hash-bound
CEO intent disposition through the existing host/CEO path, actual member/event-host
alignment, and independent non-author CISO disposition of version/restore privacy,
channel authority, bounded transaction-held external reads and frozen private copy.
Roadmap, routing receipt, this author's analysis and spec merge satisfy none of
those gates. Material edits invalidate the disposition. No decision is fabricated
here; current runtime/production stay unchanged. Future build separately requires
admission, protected acceptance tests, exact-head CI and independent review.

By **2026-11-02**, seek authorized evidence of distinct value over Declined/native
behavior and explicit acceptance/revision of W01-W22 and the three coupled store
seams. No new analytics or member recruitment is authorized. Missing value, intent,
independent boundary disposition or bounded implementation => **A/no build**,
not automatic activation. Acceptance of a spec alone never creates a build task.

ACT: publish one indexed proposal and matching task spec. WHY: make absent versus
Declined explicit without reopening delivery. UNDO: reviewed docs revert only;
no runtime/data changes to reverse. Any later code rollback stops withdrawal
admission and preserves versions and every existing delivery/recovery fence.
