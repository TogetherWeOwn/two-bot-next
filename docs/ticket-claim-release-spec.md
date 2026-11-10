# Tickets: replay-safe release of my own staff claim

Status: **Deferred to 2026-12** (owner decision 2026-11-01). Revisit then, or
earlier if a host names a need and an M2 slot opens. Bundle with [ticket
priority](ticket-priority-spec.md). Original proposal: written 2026-10-25. No
implementation, migration run, Discord mutation, new credential, spend or
production activation. Merge publishes a proposal, not an execution grant. This
file is the single repository contract; the task document mirrors it. Intent
review binds the exact file SHA-256 and commit, not a moving branch.

## 1. Decision and evidence

Consumer: a staff member who cannot continue one open support ticket.
Hypothesis: returning only the assignment to the queue avoids abandoned cases
without closing the channel. Demand and operational benefit are unmeasured.

[Ticket Tool claiming options](https://docs.tickettool.xyz/dashboard/panel-configs/claiming-options.md),
read 2026-10-25, documents claimant-only unclaim, but exempts dashboard roles
and administrators and offers replacement claimants, permission/category/name
changes and role changes. Those broader powers are **not** proposed here.

| Option | Value / cost | Disposition |
| --- | --- | --- |
| A. No build; manual staff coordination | No new writes/schema; another staff member can help in the existing channel, but assignment remains with the claimant. Never pretend a chat handoff cleared it; no direct SQL workaround | Standing fallback until acceptance and whenever safe migration cannot fit |
| B. Self-release with generation-bound confirmation | One authenticated current claimant, one open ticket, assignment clear only; requires durable fencing and atomic audit | **Recommend for spec/intent review**, the smallest safe self-release; not approved to build |
| C. Forced reassignment / staff override | Handles absent claimants but introduces authority over another staff member's assignment, notification and race policy | Reject from this slice; requires separate demand and security review |

No collaborator management, forced release (even for an administrator),
close-confirmation, channel/role/category/name/permission change, transcript
work or queue implementation. Existing queue and close-confirmation work are
adjacent, not prerequisites; do not copy their designs into this feature.

## 2. Verified baseline and seams

Inspected source head: `0b8f8c6427859a606c4be710224dc68649fabe00`.
These are repository facts, not deployed-state assertions:

- `crates/core/src/tickets.rs:8-35,73-84,108-125,187-203`: Open/Claim/Close;
  `claimed_by` has no generation. Staff means configured staff role **or**
  Manage Channels **or** Administrator, in the configured guild. Claimant
  identity alone never supplies staff authority.
- `crates/cutover/migrations/0210_tickets.sql:2-35`: TEXT ticket primary key,
  unique nullable channel, five states, TEXT lifecycle times; transcripts
  cascade on ticket deletion. The runtime reserves random 32-lowercase-hex
  IDs (`crates/bot/src/ticket_runtime.rs:1084-1087`); the schema also permits
  legacy IDs. Neither names nor channel topics are claim identity.
- `crates/cutover/src/tickets.rs:96-178`: `by_channel` is an ordinary read;
  transitions lock `(guild_id,id)` FOR UPDATE and persist assignment, channel
  and lifecycle together. `purge_expired` at `:338-360` is a **separate** status
  writer. A process-local mutex or a read followed by an unconditional UPDATE
  is not a cross-process CAS.
- `crates/bot/src/ticket_runtime.rs:951-1054` and
  `crates/bot/src/command_runtime.rs:1616-1686`: current authority comes from
  invoking member/live config; successful ephemeral defer precedes execution.
  A new action needs shared-router recognition and admission, not a private
  dispatcher. Existing claims post a public marker; release must not copy it.
- `crates/core/src/audit_store.rs:175-212,791-800`: audit INSERT uses its own
  pool and has a closed kind decoder. It is **not** already atomic with ticket
  transitions; a transaction-aware insertion and explicit non-mirrored kind
  are required, not a best-effort call after commit.
- `crates/cutover/src/member_erasure_plan.json:199-201,218-226` erases audit
  actor/target/metadata and ticket/transcript opener/current claimant.
  `crates/cutover/src/retention.rs:94,605-607` provides 45-day operational audit
  retention; ticket transcripts retain their existing 90-day capture ceiling.
- `crates/cutover/src/rollback_import.rs:447-451` populates whole records;
  absent JSON fields become NULL rather than column defaults.
  `crates/cutover/src/rollback_delta.rs:267-273` measures ticket creation/close
  timestamps, so claim-only changes are currently invisible to that measure.

No live guild, database, claimant, migration ledger or host workflow was
inspected. [Ticket runtime](tickets.md) documents other lifecycle fences;
this proposal does not establish that older acceptance notes are current.

## 3. Private UX and authority

Proposed entry: `/tickets release`, **in the one ticket channel**, no target
member, ticket-ID or force option. No shared-channel release button, public
marker, DM, ping or queue announcement. All replies are ephemeral, plain-text
bounded to 300 characters, with parsed mentions and replied-user pings off.

1. Preserve configured-guild, ticket feature/activation, application identity,
   existing component admission and timeout fences. A failed private defer
   performs no mutation. Obtain the actor only from the authenticated Discord
   interaction, never options, custom IDs or agent-supplied JSON.
2. Resolve the current channel to the same-guild ticket. Require `open`, a
   nonempty bound channel and **current** `claimed_by == actor`. Re-evaluate
   the existing staff predicate using current configured staff role and fresh
   invoking-member/permission evidence at preview and again on confirmation,
   after lane admission. Loss of staff status denies even their own release;
   administrators cannot release someone else's claim. Unknown proof refuses.
3. Prove channel ID/guild/type and actor access through the shared executor;
   missing channel, 403 or unproven access returns unavailable, leaves the row
   intact and does not invoke recovery/retirement. No history read is needed.
   Authority/channel proof has a five-second monotonic deadline: check before
   transaction entry, after lock acquisition and before commit. If any lock
   wait exhausts it, roll back and ask for a fresh preview; never refresh via
   Discord I/O inside the transaction or weaken the deadline. Recheck local
   policy revision before commit; observed invalidation discards the proof.
   Discord role/channel changes after the external read cannot be made atomic
   with Postgres; that bounded residual race is not a new role grant.
4. Preview: “Return your claim to the unclaimed queue? The ticket stays open;
   channel access does not change.” Offer **Release my claim** and **Cancel**.
   Prepare is read-only. Cancel writes nothing. No claim means a private no-op
   with no confirmation control. Another claimant gets a generic refusal,
   never their name or ID. Lifecycle refusal takes precedence over assignment.
5. Confirmation carries exactly the preview's ticket ID, revision and expiry;
   never silently upgrade to a newer claim. Strict ID format:
   `two:tr:1:<32-hex-ticket-id>:<revision>:<expires-unix-seconds>:<boot-nonce>`.
   Both numbers use canonical unsigned decimal (no leading zeros except `0`);
   revision is 0..9223372036854775807 and expiry is database UTC +300 seconds,
   at most 12 digits. Boot nonce is canonical unpadded base64url of 16 random
   bytes (22 characters), newly generated on **every** runtime start, never
   restored/reused. Entropy failure leaves release unavailable. Maximum 97
   ASCII characters, below Discord's 100-character limit. Match nonce against
   this receiving runtime; another runtime/restart refuses stale and asks for
   a fresh preview, never refreshes the token. No cross-runtime availability
   guarantee is made. Bind real guild/channel/application to the row; require
   Discord-authenticated bot authorship, ephemeral flag and originating preview
   recipient metadata matching the actor. If that metadata is absent, refuse;
   no exact persisted preview-message-ID binding is claimed. The ID is a
   nonsecret witness, **not a bearer capability**. Never log raw custom IDs,
   interaction tokens or bodies. Authorized diagnostic results may carry the
   stable ticket/revision, including for commit-unknown inspection.
   Legacy non-32-hex ticket IDs get unavailable, never guessed/truncated IDs;
   no ID backfill or new mapping table in this minimum design.

A confirmed release changes **only** `claimed_by` to NULL and the witness
metadata in §4. Opener, channel, lifecycle/times, access overwrites, roles,
messages, content and transcripts remain byte-identical. The queue, if present,
reads the now-unclaimed row; release cannot promise another staffer will help.
Old public claim markers are historical facts, not current assignment state;
this slice does not edit or delete them.

## 4. Durable witness, transaction and compatibility

Proposed additive schema, **not DDL to execute**:

- `tickets.claim_revision BIGINT DEFAULT 0`, eventually NOT NULL, CHECK >=0.
  A claim ID is the stable pair `(ticket_id, claim_revision)`, within its guild.
  Already-claimed rows start at revision 0; only post-upgrade previews can
  issue release controls. Never infer a version from `claimed_by` or a clock.
- `tickets.claim_revision_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP`, eventually
  NOT NULL, for conservative rollback-delta discovery **only**, never CAS.
  Initial migration time may select extra rows for export; it must not omit
  assignment changes. No claim-history/receipt table and no new index needed.
- A database-owned BEFORE UPDATE trigger increments revision once, and stamps
  `clock_timestamp()`, whenever `(claimed_by,status,channel_id)` is DISTINCT
  FROM its OLD tuple. Include close, reopen, recovery, channel rebinding and
  direct purge reconciliation, not just claim/release. No-op updates do not
  increment. Reject caller changes to either witness column and to immutable
  `(id,guild_id,opener_id)`; reject overflow, never wrap/reset. The function is
  security-invoker, fixed search_path, no dynamic SQL or role elevation.
  Runtime roles must not own/disable/replace it. Reads, transcript-only purge
  and audit retention never reset the witness. Deletes leave no usable row;
  normal reservation generates fresh 128-bit random ticket IDs inside the
  trusted runtime; production callers cannot supply/recycle an ID. PK conflicts
  refuse instead of replacing a row. This uses cryptographic collision resistance
  (not a permanent deleted-ID tombstone); accidental reuse within one boot has
  negligible, not mathematically zero, probability. Intent review must accept
  that existing identity assumption. Import/restore invalidates the boot nonce
  before admission (§4 below), so historical IDs cannot revive old controls.
- Add nullable `operational_audit_log.ticket_id TEXT REFERENCES tickets(id)
  ON DELETE CASCADE`, with an index on non-NULL `ticket_id` added concurrently
  per migration policy. Other audit kinds keep NULL. Release derives this FK
  and guild from the locked parent, not input; typed decoder supports this kind.
  This explicit relation bounds ticket-linked deletion; do not JSON/regex-scan
  the whole audit log to simulate a cascade.

Release transaction: take existing erasure-compatible table locks first if
needed by the atomic audit seam, then lock the same-guild ticket row. Recheck
policy proof freshness, actual channel binding, open state, actor equality,
exact expected revision and unexpired token against database time. Conditional
UPDATE predicates repeat these checks, SET **only** `claimed_by=NULL`; the
trigger advances the revision. Read its RETURNING witness (not the pre-trigger
in-memory struct), insert §5 audit on the **same connection/transaction**, then
commit. Only a confirmed commit can return saved. Zero affected rows is a
refusal; never fall back to another ticket or retry with the newest revision.
No Discord I/O or advisory-lock wait is held inside this transaction.

This serializes with current claim/close writers on the row. Example:
`A@7 -> release -> unclaimed@8 -> claim A -> A@9`. A delayed release of `A@7`
refuses even though the actor is A again. Close first makes release stale;
release first may be followed by the existing authorized close. Close/reopen
also advances revisions, so an old release cannot act on a recovered lifecycle.
Two deliveries using revision 7 can commit at most one clear; the loser is stale,
not a fabricated replay success. No audit receipt is used as a replay fence.

**Bounded follow-up admission requirements:**

- Allocate unused bot migration numbers and checksums per [migrations](migrations.md).
  Expand with defaults, backfill/validate constraints and finalize NOT NULL
  separately as required by strict migration lint; do not edit migration 0210
  or legacy fixtures. No migration number or applied state is assumed here.
- Existing explicit SELECT lists tolerate added columns while release stays
  dark; new readers must select/decode revision/time everywhere and fail
  unavailable on missing/NULL/invalid witness or absent trigger. Trigger tests
  must cover old-style writers that omit revision, including purge. New
  `persist`/transition responses must return the **database** witness, not
  an unincremented domain clone. Table permissions are verified, not broadened.
- Gate release off until migration, trigger, audit seam, both erasure paths,
  import/export and exact-head caller tests are accepted. No mixed-writer
  interval may issue a control without the database trigger present. Rolling
  back to the old binary leaves additive columns/trigger in place and stops
  release issuance; do not drop/reset them as an operational undo.
- Update rollback delta to include `claim_revision_at`. Import/export must
  explicitly preserve valid witness fields in same-history exports, fill
  older missing fields only through a reviewed compatibility path (not NULL
  whole-record inserts), and never overwrite a newer row with a stale dump.
  On **any** ticket import or disaster restore/history rewind, keep release
  dark until every old worker is proved drained and all admitting runtimes
  start with fresh boot nonces. Old controls then fail nonce validation even
  after database clock rewind; waiting 300 seconds alone is insufficient.
  Unknown drain/nonce evidence keeps release dark. Restore re-applies erasure
  before reads. This is a restore precondition, not a restore operation grant.

If these coupled seams cannot fit a bounded reviewed follow-up, choose A and
**defer B rather than remove the generation, trigger or migration checks**.

## 5. Truthful outcomes, audit and privacy

| outcome / reason / next_action | Private response and semantics |
| --- | --- |
| `preview / confirm_required / confirm_same_claim` | Preview above; no persisted change |
| `saved / released / none` | “Your claim was released. The ticket remains open.” Confirmed commit only |
| `no_op / unassigned / none` | “This open ticket is unclaimed. Nothing changed.” Fresh authorized preview only, not a stale submit |
| `no_op / cancelled / none` | “Cancelled. Nothing changed.” Cancel performs no release |
| `stale / claim_changed / refresh_private_preview` | “This control is out of date. No new change was made. Refresh before continuing.” Also duplicate, changed lifecycle or expired control; never names the new claimant |
| `denied / not_claimant_or_staff / ask_authorized_staff` | “Only the current staff claimant can release this ticket.” No assignment details |
| `unavailable / context_unavailable / verify_access` | “Release is unavailable here. No change was saved.” Proven no-commit only; collapse absent/foreign/unproven context or exhausted proof deadline; reveal no ticket/claim IDs |
| `unavailable / store_unavailable / retry_private_preview` | “Release could not be saved. Refresh and try again.” Only proven pre-commit failure/rollback; no automatic mutation retry |
| `unavailable / commit_unknown / inspect_current_assignment` | “The release result is unconfirmed. Check current assignment before trying again.” Never assert rollback or success after ambiguous commit |

Failed reply edit **after** confirmed commit does not undo or repeat release.
Retry delivery follows current state/generation and can be stale, not saved.
Restart reloads durable revision and policy but invalidates old controls via
its fresh boot nonce; no in-memory delivery dedup is required for safety.
Lost commit acknowledgement remains unknown even if a later read is
unassigned: another action could have caused that state. No-op is not proof of
who performed an earlier release. Unknown outcomes do not enable fallback SQL.

Atomic saved audit: one `ticket_claim_release` operational audit event per
`(guild,ticket,revision_after)`, with fixed action `release`, occurred_at,
actor_id = prior claimant, target_id = opener, explicit ticket_id FK, and
metadata containing only `revision_before`, `revision_after`, `outcome=saved`,
`reason=released` (<=512 UTF-8 bytes). Entry ID is
`tr1:<guild>:<ticket>:<revision_after>` (<=77 ASCII characters). No raw
interaction ID/token, custom ID, body, names, channel IDs, transcript, free text
or other claimant history. Specify the new event kind in enum/decoder tests;
`mirror_channel_id=NULL`, `delivery_state=none` permanently, including config
changes/reconciliation. Audit failure **or zero inserted rows** rolls back the
assignment clear; baseline `ON CONFLICT DO NOTHING -> false` is not success.
Never commit a release after an entry-ID collision or adopt another row's facts.
Denials/replays need only bounded reason/outcome counters, no actor/claim IDs
as metric labels and no unbounded per-refusal audit rows.

Actor/opener/ticket linkage is personal data, used only for staff accountability.
Operational audit uses the existing 45-day purge, not an indefinite claim ledger;
both operator and queued member-erasure executors must delete rows by actor OR
opener via those explicit columns. The indexed FK cascades ticket-linked release
audit on parent deletion/guild purge. Update/test both erasure contracts, schema
coverage and dry-run counts (including FK side effects); existing member-only
predicates alone do not promise this ticket-ID cascade.
Erasure of a former claimant after release still finds their audit row even
though `tickets.claimed_by` is NULL. Numeric revision is retained with the
parent ticket until that ticket is removed; it holds no former actor and is
never reset by audit erasure/retention. Existing transcript erasure/retention
is unchanged. Do not create a new transcript on release or rewrite a captured
claimant. Restore re-applies erasure before reads; no backup or Discord-side
message destruction is claimed by database erasure.

## 6. Agent handoff and deterministic acceptance oracle

No new executable CLI, MCP or HTTP write surface is registered. Agent handoff
is a diagnostic/result contract, **not** authority to impersonate Discord users.
Discover this file through the [documentation map](README.md). Proposed JSON:

```json
{"schema":"tickets.release.v1","mode":"dry_run","guild_id":"g1","ticket_id":"0123456789abcdef0123456789abcdef","claim_id":{"ticket_id":"0123456789abcdef0123456789abcdef","revision":"7"},"outcome":"preview","reason":"confirm_required","retryable":false,"next_action":"confirm_same_claim"}
```

All generations are decimal strings (avoid JSON/JavaScript precision loss).
Identity fields are null/omitted before authorization or for foreign/unavailable
context. `mode=dry_run` is a typed read-only path: same checks, no UPDATE, audit,
revision increment, permission effect, job or send beyond the private preview;
it predicts eligibility at read time, never reserves it. Commit-unknown has
`retryable=false`: inspect, never blindly reapply. Handoff also includes exact
source commit/spec hash, protected-file hashes and the check below; examples
use synthetic IDs only. No runtime activation follows a passing document check.

Fixture base: configured guild g1, channel c1, open ticket T, claimant A,
revision 7; B is another staffer, N a nonstaff member. Each row resets it.
Every release/refusal asserts unchanged opener/channel/lifecycle/content/roles/
permissions/transcript except lifecycle changes explicitly injected by the
fixture. No new public messages, DMs or mentions in any row.

| Case | Deterministic order / required assertion |
| --- | --- |
| claimant | A previews/confirms 7 -> saved, NULL@8, exactly one audit |
| non_claimant | B confirms -> denied, A@7 unchanged, including Administrator B |
| nonstaff | N or A after staff removal confirms -> denied, unchanged |
| foreign_guild | g2/DM/application mismatch -> unavailable, no IDs or writes |
| wrong_binding | Foreign channel/message/private recipient -> unavailable, unchanged |
| unassigned | Fresh authorized preview of NULL@8 -> no_op, no control/audit |
| cancel | Cancel -> no_op, unchanged |
| dry_run | Eligible A@7 -> preview, no state/audit change |
| same_actor_aba | Hold release 7; release 7 commits, A reclaims 9; deliver held 7 -> stale, A@9 |
| other_actor_aba | Release 7; B claims 9; delayed A release 7 -> denied or stale, B@9 unchanged |
| duplicate | Two independent connections read 7; barrier; submit both -> one saved, one stale, one audit |
| close_first | Barrier; close commits closing@8 before release 7 -> stale, closing unchanged |
| release_first | Release commits NULL@8 before authorized close -> saved then closing@9, transcript path unchanged |
| close_reopen | Close 8 then interrupted reopen 9; delayed release 7 -> stale, A@9 open |
| stale_ui | Expired at exact DB deadline, altered revision or old UI -> stale, unchanged |
| restart_before | Drop process after preview; reload A@7 with fresh boot nonce; old control -> stale; fresh preview may save once |
| restart_after | Crash after commit before reply; old nonce/duplicate 7 -> stale, NULL@8, no second audit |
| missing_channel | 10003, 403, missing binding or unknown proof -> unavailable, no retire/release |
| db_failure | Inject before UPDATE or force audit INSERT failure -> rollback, A@7, unavailable |
| commit_unknown | Disconnect at COMMIT acknowledgement -> unavailable/commit_unknown; inspect, never claim saved |
| reply_failure | Confirm commit then fail private edit -> NULL@8 and one audit, no compensation/retry |
| legacy_writer | Old SQL claim/close/reopen and purge status reconciliation still advance DB witness |
| erasure | Former A audit erased after release; parent erasure cascades audit/transcript; g2 unchanged; dry-run unchanged |
| audit_expiry | Purge 45-day audit -> witness unchanged; stale 7 never valid again |
| schema_rollback | Missing/NULL witness/trigger, old-format import or unproven restore drain -> unavailable/dark |
| overflow | Revision at i64 MAX refuses mutation; never NULL@0 |
| proof_timeout | Barrier holds row lock past proof deadline -> rollback/unavailable; no Discord I/O while locked |
| id_reuse | Caller tries supplying a deleted ticket ID -> refused at runtime boundary; no restore/import under an old boot nonce |
| audit_collision | Seed deterministic audit entry-ID collision -> zero insert forces rollback, never saved |
| clock_rewind_restore | Restore old row/revision and rewind DB time; drained workers/new boot nonce -> old control stale; unproven drain stays dark |

Offline consistency recipe (Python stdlib, repo root, no network/DB/Discord):

```sh
python3 -m doctest -v docs/ticket-claim-release-spec.md
python3 scripts/check_docs_index.py --json
```

The doctest checks the oracle's arithmetic, discovery and handoff hashes only;
it does **not** prove SQL serialization, trigger installation, auth, atomic
audit, erasure or rendered replies. Future tests need actual callers, separate
disposable DB connections, barriers rather than sleeps, injected clock/commit/
reply failures and the shared Discord double. No live tests here.

```pycon
>>> import hashlib, json, pathlib
>>> p = pathlib.Path("docs/ticket-claim-release-spec.md")
>>> text = p.read_text()
>>> cases = text.split("| Case |", 1)[1].split("\n\n", 1)[0].splitlines()[2:]
>>> names = [line.split("|")[1].strip() for line in cases]
>>> assert len(names) == len(set(names)) == 30
>>> assert {"same_actor_aba", "duplicate", "close_first", "release_first", "close_reopen", "restart_after", "erasure", "commit_unknown"} <= set(names)
>>> # Abstract witness arithmetic, not a runtime or database implementation.
>>> released, reclaimed, reopened = 7 + 1, 7 + 2, 7 + 2
>>> assert released == 8 and reclaimed == reopened == 9 and 7 != reclaimed
>>> assert 7 != reopened and not (300 < 300)  # equality is expired
>>> token = "two:tr:1:" + "a" * 32 + ":" + str(2**63 - 1) + ":253402300799:" + "A" * 22
>>> assert len(token) == 97 and len(token) <= 100
>>> protected = ["crates/core/src/tickets.rs", "crates/cutover/src/tickets.rs", "crates/cutover/migrations/0210_tickets.sql", "crates/cutover/src/member_erasure_plan.json"]
>>> hashes = {f: hashlib.sha256(pathlib.Path(f).read_bytes()).hexdigest() for f in protected}
>>> assert all(len(h) == 64 for h in hashes.values())
>>> handoff = {"schema": "tickets.release.design-check.v1", "outcome": "checked", "reason": "offline_oracle_only", "next_action": "request_exact_spec_intent_alignment", "spec_sha256": hashlib.sha256(p.read_bytes()).hexdigest(), "protected_file_hashes": hashes}
>>> assert json.loads(json.dumps(handoff)) == handoff
>>> print(json.dumps({"schema": handoff["schema"], "cases": len(names), "outcome": handoff["outcome"], "reason": handoff["reason"], "next_action": handoff["next_action"]}, sort_keys=True))
{"cases": 30, "next_action": "request_exact_spec_intent_alignment", "outcome": "checked", "reason": "offline_oracle_only", "schema": "tickets.release.design-check.v1"}

```

## 7. Boundary disposition, intent gate and undo

**CISO disposition: DESIGN_ONLY; no runtime grant.** Narrow circulation of B
is acceptable; an actor-equality-only toggle, override release, silent generation
upgrade, new permission effect or migrationless activation is refused. This is
an assessment of the proposal, **not independent approval of our own request**.
An independent boundary disposition remains required for future security-sensitive
implementation, alongside exact-head CI, CodeRabbit and Devin Review.

Request CEO acceptance and actual staff-host intent alignment on this exact
spec through the President/COO, not directly from the owner. The decision must
accept/revise: A fallback versus B, claimant-only even for administrators,
private confirmation/300-second expiry and restart invalidation, legacy-ID
refusal/fresh random-ID assumption, database trigger/CAS and lifecycle ABA,
atomic non-mirrored 45-day audit with indexed erasure cascade, and additive
schema/import/restore boundaries. A routing receipt is not host alignment;
no implementation, protected acceptance or build cards before both records.
Any material spec edit needs a new hash-bound request, not carried approval.

By 2026-10-31, measure **30/30 fixture outcomes explicitly accepted or revised,
CEO/host disposition recorded, and bounded migration/audit/compatibility seams
accepted or B deferred**. No demand/engagement success is claimed. At expiry,
default A (no build); unanswered intent never defaults to activation.

ACT: publish the proposed spec, offline check and exact-revision review request.
WHY: expose assignment/lifecycle ABA before a seemingly small unclaim button
creates a stale write. UNDO: reviewed docs revert; no operational state changes.
Future rollback disables release admission but preserves additive witness and
audit/erasure safety. No production test, migration or live release here.
