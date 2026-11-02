# Private ticket subjects and channel-name boundaries

Status: **Deferred to 2026-12** (owner decision 2026-11-01). Revisit then, or
earlier if a host names a need and an M2 slot opens. Bundle with [ticket
priority](ticket-priority-spec.md). Original proposal: written 2026-10-25. No
runtime, migration, command, channel mutation, permission, production action or
activation is added. This file is the authoritative editable repository spec;
the issue document records its exact commit/hash and review disposition.

## Decision and evidence

Members/staff may want to distinguish cases without exposing their subject.
That is a hypothesis, not measured TWO demand. [Ticket Tool commands](https://docs.tickettool.xyz/general/commands)
(read 2026-10-25) documents `$rename [Name]` and `$new {Reason}`; the reason may
appear through `{reason}` in a ticket message. This establishes a comparable
workflow, **not** confidential channel names, safe reason handling or TWO intent.

| Option | Value and boundary | Recommendation |
| --- | --- | --- |
| A. No build; keep generated names | No new retained private text or effects; current `ticket-<safe username>` is not a confidential name and can identify the opener | **Use now**, pending intent and privacy decisions |
| B. Private stored subject only | Helps an authorized requester identify a case, without renaming or publishing its subject; adds a private read/write and erasure boundary | **Smallest useful candidate**, only after the gates below; otherwise A |
| C. Constrained channel rename | Only a neutral `case-<reservation-id>` name, not free text or subject-derived text; no demonstrated incremental case-discovery value over B | **Do not build**; conditional safety contract below is not approval |

Never derive a channel name from a subject, reason, opener/member ID, display
name, category description or sensitive label. Existing generated names are not
an endorsement for new identity-bearing names. Even a private channel's name may
reach clients, notifications, integrations, moderation logs and administrators;
permissions do not make it confidential. No automatic rename on create, subject
edit, claim, close, recovery or config change. Do not backfill existing cases.

A subject is an optional short reference, not a complaint, transcript, reason,
classification, authorization credential or search index. Ticket lists/queue,
collaborators, claimant self-release, transcripts, close confirmation and
[configuration transitions](dashboard-ticket-config-spec.md) retain their separate
scope. B adds no subject column to a list/queue/export and grants nobody access.

## Source-backed baseline

Inspected source: `c3af7333df80a766f5d4c229506a3bce53680828`. These are source
facts, not deployed-feature or permission measurements.

| Seam | Evidence and constraint |
| --- | --- |
| Lifecycle/action | [Domain](../crates/core/src/tickets.rs#L8-L84): Open/Claim/Close; Creating/Open/Closing/CleanupPending/Closed. No subject, rename action, subject revision or rename journal |
| Staff today | [Authorization](../crates/core/src/tickets.rs#L106-L125) accepts current configured staff role OR ManageChannels OR Administrator for Claim/Close; opener/claimant alone is not that authority. [Runtime](../crates/bot/src/ticket_runtime.rs#L951-L981) takes member facts from the interaction and live config |
| Durable identity | [Schema](../crates/cutover/migrations/0210_tickets.sql#L2-L35): primary ticket ID, unique channel ID, guild/opener, claimant and state; no creation-role/category snapshot. [Runtime](../crates/bot/src/ticket_runtime.rs#L1084-L1088) generates a random 128-bit reservation ID, not a member ID. [Store](../crates/cutover/src/tickets.rs#L108-L178) resolves guild/channel and locks transitions |
| Creation proof | [Executor](../crates/discord/src/executor/tickets.rs#L46-L146) requires same guild/category, GuildText, canonical channel ID, exactly four explicit overwrites and exact topic `two-ticket:<reservation-id>`. Administrators are not excluded. Name is not a proof input |
| Creation name/topic | [Create](../crates/discord/src/executor/tickets.rs#L150-L172) sends generated name and exact topic, single attempt; uncertain receipt never becomes safe failure. [Name helper](../crates/core/src/tickets.rs#L496-L509) uses a bounded sanitized username, not opaque case numbering |
| Config drift | [Runtime proof](../crates/bot/src/ticket_runtime.rs#L1286-L1308) compares against current category/staff config, not saved creation values. Old staff may retain channel access while new staff lack it; current role config alone cannot prove original audience |
| Close/recovery | [Close](../crates/bot/src/ticket_runtime.rs#L1212-L1262) freezes opener writes, captures and commits before cleanup; [opener overwrite](../crates/discord/src/executor/tickets.rs#L325-L355) leaves staff/bot permissions unchanged. [Store capture](../crates/cutover/src/tickets.rs#L180-L199) is transactional. [Recovery](../crates/bot/src/ticket_runtime.rs#L1681-L1767) preserves uncertain creation and fenced close recovery. No rename-specific uncertain state exists |
| Retention/erasure | [Store](../crates/cutover/src/tickets.rs#L338-L372) purges transcript rows and erases opener/claimant-owned tickets; [erasure inventory](../crates/cutover/src/member_erasure_plan.json#L219-L226) does not cover a new subject author. Ticket metadata has no age purge here. [Privacy](privacy.md) excludes Discord-side deletion |

Do not replace/append to the topic marker. Preserve ticket ID, guild/channel
binding, transcript identity and retention, claim winner, close token and recovery
state. A subject update cannot create/reopen a ticket, transfer a claim, repair
ACLs, move a channel, publish a message or alter transcript capture.

## Conditional B: authority, visibility and lifecycle

Everything below is a **future acceptance contract**, not an available command or
API. No build/test card before accepted exact intent and CISO disposition.

- **Propose/change/clear/view:** only the ticket's opener or a currently authorized
  staff member (current configured staff role OR ManageChannels OR Administrator),
  with current guild membership and proven effective ViewChannel + ReadMessageHistory
  on the bound ticket channel. Claimant identity, past staff status, a service MAC,
  supplied actor ID, subject text or knowing a ticket ID grants nothing. A future
  collaborator does not gain subject access from this spec.
- Require the configured guild, feature/application/activation fences and a trusted
  authenticated actor; no arbitrary-guild endpoint. Reauthorize on every read,
  modal submission, retry and commit admission, not just opening the UI. Deny when
  membership/roles/access facts are missing, revoked or stale. Never widen an ACL
  to satisfy a request. Administrator bypass of Discord ACLs is not a privacy
  promise; application eligibility still applies.
- Prove canonical bound channel ID, guild, GuildText, unchanged creation category,
  exact topic and the four expected creation overwrites (everyone deny View;
  opener/staff View/Send/History; bot plus ManageChannels). Also resolve effective
  access for this requester using current role/member facts. Overwrite shape alone
  does not establish current member authority or secrecy from administrators.
- **Unresolved prerequisite:** a trusted immutable creation category/staff-role
  binding and its consistency with current config are required. Today's row lacks
  that evidence; do not infer it from current config, a channel name or a matching
  topic. Refuse B for a legacy/unknown binding, moved category or changed staff config.
  No retrospective snapshot/backfill from an unproven channel. A future accepted
  slice must decide how new tickets persist proof and how config transitions
  fence operations; the configuration proposal does not already implement this.
- Only **Open**, bound and proven, with no pending close/recovery/uncertain work,
  permits B, including reads. Refuse B for Creating, Closing, CleanupPending,
  Closed, missing channel, ambiguous creation or expired subject. There is no guessed
  `claiming` state: serialize with real claim/close transitions and re-read under
  the ticket lock. A stale claimant shown in a client is not authority.
- For a missing/foreign/hidden/deleted case, nonparticipant or unprovable access,
  return the same `NOT_AVAILABLE` shape: no existence flag, ticket/channel ID,
  subject, claimant, state, version, counts or guessed link. No preliminary search,
  autocomplete, pagination total or error detail may reveal a case. Internal
  diagnostics use bounded reason tokens only. Do not claim constant-time behavior;
  future tests must check response/status/shape parity and absence of side effects.
- After positive authority and binding proof, show participant-only state/conflict
  guidance. Reads are private, non-cacheable, with no shared analytics, embeds,
  notifications, channel history, transcript inclusion or public response. No new
  dashboard/service reader or database grant is implied.

A permission change can race after the final read. No DB lock can make Discord
permission revocation and a private response atomic. Recheck just before release;
if the residual in-flight disclosure window is unacceptable to CISO, choose A.

## Text bounds, safe display and human UX

B uses an explicit private **Set subject** flow; creation has no subject prompt.
The opener and eligible staff have equal edit rights, with CAS rather than
last-writer-wins. Read and edit are deliberate separate actions, not prefilled
public controls; clearing requires a separate confirmed **Clear subject** action.

Normalize NFC, trim ASCII space at both ends, then require **1–120 Unicode
scalars and <=480 UTF-8 bytes**. Reject, never silently truncate: empty input,
invalid Unicode, any Unicode general category C (including controls, bidi/zero-width
formatting and surrogates), line/paragraph separators, `@`, `<`, `>`, or backtick.
Thus mention syntax, Markdown mention/link delimiters and multiline input cannot
be stored; legitimate RTL letters and combining text remain valid. Escape all
remaining Markdown/HTML and render as inert plain text with directional isolation
in UI, not added stored control characters. Closed allowed_mentions on every
Discord response: empty parse/users/roles and replied_user=false. No embeds.

Warn before confirmation: “Use a short reference, not confidential case details.
No passwords, tokens, payment/health details, real-person allegations or other
sensitive personal information. Staff with ticket access can read this. Channel
names will not change.” Require explicit acknowledgement; no automatic claim that
validation detects sensitive information. Do not echo invalid input in errors,
logs or audit. Keep an unsent draft only in the requesting client's private view,
<=5 minutes; clear on exit, auth loss or success, never local durable storage.

| Outcome | Private copy / next_action |
| --- | --- |
| `SAVED` | “Subject saved privately. The channel name is unchanged.” / `none` |
| `CLEARED` | “Subject cleared. The channel name is unchanged.” / `none` |
| `EMPTY` | Authorized fresh read of a present eligible case with no subject: “No subject saved.” / `set_subject` |
| `NOT_AVAILABLE` | “This case or action is not available here.” / `stop` |
| `INVALID_SUBJECT` | “Not saved. Use a short single-line reference without mentions or sensitive details.” / `edit_private_draft` |
| `STATE_UNAVAILABLE` | Proven participant: “Not saved. The case is busy or no longer open.” / `reload_when_open` |
| `VERSION_CONFLICT` | “Not saved. Another update won; reload before confirming your draft.” / `reread_then_reconfirm` |
| `RATE_LIMITED` | “Not saved. Try after the displayed time; nothing is queued.” / `retry_after` |
| `SAVE_UNCONFIRMED` | “Save not confirmed. Do not submit a different update yet.” / `reconcile_same_operation` |
| `PREVIEW_ONLY` | “Preview only; nothing saved, sent or renamed.” / `confirm_or_edit` |
| `RENAME_UNCONFIRMED` | C only: “The channel name may have changed. Do not rename again.” / `inspect_bound_channel` |

Network/read failure is not EMPTY. A confirmed subject commit followed by failed
private reply is saved, not unsaved; a commit timeout is unknown, not success.
Never retry with a fresh operation to resolve uncertainty. Display storage truth
separately from reply delivery. Keyboard labels and validation errors must work
without color, hover or a channel-name change; no inaccessible disabled-only hint.

## Serialization, idempotency, limits and retention

B needs a durable per-ticket monotonically increasing subject version (including
clear/expiry), shared serialization with claim/close/config transitions and an
atomic subject/version/audit commit. No DB lock across Discord I/O. Compare
expected_version at commit after fresh authority/proof; two concurrent writers
have one winner. Same canonical value is a no-op with no version/audit change.
Proposed write limits: >=30 seconds between admitted changes per ticket, <=5 per
actor per rolling 10 minutes, <=20 per guild per rolling hour, and one in-flight
change per ticket. Quota admission is atomic; no background debounce/save queue.
Preview and read never consume write quota. A cancelled/unknown admitted save
keeps its operation fence; replay cannot consume a second quota or write again.
These are proposal bounds, not current constants or measured capacity.

Each intent carries a random operation ID and exact expected_version. Retain only
current subject plus last operation ID/result/version, not a history of raw
bodies, raw text hashes, replay responses containing text or request payloads.
Same key retry returns the recorded **receipt**, never reapplies or returns an
old subject; fresh authorized read is required. Reuse for changed content/version
is a client error and must never write: the key cannot initiate a second effect.
A replay receipt does not attest that the new submitted body was saved. Older
operations fail CAS even with a new key. After clear/expiry keep only the minimal
per-ticket version/fence; underlying ticket absence always refuses recreation.
Exact storage and replay-fence expiry/restore design need CISO acceptance before
implementation; do not invent an indefinite retention exemption.

B retention proposal: one current value, author ID and update time; no revisions
of text. The value expires at the earlier of close admission, explicit clear,
member erasure, or **90 elapsed days from ticket creation**, never extended by
edits/reopen. An expired value cannot be read while physical purge waits; purge
within the existing hourly privacy-maintenance budget, independently of slow
Discord recovery. Clear text atomically with begin-close; close/reopen never
resurrects it. Tickets older than the ceiling cannot acquire a subject. Retention
is not automatically provided by transcript purge: new subject storage must have
its own ceiling and erasure inventory entry, plus backup/restore non-resurrection
proof. If those cannot be met without an unresolved boundary, choose A.

Member erasure clears subject data when the member is opener, current claimant
or **subject author**, and removes actor-bearing quota/audit state. Do not search
for member names inside text as an erasure oracle. Existing ticket/transcript
selectors remain unchanged. Proposed subject audit minimum: guild/ticket ID,
operation ID, actor, time, version, set/clear/noop/refusal/unknown and fixed cause;
no text, length, name, topic, reason, hash of text or remote body. Audit actor data
expires within 90 days and erases with that actor; quota actor data <=1 hour.
Only minimal non-text integrity fences may remain for the extant ticket lifetime,
subject to explicit disposition of their linkability. No subject in metric labels,
SQL errors, traces, HTTP/access logs, clipboard export or private-to-public summary.
Discord/client copies cannot be recalled by local erasure; no subject messages
are posted to the channel, so transcripts gain no new subject-bearing content.

## Conditional C: constrained rename, not selected

If a later exact accepted scope establishes value, the only candidate is a
staff-confirmed neutral name `case-<32 lowercase hex reservation-id>` (37 ASCII
characters), from the existing random ID. No user-selected suffix, subject slug,
member ID, role/category label or raw Discord reason. A channel name is not unique
identity; use the saved channel ID, never search by name. This change could remove
the username from a name, but it does not prove past notifications were erased.

Require all B authority/channel proofs, current staff authority (opener alone
cannot rename), fresh bot ManageChannels, approved exact intent and a five-minute
private preview binding ticket/channel/category/topic/ACL/config/lifecycle and
subject-independent target. Revalidate at admission, serialize with lifecycle
and other renames, no effect for preview. At most one admitted rename per ticket
per rolling 10 minutes, two per guild per rolling minute, one pending intent per
ticket; explicit confirmation only, no timer or coalesced automatic rename.

Persist expected old name, target and operation state before one PATCH containing
**name only**. Keep subject out of that journal. No topic/overwrite/parent edits;
no claim/close/transcript changes. Same target already present is a no-op after
proof. Competing operations refuse; identical replay returns its receipt without
another PATCH. Unknown admission commit permits no PATCH. Unknown wire outcome,
429/5xx, crash or receipt-store failure becomes `RENAME_UNCONFIRMED`; disable
automatic transport retries and keep the durable pending fence across restarts.
No arbitrary retry or new operation can bypass it.

Authorized read-only reconciliation fetches the exact bound channel, re-proves
identity/ACL/topic, and compares target: matching target confirms **observed current
name**, not causality; different name is drift, not proof the attempt never ran.
403, generic 404, deleted/foreign/moved channel or missing proof stays unavailable;
never create a replacement or repair access. A manual actor can race after any
observation. Do not auto-undo a later human rename. Subject operations refuse
while rename is uncertain; close safety/recovery must not be permanently blocked
by an optional rename. Any eventual C design must fence a pending rename before
close and prove no stale PATCH after close/delete; absent such evidence C remains
no-build. An undo is a new proven/confirmed safe intent, never a blind rollback.

## Agent contract and runnable offline decision mock

No new endpoint or CLI flag exists. Proposed bounded request: schema, string IDs
(guild/ticket/actor/operation), action read/set/clear, expected_version string,
subject only for set, and strict Boolean dry_run. Reject unknown fields/actions,
bad IDs, non-Boolean/missing dry_run and >2048-byte request before dispatch; never
interpret a wrong flag as permission to mutate. Replies <=2048 UTF-8 bytes have
`schema,operation_id,outcome,error_code,next_action,saved,renamed`; IDs/version and
optional escaped subject appear only after successful participant proof. No raw
request serialization in observability. Agent handoffs carry exact spec commit/hash
and protected-test branch/SHA/path hashes; machines cannot manufacture human intent.

A dry-run uses **local synthetic facts only**. It has no network or credentials.
It never reserves nonces, replay fences or quotas; writes SQL/audit records;
schedules work; or calls Discord. It predicts only the supplied snapshot, never
live authority or save readiness.
The following miniature mock runs offline and checks policy examples; it is not
production code, protected behavior tests, runtime acceptance or a security proof.
Its dictionaries are policy fragments, not complete replies; actual replies must
use the seven-field envelope above, including on refusal.

```pycon
>>> import json, pathlib, re, unicodedata
>>> def valid_subject(raw):
...     text = unicodedata.normalize("NFC", raw).strip(" ")
...     return (1 <= len(text) <= 120 and len(text.encode("utf-8", errors="surrogatepass")) <= 480
...             and not any(unicodedata.category(c).startswith("C") or ord(c) in (8232, 8233) or c in "@<>`" for c in text))
>>> def decide(*, participant=True, proof=True, state="open", uncertain=False, version="3", expected="3", action="set", subject="Login help", dry_run=True):
...     if not participant or not proof:
...         return {"outcome":"refused", "error_code":"NOT_AVAILABLE", "next_action":"stop"}
...     if action not in {"read", "set", "clear"} or type(dry_run) is not bool:
...         return {"outcome":"refused", "error_code":"INVALID_REQUEST", "next_action":"fix_request"}
...     if state != "open" or uncertain:
...         return {"outcome":"refused", "error_code":"STATE_UNAVAILABLE", "next_action":"reload_when_open"}
...     if action != "read" and expected != version:
...         return {"outcome":"refused", "error_code":"VERSION_CONFLICT", "next_action":"reread_then_reconfirm"}
...     if action == "set" and not valid_subject(subject):
...         return {"outcome":"refused", "error_code":"INVALID_SUBJECT", "next_action":"edit_private_draft"}
...     return {"outcome":"preview", "error_code":"PREVIEW_ONLY", "next_action":"confirm_or_edit", "saved":False, "renamed":False}
>>> assert decide()["error_code"] == "PREVIEW_ONLY"
>>> assert decide(participant=False) == decide(proof=False) == decide(participant=False, state="closed", subject="")
>>> assert all(decide(state=s)["error_code"] == "STATE_UNAVAILABLE" for s in ["creating", "closing", "cleanup_pending", "closed"])
>>> assert decide(uncertain=True)["error_code"] == "STATE_UNAVAILABLE"
>>> assert decide(expected="2")["error_code"] == "VERSION_CONFLICT"
>>> assert decide(action="rename")["error_code"] == decide(dry_run="false")["error_code"] == "INVALID_REQUEST"
>>> assert all(not valid_subject(s) for s in ["", " " * 5, "x" * 121, "@everyone", "<@123>", "a\nb", "a\tb", "a" + chr(8238) + "b", "a" + chr(8205) + "b", "`code`", "\ud800"])
>>> assert all(valid_subject(s) for s in ["x", "x" * 120, "\U0001f642" * 120, "Café", "مساعدة"])
>>> assert len(unicodedata.normalize("NFC", "Café")) == 4
>>> text = pathlib.Path("docs/ticket-subject-spec.md").read_text()
>>> cases = re.findall(r"^\| (TS-F\d{2}) ", text, re.M)
>>> assert cases == [f"TS-F{i:02}" for i in range(1, 17)]
>>> print(json.dumps({"schema":"ticket.subject.mock.v1", "matrix_rows":len(cases), "outcome":"offline_examples_pass", "saved":False, "renamed":False}, sort_keys=True))
{"matrix_rows": 16, "outcome": "offline_examples_pass", "renamed": false, "saved": false, "schema": "ticket.subject.mock.v1"}

```

The mock always refuses unsupported rename and never writes, even for
`dry_run=False`; it has no live adapter. It does not model durable CAS, quotas,
retention, JSON parsing or actual ACL resolution. Those need independent tests.
Run from the repo root:

```sh
python3 -m doctest -v docs/ticket-subject-spec.md
python3 scripts/check_docs_index.py --json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
```

## Must-pass future acceptance matrix

All fixtures below are **planned, not executed runtime tests**. Use synthetic
identities, unique channels per case, an injected clock, independent disposable
test-DB connections and the shared recording Discord double. For each row collect
bounded result JSON plus commit/audit/PATCH/message counts; never capture private
text in evidence. Future UI staging evidence needs separately authorized access,
not live private-case collection. Timing limits are acceptance targets, not SLAs.

| Case | Given / when / then oracle |
| --- | --- |
| TS-F01 | Given proven Open case, when opener sets valid acknowledged subject, then one saved private value/version/audit, zero channel/message effects; eligible current staff read same value. |
| TS-F02 | Given valid actor/proof, when current staff edits/clears, then CAS applies; claimant/past-role/collaborator alone or forged actor never grants access. |
| TS-F03 | Given missing, foreign, hidden, deleted or nonparticipant case, when read/edit/preview occurs, then identical NOT_AVAILABLE body/status with no IDs/state/text and zero writes. |
| TS-F04 | Given wrong channel type/category, topic, overwrite principal/bits, unresolved creation binding or failed permission fetch, when requested, then no disclosure/effect; no ACL repair. |
| TS-F05 | Given Creating/Closing/CleanupPending/Closed or uncertain create/rename, when any subject operation runs, then refusal; no queued save or reopened case. |
| TS-F06 | Given role/config change or actor access revocation between preview and submit, when commit admitted, then fresh proof refuses; stale claimant UI gives no bypass. |
| TS-F07 | Given empty/no subject, when reading authorized fresh case, then EMPTY; failed read is unavailable, while blank set rejects and explicit clear is distinct. |
| TS-F08 | Given 1/120/121 scalars, 480-byte Unicode, NFC/RTL/controls/mentions/invalid encoding, when set, then exact bounds above; no truncation, echo, mention or unsafe render. Sensitive-text acknowledgement is required, not an automatic detection claim. |
| TS-F09 | Given two concurrent writers or close/claim/config transition race, when committing same version, then one valid winner under shared serialization; no lost update, stale effect or claim/close corruption. |
| TS-F10 | Given replay/same key, changed body, old version or restart, when resubmitted, then receipt-only or conflict, never repeated DML/audit or old text disclosure; unknown commit stays SAVE_UNCONFIRMED until reconciled. |
| TS-F11 | Given limit boundary/one-over and busy runtime, when writes burst, then bounded admission/no queue; reads/previews do not consume write quota; proposed private acknowledgement <=3 s and completion/refusal <=10 s under fixture load, otherwise unknown/refusal, never false success. |
| TS-F12 | Given subject commit then reply failure, when response lost, then storage remains saved and same operation reconciles; zero channel notification, subject transcript copy or name change. |
| TS-F13 | Given clear, close/reopen, 90-day ceiling, author/opener/claimant erasure or restore, when reading/replaying, then no resurrected value; expired reads refuse immediately and physical purge meets <=1-hour budget. |
| TS-F14 | Given C-only rename refusal/429/timeout/crash/concurrent update, when request repeats, then <=1 PATCH containing name only, retained uncertain fence, no retry; target readback proves observation only. |
| TS-F15 | Given C-only renamed/deleted/moved channel or concurrent close, when reconciling, then exact ID/topic/ACL proof required, no replacement, no stale PATCH after close and no indefinite close hold; ticket/transcript/claim identity stays unchanged. |
| TS-F16 | Given local dry-run, malformed JSON/unknown fields/flags or oversized request, when invoked, then deterministic bounded error/next_action and zero network/SQL/nonce/quota/audit effects; telemetry contains no text/name/raw reason/hash. |

Existing reusable mocks/tests: [shared recording transport](../crates/discord/tests/common/mod.rs#L381-L448),
[receipt acceptance](../crates/discord/tests/ticket_creation_receipt_acceptance.rs#L22-L150),
[store races/retention](../crates/cutover/tests/tickets_store.rs#L103-L625),
[runtime lifecycle](../crates/bot/src/ticket_acceptance_tests.rs#L359-L619),
[destination moves](../crates/bot/src/ticket_destination_move_tests.rs#L180-L283)
and [claim/close truth](../crates/core/tests/ticket_claim_close_truth_controls.rs#L23-L100).
Their existence is not evidence of B/C passing. No behavior tests are added or
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

CEO/host decision: keep A, or substantiate B's use case and accept no channel-name
change, optional short-reference policy, proposed limits and conditional legacy
ineligibility. C requires a separate exact scope/value decision; do not silently
turn a private-subject acceptance into rename permission.

CISO decision: creation/current-role consistency, current member/visibility proof,
nonparticipant oracle, sensitive text/recipient limits and residual disclosure
race, retained/linkable version/replay facts, author erasure, ceiling/purge and
backup/restore behavior. New access/data boundaries require this disposition;
existing grants, activation and secret fences remain untouched. Missing proof or
rejected privacy/usefulness at review means A/no-build, not weakened permissions.

Metric: all 16 oracles and three option decisions have an explicit disposition by
2026-11-01; no live experiment, benefit or performance result is claimed. Unverified:
actual guild objects/permissions, deployed ticket activation, runtime behavior of
B/C (absent), subject storage/erasure and usable private UI. No production tests,
live rename, migration, private data collection, new service or spend occurred.

ACT: publish this indexed proposal and offline decision examples. WHY: keep private
case references out of channel metadata and prevent role drift from becoming a
read grant. UNDO: revert docs; there is no operational state to roll back.
