# Existing event reminders: member quiet hours

Status: **proposed, design only**. Written 2026-10-24; review by **2026-10-31**.
No runtime, schema, command, endpoint, permission or configuration change.
Merge records a proposal, not approval, activation or proof of production behavior.
This file is the sole editable spec; approval records bind its commit and SHA-256.

## 1. Decision, evidence and limits

Members should be able to limit their inclusion in optional attendee reminders
without changing their RSVP. [sesh](https://sesh.fyi/) advertises optional reminders;
[Dyno](https://docs.dyno.gg/en/modules/reminders) describes private reminders and
limits on inspecting/removing active reminders. These are roadmap-supplied vendor
references, not independently reverified capabilities, evidence of TWO demand or
vendor claims about quiet hours. Quiet hours are our hypothesis. No member
recruitment, live send, preference change or production inspection was performed.

**Important limit:** the existing reminder is one shared event-channel message,
not a DM; its rendered member mentions do not trigger notifications under the
closed allowed-mentions policy. A personal rule can omit your mention, not hide
that channel message from you or stop Discord/client notifications for other
messages. Do not market this as server-wide Do Not Disturb or guaranteed silence.

| Alternative | Value / cost / consent | Disposition |
| --- | --- | --- |
| No new preference | No new routine data or controls; current reminder limitations remain | Safe baseline; stop if omission has no useful member value |
| Per-event opt-out | Omit only the actor on one event; no timezone, DST or routine storage, but repeated choices for each event | Narrow fallback if quiet hours cannot meet the fences or privacy gate |
| Per-member quiet hours | One explicit daily rule for existing reminders in one guild; timezone/routine data, DST and erasure complexity | Proposed only if the omission limit is accepted; no global notification center |

Smallest viable quiet-hours default: **unset**, no timezone inferred or collected,
no new messages/pings/DMs and no change to RSVP. Existing valid reminders remain
eligible except where stricter privacy/admin safety gates suppress them. Offer
one opt-in daily interval and an all-day **Off** choice, per member per guild.
Do not implement the per-event alternative alongside it. No new reminder kinds,
lead times, occurrence identities, rosters, voice effects or personal-reminder
work. This does not amend the separately proposed [seat/waitlist spec](rsvp-capacity-spec.md).
If that lands, its exclusion of waiting/provisional actors also wins.

## 2. Verified repository baseline and gaps

Baseline commit: `f2ca0f763c` (source inspection, not live runtime verification).

- [Current help](event-reminders.md): Going/Interested only, final 15 minutes,
  channel-only, two events/tick, best effort, whole message <=2,000 UTF-16 units.
- `crates/core/src/event_reminder_store.rs:15-59`: fixed lead, two-event batch,
  scheduled future mirror rows, channel required, sorted bounded RSVP array.
- `event_reminder_store.rs:69-112`: mirror fields/time and full sorted array
  revalidated; guild advisory lock, unique `event:reminder:<guild>:<event>` fence,
  `rsvp.reminder_attempt`, NULL actor, event-only target, 10,000 retained attempts
  per guild. Only a known new committed admission permits the wire attempt.
- `crates/bot/src/scheduled_jobs.rs:62-104,187-275`: boot automation/identity gate,
  reminder dispatch after ordinary schedules; immutable attempt then separate
  result; invalid/oversized rendering consumes the attempt without a post.
- [Privacy](privacy.md#member-opt-out-per-user-per-guild): analytics suppression
  is guild/member scoped, separate from full erasure; retained privacy tombstones
  are explicit exceptions, not a promise all subject data disappears.

**Gaps, not delivered controls:** the inspected reminder SQL does not read
`privacy_opt_outs`, personal preferences or a dedicated live guild reminder-disable
setting. Do not claim otherwise. A future change must honor existing automation/
activation and any applicable guild-disable policy, and exclude privacy-opted-out
members at admission. This is a proposed stricter use of analytics opt-out, not
its current documented delivery scope; a future accepted change must update the
privacy command/help disclosure, including assisted opt-outs, before activation.
Opt-in still does not undo a personal Off rule or replay old reminders.
`GUILD_DISABLED` means a known effective automation/activation disable or a
separately verified applicable host policy, not an invented new setting. An
unresolved required policy mapping is `STATE_UNKNOWN`, never inferred enabled;
Q08 must exercise the actual resolved gate. No extra per-attendee Discord
membership probes or new admin authority are proposed. The mirror can be stale; cancellation/access loss
not yet observed cannot be promised to prevent an in-flight message.

## 3. Private UX and consent boundary

Proposed actions belong in existing event help/RSVP replies, with a discoverable
**My reminder preferences** entry and a readable text fallback. They are design
labels, not commands available today. All replies are ephemeral, never a DM or a
public preference summary. Derive actor and guild from authenticated context;
no target-member parameter, delegation, bulk updates or host impersonation.
A stale component grants no authority. Validate current guild membership for
reads/changes; unavailable membership fails closed without disclosing state.

| Action | Result / required explanation |
| --- | --- |
| Inspect / preview | Read-only own saved rule, revision, effective reason, relevant UTC/local times, timezone database version, decision status if already fenced; no writes or roster lookup |
| Change | Show exact daily interval + IANA zone and scope; explicit Save consents only to storing/applying this suppression rule in this guild |
| Off | Omit own inclusion all day; no RSVP mutation or new notification |
| Reset to default | Confirm: “Remove quiet hours and allow your inclusion in future existing channel reminders, subject to server/privacy gates. No missed reminder will be sent.” Clear zone/interval, retain only minimal replay control |
| Erase preference details | Clear zone/interval and leave **Off**, explicitly disclose minimal retained suppression/replay record; not the same as Reset or full member erasure |

Example private state: “Quiet hours 22:00–07:00 Europe/London. Your inclusion is
decided once when the reminder is admitted, up to 15 minutes before event start,
not at event start. If that instant is quiet, you are omitted. Other attendees
may still cause a channel message you can see. No DM/ping is added. RSVP unchanged.”
A masked rule still displays “Saved, but server reminders are disabled” or
“Saved, but your privacy opt-out suppresses inclusion”; Save never enables either.
A privacy opt-out is a distinct action, never an implicit side effect of Reset.
No host/admin read of members' routines, override of an individual's suppression,
public suppression counts, public denial reasons or preference export to another
service. A host controls existing event/server policy, not personal consent.

## 4. Deterministic time policy

- Event start is the mirror's absolute UTC instant. Evaluate at the **final DB
  admission instant**, not event start, interaction time or a cached worker clock.
  Window is `[starts_at - 15 minutes, starts_at)`; equality at start suppresses.
- Accept one allowlisted IANA zone (including `Etc/UTC`) and two minute-resolution
  `HH:MM` values. No guessed locale, timezone from IP, free-text abbreviations,
  fixed-offset input or device-zone auto-updates. Midnight is `00:00`; `24:00`
  refuses. Equal endpoints refuse; use Off for all-day suppression.
- Convert the UTC decision instant to local wall time using a pinned timezone
  database. Quiet is start-inclusive/end-exclusive; start > end crosses midnight.
  Seconds within a selected minute share that minute's result.
- Spring-forward nonexistent local minutes never occur. Fall-back repeated local
  minutes are quiet on **both** occurrences when they fall in the interval. No
  ambiguous local appointment needs resolving because inputs are daily wall times
  and the evaluated instant is UTC. Fractional offsets work identically.
- The first admitted event decision is terminal. If it falls in quiet hours,
  suppression lasts for this event's reminder even if quiet ends before start.
  **Never defer** to morning/end-of-quiet, never catch up or backfill. Restart
  inside a still-open window with no decision may make the ordinary first attempt;
  restart after start makes none.
- Unknown/invalid zone, unavailable zone database, failed privacy/preference read,
  unhealthy time or inconsistent revisions: no message for that decision pass;
  bounded failure reason, no success claim. A later healthy pass before start may
  make a first decision only if no fence exists. Never substitute UTC or unset.
  Zone database upgrades require version-pinned fixture review, not silent fallback.

## 5. Admission, volume and races (future requirements)

The rule is **suppression-only**, not a new scheduler. Keep two events/tick,
current safety bounds and channel/closed-mentions transport. At the same decision
snapshot, output recipients must be a subset of the existing valid recipient set
and output message count must be <= baseline: at most one per admitted event,
at most two per tick, never more than the unfiltered baseline would have posted.
First validate that the **unfiltered baseline** would fit the whole-message limit;
filtering an oversized list must not turn a previously skipped event into a post.
Never split, add a replacement announcement or send a message just to acknowledge
suppression. No preference operation enqueues a reminder.

Future store protocol, to be proven before build approval/activation:

1. Snapshot baseline event/RSVP inputs and bounded preference/privacy inputs for
   that same guild. Use the identical eligibility predicate in scan and admission:
   baseline RSVP eligibility AND no privacy suppression AND no personal Off AND
   not quiet. Applicable guild disable/activation gates precede any send.
2. Under erasure-compatible locks and existing guild reminder admission lock,
   re-read event fields/status/start, DB time, privacy and preference revisions,
   applicable guild policy and the full baseline/filtered arrays. Snapshot mismatch
   returns `STALE_SNAPSHOT`, no fence/no send; a later pass may rescan while due.
   Preference/privacy/admin writes must serialize with this commit; stale process
   caches cannot authorize dispatch. No DB lock held over Discord HTTP.
3. A matching valid decision consumes the **same permanent event fence**, even
   when filtering leaves zero recipients or a known guild disable suppresses it.
   This requires a future terminal no-send admission branch: the current code does
   not implement it. Preserve action/key/NULL actor/event target/cap; an attempt
   fact is an admission fence, not proof HTTP occurred. Append a bounded suppressed
   result without member identifiers or routines. Never encode recipients in fences.
4. Only a newly committed, eligible nonempty decision authorizes one POST. Unknown
   commit outcome authorizes none. Crash after fence, render failure, transport
   refusal/429/timeout/lost receipt or result-audit error never grants a repost.
   A process-wide disabled scheduler runs nothing; re-enable can only make a first
   decision in an open window, never replay a retained one.

Suppression/update committed before admission affects that decision; admission
committed first may already be in flight. Tell the member that Save cannot recall
it. Recheck through existing executor safety gates; a failure never retries this
event. Removing a preference, opting back in, late RSVP, timezone change, event
move/channel change, deletion/recreation of mirror rows or restart cannot reset
the guild/event fence. A move before admission uses the new mirrored start;
a cancel/active/completed/deleted mirror prevents a send. A move after admission
never creates a second attempt. No per-recipient fence, occurrence-by-start key,
expiry/recycling of replay facts or history-search “proof of no send.”

## 6. Storage, retention and erasure proposal

No migration now. Future minimum: one `(guild_id, member_id)` row with mode
`unset | quiet | off`, nullable IANA zone/start/end (quiet only), revision and
last explicit change time. IDs are personal data; zone/interval can reveal a
routine. Purpose is reminder suppression only, never analytics/profiling. No event
roster, location, name, message body or interaction token. Keep only the latest
state, not a preference history. Reads are exact-pair; dispatch joins only the
existing bounded RSVP batch, not a guild member census.

Proposed active quiet-rule retention is **180 days after explicit Save**. Inspection
and duplicate requests do not extend it. Expiry discards zone/interval and changes
to Off, never silently resumes reminders. Evaluate expiry at admission even if
cleanup is delayed. Healthy cleanup targets removal of expired detail within one
15-minute pass; outage delay is reported, not represented as a guaranteed TTL.
No unsolicited expiry notice. Off/reset rows keep only scope, mode, revision and
minimal replay cursor/cutoff until a fresh explicit reset/change or guild purge;
**there is no automatic expiry for suppression**.

Preference-detail erasure and full member erasure remove routine and mutation
receipt details, but retain a minimal pair-scoped Off/cutoff tombstone so erasure
does not opt a person back into optional inclusion. This is a **new proposed
retention exception**, not existing behavior or a claim of complete deletion.
A full-erasure request installs that marker even if no preference existed; RSVP
removal uses the existing erasure manifest, never the preference handler.
Both erasure executors must participate atomically before new dispatch admission;
full erasure may separately remove RSVP as already documented. Changing/resetting/
erasing preference details alone never touches RSVP or anybody else's state.

The retained marker must reject mutations generated at/before its cutoff (DB
now + 30 seconds at erasure), including previously unseen delayed Save/Reset.
It retains no prior timezone/interval or event/roster history. Subsequent fresh
member choices may replace Off, but privacy opt-out continues to win independently.
Review must explicitly accept this residual identity retention; if it cannot,
narrow/stop rather than retain secretly or let erasure resume inclusion.

Inventory future fields in member export, both deletion executors, schema-coverage
checks, guild purge and backup allowlist before enablement. Member export shows
only own current rule and retained-marker disclosure. Logs/metrics contain fixed
codes/aggregate counters, no member/event identifiers, zone, interval or rosters;
per-event existing audit keeps its current event-only shape. Backups retain the
existing snapshot-count policy (not a newly promised number of days). Restore
must reapply authorized erasure/suppression/cutoffs before reopening dispatch;
never restore erased routines or delete permanent event attempts to send again.

## 7. Future adapter contract, not a new endpoint

Policy ID `event-reminder-preferences/v1`; identity is native guild + member,
and guild + native scheduled-event ID for authorized event preview. An event ID
is not permission: use existing access checks, collapse foreign/missing/denied
events to `NOT_AVAILABLE`, and never return attendees or other members' rules.
Own preference inspection needs no event. Structured result fields:
`policy_id`, `code`, `reason`, `next_action`, `revision`, `saved_mode`,
`effective_mode`, and optional own-rule/authorized-event local/UTC decision data.
Preview is explicitly hypothetical until admission; a fence reports
`ALREADY_DECIDED`, not a promise that an HTTP message was delivered.

| Stable code / reason | next_action |
| --- | --- |
| `OK` / `DEFAULT`, `RULE_SAVED`, `UNCHANGED` | `inspect_or_change` |
| `SUPPRESSED` / `QUIET_NOW`, `PERSONAL_OFF`, `PREFERENCE_EXPIRED` | `inspect_or_reset_explicitly` |
| `SUPPRESSED` / `GUILD_DISABLED`, `PRIVACY_OPT_OUT` | `inspect_server_or_privacy_status` |
| `ALREADY_DECIDED` / `EVENT_FENCE_PRESENT` | `inspect_existing_result_do_not_resend` |
| `NOT_DUE` / `BEFORE_WINDOW`, `EVENT_TERMINAL` | `no_send` |
| `REFUSED` / `NOT_AVAILABLE`, `INVALID_RULE`, `ACTION_CONFLICT`, `STALE_ACTION`, `REVISION_CONFLICT` | `reload_and_choose_explicitly` |
| `UNAVAILABLE` / `STATE_UNKNOWN`, `TIME_UNKNOWN`, `ZONE_UNKNOWN`, `STALE_SNAPSHOT` | `retry_after_server_backoff` |

All actions authorize afresh. Updates take an expected revision and authenticated
interaction ID; fingerprint canonical typed actor/guild/action/rule fields, not
raw JSON or labels. Same ID/same payload returns prior bounded result plus current
own state; changed payload conflicts; older IDs cannot overwrite newer choices.
Derive action creation time from the Discord interaction snowflake carried by
the authenticated Discord gateway context, never payload time or first arrival.
User-supplied IDs/times are not accepted; a future non-Discord adapter needs its
own authenticated equivalent before use. Admit those creation timestamps only in
`[DB now - 15 minutes, DB now + 30 seconds]` and after retained cutoff; reject
unknown/unhealthy time. This trusted creation time, not first-seen delivery time,
makes the unseen delayed-action erasure rule testable. Under the pair lock,
compare revision, apply once and save a bounded latest cursor atomically; never
replay a mutation from an interaction token. No-op does not extend retention.
Reset keeps a minimal cursor to prevent delayed Save resurrecting the old rule.
Erasure removes detailed receipts but retains its cutoff. Rate/cap refusal is
not success; keep existing suppression at capacity. Future storage caps and
measured dispatch/query headroom must be set before any implementation is enabled,
without new spend or bypassing existing limits.

## 8. Deterministic offline scenarios and check

Design recipe only; **not runtime tests or an implemented reducer**. Test Author
must turn these rows into protected injected-clock/zone/store/transport fixtures
before any later builder starts. Use synthetic pairs in two guilds, two independent
pools and barriers for races; no live Discord, staging/production DB or real IDs.
Every case asserts unchanged RSVP/other pair and no DM/new ping, plus post/fence
counts. Boundary tests must include each side, not only a happy-path instant.

| ID | Input / deterministic oracle |
| --- | --- |
| Q01 | Unset, valid due event: same <=1 channel message and recipients as baseline; no zone collected |
| Q02 | `Asia/Kolkata`, quiet 22:00–07:00: 2026-10-24T16:30Z suppresses (22:00); 2026-10-25T01:30Z includes (07:00); test one second before each |
| Q03 | `America/New_York`, quiet 01:00–03:00: 2026-03-08T06:59Z and 07:00Z are quiet/then not quiet; no catch-up for nonexistent 02:00 |
| Q04 | Same zone, quiet 01:00–02:00: 2026-11-01T05:30Z and 06:30Z both suppress; 07:00Z includes; no double decision |
| Q05 | Same-day 09:00–17:00: start inclusive/end exclusive; midnight crossing works; equal endpoints/24:00/unknown zone refuse without clearing rule |
| Q06 | Restart before fence inside window may make first decision; after fence/unknown commit or after start => zero new posts; duplicate workers => one fence/<=1 POST |
| Q07 | All recipients quiet/Off: terminal no-send fence; reset/end of quiet/late RSVP in same window => no post; mixed recipients => subset only, others' rules/RSVP unchanged |
| Q08 | Guild disabled or privacy opt-out committed before admission: no affected inclusion; personal reset cannot enable; commit after admission has disclosed in-flight limit |
| Q09 | Save/Reset/opt-out races with dispatch: barriers prove serialized current snapshot or stale refusal; no cached-state bypass |
| Q10 | Move/cancel before admission uses changed mirror or refuses; move/cancel after fence preserves it; unobserved Discord change is not claimed blocked |
| Q11 | Duplicate update/restart returns same result; altered payload/old Save after Reset refuses; neither extends TTL nor changes RSVP |
| Q12 | Detail/full erasure vs delayed known/previously unseen Save: Off/cutoff persists, routines/cursor detail gone, fresh explicit reset alone may restore default; other guild untouched |
| Q13 | 180-day expiry becomes Off; preview read does not renew; unknown state/time/zone => zero send this pass, no fallback to default |
| Q14 | Baseline oversized remains no-send even if filtered fits; 10,000 attempt cap refuses new admissions, never prunes; restore preserves fences/suppression |

Runnable check **now**, only for this indexed design:

```sh
python3 scripts/check_docs_index.py --json
git diff --check
```

Future acceptance target: `event_reminder_preferences_contract` in
`crates/core/tests/`, plus runtime transport integration for Q06–Q10/Q14. It does
not exist as a consequence of this document. Once approved and supplied, run:

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db --test event_reminder_preferences_contract -- --test-threads=1
```

Use only the documented disposable test DB. Missing target, skipped/zero tests or
unchanged reminder suites cannot be represented as proof of these future rules.

## 9. CISO analysis and required disposition

**CISO author analysis: acceptable to propose, not implementation approval.**
Boundaries are authenticated self-service → own preference; host/admin policy →
guild send permission; scheduler/store → permanent admission; Discord → shared
visible message. Spoofing/IDOR is prevented by actor-derived scope and fresh auth;
tampering/replay by expected revisions/cursors/cutoffs; information disclosure by
ephemeral own-state allowlists and no routine telemetry; overload by current
bounded scans and future measured caps; privilege escalation by personal rules
only narrowing admin/privacy authority. Unknown state fails closed. No credential,
new service, grant or production authority is requested.

Residual risks requiring independent disposition: omission may not solve the
member's interruption problem; routine storage and indefinite Off/cutoff identity
retention; absence of verified baseline privacy/dedicated guild-disable reads;
mirrored cancellation/in-flight race; filtered rendering must not increase volume;
terminal suppression deliberately loses reminders. The author cannot accept their
own new retention/boundary request. Preserve normal independent security review
and same-head CI (`pr-lint`, `ci-ok`, `gitleaks`) / Paperclip Review 5/5.

**Requested CEO design disposition and host intent alignment:** accept, narrow to
per-event opt-out, or reject the exact revision. Explicitly resolve the omission
value proposition, personal-vs-admin scope, no-defer/no-catch-up behavior, erasure
exception and actual existing host reminder-disable intent. CEO routes the existing
host alignment path; an intake receipt or merged doc is not an alignment verdict.
No owner approval hop is added. **No implementation or build/acceptance subtasks
until revision-bound design acceptance AND recorded host intent alignment.**
If the host route is unavailable, leave the gate unresolved rather than inventing
an executor. Material edits require new revision-bound disposition/alignment.

Review on **2026-10-31**: continue only if Q01–Q14 have deterministic agreed oracles,
current permanent fences can accommodate terminal suppression and erasure without
extra unsolicited messages, and omission has accepted value. Otherwise narrow to
per-event opt-out under the same fences; stop if that also fails. No launch or
member experiment is authorized by this date. No production KPI is claimed.

Undo now: reviewed docs revert; no operational effect. Future rollback must stop
reminder dispatch while preserving preferences, erasure markers and event fences;
never roll back to a sender that ignores suppression, erase replay facts, perform
a destructive down migration or send missed reminders. ACT / WHY / UNDO belongs
in any later authorized rollout record.
