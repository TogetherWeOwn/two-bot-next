# Giveaways: voluntary withdrawal without refunding entry budgets

Status: **Deferred to 2026-12** (owner decision 2026-11-01). Revisit then, or
earlier if a host names a need and an M2 slot opens. Original proposal: written
2026-10-25. No runtime/store/schema implementation, migration run, live member
test, credential, spend or activation. Merge publishes a proposal, not accepted
intent. This file is the single repo contract; the task spec mirrors it. Intent
review binds its exact commit and SHA-256, not a moving branch.

## 1. Choice and evidence

Consumer: a member who entered accidentally or no longer wants to participate.
Benefit is a hypothesis, not measured demand or a claim about a competitor.

| Choice | Trade-off | Recommendation |
| --- | --- | --- |
| A. No build; keep Enter only | No new writes; cannot voluntarily leave. Never offer manual SQL or legal erasure as the leave workflow | Current behavior and safe default |
| B. Explicit self-only Leave, private confirmation | Removes one current admission; re-entry is a new, charged admission. Needs durable debits and generation fencing | Smallest useful candidate for intent review, conditional on one bounded safe follow-up PR |
| C. Enter/leave toggle | A delayed or repeated click changes meaning; still needs B's accounting and generations | Reject; no smaller safe implementation |

If B's migration, old-writer fence, privacy and rollback seams cannot fit **one
later PR**, or benefit is insufficient, choose A. Do not strip safeguards to fit.
No admin removal, participant enumeration, winner exclusion/reroll rule, prize
system, DM, notification, new service or new agent write endpoint.

Verified source baseline: `ee5d314408d3c1e06fa5dc0f73669bd2f1bbb7f8`.
These are repo facts, **not deployed-state assertions**:

- `crates/discord/src/giveaway.rs:527-605,608-622,1218-1241`: Enter only, private
  feedback, aggregate public count. An actor-local fixed 60-second attempt budget
  is consumed even on duplicate/refused entry calls; it is not durable.
- `crates/core/src/giveaway_store.rs:781-854`: parent row lock, then guild/member
  advisory lock; max 10 surviving entry rows with `entered_at > now - 1 minute`.
  Duplicate entry retains its original timestamp. Capacity counts current rows.
- `crates/core/src/giveaway_store.rs:307-396,478-525,873-886`: draw claims persist
  a token while status may remain open; entrant reads happen after claiming.
  Cancel can clear a claim; stale draw completion must fail. Status alone is
  therefore not a draw fence. Current draws/rerolls read all surviving entries.
- `crates/core/src/giveaway_store.rs:899-1005`: paused entry is closed; resume
  shifts the deadline. This proposal does not rewrite pause/extend behavior.
- `crates/core/src/giveaway_store.rs:254-302` and
  `crates/cutover/src/member_erasure_plan.json:34-46`: hard deletion and creator
  erasure can delete other members' entry rows too. An entry tombstone alone
  cannot keep a debit independent of these deletes; old draw code would also
  include that tombstone. Neither naive deletion nor a new withdrawn flag is B.
- `crates/cutover/src/privacy.rs:40-44` limits analytics opt-out purges to other
  tables. Opt-out is not giveaway withdrawal. No giveaway-specific age TTL was
  established in the inspected retention code; do not borrow audit's 45 days.

No live guild, database, migration ledger, entrant, permissions or activation
state was inspected. Adjacent create-preview and self-eligibility proposals do
not authorize this feature or block this design.

## 2. Authority and private UX

Proposed discoverable entry: **Leave giveaway**, beside Enter on the existing
bound bot post. It opens a private preview, never immediately toggles membership.
Do not broaden the staff-only `/giveaway` command to make a member leave path.
An older Enter-only post is not silently rewritten by this docs change; a future
build must define its reviewed reconciliation path or leave it unsupported.

- Preserve configured guild, automation/feature/activation, application, router,
  interaction admission and private-reply gates. Actor is the authenticated
  non-bot Discord member; no supplied user ID, target option, force or staff
  override. Staff cannot remove someone else's entry, even as Administrator.
- Prove current membership and current access to the exact giveaway channel,
  plus same-guild channel/message binding and authentic bot authorship. Reject
  DMs, foreign guilds, copied/forged buttons and hidden/deleted/unproven channels
  generically, before disclosing entry existence. No message-history fetch or
  participant list is needed. Unknown is not an empty entry list.
- Re-evaluate authority and local feature revision at preview and confirmation.
  Current role/age entry requirements need **not** still be satisfied to leave;
  losing channel access or leaving the guild does deny this route. Departed
  members use the existing privacy process, not impersonation or staff removal.
- Fresh external authority proof has a five-second monotonic deadline, checked
  after lock waits and before commit; expired/invalidated proof rolls back.
  No Discord I/O while holding DB locks. External membership/access can change
  after observation; bounded evidence freshness is not atomic Discord/DB auth.
- Preview: “Leave this giveaway? You can enter again only if it is still open
  and you qualify. Leaving does not refund entry limits.” Buttons: **Leave this
  entry**, **Keep entry**. No current entry means no confirmation control.
- Confirmation binds the exact random admission generation, actor, guild,
  giveaway, channel and source post to a boot-local opaque 128-bit handle.
  Use a versioned strict component ID under 100 ASCII characters; the handle
  is not authority. Keep at most 1,024 previews, evict expired entries, expire
  after 300 monotonic seconds, and refuse at equality. Saturation returns
  unavailable, never evicts an unexpired preview to permit a write. Restart,
  missing handle, malformed ID, wrong recipient or stale generation requires
  a fresh preview. Bind the private message's recipient/authorship as well.
  No new durable preview store, secret, raw-body log or cross-runtime guarantee.
- All replies are ephemeral plain text, at most 300 characters; parsed mentions
  and replied-user pings are disabled. No prize/member-controlled text is echoed.
  Failed private defer means zero mutation. Keep entry/cancel writes nothing.

## 3. State, atomicity and idempotency

Withdrawal is admitted only for **open**, `ends_at > database clock`, published
non-pending message, and **no claim token**, even an expired one. Paused, drawing,
drawn and cancelled states refuse; deadline equality is closed. Paused members
must wait for a genuine resume and fresh preview. No deadline extension, lease
clear/reclaim, automatic resume or winner removal is a withdrawal side effect.

On confirmation, one transaction locks `(guild,giveaway)` FOR UPDATE, rechecks
open state **and NULL claim token**, message binding, fresh authority and preview
expiry, then addresses only
`(guild,giveaway,actor,expected_generation)`. Delete at most that active entry;
leave all admission debits untouched. Check current DB time **after** waiting
for the lock, not transaction-start or caller-supplied time, and again before
commit. Claim UPDATE takes this same parent row lock; leave holds it until
commit and repeats NULL-token/state predicates on DELETE. A current entry of
another generation is stale, never substituted. Only confirmed commit returns
saved. No process-local mutex is a DB fence.

- Leave first: active row is gone before draw can claim; subsequent input omits
  it. Claim first: leave refuses even while status still says open. Once claimed,
  entry rows are immutable to this operation through draw completion/recovery.
- Cancel first: leave refuses. Leave first: normal cancel can follow. No draw
  row, seed, nonce, proof, winner, historical input or existing announcement is
  rewritten. Privacy erasure remains its separate authorized exception.
- Double delivery: at most one delete. If no current row remains in an otherwise
  authorized open giveaway, return no_op; this does not prove who removed it.
  If the member re-entered, the old generation refuses without touching the new
  entry. Lost acknowledgement is unknown, not fabricated idempotent success.
- Re-entry is explicit Enter, with fresh eligibility/capacity checks, a new random
  generation and **one new debit**. Duplicate Enter consumes no durable debit
  and never refreshes timestamps. Concurrent Enter/Leave obey parent-lock order:
  duplicate-before-leave leaves absent; leave-before-Enter may create a new row.
  Delayed leave of the old generation cannot delete that new admission.

## 4. Durable accounting, privacy and cutover requirements

Proposed additive data contract, **not DDL to execute**:

1. Active entry gains an immutable unpredictable 128-bit `admission_id`.
   Existing entries get unique IDs without changing weight or `entered_at`.
   No generation reuse/reset; collision is a failed transaction, never upsert.
2. A separate admission-debit table holds only `(admission_id,guild_id,user_id,
   admitted_at)`, indexed for guild/member/time. These are personal data for
   anti-spam only. **No giveaway FK, creator ID, prize, channel or payload.**
   Entry deletion, cancellation, parent deletion and creator erasure cannot
   cascade/refund another member's debit. The debit is not a draw roster.
   Do not FK active entries to expiring debits: active entries outlive 60 seconds.
3. Every successful new admission inserts the active row **and** debit in the
   same transaction. Preserve lock order: parent first, existing guild/member
   advisory lock second. Under that lock count debits with `admitted_at >
   database_now - 60 seconds`; refuse at 10. Future-dated debits remain counted
   conservatively, never ignored to create headroom. No-op, failed transaction,
   full/ineligible entry, preview and leave create no debit. Use trusted DB time;
   migration preserves historical original precision, never stamps backfill now.
4. Preserve the existing actor attempt budget and guild/member durable bound,
   current giveaway `max_entries` and all guild/feature creation bounds. Active
   capacity may be freed; the member's admission allowance is not refunded.
   Leave has a separate bounded 10-attempt/60-second actor throttle, capped at
   4,096 actor keys; it neither consumes nor resets Enter's throttle. Expire
   keys at 60 seconds from their fixed interval's first attempt before checking
   capacity; never evict an unexpired key to admit another actor. Saturation
   refuses with rate_limited, an explicit bounded availability trade-off.
   Existing shared admission/transport protections still apply.
5. Expired debits cease counting exactly at 60 seconds; integrate bounded indexed
   deletion into existing retention, with a target of removal within the next
   existing retention cycle and an alert on overdue backlog. No indefinite abuse
   ledger or new cron/service. Never purge a debit still in its accounting window;
   clock anomalies fail conservatively. Before build, name the existing job,
   cadence, batch bound and measurable overdue threshold in the accepted plan;
   if no bounded existing executor is suitable, choose A.
6. Both operator member erasure and queued deletion erase the subject's debits
   by `(guild,user)`; guild purge includes all debits. Creator erasure removes
   that creator's debits, **not other members'**. Legal erasure takes precedence
   over abuse history: removal of the erased subject's debits is an intentional
   exception to no-refund, including when the subject is also the creator.
   Re-entry afterwards still requires all current privacy/admission gates; an
   erased debit cannot be reconstructed or used to deny it. Do not retain a
   hash/tombstone to evade erasure. Preserve suppression, opt-out and restore-erasure fences. Analytics opt-out remains
   separate, not implicit leave or indefinite collection permission. Update both
   erasure contracts, schema coverage, dry-run counts and retention tests.
   Preserve erasure's sorted table-lock order before row/member-budget locks
   (`crates/cutover/src/member_erasure.rs:269-288`); never introduce a lock inversion.

Migration/cutover must cover both migration chains, all admission writers
(including `upsert_entry`), typed decoders and old clients. Expand/backfill/validate
per existing migration policy; no old SQL file edits or permission broadening.
Make missing generations/schema/ledger capability unavailable, never fallback to
surviving-row counting. Old admission writers must be drained/fenced before
withdrawal is enabled; a process-local feature flag alone proves no such fence.
Backfill surviving entries from their original timestamps, idempotently.
Already-deleted history is unknowable: keep admission paused for a full 60-second
window after the last possible old writer, using verified drain and non-rewinding
clock evidence, before accepting the ledger as complete. Do not extend giveaways
to compensate. This is a future cutover prerequisite, not a grant to pause live
admission or migrate production.

Use only a compatible rollback artifact that preserves debit writes and generation
fencing; disable new leave admission first. Physical deletion keeps withdrawn
rows out of old draw readers, but **does not** make old admission writers safe.
No resurrection from an older entry snapshot. Add the ledger to backup/version
coverage (`crates/core/src/backup/dump_file.rs:211-214` currently lists only the
three existing giveaway tables). Rollback-delta/export must represent entry
deletions, debit rows and generations, not just surviving `entered_at` rows.
Restore drains old runtimes/preview handles, reapplies erasure, reconciles active
entries and debits and re-establishes the accounting window before admission.
Unknown compatibility evidence means no build/activation, not a fallback SQL fix.

## 5. Bounded outcomes and agent contract

| outcome / error / next_action | Private copy and meaning |
| --- | --- |
| `preview / confirm_required / confirm_same_admission` | Preview above; no reserved right to leave |
| `saved / none / none` | “You left the giveaway. Entry limits are unchanged.” Confirmed commit only |
| `no_op / no_entry / none` | “You have no current entry in this open giveaway. Nothing changed.” Authorized context only |
| `no_op / cancelled / none` | “Cancelled. No change was requested.” Does not assert the entry still exists |
| `closed / already_closed / none` | “This giveaway is paused, closed or being drawn. No entry was removed.” Proven no-commit only |
| `stale / admission_changed / refresh_private_preview` | “This control is out of date. Refresh before continuing.” Includes restart/expiry; never acts on a new entry |
| `unavailable / context_unavailable / verify_access` | “Leaving is unavailable here. No change was saved.” Unknown/foreign/hidden context, no disclosed IDs |
| `unavailable / rate_limited / retry_after_window` | “Too many leave attempts. Try again in a minute.” No admission debit or mutation |
| `unavailable / store_unavailable / refresh_private_preview` | “Leaving could not be saved. Refresh and try again.” Known rollback only |
| `unknown / commit_unknown / inspect_own_current_entry` | “The result is unconfirmed. Check your current entry before trying again.” Never asserts rollback/success |

After confirmed commit, refresh only the existing post's aggregate count using
current committed state and existing coalescing/transport fences. Do not publish
actor identity or a leave event. Failed/missing post refresh does not undo/retry
the delete: append “The count display is not confirmed updated.” A later count
is not proof of individual membership, draw eligibility or winning odds. Do not
promise a refresh job exists unless it is actually scheduled. Failed private
reply delivery similarly does not undo a commit. Logs/counters use bounded codes,
not user/giveaway IDs as metric labels; no new persistent per-attempt audit store.

No CLI/MCP/HTTP mutation is introduced. Agent-facing result/handoff, not authority
to impersonate members (IDs are strings; synthetic example):

```json
{"schema":"giveaway.withdraw.v1","mode":"dry_run","guild_id":"g1","giveaway_id":"G","admission_id":"a1","outcome":"preview","error":"confirm_required","retryable":false,"next_action":"confirm_same_admission","count_refresh":"not_attempted"}
```

Identity fields are null/omitted until context authorization; `commit_unknown`
has `retryable=false`, inspect rather than blindly retry. Typed dry-run runs the
same read-only checks without DELETE/INSERT, debit, throttle mutation, preview
reservation, count refresh, audit, job, public send or DM. It predicts a read-time
outcome, never holds locks across calls or authorizes a real action. This dry-run
contract is offline/authorized diagnostic handoff only, **not** an unthrottled
member-facing probe endpoint; unauthorized context always collapses to unavailable.
Unknown flags/modes refuse before any connection/write; no production executable recipe.
Handoff includes exact source commit, spec hash and future protected-test manifest.
Discover this sole source through the [documentation map](README.md).

## 6. Deterministic fixture matrix and offline design oracle

Each case resets synthetic guild g1, visible bound post P, actor A, open giveaway
G ending at t=120, current admission a1 at t=1. g2/B are negative controls. All
refusals assert zero DB/public/DM effects; all leave paths preserve debits, draw
history, requirements and other members. Use injected clocks and explicit
barriers/separate disposable DB connections, never sleeps, in **future** tests.

| Case | Ordered actions / expected assertion |
| --- | --- |
| absent | Authorized leave without active row -> no_op, no debit |
| double_leave | Two a1 confirmations -> one saved, other no_op; one delete |
| reentry | Leave a1; Enter creates a2 and second debit; delayed a1 -> stale, a2 intact |
| budget_exhausted | Ten admissions/leave cycles across open giveaways at t=1..10; eleventh at t=11 refused despite zero active rows |
| boundary | Debit at t=1 counts at t=60, expires at t=61; never migration-time refreshed |
| cross_runtime_budget | Two connections race at nine debits -> at most one tenth admission |
| duplicate_enter | Repeated Enter while a1 active -> same generation/time and one debit |
| enter_then_leave | Existing a1; duplicate Enter commits then leave -> absent |
| leave_then_enter | Leave commits then fresh Enter -> a2; old confirmation cannot remove a2 |
| claim_first | Explicit/timed claim token commits before leave -> closed; input unchanged |
| leave_first | Leave commits before claim -> draw excludes A, historic proof untouched |
| expired_claim | Any token, even expired while status open -> closed, no lease repair |
| cancel_race | Cancel-first refuses leave; leave-first then cancel preserves debits |
| terminal | Drawn/cancelled/drawing and stale winning-entry control -> closed, no winner/proof rewrite |
| deadline | Lock held until t=120 -> closed at equality, no stale pre-lock clock |
| paused | Pause -> closed; genuine resume plus fresh preview can leave if deadline future |
| foreign_hidden | DM/g2/copied post/hidden channel/unknown permission -> unavailable, no entry-existence leak |
| departed | A left guild -> unavailable; B/Admin cannot supply A as target |
| lost_entry_role | A still member with channel access but lost qualifying role -> may leave, must requalify to re-enter |
| preview_stale | Expired at 300s, restart, wrong recipient/handle or missing generation -> stale/unavailable; 1,024 live previews/4,096 unexpired throttle keys refuse new allocation without eviction |
| dry_run | Same reads, zero writes/throttle/preview reservation/sends |
| failed_commit_reply | Precommit fault rolls back; ambiguous COMMIT -> unknown; postcommit reply/count failure never repeats delete |
| creator_erasure | Delete creator/G/active rows; unrelated A's unexpired debit still counts |
| subject_erasure | Erase A (also creator variant) through each supported path -> A's debit/entry gone; B/g2 debits unaffected; re-entry requires current gates but cannot charge erased history; no retained hash |
| retention | Expired debit purge leaves active rows/generations/draw proof intact; unexpired/future-dated debits retained |
| cutover_restore | Original-time backfill, old writer drain/window, deletion export and compatible restore proved; otherwise unavailable |

Run now, repo root; stdlib only, no network/DB/Discord:

```sh
python3 -m doctest -v docs/giveaway-withdrawal-spec.md
python3 scripts/check_docs_index.py --json
```

This checks **design arithmetic and matrix discovery**, not implementation tests
or runtime security. No protected acceptance tests are authored before accepted
intent. Later the Test Author supplies immutable public-safe branch/SHA and JSON
path/hash manifest, behavior-specific RED on the named baseline, and unchanged
positive/negative controls. Compilation/cache/env failures are not RED.

```pycon
>>> import json, pathlib
>>> text = pathlib.Path("docs/giveaway-withdrawal-spec.md").read_text()
>>> rows = text.split("| Case |", 1)[1].split("\n\n", 1)[0].splitlines()[2:]
>>> cases = [row.split("|")[1].strip() for row in rows]
>>> assert len(cases) == len(set(cases)) == 26
>>> assert {"reentry", "budget_exhausted", "claim_first", "leave_first", "creator_erasure", "subject_erasure", "cutover_restore"} <= set(cases)
>>> debits = list(range(1, 11))  # Independent of active-entry rows.
>>> active = {}  # Ten successful admissions followed by ten leaves.
>>> assert sum(t > 11 - 60 for t in debits) == 10 and not active
>>> assert sum(t > 60 - 60 for t in [1]) == 1
>>> assert sum(t > 61 - 60 for t in [1]) == 0
>>> current, held = "a2", "a1"  # New admission never matches old confirmation.
>>> assert current != held and not (300 < 300)
>>> result = {"schema": "giveaway.withdraw.design-check.v1", "cases": len(cases), "outcome": "checked", "error": None, "next_action": "request_exact_spec_intent_alignment"}
>>> assert json.loads(json.dumps(result)) == result
>>> print(json.dumps(result, sort_keys=True))
{"cases": 26, "error": null, "next_action": "request_exact_spec_intent_alignment", "outcome": "checked", "schema": "giveaway.withdraw.design-check.v1"}

```

## 7. Boundary recommendation, acceptance and undo

**CISO recommendation: DESIGN_ONLY; no runtime grant.** Prefer explicit B over
C only if the independent debit/generation and cutover/erasure seams remain
bounded; otherwise A. Refuse toggle ambiguity, refunded budgets, staff override,
claim-token clearing and historical draw edits. This is an author's boundary
recommendation, **not independent approval of their own security request**.
Future implementation still needs an independent security disposition plus
same-head required CI, CodeRabbit and Devin Review, and a squash merge.

Request CEO acceptance and actual giveaway-host intent alignment **through the
President/COO**, against this exact spec commit/hash and saved document revision,
before any implementation or acceptance-test cards. A routing receipt, merged
proposal or approval of adjacent work is not accepted host intent. They must
accept/revise B versus A, explicit confirmation and charged re-entry, paused/
deadline/claim refusal, no staff override, bounded personal-data retention and
erasure precedence, and the exact cutover/rollback contract. Material edits
require a new revision-bound request. No production activation follows acceptance.

Review target 2026-11-01: measure 26/26 fixture outcomes accepted or revised,
CEO/host dispositions recorded, and one bounded implementation scope or no-build.
An unanswered request defaults after 72 hours to **A, no build**, never activation;
record that default on the next event/backstop wake. Demand/engagement impact
remains unverified. No build/acceptance cards are created by this design run.

ACT: proposed spec, offline consistency check, exact-revision intent request.
WHY: withdrawal must not refund anti-spam allowance or mutate a claimed draw.
UNDO: reviewed docs revert only; there are no operational changes to undo.
Future operational rollback stops leave admission and preserves committed deletes,
debits and generations; it never restores withdrawn entrants or resets budgets.
