# RSVP: an optional registration cutoff

Status: **Proposed, design only**. Written 2026-10-26; review by **2026-11-02**.
No runtime, command, endpoint, schema, permission, migration or deployment change.
Merge records this proposal, not accepted intent, security disposition or build
approval. This indexed file is the sole editable contract; the task document
captures its exact commit and SHA-256, not an independent moving specification.

## 1. User value and no-build comparison

An organizer may need a stable list before preparing a session. An explicit
cutoff could stop new positive commitments without deleting earlier responses or
preventing a member from declining. Demand, frequency and benefit are unmeasured.
The roadmap's [Sesh manual](https://sesh.fyi/manual/) research reports Close RSVPs
Early; it is adjacent product evidence, not TWO demand or permission to import
that product's staff bypass, paid tier or organizer model. No live inspection,
member recruitment, new analytics or Discord action was performed here.

| Option | Value / cost | Disposition |
| --- | --- | --- |
| A. No build; current RSVP behavior | Zero additional policy/receipt storage; organizers coordinate manually, without pretending chat freezes writes | **Default now** and fallback if demand or safe writer fencing is insufficient |
| B. Explicit nullable cutoff on one existing event | Predictable last admission, with private refusal and continued decline; needs atomic policy/read/write fencing and replay/privacy work | Conditional recommendation for exact intent review, **not implementation permission** |
| C. Seat limits, waitlist deadlines or reminder suppression | Different allocation/delivery problems; greater state and consent surface | Excluded; do not implement through B |

B adds no event, public RSVP post, button family, dashboard, staff exemption,
host-membership concept, automatic notification or new scheduler. Use the
existing slash RSVP surface for members. A later organizer adapter must be
separately accepted and registered; the examples below are not available commands.
[Seats/waitlists](rsvp-capacity-spec.md), withdrawal and
[reminder preferences](event-reminder-preferences-spec.md) retain their own
scope and decisions. B must not depend on or silently amend those proposals.

## 2. Observed baseline and boundaries

Inspected source: `e1868d97d3519cae3f2ed91726e83e98f77dff0d`. Source evidence,
not proof of deployed behavior:

- `crates/discord/src/rsvp.rs:81-155,320-455`: private defer before execution;
  strict options; current actor membership before event lookup; one response and
  required audit transaction, then live postcheck with exact-attempt compensation.
  Unknown membership/event evidence refuses, never becomes successful admission.
- `crates/discord/src/rsvp.rs:215-232`: matching guild/event IDs and statuses
  Scheduled (1), Active (2), Completed (3) pass; Canceled (4) refuses. No cutoff.
  Completed-event behavior is **not identified as a bug** by this proposal.
- `crates/discord/src/rsvp.rs:330-367`: totals and the actor's own saved response
  are historical; no live-event lookup or write. Keep that read policy unchanged.
- `crates/discord/src/rsvp.rs:457-469`: existing staff check-in authority is
  **Manage Events**. It does not establish a cutoff editor or a fresh effective
  permission resolver. New editor authority needs current evidence, not a copied
  interaction permission snapshot or an event creator/host label.
- `crates/cutover/migrations/0160_rsvp.sql:24-34`: RSVP identity is
  `(guild_id,event_id,user_id)`, not a start timestamp or occurrence label.
- `crates/core/src/rsvp_store.rs:105-206,281-301`: event -> member-minute ->
  identity locks; audit-ID dedupe does not deduplicate the preceding upsert.
  Rate uses the caller's response timestamp; the proposed DB cutoff clock is new.
  Compensation currently takes identity lock only (`:470-644`), so extending it
  to the shared event order needs a real caller/deadlock proof, not an assertion.
  `crates/core/src/rsvp.rs:489-516` declares 1,000 rows, 20 writes/minute,
  90-day RSVP / 365-day audit floors; `rsvp_store.rs:675-714` actually enforces
  the 365-day floor for its history prune. This is not a 90-day cleanup guarantee.
- `crates/core/src/event_reminder_store.rs:15-20,38-123`: Going/Interested,
  scheduled future mirror, fixed T-15 window, two events/pass, exact recipient
  revalidation and permanent one-attempt guild/event fence. A cutoff is not a
  recipient filter or delivery cancellation.

Preserve [RSVP security requirements](command-wiring-security.md), configured
guild/application/feature/activation gates, human actor classification and
[privacy](privacy.md). No new credential, broader principal or service is proposed.

## 3. Conditional semantics: one event, explicit UTC

Key B by `(trusted guild_id, discord_scheduled_event_id)`. Rescheduling retains
identity; title, channel and start time are not keys. A different native event ID
is a different event with **no cutoff**, no copied RSVP or receipt. Never reuse
an old ID or infer recreation from a matching title. Cutoff deletion must not
reset any reminder fence. A series/template ID, attendance `{event_id}:label` or
voice instance timestamp is not a proven RSVP occurrence identity: **refuse B
for recurring/ambiguous events** until a separate native-occurrence contract is
accepted. A one-off native event is the smallest supported future scope.

Default: **no policy row and no behavior change**, including same-choice writes,
Active/Completed mutations and historical reads. A verified absent row is not an
unreadable policy. On a previously configured event, clear sets `cutoff_at=null`
and restores baseline choice/status eligibility and fresh same-choice writes,
but retains generation, receipt/erasure guards and their admission limits. It
must not turn an old refused callback into a new write. Thus clear is not removal
of the safety protocol. There is no automatic clear/reopen on reschedule/restart.

For B, setting/replacing requires a currently Scheduled one-off event, fresh
Manage Events authority and `cutoff_at <= live starts_at`. Accept a past cutoff
for immediate closure; never backdate or remove saved RSVPs. Accept only explicit
RFC 3339 instants with a numeric offset or `Z`, normalize to UTC microseconds,
reject unsupported precision, missing zone, leap-second/invalid input and
dates outside `[2000-01-01T00:00:00Z, 9999-12-31T23:59:59.999999Z]`.
No local-time parsing, timezone inference, daily rule or DST schedule is stored. Offset-equivalent instants compare equal. Render an absolute
UTC value plus Discord local time, never only a countdown.

**Admission boundary:** after acquiring the event lock and re-reading policy,
sample database UTC wall time immediately before guarded RSVP DML (not transaction
start `now()`, click receipt, cached mirror time or reply time). For a present
cutoff, positive admission requires `db_time < cutoff_at` **and Scheduled status**.
Exactly at or after cutoff refuses. Active/Completed also refuse positive
admission in B even if a session started early; with no cutoff their baseline
behavior is unchanged. Canceled/missing/foreign events retain existing refusals.
The deadline is the admission instant, **not** a promise that the commit, external
postcheck or private success reply precedes that instant. A pre-deadline admitted
write may settle later, subject to the existing postcheck/compensation contract.
A queued click whose admission occurs at the deadline is too late.

Set/replace is Scheduled-only; clear may restore baseline on Scheduled, Active
or Completed after current authority/event validation. Do not configure/clear a
missing or canceled event. Rescheduling leaves the absolute cutoff unchanged:
move later does not extend it; move earlier does not silently rewrite it. If the
new start precedes the saved cutoff, positive admission also stops when Active
is observed; do not claim a scheduled timestamp alone closes this design. Show
that mismatch to the organizer for an explicit replace/clear. No time change
rewrites history or resurrects a declined/erased response. Whether this exact
start-early/clear behavior is wanted is an explicit intent question, not consent.

### Existing rows after closure

After all existing membership/live-event gates, apply this B-only transition table
under the same event lock. Same-choice no-op does not refresh `responded_at`,
consume another write budget or append audit. Every actual RSVP change, including
Declined and Going -> Interested, retains the existing budgets and atomic audit.

| Saved response | Requested response after closure | Result |
| --- | --- | --- |
| Absent | Going / Interested | `REGISTRATION_CLOSED`, no insert |
| Declined | Going / Interested | `REGISTRATION_CLOSED`, keep Declined |
| Interested | Going | `REGISTRATION_CLOSED`, keep Interested |
| Going | Interested | Allowed de-escalation; save Interested, no seat/allocation promise |
| Any existing choice | Same choice | `UNCHANGED`, exact saved row/audit untouched |
| Absent / Going / Interested | Declined | Allowed ordinary Declined write, subject to current row/rate limits |

Before closure, ordinary changed choices are allowed; B same-choice requests are
still no-ops. No staff bypass applies, including to the organizer's own Going.
Declined is a saved answer, **not deletion/withdrawal**. A separate accepted
withdrawal action, if implemented later, must not be blocked merely by cutoff;
this proposal neither supplies it nor relaxes its membership/event/privacy gates.
The existing live-event gate can still refuse Declined for a deleted/canceled
event. Historical totals/own saved answer stay readable under their existing
policy; never add a roster, names or a live-event existence oracle.

## 4. Future serialization, authority and replay contract

These are missing capabilities to prove, not claims about today's store.

1. Preserve existing acknowledgement and router gates. Derive guild/actor from
   authenticated context, never caller JSON. Organizer inspect/preview/save/clear
   requires the existing Manage Events bit, freshly resolved for that guild at
   preview and again after queue admission, immediately before save. No creator,
   event-host role, Administrator-only alternative or target-member parameter.
   Resolve effective authority from trusted current role/member/guild evidence;
   unknown/partial/403/429/timeout means `UNAVAILABLE`, not a stale grant. Bound
   each validation phase to 5 seconds and <=6 paced read-only Discord calls;
   no retry loop, probe write or permission repair. An observed authority/config
   change invalidates the preview. No DB lock spans HTTP. External permission
   revocation after the last read can race SQL; disclose this residual race for
   independent disposition, never promise atomic Discord/Postgres authorization.
2. Preview is private and read-only: exact event ID, policy revision, old/new
   cutoff, explicit baseline restoration on clear, status and expiry. A save binds
   the same actor/event/revision/normalized input with a 5-minute integrity-bound
   intent. Dry-run does not mint that proof. Revalidate event identity/status/start
   and current authority on confirmation; changed start/status requires preview
   again, even if the requested cutoff would still fit. Time passage alone does
   not invalidate an otherwise current preview: it confirms that exact absolute
   instant, possibly now past. Preview must warn that saving after that instant
   closes immediately; never silently shift it into the future.
3. Use a nullable policy row with monotonic revision, not deletion on clear.
   All cutoff-aware writers, policy changes and compensation use the existing
   event advisory lock/order. Compare expected policy revision under lock; edits
   increment it, including clear, so A -> B -> A is not invisible. Member admission
   reads current policy in that transaction; no separate cached check then upsert.
   Edit first -> next RSVP observes new policy. RSVP admission first -> its earlier
   admitted answer is preserved; an edit never retrospectively deletes it. Two
   edits from one revision -> at most one save. Unknown policy/storage/time state
   means no mutation, including Declined (do not invent an outage bypass).
4. RSVP pre/post live checks and fenced compensation remain mandatory. The DB
   policy decision is not a replacement for membership/event revalidation. The
   postcheck for B must reject Active/Completed for **positive** attempted changes;
   it must not apply that new positive rule to Declined/de-escalation/no-op or
   absent-cutoff baseline. Time passing C alone after admission does not trigger
   compensation; a live postcheck observing Active/Completed for a present-cutoff
   positive change does. Thus R03 succeeds only while that postcheck remains valid.
   A cutoff edited after admission is not retroactive;
   compensation must restore only the exact attempted predecessor, never overwrite
   a newer response or policy. A postcheck/store/compensation failure is never
   reported as saved. Receipt remains pending until postcheck/fenced settlement;
   a duplicate pending operation may inspect/recover it but cannot repeat DML or
   return success. Settlement records the actual validated/refused outcome; an
   unresolved compensation keeps it unavailable, not a successful receipt.
   No success merely because the audit INSERT deduplicated.
5. Replayed **member** interaction: deduplicate the authenticated interaction ID
   and canonical action binding **before any RSVP upsert**, using an atomic
   operation receipt, not current audit-ID dedupe after the write. Same ID/same
   input returns a reauthorized historical result, marked replayed; never refresh
   row time, consume budget, resurrect a choice or write a second audit. Same
   ID/different input refuses `ACTION_CONFLICT`. Record completed refusals as well
   as successes; a refused Going cannot become a fresh admission after clear.
   A new explicit interaction may retry. Unknown commit outcome -> inspect the
   same operation, never resubmit a different key as recovery.
6. Organizer receipts similarly bind actor, event, expected revision, set/clear,
   normalized cutoff and confirmed event facts. Look up a matching receipt after
   reauthorization **before** revision CAS; a saved edit already advanced revision.
   Conflicting binding refuses. A fresh same-value edit is `UNCHANGED`, not proof
   of an earlier save. No response may present a historical replay as current
   authority or as a currently open registration policy.

Keep receipts at most 24 hours; admit new interactions only within a 15-minute
age / 30-second future-skew envelope and confirmations within 5 minutes. Expired
or pruned receipt keys never become fresh mutations: return `INTENT_EXPIRED`.
Before build, specify integrity-bound original issuance/expiry proof so a caller
cannot revive an expired ID by changing its timestamp. Restart discards cached
proofs; unavailable/unhealthy clock proof means no B mutations. DB UTC evaluates
cutoff; monotonic elapsed time bounds proof age/HTTP/lock waits. UTC/DST display
changes do not extend admission. No claim that an unhealthy wall clock is safe.

Full erasure must delete subject receipts and audit data per the privacy manifest
and invalidate outstanding proofs, including **unseen delayed interactions**.
Proposed non-subject fence: under erasure-compatible table locks, advance every
configured event's `action_not_before` to DB UTC +30 seconds; all B actions check
it before receipt lookup and reject older/equal issuance times. At most 1,000
policy rows/guild are invalidated, with no retained erased-member ID/hash.
Order after authorization: event lock -> current policy and action fence ->
receipt lookup -> replay/CAS/choice checks -> budget/cap -> fresh cutoff-time
sample -> DML. The fence applies to all managed-event actions, including clear,
Declined and no-op; no subject row/receipt lookup precedes it. Never-configured
events keep their existing baseline. This may briefly require unrelated members to make a fresh explicit request; record
that tradeoff for independent CISO/intent disposition. Dry-run erasure writes
nothing. Both manifest erasure executors, retention and restore must honor it.
If this cannot preserve erasure and replay safely, choose **A**, not a new PII
retention exception. Never repurpose analytics opt-out or reminder fences.

## 5. Persistence, compatibility, bounds and reminder independence

Proposed storage only: one policy row/event (`cutoff_at`, monotonic revision,
non-subject action fence), bounded operation receipts and minimal private audit
(old/new cutoff, action, actor, event, revision, operation ID/time). No attendee
snapshot, event title, raw permission payload, interaction token, free-form reason,
public audit mirror or new member preference. New actor-attributed policy audit
has an explicit 365-day maximum (extend retention action coverage); receipts
expire within 24 hours after settlement, subject erasure acts immediately.
Pending-operation recovery state is not pruned as if settled: reconcile or keep
B dark and request disposition if it cannot meet that retention contract.
Policy revision/fence has no subject field and is retained while its native event
exists, even when cutoff is cleared. Never expire it at the deadline or by idle
time and reopen an old event. Purge only after confirmed native deletion, all
proof/receipt expiry and >=365 days since the last associated RSVP/policy audit;
that ID can never be reused, so no old revision can address a replacement event.
At the 1,000-row cap refuse new configuration, not recycle live policy history.
This bounded-count, potentially long event-ID retention is an explicit CISO
question, not an approved exception. Never delete permanent delivery fences.
Subject receipts/audits participate in export, erasure, guild purge and backup;
restore preserves revisions/fences or leaves B writes dark pending reconciliation.
Missing/unreadable schema is unavailable, never an absent-cutoff fallback.

Keep **1,000 total RSVP rows/event**, **20 actual writes/member/guild/UTC minute**
and current rate-before-row-cap ordering. B never evicts rows or reserves Going
seats. Proposed additional hard bounds: <=1,000 retained policies/guild;
<=20 editor attempts/actor/guild/minute; <=50,000 retained receipts/guild and
<=1,000/event, with no eviction of live replay guards to make room. At receipt
capacity refuse new B actions `SAFETY_LIMIT`; a no-write read remains available.
A full receipt store cannot remember another refusal: atomically advance that
event's non-subject `action_not_before` to DB UTC +30 seconds before replying,
so the unrecorded old operation cannot execute after space frees. This has the
same fresh-click tradeoff as erasure; never evict a guard or claim a remembered
outcome. If even that fence cannot persist, keep B writes unavailable through
the intake/proof expiry window and reconcile before reopening. This can delay
Declined, just as existing rate/storage limits can; do not falsely report success.
New explicit actions can retry after eligible cleanup and the action fence. Point
reads are O(1), authorized pages <=25, cleanup <=25 expired rows/pass in existing
retention machinery, no whole-member scan or perpetual per-event timer. Use
existing stricter pacing/timeout limits; retries count against the stated budgets.

At assumed <=1 KiB/indexed receipt, cap costs <=50 MiB/guild plus <=1 MiB policy
storage, excluding WAL/backups/audit. These are unmeasured sizing assumptions,
not authorized spend or measured headroom. Measure in disposable fixtures before
any activation; reduce bounds or choose A rather than purchase capacity.

Every local/direct/shared-database writer must be inventoried before enabling B.
Known local seams: both `rsvp_store.rs` upsert wrappers, compensation/pruning;
`crates/cutover/src/member_erasure.rs:256-334`, generic deletes in
`crates/cutover/src/retention.rs:1230-1242,1442-1453`, backup restore in
`crates/core/src/backup/dump.rs:617-647`, and rollback import in
`crates/cutover/src/rollback_import.rs:447-464`. This list is not a complete
cross-repository proof. An unchanged writer can bypass cutoff; keep B dark if
any writer cannot be fenced or safely excluded.

**Reminder contract is unchanged.** Closing/clearing cutoff neither changes RSVP
rows nor filters existing Going/Interested. An allowed Declined before reminder
admission changes its ordinary eligible array, so a stale array must fail current
atomic revalidation without consuming an attempt. Declined after admission cannot
recall a message already in flight. Keep fixed T-15, <=2 events/pass, one automatic
attempt/guild/event, whole-message 2,000 UTF-16 limit, closed allowed mentions and
10,000 retained fences/guild. Unknown commit/render/send result never authorizes
repost; no reset on cutoff edit, reopen, reschedule, mirror recreation or erasure.
No DM, new ping, catch-up, deferred reminder, automatic attendance, voice action
or delivery guarantee. A reminder does not prove registration is still open.

## 6. Private UX and future agent results

All control/RSVP replies remain ephemeral; no public fallback or new messages.
Plain text, <=500 UTF-16 units, escaped identifiers, parsed/replied-user mentions
off. No event title, other member identity, roster or raw upstream/SQL errors.
Exact synthetic examples (authorized context only):

- Allowed: `Registration cutoff saved for event 200: 2026-11-02 18:00 UTC. Existing responses remain. No reminder changed.`
- Allowed member de-escalation: `RSVP saved: Interested. Registration is closed; choosing Going again is not available.`
- Closed: `Registration is closed. Your saved response was not changed. You can still choose Declined, subject to the usual checks.`
- Unchanged: `Your saved response is already Going. It was not changed.`
- Clear preview: `Clear the cutoff for event 200? This restores the usual RSVP rules, including Active and Completed events. No reminder will be resent.`
- Denied/missing/foreign: `This event or action is not available in this context.`
- Unknown policy/authority/store: `Unable to verify registration policy or authority. Nothing is confirmed saved. Try a fresh preview when available.`
- Unknown commit: `Save outcome unknown. Inspect this same operation; do not submit a new save as recovery.`

Illustrative bounded JSON, **not a live endpoint or client-supplied authority**:

```json
{"schema":"rsvp.cutoff.v1","code":"PREVIEW","reason":"ELIGIBLE","retryable":false,"next_action":"confirm_exact_preview","guild_id":"100","event_id":"200","policy_revision":"7","operation_id":"cutoff-1","cutoff_at":"2026-11-02T18:00:00Z","dry_run":true,"mutated":false}
{"schema":"rsvp.cutoff.v1","code":"REFUSED","reason":"REGISTRATION_CLOSED","retryable":false,"next_action":"keep_response_or_decline","guild_id":"100","event_id":"200","policy_revision":"8","operation_id":"rsvp-1","mutated":false}
{"schema":"rsvp.cutoff.v1","code":"UNAVAILABLE","reason":"POLICY_UNKNOWN","retryable":true,"next_action":"retry_fresh_preview_when_available","mutated":false}
```

IDs/revisions are canonical bounded strings (native Discord snowflakes, unsigned
revision <= signed bigint maximum, operation ID <=64 ASCII characters). Input
rejects unknown/duplicate keys, actor overrides, batches and ambiguous timestamps;
output <=2 KiB, no nested unbounded data. Denied/unknown-authority replies omit
IDs, revisions, saved choice and cutoff even when the caller supplied selectors.
Codes/reasons/next actions form this closed vocabulary; adapter failures exit
nonzero, without inventing today's HTTP statuses:

| code / reason | retryable / next_action |
| --- | --- |
| `PREVIEW / ELIGIBLE` | false / `confirm_exact_preview` |
| `SAVED / APPLIED`, `UNCHANGED / SAME_VALUE` | false / `none` |
| `REFUSED / REGISTRATION_CLOSED` | false / `keep_response_or_decline` |
| `REFUSED / NOT_AVAILABLE` | false / `check_authorized_context` |
| `REFUSED / INVALID_INPUT`, `UNSUPPORTED_OCCURRENCE` | false / `fix_input_or_keep_baseline` |
| `REFUSED / CONFLICT`, `INTENT_EXPIRED` | false / `reread_then_new_preview` |
| `REFUSED / ACTION_CONFLICT` (member RSVP interaction) | false / `retry_new_explicit_interaction` |
| `REFUSED / RATE_LIMITED`, `SAFETY_LIMIT` | true / `retry_new_action_after_server_delay` |
| `UNAVAILABLE / POLICY_UNKNOWN`, `AUTHORITY_UNKNOWN`, `EVENT_UNKNOWN`, `CLOCK_UNKNOWN`, `STORE_UNKNOWN` | true / `retry_fresh_preview_when_available` |
| `UNKNOWN / COMMIT_UNKNOWN` | false / `inspect_same_operation_no_new_save` |

`mutated` describes RSVP/cutoff business state, not operational receipt or replay
fence bookkeeping. Those writes remain atomic and private. A fence-expired
operation returns `INTENT_EXPIRED`, not a cached historical success.
A replay response additionally carries `replayed:true`, original constant outcome
and an authorized **current** policy revision/state if readable; unknown current
state is explicit, never inferred from an old receipt. Reauthorization precedes
receipt disclosure. `mutated` on replay describes this call (false), not the first
call. Unknown commit uses `mutated:null`, not false/rollback. Offline dry-run is a
deterministic decision over synthetic/already authorized snapshots: no DB/Discord
I/O, audit, receipt, proof, enqueue or save. It grants no live permission and has
no production/apply flag. Do not ship a live mutation adapter as part of this doc.

## 7. Deterministic future scenario matrix and runnable docs checks

These are design oracles, **not executed product tests**. A future Test Author
supplies protected acceptance fixtures only after intent/security disposition.
Use injected DB time/REST, independent disposable pools and barriers, not sleeps,
real member data, staging/production DB tests or live RSVP actions.

| ID | Inputs/order | Expected oracle |
| --- | --- | --- |
| R01 | Policy absent; Scheduled/Active/Completed; each choice | Baseline behavior including actual same-choice write unchanged |
| R02 | B; C-1 microsecond / C / C+1 microsecond | Positive admission allowed / closed / closed; queued-before-C but admitted-at-C closed |
| R03 | C-1 admission; commit/postcheck/reply after C | May succeed after normal postcheck; no admission-time rewrite or claim reply was before C |
| R04 | All 3 saved choices + absent, all 3 requested choices, closed | Full §3 table: positive upgrade/insert refused, Going->Interested allowed, Declined allowed, same choice exact no-op |
| R05 | B; early Active/Completed versus no cutoff | B positive denied, reductions allowed after normal gates; baseline unchanged |
| R06 | Member/organizer lost authority or stale preview; unknown/foreign event | Zero mutation; no receipt/state disclosure on failed authority; unknown proof unavailable |
| R07 | Editor wins lock / RSVP admission wins lock; two editors | New cutoff observed / earlier answer preserved; one matching-revision edit only |
| R08 | Same callback after newer response or clear; changed binding | No repeated upsert/audit/budget; prior refusal stays historical; mismatch conflicts |
| R09 | Set A->B->A; stale revision; clear on Completed | Stale edit refuses; explicit current clear restores baseline, never resets reminder |
| R10 | Reschedule earlier/later, native deletion/recreation, recurring ID | Absolute C unchanged; mismatch disclosed; no copied policy/history; ambiguous recurrence refused |
| R11 | Equivalent offsets, DST transitions, unknown clock, long lock wait | Same UTC boundary; no local rule; unhealthy/expired proof refuses; time sampled after locks |
| R12 | Audit/transaction failure, lost commit receipt, restart | Atomic rollback or unknown; same-operation inspection only; no guessed success/re-execution |
| R13 | Live member/event changes pre/post write; concurrent newer response | Existing compensation fences retained; no success on invalid positive postcheck; no stale overwrite |
| R14 | Declined before / after reminder admission; close/reopen | Fresh recipients / disclosed in-flight limit; permanent fence and all volume/mention limits unchanged |
| R15 | Erasure during proof, unseen delayed action, receipt TTL, restore | No erased-subject resurrection; event action fence/proof expiry wins; no identity tombstone; dry-run zero writes |
| R16 | Each exact cap/cap+1; same-choice at cap; unavailable policy | No overflow/guard eviction, business no-op writes zero, no missing-policy fallback; receipt-cap refusal fences replay after cleanup; actual reductions remain budgeted |

Run now from the repo root:

```sh
python3 scripts/check_docs_index.py --json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_index.py' -v
git diff --check
```

No new reducer, acceptance test target or runtime check exists. Passing these
checks proves document indexing/path/format conformance, **not cutoff behavior**.

## 8. Smallest later slice, decisions and stop rule

Smallest conditional build slice: **dark policy/store admission contract only**,
covering existing event lock, nullable cutoff/revision, UTC boundary/transition
reducer, atomic receipts/audit, compensation and privacy/retention manifest seams.
No editor, runtime activation, shared-writer enablement or deployment. One PR,
<=one engineering day excluding protected-test/CI/review latency; if the real
writer/privacy inventory cannot fit, recut before authorization, not halfway
through an unsafe activation. Existing unchanged RSVP tests are not proof of B.
No build, test-author or follow-on implementation cards are created by this spec.

Exact-revision CEO/host alignment is **not obtained**. Route the decision brief
through the existing CEO/host intent path, never directly to the owner. Ask for
accept/change/reject on B versus A, evidence of organizer value, Scheduled-only
positive admission when configured, late de-escalation/no-op/Declined semantics,
explicit clear restoring Active/Completed baseline, fixed UTC on reschedule,
one-off-only scope and unavailable/cap behavior. Queue/intake receipt, spec merge,
ordinary PR review or silence is not aligned intent.

Independent non-author CISO disposition is **not obtained** for fresh authority
proof/residual remote race, new subject-bearing operation receipt retention and
erasure-wide replay fence. The author cannot accept that boundary. Record exact
file commit/hash and task-document revision for both paths; changed intent needs
fresh disposition. No new privilege, credential, routine approval hop or spend.
Later authorized code still needs test-first protected fixtures, exact-head
required checks and independent Paperclip Review 5/5, controller-managed squash
auto-merge and normal staging/health/rollback gates; never a manual merge bypass.

Review by 2026-11-02: metric is 16/16 scenario rows with agreed future assertions
and explicit intent/security disposition, not registrations or measured demand.
Missing useful value, unavailable intent route, unsafe replay/privacy policy or
unfenced writer -> **A/no build**. Do not relax a fence to keep B alive. Unverified:
actual demand, deployed state, complete effective-authority resolver, external
writers, native recurrence mapping and receipt/proof implementation/headroom.

ACT: publish one indexed Proposed contract. WHY: separate registration timing
from seats and delivery while preserving history and honest failure boundaries.
UNDO: reviewed documentation revert only; no live state changed.
