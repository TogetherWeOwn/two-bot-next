# Events: one-shot publish to Discord native scheduled events

Status: **Proposed — design only**, 2026-10-26; review by **2026-11-02**.
This is the single editable repository contract; the task spec mirrors its exact
commit and SHA-256. Merge is not acceptance, host/CEO intent alignment, CISO
disposition, build approval or activation. No runtime, acceptance-test,
migration, permission, credential, publication, deployment or spending change
is included.

## 1. Decision and baseline

Consumer: an organizer who plans a game night in TWO and wants members to find
it in Discord's native Events tab with native reminders. Hypothesis: one
explicit per-event publish beats today's paths. Demand is **unproven**.

The [sesh manual](https://sesh.fyi/manual/), read 2026-10-26, documents a
"Mirror to Discord Native Event" checkbox on its Create Event dashboard page.
That is vendor capability comparison, not TWO demand or a grant to reproduce
its other controls. No member outreach or live audit ran.

Premise corrections, verified at the head in §2. The planning note said
writes exist only for the hardcoded Sunday Squad anchor. The anchor module
(`crates/core/src/anchor_event.rs`) shapes POST bodies but has **no sender**
in-tree: only `crates/core/tests/anchor_event.rs` calls its builders. The live
write path is signed website/dashboard internal actions `event.upsert` and
`event.cancel` through `ActionExecutor::execute_event`
(`crates/discord/src/internal_events.rs:178-235`), with a durable claim fence,
response binding and a single-row mirror refresh. What does not exist is an
**in-Discord organizer entry**: no slash command, modal or button lets an
organizer publish an event where members already are. No series-template doc
exists in this repo; the closest seams are per-night EXTERNAL plans
(`plan_game_night_creates`, `crates/core/src/internal_actions.rs:1225-1238`)
and confirmation-before-save for voice templates
(`docs/voice-template-lint-core.md:73`). Per-event confirmation for native
publish is proposed here, not existing.

| Alternative | Organizer effect / cost | Proposed disposition |
| --- | --- | --- |
| A. No build; current website-signed upsert + anchor shaping | Organizer leaves Discord for the dashboard, or nothing is published; zero new write boundary | Standing fallback until acceptance; choose it if value or safe bounds fail |
| B. One-shot explicit per-event publish in Discord | One modal draft, one ephemeral preview, one confirmed POST; needs a publish ledger, claim fence and organizer authority checks | Submit for exact intent/security review, not implementation |
| C. Two-way sync after publish | TWO edits/deletes track Discord state; conflict, loop and silent-public-edit risk | Excluded; one-way only (§3) |
| D. Auto-publish on draft/save | Publishes without a per-event decision | Excluded; never silent (§3) |

Source baseline: `a6fa0fb7d40e2ee4eaa6bfce9834d639a25c166c`; source inspection
only, not deployed-state evidence:

- `crates/discord/src/internal_events.rs:30-44,129-151,178-308`: `EventCall`
  Upsert/Cancel/Read; body always carries `privacy_level: 2`, entity type 2
  (voice channel) or 3 (external + location, channel cleared); create vs
  update by mapped ID; cancel is PATCH `status: 4` and retains the mapping so
  a later edit cannot silently recreate; every response binds guild and event
  ID before the mirror write; unknown outcomes never retry the original POST.
- `crates/core/src/internal_actions.rs:82-85,1211-1238,1240-1317`: name ≤100,
  description ≤1000, location ≤100 UTF-16 units; `ends_at` strictly after
  `starts_at`; exactly one of `channel_key`/`location`; channel IDs resolve
  through a key map, never raw caller snowflakes; at most 100 active events
  per guild and a full calendar files nothing.
- `crates/core/src/anchor_event.rs:8-16,58-73,95-105`: Sunday Squad weekly
  series at 20:00 America/New_York; each occurrence from its own local date
  (the DST week is 169 hours, never a fixed 604 800 s step); Discord refuses a
  start in the past; `recurrence_rule` exists only on this hardcoded series.
- `crates/core/src/scheduled_events.rs:30-41,102-124`: the poller mirrors
  Discord every 10 minutes, ≤100 rows, no end-time column; mutation rows stamp
  `updated_at` after Discord answers so last-observed-wins beats a stale poll.
- `crates/discord/src/executor.rs:2114-2160`: `get_scheduled_event` single
  lookup where only 404 means absent; transport failures never masquerade as
  missing. POST/PATCH/DELETE route labels in
  `crates/discord/src/executor_metrics.rs:74-81` are metric classification,
  not senders.
- `crates/core/src/rsvp.rs:229-236`: the server-side Manage Events gate reused
  for check-in; identity is a Discord event snowflake, never a name or time.
  RSVP/agenda/calendar/recap/role proposals add no organizer publish entry.

## 2. Proposed publish contract

Draft source is one organizer modal, not a stored TWO event object: name,
start, end, exactly one placement (voice-channel key or external location),
optional description. Bounds reuse §1 verbatim: name ≤100, description ≤1000,
location ≤100 UTF-16 units; end strictly after start; start strictly in the
future at confirm time. The preview mints `draft_id`
(`npub:<guild>:<uuidv4>`); the ledger row `(guild_id, draft_id)` is the only
draft store. No TWO event table, no series object, no template change.

Authority, in order: configured guild from interaction context; Announcements
enabled (the existing events surface); current live human membership; fresh
Manage Events evaluated on the invoking interaction. Event creator, channel
ownership or Manage Roles alone grant nothing. The bot must hold Manage
Events at confirm (verify the exact Discord requirement at build; unknown
evidence fails closed). Foreign guild, DM, missing or denied evidence
collapse into one reply with no existence oracle.

Preview is read-only and ephemeral, ≤400 UTF-16 units per message, no
pings/DMs: exact name, start/end as Discord-localized tags plus UTC, the
resolved channel name or location, description or "(no description)", the
current active-event count against the 100 cap, and the disclosure "This
creates a public event every member can see." Controls are **Publish this
event** / **Cancel**. Confirm binds the preview's exact content hash, draft
ID, expiry (DB UTC preview time + 300 s, equality expired) and runtime boot
nonce, never silently the newest draft. One fixed encoding, mirroring the
withdrawal proposal: `two:np1:` + unpadded canonical base64url of draft UUID
16 bytes + content hash 32 bytes (SHA-256 over canonical fields) + expiry u64
+ boot nonce 16 bytes (network order), 72 payload bytes / 96 encoded
characters. Reject malformed length or invalid fields; never log tokens, raw
component IDs or reply bodies.

Confirm revalidates everything before any POST: authority, membership,
channel binding with fresh evidence of organizer View Channel, bot Manage
Events, future start, active-count headroom, draft state `previewed`, and the
content hash. Then one transaction: take the per-`(guild,draft)` advisory
lock, re-check state, mark `publishing`, commit, POST once, bind the response
(guild + nonzero snowflake id), upsert the mirror row, mark `published` with
the Discord event ID. A second confirm on a `published` draft returns the
existing mapping with zero POST. A confirm whose fields differ from the
preview hash is stale: fresh preview required. No automatic POST retry; the
existing call-once semantics apply.

One-way only. After publish the ledger row is read-only: Discord event ID,
content hash, organizer, timestamps. TWO never PATCHes or DELETEs the native
event in this proposal. Organizer edits or cancellation happen in Discord
directly until a future explicit verb with its own preview+confirm is
specified, reviewed and accepted; that verb is not this slice. Discord-side
edits are observed through the existing poll, never pulled back into TWO
state. The stored content hash exists only so a future read can say
"published; changed in Discord since" — drift display itself is out of scope.

Recurrence and DST: one POST creates one native event with no
`recurrence_rule`. A weekly night is published occurrence by occurrence, each
explicitly confirmed. Absolute instants per occurrence only; fixed-interval
stepping is forbidden (the §1 DST lesson). Start-in-the-past is refused at
confirm; a start that passed between preview and confirm is stale, not
shifted. Time-zone input renders to UTC millis at preview and the hash binds
the rendered instants, never the typed string.

## 3. Failures, races, cost and audit

Unknown outcomes: a lost POST acknowledgement is `commit_unknown`, never
claimed success or rollback. Recovery is inspect-then-decide through the
keyless single-event GET: if Discord holds an event the ledger cannot bind to
this draft, the draft stays `publishing` and needs organizer inspection, not
a blind re-POST. Mirror-write failure after a bound response retains the
fence and reports failure (the existing `EventActionError::Mirror` pattern);
Discord holds the event and the ledger says so honestly. Discord-side
deletion after publish leaves the ledger row `published` with a note from the
poller tombstone; TWO never recreates it. Pre-wire refusals (authority loss,
cap reached, Discord 4xx on a preflight GET) write nothing.

Races resolve by the ledger: concurrent confirms on one draft serialize on
the advisory lock — first publishes, the rest return the mapping. Organizer
loses Manage Events or membership between preview and confirm: refused at
confirm, draft stays `previewed`. Channel deleted or organizer loses View
Channel: refused. Bot loses Manage Events: refused. Restart or boot-nonce
rotation invalidates pending previews. Ledger state machine is
`previewed -> publishing -> published`, plus terminal `abandoned` (cancelled
or expired preview) and non-terminal `publishing` requiring inspection; no
other transitions exist.

Bounded Discord cost per attempt: ≤4 paced GETs (membership evidence, channel
fetch, bot permission read, optional single-event inspect on the unknown path
only), exactly ≤1 POST, one mirror upsert. No POST retry loop, no sweep, no
background reconciliation. Publishes consume the existing member write budget
(20 writes/member/guild/UTC minute) under the same budget lock with the new
audit action counted; rolled-back attempts consume nothing. Guild headroom
comes from the local mirror active count, not a Discord scan; a guild at the
100 cap refuses before any POST.

Atomic publish audit in the existing transaction-aware seam:
action `event.native_publish`, guild, actor=organizer, target=Discord event
ID, draft ID, SHA-256 content hash, outcome; deterministic ID
`npub:<interaction-id>`; exactly one row required or the ledger commit rolls
back. No description text, location text, raw payload or token in metadata.
Per-refusal audit is forbidden. Retention follows the existing 365-day audit
floor; the new action joins purge/count/erasure tests before any build.
Organizer erasure deletes actor-attributed rows under the existing predicate;
it never nulls the Discord event ID on a published row (that ID names a
public fact, not the organizer). Disclose in preview help that publishing
cannot be recalled by TWO and that audit outlives the draft.

## 4. Bounded examples

Draft modal (organizer-typed, validated before preview):

```text
name: "Friday Melee Night"
starts_at: "2026-11-06T19:00:00-05:00"  -> 2026-11-07T00:00:00.000Z
ends_at: "2026-11-06T22:00:00-05:00"    -> 2026-11-07T03:00:00.000Z
channel_key: "arena-voice"  (exactly one of channel_key / location)
description: "Round robin, all skill levels. Bring a controller."
```

Preview reply (ephemeral, exact rendering bound by the content hash):

```text
Publish this public event? Every member can see it.
Friday Melee Night — <t:1793486400:F> to <t:1793497200:F>
In Arena Voice. 12 of 100 active events used.
[Publish this event] [Cancel]
```

Published reply: "Published Friday Melee Night to the Events tab."
Failed reply (all failures share this shape, reason varies):
"Could not publish. <bounded reason> Nothing was created." Unknown outcome:
"The publish result is unconfirmed. Check the Events tab before trying again."

Agent-facing envelope, `schema=events.native_publish.v1`: `mode`
(`preview`|`confirm`), `outcome`, `reason`, `retryable`, `next_action`;
confirm context may carry guild/draft/content hash, otherwise omit identity.
Human replies stay plain language.

| Outcome / reason | Retry / next action |
| --- | --- |
| `preview / confirmation_required` | false / confirm same hash or cancel |
| `published / created` | false / none; bound response + mirror row only |
| `noop / already_published` | false / none; existing Discord event ID returned |
| `noop / cancelled` | false / none; no store call |
| `stale / preview_changed_or_expired` | false / fresh preview |
| `denied / organizer_or_context_mismatch` | false / invoke as organizer in the guild |
| `unavailable / event_or_access_unavailable` | false / verify channel/access; no existence oracle |
| `unavailable / active_cap_reached` | false / cancel or wait for an active event to end |
| `unavailable / rate_limited` | true / fresh preview next UTC minute |
| `unavailable / publish_refused` | false / fix fields per reason; Discord rejected pre-wire |
| `unavailable / commit_unknown` | false / inspect Events tab, never blindly re-publish |
| `unavailable / store_or_mirror_failed` | true / inspect first; fence retained, no success claim |

## 5. Deterministic future scenario matrix

Planned oracles, not executed tests. Fixture: guild G, organizer O with
Manage Events, member M without it, voice channel C, mirror active count 12;
reset per row. All cases preserve other guilds/members, RSVP rows, reminder
fences and zero public sends/DMs/pings. Disposable DBs, independent
connections/barriers, injected clocks/POST failures and the shared Discord
double only; never staging/production databases.

| ID | Controlled order / required outcome |
| --- | --- |
| P01 | Valid modal -> preview -> confirm => one POST, bound ID, mirror row, one audit; reply names the event only |
| P02 | Second confirm same draft => `already_published`, zero new POST/audit; mapping returned |
| P03 | Two devices confirm one draft at a barrier => one publish, one mapping; exactly one POST/audit |
| P04 | Confirm with edited fields vs preview hash => stale; fresh preview required; zero POST |
| P05 | DM/foreign guild/M-as-invoker/creator-without-Manage-Events => denied; no disclosure or writes |
| P06 | O loses Manage Events or membership after preview => refused at confirm; draft stays previewed |
| P07 | C deleted / O loses View Channel / bot loses Manage Events => refused; zero POST |
| P08 | Start passes between preview and confirm => stale; never shifted or auto-republished |
| P09 | Mirror count at 100 => refused pre-wire; a full calendar files nothing |
| P10 | EXTERNAL location >100 chars / both placements / end <= start => typed field refusal; zero POST |
| P11 | Restart/boot-nonce rotation/expired or forged control => stale; no clock-rewind revival |
| P12 | Discord 400/403 on POST => `publish_refused`, draft stays previewed; 429/5xx pre-wire => retryable, zero claim |
| P13 | Lost POST acknowledgement => `commit_unknown`; no auto retry; inspect path decides, never a blind re-POST |
| P14 | Bound response then mirror failure => fence retained, failure reported; Discord state disclosed honestly |
| P15 | Discord-side delete after publish => ledger stays published + tombstone note; never recreated |
| P16 | Discord-side edit after publish => observed by poll only; ledger hash unchanged; no TWO write-back |
| P17 | Same name/second draft => allowed; identity is draft ID, never name matching |
| P18 | 20 budgeted writes in the same UTC minute => next publish rate-limited; new minute eligible; previews free |
| P19 | DST pair: two occurrences across the fall-back week publish at their own absolute instants; no 604 800 s step |
| P20 | Erase O / purge expired audit => permitted rows removed, published Discord IDs retained; M/G2 unchanged |

Run now from repository root:

```sh
python3 -m doctest -v docs/event-native-publish-spec.md
python3 scripts/check_docs_index.py --json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
git diff --check
```

These validate the oracle below, discovery, paths and whitespace only, not
P01-P20, SQL safety, member value, deployment or authority. No new runtime
tests are authored here.

## 6. Offline oracle

No executable CLI/API is registered. These are **proposed typed results**,
not commands for a guild. The check has no environment selector,
credentials, network, database, Discord client or side effect; no staging or
live pilot is authorized. The doctest runs a pure reference model of the
rules above and asserts each fixture; it proves the **specified rules are
consistent**, not SQL, REST or deployed behavior. The oracle output schema is
`events.native_publish.oracle.v1`, intentionally distinct from the §4 result
envelope: a test-harness report, not a result envelope.

```pycon
>>> import json
>>> CAP, BUDGET, WINDOW = 100, 20, 300
>>> def preview(d):
...     # fail closed: every missing key behaves as denied
...     if not d.get("guild_ok"): return "UNAVAILABLE", "verify_access"
...     if not d.get("announcements"): return "ANNOUNCEMENTS_DISABLED", "enable_announcements"
...     if d.get("member") != "present": return "MEMBER_UNAVAILABLE", "verify_membership"
...     if not d.get("manage_events"): return "MISSING_MANAGE_EVENTS", "ask_organizer"
...     if len(d.get("name", "")) == 0 or len(d.get("name", "")) > 100:
...         return "BAD_NAME", "fix_name_length"
...     if d.get("ends", 0) <= d.get("starts", 0): return "ENDS_BEFORE_STARTS", "fix_times"
...     if d.get("starts", 0) <= d.get("now", 0): return "START_IN_PAST", "pick_future_start"
...     if bool(d.get("channel")) == bool(d.get("location")):
...         return "PLACE_AMBIGUOUS", "pick_exactly_one_place"
...     if len(d.get("location") or "") > 100: return "LOCATION_TOO_LONG", "shorten_location"
...     if len(d.get("description") or "") > 1000: return "DESCRIPTION_TOO_LONG", "shorten_description"
...     if d.get("active", 0) >= CAP: return "ACTIVE_CAP_REACHED", "wait_for_slot"
...     return "OK", "confirm_same_hash"
>>> def confirm(c, led):
...     # led: {} | {"state": ...}; one draft, advisory lock held
...     if c.get("state") != "previewed": return "refuse", "NOT_PREVIEWED", 0
...     if c.get("hash") != led.get("hash"): return "stale", "PREVIEW_CHANGED", 0
...     if c.get("now", 0) >= c.get("expiry", 0): return "stale", "PREVIEW_EXPIRED", 0
...     if c.get("nonce") != led.get("nonce"): return "stale", "NONCE_ROTATED", 0
...     if not c.get("manage_events", False): return "refuse", "AUTHORITY_LOST", 0
...     if c.get("member") != "present": return "refuse", "MEMBER_LOST", 0
...     if not c.get("channel_ok", False): return "refuse", "CHANNEL_LOST", 0
...     if not c.get("bot_ok", False): return "refuse", "BOT_MANAGE_EVENTS_LOST", 0
...     if c.get("starts", 0) <= c.get("now", 0): return "stale", "START_PASSED", 0
...     if c.get("active", 0) >= CAP: return "refuse", "ACTIVE_CAP_REACHED", 0
...     if c.get("budget_used", 0) >= BUDGET: return "refuse", "RATE_LIMITED", 0
...     if led.get("published"): return "noop", "ALREADY_PUBLISHED", 0
...     if c.get("post") == "unknown": return "unknown", "COMMIT_UNKNOWN", 1
...     if c.get("post") == "refused": return "refuse", "PUBLISH_REFUSED", 0
...     if c.get("mirror") == "failed": return "unavailable", "MIRROR_FAILED", 1
...     return "publish", "CREATED", 1
>>> OK = dict(guild_ok=True, announcements=True, member="present", manage_events=True,
...           name="Friday Melee Night", starts=200, ends=300, now=100, channel="arena-voice",
...           active=12)
>>> pre = [
...     ("ok", OK, "OK"),
...     ("missing_evidence", {}, "UNAVAILABLE"),
...     ("announcements_disabled", dict(OK, announcements=False), "ANNOUNCEMENTS_DISABLED"),
...     ("member_not_present", dict(OK, member="left"), "MEMBER_UNAVAILABLE"),
...     ("no_manage_events", dict(OK, manage_events=False), "MISSING_MANAGE_EVENTS"),
...     ("name_empty", dict(OK, name=""), "BAD_NAME"),
...     ("name_too_long", dict(OK, name="n" * 101), "BAD_NAME"),
...     ("ends_before_starts", dict(OK, ends=200), "ENDS_BEFORE_STARTS"),
...     ("start_in_past", dict(OK, starts=100), "START_IN_PAST"),
...     ("both_places", dict(OK, location="Park"), "PLACE_AMBIGUOUS"),
...     ("no_place", dict(OK, channel=None), "PLACE_AMBIGUOUS"),
...     ("location_too_long", dict(OK, channel=None, location="l" * 101), "LOCATION_TOO_LONG"),
...     ("description_too_long", dict(OK, description="d" * 1001), "DESCRIPTION_TOO_LONG"),
...     ("cap_reached", dict(OK, active=100), "ACTIVE_CAP_REACHED"),
... ]
>>> for name, case, want in pre:
...     assert preview(case)[0] == want, name
>>> assert all(preview(c)[1] for _, c, _ in pre)
>>> LED = {"state": "previewed", "hash": "h", "nonce": "n"}
>>> GOOD = dict(state="previewed", hash="h", nonce="n", manage_events=True, member="present",
...             channel_ok=True, bot_ok=True, starts=200, now=100, expiry=100 + WINDOW,
...             active=12, budget_used=0, post="ok", mirror="ok")
>>> con = [
...     ("publish", GOOD, ("publish", "CREATED", 1)),
...     ("replay_mapping", dict(GOOD), ("noop", "ALREADY_PUBLISHED", 0)),
...     ("changed_fields", dict(GOOD, hash="h2"), ("stale", "PREVIEW_CHANGED", 0)),
...     ("expired", dict(GOOD, now=400, expiry=400), ("stale", "PREVIEW_EXPIRED", 0)),
...     ("nonce_rotated", dict(GOOD, nonce="n2"), ("stale", "NONCE_ROTATED", 0)),
...     ("authority_lost", dict(GOOD, manage_events=False), ("refuse", "AUTHORITY_LOST", 0)),
...     ("member_lost", dict(GOOD, member="left"), ("refuse", "MEMBER_LOST", 0)),
...     ("channel_lost", dict(GOOD, channel_ok=False), ("refuse", "CHANNEL_LOST", 0)),
...     ("bot_lost", dict(GOOD, bot_ok=False), ("refuse", "BOT_MANAGE_EVENTS_LOST", 0)),
...     ("start_passed", dict(GOOD, starts=100), ("stale", "START_PASSED", 0)),
...     ("confirm_cap_reached", dict(GOOD, active=100), ("refuse", "ACTIVE_CAP_REACHED", 0)),
...     ("budget_spent", dict(GOOD, budget_used=20), ("refuse", "RATE_LIMITED", 0)),
...     ("post_refused", dict(GOOD, post="refused"), ("refuse", "PUBLISH_REFUSED", 0)),
...     ("post_unknown", dict(GOOD, post="unknown"), ("unknown", "COMMIT_UNKNOWN", 1)),
...     ("mirror_failed", dict(GOOD, post="ok", mirror="failed"), ("unavailable", "MIRROR_FAILED", 1)),
...     ("not_previewed", dict(GOOD, state="abandoned"), ("refuse", "NOT_PREVIEWED", 0)),
... ]
>>> for name, case, want in con:
...     led = dict(LED, published=(name == "replay_mapping"))
...     assert confirm(case, led) == want, name
>>> assert all(w == 0 for _, _, (d, _, w) in con if d in ("refuse", "stale", "noop"))
>>> assert all(w <= 1 for _, _, (_, _, w) in con)
>>> assert CAP == 100 and BUDGET == 20 and WINDOW == 300
>>> names = [n for n, _, _ in pre] + [n for n, _, _ in con]
>>> assert len(names) == 30 and len(set(names)) == 30
>>> print(json.dumps({"schema": "events.native_publish.oracle.v1", "fixtures": len(names), "status": "pass"}, sort_keys=True))
{"fixtures": 30, "schema": "events.native_publish.oracle.v1", "status": "pass"}

```

Fixtures the reference model cannot express need future protected acceptance
tests with barriers, an injected clock and REST fault injection against the
real callers in disposable fixtures: concurrent confirm/confirm interleavings
(the per-draft lock plus the published mapping), confirm versus authority
revocation on both sides of the POST, duplicate delivery of one confirm,
unknown-outcome inspect ordering, per-guild isolation of locks, and rollback
to an older binary. Zero tests is not proof; none are created here.

## 7. Smallest future slice, acceptance gates and kill rule

One possible later slice: one modal plus ephemeral preview/confirm handler via
the existing router, one additive publish-ledger table with the §2 state
machine and per-draft advisory lock, reuse of the existing member write
budget, audit seam and single-event GET, and minimum compatibility work. No
recurrence, edit/cancel-after-publish, drift display, DM, calendar, RSVP
change, series template or follow-on build card. If the ledger, claim fence
or revalidation scope cannot fit a small reviewed slice, choose A; never cut
a fence to make it fit.

Before **any build or acceptance-test allocation**, require exact commit/
hash-bound CEO intent disposition through the existing host/CEO path, actual
organizer/member alignment, and an independent **non-author** CISO disposition
of organizer authority, the public-creation boundary, the one-way rule and
the frozen preview copy. The author of this proposal is the CISO agent, so
its own authorship satisfies none of those gates and self-disposition is
refused: the CEO routes disposition to an independent reviewer. Roadmap,
routing receipt, this analysis and spec merge satisfy none of those gates.
Material edits invalidate any disposition. No decision is fabricated here;
current runtime/production stay unchanged. A future build separately requires
admission, protected acceptance tests, exact-head CI and independent review.

By **2026-11-02**, seek authorized evidence of distinct organizer value over
the dashboard upsert path and explicit acceptance/revision of P01-P20 and the
oracle. No new analytics or member recruitment is authorized. Missing value,
intent, independent boundary disposition or bounded implementation =>
**A/no build**, not automatic activation. Acceptance of a spec alone never
creates a build task.

ACT: publish one indexed proposal and matching task spec. WHY: give organizers
one explicit, reviewable path from a TWO plan to the native Events tab without
silent public writes. UNDO: reviewed docs revert only; no runtime/data changes
to reverse. Any later code rollback stops publish admission and preserves the
ledger and every existing delivery/recovery fence.
