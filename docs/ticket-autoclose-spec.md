# Tickets: inactivity warning and reversible closure

Status: **Proposed — design only**. Written 2026-10-26; review by 2026-11-02.
This is the editable repository contract; the card's spec records its commit and
SHA-256. Merge is not build, migration, permission, retention or activation
approval. No runtime or build cards, real ticket data or live operations here.

## Decision: keep manual Close now

Consumer: staff sweeping abandoned support channels, and members who still need
help. Reduced staff work is a hypothesis; demand and ticket inactivity rates have
not been measured. A quiet ticket is not evidence that its problem was resolved.

| Option | Value / cost | Recommendation |
| --- | --- | --- |
| A. No build; current staff Close | No new tracking, automation authority or scheduler. Staff decides when to use today's capture-before-delete flow | **Use now**; no safe complete one-day runtime slice established |
| B. Opt-in per-panel warning, grace, reversible soft-close | Avoids an unattended destructive close, but needs durable activity/coverage, warning receipts, a new non-deleting lifecycle and privacy proof | Conditional design candidate only; never reuse current Close as timer executor |
| C. Timer calls current Close or auto-delete | Deletes channels, releases admission and hides uncertainty behind apparent success | **Reject**; no automatic deletion, cleanup_pending transition or reservation release |

[Ticket Tool automation options](https://docs.tickettool.xyz/dashboard/panel-configs/automation-options),
read 2026-10-26, supports message absence checks for Anyone/Ticket Owner/Support
Team, configurable reset and Closing/Deleting/custom-command actions, with a
five-minute minimum and ten automations per panel. It does **not** document a
dedicated pre-close warning there. This is comparable functionality, not TWO
demand, authorization or proof of safe warning delivery. No competitor behavior
is imported as policy.

B means a **reversible soft-close**, not resolved, deleted or current `Closed`.
The channel remains visible to its existing audience; only the opener's
SendMessages is denied, staff retain access/write, and the active reservation
continues to prevent another ticket. GuildText has no thread archive bit: do not
claim to archive it, move it to another category or synchronize broader ACLs.
The new timer may never grant close authority to an opener, claimant, collaborator,
agent-supplied actor or service MAC. It requires separately accepted staff-delegated
automation authority; elapsed time is not a staff identity.

## Source baseline and missing seams

Inspected source head: `cdf33be676858f72f5874c000cd0e63efda6a02a`.
These are repository facts, not deployed-state or acceptance measurements.

- `crates/core/src/tickets.rs:8-35,73-84,106-125`: Open/Claim/Close and five
  lifecycle states; staff = configured role OR ManageChannels OR Administrator.
  No inactivity, warning, exemption or soft-closed state exists.
- `crates/cutover/migrations/0210_tickets.sql:2-35`: ticket/channel/guild/opener,
  claimant and lifecycle times, no panel-generation or creation ACL snapshot.
  A displayed panel message is not an immutable panel policy binding.
- `crates/bot/src/ticket_runtime.rs:1212-1262`: staff close freezes opener,
  captures, commits, then cleans up by deletion. The store's
  `crates/cutover/src/tickets.rs:180-199` atomically saves snapshot and marks
  cleanup_pending. B cannot call that transaction or feed its rows to cleanup.
- `crates/discord/src/executor/tickets.rs:46-146` and
  `crates/bot/src/ticket_runtime.rs:1286-1308`: exact guild/channel/type/category,
  topic and four-overwrite proof; current config is not original destination
  evidence. Role/category drift must refuse, not repair access.
- `crates/cutover/src/tickets.rs:338-372`: inclusive transcript purge and
  opener/current-claimant erasure. Parent ticket metadata has no age purge here.
  [Privacy](privacy.md) does not promise deletion of Discord-side copies.
- [Ticket runtime](tickets.md) supplies recovery/component/capture limits;
  [configuration](dashboard-ticket-config-spec.md), [claim release](ticket-claim-release-spec.md)
  and [private subjects](ticket-subject-spec.md) are adjacent designs, **not**
  installed panel generations, replay fencing or new data grants. An opener's
  resolution signal is independent and must never accelerate this timer.

Unknown legacy binding, schema, activity coverage or live capability means A.
No historical backfill from a topic, name, channel history or current config.
The current runtime has one configured panel; B introduces no multi-panel editor.
A future policy must bind the one panel's immutable identity/revision and newly
enrolled tickets, not reinterpret a mutable channel ID as a panel generation.

## Conditional B: clocks, enrollment and activity

All numbers below are proposed bounds, not settings or measured capacity.
Default **off**, per panel; current staff must explicitly opt in after the gates.
Only new tickets with saved creation category/staff-role/panel-policy binding may
enroll. A durable opt-in record must bind current authorized staff actor, panel
identity, exact policy revision and enrolled ticket generation; no memory-only
flag. Enrollment stamps last_activity_at at confirmed Open activation in DB time.
No retroactive enrollment of existing tickets. Proposed inactivity
choices: **24 hours, 72 hours (suggested), or 7 elapsed days**; fixed **24-hour
grace**. No shorter grace, SLA, priority, escalation or opener-only clock.

Use injected **database UTC epoch milliseconds** for persisted deadlines; use
monotonic time for per-call budgets. Never use an agent clock, client timestamp,
message edit time, local timezone or Discord display timestamp as elapsed time.
Eligibility at `now >= last_activity_at + inactivity_ms`; closure at
`now >= warning_confirmed_at + grace_ms`. Grace starts at confirmed warning
receipt, not the intended send or old inactivity deadline. Checked arithmetic;
invalid/future timestamps or a detected clock discontinuity make the row
`coverage_unknown`, not instantly overdue. After discontinuity, re-enrollment by
staff starts a fresh full inactivity period; do not catch up missed timers.

Activity is a **new, authenticated, same-channel human MESSAGE_CREATE** from the
opener or current authorized staff with proven access. Attachments-only messages
count without reading/storing attachment URLs or content. Any other proven human
participant also cancels the current warning and marks the ticket exempt pending
staff review: an unexpected audience must not be silently treated as idle.
Unexpected-human activity deliberately latches exemption rather than merely
resetting time, including during grace. This permits a participant to prevent
automatic closure; preserving help/access beats enforcing an idle timer. No
review queue is implied: staff can use the retained channel and explicit Resume.
Threads are outside this clock: a ticket with a thread, or any observed child-thread
message, becomes exempt; B may not ignore a conversation in a child channel.
Bot/webhook/system messages, warnings, transcripts, slash acknowledgements,
reactions, typing, edits and deletions do not count or extend time. A replayed or
out-of-order message cannot move the activity high-water mark or clock forward;
retaining a message ID does not retain its body. Unknown bot/author/access facts
invalidate coverage rather than treating that event as nonhuman silence.

The DB serialization point of first accepted new event stamps its activity time.
At the same millisecond as a threshold, a committed activity revision wins over
a timer observing the old revision; close admission must re-read that revision.
An event delayed before persistence is **not** proof of inactivity. Durable
ordered ingestion and a caught-up coverage fence are prerequisites: a queue gap,
disconnect, restart, dropped event or unproven REST history means no timer effect.
A bounded recent-history read alone cannot prove an interval had no human activity
(bots may bury it). No full-history polling or content collection to fill gaps.
On restart/gap invalidate every affected warning and require staff re-enrollment
with a new full inactivity period after continuity is established. A persisted
past deadline alone never authorizes catch-up closure.

Activity bookkeeping keeps one ticket-scoped high-water ID/time/revision and
coverage epoch, no author history or message text. Current privacy policy excludes
ordinary ticket lifecycle from analytics opt-out; that is not a disposition for
new inactivity tracking. Independent privacy review must classify this new
collection and decide suppression behavior before B; absent that decision, do not
enroll. Never silently repurpose analytics activity projections or bypass their
suppression. Existing member erasure remains mandatory for all ticket data.

## Warning, keep-open, exemption and close UX

1. Immediately before admission and delivery re-prove policy, immutable binding,
   current private channel/effective bot/opener/staff access and `Open`. Preserve
   application, guild, capability and feature fences. No role grant, invitation,
   ACL repair, arbitrary target selector or cross-guild diagnostic read.
2. Send **one channel warning per idle episode**, <=300 characters, plain text,
   closed allowed_mentions (no parse/users/roles/replied-user ping), no names,
   subjects, excerpts, embeds, transcript or attachment links. Same proven private
   channel only; no DM, staff channel, public panel or fallback recipient.
   Example: “This ticket has been inactive. In 24 hours it may be locked for new
   opener messages, not marked resolved or deleted. Choose Keep open or post a
   message to cancel. Staff can exempt it. Existing channel access remains.”
3. **Keep open** is available to the authenticated opener or current staff with
   current access, even though only staff may Close. It cancels this generation
   and resets inactivity from DB commit time; it never closes/reopens anything,
   transfers a claim or changes access. All buttons are generation-bound,
   same-guild/channel/application, authenticated bot-message controls, not bearer
   capabilities. No free text. Refuse forged/old controls after reauthorization.
   New opener/current-staff messages during grace likewise cancel and reset;
   unexpected-human messages instead latch exemption as specified above.
4. Staff-only **Exempt** cancels pending timers and latches exemption until
   deliberate staff **Resume inactivity** on an Open ticket. Resume starts a full
   new period; never restores an old warning. Panel disable/config or staff-role
   change cancels enrollment, not an immediate reversion of ACLs. Already
   soft-closed tickets need separately authorized staff reopening.
5. Claimed tickets are **exempt by default**, including existing claim changes.
   A claim/transfer during grace cancels it. Self-release is not implemented here;
   even a future release does not silently enroll. Staff must explicitly resume
   after unclaim; preserve first-winner claim and existing staff authority.
   Claim/config/disable invalidation after wire admission but before capture
   commit also aborts/reconciles that idle intent; it never completes soft-close.
   Cancelling enrollment cancels every pending/warned generation as well.
6. After full confirmed grace, recheck all facts, including current enrollment,
   unchanged policy/claim/activity revision and that the same proven
   warning is still visible with usable Keep open controls. Missing/deleted/edited
   or unproven warning cancels automatic closure; no replacement send. Serialize
   with activity, keep-open, exemption, claim and manual close. Reserve one durable
   soft-close intent for that exact generation, then deny **only opener
   SendMessages** via the shared executor's `set_ticket_opener_writes` semantics
   (preserve every other overwrite bit/principal). Freeze before bounded capture;
   validate every page and atomically save a complete bounded snapshot plus
   soft-closed result in a **new non-cleanup transaction**. Partial/timed-out capture
   cannot commit as complete. Do not claim soft-closed before permission receipt
   **and** capture commit are confirmed. Never invoke `cleanup` or
   `delete_ticket_channel` from B, including failure/recovery. No deletion queue.
7. Capture failure/cancellation keeps an explicit `soft_close_pending`/uncertain
   operation. Proven safe restoration may return to Open; unknown freeze/restore
   never reports success or blindly toggles ACLs. Staff reopening must prove
   unchanged original ACL/policy and current authority; it may restore only the
   opener's prior SendMessages, never widen ViewChannel or undo a newer staff deny.
   No opener self-reopen after lock. Copy: “Locked for opener messages; not marked
   resolved or deleted. Staff can reopen. A bounded transcript is retained for up
   to 90 days under existing policy; no transcript link was sent.”

Reopen does not edit, expose or extend a saved transcript. No automatic recapture
or new snapshot on repeated closure of that ticket; choose permanent timer
exemption after its first soft-close. A later **manual staff Close** needs a
separately accepted compatibility rule to capture any newer staff messages before
deletion without replacing/refreshing the original snapshot's retention ceiling.
Until that rule is implemented/proved, such tickets cannot enter current Close
or current cleanup/recovery; B stays dark, rather than silently deleting later
content or changing retention policy. The design does not pretend this seam is
already supported by the one-transcript store.

## Serialization, replay, cost and privacy

Proposed overlay states are `observing`, `warning_pending`, `warned`,
`soft_close_pending`, `soft_closed`, `exempted`, `uncertain`; **not current enums**.
They need a durable increasing activity/policy/episode revision, single intent and
shared per-ticket serialization across **all** writers, including erasure and
manual close. A separately accepted schema design must map these states to the
active unique index, every lifecycle writer, recovery and purge: never encode
soft_closed as existing Closing/CleanupPending, or add a status outside the active
index. Do not leave it as ordinary Open visible to today's recovery/Close either.
No INSERT-overwrite/upsert may replace the one existing transcript or reset its
purge_after. Missing schema/writer compatibility keeps B dark, not partially live.
No DB lock spans Discord I/O. Claim a version with CAS, revalidate before effects
and settle against the same token. A process mutex is insufficient.
Manual Close first cancels the idle intent. A staff override cancels any pre-wire
idle intent and drains its worker; a wire-admitted/unknown freeze must first settle
or reconcile before either close path acts. The timer cannot hold a DB/lane lock
indefinitely or authorize staff to ignore an unresolved effect. Each attempt ends
within its 20-second row deadline, releasing execution resources but retaining
unknown-effect fences. There is **no guaranteed resolution time** for an unknown
Discord effect, so manual Close may remain unavailable pending authorized
inspection. That availability cost requires explicit intent acceptance and is
another reason to retain A; never bypass the fence to meet a close SLA.
No stale worker may PATCH after a winning keep-open, close, erase or replacement
generation. A human message arriving
after effect admission is a residual in-flight race: if persisted before snapshot
commit, abort/reconcile rather than call it inactive. Preventing even a temporary
freeze after such a late event is not promised; intent review must accept this.

Durable warning admission precedes **one** send attempt. A receipt proves the
bound bot-authored warning/control message; 2xx alone is not proof. Warning send
plus receipt settlement has a **five-second total deadline**, no automatic
wire retries; narrower remaining row/pass/REST deadlines win. Crash, timeout,
429/5xx or receipt-write uncertainty latches `uncertain`: no new send key, resend,
grace countdown, close, or fallback. Read-only reconciliation of the exact known
message may prove observed delivery, but an uncertain episode never resumes its
countdown. Cancel after proven drain and require fresh deliberate staff enrollment;
missing ID/403/generic 404 is not absence or success. A merely slow but still
within-budget confirmed send can start full grace only for its unchanged episode.
Staff may cancel/re-enroll only after proving old work drained, never by discarding
an unresolved fence. Confirmed capture followed by a lost reply remains captured;
unknown commit is unknown, not retryable success. Restore/import must drain old
workers, invalidate episodes/controls and reapply erasure before admission.

Bounded scheduler proposal: reuse the owned ticket supervisor, no host cron,
separate HTTP client or unbounded in-memory timers. At most **one pass/300 s**, one
same-guild indexed due-page of **25 rows**, rotating durable cursor, **4 rows**
acted on/pass, **one warning send/pass**, one effect lane, **20 s/row** within
**80 s/pass**. Skip elapsed ticks and retain due work rather than pile up tasks.
These are ceilings on **remaining** recovery row time, never an additional 80 s:
existing work plus idle work together stay within 180 s row work and 240 s total,
with the 30 s panel slot reserved. Existing work consuming the budget admits
**zero** idle rows that pass. Preserve component 16 global/3 member, 10 s lane
wait/120 s work and shared REST/send-admission limits. This is configured-guild
only, not a cross-guild scheduler. Cursor wrap revisits still-due DB rows, not an
unbounded retry queue; a permanently failing row cannot block later rows.
Capacity unavailable means defer, no admission-fence bypass. Warning and capture work must not starve manual buttons,
panel repair or the independent hourly privacy purge. No extra history poll per
tick; only close capture uses existing <=10x100 messages plus boundary probe and
200,000 UTF-16-unit chronological prefix. Attachments are URLs only, not copied
files; deleted messages/edits and history beyond the cap are not preserved. Staff
writes remain possible during capture, so this is not an atomic channel snapshot.
Due-query boundedness needs EXPLAIN and multi-connection/slow-prefix tests, not
LIMIT as a claim of bounded scanned work. Existing `recoverable()` loads an
unpaginated set; do not reuse that query as the proposed 25-row due page.

Capture is preservation **within existing policy**, not indefinite evidence:
transcript 90 elapsed days from capture, inclusive purge, no retention reset on
reopen/replay/recovery, and member-erasure cascade unchanged. Bodies remain only
in `ticket_transcripts`; no new transcript read, download, DM, dashboard reader
or nonstaff access. Administrators remain outside any promise of confidentiality.

Proposed audit: fixed transition/reason, time, guild/ticket/episode/operation IDs,
policy revision and staff actor only for deliberate enroll/exempt/resume/reopen;
never body, attachment URL, subject, username, raw custom ID/token or free reason.
Audit must commit with its state transition and be non-mirrored; today's separate
pooled audit writer does not already guarantee that. Ticket-linked FK/erasure
must cover former staff actors as well as opener/claimant. Apply existing
45-day operational-audit ceiling, no refusal-history stream or ID metric labels.
Proposed linked enrollment/high-water/receipt ceiling: **90 elapsed days from
first enrollment**, never extended by activity, Keep open, resume or reopen.
At expiry stop automation, drain/fence pending work, then erase tracking; an old
control or worker can never recreate it. This ceiling is a review candidate, not
an enacted retention change. Indexed erasure/export/restore and a non-resurrecting
lifetime fence need explicit design/acceptance **before build**; parent rows are
not age-purged and transcript TTL is not their TTL. If replay safety would require
an indefinite linkable exception, reject B rather than retaining it by default.
The unresolved implementation/privacy boundary keeps A in use.

## Stable outcomes and deterministic examples

No CLI, endpoint or settings key is added. Proposed internal diagnostics <=2048
UTF-8 bytes; string IDs/revisions, finite `reason`/`next_action`, no message text.
Only authorized staff with access see ticket/episode IDs; unavailable/foreign
contexts omit all identity/state fields and use one generic unavailable result.
Typed `mode=dry_run` is a local synthetic-only evaluator: zero network, SQL,
nonces, quota, audit, warnings, controls or scheduled work, not a live readiness
check or a flag that can accidentally run effects.

```json
{"schema":"tickets.inactivity.v1","mode":"dry_run","ticket_id":"0123456789abcdef0123456789abcdef","episode_id":"7","operation_id":"idle-7","outcome":"allowed","reason":"not_due","retryable":false,"next_action":"await_activity_or_threshold"}
```

| outcome / reason / next_action | Bounded example / effect |
| --- | --- |
| `allowed / not_due / await_activity_or_threshold` | Observing at T+71 h; no effect, not a promise of later Close |
| `warned / warning_confirmed / keep_open_or_contact_staff` | One proven warning at T+72 h; grace starts then |
| `allowed / kept_open / await_activity_or_threshold` | Opener presses valid Keep open at grace+23 h; cancel episode, no permission effect |
| `closed / soft_closed / staff_reopen_if_needed` | Confirmed freeze + capture commit after full grace; channel retained, not resolved |
| `exempted / claimed_or_staff_exempt / staff_resume_if_appropriate` | Claim wins during grace; zero timer close |
| `deferred / capacity_unavailable / await_bounded_pass` | No quota/fence bypass or extra queue |
| `unavailable / context_unavailable / stop` | Missing/foreign/hidden/access or schema/binding failure; no IDs/disclosure |
| `uncertain / coverage_unknown / staff_reenroll_after_continuity` | Gap/restart/clock uncertainty; no catch-up |
| `uncertain / effect_unconfirmed / inspect_same_operation` | Lost send/freeze/commit receipt; never blindly retry |
| `stale / generation_changed / reread_authorized_state` | Late button/worker or manual close won; no new effect |

All `retryable=false` refers to **agent/client mutation replay**. A deferred row
with no effect admitted can be reconsidered by the bounded supervisor after fresh
proof; this is not a resend of an admitted operation. Unknown sends/effects are
never scheduler retries. Closed reports storage truth only after confirmed commit;
raw Discord errors never enter results.

Reset each fixture: g1/T1/c1, newly enrolled unclaimed Open ticket, inactivity
72 h, grace 24 h, known continuous coverage and valid private binding. Use
injected DB/monotonic clocks, unique channels, barriers, independent disposable
DB connections and shared recording Discord double. Assert effect counts,
unchanged claim/identity/ACLs except named opener write deny, no DELETE and no
nonstaff transcript response in every row. These are **future oracles**, not
executed runtime acceptance:

| Case | Deterministic order / oracle |
| --- | --- |
| TA-F01 | Disabled/legacy/unknown panel binding -> exempt/unavailable, zero tracking/send/close; no backfill. |
| TA-F02 | At threshold-1 ms -> not_due; at/after threshold -> one warning only if all proof/coverage holds. |
| TA-F03 | Human activity commits just-before/at/after threshold before timer CAS -> new revision, old timer cannot warn/close. |
| TA-F04 | Opener/staff/attachment-only activity resets; bot/webhook/system/edit/delete/reaction does not; unknown facts invalidate coverage. |
| TA-F05 | Activity racing warning cancels that episode: its warning can never lead to a new close admission. Activity after a later freeze is already wire-admitted follows TA-F21, with no completed soft-close. |
| TA-F06 | Slow within-budget receipt on unchanged episode starts full grace; timeout/lost receipt/429/5xx -> uncertain, no resend/countdown/close even if later observed delivered. Deleted/edited warning also forbids close. |
| TA-F07 | Grace-1 ms -> wait; equality/after -> only fresh proven soft-close; eligible Keep open committed first cancels. |
| TA-F08 | Nonstaff stranger/forged/wrong-message/expired-generation Keep open -> unavailable/stale, no effect or state disclosure. |
| TA-F09 | Exempt/claim/transfer wins in grace -> cancel; future release/disable/re-enable never resurrects deadline or silently resumes. |
| TA-F10 | Manual Close first -> stale timer, original capture-before-delete unchanged; idle intent first -> manual Close cannot compete. |
| TA-F11 | Freeze/capture failure or timeout -> no completed snapshot/closed result; safe restore only with proof, otherwise uncertain. |
| TA-F12 | Capture succeeds -> atomic non-cleanup snapshot/soft-closed; zero DELETE, reservation retained; duplicate settles once. |
| TA-F13 | Restart before/after send/freeze/commit -> retain unknown fence, invalidate grace/coverage, no catch-up; staff re-enrollment requires drain. |
| TA-F14 | Replayed/out-of-order event or competing timer connections -> no clock refresh, one episode/intent/effect; no old PATCH after winning cancellation. |
| TA-F15 | Role/category/ACL/application/guild drift or 403 -> no repair/send/close; same unavailable shape for outsiders. |
| TA-F16 | Erasure races pending work -> drain/fence then erase linked metadata/body; no stale send/PATCH/recreation, foreign guild unchanged. |
| TA-F17 | 90-day inclusive purge during soft-close/uncertain/reopen -> delete body, never extend/recreate it; audit at 45-day ceiling. |
| TA-F18 | Staff reopen retains old capture/claim/reservation; opener cannot reopen; current Close blocked until newer-message preservation compatibility proved. |
| TA-F19 | Overload/slow prefix/many due rows -> 25 read/4 work/1 send and deadlines hold, cursor progresses; buttons/panel/purge not starved. |
| TA-F20 | Clock jump/rewind, import/restore or opt-out -> exempt/coverage_unknown, no overdue cascade or data resurrection. |
| TA-F21 | Human arrives after effect admission but before capture commit -> abort/reconcile; residual brief freeze explicit, never false inactivity success. |
| TA-F22 | Dry-run identical synthetic facts -> identical bounded result, malformed mode refuses; zero network/SQL/audit/nonce/quota/scheduler effects. |

## Runnable docs checks and gated future slice

From repository root (stdlib only, no credentials/DB/Discord):

```sh
python3 -m doctest -v docs/ticket-autoclose-spec.md
python3 scripts/check_docs_index.py --json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
```

```pycon
>>> import json, pathlib, re
>>> text = pathlib.Path("docs/ticket-autoclose-spec.md").read_text()
>>> cases = re.findall(r"^\| (TA-F\d{2}) ", text, re.M)
>>> assert cases == [f"TA-F{i:02}" for i in range(1, 23)]
>>> H = 3_600_000; idle = 72 * H; grace = 24 * H
>>> def oracle(now, last=0, warning=None, exempt=False, coverage=True):
...     if exempt: return "exempted"
...     if not coverage: return "coverage_unknown"
...     if warning is not None: return "soft_close_candidate" if now >= warning + grace else "wait"
...     return "warn_candidate" if now >= last + idle else "not_due"
>>> assert [oracle(t) for t in [idle-1, idle, idle+1]] == ["not_due", "warn_candidate", "warn_candidate"]
>>> assert oracle(idle, last=idle) == "not_due"
>>> assert [oracle(t, warning=idle) for t in [idle+grace-1, idle+grace, idle+grace+1]] == ["wait", "soft_close_candidate", "soft_close_candidate"]
>>> assert oracle(idle+grace, exempt=True) == "exempted"
>>> assert oracle(idle+grace, coverage=False) == "coverage_unknown"
>>> sample = json.loads(text.split("```json\n", 1)[1].split("\n```", 1)[0])
>>> assert sample["schema"] == "tickets.inactivity.v1" and sample["mode"] == "dry_run" and sample["retryable"] is False
>>> print(json.dumps({"schema":"tickets.inactivity.design-check.v1", "cases":len(cases), "outcome":"checked", "reason":"offline_oracle_only", "next_action":"seek_exact_intent_and_independent_disposition"}, sort_keys=True))
{"cases": 22, "next_action": "seek_exact_intent_and_independent_disposition", "outcome": "checked", "reason": "offline_oracle_only", "schema": "tickets.inactivity.design-check.v1"}

```

The doctest checks boundary arithmetic, case inventory and sample shape only,
not durable ingestion, SQL/ACL/CAS/replay, privacy or scheduler behavior. No new
behavior tests here. Future independent acceptance needs actual caller tests,
behavior-specific RED and immutable protected test SHA/path/hash manifests;
compile/cache failure is not RED, and a builder may not weaken protected tests.

**Smallest future build slice: <=1 engineering day, only if separately accepted.**
A framework-free inactivity decision planner with injected facts/time and synthetic
TA-F02/F03/F04/F07/F09/F20/F22 decision tests, returning typed no-effect candidates.
No runtime caller, scheduler, command, DB/schema, persistent activity data, network
or activation. This is model validation, **not** delivery of automatic closure.
Full B exceeds that bound until coverage, immutable policy, non-deleting capture,
manual-close compatibility and finite metadata/replay erasure are resolved.
Do not remove controls or create follow-on build cards to make it fit.

**Boundary assessment: DESIGN_ONLY; no runtime grant.** This authored assessment
is not independent CISO approval of our own request. Through President/COO, obtain
CEO/host exact intent alignment and independent security accept/change/reject,
bound to the saved spec revision, repo commit and SHA-256. Keep Proposed until
both records exist; docs CI/merge and routing acknowledgements are not acceptance.
Review must decide A vs B, staff-delegated automation, retained active reservation,
claimed default exemption, 24-hour grace/Keep open, late-event residual race,
non-deleting capture/manual-close compatibility, and metadata lifetime/erasure.
No routine owner approval hop; existing activation and exact-head CI/review,
staging/deploy, credential and production fences remain unchanged.

Review metric by 2026-11-02: 22/22 oracles explicitly accepted/revised, intent and
independent boundary records present, and bounded feasibility accepted or B
rejected/deferred. Default/kill rule: if substantiated staff value is absent or any
safety prerequisite remains unresolved, choose **A/no-build**; expiry never enables
a feature.
Unverified: deployed ticket capability, real guild/private audience, activity
frequency, host intent, runtime B (absent), ingestion continuity and performance.
Cross-model review skipped: non-interactive context; no external CLI invoked.

ACT: publish an indexed proposal and offline oracle. WHY: unattended inactivity
must not become destructive Close authority or indefinite private-data retention.
UNDO: reviewed docs revert; no operational state changed. Future rollback disables
new timer admission and drains pending work, retaining capture/replay/privacy
fences; never blindly restore permissions or run a destructive down migration.
