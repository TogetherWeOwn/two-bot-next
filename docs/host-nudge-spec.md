# Events: host-side game-night start nudge

Status: **Proposed, design only**. Written 2026-10-27; review by **2026-11-05**.
Recommendation: **A, no build now**. A bounded host-only DM nudge (B) is specified
only so it can be reopened without redesign. No runtime, schema, command,
permission, configuration, DM, send, test, deployment or spend change is included.
Merge records this analysis, not design acceptance, host intent alignment, CISO
disposition, build approval or activation. This file is the sole editable spec.

## 1. Decision and evidence

Hypothesis: a host who forgot game night gets one quiet heads-up with the signup
count, so fewer game nights fail from host no-shows.

**Choose A (no build) today.** Three facts decide it:

1. **No recorded host exists.** The event mirror (`scheduled_events`, columns
   `guild_id, event_id, name, starts_at, channel_id, description, status,
   updated_at`; `crates/store/migrations/0401_web_contract_tables.sql:109-121`)
   and `RawScheduledEvent` (`crates/core/src/scheduled_events.rs:93-100`) carry no
   creator. Game nights the bot files itself (`GameNight` /
   `plan_game_night_creates`, `crates/core/src/internal_actions.rs:1204-1243`)
   have no human host at all. The only proposed host record is the organizer in
   the unbuilt [native publish ledger](event-native-publish-spec.md). Mirroring
   Discord's creator would need a migration, which is not authorized here.
2. **The failure mode is unevidenced.** No authorized evidence of host no-shows
   was found or collected: no member recruitment, analytics, live send or
   production inspection was performed. Roadmap value is not demonstrated demand.
3. **DM transport exists, but scheduled host nudges do not.** Voice notices can
   fall back to a guild-owner DM (`crates/bot/src/voice_rooms.rs:5414-5437`);
   `RoomHttp::direct_notice` opens a private channel with `create_private_channel`
   and posts the notice (`crates/discord/src/voice_rooms.rs:1222-1238`). The voice
   actor calls `send_notices`, so this is runtime code, not a test-only helper.
   The original grep missed the SDK spelling. B adds a scheduled host-recipient
   and event-content use of DM, not the bot's first DM transport. Independent
   security review must cover that recipient/trigger boundary and whether the
   existing transport meets B's silent, no-fallback, single-attempt contract.

| Alternative | Effect and cost supported by this analysis | Disposition |
| --- | --- | --- |
| A. No build; [attendee reminder](event-reminders.md) (one channel post, final 15 min, attendees only) plus existing host tooling | Zero new data or surface. Does not address a host who forgets; members only get a mention if at least one RSVP exists | **Recommended**; standing fallback |
| B. One DM to the recorded host at T-2h with the signup count | Targets the stated failure; needs a host source, a fence, opt-out state and a scheduled host-recipient boundary review | Specified in sections 2-5; reopen only per section 6 |
| C. Post the nudge in the event channel/thread | Reuses transport, but every attendee sees "host forgot"; mention suppression hides pings, not the message | Rejected: not host-only, not quiet |
| D. Ephemeral message | Private by construction, but needs a live interaction token; the scheduler has none | Rejected: impossible for a time-driven send |

Not a duplicate of member reminders, the member quiet-hours question (now the
[no-new-preference baseline](event-reminder-preferences-spec.md)) or LFG signup
reminders: this proposal addresses one
recipient and adds no member ping, `@everyone`/`@here`, role ping, member DM or new
scheduling surface.

## 2. Specification of B (only if reopened)

**Host identity.** One recorded human user ID per event, taken only from a TWO
write path that stores it at creation (never inferred from channel history, name
text, `Manage Events` holders or the Discord creator unless that is mirrored by an
accepted separate change). No co-hosts, no delegation, no target field. Bot-created
events and events with no record are `UNAVAILABLE / no_recorded_host`. A **former
host** (left the guild, erased, or no longer eligible when read fresh) gets nothing.
A **non-host** (any other member) is never a recipient.

**Visibility.** Guild-scoped mirror row with `status = 'scheduled'` and a future
`starts_at`. The nudge reveals only the event name, start time, event link and a
count; it never names an attendee and never posts anywhere shared.

**Signup count.** The same scalar the reminder uses: `count(*)` of RSVPs `going`
or `interested` for `(guild, event)` (`crates/core/src/event_reminder_store.rs:44-60`),
no roster load, no mentions. Zero is sent as `0`: it is the cancel-or-promote moment.
It is the mirror's view at send time, not live Discord truth, and says so.

**Timing.** One nudge at T-2h, not T-15: that is the existing attendee reminder
window and too late to find a host. A second nudge doubles noise. Window is
`start-120 min <= now <= start-30 min`; before it is `PENDING / not_due`, after it
`SKIPPED / window_closed`. An event created inside the window is nudged only if
at least 30 minutes remain. No catch-up, no replay, no repeat.

**Delivery channel.** DM to the recorded host only, never a channel fallback.
Privacy: the content is an aggregate count about the host's own event, so the DM
adds no new disclosure to the host; a channel post would disclose it to attendees.
A closed-DM host (Discord error 50007) is `UNAVAILABLE / dm_closed`: no retry, no
channel fallback, no second attempt. The send uses the closed allowed-mentions
policy and the Discord suppress-notifications message flag (documented value to be
verified against the pinned API docs before build; not verified here).

**Quiet hours.** The reminder-preferences spec stopped at the baseline and withdrew daily quiet hours,
so there is **no member rule to reuse**, and none is assumed. No timezone is
stored, so local-time suppression is impossible and is not promised. The host rule
is: one fixed, non-configurable, event-relative time; silent delivery; and no
catch-up if the window is missed. An event the host scheduled for 03:00 local still
yields a silent DM at 01:00; this is a disclosed limit, not suppressed.

**Opt-out.** Default is **off per guild**; an admin enables it (activation-fenced,
unknown setting fails closed). A host may also opt out: this needs a
`(guild, host)` marker, an identity-bearing row. It must be deleted on host erasure
and restore, carry no history, and never be hashed into a hidden marker. Whether a
marker is acceptable at all is an open **CISO** question (the same trade-off that
ended the member preference proposal); if it is not, B ships guild-off-by-default
with the disclosed limit that the host cannot opt out individually, or it stops.

**Idempotency and restart.** Reuse the reminder fence pattern with no migration:
one permanent `automation_audit_log` row
`id = 'event:host_nudge:<guild>:<event>'`, action `event.host_nudge_attempt`, NULL
actor, event-only target, committed **before** the one DM POST; only a committed
new claim authorizes the wire attempt, an unknown commit authorizes nothing, and
guild admission is serialized with a retained cap (fail closed at the cap). Results
are a separate fact (`posted|failed|delivery_uncertain|skipped`). A crash, timeout,
rate limit or lost reply after admission never reposts. A time or host change after
admission never creates a second nudge. Whether the new action names fit the audit
and backup allowlists is a build prerequisite, unverified here.

**Truthful states.** `PENDING / not_due` means not yet in the window and not
admitted; `PENDING / delivery_uncertain` means admitted without a confirmed result
and is not proof of non-delivery. `SENT` means Discord accepted the POST, not read
or seen. `UNAVAILABLE` means cannot decide or deliver; a confirmed non-50007 POST
failure records `failed` and maps to `UNAVAILABLE / send_failed`, with no retry.
`SKIPPED` means decided no send. A known existing fence takes precedence over
current guild settings; an unknown fence is `UNAVAILABLE / state_unknown`, never
permission to send. Never "delivered", never "read".

## 3. Stable IDs and compact handoff JSON

- `event_id`: Discord scheduled-event snowflake (existing).
- `nudge_id`: `event:host_nudge:<guild>:<event_id>` (the fence id; one per event).
- `host_ref`: `h:` plus the first 8 hex of SHA-256(`nudge_id|host_user_id`). It is
  for agent handoff only: never log or store the raw host ID, and it is not an
  anonymization claim.

```json
{"nudge_id":"event:host_nudge:G:E","host_ref":"h:1a2b3c4d","outcome":"UNAVAILABLE","reason":"dm_closed","next_action":"none"}
```

`outcome` is one of `PENDING|SENT|UNAVAILABLE|SKIPPED`. `next_action` is one of
`none`, `wait_until_window`, `ask_guild_admin_enable`, `record_host_source`.
Reasons are the finite set in the oracle below.

## 4. State and test matrix (scenario recipes, not executed tests)

Synthetic IDs, clocks and stores only; no real member data, Discord send or
staging/production database test.

| ID | Case | Expected |
| --- | --- | --- |
| H01 | Recorded host, member, 5 signups, T-2h, DM open | `SENT / sent`, count 5 |
| H02 | Recorded host, zero signups | `SENT / sent`, count 0 |
| H03 | Non-host member (any other user) | never a recipient |
| H04 | Former host (left, erased or ineligible) | `UNAVAILABLE / host_not_eligible` |
| H05 | Bot-created game night / no host record | `UNAVAILABLE / no_recorded_host` |
| H06 | Guild setting off or unknown | `SKIPPED / guild_disabled` |
| H07 | Host opted out | `SKIPPED / host_opted_out`, no DM |
| H08 | Before T-2h / after T-30m | `PENDING / not_due` / `SKIPPED / window_closed` |
| H09 | Event cancelled or completed before admission | `SKIPPED / event_not_scheduled` |
| H10 | Event moved earlier or later before admission | window recomputed from the current mirror |
| H11 | Event moved or host changed after admission | no second nudge |
| H12 | Restart or duplicate worker, fence exists | `SKIPPED / already_decided`, one POST max |
| H13 | Fence commit unknown or host membership unknown (independent cases) | `UNAVAILABLE / state_unknown`, no POST |
| H14 | DM closed (50007) | `UNAVAILABLE / dm_closed`, no fallback, no retry |
| H15 | POST timeout or lost reply after admission | `PENDING / delivery_uncertain`, no repost |
| H16 | Quiet-hours overlap (event at 03:00 host-local) | still one silent DM; no local-time rule |
| H17 | Retained-fence cap reached | `UNAVAILABLE / state_unknown`, admission stops |
| H18 | Confirmed non-50007 POST failure | `UNAVAILABLE / send_failed`, no retry |

## 5. Runnable offline check

```sh
python3 -m doctest -v docs/host-nudge-spec.md
python3 scripts/check_docs_index.py --json
git diff --check
```

The doctest checks the **specified oracle's internal consistency**, not SQL
serialization, Discord behavior, DM delivery, erasure or deployed behavior.
Future protected acceptance tests must implement H01-H18 against real callers in
disposable fixtures; zero tests is not proof. No acceptance or build task is
created by this proposal.

```pycon
>>> def decide(host="member", guild_on=True, opted_out=False, mins_to_start=90,
...            status="scheduled", fence=False, state_known=True, dm="ok"):
...     if fence is None:
...         return "UNAVAILABLE", "state_unknown", "none"
...     if fence:
...         return "SKIPPED", "already_decided", "none"
...     if not guild_on:
...         return "SKIPPED", "guild_disabled", "ask_guild_admin_enable"
...     if status != "scheduled":
...         return "SKIPPED", "event_not_scheduled", "none"
...     if host is None:
...         return "UNAVAILABLE", "no_recorded_host", "record_host_source"
...     if opted_out:
...         return "SKIPPED", "host_opted_out", "none"
...     if not state_known:
...         return "UNAVAILABLE", "state_unknown", "none"
...     if host == "former":
...         return "UNAVAILABLE", "host_not_eligible", "none"
...     if mins_to_start > 120:
...         return "PENDING", "not_due", "wait_until_window"
...     if mins_to_start < 30:
...         return "SKIPPED", "window_closed", "none"
...     return {"ok": ("SENT", "sent", "none"),
...             "closed": ("UNAVAILABLE", "dm_closed", "none"),
...             "failed": ("UNAVAILABLE", "send_failed", "none"),
...             "uncertain": ("PENDING", "delivery_uncertain", "none")}[dm]
>>> oracle = {
...     "H01": (dict(), ("SENT", "sent", "none")),
...     "H04": (dict(host="former"), ("UNAVAILABLE", "host_not_eligible", "none")),
...     "H05": (dict(host=None), ("UNAVAILABLE", "no_recorded_host", "record_host_source")),
...     "H06": (dict(guild_on=False), ("SKIPPED", "guild_disabled", "ask_guild_admin_enable")),
...     "H07": (dict(opted_out=True), ("SKIPPED", "host_opted_out", "none")),
...     "H08a": (dict(mins_to_start=121), ("PENDING", "not_due", "wait_until_window")),
...     "H08b": (dict(mins_to_start=29), ("SKIPPED", "window_closed", "none")),
...     "H09": (dict(status="cancelled"), ("SKIPPED", "event_not_scheduled", "none")),
...     "H12": (dict(fence=True), ("SKIPPED", "already_decided", "none")),
...     "H13": (dict(state_known=False), ("UNAVAILABLE", "state_unknown", "none")),
...     "H13b": (dict(fence=None), ("UNAVAILABLE", "state_unknown", "none")),
...     "H18": (dict(dm="failed"), ("UNAVAILABLE", "send_failed", "none")),
...     "H14": (dict(dm="closed"), ("UNAVAILABLE", "dm_closed", "none")),
...     "H15": (dict(dm="uncertain"), ("PENDING", "delivery_uncertain", "none")),
...     "H08c": (dict(mins_to_start=120), ("SENT", "sent", "none")),
... }
>>> assert all(decide(**args) == want for args, want in oracle.values())
>>> outcomes = {"PENDING", "SENT", "UNAVAILABLE", "SKIPPED"}
>>> actions = {"none", "wait_until_window", "ask_guild_admin_enable", "record_host_source"}
>>> assert all(want[0] in outcomes and want[2] in actions for _, want in oracle.values())
>>> assert decide(fence=True, host=None, status="cancelled")[1] == "already_decided"
>>> assert decide(fence=True, guild_on=False)[1] == "already_decided"
>>> assert decide(fence=None, guild_on=False) == ("UNAVAILABLE", "state_unknown", "none")
>>> assert decide(mins_to_start=30)[0] == "SENT" and decide(mins_to_start=120)[0] == "SENT"
>>> # one nudge per event: the same fence id for any host or time change
>>> import hashlib
>>> nudge_id = "event:host_nudge:G:E"
>>> ref = lambda h: "h:" + hashlib.sha256(f"{nudge_id}|{h}".encode()).hexdigest()[:8]
>>> assert ref("u1") == ref("u1") != ref("u2") and len(ref("u1")) == 10

```

## 6. Dispositions, reopen rule and kill rule

**Author analysis:** retain A. This is not independent approval of the DM
boundary: the author grants none, and requests no credential or permission.

Reopening B requires **all** of: (1) an accepted, built host source (the publish
ledger organizer or a separately accepted mirrored creator); (2) authorized
evidence that host no-shows occur and that member reminders cannot fix them;
(3) an independent **non-author CISO disposition** of the scheduled host-recipient
and event-content boundary, reuse of the existing DM transport, the opt-out marker
and the suppress-notifications behavior; (4) a recorded, revision-bound
**CEO design confirmation** and host intent alignment on this exact commit
and SHA-256 (a roadmap, intake receipt or spec merge is neither); and (5) a
verified answer to the audit/backup allowlist question in section 2. A material
edit voids any disposition. Then separate admission, test-first execution, exact-
head `pr-lint`, `ci-ok` and Paperclip Review 5/5 apply. No live send, DM,
database change, migration, access grant, activation or new spend is authorized.

**Kill rule, 2026-11-05:** retain B only if host no-shows are an evidenced failure
mode that member reminders cannot fix; defer if hosts are reachable through
existing announcements, if no host source exists, or if quiet delivery cannot stay
bounded. This design already defers (host source absent, no evidence). It kills
this hypothesis, not the Events project, and nothing is built either way.

ACT: publish one indexed proposal and matching task spec. WHY: no recorded host,
no demonstrated failure mode, new scheduled host-recipient use of existing DM
transport. UNDO: reviewed docs revert only; no
operational state changes. Never undo by deleting replay facts or sending a
missed nudge.
