# Private, bounded warning history for staff

Status: **Accepted for M2** (owner decision 2026-11-01). Build is tracked on
linked cards; each build re-checks this spec's `file:line` seams on current
`main` first. Original proposal: written 2026-10-26. This is the sole editable
repository spec; the task document mirrors it. Merge publishes a proposal,
**not** intent acceptance, a historical-data grant, command publication or
production activation. No runtime code, schema, retention policy, credential,
permission, data collection or live operation changes.

## Decision and alternatives

Consumer: current staff checking recorded prior warnings before deciding what to
do next. Benefit/frequency are **unmeasured**. A warning record is not proof of
misconduct, current membership or a sanction due. Dates/attribution can establish
that retained records exist; this minimal read cannot explain the incidents.

| Option | Value / limitation | Disposition |
| --- | --- | --- |
| A. No build; existing authorized evidence workflows | No new disclosure. Current moderation/audit procedures may supply evidence within their own authorization; `/warn` records, it does not list history. No ad-hoc DB access, broader token, public question or permission bypass | **Default now**, and whenever gates below remain unresolved |
| B. Private exact-user, current-guild **metadata-only** history | Reuses retained warning facts: reference, time, recorded actor; reasons withheld. Avoids case search/export and new reason disclosure, but staff still need existing authorized incident context | Recommended **proposal for review**, conditional on independent acceptance |
| C. Reason-bearing read, self-service, public history, dashboard/Admin API or export | Reasons can hold allegations, third-party data or pasted secrets; escaping/truncation cannot promise redaction. Other audiences/transport create separate boundaries | Excluded from the smallest slice; no reason permission inferred, build or follow-on cards |

[Carl-bot moderation documentation](https://raw.githubusercontent.com/botlabs-gg/carlbot-docs/master/docs/moderation.md),
read 2026-10-26, describes `/warns @user` listing server/member warnings. That is
UX precedent only; neither broad server listing nor its permission policy is ours.

## Verified source inventory and limits

Inspected baseline: `e1868d97d3519cae3f2ed91726e83e98f77dff0d`.
These are repository facts, **not** deployed-state or live-data observations.

| Seam | Source / consequence |
| --- | --- |
| Warning persistence | `crates/core/src/member_moderation_store.rs:435-456`: guild-bound INSERT of `id,guild_id,user_id,actor_id,reason,request_id,created_at`; request conflicts deduplicate. SQL error handling suppresses parameters/DETAIL (`:236-266`). No second case database needed |
| Command vs read authority | `crates/core/src/moderation.rs:176-178`, `docs/commands.md:778-788`: guild-only `/warn`, Moderate Members, target/reason. `crates/core/src/member_moderation.rs:562-574` is a write seam, **not** a history API; do not call the moderation service to read |
| Stable ID trap | `crates/core/src/member_moderation.rs:1455-1465`: current writer passes request ID as **both warning ID and request ID**. Raw warning ID must not appear in the response; imported ID format/length is unverified |
| Schema/index gap | `crates/cutover/migrations/0110_moderation_member.sql:18-28`: TEXT primary key, unique request ID, native timestamptz; history index is `(guild_id,user_id,created_at)`, **without ID/C-collation tie-breaker**. LIMIT bounds returned rows, not timestamp-tie scan/sort work. Full-order bounded retrieval is not established |
| Fresh-authority building blocks | `crates/bot/src/member_runtime.rs:364-447`: fresh guild-role parsing/permission union and owner/admin expansion; no channel-overwrite resolver here. Existing private read routing uses interaction snapshots, not the new complete authorizer. `crates/core/src/self_roles.rs:525-535` names ViewAuditLog=128 only in a catalogue, not a history-read grant |
| Erasure/retention | `crates/cutover/src/member_erasure_plan.json:93-97`: subject **or actor** erasure selects the whole warning, not actor anonymization; it does not search reason prose. `crates/cutover/src/retention.rs:10-26`: member-deletion paths are distinct from time-driven expiry; no warning age TTL established here. No new TTL invented |

Companion contracts: [member moderation](member-moderation.md#runtime-wiring),
[private replies](interaction-replies.md), [privacy](privacy.md),
[database roles](database-roles.md). Opt-out is not erasure; backups/prose/Discord
copies have documented limits. No real warning/member/role/channel, production
DB, deployed flag or live command registry was read. Re-inventory before a build.

## Proposed read authority and privacy boundary

**Least proposed existing staff authority: Moderate Members**, checked from
current guild roles, **only after explicit metadata-history authorization**.
It matches the existing moderation responsibility without requiring Manage Server
or new roles/tokens. It is **not today's grant to this new surface**. Manage Server,
View Audit Log, authorship, target identity, knowing a reference or a service HMAC
alone are insufficient. Owner/Administrator expansion must use current trusted
resolver semantics, never caller-supplied bits. No actual eligible staff census
was performed. If the approved predicate has no eligible members, retain A; do
not grant permissions to make the feature usable.

**Who sees reasons: nobody through B**, including owners/admins, the original
moderator and the target. Never SELECT, cache, render, log or expose reason text,
reason length, snippets or full-reason controls. UI states “Reasons are not shown
in this view.” Existing authorized operational workflows are unchanged, not
expanded by a link or a denial message. A reason-bearing read requires a revised
exact contract and independent explicit disposition; `/warn` write authority,
View Audit Log, a review score and a merged proposal cannot substitute for that.
The author does not approve their own requested boundary.

Every initial read and continuation:

1. Preserve configured guild/application, moderation activation, human-actor
   admission, shared interaction lanes and existing STOPs. Actor/guild/invoking
   channel come from authenticated interaction context, never options/components.
   No prefix command, public entry, DM or identity override.
2. Confirm ephemeral defer **before warning lookup**. Failed/unknown privacy,
   acknowledgement or expired interaction means no query and no fallback reply.
3. After lane admission, resolve current guild/owner/complete roles, invoking
   human membership, bot membership and the same-guild channel through the shared
   executor. Require Moderate Members at guild level; actor/bot must View Channel
   in a supported GuildText channel, and actor must Use Application Commands there.
   Resolve current leaf overwrites and held roles; no channel-name/category
   inference, cached interaction permissions, threads or forums. Missing/unknown
   facts refuse; no permission-repair calls or new grants.
4. Order explicitly: **fresh authority -> short read-only SQL transaction ->
   commit/close -> final fresh authority -> private reply**. No network within
   the SQL transaction. Discard projected rows on detected authority/access/policy
   change or unknown evidence. The read authorizer is a future prerequisite, not
   the write resolver copied wholesale with target/hierarchy mutations.

One attempt has a **12-second total monotonic deadline** including admission,
ACK, SQL, rendering and reply. Both authority refreshes share at most **seven
seconds / twelve read-only Discord GETs total**; each refresh is at most five
seconds/six GETs and may use only the remaining total. SQL has at most two seconds
within that same deadline, not an extra allowance. Exhaustion means unavailable
(or no reply if the private response cannot be delivered), not relaxed checks,
automatic retry or an extended transport deadline.

Discord GETs are sequential snapshots, not atomic with SQL/reply. Remote
revocation after the final read can race disclosure; erasure after the SQL read
snapshot can likewise race that attempt. Acceptance must cover these residuals
and Discord's already delivered ephemeral copy. Revocation stops future pages,
not screenshots; no promise to recall a past reply or unrequested cleanup call.

## Exact target and minimized data

Proposed entry: `/warnings target_id:<canonical nonzero u64 decimal string>`.
No names/autocomplete search, guild override, page number/size, filters, export,
by-reference API or arbitrary SQL. This command is **not registered** here.

- Scope every SQL lookup, including continuation, to configured guild **and**
  exact target. A target known only in another guild yields local empty history,
  never a global lookup or foreign-user existence signal. Denied callers get no
  metadata, count or existence hint.
- Departed/banned/deleted targets may have retained rows. No target/global profile
  GET, invite or unban is needed. Heading says “Recorded user ID …”; do not claim
  current membership. Read authority is not write-target hierarchy: B could show
  retained metadata about current/former staff or protected targets too. That
  disclosure is explicit in WH-G1, not assumed safe because it is read-only.
- Return **only** stable display reference, recorded UTC time and recorded actor
  ID (or “Actor unavailable”). Target ID appears once. No raw stored warning ID,
  request/idempotency ID, reasons, metadata, token, signature, claim, nickname,
  role list, avatar, join date or other case facts.
- Stable reference is `W-` + 64 lowercase hex characters of SHA-256 over exact
  bytes `UTF8("warning-history-v1") || 0x00 || ASCII(guild_id) || 0x00 || UTF8(stored_id)`.
  Separators are single NUL bytes, **not** backslash-and-zero text. No shortened
  hash/page numbering, new stored ID or signing key. Same row yields same reference;
  it is pseudonymous, not anonymous or a bearer capability.
- Actor is historical attribution, not current membership/staff proof. Show only
  canonical ID as plain text, never a mention or username; malformed/unavailable
  ID gets the fixed label. No row-actor profile lookup. Subject **or actor** erasure
  can remove a whole row; never reconstruct it or assert “never warned.”

## Rendering, SQL and cursor bounds

One private page: **at most five complete rows**, **1,900 UTF-16 units total**.
Fixed ASCII row format: `W-<64hex> | <UTC timestamp> | Actor ID <id>` or fixed
“Actor unavailable”. UTC time uses a fixed format of at most 32 ASCII characters;
unsupported timestamp range is unavailable, not malformed Markdown. Each row is
at most 140 units including separators/newline; five rows use at most 700,
leaving 1,200 for the bounded heading, notices and controls. Enforce the final
body bound; never truncate a reference or silently omit a selected row.

No untrusted prose is rendered. Canonical decimal IDs, hex reference and formatted
time form the only variable output; reason/actor-name injection cannot enter it.
Escape any future nonnumeric label before considering it for this contract; it
is not currently an output field. Disable embeds, attachments, link/resource
fetches and evaluated templates. Explicit allowed-mentions parse/users/roles
empty, replied-user ping off; no active mention, markdown, URL or code syntax.

- Project bounded IDs/time/actor only, **not reason or request_id**. Stored IDs
  over 200 UTF-8 bytes yield a bounded invalid marker in SQL; a page containing
  one refuses unavailable rather than truncating the ordering key or skipping
  evidence. This deliberately permits a malformed historical row to block that
  target's page; investigate through existing authorized procedures, never auto
  repair/quarantine/delete. Raw ID formats are not asserted canonical. Actor
  projection must validate/bound to canonical nonzero u64, otherwise fixed label.
- One short **READ ONLY** transaction, at most **six rows** (five plus has-more
  sentinel), no COUNT/OFFSET/SELECT *, locks, claims, audit/read-receipt INSERT or
  mutation-service call. Sentinel is ID/time only; do not project its actor.
  IDs and actor fields are bounded in SQL before deserialization. No whole-history
  fetch, joins or implicit DB write. Timeout/decode failure is unavailable, not empty.
- Order by native `created_at DESC, id COLLATE "C" DESC`. First page binds highest
  tuple as high-water; later predicates are `tuple <= high_water AND tuple < last_shown`
  with the **same C collation** and guild/target predicates. Last **displayed** row,
  not sentinel, advances. Equal-time IDs `c,b,a`, illustrative size two: show `c,b`,
  next `< b` shows `a`. ID is only a tie-breaker, not causal chronology.
- Newer tuples wait for a fresh invocation. This is **not a frozen snapshot**:
  newly committed backdated rows may enter below the cursor; erasure can remove
  rows between pages. Never claim an exact lifetime total or complete history.
  The missing full-order index is a prerequisite, not repaired by LIMIT: WH-G2
  needs a tie-heavy disposable fixture and EXPLAIN evidence of an ordered bounded
  plan. No index migration or production query is authorized by this proposal.
- Only **Next older / Stop**. Cursor is an opaque random component handle with
  server-side binding to version, guild, actor, channel, target, high-water, last
  tuple, page count and expiry. No response bodies/reasons/profiles cached. Handle
  is a selector, never authority; never render/log it or embed raw stored IDs.
  Validate shape/size before lookup. Reauthorize on every page, no payload replay.
- **Five pages / 25 rows per session**, **two-minute absolute lifetime**, one session
  per actor/guild, **128 sessions per process**, one page in flight per session.
  Consumed handles cannot skip/duplicate pages concurrently. Expiry/restart/
  unknown/foreign/reused handle gives the same expired-page response, no query,
  retargeting or page-one fallback. Stop/expiry discards transient bindings; no
  cleanup job or durable cursor store.
- One attempt per guild/actor per **five seconds**, before authority reads,
  including failed attempts/pages; **1,024 active cooldown entries per process**.
  Full session/cooldown capacity returns WH_BUSY, never evicts active entries or
  allocates beyond bounds. These process caps can cause cross-guild refusal;
  accept that availability trade-off explicitly, not claim tenant-isolated quotas.
  Restart clears local limits; shared admission remains required. Repeated fresh
  invocations/screenshots can accumulate data: bounds are not an exfiltration cure.

Only the deliberate invoking private interaction ACK/original-response edit is
allowed. No ordinary channel post/edit/delete, DM, public audit mirror, export,
background refresh/retry, moderation effect, publication, job, or nonce burn.

## Error / next-action matrix

Codes are stable **proposal/test IDs**, not deployed wire responses. Human replies
remain plain language; do not echo rejected input, cursor, SQL/Discord error or
request/token values. Without confirmed ephemerality send **nothing**, not fallback.

| Code | Example private copy | Retryable / next action |
| --- | --- | --- |
| `WH_DENIED` | “Warning history is not available to you here.” | No; stop or ask existing guild administration to check current access, not grant new permission |
| `WH_INVALID` | “Enter one exact user ID, not a name or server ID.” | No; correct the invocation; zero warning reads |
| `WH_EMPTY` | “No retained warnings found for this user ID in this server.” | No; only successful authorized initial zero read. Not ‘never warned’ |
| `WH_PAGE` | “Recorded warnings for user ID … — newest first. W-… / time / recorded actor ID … Reasons are not shown in this view.” | No automatic retry; Next older / Stop |
| `WH_END` | “No older retained warnings found. Start again to refresh.” | No; successful continuation zero read, not initial empty history |
| `WH_LIMIT` | “Session limit reached: up to 25 retained warnings shown. This is not a complete history.” | No; stop, no export fallback |
| `WH_EXPIRED` | “This private page has expired. Run /warnings again.” | No; fresh invocation, never invalid-cursor-as-page-one |
| `WH_BUSY` | “Warning history is busy. Please try again later.” | Yes, explicit later retry after cooldown/capacity clears, no queued read |
| `WH_UNAVAILABLE` | “Warning history could not be checked. No conclusion about prior warnings can be drawn. Try again later.” | Yes, explicit retry; no guessed empty, cached payload or auto repair |

Missing/forged/revoked authority or foreign invocation context denies before
warning lookup. Failed/incomplete current Discord or DB evidence is unavailable,
not absence; only authoritative caller departure establishes denial. Error copy
never exposes a foreign target's existence, count, actor or reason.

## Threat model / future acceptance matrix

Assets: sanction existence, attribution, reasons/secrets, tenant isolation and
unchanged moderation state. Boundaries: untrusted interaction/component -> fresh
Discord authority -> guild-bound read -> minimized private response. Component
secrecy/picker visibility are not gates; guild membership alone is insufficient.

**Planned oracles, not executed runtime tests.** Every scenario also spies zero
DML/claims/audits/jobs, no reason/request field reads or logs, zero Discord writes
except the requested ephemeral reply, no target/row-actor profile lookup.

| ID | Input / threat | Required oracle |
| --- | --- | --- |
| `WH-S01` | Current eligible human and known guild/channel access | At most five scoped metadata rows, stable full references, private only |
| `WH-S02` | No Moderate Members; Manage Server/View Audit Log only; self/author claim | Denied, no history reads/hints. Moderate Members qualifies only under the independently accepted historical-read rule |
| `WH-S03` | Removed staff/departed caller; forged permissions/roles/actor; real owner/admin | Resolve current facts; forged/removed denied; trusted owner/admin expansion only; unknown evidence unavailable |
| `WH-S04` | Revocation/visibility/policy change in lane/SQL/send wait | Refresh after waits, discard data on change/unknown; assert request deadline; final-read race documented |
| `WH-S05` | DM, foreign application/guild/channel, hidden channel, malformed/duplicate options | Denied/invalid with zero history lookup, fallback or repair |
| `WH-S06` | Same target/ID-shaped rows only in foreign guild, then local rows | Local EMPTY versus local-only page; no global existence check |
| `WH-S07` | Departed/banned/protected/current staff target; departed/malformed/erased row actor | No present-membership claim; historical ID/fixed label only; actor erasure can remove whole row |
| `WH-S08` | Hostile reasons containing mentions/markdown/URLs/bidi/tokens; malformed actor; oversized ID/time; maximal output | Reason column never read or rendered; no raw request ID; ID poison fails closed; complete bounded inert rows and no payload logs |
| `WH-S09` | Large equal-timestamp set, C-order IDs, clock rollback, >six rows | Six-row bound and last-shown keyset; sentinel not skipped; EXPLAIN full-order index/range proof on disposable tie-heavy fixture, not just row-count assertion |
| `WH-S10` | Newer/backdated insert or erasure between pages | Honest high-water/current-retained semantics; no frozen-copy/total promise or saved payload replay |
| `WH-S11` | Tampered/oversized/expired/reused/foreign cursor, restart | Uniform expired response after context gate; no query/retarget/page-one fallback |
| `WH-S12` | Concurrent Next, sixth page, full session/cooldown capacity | Single in-flight page, bounded memory, no skip/duplicate; busy/limit/expiry as specified |
| `WH-S13` | REST 403/429/404/5xx/timeout, incomplete roles, DB timeout/decode/error | Denied only on proven access loss; unavailable otherwise, never empty; bounded attempts/no retry loop |
| `WH-S14` | Authorized first zero read, exhausted continuation, failed read | Distinct EMPTY / END / UNAVAILABLE with next action, no fabricated total |
| `WH-S15` | Private ACK/send failure or token expiry | Zero lookup before ACK; no public/DM fallback, resend or saved result replay |
| `WH-S16` | Spy SQL/executor; subject and actor erasure; repeat/Stop | Only bounded read + requested private reply; all warning/audit/idempotency/unban rows and jobs unchanged; source erasure not reconstructed |

No warning TTL, privacy purpose or erasure policy changes. No durable page cache,
export, per-member telemetry or new read-access ledger. Transient bindings reuse
existing interaction context only, expiring as above. Existing redacted diagnostic
signals are **not** an attributable per-reader audit trail: acceptance must decide
if this lack is acceptable; otherwise revise scope before collecting anything new.

## Handoff and explicit STOPs

Smallest conditional future slice: **one bot-only metadata `/warnings` read**,
its guild-bound keyset projection/private components and protected mock/disposable
DB acceptance for WH-S01–S16. Reasons remain withheld. No schema migration,
web service, new role/key/policy, runtime implementation or build/acceptance cards
are created in this design task.

Before **any** future build, bind recorded host/CEO intent alignment and independent
security/frozen-copy disposition to the exact task spec revision, repository commit
and file SHA-256. Explicitly resolve:

- `WH-G1`: metadata-history permission for the proposed current staff cohort,
  retained departed/protected/staff targets, and **withheld reasons for everyone**.
  If reasons are required, revise the spec and disposition first; no inferred grant.
- `WH-G2`: stored-ID bounds, timestamp/index evidence and subject/actor erasure/
  retention limitations. Current index lacks full-order tie-breaking; if safe bounded
  retrieval needs an index migration, a separately accepted prerequisite is required
  before this no-schema read slice, not silent scope expansion or a production EXPLAIN.
- `WH-G3`: missing fresh guild/channel resolver and component integration, residual
  ACL/erasure races, ephemeral copies, process-wide capacity trade-off and no new
  attributable read log. An incompatible accepted frozen-copy rule means retain A.
- `WH-G4`: unchanged security requirements and protected acceptance handoff:
  immutable baseline/manifest/hash, behavior-specific RED (environment failure is
  not RED), then existing exact-head CI/review/release/activation gates. No copy of
  another pending branch, new admin gateway or activation clearance inferred here.

Pending evidence or elapsed review date is **not** acceptance; default remains A.
This is not a routine deployment-approval hop or a permission request. Review by
2026-11-02; metric: 16 scenario oracles and four gates explicitly decided, zero new
runtime/disclosure. Unverified: deployed state/data, demand, operational costs and
independent decisions. No live experiment or spend.

Run from repository root (offline; no DB/Discord credentials):

```sh
python3 scripts/check_docs_index.py --json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_index.py' -v
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
```

These verify doc discovery/budgets/paths, **not** WH-S01–S16 runtime behavior.
ACT: publish a bounded proposal. WHY: settle historical-read privacy before build.
UNDO: revert docs; no operational state changed.
