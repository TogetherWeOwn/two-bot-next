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
  Cancel and Read with a mirror refresh. A build reuses this executor;
  the new part is the human-confirmation bridge to it, not a raw Discord
  path.
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
pattern, game label, voice/text channel, description, duration,
first start (RFC3339 with explicit offset, strictly future, ≤365 days,
same shape as one-off `at`), cadence (weekly only: exact 7 × 24 h UTC
steps from the first start), and the end rule (§4). Creation writes a
`draft` series row and proposes occurrence #1; nothing is scheduled and
no native event exists until the organizer confirms the preview.

Per-occurrence overrides apply to **unmaterialized** occurrences only and
are stored on the occurrence row: start shift, channel swap, skip (§4),
short note. A series edit changes the template for future
unmaterialized occurrences only; it never rewrites an already-created
native event or an already-decided occurrence. Materialized occurrences
are edited in Discord; the mirror observes them.

## 4. Occurrence lifecycle

Series states: `draft → active → ended`, plus `deleted`. Occurrence
states: `proposed → confirmed | skipped`; `confirmed → held | kept |
cancelled`. `ended`/`held` are computed at read time from the end rule
and the mirror, not by a background sweeper: this proposal adds no tick,
queue or sweep.

- **Skip/cancel-one.** Skipping a `proposed` occurrence marks it
  `skipped`: no native event is ever created for it. Skipping a
  `confirmed` occurrence needs a second explicit confirmation to cancel
  the bound native event through the existing Cancel path; if the
  organizer declines, the occurrence stays bound (`kept`). Never a
  silent cancel.
- **End-after-N vs end-on-date.** Both are set at creation; at least one
  is required (no infinite series). N is 2–52; the end date is ≤365 days
  out. The series ends at whichever is hit first. N counts **decided**
  occurrences (`confirmed`, `skipped`, `kept`, `held`, `cancelled`), so
  skips cannot stretch a series forever. Proposed UX examples use N = 4
  to match the community-goal streak, but the organizer picks.
- **DST/timezone.** Cadence is fixed 7-day UTC steps: occurrences keep
  the same UTC instant while local wall time may drift ±1 h across a DST
  boundary. The preview flags the affected occurrences explicitly
  ("local time shifts on this occurrence"). The organizer may override
  those starts individually. Wall-clock-anchored recurrence (IANA zone,
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
preview binds `(series_id, occurrence_index, revision, start, channel)`;
commit under the lock requires an exact match, else `STALE` with no
write. Series writes take only the series lock and row locks: they
write no RSVP or reminder rows, so they do not join the
save/withdrawal/reminder canonical lock order; if a future slice
couples them, it must join that order before enabling.

Organizer loss mid-series is a non-event for the series: because
authority is role-based, any other current holder continues confirming
occurrences. There is no auto-transfer and no self-service transfer in
this slice. A series with zero eligible holders sits unchanged (no
automatic mutation) and reads as dormant.

## 7. Storage, audit, retention and bounded costs

Proposed tables only; no DDL, migration number or compatibility plan is
assigned here. `event_series` holds the template, end rule and state;
`event_series_occurrences` holds one row per occurrence (index,
revision, start, channel, status, override, bound native id). Hard caps:
≤3 active series per guild, ≤52 occurrences per series, 1 native event
per occurrence, 1 confirm per occurrence. No indefinite series, no
background sweep, no digest fan-out.

Audit each mutation as `event_series.*` (create, confirm, skip, edit,
end, delete, rebind) with guild, actor, series, occurrence and outcome;
no titles, descriptions, rosters or free text. Proposed home is
`operational_audit_log` with an explicit retention exemption until
series end + 90 days, mirroring the attendee-role precedent — the
required prune change must land before any build. Series rows persist
until end + 90 days. Occurrence rows hold no member data; series rows
carry the creating organizer id and need a `member_erasure_plan.json`
entry with the same disposition before build. RSVP and reminder rows
keep their existing retention and erasure paths untouched.

## 8. Native Discord event writes

Occurrences may bind native Discord scheduled events **only** through
explicit per-occurrence organizer confirmation of an exact preview
(name, start, channel, description rendered literally). One confirmation
authorizes exactly one create through the existing signed internal
executor (§2), followed by its mirror refresh. There is no bulk
confirm, no auto-create of occurrence #1, no silent creation on a tick,
and no automatic edit or cancel of a native event. Cancelling a
materialized occurrence needs its own separate confirmation. Volume is
bounded by the §7 caps plus existing REST pacing. This human bridge to
a Discord write path needs independent CISO disposition before any
build; a refused disposition means option A, not a quieter auto-create.

## 9. Private UX and agent handoff

Proposed command sketch, illustrative only — no command is registered
by this document: `/series-create` (template + end rule),
`/series-confirm` (exact preview of the next occurrence),
`/series-skip`, `/series-end`, `/series-status` (own-guild, staff-only
except a member-safe next-occurrence read, still to be scoped). All
organizer replies ephemeral, ≤400 UTF-16 units, no pings or DMs.

| Outcome / reason | Bounded private example | Retry / next_action |
| --- | --- | --- |
| `preview / confirmation_required` | Template or occurrence preview with exact native payload, caps and DST flag | false / confirm_exact_preview or cancel |
| `saved / series_created` | “Series saved. No event exists yet — confirm occurrence 1 to create it.” | false / confirm_next or none |
| `saved / occurrence_confirmed` | “Occurrence N created from the template.” | false / none; confirmed commit only |
| `saved / occurrence_skipped` | “Occurrence N skipped. No event will be created for it.” | false / none |
| `saved / series_ended` | “Series ended. Existing events are unchanged.” | false / none |
| `noop / already_decided` | “That occurrence is already decided. Nothing changed.” | false / none |
| `stale / preview_changed_or_expired` | “This preview is out of date. Preview again.” | false / fresh_preview |
| `denied / manage_events_required` | “You need the Manage Events permission for series actions.” | false / ask_staff |
| `unavailable / event_or_access_unavailable` | “That series or channel is unavailable. Nothing changed.” | false / verify_access; no existence oracle |
| `unavailable / commit_unknown` | “The result is unconfirmed. Check series status before trying again.” | false / inspect_status, never blindly retry |

Agent handoff envelope (typed result, not a command):
`schema=events.series.v1` with `guild_id`, `series_id`,
`occurrence_index`, `mode` (`preview|apply`), `outcome`, `reason`,
`retryable`, `next_action`, and exact spec commit/hash. Redact ids in
denied responses. Agents may preview but cannot supply the human
confirmation.

## 10. Deterministic scenario matrix and reference oracle

S01–S16 are **planned assertions, not executed tests**. Fixtures use
fake UTC clocks, synthetic guilds/actors/channels, deterministic stores
and a recording one-shot transport against disposable test databases
only; never staging or production.

| Case | Required result |
| --- | --- |
| S01 create, first confirm | Template saved as draft; occurrence 1 proposed; one confirm creates exactly 1 native event with the literal previewed payload; RSVP starts empty |
| S02 skip before create | Skipped occurrence never creates; decided count advances; later occurrences unaffected |
| S03 skip after create | Second confirm cancels via the Cancel path; declined cancel keeps the binding with zero writes |
| S04 end-after-N | Series ends when decided count hits N even with skips mixed in; no N+1 proposal |
| S05 end-on-date | No occurrence proposed with start past the end date; earlier of N/date wins when both set |
| S06 DST boundary | UTC cadence keeps the same instant; affected previews carry the local-shift flag; individual override moves one start only |
| S07 series edit | Template change applies to future unmaterialized occurrences only; materialized and decided rows byte-identical |
| S08 occurrence override | Start/channel override moves one unmaterialized occurrence; cadence of the rest unchanged |
| S09 organizer loss | Any other Manage Events holder confirms; no transfer flow; zero-holder series sits dormant with no auto-mutation |
| S10 RSVP fresh | Week-1 Going never appears on week 2; each occurrence admits its own reminder fence or skips empty |
| S11 reminder interplay | Post-admission Discord edit arms no second attempt; skip/end never resets a fence |
| S12 confirm races | Two concurrent confirms of one preview yield one bound native event and one STALE; same key with different binding is CONFLICT |
| S13 unknown commit | Unknown DB result authorizes zero Discord writes; lost wire receipt is terminal uncertain, never retried |
| S14 caps | 4th active series, 53rd occurrence and 2nd confirm of one occurrence refuse with zero writes |
| S15 deletion | Series delete removes template + unmaterialized rows; native events and RSVP rows untouched |
| S16 restart | Decided rows fence repeats; proposed rows need a fresh preview; no automatic send on boot |

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
...     return first_offset_min != occ_offset_min
>>> assert dst_flag(-300, -300) is False and dst_flag(-300, -240) is True
>>> def confirm_ok(preview, current):
...     return preview == current
>>> p = ("s1", 2, 7, "2026-11-08T19:00:00Z", "c9")
>>> assert confirm_ok(p, p) is True
>>> assert confirm_ok(p, ("s1", 2, 8, "2026-11-08T19:00:00Z", "c9")) is False
>>> names = [n for n, _, _ in tr]
>>> assert len(tr) == 6 and WEEK == 604800 and len(DECIDED) == 5
>>> print(json.dumps({"schema": "events.series.oracle.v1", "fixtures": len(names) + 6, "status": "pass"}, sort_keys=True))
{"fixtures": 12, "schema": "events.series.oracle.v1", "status": "pass"}

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
caps. Deferred to later slices: occurrence start/channel overrides UI,
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
S01–S16 and the coupled store seams. Run the game nights manually
first: if four manual weeks show no setup pain, or intent, independent
boundary disposition or a bounded implementation is missing, the
outcome is **A/no build**, not automatic activation. Acceptance of a
spec alone never creates a build task.

ACT: publish one indexed proposal and a matching task spec. WHY: decide
the series shape — weekly-first fixed cadence, fresh RSVP, per-write
confirmation — before any build discussion. UNDO: reviewed docs revert
only; no runtime/data changes to reverse. Any later code rollback stops
series admission and preserves every existing RSVP/reminder fence.
