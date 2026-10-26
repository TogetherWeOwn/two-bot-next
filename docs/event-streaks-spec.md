# Events: attendance streaks on host check-ins

Status: **Proposed — design only**, 2026-10-26; review by **2026-11-09**.
No commands, storage, migrations, permissions, sends, XP/leveling changes or
runtime tests are added. Merge publishes a proposal, not permission to build,
pilot or activate it. Public leaderboards are out of scope.

## 1. Decision and evidence

Consumer: a member who already attends recurring events, and a host who wants
attendees to come back next week. Hypothesis: a private reminder of their own
recorded rhythm ("checked in 4 weeks in a row") makes repeat attendance more
likely. Demand and effect are **unproven**; no member outreach, live audit or
external evidence is claimed here. Host check-in attendance is live, but no
attendance-streak surface exists (§2).

| Option | Value and cost | Disposition |
| --- | --- | --- |
| A. No build | Hosts keep check-ins and totals; zero new member-facing pressure, storage or copy | Standing default; choose it if any boundary in §8 is not accepted |
| B. Private week streak, pull-only | Member reads their own streak in an ephemeral reply; derives from facts already held; needs one bounded read and frozen copy (§3–§4) | **Smallest option that tests the hypothesis; recommend for spec review, not build** |
| C. Per-series occurrence streak | "3 sessions of game night in a row" needs a series identity. None exists: distinct recurring occurrences carry distinct native event IDs ([capacity proposal](rsvp-capacity-spec.md), §3), and labels under one anchor store no start times (§2) | Do not build on current data; would silently read as streak 1 for separately published sessions |
| D. Public streaks, celebrations or leaderboard | Highest shame, gaming and bot-noise cost; a check-in is recorded by the host, not published by the member | Rejected here and explicitly out of scope; any revisit needs its own spec |

Option B's benefit over A is small and unproven. Its costs are bounded only
while it stays private, pull-only and reward-free: attaching XP, roles,
badges or public status to the same number changes the gaming and consent
analysis (§5) and is not authorized by this proposal.

## 2. Verified baseline and missing seams

Inspected repository head: `92d85042527b756062bfc9e19c8d77aed0cee0b3`.
Source inspection only; no live guild, deployment or host workflow was read.

- `crates/core/src/rsvp.rs:1-31` ports RSVP plus host check-in as pure
  functions. A recorded check-in appends one `event_attended` fact to
  `community_facts`, keyed `event-attended:{occurrence}:{actor}`
  (`rsvp.rs:397-420`), with metadata `{eventOccurrenceId, proof}`. The `rsvp`
  proof writes nothing (`rsvp.rs:360-388`): an RSVP alone is never attendance.
- An occurrence is a bare scheduled-event ID or `{anchor}:{label}`
  (`rsvp.rs:339-358`). The handler verifies the anchor is a live event in the
  invoking guild before and after the write; status 4 (cancelled) refuses
  with zero writes, while Scheduled, Active and Completed anchors all accept
  a recording (`crates/discord/src/rsvp.rs:509-512,646-669`). The acting
  host needs Manage Events (`rsvp.rs:288-294`) and the fact plus its
  `event.checkin` audit row commit together (`rsvp_store.rs:460-467`).
- The fact's `occurred_at` is the **recording time** (`now_iso()`,
  `crates/discord/src/rsvp.rs:583`), not the event start. No start time,
  series ID or expected-attendee list is stored on the fact
  (`crates/cutover/migrations/0160_rsvp.sql:50-69`). An occurrence with zero
  check-ins leaves no fact at all, so "events the guild held" is not directly
  enumerable from this table.
- The existing scorecard attendance count already defines the eligible set:
  `event_attended` facts classified `eligible_human` with proof in
  {`host_checkin`, `durable_checkin`, `voice_600s`}, deduped by
  occurrence:actor (`crates/core/src/community.rs:1235-1255`). Bot check-ins
  are recorded with classification `bot`, never as human attendance
  (`rsvp.rs:443-459`). Admission caps one occurrence at 1,000 check-ins and
  retries dedupe on the idempotency key (`rsvp.rs:506-508`,
  `rsvp_store.rs:409-426`).
- The scorecard week is Monday 00:00 UTC (`week_start_ms`,
  `community.rs:687-694`) and the bot-noise alert line is a ratio of 0.2 over
  human-channel messages (`BOT_NOISE_THRESHOLD`, `community.rs:120-122`;
  ratio built at `:1257-1280`). Both are reused below instead of inventing a
  second week or a second noise definition.
- Privacy opt-out purges the member's `community_facts` rows for that guild
  and installs suppression (`crates/cutover/src/privacy.rs:4-16,40-44`); the
  check-in writer drops suppressed pairs inside its transaction
  (`rsvp_store.rs:394-400`). Opting back in does not restore purged rows
  ([privacy](privacy.md), "Member opt-out"). Opt-out does **not** purge
  `announcements_audit_log`, so the audit log must never be a streak source
  (§5). Member erasure deletes `community_facts` rows through the reviewed
  manifest ([privacy](privacy.md), "What is stored").
- Missing seams a build must add or prove: a bounded per-member fact read
  (existing indexes lead with guild plus time or type/classification, not
  actor: `0160_rsvp.sql:71-74`), the streak core itself, and a self-only
  command surface. No actor-index migration is assumed acceptable in advance
  (§8, finding 4).

## 3. Streak definition (proposed, normative for option B)

A streak is per `(guild_id, member_id)`, computed on read from recorded facts
only, at an evaluation instant `now` (UTC). Nothing is materialized.

**Eligible fact.** A `community_facts` row in this guild with
`event_type = event_attended`, `classification = eligible_human`, proof in
{`host_checkin`, `durable_checkin`, `voice_600s`}, a nonempty actor, and an
`occurred_at` that parses as ISO-8601 UTC millis and is not after `now`. This
mirrors the scorecard set in §2 exactly. Malformed timestamps are excluded
and flagged for the quality note, never defaulted. For the subject member the
actor must be that member.

**Week.** The Monday 00:00 UTC bucket `[week_start_ms(t), +7 days)` of the
fact's `occurred_at`. Weeks are therefore **recording weeks**: a check-in a
host records late counts in the week it was recorded, not the week the event
ran, and the UTC boundary can split a local Sunday evening from its local
week (§8, finding 1). Copy must say "based on host check-ins" (§4).

**Attended week (member)** holds at least one eligible fact for the member.
**Guild-active week** holds at least one eligible fact for any member of the
guild; an attended week is therefore always guild-active. Only guild-active
weeks are **counting weeks**. A week with no recorded check-in for anyone —
no event ran, every event was cancelled before check-in, or hosts recorded
nothing — is skipped: it neither extends nor breaks a streak ("paused").

**Window.** Only the 53 week buckets ending with the current week are read
(current week plus the 52 before it). A run that reaches the oldest bucket is
reported as capped ("at least N", §4); "longest" always means longest inside
this window, labeled "in the last 53 weeks".

**Current streak.** Take the counting weeks in the window, in order. If the
current week is counting but the member has not attended it yet, drop it as
**pending**: an unfinished week cannot break a streak. The current streak is
then the length of the trailing run of attended counting weeks — 0 when the
most recent remaining counting week is unattended. **Longest streak** is the
longest attended run over the same counting sequence. Multiple check-ins,
occurrences or retries inside one week still credit that week once.

Kept, missed and cancelled outcomes:

| Situation | Effect on the streak |
| --- | --- |
| At least one eligible check-in recorded in a counting week | Week credited once; run extends |
| Counting week closes (next Monday 00:00 UTC) with no eligible fact for the member | Run ends. The next attended counting week starts a new run at 1. No debt, no negative state |
| Current counting week, not yet attended | Pending; the run through the last closed counting week still displays |
| Week with zero recorded check-ins guild-wide (including a cancelled-only week: a cancelled anchor records nothing, §2) | Not counting; paused, never a break |
| Event cancelled **after** check-ins were recorded | Recorded facts stand; later status is not an input and history is not rewritten. Compensation exists only for the immediate post-write race (§2) |
| RSVP Going/Interested/Declined without a check-in; `rsvp` proof | No credit and no fact (§2). If others' check-ins make the week counting, it closes unattended for this member, but copy never calls it a no-show (§4) |
| Host never recorded the member (host omission) | Indistinguishable from absence. If the week is counting via others, the run can end; the streak is a record of **recorded** attendance (§4 copy, §8 finding 1) |
| Bot, suppressed (opted-out) or otherwise ineligible fact | Never credits the member and never makes a week guild-active |
| Same member in another guild | Separate streak; guilds never combine |

Evaluation is a snapshot: a week that is not yet guild-active can become
counting later in the same week when someone is checked in, which can move a
displayed streak from current to pending, never retroactively change a
closed week.

## 4. Visibility, copy and bot-noise budget

**Private-only in v1.** The only surface is a self-invoked command (proposed
shape: one new command with no member option) answering **ephemerally to the
invoking member about themselves**. Resolved answers to the visibility
question: private by default means private only — there is no host lookup of
another member, no public channel post, no DM, no append to the check-in
reply (that reply goes to the acting host, `crates/discord/src/rsvp.rs:670-673`,
so naming the checked-in member's streak there would leak it), no milestone
congratulation, no "streak at risk" warning, and no timer or background send
of any kind. Pull-only also means a member who never invokes it is never
shown a streak.

**Opt-in sharing is not in v1.** A later share surface requires its own
slice (§7, S5) with an explicit per-guild member opt-in, default off and
revocable. Host recording of a check-in is not member consent to publish —
the same reason names stayed out of the [recap proposal](event-recap-spec.md).

**No-shame copy (frozen templates).** Only the member's own numbers; never
another member's name, count, rank or percentile; never the reason a week
counted. Banned patterns: broken, lost, failed, missed, no-show, at risk,
"don't break", and any call to action to attend. Exactly one of:

| State | Member-facing copy |
| --- | --- |
| No eligible facts in window (never attended, opted out, erased and fresh member are indistinguishable) | "No host check-ins are recorded for you in this server yet." |
| Current streak at least 2 (including pending) | "You've been checked in {N} weeks in a row (based on host check-ins)." Capped runs read "at least {N}". |
| Has facts, current streak 0 or 1 | "Your last recorded check-in was in the week of {Monday date}." When longest is at least 2, append " Your longest run in the last 53 weeks was {L} weeks." |

One reply per invocation, no mentions, no week-by-week history (§5). Failure
replies (store unavailable, not a guild context) name no streak state.

**Bot-noise budget.** Streak-attributable output in v1 is ephemeral replies
only: zero channel messages and zero DMs, so it contributes 0 to the
scorecard bot-noise ratio (§2). The budget binds any future non-ephemeral
surface, measured per guild per closed scorecard week: streak messages must
stay **below 20% of all bot-authored messages in human channels**, and the
guild bot-noise ratio must stay below the existing 0.2 alert line (so streak
traffic cannot be the marginal cause of a `BOT_NOISE_HIGH` week). Small
denominators do not license bursts: in a week with fewer than 20 bot
messages, any streak push volume above zero fails the budget review. A
breach disables the push surface; it is not a request to post more other bot
traffic to dilute the share. Section 6 keeps this as a pilot guardrail.

## 5. Privacy, gaming and abuse

- **Derived, not stored.** V1 keeps no streak table, cache or tombstone, so
  nothing new needs an erasure-manifest entry. If a build ever materializes
  streak state, that store must join the erasure manifest and its schema
  tripwires in the same change, or the build stops.
- **Opt-out and erasure.** Opt-out purges and suppresses `community_facts`
  (§2): the member immediately reads as the no-facts state, future check-ins
  are dropped at the writer, and opting back in starts from new facts only —
  purged weeks never return. The streak read must use `community_facts`
  only: reading `announcements_audit_log` (`event.checkin` rows name the
  member in `target_key`, and opt-out does not purge them, §2) as a fallback
  would resurrect an opted-out member's attendance and is prohibited.
  Erasure removes the facts, so the streak reads as no-facts afterwards; the
  identical neutral copy (§4) keeps never-attended, opted-out and erased
  indistinguishable to the member and to anyone who sees their screen.
- **Guild-active inference.** A displayed number is computed against weeks in
  which *someone* was checked in. The reply exposes only the member's own
  run, longest run and last week, never which weeks were guild-active or who
  attended them. At most it reveals that the guild recorded check-ins in the
  weeks spanned by the member's own run — accepted as an open finding (§8,
  finding 3), with week-by-week history staying out to keep it that small.
- **Host gaming.** A Manage Events holder can already record a check-in for
  any current member, including before a Scheduled event runs (§2); that
  trust boundary predates streaks and every write carries the host as audit
  actor. Streaks raise the incentive, so v1
  removes the payoff: private-only, no XP, roles, badges, leaderboard or any
  other reward reads the number; one week credits at most once per member no
  matter how many occurrences a host fabricates inside it; the existing
  1,000-per-occurrence cap and duplicate dedupe still apply. Residual risk —
  a host fabricating a weekly check-in is indistinguishable from real
  attendance in this data — is accepted only under those conditions. Any
  future reward or public surface must re-review proof strength first (§8,
  finding 2).
- **Member gaming.** RSVP spam cannot credit a week (no fact is written),
  and attending many events in one week cannot stack credit. There is no
  self-check-in path without Manage Events, and that gate is unchanged.

## 6. Metric, pilot and kill/scale rule

This section binds a **future** pilot; nothing here pilots anything. A pilot
needs §8 disposition, a guild whose hosts agree to it, and the §7 slices.

**Primary metric — repeat-attendance rate**, computable from existing facts
with the §3 definitions. For a 4-week window of counting weeks:
`repeat_rate = members with eligible check-ins in at least 2 distinct
counting weeks / members with at least 1 eligible check-in`. Compare the
4 counting weeks immediately before the surface is enabled in the pilot
guild (baseline) with the 4 counting weeks after (pilot). This is a
before/after signal, not causal proof; seasonality and host behavior changes
confound it, and the decision record must say so.

**Decision rule** (review at pilot start + 28 days, recorded by +35 days):

- Minimum evidence first: each window needs at least 20 distinct checked-in
  members and at least 3 counting weeks. Below that the result is
  inconclusive: extend once by 4 counting weeks or drop — never scale.
- **Kill:** pilot repeat rate lifts less than **+5 percentage points** over
  baseline, or total streak invocations in the pilot window fall below 10%
  of the checked-in cohort size (invocations upper-bound distinct viewers,
  so this proves low reach without creating per-member view tracking, §5),
  or any guardrail below breaches. The decision record publishes both window
  denominators so a rate move driven by cohort-size change is visible. Kill
  means removing the surface, not keeping it dark indefinitely.
- **Scale decision** (separate disposition, not automatic): lift at least
  +5 points with guardrails clean.
- **Guardrails** (any breach pauses the pilot regardless of lift): the §4
  noise budget; one validated member complaint that the surface pressures or
  shames them; a validated privacy, opt-out or erasure defect; or evidence
  of fabricated check-ins chasing streaks (for example occurrences whose
  audit trail shows one host mass-recording members with no matching event).

Spec-phase metric, separate from the pilot: by **2026-11-09** every §9
fixture row has an agreed or explicitly rejected outcome and every §8 finding
has a written disposition. Missing either keeps option A.

## 7. Cut-ready future slices (not created here)

| Slice | Scope | Depends on |
| --- | --- | --- |
| S0. Decision gate | Written disposition of §8 findings against the merged spec revision; frozen §3 definition and §4 copy | This spec merged |
| S1. Pure streak core | Eligibility filter, week bucketing and streak computation as framework-free functions in the core crate (`rsvp.rs` style), unit-tested from the §9 matrix; no DB, no Discord | S0 |
| S2. Bounded store read | One read that aggregates distinct week buckets in the query (member weeks plus guild-active weeks) inside the 53-week window — at most 53 buckets out, never raw fact rows — with statement timeout; index need proven against a disposable database, any new index shipped with its migration/rollback review | S1; disposable-DB test path only |
| S3. Private command | Self-only ephemeral command wired through the existing interaction router, §4 copy verbatim, aggregate (member-free) invocation counter; command-wiring security gates for the new shape | S1, S2 |
| S4. Pilot enablement | Guild-scoped default-off enable switch, baseline/pilot computation from existing facts, §6 review recorded on the pilot guild; kill switch is the same switch | S3; pilot-guild host agreement |
| S5. Opt-in sharing exploration | Separate spec for any non-private surface; re-reviews consent, proof strength and the §4 budget before code | S4 scale decision only |

Each slice is one PR-sized change with its own acceptance tests authored
before implementation. No slice may add rewards, leaderboards, push
messages or a streak store by riding along (§5).

## 8. Findings, acceptance boundary and undo

Acceptable to circulate for intent review; **not** a build, security or
privacy approval. Open findings needing independent written disposition
before S1 is cut:

1. Accept the recording-week definition with "recorded" copy, or require
   storing the scheduled event start on (or beside) the fact first — a
   schema change that also revisits late and backdated check-ins.
2. Accept the host-trust residual while the number is private and
   reward-free, and confirm no existing consumer (XP, roles, scorecard
   interventions) may read the streak without a new review.
3. Accept the guild-active-week inference as bounded in §5, or require a
   definition that uses only the member's own facts (which would let long
   absences hide inside one "run" and must then be renamed, not shipped as a
   streak).
4. Confirm the bounded read: prove the 53-week, single-member query stays
   bounded on existing indexes at realistic fact volumes, or accept an
   actor-leading index migration with its rollback story.
5. Accept the frozen §4 copy and the §6 +5-point kill threshold as pilot
   terms, including that inconclusive evidence defaults to no-build.

Record spec acceptance against the exact merged revision before any §7 slice
exists. A material spec edit invalidates that disposition. Default while
unresolved is option A; no timeout auto-approves a build.

ACT: publish this proposed contract and its offline oracle (§9). WHY: fix the
definition, privacy posture and kill rule before anyone weighs a build. UNDO:
revert the docs; no operational change exists to reverse.

## 9. Design matrix and offline oracle

Deterministic fixtures, no network, database, Discord client or credentials.
Weeks are integer Monday indices; `now` is week 100 unless a row passes a
smaller `window`. The oracle consumes **post-filter** week lists (duplicate
facts kept, to prove dedupe); eligibility rows exercise the filter itself.
Every row ends with zero channel messages and zero DMs (v1 has no non-
ephemeral output, §4).

| Case | Controlled input / expected outcome |
| --- | --- |
| filter_proofs | Proofs host_checkin / durable_checkin / voice_600s pass; rsvp and unknown proofs excluded |
| filter_classification | eligible_human passes; bot, webhook and other classifications excluded |
| filter_malformed | Unparseable `occurred_at` (modeled as week `None`) excluded, never bucketed |
| no_facts | Guild active 98-100, member none (never attended, opted out or erased alike) -> current 0, copy NO_RECORDS |
| one_this_week | Member {100} -> current 1, LAST_WEEK copy (no streak claim) |
| three_in_row | Member {98,99,100} -> current 3, STREAK copy |
| duplicate_retries | Facts in weeks [100,100,99,99,100] credit weeks {99,100} once -> current 2 |
| missed_closed_week | Member {97,98,100}, guild active 97-100 -> current 1, longest 2 |
| ended_after_miss | Member {97,98}, guild active {97,98,99}, current week inactive -> current 0, longest 2 |
| inactive_pause | Member {97,99,100}, week 98 inactive guild-wide -> current 3 across the calendar gap |
| cancelled_only_week | Week 99's only event cancelled before check-in (no facts), member {98,100} -> current 2, paused not broken |
| cancelled_after_checkin | Later cancellation is not an input; member {98,99,100} -> current 3 |
| pending_current | Member {98,99}, week 100 counting via others -> current 2, pending, STREAK copy unchanged |
| current_week_inactive | Member {98,99}, week 100 not yet guild-active -> current 2, not pending |
| host_omission | Member recorded {98,100}, week 99 counting via others -> current 1, longest 1 (recorded-attendance limit, §3) |
| longest_past | Member {90,91,92,98,100} over active {90,91,92,93,98,99,100} -> current 1, longest 3 |
| future_fact | A fact bucketed to week 101 with now=100 is excluded -> current 1 from week 100 alone |
| window_capped | Window 4 (weeks 97-100), member {96,97,98,99,100} -> current 4, capped, "at least 4" |
| window_not_capped | Window 4, member {99,100} over active {97,98,99,100} -> current 2, not capped |
| monday_boundary | Sunday 2026-10-25 buckets to Monday 2026-10-19; Monday 2026-10-26 00:00 UTC starts a new bucket |
| noise_budget | Share component of §4: 19 streak of 100 bot messages passes; 20 of 100 fails; any push with a 10-message denominator fails |

Runnable offline document check (Python stdlib; from repo root):

```sh
python3 -m doctest -v docs/event-streaks-spec.md
python3 scripts/check_docs_index.py --json
```

The doctest checks the **specified oracle's consistency** with a tiny
reference model, not real SQL, classification, Discord delivery or deployed
behavior. S1/S2 acceptance tests must re-prove the same rows against the
actual core functions and a disposable database (§7).

```pycon
>>> import datetime, json
>>> PROOFS = {"host_checkin", "durable_checkin", "voice_600s"}
>>> def eligible_weeks(facts):
...     """facts: (week|None, classification, proof) -> post-filter weeks, order kept."""
...     return [w for w, cls, proof in facts
...             if w is not None and cls == "eligible_human" and proof in PROOFS]
>>> def streak(member_weeks, guild_weeks, now=100, window=53):
...     """Returns (current, longest, pending, capped, last) per the spec definition."""
...     lo = now - window + 1
...     counting = sorted({w for w in list(guild_weeks) + list(member_weeks) if lo <= w <= now})
...     att = {w for w in member_weeks if lo <= w <= now}
...     longest = run = 0
...     for w in counting:
...         run = run + 1 if w in att else 0
...         longest = max(longest, run)
...     seq, dropped = counting, False
...     if counting and counting[-1] == now and now not in att:
...         seq, dropped = counting[:-1], True
...     current = 0
...     for w in reversed(seq):
...         if w not in att:
...             break
...         current += 1
...     capped = current > 0 and seq[-current] == lo
...     pending = dropped and current > 0
...     return (current, longest, pending, capped, max(att) if att else None)
>>> def copy_state(result):
...     current, longest, _, _, last = result
...     if last is None:
...         return "NO_RECORDS"
...     return "STREAK" if current >= 2 else "LAST_WEEK"
>>> def noise_ok(streak_msgs, bot_msgs):
...     return streak_msgs == 0 or (bot_msgs >= 20 and streak_msgs * 5 < bot_msgs)
>>> cases = {
...     "no_facts": (([], [98, 99, 100]), (0, 0, False, False, None)),
...     "one_this_week": (([100], [98, 99, 100]), (1, 1, False, False, 100)),
...     "three_in_row": (([98, 99, 100], [98, 99, 100]), (3, 3, False, False, 100)),
...     "duplicate_retries": (([100, 100, 99, 99, 100], [99, 100]), (2, 2, False, False, 100)),
...     "missed_closed_week": (([97, 98, 100], [97, 98, 99, 100]), (1, 2, False, False, 100)),
...     "ended_after_miss": (([97, 98], [97, 98, 99]), (0, 2, False, False, 98)),
...     "inactive_pause": (([97, 99, 100], [97, 99, 100]), (3, 3, False, False, 100)),
...     "cancelled_only_week": (([98, 100], [98, 100]), (2, 2, False, False, 100)),
...     "cancelled_after_checkin": (([98, 99, 100], [98, 99, 100]), (3, 3, False, False, 100)),
...     "pending_current": (([98, 99], [98, 99, 100]), (2, 2, True, False, 99)),
...     "current_week_inactive": (([98, 99], [98, 99]), (2, 2, False, False, 99)),
...     "host_omission": (([98, 100], [98, 99, 100]), (1, 1, False, False, 100)),
...     "longest_past": (([90, 91, 92, 98, 100], [90, 91, 92, 93, 98, 99, 100]), (1, 3, False, False, 100)),
...     "future_fact": (([100, 101], [100, 101]), (1, 1, False, False, 100)),
... }
>>> for name, (args, expected) in cases.items():
...     assert streak(*args) == expected, name
>>> assert streak([96, 97, 98, 99, 100], [96, 97, 98, 99, 100], window=4) == (4, 4, False, True, 100)
>>> assert streak([99, 100], [97, 98, 99, 100], window=4) == (2, 2, False, False, 100)
>>> assert copy_state(streak([], [98, 99, 100])) == "NO_RECORDS"
>>> assert copy_state(streak([98, 99], [98, 99, 100])) == "STREAK"
>>> assert copy_state(streak([100], [98, 99, 100])) == "LAST_WEEK"
>>> # filter_proofs + filter_classification + filter_malformed
>>> raw = [(100, "eligible_human", "host_checkin"), (100, "eligible_human", "durable_checkin"),
...        (99, "eligible_human", "voice_600s"), (100, "eligible_human", "rsvp"),
...        (100, "eligible_human", "mystery"), (100, "bot", "host_checkin"),
...        (100, "webhook", "host_checkin"), (None, "eligible_human", "host_checkin")]
>>> assert eligible_weeks(raw) == [100, 100, 99]
>>> assert streak(eligible_weeks(raw), [99, 100]) == (2, 2, False, False, 100)
>>> # monday_boundary: Monday 00:00 UTC buckets (week_start_ms parity)
>>> def monday(d):
...     return d - datetime.timedelta(days=d.weekday())
>>> assert monday(datetime.date(2026, 10, 25)) == datetime.date(2026, 10, 19)
>>> assert monday(datetime.date(2026, 10, 26)) == datetime.date(2026, 10, 26)
>>> # noise_budget: strictly below 20 percent, small denominators cannot license a push
>>> assert noise_ok(0, 0) and noise_ok(19, 100) and not noise_ok(20, 100) and not noise_ok(1, 10)
>>> extra = ["filter_proofs", "filter_classification", "filter_malformed",
...          "window_capped", "window_not_capped", "monday_boundary", "noise_budget"]
>>> assert len(cases) + len(extra) == 21
>>> print(json.dumps({"schema": "events.streaks.oracle.v1", "fixtures": 21, "status": "pass"}, sort_keys=True))
{"fixtures": 21, "schema": "events.streaks.oracle.v1", "status": "pass"}

```
