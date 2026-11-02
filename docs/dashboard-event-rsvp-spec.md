# Dashboard event and RSVP configuration contract

Status: **Deferred to the two-web-next roadmap** (owner decision 2026-11-01).
Build the bot half only when a two-web-next goal claims this page. Revisit
together with the 2026-12 RSVP-policy review. Since this decision,
registration cutoff and withdrawal are not separate designs; both are detailed
records under [RSVP capacity](rsvp-capacity-spec.md), the single RSVP-policy
spec. Original proposal: written
2026-10-26. Source inventory: `cdf33be676858f72f5874c000cd0e63efda6a02a`. No
page, API, runtime/store change, migration, credential, activation or live write
is added. Merging this document is not approval to build or deploy its proposed
contracts.

## Decision and no-build alternative

Organizers need to distinguish saved response intent from event availability,
verified attendance, reminder delivery and a usable voice room. Recommend
**bot-contract-first, read-first**: a bounded private event summary, aggregate
saved RSVP counts and a local edit proposal, not a live settings form over tables.
Even this read page needs an accepted reader/authority contract; none is added here.
A preview never calls existing mutating actions. Live save stays unavailable.

No-build: keep native Discord event management, `/rsvp`, `/rsvp-attendance` and
staff `/attendance`, subject to their existing publication/runtime/activation
gates. Totals already say historical saved responses. Do not obtain credentials,
turn on gates or widen a receiver to make a dashboard work. No live state was
inspected. The web shell, session authority and signer are **unverified**, not
asserted absent; this bot-only proposal does not inspect or modify another repo.

Reuse [internal actions](internal-actions-receiver.md), [event executors](internal-event-executors.md),
[settings boundaries](internal-action-settings.md) and the separate
[ticket](dashboard-ticket-config-spec.md), [onboarding](dashboard-onboarding-self-roles-spec.md)
and [custom-response](dashboard-custom-responses-spec.md) page proposals.
[Seats/waitlist](rsvp-capacity-spec.md), [reminder preferences](event-reminder-preferences-spec.md),
registration cutoff and withdrawal remain separate designs, not available controls
or dependencies for this inventory. Open PR [#1776](https://github.com/TogetherWeOwn/two-bot-next/pull/1776)
is a cutoff proposal, not an implemented cutoff at this source head.

## Source-backed control-to-consumer map

Anchors below are at the inventory commit; function names survive line movement.
Product rows are not `/ops/config` override keys. No per-event RSVP configuration
row, revision or editor exists in these stores.

| ID / proposed control | Current validator, authority, store/executor and audit | Consumer / truthful consequence |
| --- | --- | --- |
| `ER-C1` Event summary; read-only | `ScheduledEvent` / `normalize_events` (`crates/core/src/scheduled_events.rs:81-124,184`); `replace_events` / `upsert_event` (`crates/core/src/website_store.rs:312,475`), mirror `(guild_id,event_id)` (`crates/cutover/migrations/0300_website_contract.sql:118-135`). Internal `event.read` requires a mapped event_key, not arbitrary event ID (`crates/bot/src/internal_action_http.rs:1679-1709`). | Poller nominally every 10 min; whole valid snapshot replaces guild rows, failures retain last good. `raw_event` drops native user_count even though GET requests it (`crates/bot/src/website_jobs.rs:800-825,1042-1100`). Mirror is last observation, not configuration or live proof. |
| `ER-C2` Saved RSVP totals; read-only | `validate_event_id` / `normalize_event_link` (`crates/core/src/rsvp.rs:104-150`); `/rsvp-attendance` configured-guild member route under Announcements (`crates/core/src/router.rs:721-733`). `list_rsvps` loads saved rows (`crates/core/src/rsvp_store.rs:234-265`); read has no audit/write. | Discord `execute` returns Going/Interested/Declined counts and only invoker's saved response/time (`crates/discord/src/rsvp.rs:330-367`). Historical read does not check live event or current membership. No attendee roster read entitlement follows. Proposed browser read must revalidate visibility separately. |
| `ER-C3` Name/start/end/description/place edit proposal; preview-only | Existing `validate_event_input` (`crates/core/src/internal_actions.rs:1240-1316`); event_key grammar at `:1431-1461`. `mutate_event` staging-pins, resolves key, claims intent then invokes executor (`crates/bot/src/internal_action_http.rs:2408-2608`). `execute_event` (`crates/discord/src/internal_events.rs:178`) mutates Discord, then synchronously persists one mirror row before acknowledgment. Internal intent/audit receipt is service-caller attributed, not authenticated human attribution. | Existing upsert can create for an unmapped key or edit a mapped event. It is NOT a safe dashboard patch: no event CAS, incomplete readback and no web authority proof. Mirror update does not create/update an RSVP policy. No cancel/create/delete in the proposed page. |
| `ER-C4` Own RSVP / staff check-in; Discord handoff only | Exact lowercase status enum; strict event/link validation; `/rsvp` checks current actor membership and event before and after write, commits response plus `event.rsvp` audit atomically, compensates on lost evidence (`crates/discord/src/rsvp.rs:369-447`; `crates/core/src/rsvp_store.rs:91-103,494`). `/attendance` requires Manage Events even for self; verifies actor membership, live target membership and anchor event, classifies and writes `event_attended` plus `event.checkin` audit (`crates/discord/src/rsvp.rs:457-605`). | No web/on-behalf RSVP writer or attendance correction/reset/export proposed. RSVP proof writes no attendance fact (`crates/core/src/rsvp.rs:386-388`). Pre/post validation currently accepts scheduled, active AND completed (statuses 1..3), refuses cancelled. No closes-at-start rule. Compensation failure can leave uncertain saved state; never present refusal as proven rollback. |
| `ER-C5` Reminder policy/result; read-only explanation | `due_event_reminders` / `begin_event_reminder_delivery` (`crates/core/src/event_reminder_store.rs:15-123`) select scheduled future mirror rows with a channel, Going/Interested only; fixed 15-min lead, max two candidates/tick. Permanent `rsvp.reminder_attempt` fence commits before rendering/send; NULL system actor. | `remind_event_attendees` (`crates/bot/src/scheduled_jobs.rs:226-277`) appends posted/failed/delivery_uncertain result. `scheduled_messages` nominal 15-s job, ordinary schedule phase first (`:35,56-118,157-173`). Schedule-store failure can still allow reminders, but either phase's failure is unhealthy. At most one automatic attempt/event, not delivery or notification guarantee. No DM, timing editor, send-now, retry or fence reset. |
| `ER-C6` Event voice availability; unsupported until observed | `configured_event_voice_category` / `EventVoiceCategoryGate` (`crates/bot/src/event_voice_category.rs:19-22,34-90`) define required category/preflight reasons; `claim_channel` (`crates/bot/src/event_voice_channels.rs:257`) wraps lease claim/bind. Instance identity `(event_id,instance_starts_at)`; tables have no guild column (`crates/cutover/migrations/0455_event_voice_channels.sql:39-63`). | Presence transition/bootstrap/repair are wired through voice actor (`crates/bot/src/voice_rooms.rs:10015,10049,10085,10235-10246`). Category preflight, create and reconciliation helpers are partial seams, not proof of a wired T−15 creator or website columns. No non-test production caller found for category loader/gate or claim_channel at inventory. No category editor, room create/move/delete or attendance inference from a channel binding. |

### Exact field bounds and data identities

- Raw RSVP event IDs: 17–20 ASCII digits, nonzero and fit u64; not trimmed.
  Accepted links: literal `https://discord.com/events/<same-guild>/<event>`,
  optionally angle-wrapped; no query/fragment/trailing slash. Keep IDs as strings,
  never lossy JavaScript numbers. RSVP statuses: `going`, `interested`, `declined`.
- Check-in occurrence: trimmed, nonempty, <=128 UTF-16 units; bare event ID or
  `event_id:nonempty-label` (`crates/core/src/rsvp.rs:303-358`). Label is not a
  verified recurrence/date. RSVP rows have no occurrence key; do not join their
  counts to one check-in occurrence and label the result attendance/conversion.
- Upsert fields: nonempty name <=100 UTF-16 units, optional nonempty description
  <=1000, RFC3339 start/end normalized to UTC milliseconds, end > start; exactly
  one allowlisted channel_key OR nonempty location <=100 UTF-16 units. No raw
  channel ID placement. event_key is `[A-Za-z0-9._:-]{1,200}`. Current upsert is
  full input, not PATCH; null description is rejected, omission stays omitted
  in the Discord request. Do not invent clear/preserve semantics for an editor.
- Mirror has name/start/channel/description/status/updated_at, no end or external
  location. Existing `event.read` returns only outcome/event_id/name/starts_at/
  location/status/observed_at, no end/description/channel/count. It cannot hydrate
  a preserving edit. `internal_event_keys` maps `(guild_id,event_key)` to event ID
  (`crates/cutover/migrations/0415_internal_event_keys.sql:7-14`); native mirror
  existence does not establish that mapping. Never invent it from an event name.
- `event_rsvps` PK `(guild_id,event_id,user_id)`, no event FK or seat allocation
  (`crates/cutover/migrations/0160_rsvp.sql:24-34`). `RsvpTotals::total` includes
  Declined. Fixed safety limits: 1000 response rows/event, 1000 check-ins/occurrence,
  20 RSVP writes/member/guild/UTC minute (`crates/core/src/rsvp.rs:506-508`). All
  statuses consume row capacity; existing respondents may update at capacity.
  These are not editable seat/cutoff/waitlist/rate/retention policies. Retention
  floors and a purge helper do not guarantee complete lifetime attendance history.

### Storage is not effective state

No new environment key is proposed. `TWO_ANNOUNCEMENTS` is Hot/HOT_WIRED,
`TWO_COMMUNITY_SCORECARD` independently gates check-in, `TWO_AUTOMATIONS` is Cold;
`TWO_EVENT_VOICE_CATEGORY_ID` and `TWO_INTERNAL_*` are environment/security-only
(`crates/core/src/settings.rs:125-159,189-193,277-317`). None becomes an event form
field. Existing `settings.get/set` CAS only protects guild_settings; it does not
protect product rows, Discord events or reminder fences. `/ops/dashboard` is a
GET-only health page; `/ops/config` is a distinct deployment/operator overlay,
not an event editor (`wrangler/src/ops-dashboard.ts:2-16`; `wrangler/src/ops-control.ts:7-26`).

Announcements' live setting is consumed by feeds (`crates/bot/src/feed_jobs.rs:239-266`),
while RSVP router gates are constructed at boot (`crates/bot/src/main.rs:994-1039`).
Stored Hot does not establish live RSVP gate application. Reminder registration
also uses boot automation/identity fencing. Preserve activation: only SelfRoles
is cleared for recognized live identity at this head
(`crates/core/src/activation.rs:51-96`). This says nothing about current deployment.

`web_v1.upcoming_events` / `next_event` expose only event ID/name/start/channel/
description, scheduled/active within 90 days (`sql/web_v1.sql:228-250`): no totals,
status, observation timestamp or voice reason. Website reader gets reviewed views,
not underlying bot/settings/audit/presence tables (`docs/database-roles.md:12-17`).
Do not solve missing readback with broad SQL grants or browser access to bot tables.

## Authority, browser safety and confirmed targets

Proposed staff read: current session member + Manage Server in the trusted
selected guild, plus current event/channel visibility. This is deliberately
narrower than the member Discord totals command; no additional member lists.
Future edit would ALSO require current Manage Events, event visibility and bot
capabilities; it must not assume creator ownership or reuse a stale interaction
permission snapshot as current web authority. Revalidate at read, preview approval
and execution; loss/unknown evidence denies without exposing foreign object facts.
This is a proposal, not permission enforced by today's service MAC.

Keep the accepted private-server boundary: CSRF checks, session revocation,
no-store/private responses, server-held signer, exact-byte signing, caller/key
mapping, durable nonce, feature/identity/activation gates. Compare trusted session
guild with receiver's pinned deployment guild BEFORE signing even event.read;
a body guild_id cannot retarget it. No production mode, test flag combination,
public proxy or generic multi-guild route is offered here.

Existing scope map: event.read needs `read`; event.upsert/cancel need exact
`moderate`, NOT `admin` (`InternalFlags::required_admin_scope`,
`crates/core/src/internal_actions.rs:2051-2095`). This foundation does not prove a
live Admin API gateway. Reuse its owning design, not a parallel bearer API or new
token. A future aggregate reader needs an explicit read-scope/holder disposition;
unmapped actions currently refuse. Never grant admin to get an event editor working.

Human actor must come from the authenticated server session. Current mutate_event
binds the service caller/intent but consumes no verified human actor; adding
browser actor_id is not attribution. A future edit needs durable human actor +
service caller + target + revision + intent + outcome audit, without member lists,
rendered descriptions or raw upstream bodies. Existing audits are reused, not
claimed to contain human evidence they lack. No credentials or token values shown.

Minimal human layout: private guild/event selector (<=100 event summaries, a
proposed bound matching mirror cap), one detail and a local proposal review.
Counts are named “Saved Going / Interested / Declined”, never “People attending”.
Aggregate only: no IDs, names, avatars, timestamps per member, roster links or
exports. Own response remains in Discord for this first slice. Use an aggregate
query behind the future reader, not list_rsvps rows or capped reminder arrays in
the browser. Escape names/descriptions/locations as inert text; no HTML, embedded
resources, auto-fetch, mention expansion or external-image loading. Accessible
labels and errors, keyboard focus to error summary, no color-only states or
optimistic saved toast; retain drafts in private memory only, no offline save queue.

A hypothetical future confirmation binds guild_id + mapped event_key + event_id +
complete baseline revision + normalized before/after fields + server-derived actor +
intent ID. Show time zone and exact UTC times, explicit effects, and warning that
reschedule does not rearm a reminder or prove voice relocation. Changed identity,
visibility, revision or draft invalidates confirmation. Cancel sends nothing.
No current save button is enabled; preview clearly says “Nothing sent or saved.”

### CAS, dry-run and uncertain effects

Missing: an atomic, lifecycle-safe revision/precondition across every event writer,
including native Discord changes, plus complete hydration. Mirror updated_at,
observed_at, a client hash or settings CAS is NOT event CAS. A GET then unconditional
upsert still races. Native Discord offers no contract here making local CAS cover
external writes; either prove a safe conflict policy or keep live edits unavailable.
Do not call a new field expected_revision and pretend today's executor enforces it.

Local preview/dry-run is deterministic for identical inputs; zero signing, nonce,
intent reservation, audit, SQL or Discord calls. It validates shape/manifest and
shows a literal bounded diff only; it cannot prove current access, CAS freshness,
effect eligibility or bot-equivalent execution. No server dry-run verb exists;
never send `dry_run:true` with event.upsert expecting a harmless write.

For already-authorized existing event actions, retain same idempotency key and
exact body for same-intent replay, with fresh nonce/time/signature. A changed body
needs a newly confirmed intent, not reuse of an old key. Current VersionConflict
also means same-key/body mismatch, NOT event revision conflict. Crash, timeout,
malformed Discord success, failed mirror or missing receipt can leave a claim
unknown; needs_reconciliation stays fenced. Never make a fresh key, remap an event
or auto-recreate after 404 to force success. Operator reconciliation is not a
browser repair endpoint. Read action mirror persistence is not an event mutation
preview. No production or live mutating-route probe is part of this proposal.

## Honest observation and failure states

Each domain carries its own source, stable identity and observed_at; absent
observation is null/unknown, not now. `saved` is persisted intent/configuration,
`effective` requires evidenced consumer readback, `last_confirmed` is a dated
Discord receipt (not current), `claimed` proves a durable attempt only, `unknown`
means evidence missing. Never translate one domain's status into another.

| Reason | Human meaning | next_action |
| --- | --- | --- |
| `HISTORICAL_SAVED` | Saved responses, not verified attendance or native Discord interested count. | `none` |
| `EMPTY_CONFIRMED` | Fresh successful aggregate read found zero saved responses; event may still exist. | `none` |
| `PREVIEW_ONLY` | Nothing sent or saved; live save unavailable. | `keep_in_discord` |
| `CONTRACT_MISSING` | Reader, hydration, actor audit or safe revision contract unavailable. Not an empty form. | `keep_in_discord` |
| `ACCESS_DENIED` | Current authority/visibility missing or revoked; clear protected displayed data. | `reauthenticate_or_stop` |
| `INVALID_INPUT` | Field-local shape/bound error, no echoed upstream body. | `edit_draft` |
| `STALE_REVISION` | Proposed conflict result: keep draft, reload and reconfirm; not today's event wire error. | `reload_reconfirm` |
| `READ_UNAVAILABLE` | Read failed. Prior data is labeled stale with time, not zero/disabled; no save. | `retry_read` |
| `CLAIMED_UNKNOWN` | Reminder attempt claimed, result unconfirmed. No resend. | `inspect_evidence` |
| `COMMIT_UNKNOWN` | Save outcome unconfirmed. Do not submit a different save. | `reconcile_same_intent` |
| `REFUSED` | Proven rejection is not a save; do not imply compensation succeeded without evidence. | `inspect_refusal` |
| `VOICE_UNVERIFIED` | Binding/config/preflight is not a usable-room proof. | `keep_in_discord` |

These are proposed adapter reasons, not new bot wire enums. Distinguish not found,
unsupported and unavailable without leaking inaccessible event existence. On
outage retry only safe reads; respect Retry-After. Clear cached data on access
loss instead of retaining even a stale protected count.

Reminder cohort count = Going + Interested, NOT total responses. The examples'
reminder_eligible field means this saved cohort only; event/time/channel and
execution gates must still pass before an attempt. Its SQL exact count is
distinct from a capped 1001-ID array. Oversize reminder produces
one mention-free count/start/link fallback, never partial member list or burst.
Outbound allowed-mentions suppress notifications ([current help](event-reminders.md)).
A claimed attempt consumes the entitlement even if rendering/send/audit result
fails. Fence identity is guild/event (not occurrence/start); rescheduling or
changing RSVP does not grant another attempt. Mirror revalidation at admission
cannot recall an in-flight send or prove live visibility/membership now.

Voice instance start and RSVP/reminder event identities differ. Rescheduling is
not a proven channel migration; do not mutate/rebind rooms from the editor. Voice
presence spans are not proof of human attendance without the existing proof and
classifier/privacy contract. Check-in privacy suppression can currently share
the duplicate reply (`crates/core/src/rsvp_store.rs:397-399`;
`crates/discord/src/rsvp.rs:622-626`); that sentence alone does not prove a fact.
No verified-attendance total is invented from RSVP, native interest or presence.

## Bounded agent examples and offline checks

Illustrative proposal schema below, NOT an endpoint, wire payload, permission or
runtime result. IDs are synthetic strings; revision is a proposal snapshot ID,
NOT a shipped CAS token. No member rows or production selection flag. Every
adapter response uses stable reason/next_action and explicit retryability.

```json
{
  "schema":"dashboard.event_rsvp.v1",
  "source_sha":"cdf33be676858f72f5874c000cd0e63efda6a02a",
  "proposal_revision":"er-1",
  "target":{"guild_id":"111111111111111111","event_id":"222222222222222222","event_key":"game-night"},
  "controls":[
    {"id":"ER-C1","mode":"read_only","evidence":"E-MIRROR"},
    {"id":"ER-C2","mode":"read_only","evidence":"E-TOTALS"},
    {"id":"ER-C3","mode":"preview_only","evidence":"E-VALIDATOR"},
    {"id":"ER-C4","mode":"discord_only","evidence":"E-CHECKIN"},
    {"id":"ER-C5","mode":"read_only","evidence":"E-REMINDER"},
    {"id":"ER-C6","mode":"unsupported","evidence":"E-VOICE"}
  ],
  "evidence":{
    "E-MIRROR":{"path":"crates/core/src/website_store.rs","line":475,"contains":"pub async fn upsert_event"},
    "E-TOTALS":{"path":"crates/discord/src/rsvp.rs","line":365,"contains":"Historical saved responses; live event availability is not checked."},
    "E-VALIDATOR":{"path":"crates/core/src/internal_actions.rs","line":1240,"contains":"pub fn validate_event_input"},
    "E-CHECKIN":{"path":"crates/discord/src/rsvp.rs","line":462,"contains":"require_manage_events("},
    "E-REMINDER":{"path":"crates/core/src/event_reminder_store.rs","line":76,"contains":"pub async fn begin_event_reminder_delivery"},
    "E-VOICE":{"path":"crates/bot/src/event_voice_category.rs","line":19,"contains":"T-15 ticker (separate card)"}
  },
  "examples":[
    {"id":"ER-X1","state":"saved","observed_at":"2026-10-26T00:00:00Z","counts":{"going":2,"interested":1,"declined":3},"response_total":6,"reminder_eligible":3,"verified_attendance":null,"native_interested":null,"reason":"HISTORICAL_SAVED","retryable":false,"next_action":"none"},
    {"id":"ER-X2","state":"unknown","observed_at":null,"counts":null,"reason":"READ_UNAVAILABLE","retryable":true,"next_action":"retry_read"},
    {"id":"ER-X3","state":"claimed","fence":"event:reminder:111111111111111111:222222222222222222","result":null,"reason":"CLAIMED_UNKNOWN","retryable":false,"next_action":"inspect_evidence"},
    {"id":"ER-X4","state":"preview","base_revision":"er-1","intent_id":"proposal-001","draft":{"name":"Game night"},"effects":[],"reason":"PREVIEW_ONLY","retryable":false,"next_action":"keep_in_discord"},
    {"id":"ER-X5","state":"unknown","intent_id":"existing-intent-001","reason":"COMMIT_UNKNOWN","retryable":false,"next_action":"reconcile_same_intent"},
    {"id":"ER-X6","state":"denied","counts":null,"reason":"ACCESS_DENIED","retryable":false,"next_action":"reauthenticate_or_stop"},
    {"id":"ER-X7","state":"conflict","base_revision":"er-1","current_revision":"er-2","reason":"STALE_REVISION","retryable":false,"next_action":"reload_reconfirm"},
    {"id":"ER-X8","state":"refused","saved":false,"compensation_confirmed":false,"reason":"REFUSED","retryable":false,"next_action":"inspect_refusal"}
  ]
}
```

Runnable from repo root (stdlib only):

```sh
python3 -m doctest -v docs/dashboard-event-rsvp-spec.md
python3 scripts/check_docs_index.py --json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
```

This checks source anchors/constants, scope-map and illustrative example
consistency only. It neither implements a fake endpoint nor runs future runtime
acceptance, proves authorization/CAS, or connects to DB/Discord. Source drift
requires re-inventory, not a waiver.

```pycon
>>> import json, pathlib, re
>>> doc = pathlib.Path("docs/dashboard-event-rsvp-spec.md").read_text(); fence = "`" * 3
>>> m = json.loads(re.search(fence + r"json\n(.*?)\n" + fence, doc, re.S).group(1))
>>> assert re.fullmatch(r"[0-9a-f]{40}", m["source_sha"]) and m["proposal_revision"] == "er-1"
>>> assert {c["id"] for c in m["controls"]} == {f"ER-C{i}" for i in range(1, 7)}
>>> assert all(c["mode"] in {"read_only", "preview_only", "discord_only", "unsupported"} and c["evidence"] in m["evidence"] for c in m["controls"])
>>> assert all(e["contains"] in pathlib.Path(e["path"]).read_text().splitlines()[e["line"]-1] for e in m["evidence"].values())
>>> src = pathlib.Path("crates/core/src/rsvp.rs").read_text()
>>> assert all(re.search(rf"pub const {k}: \w+ = {v};", src) for k, v in {"MAX_RSVPS_PER_EVENT":"1_000", "MAX_CHECKINS_PER_OCCURRENCE":"1_000", "MAX_RSVP_WRITES_PER_USER_PER_MINUTE":"20"}.items())
>>> reminder = pathlib.Path("crates/core/src/event_reminder_store.rs").read_text()
>>> assert "EVENT_REMINDER_LEAD_MS: u64 = 15 * 60 * 1000" in reminder and "IN ('going', 'interested')" in reminder
>>> scope = pathlib.Path("crates/core/src/internal_actions.rs").read_text().split("pub fn required_admin_scope", 1)[1].split("/// Whether a token", 1)[0]
>>> assert re.search(r'"event\.upsert"\s*\| "event\.cancel" => Some\(ADMIN_SCOPE_MODERATE\)', scope)
>>> assert '"settings.get" | "event.read" | "automations.export" => Some(ADMIN_SCOPE_READ)' in scope
>>> examples = {x["id"]: x for x in m["examples"]}; assert len(examples) == 8
>>> x = examples["ER-X1"]; assert sum(x["counts"].values()) == x["response_total"] == 6
>>> assert x["counts"]["going"] + x["counts"]["interested"] == x["reminder_eligible"] == 3
>>> assert x["verified_attendance"] is None and x["native_interested"] is None
>>> assert all(examples[k]["counts"] is None for k in ["ER-X2", "ER-X6"])
>>> assert examples["ER-X3"]["result"] is None and examples["ER-X3"]["retryable"] is False
>>> assert examples["ER-X4"]["effects"] == [] and examples["ER-X5"]["next_action"] == "reconcile_same_intent"
>>> assert examples["ER-X7"]["base_revision"] != examples["ER-X7"]["current_revision"]
>>> assert not examples["ER-X8"]["saved"] and not examples["ER-X8"]["compensation_confirmed"]
>>> reasons = dict(re.findall(r'^\| `([A-Z_]+)` \| .*? \| `([^`]+)` \|$', doc, re.M))
>>> assert all(x["reason"] in reasons and x["next_action"] == reasons[x["reason"]] and isinstance(x["retryable"], bool) for x in m["examples"])
>>> assert all(word not in json.dumps(m) for word in ['"user_id"', '"members"', '"mode": "production"'])
>>> assert len(set(re.findall(r'^\| `(ER-F\d+)` \|', doc, re.M))) == 12
>>> print(json.dumps({"schema":m["schema"],"controls":len(m["controls"]),"examples":len(examples),"status":"pass"}, sort_keys=True))
{"controls": 6, "examples": 8, "schema": "dashboard.event_rsvp.v1", "status": "pass"}

```

## Acceptance matrix and future slices

All rows are **must-pass planned fixtures, NOT executed by this document**. Use
mock Discord, injected clock and disposable agent-testdb/CI only. Retain JSON
responses, zero-effect call counters, relevant synthetic row/audit deltas and
UI screenshots/timings when an actual page exists. No live mutating probes.

| ID | Given / when / then; evidence required for future acceptance |
| --- | --- |
| `ER-F01` | Given an authorized visible event, when summary is read, then each domain has exact identity/source/time and no saved-as-applied claim; response snapshot. Golden path stays read-only until accepted writer contracts. |
| `ER-F02` | Given 2 Going/1 Interested/3 Declined, when counts render, then response_total=6, reminder_eligible=3 and verified/native totals unknown; payload contains no member data. Given a fresh zero-row aggregate, then EMPTY_CONFIRMED; failed read never zero. |
| `ER-F03` | Given access was valid, when membership/Manage Server/event visibility is lost before read or confirmation, then data is cleared, denial returned and zero signatures/effects; repeat with Manage Events lost for hypothetical write. |
| `ER-F04` | Given two drafts at revision r, when another writer changes state, then stale draft cannot overwrite it; prove atomic enforcement and native Discord race handling before enabling writes. Timestamp/hash/read-then-write is insufficient; otherwise CONTRACT_MISSING. |
| `ER-F05` | Given same preview input, when repeated, then identical normalized diff and zero network/nonce/intent/audit/SQL/Discord effects; cancel also zero. No dry_run flag reaches a mutation. |
| `ER-F06` | Given invalid/foreign IDs, duplicate options, oversized UTF-16 name/description/location, bad dates, end<=start or both/no placements, when validated, then field error and zero effects. Include 51 emoji in a 100-unit name, RTL text, u64 overflow and missing end hydration. |
| `ER-F07` | Given same intent/body replay, when receipt is lost or body changes, then no duplicate mutation and correct replay/mismatch/unknown result; restart keeps claim. Foreign-guild session cannot sign a request to staging-pinned receiver. |
| `ER-F08` | Given rejected RSVP or post-write lost membership/event evidence, when compensation fails, then REFUSED with saved state unknown, not rolled-back/saved toast; audited write/fact suppression and duplicate text never imply verified attendance. |
| `ER-F09` | Given a due reminder, when a schedule-store phase fails, then reminders remain eligible under existing tick semantics; failed or absent result stays distinct from posted. Crash after claim never resends; full count >1001 must not equal capped array length. |
| `ER-F10` | Given reschedule/cancel/channel move/RSVP change racing reminder admission, when facts differ, then no admission from stale candidate; after claim no rearm and no promise to recall a send. Voice identity/binding never proves relocation or attendance; occupied/missing room state not silently repaired. |
| `ER-F11` | Given 100 summaries and an event at 1000 saved rows, when list/detail loads, then payload is bounded and aggregate-only, no member-row query transfer, hangs or fan-out; record timings against an accepted future latency budget (not invented/measured here). Mirror/read outage keeps last-confirmed stamp, disables edit; revocation instead clears it. |
| `ER-F12` | Given existing native/unmapped events and historical responses, when page reads/previews, then no mapping creation, event mutation, RSVP rewrite, audit deletion, voice transition or reminder-fence change. Existing identities, history and Discord URLs remain intact; no new runtime telemetry promised by a docs change. |

N/A: production rollout/performance/UI measurements in this docs-only slice.
No claim of runtime PASS or attendance coverage. Missing aggregate reader,
verified-attendance read policy, full edit hydration, CAS/native conflict policy,
human attribution and effective-state diagnostics are design findings, not reasons
to widen scope or block this proposal behind implementation work.

Possible bot-side slices, each <=1 engineer-day **after intent acceptance**, not
build cards or commitments: `ER-W1` freeze aggregate-only reader/auth/scope DTO and
independent access/PII acceptance fixtures; `ER-W2` extend mapped-event hydration
contract without adding writes; `ER-W3` decide native-write conflict/confirmation
and actor-audit contract with refusal fixtures (no optimistic CAS promise);
`ER-W4` add reminder/voice observation DTO fixtures preserving fences/unknown.
Split again rather than exceed a day. A receiver/web implementation is separately
scoped only after its prerequisites are accepted; this card creates no build cards.

Keep Proposed until recorded host/CEO intent alignment and relevant CISO
read-access/actor/scope disposition. A queued request or timeout is not acceptance;
no-build is the default. Bind any later decision to task-document revision, repo
commit and file SHA-256. Review target 2026-11-02; metric: all six controls have
source/authority/consumer/state decisions and all 12 fixture oracles are accepted
or rejected explicitly. This is not a live experiment or scheduled activation.

ACT: publish proposal plus offline checks. WHY: prevent historical saved responses
or durable attempts from masquerading as live attendance/effect. UNDO: revert the
document/index changes; no operational state changes. Unverified: actual web
contracts, deployed gates/identity, current guild visibility, effective settings,
Discord state, delivery, event-voice creation and production behavior.
