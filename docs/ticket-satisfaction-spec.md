# Tickets: private post-close satisfaction and staff statistics

Status: **Proposed — design only**. Written 2026-10-26; intent review **not
before 2026-11-02**. This indexed repository file is the editable contract;
the task spec records its commit and SHA-256. Merge is not approval to implement,
collect feedback, enable DMs, change privacy policy or grant report access.
No code, migrations, transcript/claim/close changes, live data or deploys here.

## 1. Intent, alternatives and recommendation

Hypothesis: an opener can tell staff whether support helped, privately, without
turning community support into a competition. Demand, response rates and usefulness
are unmeasured. A score is subjective feedback, not staff performance, resolution
proof, an SLA, compensation input or permission to act on a member.

| Option | Value / boundary | Recommendation |
| --- | --- | --- |
| A. No build; existing ticket conversation | No new personal data, outbound prompt or reporting authority | **Use now** until exact intent and privacy acceptance |
| B. One private rating per ticket by its opener | 1–5 plus optional short comment; private self-service, optionally one consented DM invitation; bounded manager reads | Conditional candidate for review, not approved implementation |
| C. Automatic unsolicited DMs, repeat surveys or staff rankings | More pressure/noise and linkable personal data; easy to game | Reject; no public leaderboard, rewards, staff-edited scores or bulk export |

The comparable-product claim motivating this work is a research lead, not verified
TWO demand or a policy precedent. No transcript body, subject, priority or close
reason is needed for B. It must not depend on a transcript-copy proposal or silently
make saved transcripts available to openers.

## 2. Baseline and composition

Source baseline: `582330a6cda79af99b1c4391268d8fcad9923514`; repository inspection,
not proof of deployed activation or live permissions. See [ticket runtime](tickets.md),
[claim release](ticket-claim-release-spec.md) and [privacy](privacy.md).

- [Actions and authority](../crates/core/src/tickets.rs#L8-L125): Open/Claim/Close;
  Close requires configured staff role OR ManageChannels OR Administrator, not
  opener/claimant identity. States: Creating/Open/Closing/CleanupPending/Closed.
- [Close runtime](../crates/bot/src/ticket_runtime.rs#L1212-L1262) freezes opener
  writes, captures and commits before cleanup. [Capture](../crates/core/src/tickets.rs#L218-L239)
  sets closed_at before terminal cleanup; it is **not** terminal completion time.
- [Other terminal paths](../crates/core/src/tickets.rs#L287-L325) and
  [missing-channel retirement](../crates/cutover/src/tickets.rs#L292-L315) can
  reach Closed without a saved support close. **Closed alone is insufficient.**
  B needs a new durable receipt proving enrolled Open, accepted close source,
  committed capture and confirmed terminal cleanup, bound to the close token.
  Receipt requires both committed terminal state and cleanup evidence: successful
  DELETE or typed Discord 10003, with the same captured normal-close token.
  Atomically stamp DB terminal_confirmed_at with that proven terminal transition
  and eligibility; recovery preserves the first stamp. Never use existing closed_at,
  generic 404 or transcript presence as proof. Rollback/retirement/unsaved
  abandonment never qualify; missing capture provenance refuses.
- [Ticket fields](../crates/core/src/tickets.rs#L73-L84) and
  [schema](../crates/cutover/migrations/0210_tickets.sql#L2-L35) have opener,
  claimant and lifecycle times, but no closer, satisfaction or first-response facts.
  Claimant is not proven closer/responder. No completed-ticket reopen action exists;
  [interrupted-close recovery](../crates/core/src/tickets.rs#L242-L267) is different:
  it restores an uncaptured close to Open, which never became feedback-eligible.
- [Retention and erasure](../crates/cutover/src/tickets.rs#L338-L372) purges
  transcript bodies at capture +90 days, including pending cleanup; opener/current
  claimant erasure removes parent tickets. [Erasure inventory](../crates/cutover/src/member_erasure_plan.json#L218-L226)
  does not automatically cover new feedback, quotas, journals or responder facts.
  Ticket metadata has no age purge here. Live ticket activation is unverified.

Related **unmerged** designs, inspected at these exact heads:
[opener resolution #1779](https://github.com/TogetherWeOwn/two-bot-next/pull/1779)
(`3a4d690782ce1e0571caa207f7d3cc32bb129261`) proposes a non-destructive signal;
[inactivity #1783](https://github.com/TogetherWeOwn/two-bot-next/pull/1783)
(`3a264ad6cbee2f57a21d8b4f64bb25680f41903e`) proposes reversible soft-close, not
current Closed. [Private queue #1731](https://github.com/TogetherWeOwn/two-bot-next/pull/1731)
was open at inspection, not a shipped reporting seam. Recheck their accepted
contracts before any future build; none is a design dependency or authorization.

| Close path or adjacent event | Source / availability at inspection | Prompt / rating eligibility under B |
| --- | --- | --- |
| Staff Close | Current source | Enrolled new ticket becomes eligible only after confirmed Closed; optional invitation as §3. Failure/recovery before that yields no prompt |
| Opener “release” / resolution request | Proposal #1779, not implemented | No prompt: it is not Close and grants no close authority. If staff subsequently closes, use the same terminal receipt exactly once |
| Claimant self-release / reassignment | Adjacent design, not a current release action | No prompt, no score/attribution change; not resolution |
| Inactivity soft-close | Proposal #1783, not implemented | No prompt: inactivity is not resolution and soft-close is not terminal Closed. Keep-open/resume does not enroll another survey |
| Later staff Close after soft-close | Conditional composition with #1783 | Same terminal staff-close rule, tagged with prior inactivity context, never a second survey |
| Terminal auto-close or opener Close | Future; not authorized here | Its separate accepted contract must emit the same fenced terminal receipt and retain close-source classification; no special bypass. Until accepted, refuse this source |
| Failed/uncertain deletion, missing channel without terminal proof, erasure | Current safety/maintenance paths | No prompt; never infer success from a generic 404, reservation release or transcript presence |
| Completed-ticket reopen | Future; not implemented | §5 invalidates invitation and submitted contribution; never re-rate the same ticket |

## 3. Conditional B: private opener flow

All interfaces and bounds below are **proposed**, not commands/settings available
today. Keep feature default off. Enroll **new tickets only**, atomically when their
Open activation is confirmed under an accepted feature/config generation. Save
that activation time and policy revision; no enrollment from close replay, existing
history, restored transcripts or an absent enrollment row. Tickets already open
at enablement never enroll. Missing or expired enrollment always refuses.

Proposed private guild `/ticket feedback` has no target-user/guild or arbitrary
ticket selector. The authenticated opener sees at most **5** eligible own tickets,
newest terminal receipt first (ticket ID breaks ties), using opaque references
and UTC close dates only; no subject, channel name, staff identity, transcript or
case text. Keyset pages, max 5 rows each, no total/count oracle. Selecting a case
opens the same form as a valid DM invitation. Staff cannot submit on their behalf.

**Prompt:** “How helpful was the support for this ticket? 1 Very unhelpful,
2 Unhelpful, 3 Neither helpful nor unhelpful, 4 Helpful, 5 Very helpful. Optional
short comment. Staff managers can read feedback; it is not anonymous. Do not add
passwords, contact/payment/health details, allegations or other sensitive case
information. You can skip. Your answer does not affect support or access.”

Use five text-labeled score controls, not color/star-only semantics. Choosing a
score opens a private confirmation/modal with an optional comment; persist score
and comment **together only on explicit Submit**. Empty comment is valid. Cancel
saves nothing, does not consume the one rating and never queues work. No preselected
score, follow-up nudge, forced completion, staff notification or channel message.
A successful receipt says “Feedback saved”; skip says “Nothing saved.”
Bind controls/modal to application, saved guild, ticket/enrollment generation,
opener, selected score and operation; a handle or supplied score is never trusted
without validation. Forms expire after five minutes or the original window,
whichever is earlier. No retained drafts, prefilled comment history or attachments.

An opener may explicitly request **one DM invitation for this ticket while Open**.
Separate private consent, off by default; guild opt-in alone is not consent.
Consent is saved while Open; **send admission happens after qualified Close**
within §4's 24-hour limit. Saved consent carries across Close only for this
enrollment/window; no post-close DM opt-in or re-consent. A private **Cancel invitation** control in the
feedback view withdraws it; serialized before send admission, withdrawal wins.
Collection opt-out overrides all consent; guild/opener DM disable and Discord
refusal suppress delivery only. Self-service never enables delivery.
An admitted invitation contains only fixed generic survey copy and a random opaque handle; no guild/channel/person names, score,
comment, IDs, transcript or links. Handle lookup is not authority. Reauthorize
at form opening and submission. An invitation is not a second stored feedback copy.

No DM consent, suppressed/blocked DMs, recipient departure, expired consent or
notification uncertainty means **no unsolicited retry and no public fallback**.
Self-service remains available only while the same opener is an eligible guild
member within the original window. Suppression of feedback collection disables
self-service too; delivery suppression alone does not. Never redirect to staff,
a different member/channel, email, webhook, transcript or a bearer web link.
A Discord DM cannot be ephemeral or recalled reliably: even generic invitation
metadata can reveal an association. This residual needs independent acceptance.

### Authority and confidentiality

- Submit/list/own receipt: trusted interaction actor must equal saved opener,
  with current membership of the saved configured guild, matching application and
  accepted feature/policy generation. DM context supplies no trusted guild; resolve
  guild only from the saved enrollment, then fetch membership. Missing/stale facts
  fail closed. No opener proxy, supplied actor ID or service MAC grants authority.
- Recheck lifecycle, suppression, policy, deadlines and actor facts at commit
  admission, not just when showing the modal. Do not require a deleted ticket's
  channel ACL for post-close feedback; **this is a new, explicit opener-only grant**
  to feedback, never a transcript or historical-channel-visibility grant.
- Feedback reports and individual comments: only current same-guild Administrator
  or ManageGuild managers, reauthorized on each page/read and immediately before
  private release. Ticket staff role, claimant, closer or being the rated staff
  member alone is insufficient. No new dashboard/service/DB reader is implied.
- Foreign/missing/erased/ineligible case, wrong opener or unresolved authority:
  identical `NOT_AVAILABLE` response/status, no case existence, identity, score,
  state, window or counts. Authorized participants may get precise state errors.
  All replies are private, non-cacheable, with closed allowed_mentions; inert
  escaped text, no embeds/autocomplete/shared analytics. No public report option.
- Discord membership revocation and a private reply cannot be atomic with a DB
  commit. Recheck immediately before release; acknowledge the remaining in-flight
  disclosure race. If unacceptable, retain A rather than claim perfect secrecy.

## 4. Scale, text, rates and resource bounds

Require integer score **1–5** (not strings/floats). Optional comment: NFC, trim
ASCII space, empty becomes absent; otherwise **1–240 Unicode scalars and <=960
UTF-8 bytes**, single line. Reject controls/Unicode category C, line/paragraph
separators, `@`, `<`, `>` and backticks; no silent truncation or invalid-input echo.
Escape remaining Markdown/HTML and isolate direction at render, not in storage.
Validation does not detect all PII; notice and minimization are still required.

Proposed submission window: terminal_confirmed_at <= now < min(
terminal_confirmed_at + **7 elapsed days**, enrolled_at + **90 elapsed days**).
At equality, close submission/invitation eligibility, not retained-data reads.
An opener's private saved-receipt/withdrawal view remains available until the
retention ceiling; pending and saved views each use the same five-row bounds. DB UTC time and checked arithmetic define boundaries; injected
clock in tests, no client time. No deadline extension on retry, restart, repeated
close, reopen, configuration change or policy re-enable. Invalid/future chronology
refuses; no catch-up invitation. Invitation admission must occur within 24 hours
of that terminal receipt and before window expiry; later self-service only.

| Operation | Atomic/shared bounds, not process-local promises |
| --- | --- |
| Submission | Unique ticket + opener, one successful insert, one in-flight intent per ticket; <=5 admitted attempts/opener/10 minutes and <=100/guild/hour. Retry same intent consumes no additional quota |
| Invitation | <=1 wire attempt/ticket; <=1/opener/24 hours and <=20/guild/hour; losing quota means self-service only, not a later send queue |
| Private reads | <=10/actor/minute, <=60/guild/minute; 5-ticket or 10-comment pages; one report staff selector, no bulk selection |
| Background maintenance | <=25 affected physical rows/pass across feedback stores, including cascade fan-out; shared existing hourly privacy-maintenance budget, not reserved capacity or a deletion deadline (§7); no unbounded scans or new host timers |

Admit a random operation ID and durable invite state **before** any DM attempt.
Unknown admission commit means no send. Single-attempt transport, no automatic
429/5xx/timeout retry. Store confirmed message receipt if obtained; any ambiguous
wire or receipt-store result stays `delivery_unknown`, consuming the attempt
permanently, including crash before known wire delivery. A new operation ID cannot
bypass it. Self-service stays independent. A generic failure does not prove non-delivery. Close must never wait for optional
feedback work or fail because its prompt failed; no DB lock held across Discord I/O.

## 5. State, idempotency and races

Enrollment/feedback state is separate from ticket lifecycle. Conceptual fields:
ticket/guild/opener, enrolled_at/policy_generation, terminal_confirmed_at/close_source,
invitation consent/state/receipt, operation ID, status, score/comment/submitted_at,
and optional first-response facts (§6). Exact schema needs future accepted work.
Never alter close tokens, capture transaction, cleanup recovery or active admission.

| State / event | Atomic result / oracle |
| --- | --- |
| Enrolled Open; begin-close/capture/cleanup_pending | Still ineligible; zero prompts/scores |
| First confirmed Closed, enrollment live | One eligibility transition; independent one-attempt invitation only with fresh consent/quotas; no close replay extension |
| Eligible + valid first Submit | One `rated` row and receipt; score+comment+operation commit atomically; no second rating |
| Two submissions race | One wins under unique constraint/ticket serialization; other gets `ALREADY_RATED`, never last-write-wins |
| Same operation retry / changed body | Return bounded receipt only, never repeat DML/quota/DM or echo old comment. Reused key does not attest changed content; require fresh authorized read for truth |
| Commit timeout / reply loss | `SAVE_UNCONFIRMED` until reconciliation by the same operation; confirmed stored commit remains saved even if reply failed |
| Future reopen before or after rating | Serialize with submission: invalidate invite, remove score/comment/response contribution, mark enrollment invalid for this ticket until Open+90 days (then age refuses); no second eligibility or re-rate. If Submit wins first, reopen still removes it |
| Submission-window expiry | No new submit/invite; retained authorized receipts/reports/withdrawal still work until retention expiry |
| Retention expiry, feedback opt-out, erasure, feature disable | Block reads/submissions/sends immediately. Disable does not erase or extend retained rows; opt-out/erasure remove contributions as §7; no queued replay on re-enable |
| Claim/release/reassignment racing with rating | Claim is not rating ownership or responder attribution; no invitation or score changes. Parent-ticket erasure still cascades |

A missing enrollment or purged parent cannot be recreated by a handle, old operation,
old close receipt, delayed worker or restored data. Persist invalidation until the
90-day enrollment ceiling; after that age alone refuses. No indefinite feedback
tombstone exemption. Unknown commit is not success, and a repeated close is not a
new ticket. Editing/replacing a rating is excluded; opener may withdraw/delete it
privately, permanently ending this ticket's feedback eligibility. Withdrawal and
reopen atomically clear contribution/DM consent and retain only invalid enrollment
identity/status/time/policy until Open+90 days; no score/comment/responder survives.
All writers, including Submit/close replay, lock the same enrollment; erasure may
delete it outright because absence refuses. Thus withdrawal racing Submit never
leaves a contribution once withdrawal has committed.

## 6. Manager reads, attribution and response-time hookup

Proposed private `/ticket feedback-stats` accepts **guild summary OR one staff
member**, never a leaderboard, sorted staff table or public target channel. Fixed
period = last completed UTC calendar month of terminal_confirmed_at, never today's
capture-based closed_at; no arbitrary dates, rolling windows,
category/priority/close-source filters or exports that facilitate differencing.
Read only retained eligible facts, max 10,000 cohort rows; above the bound refuse
`REPORT_TOO_LARGE`, never silently sample. Report carries schema version, period,
as_of, policy revision and coverage/missing-data status. No names of openers,
per-ticket scores or comments in this aggregate response.

One attribution per ticket: **first qualifying human staff response**, frozen
from authenticated same-channel MESSAGE_CREATE observed after Open activation,
with current staff authorization and channel access proven at observation. Exclude
bots/webhooks/system messages, slash acknowledgements, claim/close actions and the
opener's own messages. Attachments-only human replies qualify without fetching or
storing attachments. Neither final claimant nor closer gets credit by default.
No staff reassignment/claim/release can rewrite attribution or select favorable
scores. Staff means configured staff role OR ManageChannels OR Administrator.
Persist exclusion facts at enrollment, qualified terminal receipt and submission;
if opener is staff at any of those points or equals the responder, exclude the
ticket from rating **and timing** aggregates. Unknown role facts exclude rather
than guess. Never retrospectively infer historical staff status from today's roles;
report-time role checks authorize the reader, not rewrite old attribution.

**Missing prerequisite:** current code does not supply this continuous, trustworthy
first-response observation. A future accepted observer must retain only first
responder ID, event ID and validated time/coverage; never message content, URLs,
name or history scans. Without continuous coverage from Open through that response,
mark response and attribution **unknown**, not zero, slow, unclaimed or inferred
from a transcript. No historical backfill. A continuity gap invalidates timing
for that ticket, not other proven tickets; no-response needs coverage through
qualified terminal closure. Exact gap/continuity proof is a prerequisite, not a
claim that a saved gateway session suffices. Tickets without proven responder may
contribute to guild satisfaction only, clearly unattributed; never a staff score.

Response latency = validated first qualifying response time minus enrolled Open
time, milliseconds; invalid/future/negative chronology is unknown. Capture source
and clock-validation policy must be accepted before observation is enabled.
Stats use the same retained closed-ticket month cohort: answered count, unknown
count and **p50/p90 first-response elapsed time**, no SLA attainment or time-to-close
substitute. Nearest-rank percentile: sorted durations, element ceil(p*N), 1-based.
Missing data never becomes zero; distinguish no response observed under complete
coverage from an ingestion gap. An unresponsive ticket has no measured duration.

Rating fields: rated count, mean (one decimal, half-up), 1–5 histogram and coverage
label. Do not show “response rate” until an accepted eligible denominator exists;
DM attempted/delivered is not consent or a survey response. Ratings are opt-in and
selection-biased. Staff gaming via fast empty replies, alternate accounts or asking
for high scores is not solved by identity checks; no rewards/targets and no claim
that these figures objectively rank support quality.

Release numerical rating statistics only with **>=5 distinct eligible rated
openers** in the selected cohort, after exclusions. Separately require >=5 distinct
openers with **measured durations** for timing; unsubmitted ratings, unknown timings
and repeated tickets from one person cannot inflate either threshold. Otherwise
show only `INSUFFICIENT_SAMPLE`, not exact small counts, means, histogram or timing.
Guild summary and targeted staff reads are not proven anonymous: cross-cohort
subtraction and erasure differences remain a CISO decision. No immutable published
rollup or cached pre-erasure total. Every report is labeled retained_only and
completeness_unproven: aged-out rows are absent, not counted as unknown or zero.
Coverage counts concern retained rows only; never compare a partial month as complete.

Individual short comments are a separate deliberate manager read (10/page, no
search/export), newest retained submission first with operation ID tie-break and
keyset pages; no date/staff filter, total, opener ID/name or transcript join. These
are available immediately for their seven-day lifetime, not held for monthly
reporting. Small samples can still identify writers: comments are not anonymized
by the numerical threshold; explicit notice and privacy acceptance are mandatory. Staff may not
edit, hide selected low scores, retarget feedback or trigger a re-survey. Managers
may remove an unsafe comment only, audited without its text; numeric score stays
unless withdrawn/erased under policy. There is no public or staff-self score page.

## 7. Minimization, retention, erasure and restore

Proposed **logical-use expiry maximums**, not physical-erasure deadlines; explicit
privacy-policy disposition and the physical-purge prerequisite below remain required:

- Score, opener/responder link, response facts, operation/invalidated enrollment
  and invitation journal expire no later than **90 days from enrolled Open**, not
  last edit/close. Pending Open tickets age out too. Comments expire **7 days after
  submission** or that ceiling, whichever is earlier. The separate immediate
  comment read never prolongs logical use to match monthly numerical reporting.
- Reads exclude expired facts immediately, including consent and receipts;
  submissions/sends cannot use expired facts either. Submission/read quota facts
  expire within 1 hour; the invitation's actor-bearing anti-spam admission fact
  within 24 hours, always bounded by enrollment expiry and erasure. These are
  logical expiry limits too; expired stored bytes await physical cleanup below.
  No permanent free-text audit, body hash, prompt payload, raw REST/SQL error,
  feedback-derived trace, metric label or transcript appendix.
- Member erasure removes entire feedback contributions for opener, attributed
  responder, consent actor and any saved actor identity. Preserve existing
  opener/current-claimant ticket-erasure semantics and cascade feedback when the
  parent is removed; new responder/consent selectors need explicit inventory entries.
  Do not merely null opener ID and keep their score in aggregates. Recompute reads
  from remaining eligible rows; if threshold falls below five, suppress figures.
- Collection opt-out immediately invalidates outstanding invites and removes that
  opener's stored contribution and associated journals; no re-enrollment. Analytics
  opt-out semantics for this new collection are **not established** by ordinary
  ticket-lifecycle exemptions: absent accepted classification, do not enroll.
- Audit only action/result, policy revision, opaque operation/ticket and necessary
  actor/time; no score/comment/text hash, names or raw payload. Linked audit and
  operational state share the ceiling/erasure selectors. A retained integrity fence
  must have an accepted finite purpose, not secretly retain the feedback body.
- Backup/restore must replay deletion/suppression before feedback reads or sends
  resume. Neither old enrollment nor old DM journal can resurrect data or resend.
  Trace journals, pre-images, caches and backup lifetimes in the accepted design;
  existing transcript purge alone is not proof. Local erasure cannot recall Discord
  notifications, user screenshots or manager copies. Unknown restore guarantees
  keep B off; no claim of end-to-end deletion today.

**Physical cleanup is asynchronous best effort, not a one-hour guarantee.** The
25-row ceiling is an upper work bound, not a promise of that much service. The
[current cadence](../crates/core/src/tickets.rs#L13) is 3,600 seconds;
[missed/failed passes](../crates/bot/src/ticket_runtime.rs#L314-L347) can defer work.
Even an isolated backlog of N expired physical rows, no new arrivals and full
25-row service needs ceil(N/25) successful passes. Thus 26 comments expiring just
after a sweep need two later sweeps, before counting consent, receipts, quota,
audit or other journal rows; one fully failed pass needs a third opportunity.
Shared work, ongoing arrivals or outages can delay cleanup further. This proposal
establishes **no unconditional wall-clock physical-purge bound**; immediate logical
suppression does not prove that stored bytes were deleted.

**Unresolved acceptance prerequisite, not permission to retain indefinitely:**
before B can be accepted for implementation, specify a finite physical-erasure
lag across all affected stores, maximum row fan-out and global expiry/admission
load (per-guild submission quotas alone are insufficient), shared-budget drain
allocation/fairness, backlog admission stops and failed/interrupted-pass recovery.
Prove the chosen lag with CSAT-12, counting every affected row, not just parent
contributions or successful DML statements. Reconcile backup/pre-image retention
and restore suppression separately. Do not silently extend logical-use limits,
add timers or weaken existing privacy policy to make capacity fit. Missing capacity
or privacy acceptance keeps B off and A/no-build in force.

## 8. Design matrix and future slices

These are **planned oracles, not executed tests**. Independent Test Author owns
future immutable acceptance tests; builder must not edit them for GREEN. Use
synthetic identities, injected clocks, unique channels and disposable test DBs.
Record fixed outcome/row/send counts, never real comments or member data.

| ID | Given / when / required result |
| --- | --- |
| CSAT-01 | Staff Close vs pending/failed/uncertain cleanup: exactly one eligibility after terminal proof, otherwise zero; never block/retry Close because of feedback |
| CSAT-02 | Opener resolution, claimant release, inactivity warning/soft-close/keep-open: zero prompt; later staff Close uses the single original enrollment |
| CSAT-03 | Future terminal auto-close without separately accepted authority: refused; accepted source must use identical receipt/window fences and cannot bypass opt-out |
| CSAT-04 | 1/5 valid, 0/6/fraction/string rejected; empty/240/241-scalar and 960-byte comments, NFC/RTL/control/mention boundaries: reject without truncation, echo or ping |
| CSAT-05 | Two submits, changed-body replay, crash/unknown commit and failed private reply: one row; receipt-only reconciliation; never overwrite or false success |
| CSAT-06 | Future reopen before/after/concurrent Submit, second Close, withdrawal and late close replay: invalidate/remove contribution, no re-rate/re-enrollment |
| CSAT-07 | Erased opener/responder/current claimant, opt-out, purge and restore: no contribution or resend; sample dropping to four suppresses statistics, no stale cached rollup |
| CSAT-08 | Claimed-by closer differs from first responder; self/staff-opened/bot ticket and claim churn: no reassignment credit, no staff/self aggregate; unknown coverage gives no invented duration |
| CSAT-09 | DM suppression, no consent, guild departure, 403/429/5xx/timeout and quota loss: <=1 wire attempt, no fallback disclosure, eligible self-service independent of DM outcome |
| CSAT-10 | Wrong guild/opener/application, forged/expired handle, revoked manager, stale modal: same nonparticipant refusal; no comments/scores/counts/IDs disclosed |
| CSAT-11 | Five distinct openers vs four, repeated tickets by one opener, unanswered/gapped timing, partial retention, exact percentile/rounding fixtures and 10,001 rows: suppression/unknown/refusal, never ranking or sampling |
| CSAT-12 | Window/logical-expiry equality, rates at/over bounds, disable/re-enable, erasure racing send/read: no mutation past admission fence; residual Discord race documented. Expire 26 comments just after a sweep plus consent/receipt/quota/audit rows; interrupt a pass and retry: immediate read/send suppression, <=25 total affected rows/pass, committed deletions survive retry, no skipped/resurrected rows. Under the accepted global load/fan-out/shared-budget and failure envelope, prove complete physical cleanup within the separately accepted finite lag (§7), never infer it from logical expiry |

Cut only **after** accepted exact intent, independent privacy disposition and
protected-test handoff. Each future row is a separate <=1-day PR-sized estimate,
not a current task creation or a promise of one-day complete delivery. Split again
rather than ship an unsafe half-enabled flow; flag stays off until all gates pass.

| Slice | Size / dependency / acceptance exit |
| --- | --- |
| Enrollment and finite lifecycle policy | <=1 day; accepted spec first; pure clock/state/close-source oracles including no reenrollment |
| Feedback persistence + privacy inventory | <=1 day; enrollment contract; unique submit/CAS, atomic withdrawal, cascade, expiry/erasure and restore coverage; no activation |
| First-response observation contract | <=1 day; accepted coverage/clock policy; content-free attribution, gaps and nonhuman controls; no report availability claim |
| Private opener list/form/receipt | <=1 day; persistence; reauthorization/nonparticipant bounds, one atomic score/comment, no DM |
| Optional consented notification | <=1 day; lifecycle + consent/privacy proof; independent one-attempt journal/restart tests; closed DMs remain self-service |
| Manager aggregate/comment reads | <=1 day; trusted observer + persistence; threshold/privacy/percentile fixtures and role-revocation controls |
| Bounded purge/restore wiring | <=1 day; §7 finite physical lag/capacity policy accepted and all stores known; immediate suppression, expiry-burst/interruption proof, finite audit/quota cleanup, restore fence and operational receipt |
| Fenced staging validation | <=1 day; all prior exact-head checks/review and normal execution contract; synthetic-only matrix evidence, health and rollback; production follows its existing pipeline |

## 9. Review disposition and no-build stop

Review gate not before **2026-11-02**: CEO/host intent alignment through the normal
router must bind the task-spec revision, repository commit and file SHA-256; do not
ask the owner for a routine click. This design does **not** supply that alignment.
Independent CISO acceptance is required for new feedback/opener/manager data access,
opt-out classification, DM consent and residual race, logical expiry and a finite
capacity-backed physical-erasure/restore policy (§7), small-cohort inference and
first-response observation. No tests/build cards before
these dispositions, no default/timeout approval. A revised spec invalidates earlier
acceptance. Existing security/activation/secret fences remain unchanged.

Design metric: by intent review, all three alternatives and twelve matrix rows have
explicit accept/change/reject dispositions. No measured satisfaction, response time,
demand or production benefit claimed. Unknowns include live ticket activation,
actual permissions, eligible cohort size, observation continuity and safe restore.
If usefulness, privacy or complete bounded implementation cannot be established,
choose A/no-build; do not weaken gates to manufacture data.

Verification for this docs-only change: docs index and path-conformance suite;
future matrix deliberately not executed. ACT: publish an indexed Proposed contract.
WHY: make feedback private, voluntary and non-destructive before collecting data.
UNDO: revert docs; no runtime state or data was created.
