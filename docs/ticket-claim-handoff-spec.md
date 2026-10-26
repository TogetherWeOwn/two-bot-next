# Tickets: consented direct staff claim handoff

Status: **proposed, design only**. Written 2026-10-26; review by 2026-11-02.
No implementation, migration run, Discord mutation, new credential, spend or
production activation. Merge publishes a proposal, not an execution grant.
This file is the single repository contract; the task document mirrors it.
Intent review binds the exact file SHA-256 and commit, not a moving branch.

## 1. Decision and evidence

Consumer: a staff member holding one open ticket claim who wants to hand it to
a specific willing colleague. Hypothesis: a direct offer plus acceptance avoids
dropping ownership into a public race and avoids forcing responsibility onto
someone else. Demand, frequency and saved coordination time are unmeasured.

Comparison, not demand evidence: assignment with claimant replacement is an
established ticket-tool pattern (for example Ticket Tool's claiming options at
<https://docs.tickettool.xyz/dashboard/panel-configs/claiming-options.md>).
This design fetched no external sample and makes no claim about that product's
current behavior. TWO today has Open/Claim/Close with first-winner claim and
`claimed_by`, and no direct willing-recipient handoff.

| Option | Value / cost | Disposition |
| --- | --- | --- |
| A. No build; manual coordination, or self-release-and-reclaim if that separate design is accepted and built | No new writes or schema; handoff stays a chat agreement plus existing claim mechanics, with a public race window after release | **Recommend now**; standing fallback and default at review |
| B. Current-claimant offer with willing-recipient acceptance, generation-bound and expiring | Directed handoff with two-party consent and no race window; needs durable offer state, generation fencing and audit/erasure seams | Conditional model for intent review only, **not authorized**; see kill rule |
| C. Forced reassignment, admin override or queue auto-assign | Handles absent claimants but grants authority over another staffer's assignment | Reject from this slice; needs separate demand and security review |

Kill/defer rule: keep A and defer or kill B unless, by review, (1) the
separate self-release design's generation contract is accepted (B reuses that
concept only after its accepted contract is known), (2) real host demand shows
self-release plus queue does not solve the handoff, and (3) the durable
offer/audit/erasure seams below fit one bounded follow-up. B must never be
built by weakening those seams. This proposal does not duplicate or revise the
self-release design, and asserts nothing about its approval.

Out of scope: collaborator management, backup-host powers, queue
implementation, close confirmation, opener/role/channel permission changes,
category/name changes, transcript work, force assignment and any new staff
power. Adjacent designs are neither prerequisites nor build permission.

## 2. Verified baseline and seams

Inspected source head: `5db6ecccd7c67202ecea6d76f9ae65ceab2e3654`.
Repository facts, not deployed-state assertions:

- `crates/core/src/tickets.rs:8-35,73-84,106-125,187-203`: Open/Claim/Close;
  first-winner claim sets `claimed_by`; no claim generation exists. Staff means
  configured staff role **or** Manage Channels **or** Administrator, in the
  configured guild. Claimant identity alone never supplies staff authority.
- `crates/cutover/migrations/0210_tickets.sql:2-35`: TEXT ticket primary key,
  unique nullable channel, five states, TEXT lifecycle times; transcripts
  cascade on ticket deletion. The runtime reserves random 32-lowercase-hex IDs
  (`crates/bot/src/ticket_runtime.rs:1084-1087`); the schema also permits
  legacy IDs.
- `crates/cutover/src/tickets.rs:96-178`: `by_channel` is an ordinary read;
  transitions lock `(guild_id,id)` FOR UPDATE and persist assignment, channel
  and lifecycle together. A read followed by an unconditional UPDATE is not a
  cross-process CAS.
- `crates/bot/src/ticket_runtime.rs:951-1054`: authority comes from the invoking
  member/live config; successful ephemeral defer precedes execution. A new
  action needs shared-router recognition and admission, not a private
  dispatcher. Claims post a public marker; handoff must not copy it.
- `crates/core/src/audit_store.rs:175-212`: audit INSERT uses its own pool with
  `ON CONFLICT DO NOTHING`; it is **not** already atomic with ticket
  transitions. A transaction-aware insertion and explicit non-mirrored kind are
  required, not a best-effort call after commit.
- `crates/cutover/src/member_erasure_plan.json` covers tickets, transcripts and
  audit actor/target linkage; `crates/cutover/src/retention.rs:605` purges
  operational audit rows older than 45 days; transcripts keep their 90-day
  ceiling. Neither ceiling is claimed for proposed offer rows.
- `crates/cutover/src/rollback_import.rs` inserts whole JSON records (missing
  added fields become NULL); `crates/cutover/src/rollback_delta.rs:267-273`
  measures lifecycle timestamps only, so assignment/offer changes need explicit
  discovery coverage.
- [Claim release](ticket-claim-release-spec.md) is a separate **proposed**
  design, not an installed generation/audit facility. No claim-revision column,
  trigger or release audit kind exists at this head; B stays dark until that
  contract is accepted and its seams are installed and tested.

No live guild, database, claimant, migration ledger or host workflow was
inspected. [Ticket runtime](tickets.md) documents other lifecycle fences; this
proposal does not establish that older acceptance notes are current.

## 3. Private UX and two-party authority

Proposed entries, **not registered commands**, all in the one ticket channel,
with no ticket-ID selector, reason, force or admin-override option:

- `/tickets handoff offer @member`: the current claimant offers to one named
  colleague.
- `/tickets handoff accept` and `/tickets handoff refuse`: the named recipient
  accepts or refuses.
- `/tickets handoff cancel`: the offeror withdraws a pending offer.
- `/tickets handoff pending`: private read of offers where the actor is the
  offeror or the recipient. Staff who are neither learn nothing about them.

1. Preserve configured-guild, ticket feature/activation, application identity,
   existing component admission and timeout fences. A failed private defer
   performs no mutation. The actor comes only from the authenticated Discord
   interaction, never options, custom IDs or agent-supplied JSON. The recipient
   is the one explicit member option on offer; it grants no authority by itself.
2. Offer requires ticket `open`, a nonempty bound channel and **current**
   `claimed_by == actor`. Accept/refuse require actor equality with the offer's
   named recipient; cancel requires actor equality with the offeror. Pending
   shows only rows naming the actor. Re-evaluate the existing staff predicate
   from the current configured staff role and fresh invoking-member evidence at
   every preview and every confirmation, for **both** principals: the recipient
   must be staff when the offer is made and still staff when accepting.
   Either side losing staff status denies; administrators gain no offer, accept
   or force power beyond their own staff-authenticated actions. Unknown proof
   refuses.
3. Prove channel ID/guild/type and actor access through the shared executor;
   missing channel, 403 or unproven access returns unavailable, leaves rows
   intact and never invokes recovery/retirement. Authority/channel proof has a
   five-second monotonic deadline checked before transaction entry, after lock
   acquisition and before commit; an exhausted wait rolls back to a fresh
   preview. Never refresh via Discord I/O inside the transaction. Discord
   role/channel changes after the external read are a bounded residual race,
   not a new grant.
4. Offer preview names the recipient in non-pinging text and states the ticket
   stays with the offeror until the recipient accepts. Offer confirmation
   commits a pending offer row (§4) and changes **no** assignment. Accept
   preview states acceptance transfers the claim; accept confirmation commits
   the transfer only if the §4 predicates still hold. Until a confirmed accept,
   the current claimant remains fully responsible.
5. Discovery is pull-only: no DM, ping, public marker or queue announcement is
   ever sent about an offer. The recipient learns of it by running `pending`
   (or by out-of-band staff coordination the bot does not perform). An offer
   may therefore expire unnoticed; that silent-expiry tradeoff is explicit and
   must be accepted at intent review, not fixed later with a notification.
6. All replies are ephemeral, plain-text, bounded to 300 characters, with
   parsed mentions and replied-user pings off. Refusals to non-parties are
   generic and never name the offeror, recipient or revision.

An accepted handoff changes **only** `claimed_by` (offeror to recipient) plus
the witness metadata in §4. Opener, channel, lifecycle/times, overwrites,
roles, messages, content and transcripts remain byte-identical. Old public
claim markers are historical facts; this slice neither edits them nor posts
new ones.

## 4. Durable offer, generation binding and races

Two-party consent cannot reuse a single-actor stateless witness alone: the
recipient's later accept must verify that this offeror actually offered this
claim at this generation. B therefore needs a durable pending-offer record.
Proposed additive contract, **not DDL to execute**:

- One pending offer per ticket at a time. A second offer while one is pending
  is denied with `offer_pending`; cancel, refuse, expiry or accept must settle
  the first. Expiry settles lazily: an offer with `expires_at <= database time`
  is treated as expired everywhere -- it does not count as pending, cannot be
  accepted, refused or cancelled (those return stale), and a new offer marks it
  expired in the same transaction that commits the new pending row. No live
  `pending` row can therefore wedge the slot, including after silent expiry.
- Offer row (names illustrative): random 128-bit `offer_id` (32 lowercase
  hex), `guild_id`, `ticket_id` FK to tickets with cascade, `offered_by`
  (claimant at offer time), `offered_to` (named recipient), expected
  `claim_revision`, `offered_at` and `expires_at` (database UTC,
  `offered_at + 1800 seconds`), and monotonic state
  `pending -> accepted | refused | expired | cancelled`. Terminal states are
  immutable; expiry is evaluated lazily against database time at read
  (equality is expired), with no timer job claimed.
- Generation binding reuses the self-release revision concept **only after its
  accepted contract is known and installed**: the expected revision is the
  ticket's claim revision at offer commit, and every accept rechecks exact
  equality plus `claimed_by == offered_by`, `offered_to == actor` and
  `status == open`. Any intervening release, reclaim, close, reopen, recovery
  or channel rebinding advances the revision and renders the offer stale.
  Until that contract exists, B is dark: a missing/NULL/invalid witness
  refuses unavailable, never a default revision.
- Preview/confirm controls are boot-bound witnesses, not bearer capabilities:
  `two:th:1:<32-hex-offer-id>:<revision>:<expires-unix-seconds>:<boot-nonce>`,
  canonical unsigned decimal (no leading zeros except `0`), control expiry at
  database UTC +300 seconds, at most 12 digits, nonce as canonical unpadded
  base64url of 16 fresh random bytes per runtime start (22 characters, never
  restored). Maximum 97 ASCII characters, below Discord's 100-character limit.
  Controls bind the originating preview recipient; wrong recipient, foreign
  binding, restart or expiry refuses stale and asks for a fresh preview. Never
  log raw custom IDs, tokens or bodies.

Offer/accept transaction: take existing erasure-compatible table locks first
if needed by the atomic audit seam, then lock the ticket row. Recheck open
state, channel binding, actor equality, fresh staff proof for the acting
principal, expected revision, the single-pending invariant and unexpired offer
against database time. Conditional predicates repeat these checks; accept SETs
**only** `claimed_by = offered_to` (the trigger advances the revision) and
marks the offer accepted; insert §5 audit on the **same
connection/transaction**, then commit from RETURNING witness. Only a confirmed
commit returns saved. Zero affected rows is stale on the accept path (or
denied with `offer_pending` on the offer path when a live pending row won the
race), never a retry with newer state. No Discord I/O or advisory-lock wait is
held inside the transaction.

Race outcomes (revision 7 at offer time):

- Close first: close commits first; accept finds the lifecycle advanced and
  refuses stale with closing unchanged. A pending offer never blocks an
  authorized close.
- Accept first: accept commits recipient@8; a separately authorized close may
  follow its unchanged capture-before-delete contract.
- Release/reclaim: an offeror self-release commits before accept, so the offer
  is stale even if the offeror reclaims (same-actor ABA: `A@7 -> offer ->
  release NULL@8 -> claim A@9`; the held offer for 7 refuses). The recipient
  never auto-receives a released claim; they may use the normal first-winner
  claim path afterwards.
- Other-actor ABA: release then C claims `C@9`; the delayed accept for `A@7`
  refuses denied-or-stale with `C@9` unchanged.
- Duplicate accept: two deliveries serialize on the row lock; at most one
  commits; the loser is stale, not a second audit.
- Restart: the durable offer row and ticket revision reload; old controls fail
  the fresh boot nonce, so both sides take a fresh preview against the
  surviving pending row. No in-memory delivery dedup is required for safety.
  A post-commit crash keeps state; duplicate delivery is stale, with no second
  audit.
- Lost access: the recipient losing staff status or channel access before the
  final check denies; the offeror losing staff status also denies accept
  (both principals must be staff-fresh), so the offeror should self-release
  instead. A denied accept leaves the pending row for expiry/cancel; a denied
  read never auto-mutates it.
- Cancel: the offeror cancels a pending offer before accept commits;
  assignment unchanged; a late accept is stale. Cancel after a terminal state
  is a no-op; cancel of a lazily expired offer returns stale, since expiry
  already settled it.
- Unclaimed ticket: offer is denied because there is nothing to hand off; the
  colleague uses the normal claim path.

Bounded follow-up admission: allocate unused migration numbers per
[migrations](migrations.md), expand/backfill/validate separately, never edit
migration 0210 or fixtures; fail unavailable on missing schema, trigger or
audit seam; keep B dark until migration, trigger, atomic audit, both erasure
paths, import/export and exact-head caller tests are accepted; rolling back
stops issuance without dropping additive state; rollback delta covers
offer/assignment writes; import/restore keeps B dark until old workers drain
with fresh boot nonces, re-applies erasure first and never revives terminal
offers. If this cannot fit a bounded reviewed follow-up, choose A.

## 5. Truthful outcomes, audit and privacy

| outcome / reason / next_action | Private response and semantics |
| --- | --- |
| `pending / offered / await_recipient` | "Offer recorded. You remain the claimant until it is accepted." Offer commit only |
| `saved / accepted / none` | "You now hold this ticket's claim." Confirmed accept commit only; the offeror learns of it via `pending`, never a push |
| `no_op / refused / none` | "Offer refused. The claim is unchanged." Recipient side; offer terminal |
| `no_op / cancelled / none` | "Offer cancelled. The claim is unchanged." Offeror side; late accept is stale |
| `stale / offer_changed / refresh_private_preview` | "This handoff is out of date. No change was made. Refresh before continuing." Covers expiry, revision advance, duplicate and terminal state; never names the other party's new state |
| `denied / not_participant_or_staff / ask_authorized_staff` | "Only the current staff claimant can offer, and only the named staff recipient can accept." No assignment details |
| `denied / offer_pending / cancel_or_await_settlement` | "An offer is already pending for this ticket. Cancel it or wait for it to settle." First offer intact; nothing terminal |
| `unavailable / context_unavailable / verify_access` | "Handoff is unavailable here. No change was saved." Collapses absent/foreign/unproven context and the dark-generation gate; reveals no IDs |
| `unavailable / store_unavailable / retry_private_preview` | "Handoff could not be saved. Refresh and try again." Proven pre-commit failure/rollback only |
| `unavailable / commit_unknown / inspect_current_assignment` | "The handoff result is unconfirmed. Check current assignment before trying again." Never asserts rollback or success |

A failed reply edit after a confirmed commit does not undo or repeat it.
Restart invalidates controls and preserves rows. A lost commit acknowledgement
stays unknown even if a later read shows the recipient: another action could
have caused that state. Unknown outcomes never enable fallback SQL.

Atomic saved audit: one `ticket_claim_handoff` operational audit event per
`(guild,ticket,revision_after)`, fixed action `handoff_accept`, occurred_at,
actor_id = offeror (prior claimant), target_id = recipient (new claimant),
explicit ticket_id FK derived from the locked parent, and metadata containing
only `revision_before`, `revision_after`, `outcome=saved`, `reason=accepted`
and `offer_id` (<=512 UTF-8 bytes). Entry ID is
`th1:<guild>:<ticket>:<revision_after>` (<=77 ASCII characters). No raw
interaction ID/token, custom ID, body, names, channel IDs, transcript, free
text or other claimant history. The typed decoder supports the kind;
`mirror_channel_id=NULL`, `delivery_state=none` permanently. Audit failure
**or zero inserted rows** rolls back the transfer; baseline
`ON CONFLICT DO NOTHING -> false` is not success. Never commit after an
entry-ID collision. Offers, refusals, cancels and expiries need only bounded
reason/outcome counters, no actor IDs as metric labels and no per-attempt audit
rows.

Offer rows and handoff audit are personal data for staff accountability only.
Operational audit uses the existing 45-day purge; both erasure executors must
delete handoff audit by actor OR target (offeror/recipient) and offers by
either principal, plus opener linkage via the parent. The ticket FK cascades
offer and handoff-audit rows on parent deletion/guild purge. Numeric revision
holds no actor and is never reset by erasure. Terminal-offer retention needs
the same 45-day operational integration proven before build; pending offers
cannot outlive `expires_at` plus read-time grace, but crash-before-expiry rows
still need the purge path, not a new timer. Restore re-applies erasure before
reads; no backup or Discord-side destruction is claimed. Transcript
erasure/retention is unchanged; no transcript is created or rewritten.

## 6. Agent handoff and deterministic acceptance oracle

No new executable CLI, MCP or HTTP write surface is registered. Agent handoff
is a diagnostic/result contract, **not** authority to impersonate Discord
users. Discover this file through the [documentation map](README.md). Proposed
JSON:

```json
{"schema":"tickets.handoff.v1","mode":"dry_run","guild_id":"g1","ticket_id":"0123456789abcdef0123456789abcdef","offer_id":"abcdef0123456789abcdef0123456789","claim_id":{"ticket_id":"0123456789abcdef0123456789abcdef","revision":"7"},"outcome":"preview","reason":"confirm_required","retryable":false,"next_action":"confirm_same_offer"}
```

All revisions are decimal strings (avoid JSON/JavaScript precision loss).
Identity fields are null/omitted before authorization or for
foreign/unavailable context. `mode=dry_run` is a typed read-only path: same
checks, no INSERT/UPDATE, audit, revision increment, reservation, send or job
beyond the private preview; it predicts eligibility at read time, never
reserves it. Commit-unknown has `retryable=false`: inspect, never blindly
reapply. Handoff also includes exact source commit/spec hash and the check
below; examples use synthetic IDs only. No activation follows a passing
document check.

Fixture base: configured guild g1, channel c1, open ticket T, claimant A,
revision 7; B is another staffer and the named recipient, N a nonstaff
member. Each row resets it. Every row asserts unchanged
opener/channel/lifecycle/content/roles/permissions/transcript except fixture
effects explicitly injected. No new public messages, DMs or mentions in any
row.

| Case | Deterministic order / required assertion |
| --- | --- |
| offer_accept | A offers T@7 to B -> pending; B accepts -> saved, B@8, exactly one audit; A responsible until commit |
| nonclaimant_offer | B (not claimant) offers -> denied, A@7 unchanged, no offer row |
| nonstaff_offer | N offers, or A after staff removal offers -> denied, unchanged |
| nonstaff_accept | B loses staff before accept -> denied, A@7, pending row left for expiry/cancel |
| foreign_guild | g2/DM/application mismatch -> unavailable, no IDs or writes |
| wrong_binding | Foreign channel/message/private recipient, or C accepts B's offer -> unavailable/denied, unchanged |
| pending_visibility | With A->B pending: A and B `pending` reads show that row privately; third staffer C reads -> no rows, learns nothing; no writes anywhere |
| unclaimed_offer | Offer on NULL@8 -> denied, no row/control/audit; B uses the normal claim path |
| refuse | B refuses -> no_op, A@7, offer terminal refused, no audit row |
| cancel | A cancels pending -> no_op, unchanged, terminal cancelled |
| cancel_after_accept | Accept commits B@8; late cancel -> stale no-op, B@8 unchanged |
| expiry | Offer expires at exact DB deadline; late accept -> stale, A@7 unchanged, slot freed for re-offer |
| duplicate_accept | Two connections read pending; barrier; submit both -> one saved, one stale, one audit |
| concurrent_offer | Pending offer to B; A offers to C -> denied/offer_pending, first offer intact; cancel, settle or expiry then re-offer allowed |
| close_first | Barrier; close commits closing@8 before accept -> stale, closing unchanged, offer unusable |
| accept_first | Accept commits B@8 before authorized close -> saved then closing@9, transcript path unchanged |
| same_actor_aba | Hold accept for A@7; release commits NULL@8, A reclaims @9; deliver held accept -> stale, A@9 |
| other_actor_aba | Release then C claims @9; delayed B accept for A@7 -> denied-or-stale, C@9 unchanged |
| offeror_lost_staff | A loses staff after offering; B accepts -> denied, A@7 unchanged; A should self-release instead |
| recipient_lost_access | B keeps staff role but loses channel access -> denied/unavailable, unchanged |
| restart_before | Drop process after offer; reload with fresh boot nonce; old controls -> stale; fresh previews may proceed |
| restart_after | Crash after accept commit before reply; duplicate/old-nonce accept -> stale, B@8, no second audit |
| dry_run | Eligible offer/accept -> preview, no state/audit change, no reservation |
| store_failure | Inject before UPDATE or force audit INSERT failure -> rollback, A@7, offer still pending |
| commit_unknown | Disconnect at COMMIT acknowledgement -> unavailable/commit_unknown; inspect, never claim saved |
| reply_failure | Confirm commit then fail private edit -> B@8 and one audit, no compensation/retry |
| proof_timeout | Row lock held past proof deadline -> rollback/unavailable, no Discord I/O while locked |
| erasure | Erase A or B through each path: offer rows and handoff audit gone; parent erasure cascades; g2 untouched |
| retention_expiry | 45-day audit purge and terminal-offer purge leave witness/ticket intact; stale offers never valid again |
| dark_without_generation | Missing/NULL witness/trigger or unaccepted generation contract -> unavailable/dark, no default revision |
| overflow | Revision at i64 MAX refuses mutation; never wraps or resets |

Offline consistency recipe (Python stdlib, repo root, no network/DB/Discord):

```sh
python3 -m doctest -v docs/ticket-claim-handoff-spec.md
python3 scripts/check_docs_index.py --json
```

The doctest checks the oracle's arithmetic, discovery and handoff shape only;
it does **not** prove SQL serialization, trigger installation, auth, atomic
audit, erasure or rendered replies. Future tests need actual callers, separate
disposable DB connections, barriers rather than sleeps, injected clock/commit/
reply failures and the shared Discord double. No live tests here.

```pycon
>>> import json, pathlib
>>> text = pathlib.Path("docs/ticket-claim-handoff-spec.md").read_text()
>>> rows = text.split("| Case |", 1)[1].split("\n\n", 1)[0].splitlines()[2:]
>>> cases = [row.split("|")[1].strip() for row in rows]
>>> assert len(cases) == len(set(cases)) == 31
>>> assert {"offer_accept", "refuse", "expiry", "close_first", "same_actor_aba", "duplicate_accept", "restart_after", "erasure"} <= set(cases)
>>> # Abstract witness arithmetic, not a runtime or database implementation.
>>> offered, accepted = 7, 7 + 1
>>> assert offered == 7 and accepted == 8 and offered != accepted
>>> assert not (1800 < 1800) and not (300 < 300)  # equality is expired
>>> token = "two:th:1:" + "b" * 32 + ":" + str(2**63 - 1) + ":253402300799:" + "B" * 22
>>> assert len(token) == 97 and len(token) <= 100
>>> sample = json.loads(text.split("```json\n", 1)[1].split("\n```", 1)[0])
>>> assert sample["schema"] == "tickets.handoff.v1" and sample["retryable"] is False
>>> assert sample["claim_id"]["revision"] == "7" and sample["reason"] == "confirm_required"
>>> assert not {"actor_id", "content", "token"} & sample.keys()
>>> result = {"schema": "tickets.handoff.design-check.v1", "cases": len(cases), "outcome": "checked", "reason": "offline_oracle_only", "next_action": "request_exact_spec_intent_alignment"}
>>> print(json.dumps(result, sort_keys=True))
{"cases": 31, "next_action": "request_exact_spec_intent_alignment", "outcome": "checked", "reason": "offline_oracle_only", "schema": "tickets.handoff.design-check.v1"}

```

## 7. Boundary disposition, intent gate and undo

**CISO disposition: DESIGN_ONLY; no runtime grant.** Narrow circulation of B
is acceptable; an actor-equality-only toggle, forced transfer, silent
generation upgrade, notification-based discovery, new permission effect or
migrationless activation is refused. This is an assessment of the proposal,
**not independent approval of our own request**. An independent boundary
disposition remains required for future security-sensitive implementation,
alongside exact-head CI and independent Paperclip Review 5/5.

Request CEO acceptance and actual staff-host intent alignment on this exact
spec through the President/COO, not directly from the owner. The decision must
accept/revise: A fallback versus B, two-party consent with the claimant
responsible until accept, pull-only discovery with silent expiry, the
1800-second offer and 300-second control expiries, the generation dependency
with its dark gate, the durable single-pending offer contract, close/release/
reclaim ABA handling, atomic non-mirrored 45-day audit with indexed erasure
cascade, and additive schema/import/restore boundaries. A routing receipt is
not host alignment; no implementation, protected acceptance or build cards
before both records. Any material spec edit needs a new hash-bound request,
not carried approval.

By 2026-11-02, measure **31/31 fixture outcomes explicitly accepted or
revised, CEO/host disposition recorded, and incremental value over
self-release plus queue proven or B deferred/killed**. No demand/engagement
success is claimed. At expiry, default A (no build); unanswered intent never
defaults to activation. No build card is created by this design run.

ACT: publish the proposed spec, offline check and exact-revision review
request. WHY: expose two-party consent and assignment ABA before a seemingly
small handoff button creates a forced or stale transfer. UNDO: reviewed docs
revert; no operational state changes. Future rollback disables handoff
admission but preserves additive witness and audit/erasure safety. No
production test, migration or live handoff here.
