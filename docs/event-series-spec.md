# Events: a recurring series template for weekly game nights

Status: **Proposed — design only**, 2026-10-26; review by **2026-11-02**.
This is the single editable repository contract; the task spec mirrors its
exact commit and SHA-256. Merge is not acceptance, host alignment, build
approval or activation. No runtime, acceptance-test, migration, permission,
credential, publication, deployment or spending change is included.

## 1. Decision, evidence and alternatives

Consumer: the organizer who sets up the weekly game night. Hypothesis: set
up once — game, voice channel, description — and each occurrence inherits
the template instead of rebuilding the event every week.

Evidence for the need is goal-level, not member-level. The TWO Community
goal targets one weekly event held 4 consecutive weeks and lists one
recurring event as work. No organizer outreach ran; weekly-manual setup
cost and setup pain are unmeasured. The
[sesh manual](https://sesh.fyi/manual/) is adjacent comparison only
(recurring schedules with skip/end variants exist as a vendor pattern);
no live vendor inspection was conducted for this proposal, and a vendor
capability is not TWO demand, retention or conversion evidence. Demand
for automation over manual weekly creation is **unproven**.

| Option | Member effect / cost | Disposition |
| --- | --- | --- |
| A. No build; weekly-manual | Organizer recreates one native event per week (~minutes). Zero new schema, locks, writes or tests | Standing fallback until acceptance; choose it if setup pain, value or safe bounds fail |
| B. Minimal weekly series template | One template; each occurrence inherits fields and needs one explicit confirm to bind a native event. New series rows, locks, audit and a human-to-internal-action bridge | Submit for exact intent/security review, not implementation |
| C. Auto-create / daily / monthly | Silent event creation; daily cadence; month arithmetic | Auto-create rejected (§8). Daily rejected: a noise vector with no goal need. Monthly deferred: month-boundary rules (Jan 31 + 1 month, "last Friday") and unproven demand |

CEO/host review must accept the assumptions in §11 and the cuts below.
Then the existing host intent-alignment path must record alignment
against the accepted revision. **No implementation or acceptance-test
cards before both.** A saved queue/intake receipt alone is not alignment.
This design neither creates a host executor nor widens authority if that
route is unavailable. Independent security review, exact-head CI/review
and normal release gates still apply later; no owner step, new spend,
production enablement or routine approval hop is added.

## 2. Verified source map (not future capabilities)

Inspected repository head: `8b53ffb84181df3265707196f9b0afca855d2b14`.
Source inspection only; no deployment, guild, message or database state
was inspected.

- `crates/core/src/rsvp.rs:506,508,515-516`: **1,000 rows/event**,
  **20 writes/member/guild/UTC minute**, 90-day RSVP / 365-day audit
  floors. `crates/cutover/migrations/0160_rsvp.sql:24-30` keys
  `event_rsvps` by `(guild_id,event_id,user_id)`; no occurrence column.
- `crates/core/src/scheduled_events.rs:31,38,46-61`: event mirror polls
  every 10 minutes, at most 100 rows; statuses
  `scheduled|active|completed|cancelled`. Reminder admission reads
  `status`, `channel_id`, `starts_at`, `name`
  (`crates/core/src/event_reminder_store.rs:64-123`).
- `crates/core/src/router.rs:704-709,721-725`: staff check-in requires
  Manage Events behind Scorecard; ordinary RSVP is member-open behind
  Announcements. `crates/core/src/router.rs:710-716`: `/schedule` family
  requires Manage Server behind Automations.
- [Command reference](commands.md) `/schedule`: `every-minutes` 60–525600
  recurs a message. [Queue order](schedule-queue-order.md): `at` is
  one-off only and never combines with recurrence; "No timezone profile,
  natural-language parsing or DST-aware recurrence is implied."
- No series concept exists: `series` in `crates/` is only metric-series
  guards and SQL `generate_series`. Sibling specs treat series/template
  IDs as unsupported: [seats](rsvp-capacity-spec.md) requires distinct
  native occurrence identities; [withdrawal](rsvp-withdrawal-spec.md) is
  event-level only; [roles](event-role-spec.md) excludes recurring-series
  roles.
- `crates/discord/src/internal_events.rs:195-240`: the existing signed
  internal-action executor already performs event Upsert (create/update),
  Cancel and Read with a mirror refresh (`:291-294`). A build reuses
  this executor; the new part is the human-confirmation bridge to it,
  not a raw Discord path. Its input carries name, starts_at, ends_at,
  description and exactly one placement
  (`crates/core/src/internal_actions.rs:1180-1196,1240-1317`):
  `channel_key` resolved through the operator allowlist — never a raw
  caller channel id — or an external `location`; channel placement emits
  `entity_type` 2, a voice-channel event
  (`crates/discord/src/internal_events.rs:140-142`).
- [Reminders](event-reminders.md): one channel-only attempt per event in
  the final 15 minutes; the permanent `event:reminder:<guild>:<event>`
  fence survives rescheduling and restarts.

## 3. Identity, template and overrides

Series identity is TWO-owned `(guild_id, series_id)`, a bot-generated
UUIDv4. Occurrence identity is `(guild_id, series_id,
occurrence_index)`, index starting at 1 with no reuse. An occurrence
binds **at most one** native Discord scheduled event id; the binding is
not identity. Rescheduling preserves identity; a delete/recreate in
Discord is a rebind to a new native id, never identity reuse. RSVP rows
stay keyed by native event id, so per-occurrence RSVPs partition with no
change to `event_rsvps`.

Template fields, frozen at creation except via series edit (§4): name
pattern, game label, placement, description, duration,
first start (RFC3339 with explicit offset, strictly future, at most
365 days after creation, same shape as one-off `at`), the organizer's
optional IANA display zone (flag/label only, §4), cadence (weekly only:
exact 7 × 24 h UTC steps from the first start), and the end rule (§4).
Placement is exactly one of: a configured `channel_key` whose resolved
id is verified as a **voice** channel at preview and at commit, or an
external `location` string — precisely the placements the reused
executor accepts (§2). The executor resolves keys without checking
channel type, so voice verification is new behavior in this proposal: a
build must read the channel type from the guild's channel cache at
preview and at commit, and an unknown type rejects. The template never
carries a raw channel id, and a text channel is not a placement. A
channel key that is unknown, unconfigured or resolves to a non-voice
channel is rejected before preview with zero writes. This proposal adds no executor widening: accepting an
organizer-picked raw channel would be a separate design needing its own
security disposition, and is rejected here. If the operating guild has
no suitable configured key, the series uses `location` placement or
stays at option A. Creation writes a `draft` series row and proposes
occurrence #1; nothing is scheduled and no native event exists until
the organizer confirms the preview.

Per-occurrence overrides apply to **unmaterialized** occurrences only and
are stored on the occurrence row: start shift, placement swap (to
another configured voice `channel_key` or a location), skip (§4), short
note. A series
edit changes the template for future unmaterialized occurrences only,
bumping every unmaterialized occurrence's revision (§6); it never
rewrites an already-created native event or an already-decided
occurrence. Materialized occurrences are edited in Discord; the mirror
observes them.

## 4. Occurrence lifecycle

Series states: `draft → active → ended`, plus `deleted` and `expired`.
Activation is the first decided occurrence: `draft` becomes `active`
when occurrence #1 is confirmed or skipped; a failed or stale first
confirm does not activate. A `draft` with no decided occurrence expires
7 days after creation (`expired`, computed — see below) and frees its
quota (§7). Occurrence states: `proposed → confirmed | skipped`;
`confirmed → held | kept | cancelled`. `ended`/`held`/`expired` are
computed at read time from the end rule, timestamps and the mirror, not
by a background sweeper: this proposal adds no tick, queue or sweep.

- **Skip/cancel-one.** Skipping a `proposed` occurrence marks it
  `skipped`: no native event is ever created for it. Skipping a
  `confirmed` occurrence needs a second explicit confirmation to cancel
  the bound native event through the existing Cancel path; if the
  organizer declines, the occurrence stays bound (`kept`). Never a
  silent cancel.
- **End-after-N vs end instant.** Both may be set at creation; at least
  one is required (no infinite series). N is 2–52. The end rule is an
  **end instant** (RFC3339 with explicit offset), normalized to UTC, at
  most 365 days after the first start and **strictly after it**: an end
  instant at or before the first start is rejected at validation with
  `invalid_input / end_before_first_start` and zero writes (no draft is
  created). The end instant is **exclusive**: an occurrence starts
  only when its start instant is strictly before the end instant, so
  end = first start admits nothing (and rejects as above), while
  end = first start + 1 minute admits occurrence #1 only. There is no
  date-only form and no standalone timezone for the boundary: the
  instant carries its own offset. The series ends at whichever of N or
  the end instant is hit first. N counts **decided** occurrences
  (`confirmed`, `skipped`,
  `kept`, `held`, `cancelled`), so skips cannot stretch a series
  forever. Proposed UX examples use N = 4 to match the community-goal
  streak, but the organizer picks.
- **DST/timezone.** Cadence is fixed 7-day UTC steps: occurrences keep
  the same UTC time each week while local wall time may drift ±1 h
  across a DST boundary. To make that drift computable the template
  optionally stores one IANA zone name (e.g. `America/New_York`): the
  **display zone**, meaning the organizer's chosen member-facing local
  time for this series — it never anchors the cadence and is not a
  member profile. An unknown zone name rejects at creation with
  `invalid_input / unknown_display_zone` and zero writes. Computing per-occurrence offsets needs a tz database; that
  dependency is part of this proposal's review surface, and if it is
  refused the display zone drops and only the generic warning below
  remains.
  When set, the preview flags occurrences whose local offset **in the
  display zone** differs from occurrence #1's offset in that zone
  ("local time shifts on this occurrence"). When unset, previews render
  in UTC and carry a fixed note that local start times may shift across
  a daylight-saving change; no data source exists from which a shift
  could be derived, so no per-occurrence shift is claimed. The
  organizer may override starts individually either way.
  Wall-clock-anchored recurrence (zone anchors the cadence,
  same-local-time-every-week) is deferred: no timezone profile exists
  today, and it needs its own dependency and semantics review.
- **Reschedule one vs all.** Rescheduling one unmaterialized occurrence
  is an override (§3). "All" is a series edit and touches only future
  unmaterialized occurrences. The bot never edits a created native
  event's fields automatically, ever.
- **Deletion.** Deleting a series removes the template and its
  unmaterialized occurrence rows. Bound native events and their RSVP
  rows are untouched; existing retention and erasure govern them.
  Deleting one occurrence follows skip semantics. No member tombstone
  beyond the audit row (§7).

## 5. RSVP inheritance and reminder interplay

RSVP is **fresh per occurrence**: a Going on week 1 is not consent to
week 4's roster or reminder, and copying responses across occurrences
would mark members who never saw the new occurrence. Fresh is also the
zero-migration choice (rows key by native event id) and matches the
event-level withdrawal contract. No carry, no copy, no "same as last
week" default. A member RSVPs on each occurrence's native event.

Reminder interplay reuses the existing per-event fence unchanged: each
occurrence's native event gets the standard final-15-minutes,
channel-only, single-attempt reminder keyed by its own native id. No
series digest, no cross-occurrence roster, no fence reset on skip/end.
Rescheduling an unmaterialized occurrence has no reminder effect (no
fence exists yet). Editing a materialized occurrence in Discord keeps
the existing rule: admission already committed stays decided; no second
attempt is armed.

## 6. Organizer authority, races and organizer loss

Authority is **role-based**, reusing the staff-event boundary: every
series mutation (create, confirm, edit, skip, end, delete) requires the
configured guild, Announcements enabled, live human membership, and
freshly evaluated **Manage Events** at preview **and** at commit —
creator identity alone is never authority, and no new organizer role or
event-creator exception is created. Any current Manage Events holder
may act on any series in that guild.

Edit races serialize on a proposed per-series advisory lock
(`event_series:<guild>:<series>`) plus a per-occurrence revision. A
preview binds the **full rendered payload it authorizes** — name,
description, start, end (start + duration), placement — as
`(series_id, occurrence_index, revision, payload_hash, expires_at)`,
where `payload_hash` is the SHA-256 of the canonical rendering of
guild, series, occurrence, revision and the five payload fields.
**Any series edit bumps the revision of every
unmaterialized occurrence** (and therefore changes every future
occurrence's hash), so a template edit between preview and confirm
cannot commit a payload the confirmer never saw. Previews expire after
**15 minutes** (`expires_at`). Commit under the lock requires an exact
match on all five values, freshly evaluated authority (§6), placement
re-verification (§3) and an unexpired `expires_at`, else `STALE`
(reason `preview_changed_or_expired`) with no write. Series writes take
only the series lock and row locks: they write no RSVP or reminder
rows, so they do not join the save/withdrawal/reminder canonical lock
order; if a future slice couples them, it must join that order before
enabling.

Organizer loss mid-series is a non-event for the series: because
authority is role-based, any other current holder continues confirming
occurrences. There is no auto-transfer and no self-service transfer in
this slice. A series with zero eligible holders sits unchanged (no
automatic mutation) and reads as dormant.

## 7. Storage, audit, retention and bounded costs

Proposed tables only; no DDL, migration number or compatibility plan is
assigned here. `event_series` holds the template, end rule, display
zone, creation timestamp and state; `event_series_occurrences` holds
one row per occurrence (index, revision, start, placement, status,
override, bound native id). A second small table (or columns) records
consumed confirmations per occurrence so the budgets below are
enforceable across proposals. Hard caps: **≤3 non-terminal series per
guild — `draft` and `active` both count; `ended`, `deleted` and
`expired` do not**, ≤52 occurrences per series, **1 native event per
occurrence, at most 1 create-confirm and at most 1 cancel-confirm per
occurrence** (§8). A further confirm of either kind refuses with
**zero writes**: `noop / already_decided` when the occurrence is decided
or that kind's budget is spent, and `stale /
preview_changed_or_expired` when the occurrence is undecided but the
preview no longer matches (§6) — the decided/budget check wins when
both apply. No indefinite series, no background sweep, no digest
fan-out.

Audit each mutation as `event_series.*` (create, confirm, skip, edit,
end, delete, rebind) with guild, actor, series, occurrence and outcome;
no titles, descriptions, rosters or free text. Proposed home is
`operational_audit_log` with an explicit retention exemption until
series end + 90 days, mirroring the attendee-role precedent — the
required prune change must land before any build. Retention ends are
defined for every terminal state: `active` series rows persist until
end + 90 days, where _end_ is the earlier of the end instant and the
instant the decided count reached N (for an ended series: whichever end
rule fired); **a `draft` that never activates persists until its
expiry (creation + 7 days) + 90 days**; `deleted` rows persist until
deletion + 90 days. Draft expiry is computed at read and at create-time
quota checks (an expired draft frees its quota slot without any
sweeper); physical row deletion after these retention ends is left to
the future prune change, which is a build prerequisite (§7, §11).
Occurrence rows hold no member data; series rows carry the creating
organizer id and need a `member_erasure_plan.json` entry with the same
disposition before build. RSVP and reminder rows keep their existing
retention and erasure paths untouched.

## 8. Native Discord event writes

Occurrences may bind native Discord scheduled events **only** through
explicit per-occurrence organizer confirmation of an exact preview
rendering the **complete native payload literally: name, description,
start, end, placement** — bound at commit by `payload_hash` (§6).
Confirmations come in two kinds with separate budgets (§7): a
**create-confirm** (at most one per occurrence) authorizes exactly one
create of the bound payload through the existing signed internal
executor (§2), followed by its mirror refresh; a **cancel-confirm** (at
most one per occurrence, only for an already materialized occurrence,
§4) authorizes exactly one Cancel of the bound native event. A second
create-confirm, a second cancel-confirm, or a stale/expired preview
refuses with zero Discord writes (outcome per the §7 selection rule).
There is no bulk confirm, no auto-create of occurrence #1, no silent
creation on a tick, and no automatic edit or cancel of a native event.
A commit whose database outcome is unknown authorizes no write until
re-inspected (S13); blind retry is never authorized, and the
hash/expiry/revision fence makes an inspected re-confirm converge on
`already_decided` rather than a duplicate create. Volume is bounded by the §7 caps plus
existing REST pacing. This human bridge to a Discord write path needs
independent CISO disposition before any build; a refused disposition
means option A, not a quieter auto-create.

## 9. Private UX and agent handoff

Proposed command sketch, illustrative only — no command is registered
by this document: `/series-create` (template + end rule),
`/series-confirm` (exact preview of the next occurrence),
`/series-skip`, `/series-end`, `/series-status` (own-guild, staff-only
except a member-safe next-occurrence read, still to be scoped). All
organizer replies ephemeral, ≤400 UTF-16 units, no pings or DMs.

| Outcome / reason | Bounded private example | Retry / next_action |
| --- | --- | --- |
| `preview / confirmation_required` | Template or occurrence preview with the full native payload (name, description, start, end, placement), caps and — when a display zone is set — per-occurrence shift flags, else a generic UTC daylight-saving warning | false / confirm_exact_preview or cancel |
| `saved / series_created` | “Series saved. No event exists yet — confirm occurrence 1 to create it.” | false / confirm_next or none |
| `saved / occurrence_confirmed` | “Occurrence N created from the template.” | false / none; confirmed commit only |
| `saved / occurrence_skipped` | “Occurrence N skipped. No event will be created for it.” | false / none |
| `saved / series_ended` | “Series ended. Existing events are unchanged.” | false / none |
| `noop / already_decided` | “That occurrence is already decided. Nothing changed.” | false / none |
| `stale / preview_changed_or_expired` | “This preview is out of date. Preview again.” | false / fresh_preview |
| `invalid_input / end_before_first_start` | “The end must be after the first start. Nothing was saved.” | false / fix_end_rule |
| `denied / manage_events_required` | “You need the Manage Events permission for series actions.” | false / ask_staff |
| `unavailable / event_or_access_unavailable` | “That series or channel is unavailable. Nothing changed.” | false / verify_access; no existence oracle |
| `unavailable / commit_unknown` | “The result is unconfirmed. Check series status before trying again.” | false / inspect_status, never blindly retry |

Agent handoff envelope (typed result, not a command):
`schema=events.series.v1` with `guild_id`, `series_id`,
`occurrence_index`, `mode` (`preview|apply`), `outcome`, `reason`,
`retryable`, `next_action`, and exact spec commit/hash. The three id
fields are **nullable**: in a `denied` response all three are `null`
(never a redacted non-null sentinel), and the remaining fields carry
the denial (`outcome=denied`, `reason=manage_events_required`,
`retryable=false`, `next_action=ask_staff`); only `schema`, `mode` and
the spec commit/hash stay populated alongside them. A consumer must
accept `null` ids exactly when
`outcome=denied`, and must reject any other shape mismatch. A denied
response therefore validates against the schema without exposing
whether the named series exists. Agents may preview but cannot supply
the human confirmation.

## 10. Deterministic scenario matrix and reference oracle

S01–S17 are **planned assertions, not executed tests**. Fixtures use
fake UTC clocks, synthetic guilds/actors/channels, deterministic stores
and a recording one-shot transport against disposable test databases
only; never staging or production.

| Case | Required result |
| --- | --- |
| S01 create, first confirm | Template saved as draft; occurrence 1 proposed; one create-confirm creates exactly 1 native event with the literal previewed payload (name, description, start, end, placement); RSVP starts empty; activation on the first decided occurrence |
| S02 skip before create | Skipped occurrence never creates; decided count advances (and activates a draft); later occurrences unaffected |
| S03 skip after create | Cancel-confirm (a separate budget from the create-confirm) cancels via the Cancel path; declined cancel keeps the binding with zero writes; a second cancel-confirm refuses with zero writes |
| S04 end-after-N | Series ends when decided count hits N even with skips mixed in; no N+1 proposal |
| S05 end instant | End instant before the first start — and end = first start — reject as `invalid_input / end_before_first_start` with zero writes (no draft); end = first start + 1 minute admits occurrence #1 only; no occurrence starts at or after the end instant (exclusive); earlier of N/end wins |
| S06 DST boundary | UTC cadence keeps the same UTC time each week. With a display zone, flags mark occurrences whose offset in that zone differs from occurrence #1's; without one, previews show UTC plus a generic daylight-saving warning and claim no per-occurrence shift. Individual override moves one start only |
| S07 series edit | Template change applies to future unmaterialized occurrences only and bumps every unmaterialized occurrence's revision; materialized and decided rows byte-identical |
| S08 occurrence override | Start/placement override moves one unmaterialized occurrence; cadence of the rest unchanged; a placement that is an unknown key, a raw id, or a non-voice channel rejects before preview with zero writes |
| S09 organizer loss | Any other Manage Events holder confirms; no transfer flow; zero-holder series sits dormant with no auto-mutation |
| S10 RSVP fresh | Week-1 Going never appears on week 2; each occurrence admits its own reminder fence or skips empty |
| S11 reminder interplay | Post-admission Discord edit arms no second attempt; skip/end never resets a fence |
| S12 confirm races | Two concurrent create-confirms of one preview yield one bound native event and one refusal with zero writes (`already_decided` once decided, `STALE` when the preview no longer matches, per §7); the same preview resubmitted with a different payload hash is `STALE` |
| S13 unknown commit | Unknown DB result authorizes zero Discord writes; lost wire receipt is terminal uncertain, never blindly retried (inspected re-confirm converges on `already_decided`) |
| S14 caps | A 4th non-terminal series in a guild (drafts count: e.g. 3 unconfirmed drafts exhaust the quota; an expired draft (7 days, undecided) frees its slot), a 53rd occurrence, a 2nd create-confirm and a 2nd cancel-confirm for one occurrence each refuse with zero writes |
| S15 deletion | Series delete removes template + unmaterialized rows; native events and RSVP rows untouched |
| S16 restart | Decided rows fence repeats; proposed rows need a fresh preview; no automatic send on boot |
| S17 edit between preview and confirm | Preview occurrence 2; another holder edits the template name/description/duration for future occurrences; confirming the old preview returns `stale / preview_changed_or_expired` with zero Discord writes. Same for a preview confirmed after its 15-minute expiry |

The doctest below is a pure reference model of the transition, counting
and flag rules. It proves the **specified rules are consistent**, not
SQL, REST or deployed behavior.

```sh
python3 -m doctest -v docs/event-series-spec.md
python3 scripts/check_docs_index.py --json
```

```pycon
>>> import json
>>> WEEK = 7 * 24 * 3600
>>> def starts(first, n):
...     return [first + i * WEEK for i in range(n)]
>>> starts(1_000_000, 3)
[1000000, 1604800, 2209600]
>>> DECIDED = ("confirmed", "skipped", "kept", "held", "cancelled")
>>> def decide(state, action, cancel_native=False):
...     # returns (new_state, reason, discord_writes); every write needs its own confirm
...     if state == "proposed" and action == "confirm":
...         return ("confirmed", "BOUND", 1)
...     if state == "proposed" and action == "skip":
...         return ("skipped", "SKIPPED_UNMATERIALIZED", 0)
...     if state == "confirmed" and action == "skip" and cancel_native:
...         return ("cancelled", "CANCELLED_WITH_CONFIRM", 1)
...     if state == "confirmed" and action == "skip":
...         return ("kept", "KEPT_DECLINED_CANCEL", 0)
...     if state in DECIDED and action in ("confirm", "skip"):
...         return (state, "ALREADY_DECIDED", 0)
...     return (state, "STALE", 0)
>>> tr = [
...     ("first_confirm", ("proposed", "confirm", False), ("confirmed", "BOUND", 1)),
...     ("skip_before", ("proposed", "skip", False), ("skipped", "SKIPPED_UNMATERIALIZED", 0)),
...     ("skip_after_yes", ("confirmed", "skip", True), ("cancelled", "CANCELLED_WITH_CONFIRM", 1)),
...     ("skip_after_no", ("confirmed", "skip", False), ("kept", "KEPT_DECLINED_CANCEL", 0)),
...     ("double_confirm", ("confirmed", "confirm", False), ("confirmed", "ALREADY_DECIDED", 0)),
...     ("skip_skipped", ("skipped", "skip", False), ("skipped", "ALREADY_DECIDED", 0)),
... ]
>>> for name, args, want in tr:
...     assert decide(*args) == want, name
>>> assert all(w == 0 for _, _, (_, _, w) in tr[1:2] + tr[3:4] + tr[4:])
>>> def ended_by_count(statuses, n):
...     return sum(s in DECIDED for s in statuses) >= n
>>> assert ended_by_count(["held", "skipped", "confirmed"], 4) is False
>>> assert ended_by_count(["held", "skipped", "confirmed", "kept"], 4) is True
>>> def dst_flag(first_offset_min, occ_offset_min):
...     # offsets are the display zone's, evaluated at each occurrence's
...     # start; with no display zone set the flag is never computed
...     return first_offset_min != occ_offset_min
>>> assert dst_flag(-300, -300) is False and dst_flag(-300, -240) is True
>>> import hashlib
>>> def payload_hash(guild, series, occ, revision, name, desc, start, end, place):
...     parts = [str(x) for x in (guild, series, occ, revision, name, desc or "", start, end, place)]
...     return hashlib.sha256("\x1f".join(parts).encode()).hexdigest()
>>> PREVIEW_TTL = 15 * 60
>>> def confirm_ok(preview, current, now):
...     # preview/current: (series, occ, revision, payload_hash, expires_at)
...     return preview == current and now < preview[4]
>>> h1 = payload_hash("g1", "s1", 2, 7, "Game Night", "bring snacks", "2026-11-08T19:00:00Z", "2026-11-08T21:00:00Z", "key:voice")
>>> h2 = payload_hash("g1", "s1", 2, 8, "Game Night (renamed)", "bring snacks", "2026-11-08T19:00:00Z", "2026-11-08T21:00:00Z", "key:voice")
>>> p = ("s1", 2, 7, h1, 1_000_000 + PREVIEW_TTL)
>>> assert confirm_ok(p, p, 1_000_000) is True
>>> # template edit between preview and confirm: name changed and the
>>> # unmaterialized revision was bumped, so the hash and revision differ
>>> assert confirm_ok(p, ("s1", 2, 8, h2, p[4]), 1_000_000) is False
>>> # name-only drift would still break the hash even without a bump
>>> assert h1 != payload_hash("g1", "s1", 2, 7, "Other Night", "bring snacks", "2026-11-08T19:00:00Z", "2026-11-08T21:00:00Z", "key:voice")
>>> # an expired preview refuses even when untouched
>>> assert confirm_ok(p, p, p[4]) is False
>>> def create_confirm(state, used_create, used_cancel, action):
...     # separate lifetime budgets: <=1 create-confirm, <=1 cancel-confirm;
...     # returns (new_state, reason, used_create, used_cancel)
...     if action == "create" and state == "proposed" and used_create == 0:
...         return ("confirmed", "BOUND", 1, used_cancel)
...     if action == "cancel" and state == "confirmed" and used_cancel == 0:
...         return ("cancelled", "CANCELLED_WITH_CONFIRM", used_create, 1)
...     return (state, "ALREADY_DECIDED", used_create, used_cancel)
>>> assert create_confirm("proposed", 0, 0, "create") == ("confirmed", "BOUND", 1, 0)
>>> assert create_confirm("confirmed", 1, 0, "cancel") == ("cancelled", "CANCELLED_WITH_CONFIRM", 1, 1)
>>> assert create_confirm("confirmed", 1, 0, "create") == ("confirmed", "ALREADY_DECIDED", 1, 0)
>>> assert create_confirm("cancelled", 1, 1, "cancel") == ("cancelled", "ALREADY_DECIDED", 1, 1)
>>> def propose_starts(first, end):
...     # end instant is exclusive; end <= first admits nothing and is
...     # rejected at validation (end_before_first_start), not proposed
...     if end is not None and end <= first:
...         return []
...     out, t = [], first
...     while end is None or t < end:
...         out.append(t); t += WEEK
...         if len(out) >= 52:
...             break
...     return out
>>> F = 1_000_000
>>> assert propose_starts(F, F - 1) == []
>>> assert propose_starts(F, F) == []
>>> assert propose_starts(F, F + 60) == [F]
>>> assert propose_starts(F, F + WEEK) == [F]
>>> SEVEN_DAYS = 7 * 24 * 3600
>>> def quota_full(series, cap=3):
...     # series: [(state, age_seconds)]; draft and active count, an
...     # undecided draft expires at 7 days, terminal states never count
...     counted = sum(1 for state, age in series
...                   if state == "active" or (state == "draft" and age < SEVEN_DAYS))
...     return counted >= cap
>>> assert quota_full([("draft", 0), ("draft", 0), ("draft", 0)]) is True
>>> assert quota_full([("draft", 8 * 24 * 3600), ("draft", 0), ("draft", 0)]) is False
>>> assert quota_full([("active", 30 * 24 * 3600)] * 3) is True
>>> assert quota_full([("active", 30 * 24 * 3600), ("active", 0), ("draft", 0)]) is True
>>> assert quota_full([("ended", 0), ("deleted", 0), ("expired", 0), ("draft", 0)]) is False
>>> names = [n for n, _, _ in tr]
>>> assert len(tr) == 6 and WEEK == 604800 and len(DECIDED) == 5
>>> print(json.dumps({"schema": "events.series.oracle.v1", "fixtures": len(names) + 15, "status": "pass"}, sort_keys=True))
{"fixtures": 21, "schema": "events.series.oracle.v1", "status": "pass"}

```

Cases the model cannot express need future protected acceptance tests
with barriers, an injected clock and transport fault injection: the
per-series lock against concurrent confirms, preview/commit authority
revalidation, mirror-observed rebinds, unknown-commit fencing and
restart recovery. Zero tests is not proof; none are created here.

## 11. Smallest future slice, acceptance gates and kill rule

One possible later slice, and only if every gate below passes: create
a weekly series (template + end rule), confirm the next occurrence
into exactly one native event through the existing executor, skip-one
and end-now — with fresh RSVP, existing reminder fences and the §7
caps. Deferred to later slices: occurrence start/placement overrides UI,
wall-clock anchoring, monthly cadence, digests, transfers. No
companion dashboard, calendar, seat/waitlist coupling, bulk confirm or
follow-on build card. If the storage, lock, audit-prune, erasure-plan
and executor-bridge work cannot fit one small reviewed slice, choose
A; never cut a fence to make it fit.

Before **any build or acceptance-test allocation**, require exact
commit/hash-bound CEO intent disposition through the existing host/CEO
path, actual organizer alignment, and independent non-author CISO
disposition of the human confirmation bridge, retained rows, erasure,
audit-prune change and frozen private copy. Roadmap, routing receipt,
this author's analysis and spec merge satisfy none of those gates.
Material edits invalidate the disposition. No decision is fabricated
here; current runtime/production stay unchanged. Future build
separately requires admission, protected acceptance tests, exact-head
CI and independent review.

By **2026-11-02**, seek authorized evidence of organizer setup pain
under weekly-manual operation plus explicit acceptance or revision of
S01–S17 and the coupled store seams. Run the game nights manually
first: if four manual weeks show no setup pain, or intent, independent
boundary disposition or a bounded implementation is missing, the
outcome is **A/no build**, not automatic activation. Acceptance of a
spec alone never creates a build task.

ACT: publish one indexed proposal and a matching task spec. WHY: decide
the series shape — weekly-first fixed cadence, fresh RSVP, per-write
confirmation — before any build discussion. UNDO: reviewed docs revert
only; no runtime/data changes to reverse. Any later code rollback stops
series admission and preserves every existing RSVP/reminder fence.
