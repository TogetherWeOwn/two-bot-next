# Dashboard scheduled-post parity contract

Status: **Deferred to the two-web-next roadmap** (owner decision 2026-11-01).
Build the bot half only when a two-web-next goal claims this page. Original
proposal: written 2026-10-26. No browser code, endpoint, migration, grant, build
card, deployment or activation. This is the repository source for the proposal
and its offline document checks. Inventory baseline:
`cdf33be676858f72f5874c000cd0e63efda6a02a`. Source-shipped below means present
at that commit, **not verified deployed**.

## Decision and alternatives

An organizer needs to find a saved announcement, understand its timing and remove
future runs without mistaking configuration for delivery. Recommend **bot-contract
first**, not an editable page over SQL or `/ops/config`. There is no accepted
schedule dashboard transport at this baseline. Web pages, session guards and
signer implementation were not inspected; their existence is unverified, not
asserted absent. No second repository is needed for this design.

- **No-build:** retain private `/schedule`, `/schedule-list [page]` and
  `/schedule-remove id:<id>` under their existing gates. Record the cancellation
  caveat below; do not use remove/recreate to recover an uncertain send.
- **Rejected shortcut:** direct website DB writes or `put_scheduled` as an editor.
  Replacement clears claim, nonce and delivery-fence evidence. A read followed by
  an unconditional write is not compare-and-swap (CAS).
- **Conditional minimum page:** private bounded list, one authorized detail,
  create-form review and exact-target removal. No calendar grid, bulk action,
  analytics, member feed, destination move, catch-up, cron or new recurrence.
  Even these controls stay unavailable until accepted native contracts exist.

## Present versus proposed capabilities

| ID | Source-shipped baseline / evidence | Dashboard disposition |
| --- | --- | --- |
| `SP-C1` create | `/schedule`: fresh ID, invocation channel, enabled definition, original creator/time (`crates/bot/src/schedule_runtime.rs:75-192`). ID generator uses 16 random bytes as 32 hex characters (`crates/bot/src/command_runtime.rs:2690-2694`). | Proposed transport only; no existing schedule create wire verb or idempotent command replay. Save means stored, never posted. |
| `SP-C2` list | Guild-wide, including disabled rows; ordered `(next_run_at,id)`, first 25 plus one hidden 25-row window and overflow count (`crates/core/src/scheduled_store.rs:152-214`; `crates/bot/src/schedule_runtime.rs:383-590`). | Proposed private summary read; preserve bounds and incomplete-snapshot labels. Current formatter receives enabled/time, **not claim/fence facts** (`schedule_runtime.rs:403-412,619-630`). |
| `SP-C3` remove | Literal case-sensitive same-guild prefix resolves at most two IDs, then physical guild-scoped DELETE (`scheduled_store.rs:119-149,441-451`); current success says “Cancelled” plus ID (`schedule_runtime.rs:315-333`). | Proposed exact full-ID confirmation and guarded removal; no recall guarantee. Current command has no expected revision or confirmation transaction. |
| `SP-C4` detail/body edit | `get_scheduled` is a store function, not a dashboard endpoint (`scheduled_store.rs:100-116`). Saved inspect/body-only edit remains open [PR #1722](https://github.com/TogetherWeOwn/two-bot-next/pull/1722), head `fafcf56ba18a2308bd28949651cb982fb3b240ac` at inventory. | Pending separate owner; do not claim inspect/edit shipped or copy its branch. Read contract must project facts safely; editing waits for that work plus CAS. |
| `SP-C5` retime | No retime operation in baseline; fresh create is not retime. | Pending separately owned never-run one-off work; no page control or new allocation here. |
| `SP-C6` preview | No create preview option in baseline. | Pending separately owned pure proposed-post preview; local form review below is not bot-equivalent preview. |
| `SP-C7` pause/resume | Only enabled bit exists; completed one-shots also disable (`scheduled_store.rs:382-438`). | Separate Proposed lifecycle design, not an available toggle. Never infer intentionally paused from disabled. |
| `SP-C8` list/receipt truth | Claim/fence projection and honest remove copy are not baseline behavior. [PR #1778](https://github.com/TogetherWeOwn/two-bot-next/pull/1778) is pending acceptance work, not delivered implementation. | Separately owned work; consume its eventual accepted contract, do not duplicate or implement it here. |

Re-inventory at a future build head: an open PR or approved design is not merged
code, and merged code is not deployment evidence. Internal ownership links live
in the task document, not public repository text. Related baseline guide:
[queue order and explicit-time commands](schedule-queue-order.md).

### Storage and system boundaries

`scheduled_messages` stores definitions, not Discord scheduled events. Its global
TEXT primary key is not a snowflake or schema-enforced UUID. Preserve the full ID
and trusted guild as a composite control target; never reuse an ID after deletion.
Schema includes body, destination, enabled, queue deadline, interval, creator/
updater/time, last-run/message and internal delivery fields
(`crates/cutover/migrations/0140_scheduled_messages.sql:20-41`). The fence is nullable
in SQL; reads conservatively project NULL as true
(`crates/cutover/migrations/0142_scheduled_delivery_uncertainty.sql:4-11`;
`scheduled_store.rs:107-111`). No definition revision, original-due-time column,
timezone profile or deletion tombstone exists. `updated_at` is not CAS.

Do not substitute these adjacent systems:

- Worker `/ops/config` changes process overrides with its own epoch; it does not
  manage schedule rows (`wrangler/src/ops-control.ts:436-470`). Its control epoch
  is not a schedule revision. `/ops/audit` reads `operational_audit_log`, not
  schedule `automation_audit_log` (`crates/bot/src/ops_http.rs:502-520`).
- Discord `scheduled_events` is a separate polled mirror, not `/schedule` data
  (`crates/core/src/scheduled_events.rs:1-38`). RSVP reminders consume that mirror
  plus `event_rsvps` with a fixed 15-minute lead and their own delivery fence
  (`crates/core/src/event_reminder_store.rs:15-18,38-106`). Sharing a ticker or REST
  client does not make an event/RSVP/reminder a scheduled-post definition.
- Implemented internal-action allowlist has no `schedule.*` verbs
  (`crates/core/src/internal_actions.rs:674-694`). Settings get/set, automation
  command import/export and admin-registry foundations do not supply this API.

## Authority, executor, audit and visibility

All current schedule commands require configured guild, automations capability
and server-side Manage Guild; list/remove are guild-wide, not creator-only or
invocation-channel-only (`crates/core/src/router.rs:709-801`;
`crates/core/src/command_permissions.rs:46-55,170-172`). Private acknowledgement
precedes DB work (`crates/bot/src/command_runtime.rs:2455-2475`). The admitted
scheduled worker, not the browser or settings poller, is the actual consumer.
Its 15 s cadence, 120 s attempt timeout and ten-claim batch are supervision bounds,
not a delivery SLA (`crates/bot/src/scheduled_jobs.rs:87-118,185-213`). Boot identity
and automation gates remain authoritative; an enabled row does not activate them.

| Proposed control | Data / executor / validation | Audit and authority consequence |
| --- | --- | --- |
| List | Bot-owned projection of same-guild rows; no bodies, actors, claims or nonces in summary. 25 rows/window, at most two windows; time/state rules below. | Manage Server on every read, private/no-store. Current successful list has no audit; failed read audits rejected. No schedule DML. |
| Detail | One exact guild/ID; summary plus separate authorized full body, interval and available attribution/last-run facts. Never expose raw row serialization. | Same management authority, not public channel permission alone. Current store read is not an authorized transport. Audit-reader access is not implied. |
| Create review/save | Existing strict decoder and body/time validators, invocation-equivalent trusted target channel. Native bot service allocates ID and admits quota. | Current create audit follows storage; not transactional with it. A new wire mutator needs atomic content-free audit and per-intent idempotency, not an invented exactly-once claim about slash. |
| Remove | Server resolves and displays exact full ID/guild/channel, rereads state; accepted store transaction checks incarnation/revision and state snapshot before deleting. No message DELETE by the page. | Current prefix/delete audit is separate and best-effort. Future confirmed control receipt must bind exact target and committed result; stale/unknown is not success. |
| Edit/retime/preview/pause | Disabled links explaining separately owned missing contract; no call, SQL, signing, audit or dispatch. | No new role, key, scope or inherited permission from these proposals. |

Current command audits record scheduled.create/update/delete and failure reasons;
audit failure warns without undoing storage (`schedule_runtime.rs:101-247,315-364,693-720`).
Worker attempts record scheduled.run with system actor NULL, IDs/outcomes/reasons,
never body (`scheduled_jobs.rs:632-655`). Last-run metadata may represent a permanent
no-effect refusal; it is **not** a successful-delivery receipt. Even a recorded
message ID is historical evidence, not proof the message still exists.

**Future web prerequisite, unverified here:** current session membership and Manage
Server on reads and writes, trusted deployment/selected-guild match, server-derived
actor, CSRF protection, revocation handling, private/no-store responses and signing
keys only server-side. HMAC service identity is not proof of admin authority.
Missing/foreign IDs return the same safe miss after authorization, with no foreign
body/channel oracle. Access loss clears private body/draft from the view and stops
requests; no durable localStorage/offline write queue. Text is inert/escaped, with
no mentions, HTML execution or external link fetching. Never log bodies, claims,
nonces, signatures, raw DB/Discord errors or credentials. New transport scopes and
body exposure require relevant CISO disposition; do not widen current gates.

### Read bounds and identity

The baseline list is multiple reads, **not a consistent snapshot**. Page 1 is the
default; reject invalid/duplicate/noninteger/underflow page values. Text packing
uses whole rows within 2000 UTF-16 units. A full first window counts once; hidden
pages read only offset 25/limit 25. Beyond 50 remains explicitly truncated, not
“all schedules.” Failed count can show unnumbered truncation in the first window;
a deep page with unknown count or failed hidden read is unavailable, not empty or
out-of-range. New quota is 25 including disabled/completed; restored excess can
exist and still be claimed. A count is not permission to fetch unbounded rows.

Proposed JSON summary adds at most 25 items and 32 KiB/response, detail at most one
record and 16 KiB. Body is separate and complete, at most 8192 UTF-8 bytes; an
over-bound/invalid restored value reports unsupported data, never silent clipping.
Supported target IDs are nonempty and at most 128 UTF-16 units like current removal;
schema has no such ID bound, so legacy excess must refuse safely, never crop identity.
Explicit exact-ID detail may locate a row outside the visible windows; no search,
export or arbitrary cursor is assumed. Read failure is unknown; successful zero
rows alone means empty. Preserve stale metadata with observation time and disable
mutations until fresh supported reads; never preserve private content after access loss.

**Missing atomic contracts:** opaque durable definition revision/incarnation across
all definition writers (including slash, any import, replacement and deletion),
plus a delivery-state precondition checked with the mutation. Worker claims need
not change a definition revision, so checking revision alone is insufficient.
A clean snapshot becoming claimed/fenced must conflict or refuse atomically.
Delete/recreate ABA must not accept the old token. Current claim token is internal
worker ownership, never a client revision. Current get-then-delete/upsert, timestamps,
client hashes and a UI refresh do not implement CAS. Keep mutation unavailable if
these protections or the accepted bot transport are absent; do not invent a live
409 endpoint. `STALE` below is a Proposed result, not current wire behavior.

## Validation and honest time

Reuse executable validators, not the stale module-header “exactly one relative
option” comment (`crates/core/src/scheduled.rs:205-256`):

- Body: sendable text, 1–2000 **UTF-16 units**; whitespace/invisible-only refuses.
  Neutralize mass mentions and Discord invite hosts before storage; recheck length
  after expansion. Legacy delivery neutralizes and surrogate-safely truncates to
  2000, but create refuses overflow (`scheduled.rs:129-149,208-249`;
  `scheduled_jobs.rs:404-416`). SQL character bounds alone are not this policy.
- `in-minutes`: integer 1–525600. `every-minutes`: integer 60–525600. Both may be
  supplied: first delay uses in-minutes, recurrence uses every-minutes. Only every
  means first delay equals interval; neither relative option nor at is an error.
  Store interval allows 60–31536000 **seconds**; do not relabel legacy sub-hour
  rows invalid simply because slash creation has an hourly minimum.
- `at`: one-off only, ASCII RFC3339 at most 64 bytes, explicit Z/numeric offset.
  Cannot combine with either relative option. Normalize to UTC milliseconds,
  truncate sub-ms, then require persisted instant strictly after sampled now and
  at most 365 days ahead (`scheduled.rs:108-126`; `schedule_runtime.rs:633-671`).
  Duplicate known options/wrong types refuse; unknown option names are ignored by
  today's decoder, not a guarantee that a hypothetical dry_run flag is safe.
- Storage is fixed UTC `YYYY-MM-DDTHH:MM:SS.mmmZ`; original offset/input mode/zone
  is not retained. Show UTC and a labeled viewer-local rendering with offset/zone.
  No local-time-only or natural-language input. DST gaps/folds cannot choose an
  instant for the user: require an explicit offset. Recurrence is elapsed seconds,
  not “same local hour daily”; localized clock time may shift across DST.

Create freezes one now for calculation; a later save must revalidate against its
own authoritative now. A deterministic local review freezes the supplied now too,
but cannot reserve a deadline, quota, channel access or delivery. No implemented
server dry-run is assumed. **Local dry-run is pure shape/intent review only:** same
input/now -> same output, no network, signature, nonce, ID allocation, DB, quota,
audit, claim or Discord effects. Unknown modes fail closed. Do not append
`dry_run:true` to an existing mutator and hope it prevents writes. Bot-equivalent
preview belongs to its pending owner.

### Worker facts and human states

Claim moves `next_run_at` to now+60 s and retains occurrence identity/fence on
reclaim (`scheduled_store.rs:217-281`). A fence is armed **before POST**. It may
mean an in-flight send, a completed send awaiting settlement or ambiguous delivery,
not “failed.” Lease expiry proves neither a dead worker nor non-delivery. Confirmed
completion clears delivery fields; one-shot disables, recurrence advances from the
worker's pre-send sampled run instant, not original due or response time. No backlog
of missed slots is replayed. Proven no-effect retry uses the current 30 s default;
uncertain outcomes retain the fence and are not automatically resent
(`scheduled_jobs.rs:366-623`; `scheduled_store.rs:284-310,382-438`).

Proposed state precedence after authorized read: unavailable/corrupt -> unknown;
confirmed absence -> deleted; otherwise delivery-fenced > claimed > disabled >
due > saved. Keep enabled/disabled as an independent visible flag so restored
mixed evidence is not hidden. An inconsistent partial claim tuple is unknown,
not clean. Known claim/fence alerts remain visible even when other data is corrupt.
NULL fence is fenced. Disabled is never automatically called paused, completed or
successfully delivered. Absence without a prior observation or confirmed delete is
NOT_FOUND, not evidence that a definition was deleted.

| State / reason | Private human copy and next action |
| --- | --- |
| saved / `SAVED_UNVERIFIED` | “Saved definition. Delivery not confirmed.” Fresh unclaimed enabled deadline in future; review or remove when supported. |
| due / `DUE_SNAPSHOT` | “Eligible as of this read; delivery may be delayed.” Deadline <= observed now, not an exact-second promise; refresh read. |
| disabled / `DISABLED` | “Disabled; no new claim while disabled.” Retain last-run facts, no ordinary pending-delivery hint and no Resume button. |
| claimed / `CLAIMED_SNAPSHOT` | “Occurrence claimed at last read; a send may be in flight.” Claim does not prove an active worker. Display queue value as lease horizon, not next delivery. Inspect before change. |
| delivery-fenced / `DELIVERY_FENCED` | “Delivery may have started or occurred. No automatic resend.” Display queue value only as lease/retry deadline; inspect channel through existing authority, never clear/retry/recreate. |
| unknown / `UNAVAILABLE` | “State could not be confirmed.” Disable changes; retry read, not write. Invalid data is `STATE_INVALID`, not absence. |
| deleted / `DEFINITION_REMOVED` | “Definition removed; future runs removed. An already-admitted delivery may still appear.” Confirmed delete/absence only, not a durable tombstone or proof of message absence. |

State text, UTC/local labels and observation time are accessible names, not color
alone. Keyboard-accessible list/detail and separate Review/Save/Remove controls;
field-linked errors and an error summary preserve the draft on ordinary conflict.
No optimistic success toast, timer countdown guaranteeing delivery, or auto-submit.
Page footer names truncation/continuation and never implies a hidden window is empty.

### Exact-target removal, including POST races

Confirm full guild, schedule ID, destination, available recurrence/state and observed
revision. No prefix-only/bulk confirmation; canonical ID stays copyable. Cancel
sends nothing. Recheck authority and atomic preconditions on confirm. A stale
revision/state keeps draft/selection and requires reload plus a new confirmation;
never silently delete a different incarnation. Ambiguous commit is `COMMIT_UNKNOWN`:
reconcile the same intent, do not issue a fresh key to force another removal.

Removal is a definition delete, **not message recall**. If it wins before
begin_scheduled_delivery arms the fence, the stale worker cannot authorize POST.
After arming, POST may still happen even if deletion commits first. On confirmed
send plus lost completion ownership the worker attempts orphan cleanup; cleanup
may fail and leave the message present (`scheduled_jobs.rs:458-480`). Unknown send
without usable receipt cannot promise cleanup. Do not claim “will not post,”
“message removed,” “cancelled delivery,” or “channel clear.” Removing a fenced row
is not uncertainty recovery; the future adapter must refuse unresolved evidence
with `DELIVERY_FENCED`, not silently erase it. This stricter Proposed refusal does
not describe today's unconditional DELETE. A clean removal can still race an
admitted send unless the accepted atomic state contract prevents admission; retain
the conservative warning even then. Page offers no retry, force-clear or cleanup
button. Recreating a deleted schedule is new intent/identity, not undo or resend.

## Bounded machine handoff and deterministic design matrix

The following JSON is a **Proposed document contract**, not verbs, routes or a
runtime implementation. All IDs/revisions are strings; revision null forbids a
mutation. Per-intent durable idempotency must bind exact guild/ID/revision/state/
body before any wire mutator is exposed. Every later retry with the same key and
exact body returns the original receipt; the mutation occurs once. Changed bytes/
target refuse; unknown commit reconciles with the same intent.
Do not confuse nonce replay protection, command-generated IDs or internal-action
idempotency errors with schedule CAS. No claim token/nonce/body appears in summaries.

```json
{
  "schema":"dashboard.scheduled_posts.v1",
  "status":"proposed",
  "source_sha":"cdf33be676858f72f5874c000cd0e63efda6a02a",
  "native_schedule_transport":false,
  "bounds":{"rows_per_window":25,"windows":2,"summary_bytes":32768,"detail_bytes":16384,"body_bytes":8192,"id_utf16":128},
  "timing":{"in_minutes":[1,525600],"every_minutes":[60,525600],"at_bytes":64,"body_utf16":2000,"ticker_ms":15000,"lease_ms":60000,"batch":10},
  "states":["saved","due","disabled","claimed","delivery-fenced","unknown","deleted"],
  "envelope":["schema","source_sha","guild_id","schedule_id","definition_revision","observed_at","state","enabled","queue_at_utc","queue_kind","reason","retryable","next_action"],
  "reasons":{
    "UNSUPPORTED":{"retryable":false,"next_action":"use_existing_controls"},
    "SAVED_UNVERIFIED":{"retryable":false,"next_action":"review_definition"},
    "DUE_SNAPSHOT":{"retryable":false,"next_action":"refresh_read"},
    "DISABLED":{"retryable":false,"next_action":"inspect_definition"},
    "CLAIMED_SNAPSHOT":{"retryable":false,"next_action":"inspect_before_change"},
    "DELIVERY_FENCED":{"retryable":false,"next_action":"inspect_channel_no_resend"},
    "UNAVAILABLE":{"retryable":true,"next_action":"retry_read"},
    "STATE_INVALID":{"retryable":false,"next_action":"authorized_inspection"},
    "DEFINITION_REMOVED":{"retryable":false,"next_action":"inspect_channel_if_needed"},
    "STALE":{"retryable":false,"next_action":"reload_then_reconfirm"},
    "INVALID_INPUT":{"retryable":false,"next_action":"correct_field"},
    "UNAUTHORIZED":{"retryable":false,"next_action":"reauthenticate_or_stop"},
    "NOT_FOUND":{"retryable":false,"next_action":"refresh_list"},
    "COMMIT_UNKNOWN":{"retryable":false,"next_action":"reconcile_same_intent"},
    "PREVIEW_ONLY":{"retryable":false,"next_action":"confirm_or_edit"}
  },
  "dry_run":{"mode":"local_only","effects":[],"reason":"PREVIEW_ONLY"},
  "example":{"schema":"dashboard.scheduled_posts.v1","source_sha":"cdf33be676858f72f5874c000cd0e63efda6a02a","guild_id":"100000000000000001","schedule_id":"0123456789abcdef0123456789abcdef","definition_revision":null,"observed_at":"2026-11-01T12:00:00.000Z","state":"claimed","enabled":true,"queue_at_utc":"2026-11-01T12:01:00.000Z","queue_kind":"lease_horizon","reason":"CLAIMED_SNAPSHOT","retryable":false,"next_action":"inspect_before_change"}
}
```

Every row below is a must-pass **future acceptance oracle**, not runtime evidence.
Given the starting input, when the named action occurs, then the stated outcome
must be observable. Offline examples following the table check only document math
and consistency; they implement no scheduler, validator, state engine or CAS store.

| Fixture | Given / when / then |
| --- | --- |
| `SP-F01` | At frozen 12:00Z, one-off in=5 -> first 12:05Z, interval null; every=60 only -> 13:00Z/3600 s; in=5+every=60 -> 12:05Z/3600 s. Both timing inputs omitted -> invalid; no implicit now/default save. |
| `SP-F02` | Explicit 2027-01-15T18:30:00+05:30 and 13:00Z -> same 13:00:00.000Z. at combined with in/every, missing offset, >64 ASCII bytes, past, due-now, >365 days and truncation-to-now -> invalid, no save. |
| `SP-F03` | DST fold 2026-11-01T01:30:00-04:00 versus -05:00 -> 05:30Z versus 06:30Z; gap/local-only input never guessed. Changing display zone cannot change stored instant; 86400-second recurrence may shift local hour. |
| `SP-F04` | now equals clean due -> due snapshot. Claim at 12:00Z -> queue 12:01Z marked lease, not due/promise. At/after lease expiry a fenced occurrence is still fenced, never retryable because of time alone. |
| `SP-F05` | Restore disabled + claim/fence/NULL fence -> visible disabled flag plus claimed/fenced precedence, no pending hint or resume. Partial tuple/corrupt time -> unknown. Completed one-shot not inferred delivered or paused. |
| `SP-F06` | 25/26/50/51 restored rows with maximal Unicode IDs: whole-row pages, one hidden window, count/truncation explicit; 51st not claimed absent. Count/hidden-read failure -> unavailable where existence unknown, not empty/out-of-range; exact-ID detail remains separately scoped. |
| `SP-F07` | Two authorized viewers read revision r1; A edit wins r2; B edit/remove r1 -> STALE, zero mutation/audit/POST from B. Claim/fence change at same definition revision also refuses; delete/recreate cannot accept old incarnation. Baseline lacks this contract. |
| `SP-F08` | Access revoked, nonadmin, foreign guild, failed session/signature/gate -> no body/metadata disclosure, signing or mutation; purge private view on access loss. Names/body/URLs remain inert; no pings or secret/error echo. |
| `SP-F09` | Removal before fence -> no worker POST; during admitted POST with confirmed/refused cleanup or unknown receipt -> definition-only receipt, never message-absence promise. Fenced adapter removal refuses, evidence unchanged; current slash race remains accurately described. |
| `SP-F10` | Same local dry-run input/frozen now twice -> identical PREVIEW_ONLY, zero effects/allocated IDs; unknown mode refuses. Later save revalidates clock/quota/authority, does not reuse a preview reservation. |
| `SP-F11` | Empty/invisible body, 2000/2001 UTF-16, emoji, neutralization growth/invites, wrong types/duplicates, invalid interval/quota-full -> exact validator refusals. Legacy second-based interval is shown in seconds without rounding to a new minute schedule. |
| `SP-F12` | Confirmed save with delayed/parked worker -> stored only. Write timeout -> COMMIT_UNKNOWN, same-intent reconcile. Every later retry with the same idempotency key and exact body returns the original receipt; the mutation occurs once. Changed target/body refuses. Audit failure cannot yield a future atomic-mutation success; baseline best-effort audit remains labeled. |

Use injected clocks, mock Discord and disposable test DBs for future behavior
acceptance. No live messages, staging/production DB tests or migrations. Performance
proposal: two bounded windows, O(items) rendering and one bounded detail; no scan
of every schedule or catch-up loop. No latency measurements were run. Backward
compatibility includes restored over-cap/disabled/fenced data and canonical TEXT
times; no schema or rollback compatibility is changed by documentation.

Runnable checks from repository root (Python stdlib):

```sh
python3 -m doctest -v docs/dashboard-scheduled-posts-spec.md
python3 scripts/check_docs_index.py --json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
```

```pycon
>>> import json, pathlib, re
>>> from datetime import datetime, timedelta, timezone
>>> doc = pathlib.Path("docs/dashboard-scheduled-posts-spec.md").read_text()
>>> fence = "`" * 3
>>> c = json.loads(re.search(fence + r"json\n(.*?)\n" + fence, doc, re.S).group(1))
>>> assert c["status"] == "proposed" and not c["native_schedule_transport"]
>>> assert re.fullmatch(r"[0-9a-f]{40}", c["source_sha"])
>>> src = pathlib.Path("crates/core/src/scheduled.rs").read_text()
>>> symbols = {"ticker_ms":"SCHEDULER_TICK_MS","lease_ms":"CLAIM_LEASE_MS","batch":"TICKER_BATCH_LIMIT","at_bytes":"MAX_AT_CHARS","body_utf16":"MAX_BODY_CHARS"}
>>> assert all(int(re.search(r"pub const " + symbol + r": \w+ = ([\d_]+);", src).group(1).replace("_", "")) == c["timing"][key] for key, symbol in symbols.items())
>>> assert all([int(re.search(r"pub const " + prefix + suffix + r": i64 = ([\d_]+);", src).group(1).replace("_", "")) for suffix in ("_MIN", "_MAX")] == c["timing"][field] for field, prefix in [("in_minutes","IN_MINUTES"),("every_minutes","EVERY_MINUTES")])
>>> verbs = pathlib.Path("crates/core/src/internal_actions.rs").read_text().split("pub const IMPLEMENTED_ACTIONS", 1)[1].split("];", 1)[0]
>>> assert not re.search(r'"schedule\.[a-z_]+"', verbs)
>>> row = pathlib.Path("crates/core/src/scheduled_store.rs").read_text().split("pub struct ScheduledMessageRow {", 1)[1].split("\n}", 1)[0]
>>> assert "revision" not in row and all(k in row for k in ("claim_token", "claimed_at", "delivery_uncertain"))
>>> assert set(c["states"]) == {"saved","due","disabled","claimed","delivery-fenced","unknown","deleted"}
>>> e = c["example"]; assert set(e) == set(c["envelope"]) and e["definition_revision"] is None and e["queue_kind"] == "lease_horizon"
>>> assert not any(k in e for k in ("claim_token","occurrence_nonce","body"))
>>> assert all(isinstance(r["retryable"], bool) and r["next_action"] for r in c["reasons"].values())
>>> assert c["reasons"][e["reason"]] == {k:e[k] for k in ("retryable","next_action")}
>>> assert c["dry_run"] == {"mode":"local_only","effects":[],"reason":"PREVIEW_ONLY"}
>>> utc = lambda s: datetime.fromisoformat(s.replace("Z", "+00:00")).astimezone(timezone.utc)
>>> assert utc("2027-01-15T18:30:00+05:30") == utc("2027-01-15T13:00:00Z")
>>> assert utc("2026-11-01T01:30:00-05:00") - utc("2026-11-01T01:30:00-04:00") == timedelta(hours=1)
>>> now = utc("2026-11-01T12:00:00Z"); due = now + timedelta(minutes=5)
>>> assert due.isoformat() == "2026-11-01T12:05:00+00:00" and (now + timedelta(milliseconds=c["timing"]["lease_ms"])).isoformat() == "2026-11-01T12:01:00+00:00"
>>> inspected_revision, committed_revision = "r1", "r2"
>>> assert inspected_revision != committed_revision  # example conflict, not a CAS implementation
>>> assert len(set(re.findall(r'^\| `(SP-F\d+)` \|', doc, re.M))) == 12
>>> refs = re.findall(r'(crates/[a-zA-Z0-9_./-]+\.(?:rs|sql)):(\d+)(?:-(\d+))?', doc)
>>> assert refs and all(pathlib.Path(p).is_file() and 1 <= int(a) <= int(b or a) <= len(pathlib.Path(p).read_text().splitlines()) for p, a, b in refs)
>>> print(json.dumps({"fixtures":12,"schema":c["schema"],"status":"pass"}, sort_keys=True))
{"fixtures": 12, "schema": "dashboard.scheduled_posts.v1", "status": "pass"}

```

These check document schema, baseline constants/absence assertions, source paths
and example arithmetic only. They do **not** run SP-F01–F12 or prove UI, permission,
SQL contention, CAS, delivery/cancellation or a staging PASS. They are runnable
explicitly; no new CI selector, protected acceptance file or runtime test changes.

## Intent gate and smallest future slice

Default **no-build** until recorded host/CEO intent acceptance binds the exact task
spec revision, repo commit and file SHA-256, with relevant CISO disposition for new
transport/scope/body-exposure boundaries. A pending request or elapsed review date
is not approval. This gate is for new design intent, not a routine deploy approval
hop. No build cards now; sibling work retains its owner and acceptance gates.

Smallest candidate **SP-W1**, estimated <=1 engineering day excluding review:
a bot-local typed, bounded **metadata read envelope and serializer** for future
adapters: guild/full ID, nullable revision, observation time, queue kind, state,
reason and next_action. Reuse the separately owned list/receipt truth projection
once merged; do not reimplement its state classification or command UX. One
25-item response/32 KiB cap with explicit overflow; exact-ID summary is one row.
No endpoint, body reader, command, schema, writes, recovery or web code. Pure
serialization fixtures cover first/hidden-window inputs, disabled/restored mixed
evidence, byte overflow and exclusion of body/claim/nonce. This missing adapter
contract is distinct from the existing Discord text-packing work. If a sibling
already supplies the same envelope, reuse it instead of allocating a duplicate.
Sizing is a hypothesis, not a build commitment. Later transport/CAS/body/detail
and browser work need separate approved scopes and protected acceptance.

Design review metric by 2026-11-02: eight capability rows source-classified,
12 fixture oracles decided, zero labels equating stored/claimed/fenced/deleted with
confirmed delivery/absence. Kill the page experiment if existing private commands
meet the organizer need or safe bounded reads/CAS/intent alignment are not evidenced;
keep the no-build alternative, not a parked project. No demand/adoption experiment,
spend, delivery timing or production behavior was measured.

ACT: publish this Proposed contract. WHY: prevent false delivery/cancellation
claims and destructive dashboard writes. UNDO: revert documentation only; no
operational state changed.
