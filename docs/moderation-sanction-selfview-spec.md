# Member sanction self-view with redacted attribution

Status: **proposed; design only**. Written 2026-10-26; review by **2026-11-02**.
No command, store seam, migration, Discord read, audit write, deployment or
activation is added. This file is the authoritative editable repository spec;
decision records bind its exact commit and SHA-256. Merge is not acceptance,
host alignment, build approval or activation.

## 1. Decision and alternatives

A sanctioned member currently learns about warnings, timeouts, kicks and bans
only from notification messages they may have missed, muted or deleted. This
spec asks whether the member should be able to privately re-read their own
sanctions, with staff identities always redacted. Member demand is
**unproven**: no outreach, survey or live audit ran. Comparable bots show a
member their own infractions; that establishes a common workflow, not TWO
demand, safe redaction or intent.

| Option | Member effect and boundary | Recommendation |
| --- | --- | --- |
| A. No build; notifications plus ask-staff | Nothing new is retained, shown or rate-limited. A member who missed a notice asks a moderator; staff tooling is unchanged | **Standing fallback** until acceptance; choose it if value, redaction or appeal-linkup fails |
| B. Own-only private read (this spec) | One private command re-reads the invoker's own active plus recent sanctions with reasons and expiry; staff shown only as a fixed team label; no other-member data; bounded rate | **Smallest useful candidate**, only after the gates in §14; otherwise A |
| C. Self-view with staff identities | Shows named or Snowflake-attributed moderators to sanctioned members | **Rejected**: retaliation and harassment risk with no member value; identities are never needed to understand one's own sanction |
| D. Proactive re-notify / DM push | Re-sends missed notices instead of offering a read | **Rejected for this slice**: closed DMs, spam surface and rate limits need their own design; not a hidden fallback |
| E. Shared or public record | Sanctions visible beyond the member and staff | **Rejected**: shaming, privacy and harassment harm; never reconsider under this spec |

No-build (A) keeps today's behavior exactly: one-shot notifications, staff as
the only re-read path, no new retained reads, no new rate state. Choose A at
the review date unless host intent names B's use case, the redaction contract
in §5 is accepted as specified, and the appeal-pointer dependency in §7 has a
credible landing plan. A silent review, a partial acceptance or an appeal
design that never lands all mean A, not a weakened B.

## 2. Source-backed baseline

Inspected base: `c6e5213b516d5ef62b357cc2f5feeaf8f59889a8`. These are source
facts, not deployed-state or demand measurements.

| Seam | Evidence and constraint |
| --- | --- |
| Warnings | [`0110`](crates/cutover/migrations/0110_moderation_member.sql#L18-L28): `id, guild_id, user_id, actor_id, reason, request_id, created_at`. No expiry, no display flag, no redaction column. Indexed by `(guild_id, user_id, created_at)`. Warn never touches Discord ([execute](crates/core/src/member_moderation.rs#L1455-L1468)) |
| Ban ownership | [`0111`](crates/cutover/migrations/0111_moderation_ban_ownership.sql) plus [domain](docs/member-moderation.md): prepared/accepted/rejected intents with durable generations; only the newest accepted generation owns the member; older undispatched expiries supersede on confirmation |
| Tempban expiry | [Staging](crates/core/src/member_moderation.rs#L1406-L1440) persists `execute_at` with the prepared intent before Discord; [schema](crates/cutover/migrations/0110_moderation_member.sql#L30-L49) carries `staged/pending/running/done/superseded/cancelled/quarantined` states. Tempban bounds are 60 s–365 d ([domain](docs/member-moderation.md)) |
| Kick/timeout | Discord mutation only; no durable rows besides the audit ([execute](crates/core/src/member_moderation.rs#L1441-L1454)). The timeout `until` is passed to Discord and never stored, so **no stored "active timeout" fact exists** |
| Audit | All five verbs write `moderation_audit` (actor, action, target, reason, outcome, metadata): bans at the PUT boundary, others post-effect ([service](crates/core/src/member_moderation.rs#L1352-L1360)). Indexed by `(guild_id, target_id, created_at)` ([schema](crates/cutover/migrations/0110_moderation_member.sql#L66-L67)) |
| Audit outcomes | Accepted outcomes are `banned`, `temporarily_banned`, `kicked`, `timed_out`, `warned`, `unbanned` ([vocabulary](crates/core/src/member_moderation.rs#L92-L99)). Refusals write `denied` on `{request}:denied` ([refusal](crates/core/src/member_moderation.rs#L1132-L1152)); failures write `{request}:{outcome}` with `failed` or `in_progress` ([failure](crates/core/src/member_moderation.rs#L1156-L1176)); sweep failures write `unban_failed` ([sweep](crates/core/src/member_moderation.rs#L1639-L1658)); `ban_reconcile`/`accepted_historical` is operator reconciliation evidence, not a member event ([reconcile](crates/core/src/member_moderation.rs#L441-L460)) |
| Self-serve precedent | `/privacy` is invoker-only with no target option, ephemeral completion, a fixed `member:self-serve` actor label and a member cooldown ([handler](crates/bot/src/privacy_command.rs#L1-L21), [label](crates/bot/src/privacy_command.rs#L38-L41), [cooldown](crates/bot/src/privacy_command.rs#L52-L57)) |
| Reply privacy | Visibility is fixed at the initial ACK, never at edit ([lifecycle](docs/interaction-replies.md)). A private read must defer ephemeral, not repair a public placeholder |
| Actor budget | Member verbs share a process-local 5 s per-actor cooldown, 1024 actors, monotonic clock ([domain](docs/member-moderation.md)). Malformed invocations never consume it |
| Registry ceiling | Guild limit is 100 merged commands; builtins can shadow custom rows ([registry](crates/core/src/commands.rs#L24-L24), [merge](crates/core/src/commands.rs#L410-L410)). A new name must prove no collision and no silent drop |
| Erasure | The ordered manifest deletes all six moderation tables for the subject (`moderation_warnings`, `moderation_audit`, `moderation_member_bans`, `moderation_scheduled_unbans`, plus channel/idempotency rows) ([manifest](crates/cutover/src/member_erasure_plan.json)). Analytics opt-out explicitly excludes moderation rows ([privacy](docs/privacy.md)) |
| Appeal | A case-insensitive search for "appeal" across `crates/` and `docs/` at the base returns no implementation, store or spec. Appeal states, entry points and content rules are owned by the separate appeal design, not this spec |
| Staff history | No member-history read path exists in-repo at the base. The staff warning-history read is a separate design; this spec grants no staff capability and reads no staff surface |

Do not weaken any cited control to fit B. In particular: no new Discord REST
read is assumed (§4), no warning expiry is invented (§8), and no appeal state
is guessed (§7).

## 3. Conditional B: proposed read surface

Everything below is a **future acceptance contract**, not an available command
or API. No build or behavior-test card before the gates in §14.

- **Entry:** one new guild slash command, candidate name `/my-sanctions`, with
  no options and no target. The subject is always the invoker, exactly like
  `/privacy`. Unknown options refuse before any read. The name is a proposal:
  the build must prove registry fit (100-command ceiling, no custom shadowing)
  or stop; renaming without a fresh collision proof is not permitted.
- **Content:** active ledger sanctions of any age plus the invoker's sanction
  events from the trailing 365 days, newest first, at most 25 rows plus the
  exact truncation receipt in §10. Per row: sanction type, reason as inert
  plain text (§5, truncated to the per-reason share in Reply bounds),
  issued-at timestamp, and one terminal-or-live state word: `active`
  (permanent bans), `active until <expiry>`, `expired`, `completed`,
  `superseded`, or `recorded` (warnings, kicks and timeouts are events; only
  tempbans carry a stored expiry). Eligibility is closed: only the per-verb
  sources below render, and only exact allowlisted audit pairs. Refused,
  failed, prepared, rejected, reconciled and uncertain records never render,
  so a denied kick or a never-accepted ban intent can never appear as a
  sanction.
- **Source of truth (per verb; dedup key `request_id`):** warns come from
  the `moderation_warnings` row; the same-request audit pair
  (`moderation.warn`, `warned`) corroborates but never creates a second row.
  Kicks and timeouts come from the audit pairs (`moderation.kick`, `kicked`)
  and (`moderation.timeout`, `timed_out`) only. Bans and tempbans come from
  the newest accepted ledger generation plus its schedule; prepared and
  rejected generations never display, older accepted generations show
  `superseded`, and ban audit pairs corroborate but never create a row
  (ban acceptance is audited at the PUT boundary, before confirmation).
  Excluded audit: `denied`, `failed`, `in_progress` and `unban_failed`
  outcomes, `ban_reconcile`/`accepted_historical` reconciliation rows, and
  every other (action, outcome) pair. Schedule states map to `active until
  <expiry>` (`staged`/`pending`/`running`), `expired` or `completed`
  (`done`; `completed` only with a (`moderation.unban_scheduled`,
  `unbanned`) marker on `{ban_request_id}:unban`), `superseded`
  (`superseded`), or exclusion (`cancelled`/`quarantined`: uncertain or
  never-effective, staff tooling only). Automod is out of scope: it writes no
  member-moderation rows at the base and never appears here.
- **Actor:** the fixed label `Moderation team` on every row. Role-derived
  labels are rejected: historical roles are not stored and live role reads
  would add a new REST surface plus a staff-presence oracle. Snowflakes,
  names and discriminators never appear.
- **Empty state:** "No sanctions on your record in this server in the last 12
  months." An erased or never-sanctioned member sees this same shape; there
  is no tombstone and no "erased" disclosure (§8). When zero rows fall in
  the window but older owned rows exist, the same sentence is followed by
  the exact 0-of-N receipt (`showing 0 of 0 in the last 12 months; N older
  entries are available from a moderator`).
- **Unavailable shape:** DM, foreign guild, guild-less, non-member and
  lookup-failure invocations share one identical private reply: "This view is
  not available here." No existence flag, count, state word or retry hint that
  differs by cause. Internal diagnostics use bounded reason tokens only.
- **Reply bounds:** ephemeral at ACK, plain text, no embeds, no DMs, no
  follow-ups, at most 1800 UTF-8 bytes of sanction content. Closed
  `allowed_mentions` on every response (empty parse/users/roles,
  `replied_user=false`). Byte budget: labels plus the receipt compose first
  in fixed ASCII formats; the remaining bytes split evenly across rendered
  rows as the per-reason share; each reason truncates on a character
  boundary to its share with a trailing `…` when cut. Reasons arrive up to
  512 UTF-16 units ([cap](docs/member-moderation.md#L20-L22)), so truncation
  is the rule, not the exception. Precedence is fixed: all 25 rows always
  render, and the reply never exceeds 1800 bytes. Truncation never splits a
  code point, so the reply is always valid UTF-8 (SS-F18).
- **No live Discord reads.** The read path uses the authenticated interaction
  context (guild plus invoker, as `/privacy` does) and stored rows only. A
  current ban cannot be re-proven live and a current timeout cannot be
  observed live; the view shows stored truth with the state words above and
  claims nothing more. If that limitation is unacceptable, choose A rather
  than quietly adding REST reads.

## 4. Authority table

| Requester | Context | Result |
| --- | --- | --- |
| Current guild member | Own rows, guild command | Served; ephemeral own-only view |
| Departed, kicked or never-member invoker | Any | Identical unavailable shape; no oracle |
| Banned member | Guild command | Unreachable: banned members cannot invoke guild commands. Their path is the appeal entry, owned by the appeal design (§7) |
| Staff member | Own sanctions | Served exactly as any member; no wider view |
| Anyone asking about another member | Any | No path exists: there is no target option, and supplied IDs are refused, not honored |
| Operator, service or bot identity | Any | Not via this surface; operator reconciliation and staff tooling are unchanged |
| Unknown or unprovable membership | Any | Fail closed with the unavailable shape |

Re-verify the authenticated guild-plus-invoker context on every invocation.
There is no session, cache or pre-authorization: each read stands alone.
Membership proof is the interaction context itself; no separate member fetch
is required or permitted in this read path.

## 5. Redaction and no-other-member-data contract

- **Redaction by non-selection.** The read selects only display columns
  (type, reason, timestamps, ledger state). `actor_id` is never selected, so
  there is no staff identifier in memory to mask, log or leak. Ban-ledger
  rows carry no actor column at all.
- **Fenced queries.** Every statement constrains `(guild_id, user_id)` to the
  authenticated pair on the existing indexes (§2). Cross-member, cross-guild
  and unscoped scans are not expressible in the seam: it takes no target
  parameter.
- **Reason text.** Reasons render as inert plain text with closed mentions.
  Discord mention syntax is deterministically neutralized before display:
  `<@id>`, `<@!id>`, `<@&id>`, `@everyone` and `@here` each become the fixed
  token `[mention removed]`. No ping, no profile link, no role highlight can
  survive. Masked links and bare URLs are defanged just as
  deterministically: `](` becomes `]` + U+200B + `(` and `://` becomes `:`
  + U+200B + `//`, the same zero-width-split precedent as the reply-slot
  sanitizer ([sanitizer](crates/bot/src/member_runtime.rs#L802-L810)).
  Masked links render as literal text and bare URLs never autolink, so no
  clickable link survives. Raw snowflake digit-strings inside prose are **not** detected or
  masked: there is no general personal-data recognizer here (same boundary as
  [privacy](docs/privacy.md)), and guessing would both miss and over-redact.
  Staff guidance on reason wording is out of scope; the structural guarantee
  is own-rows-only plus mention-syntax neutralization.
- **Logs and telemetry.** Reasons, actor identifiers and per-member sanction
  counts never reach logs, traces, metrics labels or error strings. Aggregate
  invocation counters without identity are the only permitted telemetry, the
  same fixed-cause discipline as existing moderation release logging.
- **No persistence of the reply.** The view is composed per invocation and
  discarded. No cache, transcript, export or history of shown content exists
  server-side.

## 6. Read-audit decision (recorded)

**Decision: self-view reads write no moderation-store rows and no access log
identifying the reader.** Identity-free aggregate invocation counters and
telemetry may still move (SS-F15), so the claim is scoped: no audit row, no
per-reader record, no sanction-table write. Unlike an opt-out purge the read
needs no accountability receipt; the purge receipt exists because data was
deleted ([privacy](docs/privacy.md)). Reasons:

1. A per-read log would create a new sensitive dataset (which member checked
   their record, when) with its own retention, erasure and access rules,
   none of which this spec grants.
2. Staff visibility into member self-reads is a new surveillance capability
   ("did they look?") that needs its own intent and its own design. This
   spec does not create it: no staff surface may show whether or when a
   member viewed their sanctions.
3. Observable self-checks chill legitimate re-reads, defeating the purpose of
   the view.
4. Anonymous aggregate invocation counts suffice for capacity and error
   monitoring.

**Who sees self-reads:** nobody. Reads are visible only as identity-free
aggregate command counters. A future abuse case may reopen this decision, but
only with its own retention/erasure/access design and a fresh disposition;
acceptance of this spec never pre-authorizes read logging.

## 7. Appeal-link hookup (interface; states owned elsewhere)

The appeal design owns appeal states, entry points, eligibility and content.
This spec defines only the hookup contract the self-view must satisfy:

- The view may show a per-case **appeal pointer only**: a short status word
  plus how to reach the appeal entry. The status word maps by exact match
  against the appeal design's enumerated states (whatever they are once
  landed); the mapping table ships with the pointer slice, not here.
- Unknown, missing or unmapped appeal state fails closed to the generic
  pointer: "Appeals are handled separately; see this server's appeal
  instructions." No status, eligibility or existence oracle leaks through the
  fallback.
- The view never shows appeal content, threads, staff notes, decision
  reasoning or decision-maker identity. Those belong to the appeal surface
  under its own authority rules.
- **Build dependency:** the pointer slice (§13, S4) is blocked until the
  appeal design enumerates its states. The base sanction list (S1–S3) is
  independent of appeal and carries the generic pointer only.
- Reachability split: members who cannot invoke guild commands (notably
  banned members) must be served by the appeal entry itself. The self-view
  does not duplicate that path and must not claim to.

## 8. Opt-out, retention and purge interplay

- **Analytics opt-out changes nothing.** Moderation rows are explicitly
  outside the opt-out purge ([privacy](docs/privacy.md)); opting out,
  opting in or staying opted out leaves the self-view byte-identical. One
  line in command help says so; routine replies do not editorialize.
- **"Recent" is a display window, not deletion.** Warnings, audit and the ban
  ledger have no age purge (retention covers presence/voice captures, the
  audit-log cache, game aggregates and deletion requests). Rows older than
  the 365-day window persist and stay available to staff tooling; the member
  sees the receipt plus the ask-a-moderator pointer (§3).
- **Erasure empties the view.** Once the ordered manifest commits, every
  covered row is gone and the next read shows the empty state (§3) with no
  tombstone and no disclosure that erasure ran. A pending deletion request
  changes nothing: reads serve pre-commit truth until the commit lands, and
  the deletion-request ledger itself is never shown here.
- **No new retention exception.** This spec retains nothing new: no read log
  (§6), no cache (§5), no tombstone. Backup and restore behavior is
  unchanged; restored rows reappear exactly as any other restored data would.

## 9. State and race table

The read runs as one `READ ONLY` transaction across warnings, the ban ledger
and audit, so a single snapshot answers every row. Snapshot truth, not live
truth, is displayed.

The read captures one clock instant at snapshot start. Every derived state
word compares `execute_at` against that instant, and the 365-day window
bounds derive from it too. Per-row clock reads are not allowed.

| Concurrent event | Read behavior |
| --- | --- |
| Sanction commits before the snapshot | Included in this read |
| Sanction commits after the snapshot | Appears on the next read; this reply is never amended |
| Tempban passes `execute_at` mid-read | Snapshot state word stands; the next read shows `expired` |
| Unban sweep completes or supersedes mid-read | Snapshot state word stands; ledger states converge on re-read |
| New warn lands while composing the reply | Next read; no live insertion |
| Appeal state flips mid-read | Pointer may lag one read; the reply frames status as of just now and points at the appeal entry for current state |
| Erasure commits between snapshot and reply | The reply may show just-deleted own rows. **Accepted residual:** the recipient is the data subject, the content was true at snapshot time, and nothing persists server-side. The next read is empty |
| Concurrent reads by any members | Independent; snapshot reads serialize nothing and take no locks beyond the snapshot |
| Discord-side timeout expiry | Never observed: no live reads (§3). Timeout rows are events with issued-at plus duration, never "active now" claims |

No lock is held across Discord I/O: the snapshot closes before the reply is
composed, and composition touches no database.

## 10. Rate, bound and no-effect rules

- **Admission:** existing router guild/feature/activation gates apply, then
  the member-verb 5 s actor cooldown precedent, then the read-specific
  budget: at most 10 served reads per member per guild per rolling hour,
  process-local like the existing limiter, no durable state. Malformed
  invocations consume neither cooldown nor budget. The 11th read in the hour
  gets a private "try again later" reply with no sanction content and no
  queue position.
- **Query bounds:** fenced indexed scans only, in one `READ ONLY` snapshot:
  the 25 display rows via `LIMIT 26` (25 rows plus the overflow witness),
  plus two bounded `COUNT(*)` queries over the same fenced, deduplicated
  eligible set (§3): the in-window total and the beyond-window total, both
  partitioned by the single captured clock instant. A bounded statement
  timeout applies. No full-history export and no pagination cursor in this
  slice: the exact receipt plus the staff pointer is the whole overflow
  story. Receipt templates: `showing R of T in the last 12 months` (R =
  rendered rows, T = in-window total), with `; O older entries are available
  from a moderator` appended when the beyond-window total O is nonzero.
- **Reply bounds:** §3 (ephemeral at ACK, plain text, ≤1800 bytes, closed
  mentions). A failed reply after a successful read retries nothing: there
  is no mutation to reconcile, and the member simply re-invokes (which
  consumes budget; that cost is disclosed in help, not negotiated per call).
- **Zero writes, proven:** no audit row, no idempotency claim (reads need
  none), no cache insert, no prefetch, no background sweep or queue. The
  acceptance matrix (§12) asserts table counters unchanged across reads;
  identity-free aggregate telemetry is the only permitted movement (SS-F15).

## 11. Offline decision sketch (runnable now, stdlib only)

The sketch below checks the redaction and byte-budget contract fragments and the matrix shape.
It is not production code, store behavior, runtime acceptance or a security
proof. Its functions are policy fragments; the real seam must still satisfy
§5 end to end, including non-selection (which no string function can prove).

```pycon
>>> import json, pathlib, re
>>> MODERATOR_LABEL = "Moderation team"
>>> def display_actor(actor_id):
...     return MODERATOR_LABEL
>>> MENTION_RE = re.compile(r"<@!?\d+>|<@&\d+>|@everyone|@here")
>>> ZWSP = chr(0x200B)
>>> def display_reason(reason):
...     text = MENTION_RE.sub("[mention removed]", reason)
...     return text.replace("](", "]" + ZWSP + "(").replace("://", ":" + ZWSP + "//")
>>> def fit_reason(reason, share):
...     encoded = reason.encode("utf-8")
...     if len(encoded) <= share:
...         return reason
...     return encoded[:max(share - 3, 0)].decode("utf-8", "ignore") + "…"
>>> assert display_actor("123") == display_actor("999") == "Moderation team"
>>> assert display_reason("Slurs toward <@123> and @everyone") == "Slurs toward [mention removed] and [mention removed]"
>>> assert display_reason("Plain reason stays intact.") == "Plain reason stays intact."
>>> link_out = "See [form]" + ZWSP + "(https:" + ZWSP + "//example.invalid/) or https:" + ZWSP + "//example.invalid/"
>>> assert display_reason("See [form](https://example.invalid/) or https://example.invalid/") == link_out
>>> assert fit_reason("x" * 100, 10) == "xxxxxxx…"
>>> clipped = fit_reason("😀" * 10, 10)
>>> assert clipped.endswith("…") and len(clipped.encode("utf-8")) <= 10
>>> text = pathlib.Path("docs/moderation-sanction-selfview-spec.md").read_text()
>>> cases = re.findall(r"^\| (SS-F\d{2}) ", text, re.M)
>>> assert cases == [f"SS-F{i:02}" for i in range(1, 19)], cases
>>> print(json.dumps({"schema": "moderation.selfview.mock.v1", "matrix_rows": len(cases), "outcome": "offline_examples_pass"}))
{"schema": "moderation.selfview.mock.v1", "matrix_rows": 18, "outcome": "offline_examples_pass"}

```

Run now from the repository root:

```sh
python3 -m doctest docs/moderation-sanction-selfview-spec.md -v
python3 scripts/check_docs_index.py --json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
git diff --check
```

These validate discovery, paths, examples and whitespace only, not the
matrix, SQL safety, member value, deployment or authority. No runtime tests
are authored here.

## 12. Must-pass future acceptance matrix

All fixtures below are **planned, not executed runtime tests**. Use synthetic
identities, unique channels per case, an injected clock, independent
disposable test-DB connections and the shared recording Discord double
([transport](crates/discord/tests/common/mod.rs#L368-L368)). For each row
collect bounded result state plus table/audit/message counters; never capture
private text in evidence. Timing bounds are acceptance targets, not SLAs.

| Case | Given / when / then oracle |
| --- | --- |
| SS-F01 | Given an accepted tempban with future `execute_at`, an accepted permanent ban, plus a warning, when the member reads, then one `active until <expiry>` row, one `active` row and one `recorded` row appear with reasons and issued-at; every actor reads `Moderation team`; no snowflake, name or ID appears in the reply |
| SS-F02 | Given warnings across 400 days, when the member reads, then newest-first rows within the trailing 365 days appear with inert reasons; the receipt states exact in-window and beyond-window totals (`showing R of T in the last 12 months; O older entries are available from a moderator`); given only beyond-window rows, the empty-state sentence plus the exact `showing 0 of 0` receipt appears |
| SS-F03 | Given an expired tempban, a superseded schedule and a completed unban, when the member reads, then each shows its terminal state word and none claims `active` |
| SS-F04 | Given kick and timeout audit events, when the member reads, then each shows as an event with issued-at (plus duration for the timeout); no row claims live-active state and no Discord member fetch runs |
| SS-F05 | Given an erased member and a never-sanctioned member, when each reads, then both see the identical empty state; no tombstone, no "erased" disclosure, zero rows |
| SS-F06 | Given a sanctioned member, when they opt out, read, opt in and read again, then all four reads are byte-identical apart from timestamps |
| SS-F07 | Given two sanctioned members A and B, when A reads, then the reply, logs, traces and telemetry contain zero rows, reasons, counts or identifiers belonging to B |
| SS-F08 | Given a reason containing `<@id>`, `<@&id>`, `@everyone`, `@here`, a masked link `[form](https://example.invalid/)` and a bare `https://example.invalid/` URL, when the member reads, then mentions become `[mention removed]` and link/URL syntax renders literally with zero-width splits; the reply carries closed mentions and renders no ping, link or highlight |
| SS-F09 | Given a departed member, a DM, a foreign guild and an injected lookup failure, when each reads, then all four receive the identical unavailable shape with zero writes and no existence signal |
| SS-F10 | Given a staff member with own sanctions, when they read, then they see only their own rows; no target option exists and supplied IDs refuse before any read |
| SS-F11 | Given the hourly budget, when a member reads 10 times then an 11th, then the 11th is privately refused with no sanction content and no queue; malformed taps consume nothing |
| SS-F12 | Given a read snapshot, when a warn commits before it and a ban commits after it, then the reply includes the warn and omits the ban; warnings, ledger and audit agree on the one snapshot |
| SS-F13 | Given expiry, sweep completion and erasure racing a read, when each lands mid-read, then the in-flight reply keeps snapshot truth and the next read converges; post-erasure reads show the empty state |
| SS-F14 | Given appeal states mapped, unmapped and absent, when the member reads, then exact states map, others fall back to the generic pointer, and no appeal content, thread or note ever appears |
| SS-F15 | Given table counters before a burst of reads, when members read, refuse and re-read, then every counter on all six moderation tables plus audit is unchanged; only identity-free aggregate telemetry moves |
| SS-F16 | Given 30 owned rows in window, when the member reads, then 25 newest rows render within 1800 bytes ephemeral-at-ACK with the exact `showing 25 of 30 in the last 12 months` receipt; reasons over the per-reason share truncate on character boundaries with `…`; no cursor or second page exists |
| SS-F17 | Given a denied kick audit, a failed timeout audit, a prepared ban intent, a rejected ban generation and a `ban_reconcile` row for the member alongside one accepted warn, when the member reads, then only the single warn renders; denied, failed, prepared, rejected and reconciled records never appear and the warn renders once (no audit double-count) |
| SS-F18 | Given 25 sanctions with maximum-length 512-unit multibyte reasons, when the member reads, then all 25 rows render within 1800 bytes with the exact `showing 25 of 25 in the last 12 months` receipt; every reason is character-boundary truncated with `…` and the reply is valid UTF-8 |

Reusable harnesses (existence is not evidence of B passing): the recording
transport above, member-moderation DB suites, privacy self-serve suites and
the reply-lifecycle tests. No behavior tests are added or modified here.
Future Test Author owns focused immutable test files plus a JSON path/SHA-256
manifest; every new behavior needs RED against the named baseline plus
unchanged positive/negative controls. Compile, cache or environment failure
is not RED; builders must not edit protected files to obtain GREEN.

## 13. Smallest future slices (cut-ready, ≤1 day each)

Each slice is independently reviewable and shippable to staging-gated dark;
each must fit one day or be cut further, never widened. Order is fixed by
dependency. No slice starts before the gates in §14.

| Slice | One-day scope | Out of scope | Exit (all required) |
| --- | --- | --- | --- |
| S1 read model | Pure shaping/redaction: state words, mention plus link/URL neutralization, per-reason byte budgeting, receipt text, empty/unavailable shapes, byte cap; unit tests only | DB, Discord, command wiring, appeal | All §11 fragments plus edge cases covered; zero I/O in the module |
| S2 store read | One fenced snapshot seam over warnings, ban ledger and audit with the §3 source-of-truth and allowlist; display rows via `LIMIT 26` plus bounded in-window/beyond-window `COUNT(*)`; one captured clock instant; ignored DB tests on disposable schemas | UX, budgets, appeal, migrations (none needed: existing indexes suffice) | Snapshot consistency plus fence proofs; zero-write assertion harness ready |
| S3 command UX | `/my-sanctions` wiring: router gates, ephemeral lifecycle, 10/hour budget, help text, registry collision proof | Appeal pointer (generic fallback only), staff views, pagination | Staging-dark with S1+S2; SS-F01–F13 and SS-F15–F18 green |
| S4 appeal pointer | Exact-state mapping table against landed appeal states plus fallback; mapping tests | Any appeal behavior, content or eligibility change | SS-F14 green; unknown-state fuzz green |
| S5 matrix closeout | Full §12 execution, evidence pack, docs finalization | New behavior of any kind | 18/18 oracles disposed; spec revision bound to results |

S4 is blocked until the appeal design enumerates its states; S1–S3 and S5-minus-F14
proceed without it. Any slice that cannot fit its day, needs a migration, or
weakens a §5/§6/§10 guarantee is cut back to A, not renegotiated mid-build.

## 14. Exact intent and disposition gates

Before **any build or behavior-test card**, bind CEO/host intent alignment and
an independent CISO disposition to the issue-spec revision, repo commit and
this file's SHA-256. A merged design or pending review is not acceptance. Any
spec revision invalidates earlier acceptance; no timeout approval.

- **CEO/host decision:** keep A, or substantiate B's member use case and
  accept the display window, fixed anonymous attribution, mention/link
  neutralization, hourly budget, zero-read-logging (§6) and the appeal
  dependency (§7). Acceptance of the list is not acceptance of the pointer:
  S4 needs the appeal states plus a second intent confirmation.
- **CISO disposition:** non-selection redaction, fenced-query shape,
  non-member/DM/foreign oracle parity, residual erasure-race disclosure
  (§9), no-read-logging plus no-staff-visibility (§6), telemetry label
  discipline, backup/restore reappearance, and frozen private copy. This
  spec is authored by the CISO role as designer; authorship is not
  disposition. The gate needs a recorded disposition pass against the exact
  revision, and this author's analysis satisfies none of it.
- **Kill rule:** missing value, intent, disposition or bounded implementation
  at the 2026-11-02 review means **A/no build**, not automatic activation.
  Acceptance of the spec alone never creates a build task.

Unverified: live guild objects and permissions, member demand, runtime
behavior of B (absent), appeal states and entry points, usable private UI.
No production test, migration, private-data collection, new service or spend
occurred.

ACT: publish one indexed proposal and a matching task spec. WHY: let members
re-read their own sanctions privately without exposing staff or other
members. UNDO: revert docs; there is no operational state to roll back.
