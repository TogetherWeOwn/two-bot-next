# Staff-set ticket priority labels with honest queue ordering

Status: **proposed; design only**. Written 2026-10-26; review by **2026-11-02**.
No runtime, migration, command, queue, message, permission, production action
or activation is added. This file is the authoritative editable repository spec;
the issue document records its exact commit/hash and review disposition.

## Decision and evidence

Staff triage today follows creation order: recovery sorts by creation time and
the Discord channel list offers no urgency signal, so an urgent case opened
later waits behind routine cases opened earlier. Members cannot tell whether
staff noticed their case is urgent. That is a hypothesis about TWO demand, not
measured guild data.

Adjacent products show the pattern but do not prove TWO intent. Community
ticket bots commonly expose a staff `/priority`-style command with Low, Medium
and High tiers (some add Urgent). The official [Ticket Tool
commands](https://docs.tickettool.xyz/general/commands) page (read 2026-10-26)
documents `$escalate {reason}` and open/close commands but no priority tiers;
a Ticket Tool `/priority` command is **unverified** and is not claimed here.
This establishes a comparable workflow, **not** tier semantics, queue
behavior, visibility rules or TWO intent.

| Option | Value and boundary | Recommendation |
| --- | --- | --- |
| A. No build; keep creation-order triage | No new retained state or effects; urgent cases keep waiting behind older routine ones; recovery order stays the only ordering | **Use now**, pending intent and privacy decisions |
| B. Staff-set priority labels with honest queue ordering | Staff flag urgency with a visible-but-quiet label; the staff queue orders by label, then creation; adds a label write/read, an ordering rule and an audit boundary | **Smallest useful candidate**, only after the gates below; otherwise A |
| C. SLA timers and auto-escalation | Promises response deadlines and automatic re-flagging; needs clocks, timers, notification policy and staffing commitments this spec does not establish | **Do not build**; separate demand and security review required |

A label is an urgency flag, not a claim, a close vote, a transcript note, a
search index or authorization. Ticket lists, claim/release, close
confirmation, subjects, collaborators, transcripts and [configuration
transitions](dashboard-ticket-config-spec.md) retain their separate scope. B
grants nobody claim or close authority, changes no permission, and adds no
timer, deadline, escalation or reminder. B never renames a channel, edits the
reservation topic, or posts member-visible pings.

## Source-backed baseline

Inspected source: `a6fa0fb7d40e2ee4eaa6bfce9834d639a25c166c`. These are source
facts, not deployed-feature or permission measurements.

| Seam | Evidence and constraint |
| --- | --- |
| Metadata | [Domain](../crates/core/src/tickets.rs#L73-L84): id, guild, channel, opener, claimant, status and lifecycle times only. No priority, label, rank or urgency field |
| Recovery order | [Runtime](../crates/bot/src/ticket_runtime.rs#L208-L224) sorts recovery by `(created_at, id)` on the decoded key. [Store](../crates/cutover/src/tickets.rs#L123-L127) lists non-closed rows `ORDER BY created_at::timestamptz, id`. Creation order is the only ordering |
| No queue surface | No `/tickets` list/queue command or staff queue UI exists; the configuration proposal explicitly excludes the staff queue from configuration scope ([config](dashboard-ticket-config-spec.md#L12-L16)). Triage today is the channel list plus recovery order |
| Staff today | [Authorization](../crates/core/src/tickets.rs#L106-L125) accepts current configured staff role OR ManageChannels OR Administrator for Claim/Close; opener/claimant alone is not that authority. [Activation](../crates/bot/src/command_runtime.rs#L498-L500) refuses the ticket runtime without the Tickets capability |
| Lifecycle | [Domain](../crates/core/src/tickets.rs#L8-L46): Open/Claim/Close; Creating/Open/Closing/CleanupPending/Closed. Claim is first-winner; an interrupted unsaved close may reopen ([runtime](tickets.md#L25-L29)) |
| Durable identity | [Schema](../crates/cutover/migrations/0210_tickets.sql#L3-L12): primary ticket ID, unique channel ID, guild/opener, claimant and state. [Runtime](../crates/bot/src/ticket_runtime.rs#L1084-L1088) generates a random 128-bit reservation ID |
| Close/recovery | [Close](../crates/bot/src/ticket_runtime.rs#L1212-L1262) freezes opener writes and commits before cleanup. [Recovery](../crates/bot/src/ticket_runtime.rs#L1681-L1767) preserves uncertain creation and fenced close recovery |
| Retention/erasure | [Store](../crates/cutover/src/tickets.rs#L338-L372) purges transcript bodies; [erasure inventory](../crates/cutover/src/member_erasure_plan.json#L219-L228) covers opener/claimant on ticket rows. Audit-cache rows roll off at 45 days ([retention](../crates/cutover/src/retention.rs#L94-L94)). No label column or label audit exists |
| Observability | [Lifecycle](ticket-lifecycle-observability.md#L18-L22) uses a closed counter vocabulary with no `other` row; no member text, channel names or ticket IDs become labels |

Do not replace/append to the topic marker. Preserve ticket ID, guild/channel
binding, transcript identity and retention, claim winner, close fencing and
recovery state. A label change cannot create/reopen a ticket, transfer a
claim, repair ACLs, move a channel, close a case or alter transcript capture.

## Conditional B: staff-only authority and lifecycle

Everything below is a **future acceptance contract**, not an available command
or API. No build/test card before accepted exact intent and CISO disposition.

- **Set/clear:** only a currently authorized staff member (current configured
  staff role OR ManageChannels OR Administrator), with current guild
  membership and proven effective ViewChannel + ReadMessageHistory on the
  bound ticket channel. The opener cannot flag their own ticket; claimant
  identity, past staff status, a service MAC, a supplied actor ID or knowing
  a ticket ID grants nothing. B never broadens close authority: flagging is
  not a close vote and grants no close right.
  - Require the configured guild, feature/application/activation fences and a
    trusted authenticated actor; no arbitrary-guild endpoint. Reauthorize on
    every set, clear, retry and commit admission, not just opening the UI. Deny
    when membership/roles/access facts are missing, revoked or stale. Never
    widen an ACL to satisfy a request.
  - Prove canonical bound channel ID, guild, GuildText, unchanged creation
    category, exact topic and the four expected creation overwrites (everyone
    deny View; opener/staff View/Send/History; bot plus ManageChannels). Also
    resolve effective access for this requester using current role/member
    facts. Overwrite shape alone does not establish current member authority.
  - Only **Open**, bound and proven, with no pending close/recovery/uncertain
    work, permits set/clear, including queue reads of the label. Refuse for
    Creating, Closing, CleanupPending, Closed, missing channel, ambiguous
    creation or expired label. Serialize with real claim/close transitions
    and re-read under the ticket lock. A stale claimant shown in a client is
    not authority.
  - For a missing/foreign/hidden/deleted case, nonparticipant or unprovable
    access, return the same `NOT_AVAILABLE` shape: no existence flag,
    ticket/channel ID, label, claimant, state, version, counts or guessed
    link. No preliminary search, autocomplete, pagination total or error
    detail may reveal a case. Internal diagnostics use bounded reason tokens
    only. Future tests must check response/status/shape parity and absence of
    side effects.
  - After positive authority and binding proof, show participant-only
    state/conflict guidance. Reads are private, non-cacheable, with no shared
    analytics, embeds, notifications, channel history, transcript inclusion
    or public response. No new dashboard/service reader or database grant is
    implied.

A permission change can race after the final read. No DB lock can make Discord
permission revocation and a private response atomic. Recheck just before
release; if the residual in-flight disclosure window is unacceptable to CISO,
choose A.

## Scale, ordering rule and queue honesty

B uses exactly three staff-set levels plus the unflagged default. No fourth
tier: Urgent appears in adjacent bots but a fourth level adds queue and UX
branches without established TWO demand. Reject numeric scores, free-text
labels and emoji-only flags: scores invite silent re-weighting, free text
invites confusion with subjects, and color/emoji alone is inaccessible.

| Stored value | Meaning | Queue rank |
| --- | --- | --- |
| `high` | Handle before routine cases | 0 |
| `medium` | Above routine, below urgent | 1 |
| (unflagged) | Routine; the default, not a demotion | 2 |
| `low` | Explicitly deferred behind routine cases | 3 |

The queue orders open tickets by `(rank, created_at, id)`, ascending, with
the decoded millisecond/instant key the cursor already uses. Ties on rank
fall back to creation order, then ticket ID, so the order is total and
deterministic. Unflagged sorts between Medium and Low on purpose: explicit
Low defers a case behind untriaged routine work, while High and Medium float
above it. The rejected alternative (Low ahead of unflagged) would let a "low"
flag promote a case, which contradicts the label. Closed tickets never appear
in the queue, whatever their stored label.

Honesty properties, all testable without member data:

- Every Open ticket appears exactly once per queue read: no silent demotion,
  no hidden skip, no duplicate. The rank, level text and creation tiebreak
  are shown with each row, so staff can verify the order by inspection.
- The ordering rule is fixed in the spec, not configurable per guild, panel
  or viewer. No per-staffer sort override exists in B; a future override
  would need its own visibility review.
- The queue shows label plus claimant side by side. Priority never implies
  assignment and assignment never implies priority; an unclaimed High case
  reads as "urgent and nobody owns it", not as "being handled".
- Queue reads never mutate: no read receipts, no view counters, no
  last-viewed timestamps, no reordering side effects. A queue read cannot
  flag, claim, close or reopen anything.
- Member-visible surfaces never show queue position, other tickets' labels,
  setter identity or set time. The opener sees only their own ticket's level
  text, per the surfacing section below.

## Claim interplay and reopen inheritance

Priority and claim are orthogonal durable facts that share only serialization:

- Set/clear never claims, releases or transfers a claim. Claiming never sets,
  changes or clears a label. Releasing a claim never clears a label. The
  claim-release witness design ([claim release](ticket-claim-release-spec.md))
  is unaffected: label writes join the same row lock but touch no witness
  column.
- Claim-then-flag and flag-then-claim commute: both orders end with the same
  claimant and the same label. A flag racing a claim has one serialized
  winner per field; neither write is lost and neither corrupts the other.
- Close admission does not require, clear or change the label. Reopen (the
  interrupted unsaved close that restores opener access) inherits the stored
  label unchanged: the urgency flag survives the round trip because the row
  survives. Closed tickets stay out of the queue regardless.
- Only an explicit staff clear removes a label before row deletion. Close,
  reopen, claim, release, recovery, purge and config change never clear it.
  Row deletion and member erasure remove the label with the row.

## Visible-but-quiet surfacing

The opener sees their own ticket's flag without being pinged about it:

- One bounded level text (`High priority`, `Medium priority`, `Low priority`)
  is visible to channel participants; unflagged shows no marker rather than a
  "normal" badge. No ping, mention, DM, new message, embed, reaction or
  notification accompanies set, clear or reflag. Allowed mentions stay closed
  on every Discord response touching this feature.
- Never surface through the channel topic, channel name, or any rename: the
  topic is creation proof and names are not confidential. The exact widget (a
  future slice edits one bot-authored status line in place) is undecided, but
  the acceptance bound is fixed: in-place edit only, no new messages, no
  history growth, no transcript content change.
- Setter identity, set/clear time, queue position, other tickets' labels and
  the queue itself are staff-only. The member-visible marker carries level
  text only. Staff sees label plus setter in the queue and in private
  confirmations.
- The marker updates on the same commit as the label: a confirmed set with a
  failed marker edit is flagged-but-unmarked until reconciled, never silently
  unflagged; a failed set never shows a marker. Reconcile by fresh read, never
  by blind re-edit.
- Level text must work without color or hover: text-first rendering with an
  optional redundant icon, never color-only or emoji-only meaning.

| Outcome | Private copy / next_action |
| --- | --- |
| `FLAGGED` | "Priority set to High/Medium/Low. No notification was sent." / `none` |
| `CLEARED` | "Priority cleared. The ticket keeps its place by creation order." / `none` |
| `UNCHANGED` | Same level re-set: "Already flagged High/Medium/Low. Nothing changed." / `none` |
| `NOT_AVAILABLE` | "This case or action is not available here." / `stop` |
| `STATE_UNAVAILABLE` | Proven participant: "Not saved. The case is busy or no longer open." / `reload_when_open` |
| `VERSION_CONFLICT` | "Not saved. Another update won; reload before confirming." / `reread_then_reconfirm` |
| `RATE_LIMITED` | "Not saved. Try after the displayed time; nothing is queued." / `retry_after` |
| `SAVE_UNCONFIRMED` | "Save not confirmed. Do not submit a different update yet." / `reconcile_same_operation` |
| `PREVIEW_ONLY` | "Preview only; nothing saved or announced." / `confirm_or_edit` |

Network/read failure is not UNCHANGED. A confirmed label commit followed by
failed private reply is saved, not unsaved; a commit timeout is unknown, not
success. Never retry with a fresh operation to resolve uncertainty. Display
storage truth separately from reply delivery.

## Serialization, idempotency, limits and retention

B needs a durable per-ticket label plus a monotonically increasing label
version (including clear), shared serialization with claim/close transitions
and an atomic label/version/audit commit. No DB lock across Discord I/O.
Compare expected_version at commit after fresh authority/proof; two concurrent
writers have one winner. Same canonical value is a no-op with no
version/audit change. Proposed write limits: >=30 seconds between admitted
changes per ticket, <=5 per actor per rolling 10 minutes, <=20 per guild per
rolling hour, and one in-flight change per ticket. Quota admission is atomic;
no background debounce/save queue. Preview and queue reads never consume
write quota. A cancelled/unknown admitted save keeps its operation fence;
replay cannot consume a second quota or write again. These are proposal
bounds, not current constants or measured capacity.

Each intent carries a random operation ID and exact expected_version. Retain
only current label plus last operation ID/result/version, not a history of
past levels, replay responses or request payloads. Same key retry returns the
recorded **receipt**, never reapplies; fresh authorized read is required.
Reuse for changed content/version is a client error and must never write: the
key cannot initiate a second effect. A replay receipt does not attest that a
new submitted level was saved. Older operations fail version check even with
a new key. After clear keep only the minimal per-ticket version/fence;
underlying ticket absence always refuses recreation. Exact storage and
replay-fence expiry/restore design need CISO acceptance before
implementation; do not invent an indefinite retention exemption.

A label lives and dies with its ticket row: no independent ceiling, no expiry
timer, no resurrection after row deletion. Close never resurrects a cleared
label and clear never resurrects after close. The label holds no member PII
beyond the ticket link itself, so opener erasure needs no new selector once
the column dies with the row; setter identity lives only in the audit event
below and erases with that actor under the existing audit predicate. If audit
retention cannot cover the new event without an unresolved boundary, choose A.

Proposed label audit minimum, one event per admitted set/clear: guild/ticket
ID, operation ID, actor, time, version, before/after level (or clear) and
fixed cause; no names, topic, reason text, channel ID, transcript reference
or remote body. Entry IDs stay within the existing audit ID budget and carry
no level text in metric labels, SQL errors, traces, HTTP/access logs or
private-to-public summaries. Only minimal non-text integrity fences may
remain for the extant ticket lifetime, subject to explicit disposition of
their linkability. Discord/client copies of the marker cannot be recalled by
local erasure; no label messages are posted to the channel, so transcripts
gain no new label-bearing content.

## Smallest future build slice (B0, not approved)

If intent and privacy reviews accept B, the smallest <=1-day candidate is B0:
an additive nullable label column plus version, a staff-only set/clear path
admitted through the shared router, the `(rank, created_at, id)` ordering in
the one staff queue read, and the audit event above. B0 defers the
member-visible marker, any dashboard reader, per-guild configuration and SLA
timers to later slices with their own reviews. B0 is a planning bound, not a
commitment: if the coupled audit/erasure/ordering seams do not fit one
reviewed day, defer B rather than ship a partial flag without audit or
erasure.

## Agent contract and runnable offline decision mock

No new endpoint or CLI flag exists. Proposed bounded request: schema, string
IDs (guild/ticket/actor/operation), action read/set/clear, expected_version
string, level only for set, and strict Boolean dry_run. Reject unknown
fields/actions, bad IDs, non-Boolean/missing dry_run and >2048-byte requests
before dispatch; never interpret a wrong flag as permission to mutate.
Replies <=2048 UTF-8 bytes have
`schema,operation_id,outcome,error_code,next_action,saved`; IDs/version and
the level appear only after successful participant proof. No raw request
serialization in observability. Agent handoffs carry exact spec commit/hash;
machines cannot manufacture human intent.

A dry-run uses **local synthetic facts only**. It has no network or
credentials. It never reserves nonces, replay fences or quotas; writes
SQL/audit records; schedules work; or calls Discord. It predicts only the
supplied snapshot, never live authority or save readiness.
The following miniature mock runs offline and checks policy examples; it is
not production code, protected behavior tests, runtime acceptance or a
security proof. Its dictionaries are policy fragments, not complete replies;
actual replies must use the seven-field envelope above, including on refusal.

```pycon
>>> import json, pathlib, re
>>> LEVELS = {"high": 0, "medium": 1, "low": 3}
>>> UNFLAGGED_RANK = 2
>>> def rank(level):
...     return UNFLAGGED_RANK if level is None else LEVELS[level]
>>> def queue_key(ticket):
...     level, created, tid = ticket
...     return (rank(level), created, tid)
>>> def decide(*, participant=True, proof=True, state="open", uncertain=False, version="3", expected="3", action="set", level="high", dry_run=True):
...     if not participant or not proof:
...         return {"outcome":"refused", "error_code":"NOT_AVAILABLE", "next_action":"stop"}
...     if action not in {"read", "set", "clear"} or type(dry_run) is not bool:
...         return {"outcome":"refused", "error_code":"INVALID_REQUEST", "next_action":"fix_request"}
...     if state != "open" or uncertain:
...         return {"outcome":"refused", "error_code":"STATE_UNAVAILABLE", "next_action":"reload_when_open"}
...     if action != "read" and expected != version:
...         return {"outcome":"refused", "error_code":"VERSION_CONFLICT", "next_action":"reread_then_reconfirm"}
...     if action == "set" and level not in LEVELS:
...         return {"outcome":"refused", "error_code":"INVALID_LEVEL", "next_action":"pick_listed_level"}
...     return {"outcome":"preview", "error_code":"PREVIEW_ONLY", "next_action":"confirm_or_edit", "saved":False}
>>> rows = [("low", 3, "c"), (None, 1, "a"), ("high", 5, "e"), ("medium", 2, "b"), ("high", 4, "d")]
>>> assert [t[2] for t in sorted(rows, key=queue_key)] == ["d", "e", "b", "a", "c"]
>>> assert rank(None) == 2 and rank("medium") < rank(None) < rank("low")
>>> assert decide()["error_code"] == "PREVIEW_ONLY"
>>> assert decide(participant=False) == decide(proof=False) == decide(participant=False, state="closed", level="bogus")
>>> assert all(decide(state=s)["error_code"] == "STATE_UNAVAILABLE" for s in ["creating", "closing", "cleanup_pending", "closed"])
>>> assert decide(uncertain=True)["error_code"] == "STATE_UNAVAILABLE"
>>> assert decide(expected="2")["error_code"] == "VERSION_CONFLICT"
>>> assert decide(action="escalate")["error_code"] == decide(dry_run="false")["error_code"] == "INVALID_REQUEST"
>>> assert all(decide(level=lv)["error_code"] == "INVALID_LEVEL" for lv in ["urgent", "critical", "p0", "", "HIGH", " high"])
>>> assert all(decide(level=lv)["error_code"] == "PREVIEW_ONLY" for lv in ["high", "medium", "low"])
>>> assert "urgent" not in LEVELS and set(LEVELS) == {"high", "medium", "low"}
>>> text = pathlib.Path("docs/ticket-priority-spec.md").read_text()
>>> cases = re.findall(r"^\| (TP-F\d{2}) ", text, re.M)
>>> assert cases == [f"TP-F{i:02}" for i in range(1, 17)]
>>> print(json.dumps({"schema":"ticket.priority.mock.v1", "matrix_rows":len(cases), "outcome":"offline_examples_pass", "saved":False}, sort_keys=True))
{"matrix_rows": 16, "outcome": "offline_examples_pass", "saved": false, "schema": "ticket.priority.mock.v1"}

```

The mock always refuses a fourth tier and never writes, even for
`dry_run=False`; it has no live adapter. It does not model durable version
checks, quotas, retention, JSON parsing or actual ACL resolution. Those need
independent tests. Run from the repo root:

```sh
python3 -m doctest -v docs/ticket-priority-spec.md
python3 scripts/check_docs_index.py --json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
```

## Must-pass future acceptance matrix

All fixtures below are **planned, not executed runtime tests**. Use synthetic
identities, unique channels per case, an injected clock, independent disposable
test-DB connections and the shared recording Discord double. For each row collect
bounded result JSON plus commit/audit/message counts; never capture private
text in evidence. Future UI staging evidence needs separately authorized access,
not live private-case collection. Timing limits are acceptance targets, not SLAs.

| Case | Given / when / then oracle |
| --- | --- |
| TP-F01 | Given proven Open case, when current staff sets a listed level, then one saved label/version/audit, zero pings, zero new messages, zero topic/name edits; second staffer reads the same level. |
| TP-F02 | Given valid actor/proof, when current staff clears or reflags, then version check applies; opener/claimant-alone/past-role or forged actor never grants set/clear. |
| TP-F03 | Given missing, foreign, hidden, deleted or nonparticipant case, when read/set/clear/queue occurs, then identical NOT_AVAILABLE body/status with no IDs/state/label and zero writes. |
| TP-F04 | Given wrong channel type/category, topic, overwrite principal/bits or failed permission fetch, when requested, then no disclosure/effect; no ACL repair. |
| TP-F05 | Given Creating/Closing/CleanupPending/Closed or uncertain create, when any label operation runs, then refusal; no queued save or reopened case; closed rows never queue. |
| TP-F06 | Given role/config change or actor access revocation between preview and submit, when commit admitted, then fresh proof refuses; stale claimant UI gives no bypass. |
| TP-F07 | Given unflagged case, when staff queues or member views, then no marker for the member and routine rank for staff; failed read is unavailable, while clear of unflagged is an explicit no-op. |
| TP-F08 | Given each listed level plus rejected `urgent/critical/p0/empty/case/whitespace` input, when set, then exact scale above; no truncation, coercion, fourth tier or silent default. |
| TP-F09 | Given two concurrent writers or claim/close transition race, when committing same version, then one valid winner under shared serialization; no lost update, stale effect or claim/close corruption; claim and label commute. |
| TP-F10 | Given replay/same key, changed level, old version or restart, when resubmitted, then receipt-only or conflict, never repeated DML/audit; unknown commit stays SAVE_UNCONFIRMED until reconciled. |
| TP-F11 | Given limit boundary/one-over and busy runtime, when writes burst, then bounded admission/no queue; queue reads never consume write quota; proposed private acknowledgement <=3 s and completion/refusal <=10 s under fixture load, otherwise unknown/refusal, never false success. |
| TP-F12 | Given label commit then reply/marker failure, when response lost, then storage remains saved and same operation reconciles; zero pings, zero new messages, zero transcript content change. |
| TP-F13 | Given clear, close/reopen, setter/opener erasure or restore, when reading/replaying, then reopen inherits the stored label, closed rows stay out of the queue, and erasure removes setter audit without resurrecting a cleared label. |
| TP-F14 | Given mixed flagged/unflagged/claimed/unclaimed open rows, when staff queues, then exact `(rank, created_at, id)` order with High/Medium above routine and Low below; every open row exactly once with visible rank and tiebreak. |
| TP-F15 | Given flagged case viewed by opener vs staff, when surfaced, then member sees level text only with no position/setter/queue, staff sees label plus setter; no member-visible queue exists. |
| TP-F16 | Given local dry-run, malformed JSON/unknown fields/flags or oversized request, when invoked, then deterministic bounded error/next_action and zero network/SQL/nonce/quota/audit effects; telemetry contains no level history/names/raw reason. |

Existing reusable mocks/tests: [shared recording transport](../crates/discord/tests/common/mod.rs#L381-L448),
[receipt acceptance](../crates/discord/tests/ticket_creation_receipt_acceptance.rs#L22-L150),
[store races/retention](../crates/cutover/tests/tickets_store.rs#L103-L625),
[runtime lifecycle](../crates/bot/src/ticket_acceptance_tests.rs#L359-L619),
[destination moves](../crates/bot/src/ticket_destination_move_tests.rs#L180-L283)
and [claim/close truth](../crates/core/tests/ticket_claim_close_truth_controls.rs#L23-L100).
Their existence is not evidence of B passing. No behavior tests are added or
modified here. Future Test Author owns focused immutable test files, branch/SHA
and JSON path/SHA-256 manifest; demonstrate behavior-specific RED against the named
baseline plus unchanged positive/negative controls. Compile/cache/environment
failure is not RED; builders must not edit protected files to obtain GREEN.

## Exact intent and privacy review gates

Before **any build or behavior-test card**, bind CEO/host intent alignment and
independent CISO accept/change/reject disposition to the issue-spec revision,
repo commit and this file's SHA-256. A merged design or pending review is not
acceptance. Use native review/approval stages; no routine owner click-gate.
Any spec revision invalidates earlier acceptance; no timeout approval.

CEO/host decision: keep A, or substantiate B's triage use case and accept the
fixed three-level scale, staff-only authority, unflagged-between-Medium-and-Low
ordering, visible-but-quiet member marker and the deferred B1 surfacing split.
C requires a separate exact scope/value decision; do not silently turn a label
acceptance into SLA or auto-escalation permission.

CISO decision: current member/visibility proof, nonparticipant oracle,
setter-identity handling and residual disclosure race, retained/linkable
version/replay facts, setter erasure, audit retention and backup/restore
behavior. New access/data boundaries require this disposition; existing
grants, activation and secret fences remain untouched. Missing proof or
rejected privacy/usefulness at review means A/no-build, not weakened
permissions.

Metric: all 16 oracles and three option decisions have an explicit disposition
by 2026-11-02; no live experiment, benefit or performance result is claimed.
Unverified: actual guild objects/permissions, deployed ticket activation,
runtime behavior of B (absent), label storage/erasure and usable queue UI. No
production tests, live flag, migration, private data collection, new service
or spend occurred.

ACT: publish this indexed proposal and offline decision examples. WHY: order
staff triage by visible urgency instead of creation order without pinging
members, broadening close authority or promising deadlines. UNDO: revert docs;
there is no operational state to roll back.
