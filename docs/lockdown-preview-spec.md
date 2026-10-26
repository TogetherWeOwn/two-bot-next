# Private lockdown preview before execution

Status: **Proposed; design only**. Written 2026-10-26; review by 2026-11-02.
No commands, components, storage, migrations, permissions, sends or runtime
tests are added, and no lockdown is performed. Merge publishes a proposal, not
permission to implement or activate it. This file is the single repository
contract; a review request binds its exact commit and SHA-256.

## 1. Decision, alternatives and scope

Problem: `/lockdown` takes only a mandatory reason and then executes. The
moderator cannot first see, in one place, which five `@everyone` bits will move,
whether an `@everyone` overwrite already exists, which unrelated bits survive,
or which roles would still be able to post through their own overwrites. Those
facts are exactly the facts a moderator needs before locking a channel that may
already be partially locked, manually edited, private, or staff-exempt.

Recommend reviewing **B**: an optional, private `/lockdown-preview` that renders
the effective diff for the invoking channel and offers one bound **Confirm
lockdown** button. Confirm re-reads live state and refuses as stale if anything
the preview depended on changed. Direct `/lockdown` stays available and
unchanged for emergencies; preview is never mandatory and adds no latency to the
raid path. **A/no-build remains in force** until the section 8 gates pass.

| Option | Benefit / cost | Disposition |
| --- | --- | --- |
| A. No build (runbook only) | `/channel-inspect` for saved state, Discord's native channel-permissions screen for live overwrites, then `/lockdown`, verify, `/unlock` if wrong. No new surface; a wrong lock is briefly live and the diff must be assembled by hand | Use now; fallback if B's gates fail (detail in section 7) |
| B. Bound preview, then confirm | One ephemeral diff, one digest-bound confirm, stale-preview refusal, preview audit row. Adds one slash command, one component path into the channel runtime, one roles read and bounded in-memory pending state | Smallest option that fixes the diff, divergence, rate and audit questions together; recommend for **spec review**, not build |
| C. Unbound read-only preview only | Less code, mirrors `/purge-preview`; but a later separate `/lockdown` can silently execute against permissions that changed after the preview | Reject here. Adopting C means amending this proposal's stale-preview requirement explicitly, never downgrading B silently |
| D. Mandatory preview inside `/lockdown` | Every lockdown is previewed; slows incident response and changes the semantics and registry of an existing command | Reject; changing `/lockdown` semantics is out of scope |
| E. Multi-channel or category preview | Shows a wider blast radius; multiplies reads, rows and rendering, and the current lockdown writes one channel only | Reject; one invoking channel per preview |

Out of scope and not proposed: server-wide or category lockdown, timed or
automatic unlock (see [timed lockdown decision](time-boxed-lockdown-decision.md)),
slowmode or purge changes, role/member overwrite edits, member lists, a dashboard
surface, notifications, new credentials or spend.

## 2. Verified baseline and existing seams

Inspected repository head: `92d850425`. Source facts below are not measurements
of a deployed guild.

- **Lockdown today.** [`docs/commands.md`](commands.md) lists `/lockdown` with
  `ManageChannels` (`16`) and one required `reason` (max 512). The permission row
  is `crates/core/src/command_permissions.rs:139-141`. The reason rule is the
  shared moderation rule: trimmed, non-empty, at most 512 UTF-16 units
  (`crates/core/src/moderation.rs:383-392`, re-exported at
  `crates/core/src/channel_moderation.rs:180-182`).
- **What lockdown writes.** `LOCKDOWN_BITS` is five bits —
  `SEND_MESSAGES` (2048), `ADD_REACTIONS` (64), `CREATE_PUBLIC_THREADS`
  (34359738368), `CREATE_PRIVATE_THREADS` (68719476736),
  `SEND_MESSAGES_IN_THREADS` (274877906944); sum `377957124160`
  (`crates/core/src/channel_moderation.rs:14-28`). `plan_lockdown`
  (`:233-251`) clears those bits from the `@everyone` allow mask, sets them in
  deny, preserves every other bit, and seeds recovery with the prior masks
  (`None` becomes `"0"/"0"`, `prior_exists: false`). It does not touch
  `VIEW_CHANNEL` or any role/member overwrite; the runtime doc states a
  role/member send allow still wins
  ([`channel-moderation-runtime.md`](channel-moderation-runtime.md) lines 17-22).
- **Execution order and fences.** In `crates/discord/src/channel_moderation.rs`,
  the mutation path claims an idempotency ticket, claims the channel lane, then
  `prepare` (`:478-535`) reads saved recovery, GETs the live `@everyone`
  overwrite, plans, persists the seed **before** the PUT, and dispatches through
  the shared executor. Refusals and outcomes are audited via `ChannelAuditRow`;
  ambiguous Discord failures deliberately keep claim and lane. Confirm in B must
  enter this same path — a second mutation implementation is a blocking finding.
  Today `ChannelModerationRuntime::accepts` (`:143-150`) admits slash
  interactions only, so routing a component press into this runtime is new
  work, not an existing seam.
- **Preview seams.** `/purge-preview` (`docs/commands.md` §`/purge-preview`) is a
  private read that mirrors execution's planner, says **not a guarantee**, and
  branches **before** audit, idempotency, claim or lane admission
  (`crates/discord/src/channel_moderation.rs:251-256`;
  [`channel-moderation-runtime.md`](channel-moderation-runtime.md) lines 85-91).
  `/channel-inspect` reports saved lockdown presence and live slowmode but is
  explicitly *not* live-overwrite verification
  (`crates/discord/src/channel_moderation_reads.rs:106-140`). `/welcome-preview`
  is a read-only ephemeral render that never calls the delivery executor
  (`crates/bot/src/welcome_preview.rs:1,59-85`). B reuses the ephemeral
  defer/edit lifecycle, mention suppression and router adjudication of the
  channel reads; it deliberately does **not** copy purge-preview's
  no-audit rule (section 6 explains why).
- **Effective-permission ordering.** `resolve_channel_access`
  (`crates/discord/src/channel_access.rs:28-81`) already encodes the Discord
  order used here: `ADMINISTRATOR` bypasses; base (`@everyone` role | held
  roles), then `@everyone` overwrite, then the union of held-role overwrites,
  then the member overwrite; send requires view (`:71-72`). It resolves one
  member, not one role, so B defines a single-role projection over the same
  ordering instead of pretending to enumerate members (section 4).
- **Bounded preview-then-confirm seam.** Voice config import already binds a
  confirm button to `(guild, member, hash)` over a bounded in-memory pending map
  with TTL, consume-once take and idempotent cancel
  (`crates/bot/src/voice_rooms.rs:8999-9053`;
  `crates/core/src/voice_custom_id.rs:63,116-122`). B copies that *shape* (new
  map in the channel runtime), with one stricter rule: at cap, refuse instead of
  evicting someone else's live moderation preview.
- **Reads available and missing.** `get_everyone_overwrite` strictly validates
  the channel document and every overwrite row's identity before it can count a
  row as "not `@everyone`" (`crates/discord/src/executor.rs:2320-2447`), but
  returns only the `@everyone` row. `fetch_channel_document` (`:2454-2473`)
  keeps the whole document without that per-row validation. There is **no**
  guild-roles read in `ActionExecutor`; the only roles consumer is online
  preflight (`crates/bot/src/preflight.rs:498-514`), which fails closed on an
  incomplete snapshot. B therefore needs one new bounded snapshot reader (full
  overwrites, strict validation shared with `read_everyone_overwrite`) and one
  paced roles GET in the executor. If either cannot fail closed, stop at A.

No live guild, role set, permission set or deployment was inspected. Discord's
category-sync effect of editing a synced child's overwrite was **not** verified
here; section 4 renders the parent id and makes no sync claim.

## 3. Proposed flow

All behaviours below are **future acceptance requirements**, not current
commands. Authorization at every step is identical to `/lockdown`: configured
guild, moderation feature gate, guild-only, `ManageChannels`, adjudicated by the
shared router. The preview command must never be a weaker side door into
permission data than the mutation it previews.

1. **Preview.** The moderator runs `/lockdown-preview reason:<text>` in the
   channel they intend to lock. Only `reason`, exactly once, trimmed,
   non-empty, ≤512 UTF-16 units (the shared validator above). No channel,
   role, member or duration option. Validation or router refusal happens before
   any Discord read and writes no audit row, mirroring the channel reads.
2. **Bounded live read.** After the rate limiter admits the preview (section 5),
   read, once each: the channel document with full strictly-validated
   `permission_overwrites`; the guild role list; the saved lockdown record.
   Channel type must be text (`0`) or announcement (`5`) and the document's
   guild must match; threads, voice, forum, category and foreign-guild
   documents refuse as `UNSUPPORTED_CHANNEL` with nothing rendered. A saved
   record whose guild differs from the interaction guild refuses the same way,
   as `prepare` and `/channel-inspect` already refuse it. Any failed
   or malformed read refuses the whole preview — no half-diff from the reads
   that succeeded. (This narrows the *preview flow* only; it changes nothing
   about which channels direct `/lockdown` accepts today.)
3. **Render and arm.** Compute the section 4 diff from that single snapshot,
   write the section 6 preview audit row, and reply ephemerally with the diff
   plus **Confirm lockdown** / **Cancel** buttons
   (`lockdown-preview:confirm:<actor>:<digest32>` /
   `lockdown-preview:cancel:<actor>:<digest32>`, each ≤100 characters).
   A pending entry keyed by `(guild, channel, actor, digest)` is stored only
   when the reply carries a Confirm button (armed previews); replies without
   Confirm (step 6) store nothing and consume none of the section 5 pending
   cap. If the audit insert fails, render nothing and store nothing.
4. **Cancel** removes the pending entry when one exists, and the buttons; it
   writes no audit row and no mutation. Expiry and process restart are
   equivalent to cancel.
5. **Confirm** consumes the pending entry on first press (consume-once; any
   second press, foreign press, forged digest or post-restart press gets the
   collapsed `PREVIEW_EXPIRED` reply). It then re-runs router adjudication,
   re-reads the same three inputs, recomputes the digest and applies the
   section 5 divergence rules. On an exact match it enters the existing
   lockdown mutation path with the pending entry's reason and the confirm
   interaction id as its idempotency identity; inside the lane, `prepare`'s
   own fresh GET must still produce the previewed prior masks and planned write,
   or it refuses before recording a seed or sending a PUT.
6. **Already locked.** If the live `@everyone` overwrite already denies all
   five bits (planned write equals current masks), the preview renders,
   is audited, states that nothing would change, and carries **no** Confirm
   button and stores **no** pending entry, so unarmed previews can never
   exhaust the 64-entry cap. A moderator who deliberately wants the existing
   repeated-lockdown
   behaviour uses `/lockdown`, whose seed-preservation rules are unchanged
   ([`channel-lockdown-acceptance.md`](channel-lockdown-acceptance.md) item 4).
7. **Partial locks.** If some but not all five bits are denied (legacy
   send-only lock or a manual edit), the preview shows the remaining diff and
   Confirm is offered. Saved-recovery presence is rendered separately from the
   live send deny, because the two can disagree in both directions (manual lock
   without a seed; seed whose deny was removed externally). Neither fact alone
   is presented as "locked".

## 4. What the preview renders (fixed)

One channel (the invoking channel), one snapshot instant, ephemeral, mentions
disabled through the standard mention-neutralizing edit (not the
`channel-inspect` verbatim-reason exception), at most **1,900 UTF-16 units**
(the repository's message-length convention, under the 2,000-unit limit).
The moderator's reason is never echoed. Fixed lines, in order:

1. `Lockdown preview — nothing was changed`
2. `channel: <#id> (text|announcement; category <parent-id>|none)` — the parent
   id is shown with `category sync is not evaluated; only this channel is read
   or written`. No other channel is sampled or claimed unaffected.
3. `expires <t:epoch:R> · Confirm re-reads live permissions and refuses if they changed`
4. `saved bot lockdown: present|none · live @everyone SendMessages: allow|deny|neutral`
5. `@everyone overwrite: present allow=A deny=D -> allow=A' deny=D'`, or
   `absent (0/0) -> created allow=A' deny=D'`, with decimal masks from
   `plan_lockdown` applied to the snapshot. One line per lockdown bit:
   `<Name>: <allow|deny|neutral|allow+deny> -> deny · roles losing usable <bit>: n · keeping: n`.
   A bit set in both masks renders as `allow+deny` (the planner's result is
   still deny), never silently normalized.
6. `ViewChannel: unchanged by lockdown (@everyone overwrite: allow|deny|neutral)`
7. `other @everyone bits preserved: allow n bit(s), deny n bit(s)` — counts of
   set bits outside `LOCKDOWN_BITS`; unrelated permission names are not
   enumerated.
8. Single-role projection summary (definitions below):
   `projections: R role(s) · would lose SendMessages: a · would keep: b · cannot send already (usable): c`
9. `roles keeping SendMessages: <@&id> …` — up to **10** role tokens, position
   descending then id ascending, then `(+n more not named; counts are complete)`
   when truncated. If the 1,900-unit bound cannot be met with names, drop
   names to the count-only form; never truncate a count or a warning.
10. `member overwrites allowing a lockdown bit: n (members are never listed;
    lockdown does not change these)` and
    `Administrator roles: n (bypass channel overwrites; unchanged)`.
11. Closing fixed caveats: members holding several roles combine role
    overwrites (any role-layer allow wins over role-layer denies) and a member
    overwrite then wins over both, so single-role rows are a projection, not a
    member roster; the bot's own write permission is **not** proven by a
    preview and Discord may still refuse the PUT; direct `/lockdown` remains
    available and does not consult this preview.

**Projection definition.** For each guild role `r` (including the `@everyone`
role, whose id equals the guild id), evaluate a synthetic member holding only
`r`, with no member overwrite, using the `channel_access.rs` ordering:
`base = @everyone.permissions | r.permissions`; `ADMINISTRATOR` in base makes
every projected bit usable; otherwise apply the `@everyone` overwrite, then
`r`'s own overwrite when `r` is not `@everyone`. A bit is *usable* only when
`VIEW_CHANNEL` and the bit are both present. "Before" uses the snapshot
overwrite; "after" replaces only the `@everyone` overwrite with the planned
write (an absent prior becomes a present planned overwrite). Lose/keep counts
compare usable-before to usable-after per bit over **all** returned roles.

**Completeness gates (refuse, never render partial claims):** role list must
contain the `@everyone` role exactly once, no duplicate role ids, no duplicate
`(type, id)` overwrite rows, every
permission mask and overwrite row strictly parseable in the
`read_everyone_overwrite` sense; at most **250 roles** and **100 overwrites**
(proposed bounds, not claimed platform limits) or the outcome is
`TOO_COMPLEX` with the section 7 fallback named. Rendering reads no member
list, no history, no other channel.

## 5. Digest, divergence, expiry and rate bounds

**Digest.** `digest = SHA-256(canonical snapshot)`, versioned
`lockdown-preview.v1`. Canonical form is a JSON array with decimal-string
integers and no whitespace: version, guild id, channel id, channel type,
parent id or `0`, actor id, trimmed reason, `prior_exists`, prior allow/deny,
planned allow/deny, saved-lockdown present flag, saved recovery generation
(or `""`), roles sorted by id as `[id, position, permissions & INTEREST]`, and
overwrites sorted by `(type, id)` as `[type, id, allow & INTEREST, deny & INTEREST]`,
where `INTEREST = LOCKDOWN_BITS | VIEW_CHANNEL (1024) | ADMINISTRATOR (8)`.
Canonical bytes are the UTF-8 encoding of that array with compact separators
and non-ASCII escaped (`\uXXXX`, astral planes as surrogate pairs) — exactly
Python `json.dumps(snapshot, separators=(",", ":"),
ensure_ascii=True).encode()`. Strings are not Unicode-normalized: the reason
is digested exactly as trimmed, so composed and decomposed spellings are
distinct inputs. A Rust implementation must reproduce these exact bytes, not
merely an equal parse.
The full digest is computed and compared in memory; buttons carry only its
first 32 hex characters as the pending lookup key. Timestamps are not digested;
expiry lives in the pending entry. The digest is not logged, audited or used
as a metric label.

**Stale-preview refusal.** Confirm re-reads and recomputes; **any** digest
mismatch refuses with `STALE_PREVIEW`, writes one refused preview audit row
(section 6), and performs no claim, lane, seed or PUT. That includes changes a
moderator might call unrelated — an `@everyone` `VIEW_CHANNEL` flip, a role
base-permission or position change, a member-overwrite allow added, saved
recovery created by another moderator's lockdown — because the planned write's
preserved bits and every rendered warning depend on the full snapshot. The
reply says permissions changed and to preview again; it does not say which
field changed (the fresh preview is the diff tool, and field-level leak to a
possibly-no-longer-authorized presser is avoided).

Honest limit, unchanged from today: the stale check covers the
preview→final-read interval only. Discord offers no conditional write; the
final GET and the PUT are separate requests, so an external edit between them
can still be overwritten
([`channel-lockdown-acceptance.md`](channel-lockdown-acceptance.md) lines 39-43).
The preview text must not claim a compare-and-swap guarantee.

**Other confirm divergences.** Actor no longer passes the router gate (role
lost, feature disabled, wrong guild) refuses before any read, pending already
consumed. Channel lane already held by another channel action yields the
existing uncertain reply — no PUT, no retry, re-preview later. A proven
no-effect Discord refusal (e.g. 403 on PUT) and an ambiguous failure keep the
existing `finish`/retain semantics byte-for-byte; confirm never resends.

**Bounds (all proposed constants).**

- Pending TTL **300 seconds** from preview reply; `now >= expires` refuses.
- At most **1** outstanding armed preview per `(guild, channel, actor)`; a new
  armed preview invalidates that actor's previous digest for that channel.
  Unarmed previews (no Confirm) create no entry and invalidate none. At most
  **64** pending entries process-wide; at cap, refuse `PREVIEW_BUSY` (never
  evict a live entry). Expired entries are swept lazily on insert/take.
- Rate: at most **10 previews per actor / 10 minutes** and **30 per guild /
  10 minutes**, charged after router+validation and **before** any Discord read,
  so refused-for-rate costs zero GETs. Counters are in-memory in the channel
  runtime; that is sound only while the bot runs as the single always-on
  process this repository deploys — if that changes, the limiter must move to
  the store before this ships. Restart resets counters (damping, not accounting;
  the audit row is the accounting).
- Reads per preview and per confirm stale-check: ≤1 channel GET, ≤1 roles GET,
  ≤1 saved-lockdown SELECT. The existing mutation `prepare` adds its own GET;
  B adds no other reads and no retries of reads beyond the executor's existing
  single paced attempt.
- Pending retention: preview interaction id, digest, prior/planned masks,
  saved-generation string, counts used for audit metadata, expiry, and the
  trimmed reason (≤512 UTF-16 units) — in memory only, never logged, gone on
  consume/cancel/expiry/restart.

## 6. Audit of previews

`/purge-preview` and `/channel-inspect` write no audit rows. A lockdown preview
differs in kind: it discloses the channel's full overwrite structure and role
projections to one moderator and it arms a permission **mutation**. An
unaudited armed preview would make "who staged this lockdown, and was the
executed write the previewed one?" unanswerable. B therefore audits previews
while keeping the metadata discipline of the existing ledger for preview rows
(`ChannelAuditRow.metadata_json` is bounded numbers only,
`crates/core/src/channel_moderation_store.rs:15-31`; the one correlation id on
confirmed-execution rows follows the ledger's existing string-`request_id`
precedent, next paragraph but one):

| Event | Audit effect (action stays `moderation.lockdown`) |
| --- | --- |
| Preview rendered | Exactly one row, outcome `lockdown_preview`, `reason: ""`, `request_id`/`idempotency_key` = preview interaction id, metadata numbers only: `prior_exists`, `roles_evaluated`, `roles_keeping_send`, `member_overwrite_allows`. Row is written **before** the reply; insert failure ⇒ no render, no pending entry |
| Router/validation refusal, read failure, `TOO_COMPLEX`, rate refusal | No row (nothing disclosed), mirroring the reads branch; the rate limiter still charged where section 5 says so |
| Cancel, expiry, restart loss, duplicate/foreign press | No row |
| Confirm → stale | One row, outcome `refused`, metadata `{"stale_preview":1}`. If that insert fails the refusal still stands (no mutation is at risk); log the fixed `channel_refusal_audit_failed` token as the missing-context path does |
| Confirm → executed or refused in the mutation path | The existing lockdown audit row (real reason, existing outcomes), with `preview_request_id` added to its metadata (next paragraph). No digest, mask or role id is added |

`preview_request_id` is the confirmed preview's interaction id as a decimal
string — the same value the preview row stores as `request_id` and
`idempotency_key`. It is the preview↔execution correlation key; actor,
channel and time adjacency is secondary context, never the key, because a new
preview can render and audit after confirm consumes the pending entry but
before the execution row lands. The value is bounded (at most 20 ASCII
digits), non-sensitive, and follows the ledger's existing `{"stage": "claim",
"request_id": "83"}` precedent while keeping snowflakes as strings, never
integers (`crates/core/src/audit.rs:87-88`). Direct `/lockdown` rows keep
their existing empty metadata.

Previews take no idempotency claim, no channel lane and no lease, and do not
increment moderation-enforcement metrics (reads today do not; a preview
command name is not a `ModerationAction` command name). The component confirm
path must record exactly one enforcement sample for the `/lockdown` it becomes
(sample recording today is keyed off the slash command name at
`crates/discord/src/channel_moderation.rs:662-671`, so a component
interaction records nothing by itself); a preview render records none. Preview rows live in the same
`moderation_audit` ledger with the same retention and member-erasure coverage
as other channel audit rows — no new store and no new retention; the build
must prove the erasure manifest selects them by actor like existing rows
([`privacy.md`](privacy.md) moderation-ledger note) or stop. The ephemeral
Discord preview message itself (masks, counts, ≤10 role tokens, no member
ids, no reason) persists in the moderator's client until dismissed; its
platform retention is **not verified** and is a section 8 boundary item.

## 7. No-build alternative (A), in force until B is accepted

1. Run `/channel-inspect` — saved lockdown presence/reason/date and live
   slowmode, knowing it is not live-overwrite proof.
2. Open the channel's native Discord **Settings → Permissions** screen (same
   `ManageChannels` authority) and read the `@everyone` row and each role row
   by hand; note roles whose row allows sending.
3. Run `/lockdown reason:<text>`; re-open the permissions screen and
   `/channel-inspect` to verify; use `/unlock` if the result is wrong (it
   restores only the recorded bot change and refuses on drift).

Cost accepted by choosing A: no single effective diff, no automated stale
check, and a mistaken lock is live until noticed. A adds no surface, no reads,
no retention and no spend, and remains the correct outcome if any section 8
gate fails.

## 8. Security boundary, acceptance matrix and sign-off path

**Threats and controls** (assets: overwrite structure, role layout, the staged
mutation; boundary: moderator interaction → live Discord reads → ephemeral
render → bound confirm → existing mutation path):

| Threat | Control and residual |
| --- | --- |
| Permission-structure disclosure to a non-moderator | Same `ManageChannels` router gate as `/lockdown` at preview **and** confirm; ephemeral only; no member ids; role names only as inert tokens, ≤10. A ManageChannels holder can already open the native permissions screen, so B discloses nothing beyond their existing authority — accept on that basis or not at all |
| Forged/foreign/replayed confirm | Pending keyed by `(guild, channel, actor, digest)`; actor equality from the authenticated interaction, never from the custom id; consume-once; collapsed `PREVIEW_EXPIRED`. A screenshot of a custom id alone arms nothing without a live pending entry |
| Stale execution against changed permissions | Full-snapshot digest recheck plus `prepare` mask/write equality inside the lane. Residual: external edit between final GET and PUT (section 5) — pre-existing, not widened by B |
| Preview as a denial-of-service / audit-flood tool | Rate charged before reads; 1 outstanding per actor+channel; 64-cap with refuse-not-evict; ≤3 reads per stage; audit row only on render/stale. Residual: 10 audited previews/10 min/actor is intended accountability volume |
| Reason leakage or retention creep | Reason absent from render, logs, metrics and preview audit; held only in the 300 s in-memory pending entry; executed row stores it exactly as `/lockdown` does today |
| Partial/misleading diff | Completeness gates refuse on any unreadable row, missing `@everyone` role or cap breach; counts are never truncated; projection limits and unproven bot-write are printed on every preview |
| Bypass of claim/lane/audit by the component path | Confirm must synthesize the same `Request` and reuse claim → lane → prepare → dispatch → finish. If component routing cannot reach that path through the shared router/admission, **stop at A** — do not fork a mutation path |

**Design test matrix** for a future Test Author (planned oracles, not runtime
evidence; synthetic REST/store doubles and an injected clock only — never a
live guild). Every row also asserts zero unexpected Discord writes:

| Case | Setup → required outcome |
| --- | --- |
| `LP-01` | No `@everyone` overwrite → render `absent (0/0) -> created allow=0 deny=377957124160`; confirm executes once through the existing path |
| `LP-02` | Present masks with unrelated bits (e.g. allow `VIEW\|EMBED`, deny `MANAGE_MESSAGES`) → planned masks change only the five bits; preserved counts exact |
| `LP-03` | A lockdown bit set in both allow and deny → renders `allow+deny -> deny`; planned masks still from `plan_lockdown` |
| `LP-04` | One role overwrite allows `SEND_MESSAGES`; one member overwrite allows a thread bit → role named as keeper; member counted (1), never identified; counts complete |
| `LP-05` | Role with base `ADMINISTRATOR`; `@everyone` view denied → admin keeps usable send; non-admin roles show `cannot send already` where view gates them |
| `LP-06` | All five bits already denied → audited preview, **no Confirm button**, no pending entry (unarmed previews never cause `PREVIEW_BUSY`); partial legacy lock (send-only deny) → remaining four bits diffed, Confirm present |
| `LP-07` | Saved seed present but live send deny removed (and vice versa) → both facts rendered separately; digest covers generation, so a mid-flight seed change is stale |
| `LP-08` | Between preview and confirm: `@everyone` lock bit change → `STALE_PREVIEW`; zero claim/lane/seed/PUT; exactly one `stale_preview` audit row |
| `LP-09` | Between preview and confirm: only a role base bit, role position, member-overwrite allow, or unrelated `@everyone` `VIEW_CHANNEL` changes → still `STALE_PREVIEW` (full-snapshot digest) |
| `LP-10` | Actor loses `ManageChannels` or the feature gate closes before confirm → refused before reads; pending consumed; foreign actor / forged digest / double press / cancel-then-confirm / restart → collapsed `PREVIEW_EXPIRED`, no audit row |
| `LP-11` | Confirm at `now == expires` → refused; at `expires - 1s` with unchanged snapshot → executes |
| `LP-12` | Malformed masks, missing/duplicate `@everyone` role, duplicate overwrite row, guild/type mismatch (thread, voice, forum, category), saved-record guild mismatch, missing `permission_overwrites`, one failed read → `UNAVAILABLE`/`UNSUPPORTED_CHANNEL`; no partial render, no audit row |
| `LP-13` | 11 keeper roles → 10 named + `(+1 more …)`; 101 overwrites or 251 roles → `TOO_COMPLEX`, no render; longest render ≤1,900 UTF-16 units with counts intact |
| `LP-14` | Audit trail: render ⇒ exactly 1 `lockdown_preview` row (numbers-only metadata, empty reason), 0 claims/lanes/seeds/metric samples; audit insert failure ⇒ no render, no pending; executed confirm ⇒ exactly 1 lockdown row added, whose `preview_request_id` equals the confirmed preview's interaction id even when another preview renders before the execution row lands |
| `LP-15` | 11th preview by one actor inside 10 min ⇒ refused with **zero** Discord GETs; 65th live pending ⇒ `PREVIEW_BUSY`, earlier entries intact; second preview same actor+channel kills the first digest |
| `LP-16` | Lane held by an in-flight channel action at confirm ⇒ existing uncertain reply, pending consumed, no PUT; PUT 403 ⇒ existing no-effect finish; PUT 5xx ⇒ claim/lane retained, no resend |
| `LP-17` | Blank / 513-unit reason, extra option ⇒ refused before reads; category-parent channel renders parent id + sync-not-evaluated line and reads no second channel |

**Offline oracle.** The doctest below fixes the bit arithmetic, the projection
ordering, digest sensitivity and serialization, and the two boundary equalities
(expiry, custom-id length). It proves nothing about Discord, SQL or the runtime;
the future Rust planner must reproduce these results. From the repository root:

```sh
python3 -m doctest -v docs/lockdown-preview-spec.md
python3 scripts/check_docs_index.py --json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
```

```pycon
>>> import hashlib, json, pathlib, re, unicodedata
>>> SEND, REACT, PUB, PRIV, TSEND = 1<<11, 1<<6, 1<<35, 1<<36, 1<<38
>>> VIEW, ADMIN = 1<<10, 1<<3
>>> LOCK = SEND | REACT | PUB | PRIV | TSEND
>>> assert LOCK == 377957124160 and bin(LOCK).count("1") == 5
>>> INTEREST = LOCK | VIEW | ADMIN
>>> def plan(prior):  # prior: (allow, deny) or None, mirroring plan_lockdown
...     allow, deny = prior or (0, 0)
...     return (allow & ~LOCK, deny | LOCK)
>>> assert plan(None) == (0, LOCK)                                   # LP-01
>>> assert plan((VIEW | 1<<14, 1<<13)) == (VIEW | 1<<14, 1<<13 | LOCK)  # LP-02
>>> assert plan((SEND, SEND)) == (0, LOCK)                           # LP-03
>>> def usable(base, everyone_ow, role_ow, bit):
...     if base & ADMIN: return True
...     p = base
...     if everyone_ow: p = (p & ~everyone_ow[1]) | everyone_ow[0]
...     if role_ow: p = (p & ~role_ow[1]) | role_ow[0]
...     return bool(p & VIEW and p & bit)
>>> planned = plan((0, 0))
>>> # @everyone-only member loses usable send; a role overwrite allow keeps it (LP-04)
>>> assert usable(VIEW | SEND, (0, 0), None, SEND) and not usable(VIEW | SEND, planned, None, SEND)
>>> assert usable(VIEW | SEND, planned, (SEND, 0), SEND)
>>> # administrator bypasses; missing view already gates send off (LP-05)
>>> assert usable(ADMIN, planned, None, SEND) and not usable(SEND, (0, 0), None, SEND)
>>> # digest: canonical JSON array, decimals as strings, interest-masked (section 5)
>>> def digest(snap):
...     return hashlib.sha256(json.dumps(snap, separators=(",", ":")).encode()).hexdigest()
>>> snap = ["lockdown-preview.v1", "10", "20", "0", "0", "30", "raid", "1", "0", "0",
...         "0", str(LOCK), "0", "", [["10", "0", str((VIEW | SEND) & INTEREST)]],
...         [["0", "40", str(SEND), "0"]]]
>>> same = json.loads(json.dumps(snap)); assert digest(snap) == digest(same)
>>> stale_role = json.loads(json.dumps(snap)); stale_role[14][0][2] = str(VIEW & INTEREST)
>>> assert digest(stale_role) != digest(snap)                        # LP-09 sensitivity
>>> stale_unrelated = json.loads(json.dumps(snap)); stale_unrelated[8] = str(VIEW)
>>> assert digest(stale_unrelated) != digest(snap)                   # LP-09 full masks
>>> # non-ASCII reason: escaped ASCII bytes, UTF-8, deterministic; no normalization
>>> uni = json.loads(json.dumps(snap)); uni[6] = "raid 🛡️"
>>> raw = json.dumps(uni, separators=(",", ":")).encode()
>>> assert raw.decode("ascii") and "🛡️".encode() not in raw
>>> assert digest(uni) != digest(snap) and digest(uni) == digest(json.loads(json.dumps(uni)))
>>> nfc, nfd = "café", unicodedata.normalize("NFD", "café")
>>> ra = json.loads(json.dumps(snap)); ra[6] = nfc
>>> rb = json.loads(json.dumps(snap)); rb[6] = nfd
>>> assert nfc != nfd and digest(ra) != digest(rb)
>>> # expiry equality refuses (LP-11); button id bound (section 3 step 3)
>>> expires = 1_800_000_300
>>> assert (expires >= expires) and not (expires - 1 >= expires)
>>> cid = f"lockdown-preview:confirm:{'9'*20}:{'a'*32}"
>>> assert len(cid) == 78 and len(cid) <= 100
>>> rows = re.findall(r"^\| `(LP-\d{2})` ", pathlib.Path("docs/lockdown-preview-spec.md").read_text(), re.M)
>>> assert rows == [f"LP-{n:02}" for n in range(1, 18)] # 17 planned cases
>>> print("lockdown preview oracle: bit, projection, digest and boundary arithmetic only")
lockdown preview oracle: bit, projection, digest and boundary arithmetic only

```

**Sign-off path (recorded here; none of it has happened).** Authorship of this
proposal is not independent approval of its own boundary request, and the
security author must not sign off their own spec. Before any implementation or
protected-test work exists:

1. Freeze the exact commit and SHA-256 of this file in the review request.
2. Route the decision brief through the President/COO to the CEO and an
   independent security decision-maker who is not the author; never directly
   to the owner. A routing receipt, merged docs or silence is not acceptance,
   and no timeout auto-approves.
3. The packet must separately accept, change or reject: B over A/C/D/E; the
   same-`ManageChannels` disclosure basis; one new slash command plus one
   component path reusing the existing mutation pipeline; the new roles/
   snapshot reads failing closed; numbers-only preview audit rows plus one
   `preview_request_id` on confirmed execution rows in the shared ledger;
   300-second in-memory retention of the pending reason and preview id;
   single-role projection labelling (never member truth); unverified ephemeral
   message retention on Discord's side; direct `/lockdown` remaining
   unpreviewed. No generic "preview approved".
4. Record guild-host intent alignment against that exact revision through the
   existing host-intent path. A material revision invalidates earlier
   acceptance and needs a fresh request.

Only after those records, cut dependency-linked slices of at most one PR each
(none created here): (1) independent protected oracles for LP-01…LP-17;
(2) pure core snapshot/projection/digest planner reproducing the oracle above;
(3) executor snapshot + roles reads with strict validation; (4) preview slash
with pending store, audit row and rate limiter, still unpublished;
(5) confirm/cancel components through the existing mutation path;
(6) registry/help/docs publication and synthetic staging evidence. Each slice
keeps the feature dark and unpublished until (6) passes its own gates; merged
foundations grant no reads. Ordinary CI and independent review still apply per
slice.

By 2026-11-02 the metric is **all 17 matrix rows and every packet item above
have an explicit accepted/changed/rejected disposition, or B stays no-build**.
That measures design completeness, not moderator demand, which is unmeasured.

ACT: publish this proposed contract and its offline oracle for review. WHY:
fix what a lockdown preview must show, when it must refuse as stale, how it is
bounded and audited, and who may approve it — before any build is considered.
UNDO: revert the reviewed docs; there is no operational change to undo. A later
accepted build must stay dark until its own tests, security and release gates
pass. No production test, migration, credential or activation here.
