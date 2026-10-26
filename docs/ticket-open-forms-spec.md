# Staff-defined ticket open forms with modal intake

Status: **proposed; design only**. Written 2026-10-26; review by **2026-11-02**.
No runtime, migration, command, modal, component, permission, production action
or activation is added. This file is the authoritative editable repository spec;
the issue document records its exact commit/hash and review disposition.

## Decision and evidence

Every ticket today starts with staff asking triage questions in the channel,
because open carries no questions or answers. Whether TWO staff want
per-panel intake forms is a hypothesis, not measured demand. Comparable
products ship a per-panel pre-creation questionnaire (Ticket Tool Form Options,
tickets.tools Opening questions, open-ticket Modal Questions); that establishes
a comparable workflow, **not** TWO intent, safe answer handling or privacy
disposition.

| Option | Value and boundary | Recommendation |
| --- | --- | --- |
| A. No build; keep channel triage | No new retained answer text or effects; staff keep asking in-channel | **Use now**, pending intent and privacy decisions |
| B. Per-panel open form with modal intake | Staff-authored question set per panel; member answers in a modal before creation; answers posted once into the new channel; no separate answer store | **Smallest useful candidate**, only after the gates below; otherwise A |
| C. Richer variants (selects, answer store, per-category forms) | Native select pre-step, durable answer table, or distinct forms per category/panel-instance | **Do not build**; each adds a store/ACL/retention boundary with no demonstrated incremental value over B |

B is conditional on CEO/host intent alignment and CISO disposition exactly as
in [Exact intent and privacy review gates](#exact-intent-and-privacy-review-gates).
A merged design or pending review is not acceptance.

## Source-backed baseline

Inspected source: `c6e5213b516d5ef62b357cc2f5feeaf8f59889a8`. These are source
facts, not deployed-feature or permission measurements.

| Seam | Evidence and constraint |
| --- | --- |
| Open flow | [Runtime](../crates/bot/src/ticket_runtime.rs#L1073-L1180): fetch category proof, reserve, create channel, record/activate, post Controls message. Zero question/form/answer/modal fields anywhere in the ticket path: no match for `question`, `answer`, `modal`, `questionnaire` or `intake` in ticket runtime, domain, store or executor sources |
| Admission | [Store](../crates/cutover/src/tickets.rs#L51-L93) reserves under one active ticket per opener plus a cooldown via [decide_open](../crates/core/src/tickets.rs#L143-L164); refusal returns existing-ticket or cooldown copy and creates nothing. [Actions](../crates/core/src/tickets.rs#L21-L40): Open/Claim/Close; Creating/Open/Closing/CleanupPending/Closed |
| Staff today | [Authorization](../crates/core/src/tickets.rs#L108-L127) accepts current configured staff role OR ManageChannels OR Administrator for Claim/Close. [Runtime](../crates/bot/src/ticket_runtime.rs#L951-L981) takes member facts from the interaction and live config |
| Panels today | Exactly one panel: [config](../crates/bot/src/ticket_runtime.rs#L357-L434) reads a single `DISCORD_TICKET_PANEL_CHANNEL_ID`; [ensure_panel](../crates/bot/src/ticket_runtime.rs#L1831-L1853) posts one [Panel](../crates/discord/src/executor/tickets.rs#L364-L369) message. There is no panel registry, panel ID type or per-panel settings row; multi-panel provisioning does not exist |
| Channel proof | [Executor](../crates/discord/src/executor/tickets.rs#L46-L146) requires same guild/category, GuildText, canonical channel ID, exactly four explicit overwrites and exact topic `two-ticket:<reservation-id>`. [Create](../crates/discord/src/executor/tickets.rs#L150-L172) sends generated name and exact topic, single attempt |
| Close/transcript | [Close](../crates/bot/src/ticket_runtime.rs#L1212-L1262) freezes opener writes, captures and commits before cleanup. Close copy promises transcript saved up to 90 days. There is no member-facing reopen: `reopen_interrupted` ([store](../crates/cutover/src/tickets.rs#L208-L217), [domain](../crates/core/src/tickets.rs#L244-L273)) only recovers an interrupted close |
| Retention/erasure | [Store](../crates/cutover/src/tickets.rs#L338-L372) purges transcript rows and erases opener/claimant-owned tickets; [erasure inventory](../crates/cutover/src/member_erasure_plan.json#L219-L226) covers opener/claimant selectors only. Ticket metadata has no age purge here. [Privacy](privacy.md) excludes Discord-side deletion |

Do not replace/append to the topic marker. Preserve ticket ID, guild/channel
binding, transcript identity and retention, claim winner, close token and recovery
state. A form definition or modal submit cannot create/reopen a ticket by itself,
transfer a claim, repair ACLs, move a channel or alter transcript capture.
"Per-panel" in B means keyed by panel identity; with today's single panel that
degenerates to one form, and B adds no multi-panel provisioning.

Discord modal limits below are external API constraints the design must respect
(title <=45 chars, custom_id <=100 chars, at most 5 text-input components per
modal, text input max_length 1-4000); they are not TWO measurements. A Discord
modal carries **only text inputs** (short/paragraph): there is no native select,
so B's "choice" is a short-text answer validated against the option list.

## Conditional B: form definition contract

Everything below is a **future acceptance contract**, not an available command,
modal or API. No build/test card before accepted exact intent and CISO disposition.

- **Scope:** at most one form per panel identity (today: the single panel
  channel), at most **5 questions** per form (modal cap), each with a stable
  staff-chosen question ID (slug, `^[a-z0-9-]{1,32}$`), a label (1-45 chars),
  a kind (`short` | `paragraph` | `choice`), a required flag, and length bounds
  (`min_length` 0-4000, `max_length` 1-4000; paragraph `max_length` <=1000).
  A `choice` adds 2-10 options, each 1-100 chars, unique after NFC+trim.
- **Staff-only authoring:** create/edit/disable a form or question only with
  current Claim/Close-grade authority (current configured staff role OR
  ManageChannels OR Administrator), current guild membership, and the
  configured guild, feature and activation fences. Opener identity, claimant
  identity, past staff status, a service MAC, supplied actor ID or knowing a
  panel ID grants nothing. Members never author, edit, disable or preview the
  definition; they only see the runtime modal.
- **Versioning:** each panel form carries a monotonically increasing version
  (including disable/clear). Modal `custom_id` binds panel identity + form
  version + operation nonce. A submit against a superseded version refuses with
  re-open guidance; fields are never merged across versions and no partial
  ticket is created.
- **Disabled/empty form:** a panel with no enabled form, or an enabled form
  with zero questions, opens exactly as today (channel + rate limit only).
  Disabling a form never deletes already-posted answers from channels or
  transcripts; that content follows existing transcript retention/erasure.
- **Authoring UX:** definition edits are deliberate staff actions with explicit
  confirmation, never inferred from channel text. Staff preview shows the exact
  modal shape (labels, required flags, bounds, options) before enabling.

## Conditional B: modal intake contract

- **Order:** Open button -> eligibility pre-check -> modal -> member fills ->
  submit admission -> reserve/create/answers message. The modal opens only from
  the panel Open control in the configured guild; no direct modal URL or
  arbitrary-guild entry.
- **Close-without-submit creates nothing:** dismiss, timeout, failed pre-check
  or failed submit leaves zero effects: no ticket row, no reservation, no
  channel, no message, no audit row, no cooldown consumed, no quota consumed.
  A modal fill holds no server lock, lease or reservation.
- **Two-phase admission:** modal open runs an advisory eligibility read (same
  predicate family as `decide_open`: existing active ticket, cooldown) so an
  ineligible member gets the existing copy without filling a form. Submit runs
  the authoritative admission under the store path; a member who became
  ineligible mid-fill gets the standard refusal copy and nothing is created.
  Advisory open never reserves or extends a cooldown.
- **Submit validation:** NFC-normalize, trim ASCII space at both ends, then
  enforce required (non-empty after trim), `min_length`/`max_length` in Unicode
  scalars, and for `choice` an exact post-normalization match against one
  option (case-sensitive; no fuzzy match, no prefix match). Reject controls,
  bidi/zero-width formatting, line/paragraph separators in `short`/`choice`
  (paragraph keeps single newlines, no other category C). Invalid submit ->
  private error naming the question label only, zero creation effects, modal
  may be re-presented with valid answers preserved client-side only.
- **Double submit / double click:** same operation nonce replays the recorded
  receipt without a second ticket; distinct nonces race through normal reserve
  and the loser gets the existing-ticket copy. No client-generated ticket ID.
- **Accessibility:** every question has a text label; required state and errors
  are text, not color-only. Validation errors never echo raw answer text.

## Answers-attached contract

- **What lands where:** on successful creation only, the bot posts exactly one
  answers message in the new ticket channel, before the Controls message, as
  inert plain text (question label + answer, each pair separated; labels escaped,
  answers rendered with directional isolation, no embeds). Closed
  `allowed_mentions` on that post (empty parse/users/roles, replied_user=false).
  Mention syntax stored in an answer cannot ping from this message.
- **Transcript interplay:** answers are captured only through the existing
  channel-message capture path; B adds no transcript writer, column or format
  change. Transcript retention stays the existing 90-day ceiling; no separate
  answer store, revision history or answer index exists to purge.
- **Audit minimum:** guild/ticket ID, panel identity, form version, operation
  nonce, actor, time, per-question answered/skipped flags and refusal/unknown
  cause. Never raw answer text, answer length, text hash, question text beyond
  stable IDs, option text, or remote bodies.
- **PII minimization:** each question carries staff-authored hint text (<=100
  chars) shown in the modal, and the form enable confirmation warns staff that
  answers are visible to ticket staff and captured in transcripts. No answers in
  metric labels, SQL errors, traces, HTTP/access logs, clipboard export or
  private-to-public summaries. Do not claim automatic sensitive-data detection.
- **Rate-limit interplay:** modal open and failed submits never consume or
  extend cooldown; only an admitted reserve starts the cooldown window, exactly
  as today. No per-question or per-modal quota beyond the existing open
  cooldown; if abuse appears, a future slice proposes explicit bounds rather
  than this spec inventing constants.
- **Reopen inheritance:** none. There is no member reopen; opening a new ticket
  after close presents the current enabled form fresh. Prior answers never
  prefill, carry over or resurrect; close-recovery (`reopen_interrupted`) moves
  no answers because answers live only in channel/transcript content.

## Authority table

| Actor / action | Author form | Edit/disable form | View definition | Fill modal | View answers | Erase answers |
| --- | --- | --- | --- | --- | --- | --- |
| Current staff (role OR ManageChannels OR Administrator, member, proven channel access) | yes, confirmed | yes, confirmed | yes | n/a (staff open path unchanged) | yes, in-channel only | via existing member-erasure/transcript-purge when that actor is opener/claimant |
| Opener (member) | no | no | no | yes, own intake only | yes, own ticket channel only | own erasure clears own tickets' transcripts per existing selectors |
| Other member | no | no | no | own intake only | no | no |
| Claimant without current staff authority | no | no | no | own intake only | no new grant from this spec | no |
| Past staff / revoked | no | no | no | own intake only | no | no |
| Service MAC / supplied actor ID / ticket ID knowledge | no | no | no | no | no | no |

Reauthorize on every authoring read, modal open, submit admission and retry, not
just opening the UI. Deny when membership/roles/access facts are missing,
revoked or stale. Never widen an ACL to satisfy a request. For a
missing/foreign/hidden/deleted case or a nonparticipant, return the same
`NOT_AVAILABLE` shape with no IDs, state, version, counts or guessed link.

## State and race table

| Situation | Behavior |
| --- | --- |
| Modal dismissed / expired | Zero effects (see intake contract) |
| Overlong / invalid answer | Private per-question error; nothing created; no cooldown consumed |
| Form edited/disabled mid-intake | Submit version mismatch -> refuse with re-open guidance; no partial creation, no field merge |
| Member already has active ticket (at open or submit) | Existing-ticket copy; no modal on pre-check fail, no creation on submit fail |
| Cooldown active (at open or submit) | Cooldown copy with next-try time; modal fill never extends it |
| Double submit, same nonce | Receipt replay; at most one ticket |
| Double submit, distinct nonces | Reserve race; loser gets existing-ticket copy |
| Channel create fails after reserve | Existing abandon/recovery path unchanged; answers message never posts without a bound channel |
| Answers post fails after activate | Ticket stays open (creation already committed); record bounded failure token, no retry loop that duplicates the message; staff triage in-channel as today |
| Close races submit admission | Admission under store serialization wins or refuses; no ticket created into Closing/CleanupPending/Closed |
| Config (category/staff/panel) changes mid-intake | Submit re-reads live config; proof failure refuses like today |
| Member erasure after open | Existing transcript/opener selectors apply; no separate answer purge path exists |

## Alternatives considered

- **A. No build (recommended now):** keep in-channel triage. Zero new surface,
  zero retained answer text, zero modal/version machinery. Choose A unless
  intent + CISO gates pass.
- **B. Conditional text-modal forms (smallest useful):** as specified above.
  Single form per panel, <=5 text questions, no new store.
- **C1. Native select pre-step:** a select-menu step before/alongside the modal
  for choice questions. Rejected for B: two-interaction flow, component-state
  expiry, and custom-id state budget, for value unproven over validated text.
- **C2. Durable answers table:** queryable Q/A rows with own retention/erasure.
  Rejected for B: a new PII store, erasure inventory entries, backup/restore
  non-resurrection proof and access grants, with no demonstrated need beyond
  channel+transcript content.
- **C3. Per-category/per-instance forms:** distinct forms per ticket category or
  per panel message instance. Rejected: no panel registry or category-scoped
  ticket config exists; provisioning that surface is its own design.
- **C4. Post-creation threaded intake:** create first, ask in thread, close
  empty on abandon. Rejected: creates channels that must be cleaned up,
  inverts the close-without-submit-creates-nothing guarantee, and complicates
  cooldown semantics.

## Cut-ready future slices (each <=1 day, only after gates)

1. Form definition schema + staff authoring command (versioned, staff-only).
2. Modal open wiring with advisory eligibility pre-check.
3. Submit admission + validation + version-match refusal.
4. Answers message render (inert text, closed mentions) after creation.
5. Double-submit idempotency via operation nonce receipts.
6. Acceptance matrix execution (Test Author owns protected tests).
7. CISO evidence pack: audit/log absence-of-text proofs, erasure walk.

Each slice needs its own exact-scope card; none is authorized by this design.

## Agent contract and runnable offline decision mock

No new endpoint or CLI flag exists. Proposed bounded request: schema, string IDs
(guild/panel/actor/operation/form_version), action open/submit, answers only for
submit (question ID -> text), and strict Boolean dry_run. Reject unknown
fields/actions, bad IDs, non-Boolean/missing dry_run and >8192-byte request
before dispatch. Replies <=2048 UTF-8 bytes have
`schema,operation_id,outcome,error_code,next_action,created,answers_posted`;
IDs/version and per-question flags appear only after successful proof. No raw
request serialization in observability.

A dry-run uses **local synthetic facts only**. It has no network or credentials.
It never reserves nonces, rows or cooldowns; writes SQL/audit records; schedules
work; or calls Discord. It predicts only the supplied snapshot, never live
authority or creation readiness.
The following miniature mock runs offline and checks policy examples; it is not
production code, protected behavior tests, runtime acceptance or a security proof.
Its dictionaries are policy fragments, not complete replies; actual replies must
use the seven-field envelope above, including on refusal.

```pycon
>>> import json, pathlib, re, unicodedata
>>> def valid_answer(raw, *, kind="short", required=True, max_len=500):
...     text = unicodedata.normalize("NFC", raw).strip(" ")
...     if not text:
...         return not required
...     if len(text) > max_len:
...         return False
...     if kind in {"short", "choice"}:
...         return not any(unicodedata.category(c).startswith("C") or ord(c) in (8232, 8233) for c in text)
...     return not any(unicodedata.category(c).startswith("C") and c != "\n" or ord(c) in (8232, 8233) for c in text)
>>> def choice_match(raw, options):
...     text = unicodedata.normalize("NFC", raw).strip(" ")
...     return text in options
>>> def decide(*, eligible=True, version="7", submitted="7", enabled=True, action="submit", answers_ok=True, dry_run=True):
...     if action not in {"open", "submit"} or type(dry_run) is not bool:
...         return {"outcome":"refused", "error_code":"INVALID_REQUEST", "next_action":"fix_request"}
...     if not enabled:
...         return {"outcome":"plain_open" if action == "open" else "created_plain", "error_code":"FORM_DISABLED", "next_action":"open_without_form"}
...     if not eligible:
...         return {"outcome":"refused", "error_code":"NOT_ELIGIBLE", "next_action":"stop"}
...     if submitted != version:
...         return {"outcome":"refused", "error_code":"VERSION_CONFLICT", "next_action":"reopen_form"}
...     if action == "submit" and not answers_ok:
...         return {"outcome":"refused", "error_code":"INVALID_ANSWERS", "next_action":"edit_answers"}
...     return {"outcome":"preview", "error_code":"PREVIEW_ONLY", "next_action":"confirm_or_edit", "created":False, "answers_posted":False}
>>> assert decide()["error_code"] == "PREVIEW_ONLY"
>>> assert decide(action="open")["error_code"] == "PREVIEW_ONLY"
>>> assert decide(action="rename")["error_code"] == decide(dry_run="false")["error_code"] == "INVALID_REQUEST"
>>> assert decide(eligible=False)["error_code"] == "NOT_ELIGIBLE"
>>> assert decide(submitted="6")["error_code"] == "VERSION_CONFLICT"
>>> assert decide(answers_ok=False)["error_code"] == "INVALID_ANSWERS"
>>> assert decide(enabled=False, action="open")["next_action"] == "open_without_form"
>>> assert all(not valid_answer(s) for s in ["", " " * 5, "x" * 501, "a\nb", "a\tb", "a" + chr(8238) + "b"])
>>> assert valid_answer("", required=False) and valid_answer("ok", required=True)
>>> assert valid_answer("a\nb", kind="paragraph") and not valid_answer("a\nb", kind="short")
>>> assert choice_match("Billing", ["Billing", "Appeal"]) and not choice_match("billing", ["Billing", "Appeal"])
>>> assert not choice_match(" Bill", ["Billing", "Appeal"]) and choice_match(" Billing ", ["Billing", "Appeal"])
>>> text = pathlib.Path("docs/ticket-open-forms-spec.md").read_text()
>>> cases = re.findall(r"^\| (TF-F\d{2}) ", text, re.M)
>>> assert cases == [f"TF-F{i:02}" for i in range(1, 17)]
>>> print(json.dumps({"schema":"ticket.open_forms.mock.v1", "matrix_rows":len(cases), "outcome":"offline_examples_pass", "created":False, "answers_posted":False}, sort_keys=True))
{"answers_posted": false, "created": false, "matrix_rows": 16, "outcome": "offline_examples_pass", "schema": "ticket.open_forms.mock.v1"}

```

The mock always refuses version mismatch and invalid answers and never creates,
even for `dry_run=False`; it has no live adapter. It does not model durable
reserve, cooldowns, quotas, retention, JSON parsing or actual ACL resolution.
Those need independent tests. Run from the repo root:

```sh
python3 -m doctest -v docs/ticket-open-forms-spec.md
python3 scripts/check_docs_index.py --json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
```

## Must-pass future acceptance matrix

All fixtures below are **planned, not executed runtime tests**. Use synthetic
identities, unique channels per case, an injected clock, independent disposable
test-DB connections and the shared recording Discord double. For each row collect
bounded result JSON plus row/message/audit counts; never capture answer text in
evidence. Future UI staging evidence needs separately authorized access, not
live private-case collection. Timing limits are acceptance targets, not SLAs.

| Case | Given / when / then oracle |
| --- | --- |
| TF-F01 | Given enabled 5-question form, when eligible member opens and submits valid answers, then one ticket, one answers message before Controls, one audit row without text; no extra posts. |
| TF-F02 | Given required/optional mix, when optional left blank, then creation proceeds with skipped flag; blank required refuses with per-question error and zero effects. |
| TF-F03 | Given min/max bounds and choice options, when answers at 1/max/max+1 and option/non-option/case-variant submitted, then exact bounds above; no truncation, echo, mention firing or unsafe render. |
| TF-F04 | Given staff authoring, when current staff creates/edits/disables, then version increments and modal shape changes; member/opener/claimant-alone/past-staff/forged actor can never author or view the definition. |
| TF-F05 | Given modal opened, when dismissed/expired or pre-check fails, then zero ticket row, message, audit, cooldown or quota effect; re-open starts fresh. |
| TF-F06 | Given overlong/empty-required/invalid-choice submit, when submitted, then private per-question error, nothing created, cooldown unconsumed; valid answers may be re-presented client-side only. |
| TF-F07 | Given form edited or disabled mid-intake, when stale-version submit arrives, then version-mismatch refusal with re-open guidance; no partial creation or cross-version field merge. |
| TF-F08 | Given existing active ticket or active cooldown at open or submit, when intake attempted, then standard existing/cooldown copy; modal open never reserves or extends cooldown. |
| TF-F09 | Given double submit with same nonce, when replayed, then receipt replay with at most one ticket; distinct nonces race through reserve and loser gets existing-ticket copy. |
| TF-F10 | Given channel-create failure after reserve, when creation fails, then existing abandon/recovery applies and no answers message posts without a bound channel. |
| TF-F11 | Given answers-post failure after activate, when post fails, then ticket stays open with one bounded failure token; no retry loop duplicates the message. |
| TF-F12 | Given close admission racing submit, when both commit, then store serialization admits one outcome; no ticket is created into Closing/CleanupPending/Closed and no close is corrupted. |
| TF-F13 | Given prior ticket closed with answers, when member opens again, then current form presented fresh; no prefill, carryover or resurrection of prior answers. |
| TF-F14 | Given close, 90-day transcript ceiling, opener/claimant erasure or restore, when reading/replaying, then answers follow existing transcript retention/erasure exactly; no separate answer purge path. |
| TF-F15 | Given answers posted, when inspecting audit/metrics/logs/traces/SQL errors, then question IDs, versions, flags and counts only; no raw text, length, hash, option text or remote body anywhere. |
| TF-F16 | Given local dry-run, malformed JSON/unknown fields/flags or oversized request, when invoked, then deterministic bounded error/next_action and zero network/SQL/nonce/cooldown/audit effects. |

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

CEO/host decision: keep A, or substantiate B's triage-saving use case and accept
single-form-per-panel scope, text-only modal limits, <=5 questions, validated
choice, answers-in-channel retention, and no member authoring or prefill.

CISO decision: staff-only authoring proof, member definition invisibility,
version-mismatch refusal, answer PII/recipient limits and residual disclosure
race, transcript/audit text boundaries, cooldown non-consumption, erasure via
existing selectors, ceiling/purge and backup/restore behavior. New access/data
boundaries require this disposition; existing grants, activation and secret
fences remain untouched. Missing proof or rejected privacy/usefulness at review
means A/no-build, not weakened permissions.

Metric: all 16 oracles and three option decisions have an explicit disposition by
2026-11-02; no live experiment, benefit or performance result is claimed. Unverified:
actual guild objects/permissions, deployed ticket activation, runtime behavior of
B (absent), modal UX, answer storage/erasure beyond transcript behavior, and usable
staff authoring UI. No production tests, live modal, migration, private data
collection, new service or spend occurred.

ACT: publish this indexed proposal and offline decision examples. WHY: stop every
ticket from starting with repeated triage questions without creating a new PII
store or weakening open admission. UNDO: revert docs; there is no operational
state to roll back.
