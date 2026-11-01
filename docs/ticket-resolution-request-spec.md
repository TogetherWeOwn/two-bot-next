# Tickets: an opener's resolution request is not Close

Status: **Proposed — design only**. Written 2026-10-26; review by 2026-11-02.
This is the repository source of truth; the task spec mirrors it. Merge publishes
neither an enabled command nor implementation approval. No runtime, schema,
permission, credential, spend, retention-policy or live-operation change here.

## Decision and alternatives

Consumer: a member who no longer needs help with their own open ticket, and
staff deciding whether to close it. Benefit is a hypothesis; demand, frequency
and saved staff time are unmeasured.

| Option | Value and cost | Disposition |
| --- | --- | --- |
| A. No build; tell staff in the existing ticket | No new data or command; staff reads the member's message and decides what to do | **Recommend no-build now**; safe complete runtime delivery within one day is not established |
| B. One private resolution signal, with irreversible withdrawal of that signal for this ticket | Bounded structured assertion, no notification; needs durable state and an authorized staff read | Smallest conditional model for intent review, not an approved build |
| C. Member Close, staff ping, auto-close or repeated request/withdraw toggle | Faster apparent closure but adds destructive authority, noise or replay/rate complexity | Excluded, not a shortcut to B |

[Ticket Tool's manual](https://ticket-tool.app/docs/commands/tickets), retrieved
2026-10-26, says `/close-request` asks the opener to confirm closure. It mentions
the opener and offers Accept & Close / Keep Open. That is **staff-to-opener** and
can close; it is not evidence for this proposed silent opener-to-staff assertion.
Current staff Close and any separate staff Confirm/Cancel proposal remain
independent. No dependency on ticket lists, subjects, collaborators or claim release.

## Verified baseline and limits

Inspected repository head: `b9db8193af9e99495402a5b94e0d6bc67f016b61`.
Repository facts are not production-state or acceptance evidence:

- `crates/core/src/tickets.rs:8-35,106-125` defines Open, Claim and Close and
  configured staff role OR ManageChannels OR Administrator authorization;
  no opener resolution request exists in that parser.
- `crates/bot/src/ticket_runtime.rs:1030-1068` routes Close directly into the
  privileged lifecycle. Never reuse it as the request handler. New reservations
  get 128-bit random IDs at `crates/bot/src/ticket_runtime.rs:1084-1088`.
- `crates/cutover/src/tickets.rs:108-159` has ordinary `by_channel` reads and
  guild-scoped row-locked transitions. The schema at
  `crates/cutover/migrations/0210_tickets.sql:2-35` has no signal or SQL identity
  immutability fence. Neither lookup nor random ID is a new enforced incarnation
  contract. The runtime's existing staff predicate is not fresh actor-channel
  access proof.
- `crates/cutover/src/tickets.rs:338-372` purges expired transcript bodies,
  **not parent ticket rows**; member erasure deletes parent rows. A 90-day
  transcript ceiling therefore cannot be claimed for proposed signal metadata.
- `crates/cutover/src/rollback_import.rs:440-452` imports whole JSON records;
  missing added fields become NULL rather than defaults.
  `crates/cutover/src/rollback_delta.rs:267-273` observes lifecycle times only;
  request-only writes need explicit discovery coverage.
- [Ticket runtime](tickets.md) documents capture-before-delete, guild-scoped
  lookups, current staff authorization, bounded admission, recovery and privacy.
  [Claim release](ticket-claim-release-spec.md) is a separate design, not an
  installed generation/audit facility to depend on.

No Discord member/channel, live database, applied migration, deployed binary or
host intent was inspected. No runtime tests are claimed. Future implementation
must recheck these seams at its exact source head.

## Smallest state and visibility model

One durable marker on **one ticket incarnation**, never a per-user queue:

| Marker / revision | Meaning | Allowed next state |
| --- | --- | --- |
| `none / 0` | No recorded assertion | `requested / 1` or `withdrawn / 2` |
| `requested / 1` | The opener asked staff to consider closure; staff has not confirmed resolution | `withdrawn / 2` only |
| `withdrawn / 2` | No active request; further requests for this incarnation are disabled | None |

Withdrawal is deliberately final **for the signal**, not for the ticket. From
`none`, it also latches `withdrawn`, cancelling the ability of an in-flight first
request to take effect later. Tell the member this before/with the private result.
They can still talk to staff; the ticket stays open. There is no re-request,
reset, expiration-driven rearm, staff override or staff acknowledgement state.
At most one request and one withdrawal commit per incarnation; repeat deliveries
are no-ops. Two operations raced under the same row lock follow this table,
including withdrawal-before-request. No history-sized receipt table is needed.

Proposed entry points, **not registered commands**:

- `/tickets resolution request`, `withdraw` and `status`, in the ticket channel,
  no member/ticket/channel selector, reason, free text, attachment or force option.
  These are opener-only, including when an administrator invokes them.
- `/tickets resolution inspect`, in that same channel, read-only for current
  staff with current channel access. No list, cross-channel search or transcript.
  This independent private read makes B useful without depending on queue delivery.
- No shared/public component, confirm button or modal. Stale/forged component IDs
  for this proposal are rejected, never translated into Close. An existing Close
  component still follows its existing staff gate; opener identity is no bypass.

Read outcomes are explicit: marker 0 gives `no_op / not_requested`; marker 1
`no_op / already_requested`; marker 2 `no_op / already_withdrawn`, for both opener
status and staff inspect. Staff copy is respectively “No active opener resolution
request”; “Opener requested staff review for closure. **Unconfirmed member
assertion — not staff-confirmed resolution.**”; or “No active request (withdrawn).”
Opener status uses the corresponding result-table copy below.
Only the current authorized reader sees it. No public marker, old-message edit,
queue badge, notification, DM, ping, unsolicited send or transcript is produced.
Staff may separately run existing Close after their own decision and current
authorization; inspect never prepares, confirms, schedules or accelerates Close.

## Identity, authority and freshness

Preserve configured-guild, feature/activation, application identity, human-actor,
shared admission/deadline and private-reply fences. A successful ephemeral defer
precedes any write; failure admits no marker mutation. Actor comes only from the
authenticated Discord interaction, never options, JSON or a component identifier.

1. Resolve the invoking channel in the trusted guild to the exact current ticket.
   Capture `(guild_id, ticket_id, opener_id, channel_id)` before any lane wait;
   never retarget the interaction to a newer row afterward. `ticket_id` is the
   **incarnation/generation ID**, not channel name, topic or status. A new ticket
   must receive a fresh runtime-generated ID; callers cannot supply/recycle IDs.
   Legacy/ambiguous identity that cannot prove this contract refuses unavailable.
2. For request/withdraw/status require `actor == current opener`. Neither a
   collaborator, claimant, staff role nor Administrator can act for the opener.
   Inspect instead re-evaluates the existing staff predicate from fresh invoking
   member/live role configuration; staff status alone does not prove access.
3. Independently prove current channel existence, same guild/ID, supported type
   and actor ViewChannel + SendMessages from trusted current Discord metadata.
   The bot must be able to view/respond privately. No history/content fetch,
   probe post, permission repair, invite or role grant is allowed. Unknown,
   inaccessible, deleted or foreign context returns generic unavailable **before**
   revealing marker, opener, claimant or ticket ID. An old interaction is not
   current access proof. All four actions, including no-ops, reauthorize.
4. Metadata proof expires after five monotonic seconds. Check before transaction
   entry, after acquiring the row lock and before commit/read return. A lock wait
   or policy/config invalidation that exhausts proof causes rollback/refusal;
   never refresh via Discord I/O inside a transaction. No external read can be
   atomic with Discord ACL changes: a change after the final read is a bounded
   residual race, not a guarantee of instantaneous access revocation.
5. Under the same guild-scoped ticket row lock as close, recheck exact captured
   identity/channel/opener, `status == open`, current policy revision and proof.
   Missing/replaced row or changed opener/binding refuses; do not adopt its new
   identity. `creating`/`closed` are `unavailable / context_unavailable`.
   Proven authorized `closing` or `cleanup_pending` gets `unavailable / closing`
   with the private closing response, never invokes recovery.

For B, immutable `(id,guild_id,opener_id)` and non-rebinding of an activated
channel must be **enforced and tested across every writer**, not assumed from a
lookup. If an allowed legacy/import writer can replace that binding, keep B dark
until a separately accepted incarnation fence exists. No direct SQL workaround.
Normal close/reopen does not create a new incarnation or reset the marker:
`withdrawn` stays withdrawn. A surviving `requested` is a **historical member
assertion**, never newly confirmed by recovery; show that qualifier after reopen.
No request survives deletion/recreation as an active assertion for a new ID.

## Persistence, concurrency and privacy prerequisites

Candidate additive ticket fields: closed marker vocabulary above and nullable
`resolution_requested_at` / `resolution_withdrawn_at` database timestamps. These
are **proposed fields, not executable DDL or migration approval**. Marker is a
monotonic revision, not a time-derived version: no path may decrement/reset it.
A same-incarnation restore rewind must not revive old admitted work (see below).

- Each action uses a short transaction: lock exact `(guild,id)`, revalidate,
  conditionally update only marker and its corresponding timestamp, commit.
  Persisted return values come from that transaction. Request: `0 -> 1`;
  withdraw: `0|1 -> 2`. Only actual transitions stamp time; no-op does not refresh
  time or create audit. Keep opener, claimant, channel, lifecycle/times, cooldown,
  permissions and transcript bytes unchanged. No DELETE, transcript INSERT,
  begin-close call, job, webhook, send-admission reservation or recovery wake.
- Close first -> closing response and no signal write. Request first -> marker
  commits, then separately authorized Close follows its unchanged freeze →
  capture → atomic snapshot/cleanup → deletion contract. The marker must never
  be read as a close precondition, permission grant or proof of capture. Old
  lifecycle persistence must not overwrite marker fields from a stale struct.
- One row bounds audit evidence: opener is already on the immutable parent;
  request/withdraw timestamps and monotonic state record at most two events.
  This is **not** a complete interaction log, signature or staff endorsement.
  No duplicated actor IDs, reason/body, channel-name, token, custom-ID, IP or
  transcript data. No new operational-audit/mirror event, event bus or analytics
  copy; do not claim the existing separately pooled audit insert is atomic here.
- The marker/timestamps are linked personal data, not anonymous counters. Both
  erasure executors and export/dry-run coverage must include them. Parent deletion
  removes them; no post-erasure tombstone or identity copy. Withdrawal suppresses
  the assertion but is not GDPR erasure or transcript deletion. **Retention gap:**
  parent-linked storage alone has no verified time ceiling because normal purge
  retains parent tickets. Do not silently inherit indefinite assertion storage
  or call it 90-day retention. A bounded metadata-lifetime policy and a proven
  cleanup/replay contract would need separate exact intent/security acceptance;
  changing retention policy is outside this slice. This unresolved gap is a
  concrete reason to choose A, not permission to build B or add a timer.
- Restart reloads committed state, never infers it from messages. Failed reply
  after confirmed commit leaves state intact; do not compensate or re-request.
  Lost commit acknowledgement is `commit_unknown`, not success or rollback.
  A later status reads current state only; it cannot attribute an uncertain
  earlier attempt. Blind retry uses the same monotonic operation and never
  reactivates withdrawn state, but inspect first after uncertainty.
- Old readers/writers, migration defaults, row decoders, rollback import/export,
  rollback-delta discovery and erasure must preserve state/timestamps. Missing,
  NULL or invalid witness/schema is unavailable, not default `none` in a new
  reader. Existing rows can initialize only via reviewed additive migration.
  Rollback disables new commands, retains additive data and invariants; no down
  migration or reset. Import/restore keeps B dark while admitted old workers
  drain; all runtimes restart, discard old queued interactions and refuse
  pre-start interactions before B is admitted. Restore re-applies erasure before
  reads. Unknown drain/rewind evidence keeps B dark; no restore is authorized here.

Request frequency is bounded durably by the monotonic state, across processes
and restarts. Invocation/metadata reads still use existing shared per-member,
global admission and deadlines; B must explicitly enroll these slash actions,
not assume the component-only admission protects them. No unbounded task queue,
automatic retry, refusal audit stream or metric label containing member/ticket
IDs. If these storage/privacy/compatibility requirements exceed the timebox,
retain A rather than ship an in-memory bit or weaken the fence.

## Private results and agent contract

Private replies are plain text <=300 characters, parsed mentions and replied-user
pings disabled. No names, message excerpts or member assertions are interpolated.
“Saved” requires a confirmed commit. All branches allow only the expected private
acknowledgement/edit, not a public send or DM.

| outcome / reason / next_action | Bounded example |
| --- | --- |
| `saved / requested / await_staff_decision` | “Request recorded. Staff have not confirmed resolution. Your ticket stays open; no one was notified.” |
| `no_op / already_requested / await_staff_decision` | “A request is already recorded. Your ticket stays open; no one was notified.” |
| `saved / withdrawn / contact_staff_if_needed` | “No resolution request is active. Further requests are disabled for this ticket. You can still talk to staff here.” |
| `no_op / already_withdrawn / contact_staff_if_needed` | “No request is active. Requests remain disabled for this ticket.” |
| `no_op / not_requested / none` | “No resolution request is recorded. Your ticket stays open.” (status only) |
| `denied / request_withdrawn / contact_staff_if_needed` | “Requests are disabled for this ticket. You can still talk to staff here.” |
| `unavailable / context_unavailable / verify_access_or_contact_staff` | “Resolution requests are unavailable here. Ask staff if you need help.” |
| `unavailable / closing / await_existing_close` | “This ticket is already closing or awaiting cleanup. No resolution request changed its close process.” |
| `unavailable / store_unavailable / retry_private_status` | “The request could not be saved. Check its private status before trying again.” (known pre-commit failure only) |
| `unavailable / commit_unknown / inspect_private_status` | “The result is unconfirmed. Check private status before trying again.” |
| `preview / eligible / issue_fresh_discord_request` | “Eligible at this read only. Nothing was saved or reserved.” |

`reason` and `next_action` are finite enums exactly as listed; malformed action,
forged/stale component and failed proof collapse to `context_unavailable`.
`retryable` is false for all results: guidance requires a fresh deliberate action,
not an automated write retry. Authorized staff inspect uses the status outcomes
and the explicit unconfirmed assertion copy above; it returns no opener identity.

No new agent CLI, MCP/HTTP mutation, impersonation or production/dry-run flag
combination is offered. Diagnostic JSON is a handoff shape, not authority:

```json
{"schema":"tickets.resolution.v1","mode":"diagnostic","guild_id":"g1","ticket_generation":"0123456789abcdef0123456789abcdef","revision":"1","outcome":"no_op","reason":"already_requested","retryable":false,"next_action":"await_staff_decision"}
```

IDs/revision are strings; ticket generation/revision are null before authority
or for unavailable/foreign context. No actor/opener/channel/content fields.
Typed `mode=dry_run` means read-only validation with the same authority checks,
no marker/audit/timestamp change, reservation, control, queue or send beyond the
private response. It predicts eligibility, not success, and mints no future grant.
The offline check below has no live mode and cannot reach Discord or a database.

## Threat model and deterministic zero-destructive-effect matrix

Boundaries: authenticated interaction → private router → current channel/actor
proof → guild-scoped locked row → private result. Assets: ticket privacy, staff
close authority, replay safety, transcript integrity and bounded bot capacity.
Spoofed actor/selector refuses; forged UI never reaches Close; stale/ABA work
cannot retarget; private authorization precedes all detail/no-op reads; monotonic
state prevents request spam and withdrawal replay. Minimal row evidence limits
repudiation but does not prove real resolution. External ACL TOCTOU and the lack
of push notification are explicit residual limitations requiring intent review.

Fixture base: configured g1, open T1/c1, opener A, staff S with access; marker 0.
Reset fixtures per row; use synthetic IDs, injected clocks, independent disposable
DB connections and barriers, not sleeps. Injected close/erasure effects are
attributed to those separate actors, never to the request. For **every** row,
assert zero request-path transcript/history reads or writes, channel DELETE,
permission/role mutation, claim transfer, public sends/edits, DMs, mentions,
close calls and enqueued jobs. Allow only bounded metadata reads and the private
reply; fixture state changes only where explicitly named.

| Case | Order / required result |
| --- | --- |
| request | A request 0 -> saved/1, one timestamp, lifecycle/claimant unchanged |
| repeat | Request 1 twice -> already_requested/1, timestamp byte-identical |
| withdraw | A withdraw 1 -> saved/2; next request -> request_withdrawn/2 |
| withdraw_empty | Withdraw 0 -> saved/2 with no request time; delayed request cannot create 1 |
| repeat_withdraw | Withdraw 2 twice -> already_withdrawn/2, no timestamp refresh |
| absent_foreign | Missing row, DM, g2, wrong application/channel -> unavailable, no IDs/writes |
| non_opener | Collaborator, claimant, staff or administrator B requests/withdraws -> unavailable, no details |
| opener_change | Hold captured A/T1; change opener/binding before lock -> unavailable, no adopt/write |
| access_loss | A or S loses access before final proof/lock -> unavailable, no detail/write |
| staff_visibility | S inspect gets unconfirmed assertion privately; nonstaff/removed S gets unavailable |
| stale_component | Any old/forged resolution custom ID -> unavailable, never routed to Close |
| recreated | Delete T1; c1 now maps T2; held T1 refuses; new T2 begins 0 only after fresh admission |
| concurrent_request | Two connections/barrier request 0 -> exactly one saved, one already_requested, marker 1 |
| concurrent_withdraw | Two withdrawals -> exactly one saved, one already_withdrawn, marker 2 |
| request_withdraw_race | Both lock orders -> final 2; request either saved then withdrawn or refused after withdrawal |
| close_first | Authorized close acquires row first -> closing response, marker unchanged |
| request_first | Request commits first -> marker 1; separate staff close retains existing capture-before-delete oracle |
| reopen | Interrupted close reopens same T1 -> marker unchanged; requested stays historical/unconfirmed; 2 never resets |
| restart | Restart after commit before reply -> reload marker; request replay no-op, withdrawal not undone |
| reply_failure | Commit then private edit fails -> state retained, no retry job/compensation |
| store_failure | Inject pre-commit failure/rollback -> store_unavailable, no transition |
| commit_unknown | Drop COMMIT acknowledgement -> unknown; status reports state, not attribution |
| erasure | Parent/member erase removes marker/times; foreign guild untouched; erasure dry-run changes nothing |
| lifetime | Transcript purge retains parent marker, never resets it; parent erasure removes it; no 90-day metadata ceiling is claimed, so B stays dark pending lifetime policy |
| proof_expiry | Hold row lock beyond five seconds -> unavailable/rollback, no network inside lock |
| dry_run | Eligible request -> preview/eligible, byte-identical state/audit, no reservation |
| overload | Saturate shared admission -> bounded private refusal/no spawned unbounded work or marker write |
| compatibility | Old writer/rollback export preserves witness; missing/invalid schema or rewind with undrained workers stays dark |

Runnable **document consistency**, not implemented security acceptance:

```sh
python3 -m doctest -v docs/ticket-resolution-request-spec.md
python3 scripts/check_docs_index.py --json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
```

```pycon
>>> import json, pathlib
>>> text = pathlib.Path("docs/ticket-resolution-request-spec.md").read_text()
>>> rows = text.split("| Case |", 1)[1].split("\n\n", 1)[0].splitlines()[2:]
>>> cases = [row.split("|")[1].strip() for row in rows]
>>> assert len(cases) == len(set(cases)) == 28
>>> assert {"recreated", "opener_change", "request_withdraw_race", "close_first", "restart", "erasure", "commit_unknown"} <= set(cases)
>>> # Abstract oracle only: no production parser, store, auth or transport is run.
>>> request = {0: (1, "requested"), 1: (1, "already_requested"), 2: (2, "request_withdrawn")}
>>> withdraw = {0: (2, "withdrawn"), 1: (2, "withdrawn"), 2: (2, "already_withdrawn")}
>>> assert all(request[s][0] >= s and withdraw[s][0] >= s for s in range(3))
>>> assert withdraw[request[0][0]][0] == request[withdraw[0][0]][0] == 2
>>> assert request[2][0] == withdraw[2][0] == 2
>>> sample = json.loads(text.split("```json\n", 1)[1].split("\n```", 1)[0])
>>> assert sample["schema"] == "tickets.resolution.v1" and sample["revision"] == "1"
>>> assert sample["reason"] == "already_requested" and sample["retryable"] is False
>>> assert not {"actor_id", "opener_id", "channel_id", "content"} & sample.keys()
>>> result = {"schema": "tickets.resolution.design-check.v1", "cases": len(cases), "outcome": "checked", "reason": "offline_oracle_only", "next_action": "seek_exact_intent_and_boundary_disposition"}
>>> print(json.dumps(result, sort_keys=True))
{"cases": 28, "next_action": "seek_exact_intent_and_boundary_disposition", "outcome": "checked", "reason": "offline_oracle_only", "schema": "tickets.resolution.design-check.v1"}

```

This arithmetic checks the proposed monotonic model and case inventory only.
Future real caller tests must prove SQL serialization, binding/ACL checks,
non-disclosure, erasure, compatibility and zero destructive wire effects. No
passing doctest constitutes protected acceptance, runtime readiness or demand.

## Proposed-only disposition, future slice and undo

**Boundary disposition: DESIGN_ONLY, no runtime grant.** Reject reuse of Close,
self-close, collaborator delegation, public request delivery, in-memory-only
state or a reversible/rearming toggle. This authored assessment is not independent
approval of our own request. Independent security disposition and host/CEO exact
intent must still be recorded before any implementation authorization.

Through the President/COO alignment path, request acceptance/revision of: A vs B;
withdrawal disabling further requests even from 0; historical assertion after
reopen; private same-channel staff inspection without notifications; immutable
incarnation assumption; lifecycle-bound minimal audit/erasure; ACL residual race;
and whether full compatibility can fit a one-day slice. Bind the review to exact
commit, spec SHA-256 and task-document revision. A routing acknowledgement is
not staff-host intent; no unanswered question defaults to feature activation.
Keep status Proposed until both records; any material edit needs fresh alignment.

**No-build decision:** B is not justified as a safe complete <=1-day runtime
slice today. Enforced incarnation, fresh actor-channel proof, compatibility and
bounded personal-metadata lifetime are substantive missing seams, not a small
button wrapper. Retain A; do not split off a partial live feature or silently
remove those controls.

**One possible future build slice, <=1 engineering day if separately authorized:**
a framework-free resolution-intent planner and synthetic transition/zero-effect
contract tests for the three-state model, with explicit typed no-effect output.
No command registration, runtime call sites, store/migration, persistent member
data, Discord transport or activation. That narrow slice can validate the state
contract, not deliver the member feature or resolve the retention gap. Its kill
rule is no accepted host demand or no bounded implementation path: retain A,
without creating further cards from this proposal. No build card is created now.
Any later live proposal needs a newly bounded plan and exact intent/security
records; exact-head CI, CodeRabbit and Devin Review, relevant security gate
and existing authorized staging/rollback/deploy contracts still apply.

Review metric by 2026-11-02: all 28 matrix rows accepted/revised, exact host/CEO
intent and independent boundary disposition present, and the one-day feasibility
accepted or B deferred. Benefit remains unmeasured. Default at that date is A;
no new build or runtime follows expiry. No follow-on build cards created here.

ACT: publish this proposed contract and bounded offline checks. WHY: let members
signal intent without laundering it into staff authority. UNDO: reviewed docs
revert; no live data changed. A future rollback disables admission while preserving
state, privacy and existing close fences, never runs a destructive down migration.
