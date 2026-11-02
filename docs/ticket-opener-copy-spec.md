# Ticket transcript delivery to the opener

Status: **Declined: no build** (owner decision 2026-11-01). This doc stays as
the record; reopen only with a named host problem. Original proposal: written
2026-10-26. This indexed repository file is the editable contract; the task spec
records its exact commit and SHA-256. Merge publishes a proposal, not permission
to build, export content or enable DMs. No code, migrations, close-mechanics
changes, live reads, new service, spend or activation here.

## 1. Decision, alternatives and prior boundary

Hypothesis: an opener benefits from a private record after their support channel
closes. Demand and benefit have not been measured. Recommend reviewing **B**, a
limited, explicitly requested and staff-reviewed plain-text copy. **A/no build
remains in force** until exact intent and independent privacy acceptance. Never
send the raw saved transcript merely because someone opened the ticket.

| Option | Benefit / cost | Proposed disposition |
| --- | --- | --- |
| A. No build | Keep existing capture/purge and live conversation; no new external copies. Opener has no post-close transcript access | Use now; also the fallback if B's prerequisites are not justified |
| B. Opt-in, reviewed opener copy | DM a bounded, redacted projection; closed DMs fall back to authenticated private retrieval. Adds purpose, provenance, review, delivery and erasure obligations | Smallest candidate worth intent review; incomplete copies must be labeled |
| C. Automatic full transcript DM/file/link | Convenient archive, but creates multi-party exports and retention-evading links/files without consent or visibility proof | Reject |
| D. Reuse staff reader for opener | Less apparent work, but silently converts staff evidence into member export authority | Reject |

**Explicit prior-boundary resolution.** The separate staff-only transcript-read
proposal (revision 1, 2026-10-23) denied opener self-read (D1), export files (D2)
and list/search (D3); D4 allowed unredacted staff pages. It explicitly rejected
automatic opener DMs because these are multi-party personal data and Discord
copies escape local purge. That proposal is not proof of shipped reads or an
approved grant. Its text remains the contract for **staff evidence reads**.

B does **not** amend D1 to let an opener call that reader, apply D4 to members,
or relax D2/D3. It proposes a **separate, purpose-limited projection and consented
external delivery**, which needs a new exact-scope decision. No member access to
`ticket_transcripts.content`, staff receipt, arbitrary-ticket read, export file,
public URL, transcript search, dashboard reader or another member's copy. A staff
member who is also the opener uses opener rules for B; staff status cannot bypass
recipient or copy limits. Missing acceptance means no opener delivery.

## 2. Source baseline and missing prerequisites

Inspected repository: `c6e5213b516d5ef62b357cc2f5feeaf8f59889a8`. Source facts
are not measurements of deployed permissions or activation. See [ticket
runtime](tickets.md), [privacy](privacy.md), and adjacent [private-subject
proposal](ticket-subject-spec.md) and [claim-release proposal](ticket-claim-release-spec.md).

- Existing Close saves a bounded transcript before cleanup; the current flattened
  body is evidence, **not a member-safe projection**. It does not establish which
  text each participant could see throughout the ticket's life. Do not parse
  author lines, Markdown, names or URLs in that body to reconstruct authority.
- [Capture](../crates/bot/src/ticket_runtime.rs#L1369-L1430) takes the newest
  **10 × 100 messages**, plus one <=100-message boundary probe: empty proves
  exactly 1,000; nonempty marks depth truncation without storing/counting the
  probe. [Formatting](../crates/bot/src/ticket_history.rs#L39-L184) keeps the
  oldest-first prefix within that captured window, **200,000 UTF-16 units plus
  the shared truncation marker**. Counts include captured messages even when
  their text is cut. The body flattens timestamp, username/discriminator, text
  and attachment URLs, not structured author/message IDs. A complete capture
  does not prove historical visibility. Copy truncation is separate (§5).
- [Purge/erasure](../crates/cutover/src/tickets.rs#L338-L372) is guild-scoped:
  purge deletes at `purge_after <= now`, including cleanup-pending captures;
  member erasure deletes whole tickets by opener OR claimant, cascading bodies.
  The separate [erasure manifest](../crates/cutover/src/member_erasure_plan.json#L218-L226)
  explicitly deletes transcripts then tickets using that predicate. The source
  ceiling is **90 elapsed days from capture**, with physical purge scheduled
  hourly, not an immediate database TTL. Neither path covers a new recipient
  preference, reviewer, message-author index, copy, send journal or fallback.
  Other participants and people mentioned in prose are not matched by those
  opener/claimant predicates.
- Current ticket identity lacks immutable creation category/staff-role history.
  Current overwrites or current staff config cannot prove past visibility. A
  source-committed event or matching topic also cannot prove it.

**Prerequisite, not new code in this change:** B needs a typed, immutable captured
message projection with trusted author/category classification and positive
opener-visibility provenance for every included message. This must cover ACL,
role and category changes, missed events and manual Discord changes, not merely
save current permissions at close. Unknown history is ineligible; staff review
cannot manufacture missing provenance. Legacy flattened transcripts are
ineligible; no backfill or re-fetch to evade the boundary. If a bounded,
verifiable provenance design cannot be accepted, choose A rather than weaken it.

The storage/retention verification must also trace journal pre-images, backup,
restore, caches and transport retries. Existing privacy documentation does not
prove these new stores are erasable. The old staff-read proposal's unresolved
journal/backup question remains unresolved for B; no claim of end-to-end purge.

## 3. Conditional B: recipient, authority and UX

All controls below are **future acceptance requirements**, not available commands.

1. **Request while Open.** Only the authenticated opener of this same-guild,
   bound ticket may request a copy in a private interaction. No default opt-in,
   inherited guild preference, staff-on-behalf consent, target-user option or
   effect from knowing an ID. Require current guild membership and effective
   ViewChannel + ReadMessageHistory on the proven ticket; preserve application,
   configured-guild, feature and activation fences. Read current facts again on
   confirmation, with a five-second proof deadline; expired/unknown proof refuses.
2. **Notice before confirmation:** “Request a limited text copy after closure?
   Staff must review it; some content may be omitted. Discord DMs or private
   replies can be saved or forwarded. Our deletion cannot recall those copies.
   You can cancel before delivery starts. No attachments are included.” Offer
   **Request my copy** / **Cancel**. No queued request or consent from preview.
   Save consent policy version, time and recipient from authenticated context.
3. **Close stays independent.** A successful immutable capture commit can make
   one copy candidate eligible; failure, missing provenance or no consent does
   not produce a candidate. Never hold channel deletion, reopen a ticket, delay
   retention or fail an otherwise successful close for this optional service.
   Copy delivery is not evidence that channel cleanup succeeded.
4. **Separate private review.** Current configured staff role OR ManageChannels
   OR Administrator, fresh guild membership and application policy, may review
   only a proven same-guild candidate. Require **reviewer != opener/recipient**
   at preview, approval CAS and send admission; staff cannot approve their own
   request. A claimant alone is not staff authority.
   No new ability to browse raw saved transcripts is granted: the review surface
   contains only the eligible projection. Review may omit whole messages, never
   add/edit text or include previously excluded content. Preview and confirmation
   bind exact ticket/capture/copy revision, recipient, deadline, policy and bytes.
   Recheck current staff authority and unchanged candidate before atomic approval;
   any edit/invalidation makes the preview stale. No public review/receipt.
5. **One DM route.** Only the immutable opener ID from the locked ticket is the
   recipient, never a supplied DM/channel ID. Revalidate consent, current guild
   membership, erasure fence, expiry, policy and approved copy before admission
   and immediately before release to Discord. A deleted ticket or unknown fact
   refuses. No DM-body delivery based solely on a close event or past consent.
6. **Closed-DM fallback.** A definitive cannot-DM refusal leaves the same copy
   eligible for a proposed `/tickets copy case:<opaque-id> [page:<n>]` interaction
   in the configured guild. Every page proves authenticated actor == stored
   opener, current membership, unexpired consent/copy and no erasure, then returns
   an ephemeral page with mentions closed. The request confirmation privately
   gives the opaque case ID and this fallback instruction **before closure**;
   do not rely on the now-deleted channel or a failed DM to carry the instruction.
   No autocomplete, list, count, search, email, staff-channel attachment or public
   notification. Unknown/foreign/other-opener/erased/malformed IDs return the same
   bounded `NOT_AVAILABLE` shape with no IDs, state, names or existence hints.
7. **Truthful receipts.** Private status distinguishes requested, awaiting review,
   ready, DM sent, DM unavailable, delivery unconfirmed, expired and cancelled.
   A Discord acceptance receipt means sent, not read or safely retained. Only a
   proven opener may see positive status; generic refusal stays generic. No promise
   that staff will approve a copy or that a missing copy means an empty transcript.

At any pre-send point the opener may withdraw privately. Withdrawal erases the
local copy and prevents new sends/pages, but cannot recall pages/DMs already
released. A guild departure, policy disable, missing source or erasure invalidates
approval; rejoining never restores consent or an old copy. A new request after
cancellation/expiry/first send is not allowed for the same capture.

## 4. Redaction and disclosure policy

Assets: multi-party support text, staff identity, attachment access and recipient
identity. Boundaries: untrusted Discord history -> typed eligible projection ->
private staff approval -> opener DM/private reply -> recipient-controlled copy.
Spoofing/IDOR is controlled by authenticated recipient equality; tampering/replay
by exact revisions; disclosure by eligibility/omissions; overload by §6;
accountability by bounded private audit. Staff or recipient forwarding remains a
residual risk, not something a database TTL can prevent.

- Include only opener-authored messages and staff-authored messages with positive
  evidence that the opener could read them when posted **and** at capture.
  Visibility must be established independently of author names or current role.
  Exclude every other participant, bot, webhook/system message and uncertain
  author; do not assume every non-opener is staff. Only complete messages
  represented in retained source may qualify; a source-byte-cut fragment never
  becomes a complete message or a reason to fetch more history.
- Never include staff-only channels/threads, internal notes, modal answers,
  private subjects, moderation/audit entries, claim markers, embeds, attachments,
  filenames, stickers, quoted/replied/forwarded message expansions, jump links
  or channel/topic/member metadata. No fetching, proxying or re-uploading URLs.
  Omit a message containing a URL, Discord mention/role/channel token, snowflake
  identifier or unrecognized structured content instead of copying it verbatim.
  A deterministic conservative classifier needs independent adversarial tests;
  unsupported syntax refuses the message, not a best-effort raw fallback.
- **Staff-attribution decision:** headings are `Support`, never staff names,
  IDs, avatars, role names, discriminator tags or per-staffer pseudonyms; opener
  headings are `You`. Do not publish claimant/reviewer/closer identity. Keep only
  UTC message time and chronological order, not Discord message IDs.
- Replacing headings is **not anonymization**. Bodies may contain real names,
  allegations, phone numbers or sensitive facts in ordinary prose. Automated
  filtering cannot guarantee their absence. Staff review must omit entire
  messages containing third-party or inappropriate sensitive information; do not
  claim regexes detect all PII. If the remaining body cannot be approved as safe
  to disclose under the accepted purpose, reject the copy, not just its heading.
  Staff review itself is an additional authority requiring independent acceptance.
- Render inert plain text, escaping Markdown/control/bidi syntax; close all
  allowed_mentions (empty parse/users/roles; replied_user=false), suppress embeds
  and use no attachment. Refuse invalid encoding. All labels/limits apply after
  escaping. No source text in logs, errors, audit, metrics or review summaries.
  No LLM processing or third-party redaction service.

## 5. Source gaps, copy bounds and retention

Proposed bounds, **not current runtime constants**:

- One immutable approved copy per ticket/capture, up to **20 eligible messages**.
  Starting at the newest eligible message, select a consecutive suffix of the
  eligible sequence, then display chronologically. No mid-message cut. Source
  counts mean captured source, not all messages ever sent. Empty after filtering
  -> unavailable, never “complete empty transcript.” No history read to fill gaps.
- At most **3 pages**, each **1,500 UTF-16 units including heading, warning and
  footer** and <=6,000 UTF-8 bytes. Build pages backwards from newest, reserving
  all fixed-label overhead, then reverse pages/messages for chronological display.
  A whole escaped message that cannot fit a new page ends selection; do not skip
  it and continue with older messages. Stop likewise at the page/20-message cap.
  If the newest eligible message cannot fit, no copy is available. Fixed format
  version makes paging stable; filtering/review omissions are separately flagged.
- Every page says **“Limited opener copy; omissions possible; not the staff
  transcript.”** Carry distinct `source_truncated`, `copy_truncated` and
  `redactions_applied` flags. Depth/byte/message/unsupported-content omissions
  set the appropriate flags even if the original source marker was outside the
  selected tail. Do not infer flags by searching raw text for a marker. Fixed
  flag labels only: “Source capture limited”, “Copy size limited”, “Content
  omitted”; no omitted range, excluded-author identity, count or omission reason
  reaches the opener. The warning and flags never claim a complete conversation.
- Candidate creation, review, fallback and all sends expire at
  **min(captured_at + 24 elapsed hours, source purge_after)**, exclusive of that
  deadline (`now >= deadline` refuses). Opt-in cannot extend source's 90-day
  ceiling. Review/download/retry never resets either clock. A delayed reviewer
  gets expired, never a refreshed copy. Server-side logical expiry is immediate;
  physical copy/candidate deletion within the existing hourly privacy budget.

Source staff evidence and opener copy have separate purposes and separate TTLs.
Local purge deletes the copy, candidates, rendered caches and recipient/reviewer/
message-author linkage. Both operator and queued erasure paths must cover new
stores by **opener OR reviewer OR any included message author**, and deletion of
the source/ticket must invalidate its copy. Erasure of an author conservatively
invalidates the whole candidate/copy; do not attempt to repair it from flattened
text. This still cannot locate a person merely mentioned in prose; the accepted
purpose and review policy must address that limitation, not claim a full DSR export.

Minimal non-body delivery fences contain no actor/recipient/message IDs or content
hashes; expire at the same copy deadline and physically purge within one hour,
never later than parent deletion. Their ticket linkage is still personal data:
independent review must accept its exact lifecycle or B stays no-build. Expired
fence removal never permits recreation: every admission checks the original
capture deadline, including delayed/replayed events. Actor-bearing approval/
accountability audit follows the existing 45-day operational ceiling and actor/
opener erasure; no mirrored audit message or raw-text/hash history. Actor quotas
expire within one hour. Backups,
rollback pre-images and restores must obey source/copy deadlines and reapply
member erasure **before reads or delivery**. Restores never re-arm sent/uncertain
operations. Missing drain/fence evidence keeps B dark; no destructive restore here.

**External copies:** database purge/erasure does not delete Discord DMs, ephemeral
responses, Discord-managed caches, screenshots, downloads or forwarding. No promise
of recall or legal compliance is made here. Best-effort Discord deletion is not
part of B and must not be represented as guaranteed erasure. If this residual
retention escape is unacceptable, select A. Disabling B stops future disclosure;
it cannot undo a completed export.

## 6. Delivery state, replay, races and resource bounds

Proposed durable states are separate from ticket lifecycle:
`requested -> awaiting_review -> ready -> sending -> sent | dm_unavailable |
uncertain`; any unsent eligible state can become `cancelled | expired | erased`.
A declined/ineligible review becomes unavailable, not an empty success. No delivery
state transition modifies `tickets.status`, claim, close token or source expiry.

Unique admission key: `(guild, ticket, capture_revision, copy_format_version)`;
recipient is derived, not part of caller-selected key material. One candidate,
one approval and one DM batch maximum; repeated close events/recovery cannot
create another. Each page's admission atomically validates original deadline,
consent/approval/source/erasure state and fresh external proof, then inserts its
attempt fence on the same transaction; erasure/withdrawal use that serialization.
Only a confirmed commit permits I/O; no DB lock crosses Discord. A batch sends
sequentially, one POST per page, at most three. A valid 2xx message receipt must
have a canonical message ID and the proven opener DM channel before the next
page can proceed. No hidden transport retry. A store/admission uncertainty permits
**zero new sends**. Crash/timeout/5xx/429, generic 403/404, malformed or lost receipt,
or cancellation during I/O is uncertain, not retryable success. Only HTTP 403
with structured Discord error **50007 (cannot send messages to this user)** from
the exact attempted route is a definitive cannot-DM refusal; do not match free
text or treat 50013/missing-permissions as that proof. A definitive cannot-DM
before page 1 permits fallback only; after any released page, fallback starts
no earlier than the first unsent page. Uncertain page
requires explicit opener confirmation before viewing that same page privately,
warning that it may duplicate an external copy; it never authorizes another DM.
No retry endpoint, fresh operation ID or staff override can bypass the fence.

| Race / failure | Required resolution |
| --- | --- |
| Duplicate close/capture event | Same capture/key, at most one candidate; no historical replay delivery |
| Review vs candidate revision/policy/consent change | CAS fails; fresh review required, no silent upgrade |
| Manual close vs auto-close | Only an accepted committed capture event is eligible, never a timer/warning/lock event; no capture -> no copy. Auto-close may preserve the channel; delivery neither deletes it nor invokes manual cleanup |
| Capture/close failure or cleanup pending | No uncommitted body delivery; committed copy may proceed independently of cleanup, within expiry; close recovery never waits on it |
| Purge/erasure/withdrawal wins before admission | Atomic check invalidates candidate/approval and denies send/read |
| Erasure after final check during HTTP | Cannot be atomic with Discord. At most one page (<=1,500 UTF-16 units) may already be in flight, with a 10-second I/O deadline; re-admission denies later pages. Record only uncertainty/receipt, never resurrect the body; independent acceptance of this residual is mandatory |
| Roles/membership lost between check and send | Fresh checks before each page; observed loss stops batch. External change after final read is residual in-flight risk, not a permission grant |
| Commit then process crash; restart or restore | Attempt fence survives; no auto-resend, replay of source event or regenerated projection |
| Closed DMs / missing permissions | No alternative recipient or ACL repair; same-owner private fallback within original deadline |
| Review/send delayed beyond deadline | Expired even if physical purge has not run; do not extend or refetch |

Global limiter plus per-recipient/per-guild limiter uses durable shared admission,
not one process's counters: <=1 active batch per recipient; <=2 batches/guild/minute;
<=10 batches/global/minute; one in-flight HTTP send globally. Fallback <=1 page per
recipient/5 seconds, <=10 pages/guild/minute, <=30 pages/global/minute and <=6
successful fallback pages per copy lifetime. Repeated requests consume admission,
never create new copies or prolong expiry. Review and refusal traffic use existing
interaction budgets, no unbounded per-refusal rows. Queue <=25 metadata-only ready
jobs globally; full queue returns retry-after without a new job or body cache.
Workers inspect <=25 metadata rows/pass; no body scan. An existing application job
can own dispatch if B is accepted; no new host scripts, timers or services. Honor
Discord's own limits too; exhausted budget can expire a copy, never burst later.

## 7. Future acceptance matrix and agent contract

All rows are **planned tests, not runtime evidence**. Use synthetic identities,
unique channels, injected clock, independent disposable DB connections/barriers
and a recording Discord double. Collect outcome/next_action and read/write/send
counts, never private text. Test Author pins immutable test paths/hashes against
a named baseline; environmental/compile failure is not behavior-specific RED.

| Case | Required oracle |
| --- | --- |
| OC-01 | No opt-in/default/preview only -> zero candidate/body send; explicit opener request -> one consent, never staff-on-behalf |
| OC-02 | Wrong guild, actor, application, forged ID, missing/erased/other-opener row -> identical NOT_AVAILABLE response/status, no identifying metadata |
| OC-03 | Current staff-role/ManageChannels/Administrator review succeeds only with fresh proof; reviewer==opener, claimant alone or stale role/config denies; opener-staff gets only their own B copy |
| OC-04 | Staff-only message/thread, other participant, bot/webhook, subject/modal/audit/claim marker -> absent; unknown provenance/legacy flattened source -> no copy |
| OC-05 | Shared staff message -> Support heading, no identity metadata; bodies containing mentions/URLs/IDs omitted; unreviewed or sensitive prose cannot be approved automatically |
| OC-06 | Attachments/filenames/embeds/quotes/forwarding/links, invalid Unicode, Markdown/bidi/mention injection -> no raw fallback, fetch, attachment, embed or ping |
| OC-07 | Source depth/UTF-16 truncation independently set source flag; exactly complete source does not claim unbounded history; source flags survive tail selection |
| OC-08 | 20/21 messages, 1500/1501 UTF-16 units, astral Unicode/UTF-8 bytes, 3/4 pages and oversize single message -> exact post-escape bounds, whole-message omissions and truthful flags |
| OC-09 | HTTP403/structured50007 on first page -> zero body sent, same-opener private fallback; generic403/404, 50013, 429/5xx or malformed receipt -> uncertain, no DM retry; pre-close receipt includes case ID/instruction |
| OC-10 | Confirm page 1 then fail page 2 -> no retry of 1, no new DM batch; uncertain page explicit private duplicate warning/confirmation, remaining pages bounded |
| OC-11 | Duplicate events, two workers, changed keys, restart, admission/receipt failure, 429/5xx/timeout -> at most one attempt per page, durable uncertainty, no fabricated sent/read |
| OC-12 | Review vs changed bytes/recipient/consent/policy/expiry -> CAS refusal; all pages of a ready copy remain identical until invalidated |
| OC-13 | Erase opener/reviewer/included author before admission -> copy unavailable in both erasure paths, source/ticket policy preserved, unrelated guild untouched |
| OC-14 | Erase/withdraw/depart/purge between pages or after final check -> stop later pages, no resurrection; in-flight disclosure explicitly bounded and external copies not claimed recalled |
| OC-15 | now == min(capture+24h, source expiry) -> refusal; reads/review/retry do not change either deadline; physical deletion within one-hour budget |
| OC-16 | Journal, backup, restore/history rewind -> no expired/erased copy served or send re-armed; source body pre-image handling has explicit evidence or B stays dark |
| OC-17 | Manual close failure/recovery, committed cleanup-pending, warning-only auto-close or preserved-channel capture -> correct independent eligibility; no cleanup call, close delay or duplicate capture |
| OC-18 | Queue/limiter exact boundary and one over across workers -> bounded work/retry-after, no extra cache/job, recipient retargeting or later burst |
| OC-19 | Capture, review, send and fallback errors/logging/audit/metrics -> canary text/URLs/IDs absent where forbidden; audit non-mirrored and erasable |
| OC-20 | Dry-run/invalid fields/oversize request -> bounded JSON error, zero body, network, quota, consent, audit, SQL write or job side effects |

Proposed diagnostic envelope, not a registered API or authority to impersonate:
`schema=tickets.opener_copy.v1`, `mode=dry_run|execute`, `outcome`, `error_code`,
`retryable`, `next_action`, `copy_revision`, `source_truncated`, `copy_truncated`.
Only a proven participant gets ticket/revision/state/flags; generic refusals omit
them. Metadata-only diagnostic responses <=2,048 UTF-8 bytes; strict typed mode,
unknown fields/actions and oversized input refuse, never default to execution.
Private fallback pages use §5 instead, not an agent body-export endpoint.
`DELIVERY_UNCONFIRMED` is not retryable; next action is inspect or confirm private
fallback, not send again. Dry-run is local synthetic evaluation only and reserves
nothing. No new tool, executable command or live endpoint exists in this PR.

Offline document check, from repository root (no credentials/network/DB needed):

```sh
python3 -m doctest -v docs/ticket-opener-copy-spec.md
python3 scripts/check_docs_index.py --json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
```

The doctest checks only matrix discovery and deadline arithmetic, not any security,
delivery, rendering, retention or race guarantee:

```pycon
>>> import pathlib, re
>>> text = pathlib.Path("docs/ticket-opener-copy-spec.md").read_text()
>>> rows = re.findall(r"^\| (OC-\d{2}) ", text, re.M)
>>> assert rows == [f"OC-{n:02}" for n in range(1, 21)]
>>> capture, source_expiry = 1000, 1000 + 90 * 24 * 3600
>>> deadline = min(capture + 24 * 3600, source_expiry)
>>> assert deadline == 87400 and not (deadline < deadline)
>>> assert min(capture + 24 * 3600, capture + 60) == capture + 60
>>> assert 1500 * 3 == 4500 and len("🙂".encode("utf-16-le")) // 2 == 2
>>> print("20 planned cases; document arithmetic only; no delivery")
20 planned cases; document arithmetic only; no delivery

```

## 8. Review gates, bounded future slices and undo

**Disposition: DESIGN_ONLY, no new grant.** Authoring this proposal is not
independent CISO approval of our own boundary request. After 2026-11-02 intent
review, route an exact commit/file-hash decision through President/COO to the
CEO/appropriate independent security decision-maker; never directly to the owner.
Record actual host intent alignment separately. A routing acknowledgement,
merged docs or a quiet deadline is not acceptance. No implementation or protected
behavior-test cards before those records. A material revision invalidates earlier
acceptance. Keep A if intent, provenance, erasure or external-copy risk is rejected.

Decision packet must explicitly accept/change/reject: A vs B; staff-only D1-D4
remaining intact; consent/purpose and recipient scope; staff release authority;
heading redaction versus residual prose identity; closed-DM fallback; hard bounds;
24-hour local TTL and fence expiry versus irretrievable Discord copies; provenance,
erasure and restore evidence; at most one <=1,500-unit in-flight page with 10-second
I/O deadline after a racing revocation. No generic “transcripts approved.”

Only **after** those gates, cut each dependency-linked slice to <=1 working day;
if one cannot fit, split it again without dropping its controls. None created here:

| Slice | Output / exit; dependency |
| --- | --- |
| 0. Evidence/provenance decision | Read-only memo traces source capture, permission-history gaps, journal/backup/restore and both erasure paths; accepted realizable provenance or choose A; feeds a revised exact spec |
| 1. Protected oracles | Independent Test Author's focused fixtures/manifest and meaningful RED for OC-01..20 in bounded groups; accepted slice 0 and exact revised intent first |
| 2. Typed projection | Pure eligibility/render/page/flag helpers + deterministic adversarial tests; no reader/send; slice 1 |
| 3. Persistence contract | Additive reviewed consent/candidate/provenance/attempt model and atomic invalidation, TTL, both erasure paths; expand/compatibility work split into separate small PRs as needed; slice 2 |
| 4. Private request/review | Authenticated opt-in/withdrawal and exact-version candidate approval, no external body send; schema/erasure evidence first |
| 5. Dispatch/fallback | Bounded one-attempt DM pages and authenticated fallback, shared durable quota/fences; separate PRs for dispatch and fallback if needed; slices 3-4 |
| 6. Close-event/staging acceptance | Consume committed capture only, duplicate/recovery/auto-close tests, exact-head security and staging synthetic evidence; slices 2-5; preserve separately owned close mechanics |

All new behavior stays dark until schema/provenance/erasure, private UX, dispatch
and fallback are jointly verified; partially merged foundations grant no reads.
Ordinary CI, CodeRabbit, Devin Review and per-builder CISO gates still apply.
Staging fixtures only, never real production tickets. Future auto-close work owns
its lifecycle; no timer or warning becomes a transcript-delivery grant here.

Design metric for the 2026-11-02 review: all 20 oracles and each packet decision
have explicit accepted/change/rejected disposition, or B stays no-build. This is
not a measured engagement, delivery rate or production-performance result.
Unverified: deployed objects/activation, real user demand, current guild permissions,
provenance feasibility, end-to-end journal/backup erasure and every B runtime behavior.

ACT: publish this indexed Proposed specification. WHY: resolve the member-export
boundary without silently weakening staff evidence privacy. UNDO: reviewed docs
revert. Future disable stops new disclosure, keeps replay fences and honors TTL/
erasure; it cannot recall an already delivered copy. No live operation occurred.
