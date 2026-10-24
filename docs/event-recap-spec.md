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
  retention is `AUDIT_RETENTION_DAYS = 365` (`rsvp.rs:456`), a floor on the
  cutoff, not a sweeper: `prune_rsvp_history` (`rsvp_store.rs:686-715`)
  deletes audit rows only `WHERE action IN (event.rsvp, event.checkin)`.
- RSVP reads are keyed by bare snowflake: `validate_event_id`
  (`rsvp.rs:94-100`) accepts snowflakes only, and `list_rsvps`
  (`rsvp_store.rs:234`) selects by `(guild_id, event_id)`. Check-in facts use
  `source = checkin_source(canonical)` (`rsvp.rs:351-353`), which includes a
  label for labeled occurrences. The two stores share a recap key only for
  bare event IDs, hence the §3 restriction.
- Component IDs are capped at 100 chars (`MAX_VOICE_CUSTOM_ID_CHARS`,
  `voice_custom_id.rs:39-40`), with a 16-lowercase-hex content-hash precedent
  (`IMPORT_HASH_CHARS`, `:69-70`); occurrence IDs run to 128 chars
  (`OCCURRENCE_ID_MAX_CHARS`, `commands.rs:59`), so a labeled occurrence
  cannot be bound literally and the recap binds bare IDs only (§7).
- Public sends pass through the shared `message_safety::content`
  neutralizer (`crates/core/src/message_safety.rs:104-106`). It breaks mass
  mentions only: user and role mentions are kept readable
  (`message_safety.rs:36-37`, asserted at `:145-151`). LFG titles are bounded
  to 100 UTF-16 units (`MAX_TITLE_CHARS`, `lfg.rs:36`). The recap title
  contract (§4) reuses both and adds its own markup strip.

No live guild, permission, deployment or host workflow was inspected. If a
once-only fence needs a new table or the counts cannot stay bounded, stop at A.

## 3. Trigger, authorization and identity

**Trigger.** Only an explicit organizer action; never automatic at an event's
end time and never from a timer, gateway event or status change. Proposed shape:
one command taking an occurrence (scheduled event ID, as `/attendance` does),
answering with an **ephemeral preview** and a Publish button (§7). Nothing is
posted by running the command.

**Identity.** `(guild_id, event_id)` where `event_id` is a bare scheduled-event
snowflake; never a title or a caller-supplied name. Labeled
`{id}:{label}` input is refused as LABELED_UNSUPPORTED: RSVP rows are keyed
by bare ID while labeled check-ins live under a labeled source, so no single
read key covers a labeled occurrence, and a labeled ID cannot fit the §7
component binding either. Series rollups are out of scope.

**Authorization**, re-evaluated on **both** the command and the button click,
from the interaction payload only: configured guild, feature enabled, a human
member holding fresh Manage Events, and that member is the one the preview was
issued to. A role change between preview and click refuses. Missing or unknown
evidence fails closed. No cached grant, no role or permission change here.

**Event state**, from the live read, as an explicit allowlist. Eligible: status
Completed, or Active and started (the organizer is closing it). Canceled is
refused as EVENT_CANCELED. Any other known status (Scheduled at any start
time, Active but not started) is refused as NOT_ENDED. An unknown status, or
an unknown or foreign ID, collapses to one UNAVAILABLE answer with no
existence oracle. **Age:** an eligible event whose scheduled start is more than
30 days (2,592,000 s) before the evaluation time is refused as EVENT_TOO_OLD;
exactly 30 days is still eligible. The age is evaluated at preview **and**
again at click, so an event that crosses 30 days between them refuses at click.

## 4. Content bounds, destination and edit/delete

**B content (recommended).** One message: event title, `Going N · Interested M
· Checked in C`, and `Snapshot <t:epoch:f>`. Counts only. Declined is never
published. The title contract is exact, in this order: (1) trim; (2) pass through the
shared `message_safety::content` neutralizer, which covers **mass mentions only**
(`@everyone`/`@here`); (3) delete every Discord markup token matching
`<[^<>]*>` whole (user `<@id>`, `<@!id>`, role `<@&id>`, channel `<#id>`,
custom emoji, timestamps, slash-command and guild-navigation tokens), then
delete any stray `<` or `>`, so no member, role or channel ID survives in the
title; (4) collapse whitespace runs to one space and trim; (5) an empty result
becomes the literal `Event`; (6) truncate to 100 UTF-16 units with an ellipsis.
Plain markdown formatting characters are left as inert text. The send sets
`allowed_mentions: {parse: []}` as a second layer, not the definition.
Counts come from existing rows read by the bare event ID: RSVP statuses
Going/Interested via `list_rsvps`, and `eligible_human` check-in facts via
`checkin_source` of that same bare ID, each at most 1,000 by the existing
caps. **Zero** mentions and zero member IDs. A recap with 0/0/0 is EMPTY and
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
rows are the §6 audit rows under actions `event.recap.claim`,
`event.recap.posted` and `event.recap.failed` (§6 keys the failed row by send
nonce, so repeated rejections keep one row each). Those actions are outside the
current prune scope (§2), so a build MUST extend the prune to cover them at
the same 365-day floor, under the existing actor-based erasure; until it
does, the retention claim is a build requirement, not a property. Eligibility
is limited to events that started within 30 days (§3, EVENT_TOO_OLD), so an
expired audit fence cannot reopen an old event.

## 5. Privacy, opt-outs and guild isolation

- Guild isolation: every read and the audit key carry `guild_id`; the live event
  read must return the same ID and guild.
- Opted-out members: their check-in facts are purged and future writes are
  suppressed, so they are absent from C. Their `event_rsvps` rows persist, so
  the Going and Interested counts must each anti-join `privacy_opt_outs` at
  read time, with a separate exclusion per status; a count in either status
  that includes them fails the fixture matrix.
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
and `ON CONFLICT DO NOTHING`, plus a per-recap **session-level** advisory lock
`recap:{guild}:{occurrence}` (like the LFG locks) held from step 1 through
step 3, so a concurrent clicker waits and then sees the outcome. The claim
insert in step 1 commits on its own, before the send, so a crash leaves it
visible.

1. Under the lock, first check the rows: a `posted` row returns
   ALREADY_PUBLISHED; a claim row without a `posted` row returns COMMIT_UNKNOWN
   for **every** caller, including the organizer who made the claim. Otherwise
   insert id `event.recap.claim:{guild}:{occurrence}` with action
   `event.recap.claim` (actor = organizer, outcome `claimed`).
2. Send once with a nonce derived from SHA-256 of the claim ID and the click's
   interaction ID, and mentions suppressed.
3. On accepted send, insert id `event.recap.posted:{guild}:{occurrence}` with
   action `event.recap.posted` and the message ID. On a definite pre-send
   rejection (permission refused before any send), run **one database
   transaction** that inserts id `event.recap.failed:{guild}:{occurrence}:{nonce}`
   with action `event.recap.failed` (outcome `failed`, reason SEND_REJECTED) and
   DELETEs the claim row, commit it, release the lock and return SEND_REJECTED.
   The two writes apply together or not at all. A later retry claims fresh and
   posts at most once; each rejection leaves its own failed row because the
   nonce differs per click. Never release after an ambiguous send: that stays
   COMMIT_UNKNOWN.

If the process dies inside that transaction it rolls back whole: the claim stays
and no failed row exists, so the state is the restart state below
(COMMIT_UNKNOWN, no resend). "Claim plus a failed row for the same nonce" cannot
occur. The cost is one stuck event; the organizer posts a wrap-up by hand
(option A).

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
Buttons: Publish and Cancel. The Publish `custom_id` is exactly
`recap:v1:{guild}:{event}:{organizer}:{snapshot_epoch}:{digest12}`, all fields
literal: 20-digit-max snowflakes, a 10-digit Unix-seconds `snapshot_epoch` (the
preview's clock reading, the **only** time source for the rendered
`Snapshot <t:epoch:f>`), and a 12-lowercase-hex digest of SHA-256 over guild,
occurrence, organizer, template version, `snapshot_epoch` and rendered text.
Worst case is 95 chars, inside the 100-char component cap, and the occurrence
and epoch are recovered by splitting on `:` with no preview store (bare
snowflakes and epochs contain no `:`). On click the service re-authorizes,
re-reads the event, re-renders with the **carried** epoch (never the click
clock), and refuses PREVIEW_STALE if the digest differs, if the carried epoch is
later than the click time, or if it is more than 900 s (15 min) old; nothing is
posted on a mismatch. Time passing with unchanged data and an epoch inside 900 s
posts, and the posted `Snapshot` equals the preview's. A Cancel
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
| `UNAVAILABLE` | false / `verify_event` (missing, foreign, unreadable or unknown status) |
| `EVENT_CANCELED` | false / `none` |
| `NOT_ENDED` | false / `retry_after_event_ends` |
| `EVENT_TOO_OLD` | false / `none` (start more than 30 days ago, at preview or click) |
| `LABELED_UNSUPPORTED` | false / `use_bare_event_id` |
| `ALREADY_PUBLISHED` | false / `none` |
| `PREVIEW_STALE` | false / `run_preview_again` (digest, version, epoch in the future or older than 900 s) |
| `COMMIT_UNKNOWN` | true / `inspect_channel` |
| `SEND_REJECTED` | true / `restore_permission_and_retry` |
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

By 2026-10-31 the metric is **all 27 fixture rows have agreed or explicitly
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
| opted_out_interested | 1 Interested member opted out -> PREVIEW 3/1/2, not counted |
| left | 1 Going member left -> PREVIEW 3/2/2 labeled recorded (finding 2) |
| labeled | `{id}:{label}` input -> LABELED_UNSUPPORTED, 0 posts |
| canceled | Event Canceled -> EVENT_CANCELED, 0 posts |
| rescheduled | Scheduled, future start -> NOT_ENDED, 0 posts |
| not_started | Active, not started -> NOT_ENDED, 0 posts |
| unknown_status | Unknown status -> UNAVAILABLE, no oracle |
| age_boundary | Completed, started exactly 30 days ago -> PREVIEW 3/2/2 |
| too_old | Completed, started 30 days + 1 s ago -> EVENT_TOO_OLD, 0 posts |
| age_crossing | Preview at 30 days passes; click 1 s later -> EVENT_TOO_OLD, 0 posts |
| mention_title | Title `Finals <@123456789012345678> <@&5> <#9>` -> rendered `Finals`, no ID, no markup |
| denied | Organizer lost Manage Events before click -> DENIED, 0 posts |
| foreign | Event in guild `g2` -> UNAVAILABLE, no oracle |
| duplicate | Publish twice -> POSTED then ALREADY_PUBLISHED, 1 post |
| concurrent | Two clicks at once -> one POSTED, one ALREADY_PUBLISHED, 1 post |
| restart | Claim row, no posted row: click by the claimant or another organizer -> COMMIT_UNKNOWN, no resend |
| send_rejected | Definite pre-send rejection -> SEND_REJECTED, claim released, 0 posts; a retry posts once; a second rejection keeps its own failed row |
| interrupted_cleanup | Crash inside the failed-insert + claim-delete transaction -> rolled back whole: claim stays, no failed row, next click COMMIT_UNKNOWN |
| stale | Counts change between preview and click -> PREVIEW_STALE, 0 posts |
| version | `recap.v1` preview clicked under `recap.v2` -> PREVIEW_STALE |
| epoch_advance | Click 5 s after preview, data unchanged -> POSTED with the preview's `Snapshot`, 1 post |
| preview_expired | Click 901 s after preview -> PREVIEW_STALE, 0 posts; future epoch -> PREVIEW_STALE |
| custom_id_max | 20-digit guild/event/organizer + 10-digit epoch + 12-hex digest -> custom_id 95 chars, occurrence and epoch literal |

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
>>> import json, re
>>> def render_title(raw):
...     """Title contract steps 1, 3, 4, 5 (the Rust neutralizer, step 2, is not modeled)."""
...     t = re.sub(r"<[^<>]*>", "", raw.strip())
...     t = re.sub(r"[<>]", "", t)
...     return re.sub(r"\s+", " ", t).strip() or "Event"
>>> def recap(going=3, interested=2, checked=2, opted_out_going=0,
...           opted_out_interested=0, status="completed", started=True,
...           manager=True, same_guild=True, labeled=False, posted=0,
...           claimed=False, send_rejected=False, stale=False, version="v1",
...           age_s=0, preview_age_s=0, epoch_in_future=False):
...     """Reference model: returns (reason, going, interested, checked, posts)."""
...     if not same_guild:
...         return ("UNAVAILABLE", 0, 0, 0, 0)
...     if not manager:
...         return ("DENIED", 0, 0, 0, 0)
...     if labeled:
...         return ("LABELED_UNSUPPORTED", 0, 0, 0, 0)
...     if status == "canceled":
...         return ("EVENT_CANCELED", 0, 0, 0, 0)
...     if status != "completed" and not (status == "active" and started):
...         if status not in ("scheduled", "active"):
...             return ("UNAVAILABLE", 0, 0, 0, 0)
...         return ("NOT_ENDED", 0, 0, 0, 0)
...     if age_s > 30 * 86400:
...         return ("EVENT_TOO_OLD", 0, 0, 0, 0)
...     if version != "v1" or stale or preview_age_s > 900 or epoch_in_future:
...         return ("PREVIEW_STALE", 0, 0, 0, 0)
...     if claimed and not posted:
...         return ("COMMIT_UNKNOWN", 0, 0, 0, 0)
...     g, i = going - opted_out_going, interested - opted_out_interested
...     if (g, i, checked) == (0, 0, 0):
...         return ("EMPTY", 0, 0, 0, 0)
...     if send_rejected:
...         return ("SEND_REJECTED", g, i, checked, 0)
...     return ("ALREADY_PUBLISHED" if posted else "PREVIEW", g, i, checked, posted)
>>> cases = {
...     "no_signups": (dict(going=0, interested=0), ("PREVIEW", 0, 0, 2, 0)),
...     "no_checkins": (dict(checked=0), ("PREVIEW", 3, 2, 0, 0)),
...     "empty": (dict(going=0, interested=0, checked=0), ("EMPTY", 0, 0, 0, 0)),
...     "opted_out": (dict(opted_out_going=1), ("PREVIEW", 2, 2, 2, 0)),
...     "opted_out_interested": (dict(opted_out_interested=1), ("PREVIEW", 3, 1, 2, 0)),
...     "left": ({}, ("PREVIEW", 3, 2, 2, 0)),
...     "labeled": (dict(labeled=True), ("LABELED_UNSUPPORTED", 0, 0, 0, 0)),
...     "canceled": (dict(status="canceled"), ("EVENT_CANCELED", 0, 0, 0, 0)),
...     "rescheduled": (dict(status="scheduled", started=False), ("NOT_ENDED", 0, 0, 0, 0)),
...     "not_started": (dict(status="active", started=False), ("NOT_ENDED", 0, 0, 0, 0)),
...     "unknown_status": (dict(status="unknown"), ("UNAVAILABLE", 0, 0, 0, 0)),
...     "age_boundary": (dict(age_s=30 * 86400), ("PREVIEW", 3, 2, 2, 0)),
...     "too_old": (dict(age_s=30 * 86400 + 1), ("EVENT_TOO_OLD", 0, 0, 0, 0)),
...     "denied": (dict(manager=False), ("DENIED", 0, 0, 0, 0)),
...     "foreign": (dict(same_guild=False), ("UNAVAILABLE", 0, 0, 0, 0)),
...     "duplicate": (dict(posted=1), ("ALREADY_PUBLISHED", 3, 2, 2, 1)),
...     "restart": (dict(claimed=True), ("COMMIT_UNKNOWN", 0, 0, 0, 0)),
...     "send_rejected": (dict(send_rejected=True), ("SEND_REJECTED", 3, 2, 2, 0)),
...     "stale": (dict(stale=True), ("PREVIEW_STALE", 0, 0, 0, 0)),
...     "version": (dict(version="v2"), ("PREVIEW_STALE", 0, 0, 0, 0)),
...     "epoch_advance": (dict(preview_age_s=5), ("PREVIEW", 3, 2, 2, 0)),
...     "preview_expired": (dict(preview_age_s=901), ("PREVIEW_STALE", 0, 0, 0, 0)),
... }
>>> for name, (kwargs, expected) in cases.items():
...     assert recap(**kwargs) == expected, name
>>> assert recap(epoch_in_future=True)[0] == "PREVIEW_STALE"
>>> # age_crossing: age is evaluated at preview and again at click
>>> assert recap(age_s=30 * 86400)[0] == "PREVIEW"
>>> assert recap(age_s=30 * 86400 + 1)[0] == "EVENT_TOO_OLD"
>>> # scheduled-but-started is also not ended; unknown fails closed either way
>>> assert recap(status="scheduled", started=True)[0] == "NOT_ENDED"
>>> assert recap(status="unknown", started=False)[0] == "UNAVAILABLE"
>>> # mention_title: markup tokens are removed whole, IDs included
>>> assert render_title("Finals <@123456789012345678> <@&5> <#9>") == "Finals"
>>> assert render_title("  a <@!7>b <  ") == "a b"
>>> assert render_title("<@1>") == "Event"
>>> # audit rows: a claim commits before the send; failed-insert + claim-delete is one txn
>>> class Store:
...     def __init__(self):
...         self.rows = {}
...     def click(self, who, interaction, send="ok", crash_in_cleanup=False):
...         if "posted" in self.rows:
...             return "ALREADY_PUBLISHED"
...         if "claim" in self.rows:
...             return "COMMIT_UNKNOWN"
...         self.rows["claim"] = who
...         if send == "rejected":
...             if crash_in_cleanup:
...                 return "CRASH"
...             self.rows["failed:" + interaction] = who
...             del self.rows["claim"]
...             return "SEND_REJECTED"
...         self.rows["posted"] = who
...         return "POSTED"
>>> # concurrent: the lock serializes; one click wins, the other sees the posted row
>>> s = Store()
>>> assert sorted([s.click("o1", "i1"), s.click("o2", "i2")]) == ["ALREADY_PUBLISHED", "POSTED"]
>>> s = Store()
>>> assert [s.click("o", "i1"), s.click("o", "i2")] == ["POSTED", "ALREADY_PUBLISHED"]
>>> # restart: a claim without a posted row is unknown for the claimant and for others
>>> s = Store()
>>> s.rows["claim"] = "o"
>>> assert [s.click("o", "i1"), s.click("o2", "i2")] == ["COMMIT_UNKNOWN", "COMMIT_UNKNOWN"]
>>> # send_rejected: claim released, own failed row per rejection, then one post
>>> s = Store()
>>> assert [s.click("o", "i1", "rejected"), s.click("o", "i2", "rejected")] == ["SEND_REJECTED"] * 2
>>> assert "claim" not in s.rows and sorted(s.rows) == ["failed:i1", "failed:i2"]
>>> assert [s.click("o", "i3"), s.click("o", "i4")] == ["POSTED", "ALREADY_PUBLISHED"]
>>> # interrupted_cleanup: the txn rolls back whole, so claim stays and no failed row exists
>>> s = Store()
>>> assert s.click("o", "i1", "rejected", crash_in_cleanup=True) == "CRASH"
>>> assert s.rows == {"claim": "o"}
>>> assert s.click("o", "i2") == "COMMIT_UNKNOWN"
>>> # custom_id_max: worst-case binding stays inside the 100-char component cap
>>> worst = "recap:v1:%s:%s:%s:%s:%s" % ("9" * 20, "9" * 20, "9" * 20, "9" * 10, "a" * 12)
>>> assert len(worst) == 95 <= 100, len(worst)
>>> extra = ["concurrent", "mention_title", "age_crossing", "interrupted_cleanup", "custom_id_max"]
>>> assert len(cases) + len(extra) == 27
>>> assert all(p <= 1 for _, (_, e) in cases.items() for p in e[4:])
>>> print(json.dumps({"schema": "events.recap.oracle.v1", "fixtures": 27, "status": "pass"}, sort_keys=True))
{"fixtures": 27, "schema": "events.recap.oracle.v1", "status": "pass"}

```
