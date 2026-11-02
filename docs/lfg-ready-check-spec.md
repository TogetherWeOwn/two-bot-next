# LFG: one opt-in pre-game ready check

Status: **Deferred to 2026-12** (owner decision 2026-11-01). Revisit then, or
earlier if a host names a need and an M2 slot opens. Original proposal: written
2026-10-24. No commands, storage, migrations, permissions, sends or runtime
tests are added. Merge publishes a proposal, not permission to implement or
activate it. This file is the single repository contract; review requests bind
its commit and SHA-256.

## 1. Decision and evidence

Consumer: a game-night host and members already using an existing LFG post.
Hypothesis: a short, explicit “ready now” answer distinguishes signup intent from
current readiness without repeated pings. No customer validation is claimed.
[sesh](https://sesh.fyi/) describes simple RSVP and optional reminders;
[Apollo reminders](https://docs.apollo.fyi/reminders) describe pre-event targeting,
thread mentions and personal controls (including default opt-in with opt-out).
Read 2026-10-24. Neither source is evidence of this ready-check feature or TWO
member demand; they establish a notification/coordination trade-off only.

| Option | Value and cost | Disposition |
| --- | --- | --- |
| No build; current signup | No new personal data or controls; role signup does not answer “ready now” | Safe default until acceptance; retain if hosts need nothing more |
| Host manually opens one bounded check on the existing post | Explicit Ready / Not yet / Withdraw; quiet controls, private counts; small short-lived state and lifecycle integration required | Smallest option that tests this distinct hypothesis; recommend for **spec review**, not build |
| Reminder-only UX | Reminds about an event but collects no explicit readiness; introduces notification preferences/delivery without answering the question | Do not add here; existing reminders stay independent |

No attendance/check-in, availability poll, waitlist, standby, reminders, voice
presence, XP, role reassignment, punishment, recruitment or new spend. Silence is
**unknown**, never “not ready”, absent or a reason to remove a signup. No automatic
member mention, DM, new channel message or nonresponder roster. No future event
inherits this consent. Readiness never changes an RSVP/signup fact.

## 2. Verified baseline and missing seams

Inspected repository head: `85670bf881b39aad068a66b349095f16eb1bc0c6`.

- `crates/core/src/lfg.rs:314-345` has post ID, guild/channel/message binding,
  start/status/creator and role signups, **not readiness**. `lfg_content:437-459`
  currently renders signup identities; the proposed readiness surface adds no
  identity to that roster and does not repurpose it as a response roster.
- `crates/core/src/router.rs:665-674` requires Manage Events for LFG management;
  `lfg.rs:559` provides the domain permission check. Signup/leave are ordinary
  member actions, not elevated host authority. Preserve configured-guild,
  feature, privacy and application gates, and [wiring requirements](command-wiring-security.md).
- `crates/discord/src/lfg_interactions.rs:249-341` persists/reuses the post and
  saves its accepted message ID. This is an existing-post binding seam, unlike
  assuming an event link proves a message. Unknown acceptance/message ID means
  **unbound**: no check, message search bypass or replacement announcement.
- `crates/core/src/lfg_store.rs:292-365,380-401` serialize signup/close per post;
  `leave_lfg:369-375` deletes separately. `put_lfg:54-138` can replace post/roles.
  The [interaction contract](lfg-interaction-contract.md) explicitly does not
  provide an atomic mutation/audit inbox. These are integration gaps, not proof
  a new check can already share an atomic transaction.
- `crates/cutover/src/member_erasure_plan.json:19-32` covers signups and creator
  posts. [Privacy](privacy.md) covers both operator erasure and queued deletion,
  schema coverage, retained replay controls, guild purge and backup limitations.
  New state would need explicit coverage in both deletion executors, not just a FK.

No live post, permission, deployment, external shared-database writer or host
workflow was inspected. No sibling LFG edit/list/standby feature is a dependency
or an assumed capability. If required writer/erasure integration cannot be small
and coherent, stop this proposal instead of weakening existing fences.

## 3. Identity, authority and consent

Post identity is `(guild_id, lfg_post_id)` plus the stored application/bot author,
channel and **existing accepted message ID**. Names, titles, start times and
caller-supplied message links are not identity. An authorized existing bot-owned
message must be readable and editable; verify its author/application and channel
before attaching controls. Missing/unknown evidence refuses without creating it.

“Host” means a current human guild member with fresh **Manage Events** and access
to that bound channel/post. Creator identity alone grants nothing; no new host
role, owner exception, delegated capability or permission grant is proposed.
Host start, cancel and private summary each recheck that boundary. A host cannot
set another member's answer. Unknown membership/access/permissions fails closed;
API 403/timeout is not proof a member departed. Do not hold SQL locks over HTTP.

Start is an explicit two-step host action in the existing LFG management flow:
private preview, then confirm against the same post revision. Proposed controls
are not callable commands today. Preview reports exact post/check identity,
deadline, quiet existing-message edit, private-count disclosure, limits and
refusals; it writes nothing and reserves no check. Confirm revalidates everything.
Only an open, bound post with start in `(now, now + 15 minutes]`, at least one
current signup and at most 100 current signups is eligible. No-start/over-limit
refusal does not consume the one check. All times use authoritative DB UTC time.

One check per post lifetime is the deliberate stricter reading of “one per
post/generation”. Generation is fixed at `1` for this accepted binding; an edit,
reschedule, host change, close/reopen, restart, replay or flag toggle cannot mint
generation 2. A genuinely separate game needs a separately created post with a
new post ID through the ordinary flow, not an automatic replacement. Post IDs
must not be reused after deletion. Start allocates an opaque stable `check_id`
and atomically consumes the unique `(guild_id, post_id, generation)` slot.
Concurrent/double starts return the same check, never extend its lifetime.

Members with current human membership, effective channel access and a current
signup on **this post** may respond; a member signing up during the window can
explicitly opt in too. No eligible-member snapshot or nonresponder list is stored.
A signup alone creates no response. Ready and Not yet both disclose a short-lived
answer to the private count summary; neither is inferred from activity or voice.

| Member action | Own private acknowledgement; durable result |
| --- | --- |
| Ready | “Ready saved for this check until <absolute deadline + UTC>. Not attendance; signup unchanged.”; own value `ready` |
| Not yet | “Not yet saved for this check until <deadline>. Signup unchanged.”; own value `not_yet` |
| Change between them | Replace own value atomically, not another vote or history row |
| Withdraw | “No readiness answer is kept for this check. Signup unchanged.”; delete own answer, keep only the short-lived replay cursor |
| My status | Current own answer or `none`, deadline and check state; never another member's answer |
| No action | No row, no penalty, no conclusion |

Withdraw needs authenticated same-guild self identity and a valid persisted
component binding, **not** continued signup/channel visibility. It deletes only
that actor's data, discloses no post/count information and never inserts an answer.
Use the existing private LFG management/reply path with known post/check IDs as
the fallback if controls are inaccessible. This is a proposed withdraw-only seam,
not an existing command or permission to bypass authentication. Missing own data
gets identical generic acknowledgement. Positive choices/status still require all
eligibility gates. Admission flag-off preserves withdraw, cleanup and host cancel.

The post gets only a quiet edit with labelled Ready / Not yet / Withdraw / My
status controls and “Optional ready check until <absolute time>. Silence is
unknown; signup unchanged. Hosts see counts; small groups may allow inference.”
No color/emoji-only state. Responses cause **no post
edit or notification**. Fresh host-requested ephemeral summary is `Ready: N ·
Not yet: M · as of <time> · ends <deadline>`. No denominator, response timestamps,
identity list, per-role split, nonresponse count, export roster or count history.
Closed/expired summaries say “Check ended; readiness answers cleared”, not zero
attendance. Exact counts in a small group can still allow inference; this is not
an anonymity promise. Explain that limitation before each member's first choice
in the visible check text/help, without an extra unsolicited message.

## 4. Lifecycle, serialization and replay

Proposed states: `unused -> open -> ended`. Terminal reasons are only
`host_closed | post_closed | post_edited | post_deleted | expired | privacy_erasure |
feature_disabled`. End is irreversible for this post, and clears all response,
cursor and opening-actor fields; no final count snapshot is retained.

- Deadline is `min(opened_at + 5 minutes, starts_at)`, never extended. Every
  positive write/read/summary compares DB time; equality at deadline is expired.
  A cleanup timer is not the admission boundary. Host cancel closes only this
  readiness check; ordinary LFG close also ends it, without falsifying signups.
- Any title, role-spec, start-time, channel or message-binding edit while open
  ends the check atomically with that edit. Earlier **or later** time edits do
  not carry consent forward or reopen the slot. Cosmetic readiness-control edits
  themselves are not LFG edits. No implementation of `/lfg-edit` is implied.
- Signup leave/role move removes that member's answer under the same transaction,
  retaining its action cursor until check end. Rejoin/move never restores an
  answer. Other members stay unchanged; a fresh eligible click may opt in again.
  Observed definitive membership/access loss removes only that answer. Unknown
  evidence cannot become “Not yet”; summaries must label freshness limitations.
- Start, response, leave/move, post edit/close/delete, summary and erasure must
  serialize against the same check state. Required lock order: existing sorted
  erasure-compatible table locks, guild readiness quota lock, existing post
  domain lock, check row, member cursor. No HTTP inside that transaction.
  All relevant writers, including direct stores and both erasure executors,
  need transaction-aware seams; current independent store commits are insufficient.
- Preflight external identity/access proof must be fresh (<=5 seconds), discarded
  on reconnect/restart or observed relevant invalidation. Commit rechecks post
  revision/binding, policy generation, signup and deadline. Unknown proof refuses.
  Remote permissions cannot be atomic with SQL: an unobserved change after proof
  is a residual limitation, not evidence of attendance or durable eligibility.

Persist one bounded check row: native post key, opaque check ID, generation,
post revision, open/end times, deadline, enum state/reason, revision and nullable
`opened_by` (PII while open). Unique response/cursor row per `(check_id, member_id)`:
nullable choice, latest authenticated interaction ID, canonical action fingerprint
and constant outcome. No free text, role copy, interaction token, message body,
eligible roster or per-answer history. Count accepted answers joined to current
signups at the serialized snapshot, bounded by 100; no whole-guild scan or
per-summary member REST census. Unknown **host** eligibility or store state
returns unavailable, not invented counts. Each choice needs fresh member proof;
a summary certifies saved choices/current signup at that snapshot, not continuous
monitoring of Discord access since each choice. Say so in the summary help.

Authenticate and authorize before replay lookup/disclosure. Components are
`two:lfg-ready:v1:<opaque-check-id>:1:<action>` (<=100 characters), with a closed
action set. Parse strictly; look up the persisted guild/post/generation and match
actual application, bot author, channel and message from authenticated context.
A component ID is **not a capability or secret**. Never accept a target member
from it; the actor comes solely from the authenticated Discord interaction.
Forged, foreign, unknown or stale bindings get generic unavailable, no IDs/counts.
Runtime and direct/internal callers need the same checks; no alternate HTTP/MCP
write route or automation actor may submit member consent.

Interaction timestamps must be at/after open and no later than DB now + 30 seconds;
DB now must still precede deadline. Unhealthy clock evidence refuses new writes.
Canonical fingerprint binds actor/application/guild/post/channel/message/check/
generation/action. Monotone authenticated interaction snowflakes order **each
member's** choices: identical ID/fingerprint returns current own state without a
write; identical ID/different fingerprint refuses; older ID refuses; newer ID
advances the cursor even for same-choice/empty-withdraw no-ops. Thus a late Ready
cannot resurrect a later Withdraw. Cursor and choice commit together, not via the
existing independently appended announcements audit. Preserve any existing required
audit boundary; if it needs new readiness subject fields, include them atomically
and in the same short retention/erasure contract, never an indefinite audit copy.

Restart reloads saved state, expires overdue checks before admitting work and
never restarts their clock. A callback never creates a missing check. A lost
commit acknowledgement is `COMMIT_UNKNOWN`: no false success, no blind reapply;
inspect My status under the same binding/cursor first. Replay after terminal
closure returns ended even if an earlier attempt succeeded. Close-versus-response
has two legal serial orders: response first may acknowledge saved then close
clears it; close first rejects response. No success after a committed close.

Only opening/ending alters the existing bot-owned message. Use the shared paced,
mention-suppressed executor (`allowed_mentions.parse=[]`), no POST/send fallback.
An uncertain opening edit keeps the consumed slot; do not open another check.
`SAVED_REFRESH_PENDING` describes saved state with a failed edit, not rollback.
Serialize/coalesce with **all** existing LFG renderers so a signup refresh cannot
silently erase live controls or restore stale ones. Retry only the newest desired
render at most 3 times (5s/15s/30s, respecting longer Retry-After); never retry an
open render after deadline. Terminal controls can remain visually stale after
outage/late edit, but every click still rejects from authoritative state. No
claim of visual convergence or erasure of Discord copies is made after retries.

## 5. Bounds, retention and privacy/security findings

Proposed bounds tighten, never replace, existing LFG/admission limits: 10 open
checks/guild; 100 distinct cursor rows/check including withdrawn actors; 1,000
response/cursor rows/guild; one in-flight execution/process using the existing LFG
queue/pool bounds. At cap, refuse **new** rows/checks, not withdraw/close/expiry or
erasure. Rate: at most one accepted choice/member/check/5 seconds; duplicate
replay, withdraw and closure exempt. Host summary at most once/5 seconds/check.
Quota reservations are atomic. Sweep at most 10 overdue checks/pass on a
one-minute healthy cadence; cleanup precedes new admission after restart. No
new service, perpetual member timer or unbounded receipt history. Incremental
spend target $0 using existing process/database; this is not measured headroom.

| Data / threat | Proposed control and remaining finding |
| --- | --- |
| Explicit choice + member/cursor/opening actor (PII) | Purpose is this <=5-minute check only; delete on terminal transaction; healthy overdue cleanup target <=1 minute, not a guaranteed physical TTL through outage. Reads refuse expired data regardless. No analytics/XP/community fact, metrics label, trace, payload log, member digest or export to another service |
| Withdraw/reordered delivery | Remove answer immediately, retain cursor only until end; no historical response audit copy. Member sees own `none`, not a disguised Not yet |
| Erasure/replay | Before deleting subject rows, **end every open readiness check in the same guild** within the existing guarded erasure transaction, then clear all subject rows/opening actors. This bounded collateral close blocks even a never-before-seen delayed click without retaining an erased-member tombstone. Other guilds are untouched; no readiness check reopens |
| Creator erasure/post/guild purge | Include check children explicitly in dry-run counts and manifest coverage before deleting parent; same-guild blanket close applies to operator `erase_member` and queued `complete_request`. Preserve unrelated privacy/replay exceptions; do not reset existing moderation fences |
| Retained spent marker | Keep identity-free check/post/generation/state/reason/time marker with the existing parent post lifetime; no counts/actor/cursor. It is still linkable to the parent, not “anonymous data”. Parent erasure/guild purge removes it. Missing/deleted identities never allow callback recreation |
| Backup/restore/caches | Inventory new rows for member export/erasure and existing backup access policy. Old snapshots may contain choices; before serving restored data, terminalize all restored checks and delete their subject fields. Do not claim backup destruction or Discord-side ephemeral/message erasure; no response data is projected publicly |
| Spoofing/privilege escalation | Fresh human/Manage Events for host, current signed-up self for choices, full persisted component binding; preview is no grant. Creator alone and automation actors refused |
| Race/repudiation | One serial state/cursor transaction; direct store + edit/leave/render/deletion writers must participate. Existing audit-only dedup is insufficient; no runtime approval until that gap is proved closed |
| Count inference/harassment | Host-only on-demand counts, no nonresponse denominator/list/history, first-choice disclosure. Small-group inference remains possible; host intent review must accept this, not call it anonymous |
| Abuse/outage | Caps, existing lane admission, no new-message/DM/mention path. Database failure means unavailable/unknown, not empty answers, absence or failed signup |

**CISO proposal assessment:** acceptable to circulate for intent review with the
above controls; **not** a grant, independent approval of our own boundary request,
or runtime security acceptance. Blocking findings before any later build/enable:
(1) prove atomic writer/render/erasure compatibility, (2) accept blanket same-guild
erasure closure and spent-marker lifetime, (3) confirm quiet private-count UX and
small-group inference are the intended host behavior. If those need persistent
member history, new messages or broad authority, stop this proposal. No credential
or permission is requested by this document. Later security-sensitive changes
still require independent applicable security disposition and exact-head tests.

## 6. Agent-facing result and offline fixture contract

No new executable CLI/API is registered. Future adapters must discover this file
through the [documentation map](README.md); these are **proposed typed results**,
not commands to run on a guild. Stable envelope: `schema=lfg.ready.v1`, `post_id`,
`check_id` (null only before allocation), `generation=1`, `revision`, `reason`,
`retryable`, `next_action`; own-choice or host-count fields only after the relevant
authorization. Denied replies omit identity/state/count fields. No raw SQL/Discord
errors or labels. `retry_after_seconds` is bounded 1..300 when retryable; honor
longer upstream Retry-After without a busy retry loop.

| reason | retryable / next_action |
| --- | --- |
| `PREVIEW` | false / `confirm_same_revision` |
| `OPEN`, `SAVED`, `UNCHANGED`, `WITHDRAWN` | false / `view_own_status` (host OPEN may `view_host_summary`) |
| `ENDED`, `EXPIRED`, `ALREADY_USED` | false / `use_existing_signup` |
| `EMPTY`, `OUTSIDE_WINDOW`, `LIMIT` | false / `keep_current_signup` |
| `UNAVAILABLE` | false / `verify_access` (collapse missing/foreign/denied) |
| `STALE`, `CONFLICT` | false / `refresh_controls` |
| `BUSY`, `STORE_UNAVAILABLE` | true / `retry_after_backoff` |
| `COMMIT_UNKNOWN` | true / `inspect_own_status` |
| `SAVED_REFRESH_PENDING` | true, render only / `inspect_saved_state` |

Preview is typed read-only with no mutation handle; confirmation is a distinct
host-authorized operation, rechecking revision/identity rather than `--execute`
on a preview. This design's offline check has no environment selector, credentials,
network, database, Discord client, side effects or wrong-flag production path.
No staging/live pilot or mutation is authorized. Future execution cannot be
reached by combining flags on this check.

The following deterministic **fixture oracle**, not an implementation, uses fixed
post `p1`, check `c1`, guild `g1`, bound message `m1`, generation 1, members `a,b`,
open time 0 and deadline 300 seconds (start=600). Each row resets that fixture;
`0` means no readiness writes/side effects on a refusal, never zero attendance.
All rows require unchanged signup/role/XP and zero new messages/mentions/DMs.

| Case | Controlled event / expected assertion |
| --- | --- |
| zero | No signups at preview/confirm -> EMPTY, slot unused; no controls edit |
| duplicate | Ready(a,id=10), exact replay -> one ready, one cursor, same revision |
| same/change/withdraw | Ready(10), Not yet(11), Withdraw(12), delayed Ready(10) -> no answer, cursor=12, STALE; fresh Ready(13) may opt in |
| conflict | Same id=10 with altered action -> CONFLICT, saved answer unchanged |
| foreign guild/message/member | g2, m2, foreign application/author, or unsignup b -> UNAVAILABLE, 0; shared ID cannot select a target actor |
| stale | Wrong generation/check/post revision or before-open callback -> STALE/UNAVAILABLE, 0; no callback creates state |
| concurrent close | Barrier response before close -> SAVED then cleared; barrier close before response -> ENDED, 0; repeat close idempotent |
| time edit | Start moved earlier or later at t=100 -> ended/post_edited, answers cleared, slot spent; original component and new start refused |
| timeout | t=300 exactly -> EXPIRED, no write; lost SQL commit acknowledgement -> COMMIT_UNKNOWN, inspect cursor/state; message-edit timeout -> consumed slot, SAVED_REFRESH_PENDING, no second check/send |
| store outage | Failure before transaction -> STORE_UNAVAILABLE, no saved reply/count; summary unavailable rather than zero; restart uses same deadline |
| erasure | Erase a at t=100 -> all open g1 checks ended/cleared, never-seen delayed a/b click cannot reopen; g2 byte-identical; creator parent cascade counted; dry-run unchanged |
| leave/move/rejoin | Remove a's answer with signup change, cursor retained; rejoin has no answer; stale pre-withdraw choice cannot restore it |
| limits/flag-off/restart | cap and cap+1 serialize; withdraw/close work at cap and admission-off; restart expires overdue before choices; no new generation |

Runnable offline document check (Python stdlib; from repo root):

```sh
python3 -m doctest -v docs/lfg-ready-check-spec.md
python3 scripts/check_docs_index.py --json
```

The doctest checks the **specified oracle's consistency**, not real SQL locks,
authentication, rendering, erasure, outage handling or deployed behavior. Future
protected acceptance tests must implement the barriers/injected clock/store/REST
faults above against actual callers in disposable fixtures; zero tests is not proof.
No acceptance/implementation task is created by this proposal.

```pycon
>>> import json
>>> # (case, reason, ready, not_yet, cursors, terminal); abstract expected snapshots
>>> oracle = [
...     ("zero", "EMPTY", 0, 0, 0, False),
...     ("duplicate", "UNCHANGED", 1, 0, 1, False),
...     ("withdraw_stale", "STALE", 0, 0, 1, False),
...     ("conflict", "CONFLICT", 1, 0, 1, False),
...     ("foreign_guild", "UNAVAILABLE", 0, 0, 0, False),
...     ("foreign_message", "UNAVAILABLE", 0, 0, 0, False),
...     ("foreign_member", "UNAVAILABLE", 0, 0, 0, False),
...     ("stale_generation", "STALE", 0, 0, 0, False),
...     ("close_first", "ENDED", 0, 0, 0, True),
...     ("response_then_close", "ENDED", 0, 0, 0, True),
...     ("time_edit", "ENDED", 0, 0, 0, True),
...     ("deadline_equal", "EXPIRED", 0, 0, 0, True),
...     ("store_outage", "STORE_UNAVAILABLE", 0, 0, 0, False),
...     ("erasure", "ENDED", 0, 0, 0, True),
...     ("leave", "UNCHANGED", 0, 0, 1, False),
... ]
>>> assert len({row[0] for row in oracle}) == len(oracle) == 15
>>> assert all(0 <= r + n <= c <= 100 for _, _, r, n, c, _ in oracle)
>>> assert all((r, n, c) == (0, 0, 0) for _, _, r, n, c, end in oracle if end)
>>> assert all(row[1:5] == ("UNAVAILABLE", 0, 0, 0) for row in oracle if row[0].startswith("foreign_"))
>>> assert dict((name, c) for name, _, _, _, c, _ in oracle)["withdraw_stale"] == 1
>>> opened_at, starts_at, now = 0, 600, 300
>>> deadline = min(opened_at + 300, starts_at)
>>> assert deadline == 300 and not now < deadline
>>> assert min(opened_at + 300, 120) == 120  # earlier start cannot extend consent
>>> print(json.dumps({"schema": "lfg.ready.oracle.v1", "fixtures": len(oracle), "status": "pass"}, sort_keys=True))
{"fixtures": 15, "schema": "lfg.ready.oracle.v1", "status": "pass"}

```

## 7. Acceptance boundary, review metric and undo

Record CEO spec acceptance **and** host intent alignment against the exact
revision before any implementation subtasks, protected test handoff or live pilot.
Route the decision brief through the President/COO to the CEO and existing host
intent path; a queue/intake receipt is not alignment. The CISO findings above
must receive an independent boundary disposition, not a self-approved grant.
No request should be sent directly to the owner or invented host executor.

The decision must accept or revise: one shot/post, <=5-minute consent only in
T-15, no response-triggered public edit, no nonresponse denominator, private exact
counts/inference limitation, live-signup eligibility, blanket same-guild erasure
closure and parent-lifetime spent marker. No build timeline follows acceptance.
On any material revision, bind a fresh review request and alignment to that revision.

By 2026-10-31, metric is **all 13 fixture-matrix rows have agreed or explicitly
rejected outcomes, and the three blocking findings have written disposition**.
This measures design completeness, not engagement. Proceed only if no unsolicited
message path is added and short-lived state fits existing identity/erasure fences.
Otherwise stop/defer this proposal, not LFG or the project. Default while unresolved
is no build/current signup; no timeout auto-approval. A future demand experiment
requires its own hypothesis/metric/date, not inferred success from this document.

ACT: publish this proposed contract and its offline oracle for review. WHY: bound
consent/data before considering a readiness feature. UNDO: reviewed docs revert;
there is no operational change to undo. A later independently accepted build must
keep admission dark until its own tests/security/release gates pass; disabling
future admission must preserve withdrawal/expiry/erasure, not delete history or
reset spent slots. No production tests, migrations, credentials or activation here.
