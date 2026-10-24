# Events: one organizer-published post-session recap

Status: **proposed, design only**. Written 2026-10-24; review by 2026-10-31.
No commands, storage, migrations, permissions, sends or runtime tests are added.
Merge publishes a proposal, not permission to implement or activate it. This file
is the single repository contract; review requests bind its commit and SHA-256.

## 1. Decision and evidence

Consumer: an organizer after a game night, and members who want to see it
happened. Hypothesis: one explicit, previewed, once-only recap line saves manual
bookkeeping. No customer validation is claimed.

[Discap](https://discap.bot/) posts "the transcript, AI recap and audio to your
server when the call ends"; [Recap Raven](https://recapraven.com/) writes
player-safe D&D recaps; attendance bots offer "visual or exportable reports on
attendance" ([survey](https://grokipedia.com/page/Discord_event_attendance_bots)).
Read 2026-10-24. These are comparable-capability evidence, not TWO demand, and
all of them record or summarize content. TWO's version renders only data it
already holds, on an explicit organizer action. No recording, transcription, AI
summary, DM, export file, or automatic end-time post.

| Option | Value and cost | Disposition |
| --- | --- | --- |
| A. No build; manual wrap-up | Closed LFG posts already keep the roster (§2); the organizer types a thank-you. No new boundary | Standing default; retain if the boundary below is not accepted |
| B. Totals-only line | One counts-only post (Going, Interested, checked in); no names, no mentions; needs one read and a once-only fence | **Smallest option that tests this hypothesis; recommend for spec review, not build** |
| C. Signup/check-in recap card | Adds display names. Names re-publish signup rosters the LFG post already shows and add host-recorded check-ins the member never published | Do not build; bounded below only so review can reject it with evidence |

Benefit of B over A is small and unproven: a once-only public "N checked in"
line (no member-facing check-in count reader was found in the cited seams).
Uncertainty: demand, and whether the counts are worth a new channel post. C is
not recommended: (1) the signup names are already in the closed LFG post; (2) a
check-in name is a host-recorded fact, a different consent than a self-signup;
(3) opt-out does not purge signups or RSVPs (§5); (4) a published name cannot
be recalled by TWO erasure; (5) no key links an LFG post to a scheduled event.

## 2. Verified baseline and missing seams

Inspected repository head: `89b26ae050ffba9fb409622ffd54d31cdc79e7fc`.

- `crates/core/src/lfg.rs:437` `lfg_content` renders title, state, per-role fill
  and `<@id>` rosters; `crates/discord/src/lfg_interactions.rs:718` sends
  `allowed_mentions: {parse: []}`. `lfg_store.rs:383` close edits that post, so
  option A already shows who signed up, without a ping.
- `lfg_posts` (`0170_lfg.sql:10-21`) has no scheduled-event column. LFG signups
  and RSVP/check-in facts share no join key: a combined card needs a new link
  or two organizer-supplied IDs, and no such seam exists.
- `crates/core/src/rsvp.rs:279` accepts a check-in occurrence as a scheduled
  event ID or `{id}:{label}`; `:229` gates Manage Events; `:446-447` cap RSVPs
  and check-ins at **1,000** each; `:424` audits `event.checkin`. Check-ins are
  `community_facts` rows with `source = checkin_source(occurrence)`, indexed by
  `0426_community_facts_guild_source.sql` as `(guild_id, source)`.
- `crates/discord/src/rsvp.rs:464-470` proves an occurrence with a live
  guild-scoped `get_scheduled_event` before any write; the recap reuses that read.
- `crates/cutover/src/privacy.rs:44` opt-out purges only `events`, `members`,
  `invite_snapshots`, `community_facts` and installs suppression. It does **not**
  touch `event_rsvps` or `lfg_signups`. `member_erasure_plan.json:19-30,144-157`
  deletes those tables and the audit rows whose `actor_id` or `target_key`
  matches the member.
- `announcements_audit_log` (`0160_rsvp.sql:36-45`) has `id TEXT PRIMARY KEY` and
  `rsvp_store::write_audit` inserts with `ON CONFLICT DO NOTHING`; the LFG
  signup path already uses deterministic audit IDs as a replay fence. Audit
  retention is `AUDIT_RETENTION_DAYS = 365` (`rsvp.rs:456`).

No live guild, permission, deployment or host workflow was inspected. If a
once-only fence needs a new table or the counts cannot stay bounded, stop at A.

## 3. Trigger, authorization and identity

**Trigger.** Only an explicit organizer action; never automatic at an event's
end time and never from a timer, gateway event or status change. Proposed shape:
one command taking an occurrence (scheduled event ID, as `/attendance` does),
answering with an **ephemeral preview** and a Publish button (§7). Nothing is
posted by running the command.

**Identity.** `(guild_id, canonical_occurrence_id)` from
`parse_attendance_occurrence`; never a title or a caller-supplied name. A
labeled repeat is its own occurrence. Series rollups are out of scope.

**Authorization**, re-evaluated on **both** the command and the button click,
from the interaction payload only: configured guild, feature enabled, a human
member holding fresh Manage Events, and that member is the one the preview was
issued to. A role change between preview and click refuses. Missing or unknown
evidence fails closed. No cached grant, no role or permission change here.

**Event state**, from the live read: eligible when status is Completed, or Active
and started (the organizer is closing it). Canceled is refused. Scheduled with a
future start (rescheduled) is refused as not ended. Unknown or foreign IDs
collapse to one unavailable answer with no existence oracle.

## 4. Content bounds, destination and edit/delete

**B content (recommended).** One message: event title (neutralized, truncated),
`Going N · Interested M · Checked in C`, and `Snapshot <t:epoch:f>`. Counts only.
Declined is never published. Counts come from existing rows: RSVP statuses
Going/Interested, and `eligible_human` check-in facts for the occurrence, each
at most 1,000 by the existing caps. **Zero** mentions and zero member IDs; the
send sets `allowed_mentions: {parse: []}`. A recap with 0/0/0 is EMPTY and
cannot be published.

**C content (specified only to be rejected).** Adds at most 25 display names per
list as inert `<@id>` text, never ping-enabled, with "+K more". Each shown name
needs a live current-membership check (at most 25 Discord reads) and a
suppression check; it would never include DM content, voice content, presence
history or message text. C stays out until §8 findings 1-2 are accepted.

**Destination.** The channel where the organizer ran the command, only if the bot
can send there and the guild matches. No channel parameter, no DM, no thread
creation, no cross-guild post, no second destination.

**Edit/delete.** A published recap is an immutable snapshot: TWO never edits it,
never re-posts after a delete, and a repeat Publish returns ALREADY_PUBLISHED
even if the message was deleted. A Manage Messages holder deletes it in Discord;
TWO neither detects nor repairs that. Fresh counts need a new event occurrence.

**Retention and erasure.** No recap store, no export, no preview history. B
contains no personal data, so erasure has nothing to recall; the only retained
rows are the two audit rows (§6), which follow the existing 365-day audit
retention and the existing actor-based erasure. Eligibility is limited to
events that started within 30 days, so an expired audit fence cannot reopen an
old event.

## 5. Privacy, opt-outs and guild isolation

- Guild isolation: every read and the audit key carry `guild_id`; the live event
  read must return the same ID and guild.
- Opted-out members: their check-in facts are purged and future writes are
  suppressed, so they are absent from C. Their `event_rsvps` rows persist, so
  the Going/Interested counts must anti-join `privacy_opt_outs` at read time; a
  count that includes them fails the fixture matrix.
- Members who left: RSVP rows persist after departure, and no bounded
  local current-membership source was verified. B counts are therefore labeled
  as **recorded** responses and may include departed members (counts identify
  nobody). Excluding them needs a bounded source or at most 25 live reads, and
  makes C unacceptable until proven. This is open finding 2.
- Small groups: a count of 1 reveals only that someone checked in; no new
  attribute is disclosed beyond the organizer's own preview.
- No DM content, voice content, presence history or message text is read.
- Public posting: B publishes a count to the destination channel's audience, an
  existing audience of the organizer's choosing; the organizer previews it first.

## 6. Once-only publish, audit and restarts

No new table. At-most-once uses `announcements_audit_log` with deterministic IDs
and `ON CONFLICT DO NOTHING`, plus a per-recap advisory lock
`recap:{guild}:{occurrence}` like the LFG locks.

1. Under the lock, insert `event.recap.claim:{guild}:{occurrence}` (actor =
   organizer, outcome `claimed`). A conflict means a prior claim exists: return
   ALREADY_PUBLISHED, or COMMIT_UNKNOWN when no `posted` row exists.
2. Send once with a nonce derived from the claim and mentions suppressed.
3. On accepted send, insert `event.recap.posted:{guild}:{occurrence}` with the
   message ID. On a definite pre-send rejection, record `failed` and release the
   claim only through the same lock, never after an ambiguous send.

A restart between 1 and 3 leaves a claim without a `posted` row: that state is
**unknown, not absent**. The runtime may run one bounded recovery scan (at most 3
history pages of 100 messages, bot author and nonce, as the LFG path does) and
otherwise reports COMMIT_UNKNOWN and never re-sends automatically. Discord's
nonce window is minutes, so after it the answer stays unknown. Two organizers
clicking at once: one wins the claim, the other receives ALREADY_PUBLISHED.
Audit facts: actor, guild, occurrence, outcome, counts, snapshot time, template
version; never names or message text. Template version `recap.v1` is part of the
preview digest, so a deploy between preview and click refuses as PREVIEW_STALE
(the claim fence stays keyed by occurrence, not version). Erasing the organizer
deletes the claim row by `actor_id`, so a later repeat Publish could succeed:
the cost is one more counts-only line (open finding 3).

## 7. Private publish UX, results and agent contract

Preview (ephemeral, only the organizer): the exact text that will post, the
destination channel, "Nobody is pinged", "Posts once", and the snapshot time.
Buttons: Publish and Cancel. The Publish `custom_id` binds guild, occurrence,
organizer, template version and a digest of the rendered text. On click the
service re-authorizes, re-reads the event, re-renders, and refuses
PREVIEW_STALE if the digest differs; nothing is posted on a mismatch. A Cancel
or an ignored preview writes nothing. Failure replies name the reason and the
next action, never raw SQL or Discord errors.

No executable CLI or API is registered. Future adapters discover this file via
the [documentation map](README.md). **Proposed typed result**:
`schema=events.recap.v1`, `guild_id`, `occurrence_id`, `message_id` (when posted),
`going`, `interested`, `checked_in`, `snapshot_epoch`, `dry_run`, `reason`,
`retryable`, `next_action`. `dry_run=true` is the ephemeral preview; it writes
nothing and has no wrong-flag production path. Stable IDs: the occurrence and
posted message ID.

| reason | retryable / next_action |
| --- | --- |
| `PREVIEW` | false / `publish_or_cancel` |
| `POSTED` | false / `none` |
| `EMPTY` | false / `nothing_to_publish` |
| `DENIED` | false / `ask_event_manager` |
| `UNAVAILABLE` | false / `verify_event` (missing, foreign or unreadable) |
| `EVENT_CANCELED` | false / `none` |
| `NOT_ENDED` | false / `retry_after_event_ends` |
| `ALREADY_PUBLISHED` | false / `none` |
| `PREVIEW_STALE` | false / `run_preview_again` |
| `COMMIT_UNKNOWN` | true / `inspect_channel` |
| `STORE_UNAVAILABLE` | true / `retry_after_backoff` |

## 8. Findings, acceptance boundary, review metric and undo

**CISO proposal assessment:** acceptable to circulate for intent review; **not** a
grant, not an approval of our own boundary request, and not runtime acceptance.
Open findings needing independent written disposition before any build card:
(1) accept or reject publishing attendance counts, and keep names (C) out;
(2) accept "recorded" counts that may include departed members, or require a
bounded membership source; (3) accept the audit-fence loss on organizer
erasure; (4) confirm opt-out anti-join at read time for RSVP counts.

Record CEO spec acceptance **and** host intent alignment against the exact
revision before any implementation subtask, protected test handoff or live
pilot. Route the brief through the President/COO to the CEO and the existing host
intent path; a queue receipt is not alignment. Nothing goes directly to the owner
or an invented executor. A material revision needs a fresh review request.

By 2026-10-31 the metric is **all 14 fixture rows have agreed or explicitly
rejected outcomes and the four findings have written disposition**. This measures
design completeness, not engagement. **Keep only if** content is provably bounded
to already-held data with opt-outs honored; otherwise the answer is **no-build**
(A). Default while unresolved is A; no timeout auto-approval; no live experiment.
A demand experiment needs its own hypothesis, metric and date.

ACT: publish this proposed contract and its offline oracle. WHY: bound the
attendance-data exposure before considering a recap. UNDO: revert the docs; no
operational change exists. A later accepted build keeps admission dark until its
own tests, security and release gates pass; disabling it must keep the audit
fence and erasure behavior. No production tests, migrations or activation here.

## 9. Offline fixture contract

Deterministic fixtures use guild `g1`, event `e1`, base rows Going 3, Interested
2, checked-in 2, one organizer `o` holding Manage Events, and no network,
database, Discord client or credentials. Every row ends with zero DMs and at most
one posted message. Counts are `going/interested/checked_in`.

| Case | Controlled event / expected assertion |
| --- | --- |
| no_signups | 0 RSVPs, 2 check-ins -> PREVIEW 0/0/2 (walk-ins) |
| no_checkins | 5 RSVPs, 0 check-ins -> PREVIEW 3/2/0 |
| empty | 0/0/0 -> EMPTY, publish refused |
| opted_out | 1 Going member opted out -> PREVIEW 2/2/2, not counted |
| left | 1 Going member left -> PREVIEW 3/2/2 labeled recorded (finding 2) |
| canceled | Event Canceled -> EVENT_CANCELED, 0 posts |
| rescheduled | Scheduled, future start -> NOT_ENDED, 0 posts |
| denied | Organizer lost Manage Events before click -> DENIED, 0 posts |
| foreign | Event in guild `g2` -> UNAVAILABLE, no oracle |
| duplicate | Publish twice -> POSTED then ALREADY_PUBLISHED, 1 post |
| concurrent | Two clicks at once -> one POSTED, one ALREADY_PUBLISHED, 1 post |
| restart | Claim row, no posted row -> COMMIT_UNKNOWN, no resend |
| stale | Counts change between preview and click -> PREVIEW_STALE, 0 posts |
| version | `recap.v1` preview clicked under `recap.v2` -> PREVIEW_STALE |

Runnable offline document check (Python stdlib; from repo root):

```sh
python3 -m doctest -v docs/event-recap-spec.md
python3 scripts/check_docs_index.py --json
```

The doctest checks the **specified oracle's consistency** with a tiny reference
model, not real SQL, authentication, rendering, locking, erasure or deployed
behavior. Protected acceptance tests must later use barriers, injected clocks
and store/REST faults against actual callers in disposable fixtures; zero tests
is not proof. No acceptance or implementation task is created here.

```pycon
>>> import json
>>> def recap(going=3, interested=2, checked=2, opted_out=0, status="completed",
...           future=False, manager=True, same_guild=True, posted=0, claimed=False,
...           stale=False, version="v1"):
...     """Reference model: returns (reason, going, interested, checked, posts)."""
...     if not same_guild:
...         return ("UNAVAILABLE", 0, 0, 0, 0)
...     if not manager:
...         return ("DENIED", 0, 0, 0, 0)
...     if status == "canceled":
...         return ("EVENT_CANCELED", 0, 0, 0, 0)
...     if future:
...         return ("NOT_ENDED", 0, 0, 0, 0)
...     if version != "v1" or stale:
...         return ("PREVIEW_STALE", 0, 0, 0, 0)
...     if claimed and not posted:
...         return ("COMMIT_UNKNOWN", 0, 0, 0, 0)
...     g, i = going - opted_out, interested
...     if (g, i, checked) == (0, 0, 0):
...         return ("EMPTY", 0, 0, 0, 0)
...     return ("ALREADY_PUBLISHED" if posted else "PREVIEW", g, i, checked, posted)
>>> cases = {
...     "no_signups": (dict(going=0, interested=0), ("PREVIEW", 0, 0, 2, 0)),
...     "no_checkins": (dict(checked=0), ("PREVIEW", 3, 2, 0, 0)),
...     "empty": (dict(going=0, interested=0, checked=0), ("EMPTY", 0, 0, 0, 0)),
...     "opted_out": (dict(opted_out=1), ("PREVIEW", 2, 2, 2, 0)),
...     "left": ({}, ("PREVIEW", 3, 2, 2, 0)),
...     "canceled": (dict(status="canceled"), ("EVENT_CANCELED", 0, 0, 0, 0)),
...     "rescheduled": (dict(future=True), ("NOT_ENDED", 0, 0, 0, 0)),
...     "denied": (dict(manager=False), ("DENIED", 0, 0, 0, 0)),
...     "foreign": (dict(same_guild=False), ("UNAVAILABLE", 0, 0, 0, 0)),
...     "duplicate": (dict(posted=1), ("ALREADY_PUBLISHED", 3, 2, 2, 1)),
...     "restart": (dict(claimed=True), ("COMMIT_UNKNOWN", 0, 0, 0, 0)),
...     "stale": (dict(stale=True), ("PREVIEW_STALE", 0, 0, 0, 0)),
...     "version": (dict(version="v2"), ("PREVIEW_STALE", 0, 0, 0, 0)),
... }
>>> for name, (kwargs, expected) in cases.items():
...     assert recap(**kwargs) == expected, name
>>> # concurrent: one claim wins under the unique audit id, the other conflicts
>>> claims = {}
>>> def click(who):
...     return "POSTED" if claims.setdefault("e1", who) == who else "ALREADY_PUBLISHED"
>>> assert sorted([click("o1"), click("o2")]) == ["ALREADY_PUBLISHED", "POSTED"]
>>> assert len(cases) + 1 == 14
>>> assert all(p <= 1 for _, (_, e) in cases.items() for p in e[4:])
>>> print(json.dumps({"schema": "events.recap.oracle.v1", "fixtures": 14, "status": "pass"}, sort_keys=True))
{"fixtures": 14, "schema": "events.recap.oracle.v1", "status": "pass"}

```
