# Events: one private add-to-calendar handoff

Status: **proposed, design only**. Written 2026-10-24; review by 2026-10-31.
No commands, components, attachments, storage, migrations, endpoints,
permissions, sends or runtime tests are added, and no export is created.
Merge publishes a proposal, not permission to implement or activate it. This file
is the single repository contract; review requests bind its commit and SHA-256.

## 1. Decision and evidence

Consumer: a member carrying one game-night commitment into their own calendar.
Hypothesis: they can do it without retyping the title and time, with no account
link and no roster. No customer validation is claimed.

[sesh](https://sesh.fyi/) advertises add-to-calendar links and automatic
timezone conversion (read 2026-10-24). That is vendor capability evidence of a
comparable flow, not validated TWO demand and not a design to copy: sesh's
mechanism and data handling were not inspected.

| Option | Value and cost | Disposition |
| --- | --- | --- |
| No build; current time tag | `/rsvp-attendance` already shows the member's saved time as a Discord-localized tag; members copy it by hand | Safe default; retain unless the boundary below is accepted |
| One-event RFC 5545 `.ics` attachment | One ephemeral file, expected to import in common calendar apps (unverified hypothesis; section 7), nothing leaves the reply except the file; needs a **file-in-reply seam this repo lacks** (section 2) | Smallest private handoff that meets the constraints; recommend for **spec review**, not build |
| Add-to-calendar link | No file, but needs a vendor URL carrying title and time in a query string, one vendor per link, and (for the vendor URL shape this author knows) an end time that TWO does not have; a TWO-hosted link would be a new public endpoint | Do not add; vendor URL behavior was **not verified** here |

Out of scope and not proposed: OAuth or calendar APIs, subscriptions, a feed
(`webcal://` or otherwise), recurring series, attendee or roster export, DMs,
notifications, reminders, update synchronization, new endpoints, spend.

## 2. Verified baseline and missing seams

Inspected repository head: `89b26ae050ffba9fb409622ffd54d31cdc79e7fc`.

- `crates/core/src/rsvp.rs:86-100` validates a 17-20 digit snowflake event id and
  `Going`/`Interested`/`Declined`. `crates/discord/src/rsvp.rs:303-336` renders
  `/rsvp-attendance` ephemeral with the invoker's own saved row from the
  per-event read and a Discord `<t:epoch:f>` tag; no live event lookup. The
  write path checks live membership before the event lookup (`:345-357`).
- `crates/cutover/migrations/0300_website_contract.sql:120-135` mirrors
  `scheduled_events(guild_id,event_id,name,starts_at,channel_id,description,
  status,updated_at)`. There is **no end-time column**: an exported event has a
  start only. `crates/core/src/scheduled_events.rs:43-49` statuses are
  scheduled/active/completed/cancelled; `:106-112` fields; `:148-166` refuses a
  name over 100 or a description over 1,000 characters; `:249-259` normalizes an
  offset start to UTC millis, e.g. `+01:00` to `...Z`. `updated_at` is the
  poller's UTC-millis observation time (`website_store.rs:280`). The mirror
  polls every 10 minutes (`scheduled_events.rs:31`); `0452` adds tombstones.
- **Missing seam 1:** no code path attaches a file to an interaction reply.
  `Attachment` appears only as an inbound command-option type
  (`crates/core/src/commands.rs:74`); the only multipart text in the tree is the
  S3 backup note (`crates/core/src/backup/s3.rs:6`), and replies are plain
  content truncated to 2,000 characters (`crates/discord/src/interactions.rs:120`).
  The recommended option therefore needs a new reply-with-file capability. That
  makes "one PR, at most one engineering day" **optimistic**; if the seam cannot
  stay small, stop this proposal rather than downgrade silently.
- **Missing seam 2:** no reusable "can this member see this channel" check was
  found for non-voice channels (`voice_permissions.rs` is voice-specific). The
  build must name or add one; it must not infer visibility from an RSVP row.

No live guild, permission, deployment or shared-database writer was inspected.
Import behavior in real calendar apps was **not** verified (section 7).

## 3. Human flow

1. **Select.** The member runs one ephemeral command with the same `event-id`
   option and snowflake validation as `/rsvp`. There is no browse or search of
   other events, no roster, and no event title in the option.
2. **Private preview** (read-only, idempotent, writes nothing, sends no
   message/DM/notification). It shows exactly what the file will contain: the
   title, the start as a Discord `<t:epoch:F>` tag **and** an explicit UTC ISO
   instant, the line "no end time: it imports as a zero-length event", and the
   snapshot time (`updated_at`). It carries one **Export** button bound to the
   guild, event, invoking member and a short digest of the previewed fields.
3. **Explicit export.** Only the bound member can press it. The handler repeats
   every check in section 4 against current state and recomputes the digest.
   Unchanged: reply once, ephemeral, with the file. Changed: **stale preview**,
   no file; show a fresh preview. The member must press again.
4. **After export** the reply says the file is a snapshot: a later edit,
   reschedule or cancellation is **not** reflected in the calendar copy, and
   re-running the command shows the current state. No promise of attendance,
   seat, or reminder is made; Going/Interested is not shown in the file.

Special cases, all decided before any file is built:

- **Canceled** (status cancelled): no file. Say it is canceled and that any copy
  already imported must be removed by hand. No `METHOD:CANCEL` file: that would
  imply synchronization this proposal does not offer.
- **Missing** (no mirror row, tombstoned, deleted): `UNAVAILABLE`, no metadata.
- **Started or past** (`starts_at` not in the future, or status active/completed):
  no file; not offered as upcoming.
- **No end time:** never invented, no default duration; the file has no `DTEND`
  or `DURATION`, which RFC 5545 section 3.6.1 reads as a zero-length event for a
  date-time start. The preview says so.
- **Timezone and DST:** the instant is the mirror's UTC `starts_at`, written as a
  UTC date-time (`...Z`). No `TZID`, no `VTIMEZONE`, no floating time and no bot
  timezone math; the calendar app and the Discord client localize and apply DST.
  A wall-clock hour that occurs twice at a DST fallback exports as two distinct
  UTC instants, 3,600 seconds apart (fixture `dst_us`).
- **Edited event:** a changed title or start between preview and press is a stale
  preview. After export, edits are stale-by-design and labeled so (step 4).

## 4. Authorization, fields and disclosure boundary

Identity is `(guild_id, event_id)`; the member is the authenticated interaction
user, never a supplied ID. Checks run in this order and **each fails closed**:

1. configured guild from interaction context, and the feature enabled under the
   same Announcements gate as `/rsvp-attendance`;
2. **current human membership verified live** (unknown fails closed);
3. the member has a saved `Going` or `Interested` row for this event (one
   primary-key read on `event_rsvps(guild_id,event_id,user_id)`; no new index).
   `Declined` and no row are not commitments;
4. a current mirror row for the guild with a non-empty channel the member can
   currently see. A channel-less row is refused (visibility cannot be shown;
   this is a CISO open question, not an assumed guild-wide event);
5. only then status and time: scheduled and in the future, else the
   canceled/started reasons above.

Steps 1-4 collapse every failure (not a member, no RSVP, foreign guild, deleted,
channel denied, channel-less) into one `UNAVAILABLE` reply with identical shape,
so the command is not an oracle for event existence. Only a verified viewer sees
`CANCELED`/`STARTED`.

**Exported fields, exhaustive:** `UID`, `DTSTAMP`, `DTSTART`, `SUMMARY`, plus the
calendar envelope (`VERSION`, `PRODID`, `CALSCALE`). `UID` is
`<guild_id>-<event_id>@events.two-bot.invalid`: stable across edits and
identical for every member, contains no member data, and uses the reserved
`.invalid` TLD. `DTSTAMP` is the mirror `updated_at` (not the clock), so the same
inputs yield byte-identical output. No `METHOD`, `ORGANIZER`, `ATTENDEE`,
`DESCRIPTION`, `LOCATION`, `URL`, `ATTACH`, `VALARM`, `GEO`, `CATEGORIES`, `SEQUENCE`.
The description is excluded because it is free text up to 1,000 characters that can
carry links, mentions or private detail; the channel is excluded because a
channel id or name can reveal a private channel. Whether importers replace an
earlier import with the same `UID` is **importer-dependent and unverified**.

## 5. Escaping, bounds and threat model

Rules the generator must satisfy (reference oracle in section 6):

- `SUMMARY` is text-escaped per RFC 5545 section 3.3.11 (`\`, `;`, `,`); C0/C1
  controls (including CR, LF, TAB), U+2028/U+2029 and Unicode bidi controls
  (U+061C, U+200E/U+200F, U+202A-U+202E, U+2066-U+2069) are **removed**, not
  translated, so a title can never start a new property line.
  An empty result is `FIELD_UNSAFE`: no placeholder title is invented.
- Lines are CRLF-terminated and folded at 75 octets without splitting a UTF-8
  sequence; the file is UTF-8, at most **2,048 bytes** (a 100-character title of
  4-byte characters is about 650 bytes). Over the bound is refused, not truncated.
- The attachment name is `event-<event_id>.ics` from the validated snowflake,
  never title-derived; content type `text/calendar`. Mentions are suppressed
  in the preview (existing closed outbound mention policy) and the title is
  shown escaped, with no link unfurl. No URL is generated anywhere.
- No log, metric label or audit row carries a title, member ID or file; counters
  carry only the section 6 `reason`. Export persists **nothing**. Erasure and
  restore therefore need no new path.

| Threat | Control and remaining finding |
| --- | --- |
| ICS property injection (CRLF in title adds `ATTENDEE`, `ORGANIZER`, `VALARM` with `PROCEDURE`/`EMAIL`/`AUDIO`) | Control characters removed; fixed property allowlist asserted by the parser check (fixture `inject_crlf`). Older calendar apps that honor action alarms are the reason the allowlist is part of the contract |
| Bidi/line-separator spoofing in the title | Stripped (fixture `inject_bidi`); the preview shows the sanitized value the file will carry |
| Link injection | No URL is emitted or accepted; no query string, no redirect, no per-vendor link |
| Attachment abuse / size | Fixed name, fixed `text/calendar`, 2,048-byte cap, one file, one event, no user-supplied bytes beyond the sanitized title |
| Cross-member or cross-guild leak | Authenticated member only, ordered fail-closed checks, collapsed `UNAVAILABLE`; no roster, response status, count or other member appears |
| Private channel leak | Visibility proved live per request; channel id and name are never exported |
| Replay or forwarded preview button | Button is bound to the invoking member and a field digest and re-verifies on press; a foreign press is `UNAVAILABLE` |
| Stale mirror (cancel/move unseen up to 10 minutes) | Labeled "as of <updated_at>"; a not-yet-polled cancellation is not known locally, matching [event reminders](event-reminders.md) |
| Discord retains the attachment | **Not verified here:** CDN retention and URL lifetime for ephemeral attachments. The file holds only the title and start the member can already see; CISO must accept or reject this storage boundary |
| Extra Discord reads | At most one live membership read and one channel-visibility read per invocation, short-budgeted; two primary-key Postgres reads; no scan, no timer. Spend is **$0** on the current process; a sizing target, not measured headroom |

**CISO proposal assessment:** acceptable to circulate for intent review with the
controls above; **not** a grant, not independent approval of our own boundary
request, and not runtime security acceptance. Blocking findings before any
build: (1) accept or narrow the new file-export boundary and the Discord
attachment retention; (2) name the channel-visibility check (seam 2) and decide
the channel-less case; (3) accept the field allowlist, notably excluding
description and location. If the file seam needs broad authority or persistent
state, stop this proposal. No credential or permission is requested here.

## 6. Agent-facing result and offline fixture contract

No executable CLI/API is registered. A future adapter discovers this file through
the [documentation map](README.md). **Proposed typed result:** `schema=
events.calendar.v1`, `guild_id`, self-only `member_id`, `event_id`, `reason`,
`retryable`, `next_action`, and, for `PREVIEW`/`EXPORTED` only, `summary`,
`starts_at_epoch`, `snapshot_at`, `file_bytes`. Denied replies omit all of them.
Raw SQL or Discord errors never appear. Preview is idempotent; repeating an
export yields identical bytes. Byte identity is serialization determinism, not
delivery deduplication: if an acknowledgement is lost, a retry may show the
member a duplicate attachment (see `UPLOAD_UNCERTAIN`).

| reason | retryable / next_action |
| --- | --- |
| `PREVIEW` | false / `press_export` |
| `EXPORTED` | false / `import_file` |
| `STALE_PREVIEW` | false / `review_again` |
| `CANCELED` | false / `remove_saved_copy_manually` |
| `STARTED` | false / `none` |
| `FIELD_UNSAFE` | false / `ask_staff_to_fix_title` |
| `UNAVAILABLE` | false / `check_event_id_and_rsvp` (identical for every denial) |
| `STORE_UNAVAILABLE` | true / `retry_after_backoff` |
| `VERIFICATION_UNKNOWN` | true / `retry_after_backoff` |
| `UPLOAD_UNCERTAIN` | true / `retry_export` (bytes are deterministic; delivery is not deduplicated, so a retry may show a duplicate attachment) |

The check has no environment selector, credentials, network, database, Discord
client or side effect, and no production path. It is a stdlib reference oracle
that fixes the **byte format and the fixture expectations**; the future Rust
implementation must reproduce the same bytes for the same inputs.

```sh
python3 -m doctest -v docs/event-calendar-handoff-spec.md
python3 scripts/check_docs_index.py --json
```

Import-validation expectations the doctest enforces on every generated file:
UTF-8; CRLF line ends and a final CRLF; every physical line at most 75 octets;
unfolded properties are exactly the allowlist in order; one `VEVENT`;
`DTSTART` and `DTSTAMP` match `\d{8}T\d{6}Z`; `DTSTART` equals the mirror
instant truncated to whole seconds (RFC 5545 date-times carry no fraction);
no `DTEND`/`DURATION`; same inputs give the same SHA-256.

```pycon
>>> import datetime as dt, hashlib, re
>>> STRIP = re.compile("[\x00-\x1f\x7f\x80-\x9f\u061c\u2028\u2029\u200e\u200f\u202a-\u202e\u2066-\u2069]")
>>> def basic(iso):
...     t = dt.datetime.strptime(iso, "%Y-%m-%dT%H:%M:%S.%fZ").replace(tzinfo=dt.timezone.utc)
...     # Truncate sub-second: RFC 5545 date-time carries whole seconds only.
...     return t.strftime("%Y%m%dT%H%M%SZ"), int(t.timestamp())
>>> def esc(text):
...     return STRIP.sub("", text).replace("\\", "\\\\").replace(";", "\\;").replace(",", "\\,")
>>> def fold(line):
...     out, cur, n = [], "", 0
...     for ch in line:
...         b = len(ch.encode())
...         if n + b > (75 if not out else 74):
...             out.append(cur); cur, n = "", 0
...         cur += ch; n += b
...     return "\r\n ".join(out + [cur])
>>> def render(guild, event, name, starts_at, updated_at):
...     summary = esc(name)
...     if not summary.strip():
...         return None  # FIELD_UNSAFE
...     lines = ["BEGIN:VCALENDAR", "VERSION:2.0", "PRODID:-//TogetherWeOwn//two-bot event handoff//EN",
...              "CALSCALE:GREGORIAN", "BEGIN:VEVENT", f"UID:{guild}-{event}@events.two-bot.invalid",
...              f"DTSTAMP:{basic(updated_at)[0]}", f"DTSTART:{basic(starts_at)[0]}",
...              f"SUMMARY:{summary}", "END:VEVENT", "END:VCALENDAR"]
...     body = "".join(fold(l) + "\r\n" for l in lines).encode()
...     return body if len(body) <= 2048 else None  # oversize is refused
>>> ALLOW = ["BEGIN", "VERSION", "PRODID", "CALSCALE", "BEGIN", "UID", "DTSTAMP", "DTSTART", "SUMMARY", "END", "END"]
>>> def check(body):
...     text = body.decode("utf-8")
...     assert text.endswith("\r\n") and "\n" not in text.replace("\r\n", "").replace("\r", "")
...     assert all(len(p.encode()) <= 75 for p in text.split("\r\n"))
...     props = text[:-2].replace("\r\n ", "").split("\r\n")
...     assert [p.split(":", 1)[0] for p in props] == ALLOW and props[-1] == "END:VCALENDAR"
...     assert re.fullmatch(r"DT(START|STAMP):\d{8}T\d{6}Z", props[6]) and re.fullmatch(r"DTSTART:\d{8}T\d{6}Z", props[7])
...     return dict(p.split(":", 1) for p in props[1:4] + props[5:9])
>>> G, E, T = "111111111111111111", "222222222222222222", "2026-10-24T08:00:00.000Z"
>>> # utc: a plain UTC start; no end time is ever written
>>> utc = render(G, E, "Game night", "2026-11-07T19:00:00.000Z", T)
>>> f = check(utc)
>>> assert f["DTSTART"] == "20261107T190000Z" and f["UID"] == f"{G}-{E}@events.two-bot.invalid"
>>> assert b"DTEND" not in utc and b"DURATION" not in utc and b"TZID" not in utc
>>> # idempotent: same inputs give byte-identical output
>>> assert hashlib.sha256(utc).digest() == hashlib.sha256(render(G, E, "Game night", "2026-11-07T19:00:00.000Z", T)).digest()
>>> # offset: the mirror stores UTC; +01:00 18:30 is the same instant as 17:30Z (core test vector)
>>> assert basic("2026-09-06T17:30:00.000Z")[0] == "20260906T173000Z"
>>> # dst_us: 01:30 America/New_York happens twice on 2026-11-01; UTC instants stay distinct
>>> a, b = basic("2026-11-01T05:30:00.000Z"), basic("2026-11-01T06:30:00.000Z")
>>> assert (a[0], b[0], b[1] - a[1]) == ("20261101T053000Z", "20261101T063000Z", 3600)
>>> # dst_eu: 2026-10-25 01:00Z is the EU fallback; the instants straddling it are 3600 s apart
>>> a, b = basic("2026-10-25T00:30:00.000Z"), basic("2026-10-25T01:30:00.000Z")
>>> assert (a[1], b[1]) == (1792888200, 1792891800) and b[1] - a[1] == 3600
>>> # subsecond: the mirror carries millis; DTSTART truncates to whole seconds
>>> assert basic("2026-11-07T19:00:00.500Z")[0] == "20261107T190000Z"
>>> assert check(render(G, E, "Game night", "2026-11-07T19:00:00.500Z", T))["DTSTART"] == "20261107T190000Z"
>>> # edit: UID is stable; DTSTAMP and DTSTART follow the mirror; the digest differs (stale preview)
>>> edited = render(G, E, "Game night", "2026-11-07T20:00:00.000Z", "2026-10-25T09:00:00.000Z")
>>> fe = check(edited)
>>> assert fe["UID"] == f["UID"] and fe["DTSTART"] == "20261107T200000Z" and fe["DTSTAMP"] == "20261025T090000Z"
>>> assert hashlib.sha256(edited).digest() != hashlib.sha256(utc).digest()
>>> # cancel / started / unavailable never reach render(); the oracle table below fixes their reasons
>>> # inject_crlf: CRLF in a title cannot add properties (a VALARM with PROCEDURE)
>>> hostile = render(G, E, "Raid\r\nBEGIN:VALARM\r\nACTION:PROCEDURE\r\nATTENDEE:mailto:x@y.z", "2026-11-07T19:00:00.000Z", T)
>>> fh = check(hostile)
>>> assert b"\r\nBEGIN:VALARM" not in hostile and b"\r\nATTENDEE" not in hostile
>>> assert fh["SUMMARY"].startswith("RaidBEGIN:VALARMACTION:PROCEDUREATTENDEE:mailto:x@y.z")
>>> # inject_bidi: bidi controls (including U+061C) and line separators are removed
>>> assert check(render(G, E, "a\u202eb\u2028c\u200f\u061c d", "2026-11-07T19:00:00.000Z", T))["SUMMARY"] == "abc d"
>>> # escape: backslash, semicolon, comma
>>> assert check(render(G, E, "A, B; C\\D", "2026-11-07T19:00:00.000Z", T))["SUMMARY"] == "A\\, B\\; C\\\\D"
>>> # fold: 100 four-byte characters fold at 75 octets, never mid-sequence, unfold losslessly, stay under the cap
>>> big = render(G, E, "\U0001f600" * 100, "2026-11-07T19:00:00.000Z", T)
>>> assert len(big) <= 2048 and check(big)["SUMMARY"] == "\U0001f600" * 100
>>> # unsafe: a title that is only control characters has no safe value
>>> assert render(G, E, "\r\n\t\u00a0\u2028", "2026-11-07T19:00:00.000Z", T) is None
>>> # size: an over-bound title is refused, never truncated
>>> assert render(G, E, "x" * 3000, "2026-11-07T19:00:00.000Z", T) is None

```

Reason oracle (access and state cases reach no generator; every row needs zero
writes, zero notifications and zero other-member data):

| Case | Controlled event / member state | reason |
| --- | --- | --- |
| `going` | Going, scheduled, future, channel visible, preview then press, unchanged | `PREVIEW` then `EXPORTED` |
| `interested` | Interested, otherwise as `going` | `PREVIEW` then `EXPORTED` |
| `declined` | Declined only | `UNAVAILABLE` |
| `no_rsvp` | Member never responded | `UNAVAILABLE` (same shape as `declined`) |
| `not_member` | Left the guild | `UNAVAILABLE` |
| `foreign` | Guild `g2` or another member's button | `UNAVAILABLE` |
| `deleted` | No mirror row or tombstoned | `UNAVAILABLE` |
| `denied` | Channel not visible to the member | `UNAVAILABLE` |
| `channel_less` | Mirror row without a channel | `UNAVAILABLE` |
| `canceled` | Going, status cancelled, channel visible | `CANCELED`, no file |
| `started` | Going, active, completed or past start | `STARTED`, no file |
| `stale_press` | Title or start changed between preview and press | `STALE_PREVIEW`, no file |
| `unsafe_title` | Title is only control characters | `FIELD_UNSAFE`, no file |
| `outage` | Store failure before the read | `STORE_UNAVAILABLE` |
| `unknown` | Membership or channel check times out | `VERIFICATION_UNKNOWN` |
| `upload_lost` | File send fails after admission | `UPLOAD_UNCERTAIN` |

Not verified, and so not claimed: that Apple Calendar, Google Calendar or
Outlook import these bytes (a manual or disposable-environment check for the
implementer, never a live guild); that an importer replaces a same-`UID` entry;
Discord attachment retention; real SQL plans, authentication, rendering,
erasure or outage behavior. Future protected acceptance tests must drive the real
callers with injected clocks and store/REST faults in disposable fixtures; this
oracle is not that test. No acceptance or implementation task is created here.

## 7. Acceptance boundary, review metric, and undo

Record CEO spec acceptance **and** host intent alignment against the exact
revision before any implementation subtasks, protected test handoff, or live
pilot. Route the decision brief through the CEO and the existing host intent
path; a queue or intake receipt is not alignment. The CISO findings above need an
independent boundary disposition, not a self-approved grant. Nothing is sent
directly to the owner or to an invented host executor.

The decision must accept or revise: the ephemeral preview-then-export flow; the
`.ics` attachment as the only export; the four-property field allowlist with
description and location excluded; the collapsed `UNAVAILABLE` ordering; the
no-end-time and UTC-only representation; and the new file-in-reply seam. A
material revision binds a fresh review request and alignment.

By 2026-10-31 the metric is **all 16 oracle rows have agreed or explicitly
rejected outcomes, the three blocking findings have written disposition, and the
existing permission path is shown to support a member-channel visibility check**.
This measures design completeness, not engagement. Retain only if the fixture
matrix shows no unintended disclosure and that permission path exists; otherwise
revise or stop this proposal, not the project. Default while unresolved is no
build and the current time tag. No timeout auto-approves; a future demand
experiment needs its own hypothesis, metric and date.

ACT: publish this proposed contract and its offline oracle for review. WHY: bound
a per-event export boundary before considering any calendar feature. UNDO: revert
the reviewed docs; there is no operational change to undo. A later accepted build
must stay dark until its own tests, security and release gates pass; disabling
it must leave no stored export to purge. No production tests, migrations,
credentials or activation here.
