# Event RSVP: optional seats and an opt-in waitlist

Status: **proposed, design only**. Written 2026-10-23; review by 2026-10-30.
No runtime, command, test, migration, permission or configuration changes are
included. Merging this document does not enable seats or authorize build work.
This file is the repository source of truth; review documents bind its commit
and SHA-256 rather than maintaining a second independently editable contract.

## 1. Outcome, evidence and decision boundary

A member can tell whether they hold a Going place, are explicitly waiting for
one, or have neither. An organizer can bound Going places on an existing event
without manually selecting a replacement after a cancellation.
[sesh](https://sesh.fyi/) advertises attendee limits and waitlists. That is a
vendor capability claim, not evidence of TWO demand, retention or conversion.
No member experiment, outreach, native Discord comparison or live inspection
was conducted for this proposal. Native event interest is not asserted to be
a guaranteed seat. Before building, check whether native behavior already meets
the actual need; stop/defer if it does or no distinct member value survives.

Recommendation: extend the existing event RSVP identity and writes, not LFG,
a generic queue service or a second attendee store. Keep unlimited organizer
capacity by default, inside **all existing system safety limits**. Offer an
explicit event-wide FIFO waitlist only when a finite capacity is enabled.
Alternatives rejected: silently waitlisting a failed Going click (no consent),
lottery/manual priority (unfair or organizer work), and DM offers with claim
expiry (new delivery dependency and substantially more states).

CEO/spec review must accept the assumptions in §11 and the proposed cuts.
Then the existing host intent-alignment path must record alignment against the
accepted revision. **No implementation or acceptance-test cards before both.**
A saved queue/intake receipt alone is not alignment. This design neither creates
a host executor nor widens authority if that route is unavailable. Independent
security review, exact-head CI/review and normal release gates still apply later;
no owner step, new spend, production enablement or routine approval hop is added.

## 2. Existing contracts and scope

Baseline: `90e57c7a965faa59d0d3cbddafe0e63dc8da6c0c`.

- `crates/core/src/rsvp.rs:36-42,429-456`: Going, Interested and Declined;
  **1,000 rows/event across all choices**, **20 writes/member/guild/UTC minute**.
  `crates/cutover/migrations/0160_rsvp.sql:24-34` keys `event_rsvps` by
  `(guild_id,event_id,user_id)`; there is no occurrence key or event FK/cascade.
  These are safety bounds, **not** Going seat capacity. Keep check-ins separate.
- `crates/core/src/rsvp_store.rs:78-206,470-493`: event → user-minute budget →
  full identity locks; rate refusal precedes row-cap refusal; existing-row
  updates remain allowed at the cap. Same-choice legacy writes still update
  time/audit. Atomic response/audit and fenced predecessor compensation exist;
  audit-ID deduplication alone does **not** deduplicate the preceding upsert.
- `crates/core/src/router.rs:638-674`: ordinary RSVP/totals are member commands
  behind Announcements; staff check-in separately requires Manage Events and
  Scorecard. `crates/discord/src/rsvp.rs:158-239,298-389` checks live actor
  membership before event lookup, and rechecks after commit with compensation.
  Scheduled, Active and Completed currently pass; Canceled refuses. The new
  start cutoff applies only to seat mode, not historical legacy RSVP/totals.
  Preserve `docs/command-wiring-security.md`; custom IDs grant no authority.
- [Attendee reminders](event-reminders.md) and merged
  [reminder PR](https://github.com/TogetherWeOwn/two-bot-next/pull/1649): reuse
  their channel-only, mention-safe, durable delivery contract, not a DM path.
- Event voice remains separately owned: T-15 channel creation, category
  preflight, occupied-room/after-party handoff and ownership are not seat
  allocation. A seat neither grants CONNECT nor creates/kicks/moves anyone.
- Per-role LFG standby and game-night availability finding remain separate
  designs. Reuse explicit consent, ordered locks, FIFO sequence, fresh
  eligibility and replay/erasure invariants; do not share queues, role slots,
  votes, consent or deadlines. No running sibling is changed by this spec.

**Premise gap:** the inspected RSVP path defers an ephemeral reply
(`crates/discord/src/rsvp.rs:104-119`); no durable public RSVP-post binding was
established. Existing-event support is proven; existing bound-post support is
not. This is a review assumption/possible defer, not a delivered post feature.
Only existing scheduled events and their existing RSVP surface are in scope.
Do not create another event, announcement, dashboard, calendar, slash-command
family or website write path. A durable bot-owned message binding is required
before attaching components to any post; an arbitrary event link or user
message is not a binding. If no such existing post can be established, return
`POST_UNBOUND` and keep controls dark; resolve that premise at review, not by
silently expanding this feature into an event-posting project.

## 3. Member and organizer behavior

Event identity is `(guild_id, discord_scheduled_event_id)`; member identity is
that event key plus `discord_user_id`, never name, title, slug or start time.
Reschedules preserve identity. Distinct recurring occurrences must have distinct
native occurrence identities; an ambiguous series/template ID is unsupported,
not permission to share seats across occurrences. Source message identity is
`(guild_id, channel_id, message_id)` bound to that same event and bot/application.
Multiple valid views share **one** allocation, not separate capacity per post.

A finite capacity is an integer `1..=1,000` (the current all-answer row cap);
`null` is unlimited organizer capacity. Configuration requires the existing
**Manage Events** permission check, freshly evaluated for this guild, plus a
matching live event and valid post binding. Event creation/ownership alone is
not authorization. This reuses the current staff-event permission boundary,
not a new organizer role or an event-creator exception.
Enabling on an existing event counts existing Going rows as held; reject a
capacity below that count. Never evict or silently demote existing attendees.
Capacity decrease below held count is rejected; increase schedules FIFO drain.
Disabling a finite limit is allowed only with an empty waitlist; otherwise
return `WAITLIST_NOT_EMPTY`, with increase/cancel/close as explicit alternatives.
An edit/reschedule never silently enrolls or changes FIFO priority. The explicit
lifecycle exceptions below can expire/close waiting; show that result through
the existing post and My status, without a new ping or re-consent workflow.

| Action | Durable result |
| --- | --- |
| Going, unlimited | Existing Going behavior within safety limits; no waiting enrollment |
| Going, finite and vacancy with no earlier waiter | Going/held after live authorization and atomic allocation |
| Going, full or an older waiter has reserved the vacancy | No change; explain no seat and offer **Join waitlist** explicitly |
| Join waitlist | Explicit consent to automatic Going promotion before the deadline; save Interested/waiting with a new sequence, then drain in FIFO order; if the queue is empty and a seat is free, this may immediately become Going |
| Repeat Going while held / repeat Join while waiting | No-op; no new sequence, audit transition or promotion |
| Interested | No seat and no queue entry; release held place or cancel waiting, then drain if needed |
| Declined | Declined/no seat/no queue; release held place or cancel waiting, then drain if needed |
| Cancel waitlist | Interested/no seat; repeat is a no-op; joining again goes to the tail |
| Cancel Going place | Interested/no seat; explicitly distinguish this from Declined |
| My status | Ephemeral authoritative own answer/seat/wait state, deadline and current position; never another member's queue details |

One RSVP row carries exactly one answer and one allocation state: `Going/held`,
`Interested/none`, `Interested/waiting`, or `Declined/none`; absent means no RSVP.
No `Going/waiting`, `Declined/waiting` or second active row is legal. Keep the
three existing choice values; waiting is an explicit allocation state, not a
new synonym for Interested. Legacy reads see waiting as Interested, never Going.
The actor can change their mind without organizer approval. A successful
self-cancel must not require new cursor/quota allocation or event visibility.
The named fallback is existing `/rsvp event-id:<known-id> status:interested`
(or `declined`), after common configured-guild/authenticated-actor/current-member
gates. For an existing seat-mode row, this is a **withdraw-only** branch before
live event/access lookup: remove only the actor's seat/wait fields, update choice
on that existing row and apply ordinary budgets. It never inserts a new RSVP,
reveals event/current-state data or grants access. Unknown/unbound/foreign or
absent own row produces the same ephemeral `No seat or waitlist entry is kept
by this request` (UNCHANGED); unauthenticated/departed/unknown membership fails
the existing membership gate without a row lookup/disclosure. Positive enrollment
and My status still need full live event/access gates. Component Cancel uses
this same reducer after validating its persisted binding. Pending-slot/rate
refusal is retryable, not false success. Admission flag-off preserves withdrawal;
this is not a broader enrollment or event-read permission.

### Lifecycle

Seat admission and promotion close irreversibly at the first observed ACTIVE,
COMPLETED, CANCELED or deletion, or at DB time `>= starts_at`, whichever comes
first. Close reason is `started | completed | canceled | deleted | expired`.
Existing held/ordinary RSVP history and check-in retention remain unchanged;
waiting entries are removed, and no later worker may promote. End/cancel does
not falsify historical Going as Declined. An ended event displays no current
seat guarantee. Existing legacy RSVP behavior outside seat mode is unchanged.

Waiting consent expires at `min(current starts_at, queued_at + 7 days)`. Show
that deadline when joining. A move earlier shortens it; moving later never
extends existing consent. A start moved into the past closes immediately. Once
allocation is closed, moving the event later does not reopen it; a new event
is needed. Expired waiters become ordinary Interested, and any rejoin before
closure gets a fresh sequence/consent. No auto-requeue after access loss.

Confirmed member departure or loss of required channel/event access removes a
waiting entry after matching-revision validation; next eligible waiter may run.
Unknown access preserves position and reserves the vacancy until eligibility
recovers or the already defined start/consent deadline closes it. Backoff caps
load, not wait duration; expose a bounded `ELIGIBILITY_UNKNOWN` diagnostic for
staff to repair access or explicitly close the event. Never evict by retry count. Confirmed loss for
a held member, when observed by existing lifecycle reconciliation, releases the
seat to Interested and schedules drain; unobserved loss is not continuously
policed, and no voice participant is disconnected. A later new Going request
must validate access afresh. Erasure removes the RSVP instead of retaining an
Interested subject. Missing/deleted posts suppress projection, not seat truth;
no replacement announcement is created automatically.

## 4. Data and serialization contract (proposed, not implemented)

Extend the existing `event_rsvps` identity, not a competing member store.
Add allocation state, nullable `wait_seq`, `queued_at`, `wait_expires_at` and a
row revision, with checks for exactly the four combinations in §3. Existing
Going rows become held; other rows have no queue data. A unique event/sequence
index and partial FIFO index prevent ties. `wait_seq` is a monotonically
increasing bigint allocated under the event lock, not a timestamp/member ID;
gaps are harmless. Rejoin after cancel/expiry/new consent receives a new value.

Add one event capacity row keyed by the native guild/event key: nullable limit,
`next_wait_seq`, `revision`, terminal status/reason, last authoritative start,
non-subject `action_not_before` and policy generation. Hold proposed `event_rsvp_post_bindings`
separately, at most **3 per event**: native event key, application/bot author,
channel/message IDs, binding revision and 404 tombstone. Populate only from a
verified existing bot-owned post receipt, never a user-supplied message ID or
newly manufactured announcement. Projection owns binding tombstones. This table
is proposed, not evidence that an existing producer/receipt has been found. Add one guild allocation-policy
row, missing/default means dark. Disabling admission/promotion increments its
generation atomically; self-cancel, closure, expiry and reads remain available.
A rollout must fence every writer of enabled events, including direct store,
internal action and shared-database consumers. If an unchanged writer can still
create Going without allocation, **do not enable**. Do not assert cross-repo
compatibility without enumerating and proving the real writers.

Local writer inventory to cover before enabling:

- `rsvp_store.rs:68,91,195`: `put_rsvp`, `put_rsvp_with_audit` and their shared
  upsert; `discord/src/rsvp.rs:260,338` is the verified runtime producer.
- `rsvp_store.rs:555-569`: compensation UPDATE/DELETE; preserve predecessor and
  newer-write fences in the event-slot protocol, not just ordinary upserts.
- `rsvp_store.rs:699`: `prune_rsvp_history` deletes; apply existing retention
  without bypassing per-event serialization/reservations, and mark any surviving
  event's vacancy/projection work only after cleanup commits.
- `crates/cutover/src/member_erasure.rs:235-313`: `erase_member`, manifest-driven
  deletion; `crates/cutover/src/retention.rs:1165-1209`: `complete_request`, used
  by `run_retention`/`fulfill_user_deletion`. These remain first-class writers.
- Internal-action reachability and external shared-DB writers are **not verified**
  by this inventory. S1/S4 must trace/prove them or exclude enabled-event writes;
  the assigned repository is not evidence about another repository's behavior.

Add bounded per-event/member action cursors (latest authenticated interaction
snowflake, action fingerprint, constant result code, updated time), and one
coalescing work row/event (dirty/drain flag, desired/projected revisions,
next-attempt/verification time, lease generation, `repair_until`). Work contains
no member list, interaction token, message body or notification history. No
process-local queue or gateway ordering is the durable authority.

Enabled writers take erasure-compatible sorted table locks, new guild allocation
quota lock, then **existing event → user-minute-budget → full identity** order.
All direct/compensation writers must obey that order; no event writer may acquire
the new guild lock after its event lock. Inventory/test actual callers before
activation. Preserve rate-before-row-cap refusal and the existing audit ledger;
automatic promotion is the consenting waiter's RSVP write: audit
`action=event.rsvp`, `actor_id=that user_id`, with bounded `seat_promoted` reason
marking automatic initiation, not falsely claiming a fresh human click. It
consumes that member's same minute budget. At 20 writes, preserve head/vacancy
and retry no earlier than the next UTC-minute boundary, still before consent
expiry; a deferral writes no RSVP audit. This explicit <=one-minute quota delay
is not unknown-access eviction or permission for a later waiter to overtake.
Worker audit ID is deterministic per accepted enrollment sequence, e.g.
`rsvp:seat:<guild>:<event>:<wait_seq>`; recovery reuses the slot/ID and never
manufactures a Discord interaction. Do not count new action labels instead
and bypass the existing ledger. Bounded transition reason is metadata
on the existing privacy-scoped audit, not a forever subject history. Only the
new seat-mode duplicate/no-op path avoids another write/audit; legacy is unchanged.

Capacity, queue/cursor, state, required audit and work commit atomically; unknown
commit outcome is resolved from durable operation/cursor/state, not blind retry.
**Preserve the existing post-write recheck/compensation gate**, including for
automatic promotion. An event has at most one durable pending validation slot
(operation ID, actor, exact predecessor/attempt revisions and queue fields).
**The slot alone is the reservation representation**, not an independent counter
or second membership. Occupancy is `held rows excluding the slot actor +
max(predecessor_is_held, attempted_is_held)`, with each boolean 0/1. With no slot,
use ordinary held count. All occupancy readers use that same rule. Restore only
the matching attempt; clear the slot once, atomically with finalize/compensate.
No drain or other seat mutation can start another pending operation for that
event while a slot exists (return `RETRY_LATER`); overlapping holder-release and
waiter-promotion slots are therefore impossible. Queue/read projection reports
the predecessor and pending result, never provisional success or a provisional
vacancy. Same-operation recovery is the only permitted resolver. Outside SQL,
perform the existing live postcheck; finalize only on success plus matching
operation/revisions/generation and fresh DB-time open/deadline checks, otherwise restore
the exact predecessor and remove only that attempt's audit under the same locks.
Restoration cannot overbook because the reservation was never spendable. A
restarted worker resolves the slot before other event work; a failed compensation
keeps its reservation and reports failure, never succeeds or retries the effect.
Finalize is the success linearization point, not the provisional SQL write.
Close may preempt a slot: restore the matching predecessor/audit first under
locks, then apply closure/queue expiry in that transaction and advance generation.
Erasure instead deletes the slot/subject and advances the cutoff; it never
restores the erased predecessor. A stale resolver cannot finalize/restore after
either preemption. Thus close before finalize yields no promotion; finalize
before close yields historical Going followed by closure.
The slot is subject-bearing, participates in export/erasure/backup, and is cleared
on settlement; it is not an unbounded history. No SQL lock spans Discord HTTP.
Tests must prove both release→compensate and promote→compensate races; a simple
precheck-only allocator is not compatible with the current security contract.

## 5. Deterministic promotion and replay

1. A release, eligible join, capacity increase or observed lifecycle change
   serializes under the same locks, advances revision and marks durable work.
   Every new seat request checks waiting state first: a vacant seat is reserved
   for the oldest waiter until definitively removed, expired or promoted.
2. Worker snapshots the lowest valid sequence and event/policy revision, then
   outside SQL checks current human membership, effective channel/event access,
   bot permission and live event status/start through the existing REST gates.
   A candidate check has a **3-second** wall budget; proofs expire after
   **5 seconds**. A proposed process-local guild validation epoch advances on
   observed member/channel/role/guild changes and gateway reconnect. Snapshot
   before REST, compare before commit/postcheck; restart discards every proof.
   Epochs are an additional invalidation seam to implement, not a current or
   cross-process authorization source; fresh REST/postcheck remains mandatory.
3. Reacquire locks; re-read DB time, open state/deadline, latest start, policy
   generation, event/member revision, erasure cutoff, head sequence and vacancy.
   Any mismatch invalidates proof and restarts from current state. A matching
   proof changes that one waiting row to Going/held, clears queue fields,
   advances revision and records provisional audit/work in one transaction.
   Complete §4's durable postcheck/finalize protocol before success/projection.
4. A verified member 404 or successfully computed access denial removes only
   that matching waiter; a bot/API 403, 429, 5xx, timeout or incomplete permission
   evidence means **unknown**, not ineligible. Preserve head and vacancy with
   bounded backoff, no overtaking. One worker's lease does not replace commit
   validation; two workers cannot promote twice or exceed held capacity.

Discord and SQL cannot form an atomic authorization transaction. Fresh REST
plus observed invalidations bounds but does not eliminate an unobserved remote
permission change after check. After a committed promotion, later observed
loss follows the held-member lifecycle in §3, not rollback/requeue. An ephemeral
success or quiet post edit is not a new proof of current membership.

Authenticated action context must match application, guild, event, bound
channel/message and actor. Reject malformed/foreign/bot/system requests before
mutation. Fingerprint canonical typed application/guild/event/actor IDs, action,
normalized choice/capacity input and origin channel/message (null for slash);
never hash a display label or arbitrary JSON field order. For seat-mode actions
admit interaction times only in `[now-15 min, now+30 sec]`.
Cursor same ID/same fingerprint returns its bounded prior result
plus **current** own state after authorization; same ID/different action refuses;
older IDs refuse; newer actions supersede older desired state. Cursors survive
Cancel/Declined so a delayed Join cannot resurrect waiting. A rejected full
Going request also needs a cursor, so later duplicate delivery cannot claim a
newly vacant seat as though it were a fresh request. Pruning after 24 hours
is safe only with the 15-minute intake fence still enforced.

`erase_member` (`crates/cutover/src/member_erasure.rs:235-313`) and retention
`complete_request` (`crates/cutover/src/retention.rs:1165-1209`) must hook after
schema/guard checks and before manifest deletion in their existing transaction.
`member_erasure::plan()`/`schema_gaps` and
`crates/cutover/src/member_erasure_plan.json:144-146` already cover `event_rsvps`;
extend manifest coverage to cursors/pending-slot subject data and test both
executors plus dry-run. Within those guarded transactions, advance **all
configured events in the guild** (<=1,000 rows) to a non-subject
`action_not_before = max(old, DB now + 30 sec)`, invalidate proofs/work revisions,
and delete subject RSVP/cursor/audit data through the privacy manifest. Reject
interaction timestamps at or before cutoff before cursor lookup. This covers
a previously unseen delayed join without an erased-user tombstone; unrelated
members may briefly need to click again. Dry-run changes nothing. Require the
admitted clock-skew envelope; unhealthy time closes new enrollment. Guild purge
invalidates/deletes all work and bindings; no retained snapshot can restore them.

## 6. UX, accessibility and safe projection

Public post text shows counts and explicit labels, never waiting identities,
queue order, member mentions or private denial reasons. Own position is an
estimate at that revision, not a promised turn time. Do not add a public roster
or expose raw user IDs. Keep existing authorized attendee visibility unchanged.
Use escaped text, empty allowed mentions and the existing bot-owned post;
no new channel message or DM is required for promotion.

| View | Visible text / action labels |
| --- | --- |
| Empty, finite | `Going places: 0 of 8. Waitlist: 0. Going · Interested · Declined · My status` |
| Empty, unlimited | `Going places: unlimited (system safety limits apply). No waitlist.` |
| Full | `Going places: 8 of 8. Full. Join waitlist to accept the next available place.` |
| Waiting, own ephemeral | `You are waiting, position 2. You do not hold a place. Automatic promotion ends <absolute local date/time + UTC>. Cancel waitlist.` |
| Vacancy reserved / unknown access | `A place is pending the next waitlist check. Your saved state has not changed. My status.` |
| Promoted, own status | `You are Going and hold a place. Cancel Going place or choose Interested/Declined to release it.` |
| Declined, own status | `You declined. No place or waitlist entry. You can choose Going again while allocation is open.` |
| Expired / closed | `Waiting has ended; no place was assigned. Interested remains saved. Reason: event started/completed/canceled/deleted or consent expired.` |
| Commit saved, edit failed | `Your change was saved; the post could not be refreshed yet. Use My status to check.` |

Labels convey state without color/emoji alone; preserve keyboard/component
labels and readable text fallback through the existing slash RSVP surface.
Use absolute timezone-qualified times as well as Discord local rendering, not
only a relative countdown. Existing held members at close get `Allocation closed;
your historical RSVP is Going`, not the wait-expired copy. Channel-local changes
are quiet; a member may miss promotion without checking. This is an explicit
tradeoff for review, not a delivery guarantee.

Projection is serialized per event/message, reloads newest state before editing,
and only acknowledges the revision sent. Failed/uncertain edits never roll back
seat truth or allocate again. An old in-flight Discord edit can land after a
newer acknowledgement; therefore repair even apparently clean bound posts via
bounded GET/compare/re-render, targeting every **10 healthy minutes**, until
terminal time + **24 hours**. Hash the canonical bot-owned rendered text and
component fields per binding (not ephemeral queue position, full remote message
or unrelated content). Byte-identical canonical render is visually clean even
if DB revision advanced; acknowledge that current revision. Retain periodic GET
verification regardless of hash/revision equality; never store fetched content.
A prolonged outage or stale edit after that repair window has no convergence
guarantee. Every component still checks authoritative DB state. 404 of a bound
message tombstones the binding; 403/unknown preserves it with backoff.

### Closed result codes for future adapters

Replies are readable text; future internal adapters return `code`, `retryable`,
`next_action`, event key, revision and authorized own state, never raw API errors.
No new endpoint is authorized here. This is the closed code set:

| Codes | Retryable / next action |
| --- | --- |
| `HELD`, `WAITING`, `INTERESTED`, `DECLINED`, `UNCHANGED` | no / view current state; explicit cancel or new choice |
| `FULL`, `WAITLIST_PENDING` | no / explicitly Join waitlist; never auto-enroll |
| `CLOSED`, `CONSENT_EXPIRED` | no / inspect reason; new opt-in only if still open |
| `POST_UNBOUND`, `UNSUPPORTED_OCCURRENCE` | no / use existing RSVP; staff review binding/occurrence |
| `NOT_AVAILABLE` | no / verify guild/access with staff; collapse missing, foreign and denied contexts to avoid an oracle |
| `INVALID_ACTION`, `STALE_ACTION`, `ACTION_CONFLICT` | no / reload and make a fresh explicit choice |
| `CAPACITY_BELOW_HELD`, `WAITLIST_NOT_EMPTY` | no / organizer raises capacity or leaves saved setting unchanged |
| `SAFETY_LIMIT`, `QUEUE_LIMIT` | no / cancel existing enrollment or wait for expiry; do not retry in a loop |
| `RATE_LIMITED`, `ELIGIBILITY_UNKNOWN`, `RETRY_LATER` | yes / retry only after bounded server Retry-After/backoff |
| `SAVED_REFRESH_PENDING` | yes, projection only / My status; do not repeat the allocation |

## 7. Bounds, privacy and cost

Proposed limits below only **tighten** existing safety bounds; the effective
limit is the smaller of these and the current storage/rate/privacy contract.
Existing normal RSVP quotas and rate accounting still apply to waiting writes.

| Resource | Hard bound / overload behavior |
| --- | --- |
| Capacity records / open events | 1,000/guild including retained terminal records / 20 open; refuse new configuration, not cancellation/closure |
| Waiting | 100/event, 2,000/guild, 5 events/member/guild; atomically reserve quota |
| Total RSVP rows / write budgets | Existing 1,000/event across all answers, 20 writes/member/guild/UTC minute unchanged, including waiting |
| Action cursors | 1,000/event, 50,000/guild; refuse new seat-mode actors at cap, preserve existing self-cancel/close/erasure |
| Work | One row/event; 1,000/guild including retained terminal work; refuse new configuration at cap |
| Worker | One DB-leased global leader (30s generation-fenced lease, renewed before further work); 10 due events/pass, 5 head decisions/event/pass; crash/restart discards proofs; per-event compare-and-commit still mandatory |
| Reads | Own state O(1), position count over at most 100 waiters; any authorized page <=25; event work scans <=10 |
| Discord | Global 60 eligibility HTTP requests/minute and 10 candidate validations/minute/guild, counting every REST call/retry; existing stricter/shared limits win |
| Projection | <=12 edits/minute globally; <=12 verification GETs/minute; retries count; <=3 bindings/event |
| Retry | 5s, 30s, 120s, 300s cap; respect longer Retry-After; no discard of a live queue for attempt count |

Waiting subject data lives only until cancel, promotion, definitive exclusion,
close or consent expiry. A healthy cleanup pass targets deletion within **5
minutes**, processes overdue closure/expiry before promotions after restart,
and makes no offline wall-clock guarantee. Cursors expire after 24 hours idle
or immediately on erasure; work/bindings retain terminal repair for <=24 hours.
Non-member event tombstones persist only with existing event retention; replay
age fence rejects callbacks after their removal. Add every new subject-bearing
field/table to export, erasure, guild purge and backup inventory; preserve
unrelated replay-fence exceptions. Never export waiting identities to public
views or add them to reminder payload/log/metric labels. Copied/quoted Discord
content is outside the bot's erasure control.

Estimated incremental spend **$0**, using the current process and Postgres, not
permission to buy capacity. At assumed 0.5–1 KiB/indexed row: <=2,000 waiters is
1–2 MiB/guild; <=50,000 cursors is 25–50 MiB/guild, excluding WAL/backups and
existing RSVP data. At most 20 live work rows and 60 live post bindings/guild
fit the open-event limit; retained terminal work is separately capped at 1,000.
These are sizing assumptions, not measured headroom. Measure relation sizes in
a disposable fixture before enabling; reduce limits rather than provision spend.
One accepted action costs one short state/audit transaction; promotion adds a
snapshot and compare-and-commit plus bounded external validation. No per-click
notification queue, whole-guild member scan or per-member perpetual timer.

## 8. Reminders, event voice and other writers

Keep ordinary Interested reminder semantics unchanged. **Waiting Interested is
not a held attendee**: exclude it from attendee reminder targets. Promoted Going
may receive the ordinary still-due reminder (future scheduled mirror, inside its T-15 window, no existing
attempt fence), but no catch-up DM/message is sent
just because promotion occurred after the reminder decision. The current seam
(`core/src/event_reminder_store.rs:15-112`, `bot/src/scheduled_jobs.rs:195-275`)
rechecks saved recipients and mirrored event fields, **not live membership**.
Use **the identical filtered recipient predicate** in due scan, EXISTS and
atomic dispatch-array comparison: ordinary Going/Interested, excluding waiting,
erased and provisional actors. Retain matching mirror fields/time and whole
sorted-array equality. A mismatch inserts **no attempt fence**; a later tick may
rescan/retry while still due. Only a committed attempt consumes the one-shot
allowance, including subsequent render/send failure. The exclusion guarantee is
at that DB admission snapshot, not after-the-fact recall of HTTP already in
flight. Do not add per-attendee REST enumeration or claim fresher membership
than the mirror/store actually knows. Respect the
immutable **one automatic attempt/guild/event** fence, including render failure,
2,000-UTF-16-unit whole-list limit and 10,000 retained attempts/guild. No reset
on reschedule, late promotion, refusal or uncertainty; no extra reminder/retry.
A member loss/cancellation unobserved by the mirror, or committed after the final
recipient check, can still produce an in-flight reminder; disclose that limit.
Copy must not imply a seat from a reminder. Ordinary Interested attendees still
receive the existing reminder, and its existing authorized mention rendering
is unchanged (the seat projection itself adds no names/mentions).

Voice storage currently keys `(event_id, instance_starts_at)`
(`crates/cutover/migrations/0455_event_voice_channels.sql:4-32`), unlike RSVP's
event-only key. Do not join them by start time or silently reuse a series seat
across voice instances; unsupported recurrence stays dark until aligned.
Voice remains independent: do not use the seat count as a voice user limit,
role grant, check-in, presence span or after-party owner vote. Closing allocation
at start does not end voice; voice's later end/grace/metadata-only handoff wins
for that subsystem. No change to occupied-room safety or privacy projections.
A shared-DB/native-event writer that does not implement these rules must either
be excluded from enabled events or upgraded and proven before activation; no
silent last-writer-wins bridge or native-interest auto-enrollment.

## 9. Future deterministic validation matrix

**Not executed or implemented by this document.** Test Author supplies protected
acceptance files after approval/alignment; builders fix code, not those files.
Use disposable fixtures, independent pools, barriers and injected clocks/REST;
no sleeps as race proof, no tests/migrations against staging or production.

| Case | Deterministic oracle |
| --- | --- |
| Two Going requests, last seat | Exactly one held; other FULL unchanged; no implicit waiting; held <= limit |
| Release + newcomer + existing head | Newcomer never overtakes head; head alone can take the vacancy |
| Two drains + duplicate release | One promotion/audit; stale lease cannot commit/ack; no second seat |
| Join/Cancel/reordered old Join | Latest accepted actor ID wins; no resurrection after cancel or restart |
| Same callback altered / full-click replay | Conflict refuses; replayed full decision never claims a later vacancy |
| Close/start/expiry vs blocked proof | Close committed first => zero promotion; promotion first => historical Going then close; equality at deadline refuses |
| Capacity edits + existing Going | Concurrent counts serialize; below-held refused; enabling never evicts; nonempty waitlist prevents unlimited switch |
| Restart, postcheck or audit failure | Before commit no effect; pending reservation survives restart; release compensation cannot lose a seat to drain; promotion compensation restores the same FIFO place; audit failure rolls back; no success before postcheck |
| Member 404 / computed denial / API 403,429,5xx | Definitive removes matching head; unknown preserves head/vacancy; later member cannot bypass |
| Permission invalidation / stale proof / reschedule | Revision/generation/start mismatch or >5-second proof refuses; shortening consent never extends it |
| Erasure during proof, unseen delayed join, dry-run | Both erasure executors fence replay and remove subject; no resurrection; dry-run untouched |
| Held departure / ambiguous occurrence | Observed definitive departure releases once; unknown doesn't; ambiguous recurrence fails closed |
| Changed/expired/erased waiting reminder target | Fresh target excluded; no duplicate reminder/promotion; post-check in-flight race explicitly bounded |
| Send/edit timeout / late old projection | Seat remains committed, saved-refresh copy truthful; clean-record repair eventually uses newest revision within retained window |
| Foreign actor/context, missing bound post | No writes, no identifiers in denied replies, no permission elevation or replacement post |
| Limits/retry storm/cleanup | Exact cap and cap+1; concurrent last quota; self-cancel/close remain possible; request/edit budgets include retries; restart cleans overdue work first |

Future runnable acceptance check, once slice S1's named target exists:

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db --test rsvp_capacity_contract -- --test-threads=1
```

Admission remains dark through S1–S3; only a later separately authorized
activation after S4's compatibility/worker proofs can admit anything. Every
admission/finalize independently checks DB time, not just a periodic closer.

The Test Author must create the future `rsvp_capacity_contract.rs` acceptance
file in the existing `crates/core/tests/` directory and make the command above
fail for missing capacity behavior before implementation.
A zero-tests/ignored run is not acceptance. Add corresponding runtime targets
for S2/S3/S4 to the normal CI inventory; existing unchanged RSVP/reminder suites
remain regression coverage, not proof of the missing capacity feature.

## 10. Proposed follow-up PR cuts (not tasks or execution permission)

Each cut is <=one engineering day excluding CI/review; planner must recut if an
actual caller/permission inventory cannot fit. No implementation cards exist
as a consequence of this document. Each later cut has a separate Test Author
handoff, real missing-behavior RED, exact-head green CI and independent review.

| Cut | Files/seam and acceptance boundary | Depends on |
| --- | --- | --- |
| S1: dark allocation/store/privacy | `core/src/rsvp.rs`, `rsvp_store.rs`, additive cutover migration, manifest/retention/backup; four legal states, locks/cursors/quotas/FIFO, enable-on-existing and erasure/replay matrix in `rsvp_capacity_contract`; no runtime controls | Accepted spec/plan and recorded host alignment |
| S2: fresh drain/lifecycle | `discord/src/rsvp.rs` validation seam and bot job wiring; durable bounded worker, fresh compare-and-commit, close/reschedule/access/lease/restart and rate tests; still dark | S1 |
| S3: existing controls/projection | Existing RSVP router/message binding and render seam; explicit join/cancel/status, organizer limits, truthful accessible copy and stale-edit repair; no new announcement or slash family; disabled-path regressions | S1, S2 and verified existing-post premise |
| S4: reminder/writer compatibility and activation contract | Existing reminder target/send seam plus all enumerated enabled-event writers; waiting exclusion, stale-target tests, no duplicate delivery, flag-off/compatible rollback proof; no production enablement | S1–S3, applicable independent security/privacy review |

S1 must include privacy/backup locks and readers, not postpone them until an
already unsafe rollout. If that makes S1 too large, planner splits it into
independently dark migration/privacy and allocation-store PRs before execution.
Do not fold changes into the running attendee-reminder or event-voice cards.

## 11. Review assumptions, success/kill rule and undo

CEO/spec reviewer decides whether to accept: finite capacity only on existing
posts; auto-promotion consent (no claim timeout); quiet projection with no DM
or ping guarantee; ordinary Interested reminders retained but waiting excluded at dispatch admission;
close at start, <=7-day waiting consent and no reopen; unknown head deliberately
blocks a vacant seat until deadline; automatic writes attributed to the consenting
member with next-minute budget deferral; the withdrawal-only permission branch;
the proposed caps/cost/slices; and unverified TWO demand.
Also explicitly review the guild-wide erasure cutoff: at most 1,000 configured
events are invalidated per guild; a burst of erasures may repeatedly require
fresh clicks. Cutoff refusals happen before cursor/audit/rate consumption. A
per-subject tombstone would narrow that blast radius but retain erased identity
and still miss never-seen callbacks; it is deliberately rejected here. Existing
memberships remain saved. Engineering recommendations above are not claimed
to be owner decisions.

Before build carding, host intent alignment must resolve: whether a durable
existing RSVP-post binding actually exists or the scope should be deferred;
which native recurrence identity is supported; which management principal
already owns event edits; and whether shared-DB writers can be fenced without
a second source of truth. No unavailable host route or missing demand is
resolved by declaring the document merged. Material revision requires fresh
revision-bound review/plan acceptance and alignment.

Seven-day **spec** review on 2026-10-30: every matrix row has an agreed expected
result, an assigned future validation seam, bounded resource cost and accepted
limitations. Stop/defer expansion if native behavior suffices or no distinct
user value survives. This is not a seven-day production experiment or a promise
to ship on that date. Any later authorized staging validation reports aggregate
vacancies/promotions, unknown-head deferrals and invariant violations; zero
overbook/double/closed/known-ineligible promotions is mandatory, not a demand
metric. A follow-up efficacy experiment needs its own approved hypothesis/date.

Future rollback: atomically disable new seat admission and promotion via policy
generation while preserving self-cancel, read-only status, expiry/closure and
privacy cleanup. Keep additive schema/data and compatible reads; do not deploy
an old writer that ignores allocation, run a destructive down migration or
restore shared data. Later authorized execution records **ACT / WHY / UNDO**.
This document itself has no operational effect; undo is a reviewed docs revert.
