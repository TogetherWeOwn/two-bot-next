# Events: one private upcoming-event agenda

Status: **Deferred to 2026-12** (owner decision 2026-11-01). Revisit with the
events-and-RSVP theme; no build until then. Original proposal: written
2026-10-24. No commands, storage, migrations, permissions, sends or runtime
tests are added. Merge publishes a proposal, not permission to implement or
activate it. This file is the single repository contract; review requests bind
its commit and SHA-256.

## 1. Decision and evidence

Consumer: a member keeping track of game-night commitments. Hypothesis: one
private place to see upcoming visible events with their own saved RSVP, without
collecting links or exporting a roster. No customer validation is claimed.

[sesh's May 2021 update](https://sesh.fyi/blog/may-2021-update/) documents
"Simply DM sesh `!list` to get a full listing of all your upcoming events."
Read 2026-10-24. That is vendor capability evidence of a comparable flow, not
validated TWO demand. TWO's current own-response read is per-event only; a
cross-event agenda was explicitly left out of that scope.

| Option | Value and cost | Disposition |
| --- | --- | --- |
| No build; current per-event lookup | No new disclosure boundary, index, or verification load; members keep one lookup per event | Safe default until acceptance; retain if the boundary below is not accepted |
| Private in-guild upcoming list | One explicitly invoked, ephemeral, paged read of own commitments; needs a per-member index and page-bounded verification | Smallest option that tests this distinct hypothesis; recommend for **spec review**, not build |
| DM list | Matches the vendor flow but opens a new DM surface with delivery state, unsolicited-message risk, and cross-guild identity questions | Do not add; no new DM path in this proposal |

No notification, automatic DM, public/shareable page, calendar/OAuth link,
add-to-calendar/export, quiet hours, event creation, new data collection, or
spend. Silence (no saved row) is **unknown**, never Declined or absent, and a
failed read is never rendered as an empty agenda.

## 2. Verified baseline and missing seams

Inspected repository head: `0e904ee2f6cd17d5aa54fa35b2730154791ee1d3`.

- `crates/core/src/rsvp.rs:38-48,103-163` has Going/Interested/Declined and
  snowflake event identity; `:506-508` caps **1,000 rows/event** and **20
  writes/member/guild/UTC minute**. `crates/cutover/migrations/0160_rsvp.sql:24-34`
  keys `event_rsvps` by `(guild_id,event_id,user_id)` with one secondary index
  on `(guild_id,event_id,status)`. There is **no per-member index**.
- `crates/core/src/rsvp_store.rs:234-258` lists one event's RSVPs; every other
  reader is per-event or per-row. No per-member cross-event reader exists.
- `crates/discord/src/rsvp.rs:307-351` renders per-event totals plus the
  invoking member's saved response and Discord-localized saved time, ephemeral,
  historical, with no live GET. `crates/core/src/feature_commands.rs:260` and
  `crates/core/src/router.rs:661,932` publish `rsvp-attendance` behind
  Announcements; `crates/core/src/help.rs:199-200` states live availability is
  not checked. The write path checks live membership before event lookup.
- `crates/cutover/migrations/0300_website_contract.sql:120-135` mirrors
  `scheduled_events` as `(guild_id,event_id,name,starts_at,channel_id,
  description,status,updated_at)` with an index on `(guild_id,starts_at)`.
  `crates/core/src/scheduled_events.rs:31,38` polls every 10 minutes with at
  most 100 rows per snapshot; `0450`/`0452` add a claim gate and tombstones.
  [Attendee reminders](event-reminders.md) already join this mirror with saved
  RSVPs and no live lookup.
- `crates/cutover/src/member_erasure_plan.json:144-147` deletes `event_rsvps`
  by `(guild_id,user_id)`; erased rows simply vanish from reads.

No live guild, permission, deployment, shared-database writer, or host workflow
was inspected. If page-bounded verification or a per-member index cannot stay
small and coherent, stop this proposal instead of scanning or guessing.

## 3. Identity, authorization, and disclosure boundary

Event identity is `(guild_id, discord_scheduled_event_id)`; member identity is
that key plus `discord_user_id` taken only from the authenticated interaction,
never a caller-supplied member ID, name, title, or start time. Reschedules
preserve identity. An ambiguous series/template ID spanning occurrences is
unsupported, not permission to merge commitments.

Before disclosing anything, in order: configured guild from interaction
context, Announcements enabled, and **current human membership verified live**,
following the existing write-path rule. Absent membership fails closed as
`UNAVAILABLE`; a membership check that cannot complete (transport, timeout, or
malformed evidence) fails closed as retryable `STORE_UNAVAILABLE`, never as a
denial. This is stricter than per-event totals because an agenda discloses
event existence across events, not one caller-named event's counts.

Each listed row must then pass effective channel/event authorization: the
mirror row is Scheduled/Active as of its poll time with `starts_at` in the
future, and the member can access that event's channel, checked live for that
row. Event status and location are mirror evidence, not live Discord reads, so
a cancel or move after the last poll can still list until the next poll,
labeled with the mirror time. Checks run only for rows on the requested page
(at most 10), never a roster sweep, and each row fails closed independently:
withheld rows are omitted, and the reply states that verification was applied
without per-row reasons. Never infer channel visibility from RSVP ownership,
and never disclose private event metadata for an unverified row, another
member's response, an attendee roster, or events the mirror reports canceled,
completed, past, or deleted as upcoming.

## 4. Proposed read contract

One explicitly invoked, ephemeral, read-only, idempotent read. It writes
nothing, consumes no RSVP quota or audit row, and sends no notification, DM,
or channel message. Availability and saved facts stay separate:

- Show only own `event_rsvps` rows with status Going/Interested joined to
  mirror rows that verify as upcoming. Declined is not a commitment and is
  excluded. Mirror statuses Canceled/Completed, past starts, and tombstoned or
  absent mirror rows are excluded from "upcoming".
- Own Going/Interested rows with no matching current mirror row (deleted/moved
  events) surface only as a metadata-free count such as "2 saved responses are
  not matched to a current event", never with names or times. Declined rows
  stay excluded even when their mirror row is missing.
- Sort by `starts_at` ascending with `event_id` as the stable tiebreak. Page
  size is 10 with a 1-based page option. At most 100 own rows are considered;
  beyond that the reply says the list is truncated instead of silently cutting.
- Times render as Discord `<t:epoch:f>` tags from the mirror's UTC `starts_at`;
  localization and DST are the client's job and no bot timezone math is added.
- Empty is explicit ("no upcoming events with your saved response"); a store
  or verification failure says the agenda is unavailable, never empty.
- Erasure needs no new path: erased rows are gone from both the join and the
  unmatched count, with no retained agenda history. No new opt-out list exists;
  Declined or deleting the RSVP is the member's existing control.

## 5. Bounds, retention, and privacy/security findings

Proposed bounds tighten existing limits: page at most 10 rows; own-row scan at
most 100 behind a new `(guild_id,user_id)` index; mirror IN-list at most 100;
live membership plus per-row channel checks at most 11 Discord reads per
invocation, each short-budgeted; no whole-guild scan and no per-member timer.
Estimated incremental spend is **$0** on the current process and Postgres;
this is a sizing target, not measured headroom.

| Data / threat | Proposed control and remaining finding |
| --- | --- |
| Cross-event existence disclosure | Ephemeral self-only reply, live membership gate, per-row verification; still a new read boundary across events the member did not name, needing disposition |
| Stale mirror (cancel/move unseen for up to 10 minutes) | Label every reply "as of <mirror time>"; a Discord cancellation not yet polled is not known locally, matching the existing reminder limit |
| Channel access lost after RSVP | Per-row check withholds the row; unknown evidence withholds rather than guessing, so an outage can hide a real commitment until recovery |
| Unmatched-count oracle | Count only, no metadata; identical shape for deleted, moved, and never-mirrored events |
| Whole-guild scan | Refuse the read shape without the per-member index; exact query plan is a build-review artifact, not asserted here |
| Erasure/restore | No agenda store to purge; restored snapshots reuse existing mirror/RSVP retention and erasure, with no agenda history to reconcile |
| Small-group inference | Exact own commitments are shown to their owner by design; no other member's data appears, so no new inference surface beyond the owner's own screen |

**CISO proposal assessment:** acceptable to circulate for intent review with the
above controls; **not** a grant, independent approval of our own boundary
request, or runtime security acceptance. Blocking findings before any later
build/enable: (1) accept or narrow the cross-event read boundary, including the
stricter-than-totals membership gate; (2) accept page-bounded live verification
with fail-closed withholding, or narrow to saved commitments only; (3) require
the per-member index and a proven bounded query plan. If those need persistent
history, new messages, or broad authority, stop this proposal. No credential or
permission is requested by this document. Later security-sensitive changes
still require independent applicable security disposition and exact-head tests.

## 6. Agent-facing result and offline fixture contract

No new executable CLI/API is registered. Future adapters must discover this file
through the [documentation map](README.md); these are **proposed typed results**,
not commands to run on a guild. Stable envelope: `schema=events.agenda.v1`,
`guild_id`, self-only `member_id`, `page`, `rows` of `(event_id, starts_at_epoch,
own_status, verified)`, `unmatched_saved`, `withheld`, `mirror_as_of`, `reason`,
`retryable`, `next_action`. `UNAVAILABLE` replies omit rows. No raw SQL/Discord errors.

| reason | retryable / next_action |
| --- | --- |
| `AGENDA` | false / `view_page` |
| `EMPTY` | false / `keep_per_event_lookup` |
| `UNAVAILABLE` | false / `verify_access` (collapse missing/foreign/admission-denied; per-row channel denial withholds the row instead) |
| `TRUNCATED` | false / `narrow_with_per_event_lookup` |
| `STALE` | false / `retry_after_next_poll` |
| `STORE_UNAVAILABLE` | true / `retry_after_backoff` (also when the membership gate itself cannot complete) |
| `COMMIT_UNKNOWN` | true / `inspect_per_event_state` |

This design's offline check has no environment selector, credentials, network,
database, Discord client, side effects, or wrong-flag production path. No
staging/live pilot or mutation is authorized. Future execution cannot be
reached by combining flags on this check.

The following deterministic **fixture oracle**, not an implementation, uses
guild `g1`, member `a`, mirror time `T`, and UTC epoch starts. Each row resets
that fixture; `shown` counts verified upcoming rows, `withheld` counts rows
failing verification, `unmatched` counts own Going/Interested rows with no
current mirror row.
All rows require zero writes, zero notifications, and zero other-member data.

| Case | Controlled event / expected assertion |
| --- | --- |
| empty | No own Going/Interested rows -> EMPTY, 0/0/0 |
| going | One Going + Scheduled future verified -> AGENDA, 1/0/0 with epoch tag |
| declined | One Declined only -> EMPTY, excluded from commitments |
| canceled | Going + mirror Canceled -> excluded, 0/0/0, not upcoming |
| deleted | Going + no mirror row -> unmatched count 1, no metadata |
| denied | Going + channel access denied -> withheld 1, 0 shown |
| unknown | Going + verification timeout -> withheld 1, never guessed visible |
| stale | Mirror older than one poll + moved event -> STALE label with mirror time |
| dst | Starts across a DST fallback render from UTC epochs, 3600 s apart |
| pages | 12 verified rows, page 1 -> 10 shown; page 2 -> 2 shown |
| cap | 101 own rows -> TRUNCATED, first 100 considered |
| outage | Store failure before read -> STORE_UNAVAILABLE, never EMPTY |
| erased | Member erased -> 0/0/0 with no retained agenda history |
| foreign | Guild `g2` or another member's ID -> UNAVAILABLE, 0/0/0 |

Runnable offline document check (Python stdlib; from repo root):

```sh
python3 -m doctest -v docs/event-agenda-spec.md
python3 scripts/check_docs_index.py --json
```

The doctest checks the **specified oracle's consistency**, not real SQL plans,
authentication, rendering, erasure, outage handling, or deployed behavior.
Future protected acceptance tests must implement barriers, injected clocks,
and store/REST faults against actual callers in disposable fixtures; zero
tests is not proof. No acceptance/implementation task is created here.

```pycon
>>> import json
>>> # (case, reason, shown, withheld, unmatched); abstract expected snapshots
>>> oracle = [
...     ("empty", "EMPTY", 0, 0, 0),
...     ("going", "AGENDA", 1, 0, 0),
...     ("declined", "EMPTY", 0, 0, 0),
...     ("canceled", "EMPTY", 0, 0, 0),
...     ("deleted", "AGENDA", 0, 0, 1),
...     ("denied", "AGENDA", 0, 1, 0),
...     ("unknown", "AGENDA", 0, 1, 0),
...     ("stale", "STALE", 0, 0, 0),
...     ("dst", "AGENDA", 2, 0, 0),
...     ("pages_p1", "AGENDA", 10, 0, 0),
...     ("pages_p2", "AGENDA", 2, 0, 0),
...     ("cap", "TRUNCATED", 10, 0, 0),
...     ("outage", "STORE_UNAVAILABLE", 0, 0, 0),
...     ("erased", "EMPTY", 0, 0, 0),
...     ("foreign", "UNAVAILABLE", 0, 0, 0),
... ]
>>> assert len({row[0] for row in oracle}) == len(oracle) == 15
>>> assert all(s <= 10 for _, _, s, _, _ in oracle)
>>> assert all((s, w, u) == (0, 0, 0) for _, r, s, w, u in oracle if r in ("EMPTY", "STALE", "STORE_UNAVAILABLE", "UNAVAILABLE"))
>>> assert all((w, u) == (0, 0) or r == "AGENDA" or r == "TRUNCATED" for _, r, _, w, u in oracle)
>>> assert dict((n, r) for n, r, _, _, _ in oracle)["deleted"] == "AGENDA"
>>> # DST fallback 2026-10-25T01:00Z: two UTC instants straddle the transition
>>> eu_before, eu_after = 1792888200, 1792891800
>>> assert eu_after - eu_before == 3600
>>> assert f"<t:{eu_before}:f>" == "<t:1792888200:f>"
>>> us_before, us_after = 1793511000, 1793514600
>>> assert us_after - us_before == 3600
>>> assert 12 == 10 + 2 and 101 > 100
>>> print(json.dumps({"schema": "events.agenda.oracle.v1", "fixtures": len(oracle), "status": "pass"}, sort_keys=True))
{"fixtures": 15, "schema": "events.agenda.oracle.v1", "status": "pass"}

```

## 7. Acceptance boundary, review metric, and undo

Record CEO spec acceptance **and** host intent alignment against the exact
revision before any implementation subtasks, protected test handoff, or live
pilot. Route the decision brief through the President/COO to the CEO and the
existing host intent path; a queue/intake receipt is not alignment. The CISO
findings above must receive an independent boundary disposition, not a
self-approved grant. No request should be sent directly to the owner or an
invented host executor.

The decision must accept or revise: ephemeral in-guild read-only; live
membership plus page-bounded per-row verification; Going/Interested only with
Declined excluded; mirror staleness labeled with its poll time; a new
per-member index with a proven bounded plan; and no DM, calendar, share, or
notification surface. On any material revision, bind a fresh review request and
alignment to that revision.

By 2026-10-31, metric is **all 14 fixture-matrix rows have agreed or explicitly
rejected outcomes, and the three blocking findings have written disposition**.
This measures design completeness, not engagement. Proceed only if visibility
and truthful freshness are provable without new spend or permissions;
otherwise narrow to saved commitments or stop this proposal, not RSVP or the
project. Default while unresolved is no build/current per-event lookup; no
timeout auto-approval. A future demand experiment requires its own
hypothesis/metric/date, not inferred success from this document.

ACT: publish this proposed contract and its offline oracle for review. WHY:
bound the cross-event read before considering an agenda feature. UNDO: reviewed
docs revert; there is no operational change to undo. A later independently
accepted build must keep admission dark until its own tests/security/release
gates pass; disabling future admission must preserve reads, expiry, and erasure
behavior, not delete history. No production tests, migrations, credentials, or
activation here.
