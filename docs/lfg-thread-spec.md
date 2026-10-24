# LFG: one explicit discussion thread for one game post

Status: **proposed, design only**. Written 2026-10-24; review by 2026-10-31.
No commands, storage, migrations, permissions, sends or runtime tests are added.
Merge publishes a proposal, not permission to implement or activate it. This file
is the single repository contract; review requests bind its commit and SHA-256.

## 1. Decision and evidence

Consumer: a game-night organizer who wants one place for chatter about one post.
Hypothesis: a standard-named thread opened on demand, and tidied up when the post
ends, saves organizers a manual step without adding noise. No customer
validation is claimed.

[Apollo's event threads](https://apollo.fyi/blog/event-threads) create a thread
per event instance at reminder time or, optionally, at event creation. They reuse
an existing thread, can add attendees, archive on delete or conclusion, and "don't
delete threads". Organizers also ask for
[auto-threads per event](https://www.reddit.com/r/discordapp/comments/1mcnxv7/automate_new_thread_triggered_by_created_event/).
Read 2026-10-24. That is comparable capability and expressed interest, not TWO
demand. Apollo's attendee auto-join is a paid option; this proposal rejects it.

Discord's documented thread-from-message call ([Start Thread from
Message](https://docs.discord.com/developers/resources/channel#start-thread-from-message))
gives the created thread **the same ID as its source message**, so a message can
carry at most one thread. For TWO that makes the post's thread identity
`(guild_id, channel_id = parent, thread_id = lfg_posts.message_id)`: derived,
single-use and recoverable without a search.

| Option | Value and cost | Disposition |
| --- | --- | --- |
| A. No build (status quo) | Any member with Create Public Threads can already start a thread from the post message natively; no bot permission use, state or audit | Standing default; retain if the boundary below is not accepted |
| B. Link an existing thread | The thread ID equals the message ID, so the bot has nothing to *link*: Discord already shows the thread under the post. Saving a pointer adds state and a stale-link risk for no new capability | Not a distinct option; fold into A. The create flow below only *reports* a foreign thread, never adopts or edits it |
| C. Bot-created thread, explicit staff action, one per post | Standard name, preflight with a staff correction, lifecycle tidy-up, quota and audit; costs one side table, a command, a sweep step and a permission dependency | Smallest option that tests this hypothesis; recommend for **spec review**, not build |

Expected organizer benefit of C over A is **modest**: it saves a manual step for
organizers who lack Create Public Threads in that channel or want one naming
convention, and it archives stale chatter. Uncertainty is high: no TWO request
exists, and A already works for the common server default. If intent alignment
cannot show organizers who lack the native step or want consistency, the
recommendation is A (no build), not a smaller unaccepted grant.

Out of scope and not proposed: auto-creation on every post, adding any attendee
to a thread, private threads, voice-room linkage, cross-guild threads, a link to a
Discord scheduled event (LFG rows carry none, `0170_lfg.sql:10-21`), thread
messages written by the bot, new spend, or production activation.

## 2. Verified baseline and missing seams

Inspected repository head: `89b26ae050ffba9fb409622ffd54d31cdc79e7fc`.

- `lfg_posts` holds `id, guild_id, channel_id, message_id (nullable), title,
  starts_at, status open|closed, created_by, created_at, closed_at`
  (`crates/cutover/migrations/0170_lfg.sql:10-21`). `message_id` is set only after
  Discord accepts the post message (nonce recovery, `lfg_interactions.rs:295-342`);
  null means **unbound**. Titles are at most 100 characters
  (`crates/core/src/lfg.rs:36`), inside Discord's 1-100 thread-name range.
- There is **no thread code** for LFG: no REST call that starts a thread exists
  in `crates/` (the only `PublicThread` uses are the voice-ghost and backfill
  readers, `crates/cutover/src/voice_ghosts.rs:128`, `backfill_messages.rs:355`).
  `CREATE_PUBLIC_THREADS_BIT` (`1 << 35`) exists only to be *denied* by
  `/lockdown` (`crates/core/src/channel_moderation.rs:18-27`).
- Management is Manage Events at builder, router and domain layers; `/lfg-close`
  is the only post mutation after create (`docs/commands.md` `/lfg-close`,
  `lfg.rs:567-572`). Signup/move has a guild+actor admission window; leave and
  close are exempt ([port](lfg-port.md)).
- Lifecycle: the 60-second `lfg_sweep` closes posts 15 minutes past start
  (`LFG_EXPIRY_GRACE`), recovers or closes five-minute-old unbound rows
  (`LFG_ORPHAN_AGE`), handles at most 20 rows per pass (`LFG_SWEEP_MAX_ROWS`) and
  keeps the open-post cap at 20 (`lfg_interactions.rs:63-65`,
  [port](lfg-port.md)). Rows are never deleted by it. The one LFG row delete is
  failed-post cleanup before a message is accepted (`lfg_interactions.rs:354`), so
  a post with a thread is never removed that way.
- **Nothing observes a deleted post message.** `MessageDelete` is classified for
  audit (`crates/core/src/classify.rs:71`) but no LFG path reacts to it. A
  moderator deleting the message leaves an open row and, per Discord, can orphan
  its thread ("can be orphaned if that message is deleted"
  ([Threads](https://docs.discord.com/developers/topics/threads))).
- Audit seam: `rsvp_store::write_audit` into `announcements_audit_log`,
  **not atomic** with the domain mutation ([port](lfg-port.md)); the erasure plan
  already lists that table (`crates/cutover/src/member_erasure_plan.json:149`).
  Erasure deletes `lfg_posts` by `created_by` and signups by creator or member
  (`member_erasure_plan.json:19-29`); `lfg_roles`/`lfg_signups` also cascade from
  the post.
- [Privacy](privacy.md) already documents thread lifecycle rows (guild, thread,
  parent IDs, archived/locked state, no names or content) in the event store.
  A created thread would produce those rows under the **existing** retention.

Discord facts used (docs read 2026-10-24, not tested live):
auto-archive choices are 60, 1440, 4320 or 10080 minutes; a thread name is 1-100
characters; a message can have **one** thread; locking and changing slowmode
require `MANAGE_THREADS`; sending a message unarchives a thread unless it is
locked; the call fires Thread Create and Message Update gateway events. Whether
the bot may archive a thread it created without `MANAGE_THREADS` is **not
established** here; treat archive and lock as capability-gated (§4).

No live guild, permission, deployment, shared-database writer or host workflow
was inspected. If the seams below cannot stay small and coherent, stop at A
instead of weakening existing fences.

## 3. Trigger, name, parentage and member policy (specified C contract)

**Trigger: explicit organizer action only.** One new Manage Events command,
proposed as `/lfg-thread id:<post>`, in the same group and gates as `/lfg-close`
(configured guild, announcements feature, application, current human member, the
interaction's channel-resolved Manage Events bit). Never automatic: not on post
creation, not on first signup, not on a schedule, not from a scheduled event.
A new default-off setting gates the command; naming and publication count pins
are a build detail.

- **Two steps, dry-run first.** The command replies ephemerally with a preview
  and a single confirm button; it writes nothing, creates nothing and does not
  consume quota. The preview shows exactly: post ID and title, parent channel,
  the thread name, the archive duration, the permission preflight result and
  remaining quota. Confirm is bound to the preview revision (post `status`,
  `message_id`, channel and a quota generation) and expires after 5 minutes.
  A changed revision answers `STALE` and asks the organizer to run the command
  again. A component ID is not a capability; the actor comes only from the
  authenticated interaction and is re-gated at confirm.
- **Name:** the post title, verbatim, after the existing title validation (no
  prefix, so the 100-character bound always holds). Plain text; build fixtures
  must show a title containing `@everyone` or a link produces no mention.
- **Parentage:** exactly the post's own `channel_id`, which must be a guild text
  channel. Announcement, forum, media, voice and existing-thread parents are
  `UNSUPPORTED_CHANNEL`. The thread is public and created *from the post
  message* (`message_id`), never standalone. Unbound or unaccepted posts refuse
  with `NOT_POSTED`.
- **Auto-archive: 1440 minutes (24 hours).** It controls how long the thread
  stays listed after inactivity. Not configurable in v1; 60 would hide a
  pre-game chat mid-evening and 10080 keeps clutter. Proposed for host review.
- **Member-add policy: explicit join only.** The bot calls no add-thread-member
  endpoint, mentions nobody in the thread and posts nothing in it. Members find
  the thread under the post and join by opening or replying. There is no
  auto-add of signups, no consent disposition to infer, and signup is never
  consent to be added. The thread is created by the bot, so the bot is the only
  initial member.
- **No bot output into the thread and no post edit.** Discord already surfaces
  the thread beside the message, so the post is not re-rendered for it. The
  organizer's ephemeral reply carries the thread mention (channel mentions do
  not ping).
- **Reuse, never adopt:** if the message already has a thread, a read of channel
  `thread_id` decides. Owned by the bot and parented to the post's channel ->
  record it as `created` (recovery, §5). Owned by anyone else -> `THREAD_EXISTS`,
  report its mention, store nothing and never mutate it.

## 4. Lifecycle: close, delete, expire, orphan

States in `lfg_threads`: `pending -> created -> ended`, plus `orphaned`. One row per post at most; the post's `lfg_id` is the key.

| Event | Result |
| --- | --- |
| Post closed (`/lfg-close`) | No thread -> no row; `created` -> `ended`; tidy-up below |
| Post expired (sweep closes 15 min after start) | same transition inside the sweep pass |
| Post message deleted | Detected only when the sweep or a create attempt reads the message and gets "unknown message": `created` -> `orphaned` |
| Post row removed (erasure, guild purge) | Row cascades away; the Discord thread is left alone |
| Failed-post cleanup (`lfg_interactions.rs:354`) | No thread can exist (no accepted message), so no row |

**Tidy-up is capability-gated, never requested.** On end the bot attempts
nothing that needs a permission it does not already hold:

1. Always: stop counting the thread toward quota and mark `ended`. Discord's
   24-hour inactivity auto-archive then retires the listing. The bot never
   deletes a thread and never posts a closing message.
2. Only if the preflight shows the bot **already** effectively holds
   `MANAGE_THREADS` in the parent channel at that moment: archive and lock the
   thread (a locked thread cannot be reopened by a plain message). Locking needs
   that permission per Discord; the bot's role is **not** to be granted it for
   this feature. If the call fails, mark `ended` anyway; the audit fact records
   `archive_skipped`.

A closed or ended post therefore may leave an unlocked, auto-archiving thread
that a member can wake with a message. That is acceptable and disclosed in the
preview. If hosts require guaranteed lock, the build is blocked until a separate
boundary decision on `MANAGE_THREADS`; this document requests none.

**Orphans.** `orphaned` means the post message is gone. The bot does nothing to
the thread, stops counting it, writes one audit fact and tells staff on the next
`/lfg-thread` for that post (`POST_DELETED`). Staff can delete a stray thread in
Discord. No bot cleanup of Discord objects.

## 5. Serialization, replay, restart and version behavior

- Reuse the existing per-post advisory lock `lfg:{guild}:{post}` and the
  runtime lock used for refresh. A create is: gate, lock, re-read post and
  quota, **persist `pending`**, release no lock, one Discord call, then mark
  `created` and audit. No HTTP inside a database transaction that holds more
  than the per-post lock; the call is a single bounded request.
- **Outcome uncertainty** (timeout, process death after the call): the state is
  `pending` or a missing audit row, and the next create, the sweep or the status
  read resolves it by one read of channel `thread_id`: exists and owned by the
  bot under the post's channel -> `created`; unknown channel -> delete the
  `pending` row. Never re-issue the create on uncertainty. `COMMIT_UNKNOWN`
  tells staff to inspect the post, not retry blindly.
- `pending` older than 5 minutes is resolved by the sweep; at most 20 rows per
  pass shared with the existing LFG limit, no new scheduler.
- **Restart:** state is the table; nothing is in memory. A restored or replayed
  component for a stale revision answers `STALE`. Interaction replay of the same
  authenticated interaction ID returns the stored outcome with no second call.
- **Version skew:** the table is additive and cascading from `lfg_posts`. A
  binary without the feature ignores it; threads created earlier stay and
  auto-archive. Rolling back leaves the table in place; no destructive rollback.
  Disabling the setting stops preview and create but does not delete rows or
  threads, and the sweep's `ended`/`orphaned` bookkeeping continues without
  Discord writes.
- Proposed storage (build detail, not created here): `lfg_threads(lfg_id TEXT
  PRIMARY KEY REFERENCES lfg_posts(id) ON DELETE CASCADE, thread_id TEXT NOT
  NULL, state TEXT NOT NULL CHECK (state IN ('pending','created','ended',
  'orphaned')), created_at TIMESTAMPTZ NOT NULL, ended_at TIMESTAMPTZ)`. It
  holds **no member ID, name or message text**; the actor appears only in the
  deletable audit row.

## 6. Permission preflight, isolation, quotas, retention and audit

**Permission preflight (fail closed).** Before the preview offers a confirm, and
again at confirm, the bot computes its own effective permission in the parent
channel from fresh role and overwrite reads: `VIEW_CHANNEL` and
`CREATE_PUBLIC_THREADS` are required. It does **not** require
`SEND_MESSAGES_IN_THREADS` (it posts nothing in the thread) or
`MANAGE_THREADS` (optional, §4). Missing or unreadable evidence refuses with
`PERMISSION_MISSING` or `UNAVAILABLE`, never a guess. The staff correction is a
fixed sentence naming the channel and the one permission to grant to the bot's
role there (including the case where `/lockdown` denies it), plus "no thread was
created". The bot never requests, changes or asks for any permission. A 403 at
commit despite a passing preflight is the same `PERMISSION_MISSING` with no
retry loop and the `pending` row removed.

**Guild isolation.** Every read and write is keyed by the interaction's guild;
the post is looked up through the guild-fenced getter, so a foreign guild or a
foreign post ID answers `UNAVAILABLE` with no identifiers. The thread's guild and
parent must match the stored post. No cross-guild thread or lookup.

**Quotas (proposed, for host review).** At most one thread per post; at most 10
`created` threads per guild; at most 5 new threads per guild per hour; the
existing guild+actor admission window and one-in-flight LFG queue apply. Caps
refuse new creates only (`CAP_REACHED`); reads, end and orphan bookkeeping,
erasure and refusal are exempt. `ended` and `orphaned` rows do not count. Discord
also has its own active-thread limit whose number this document does not assert;
a Discord limit error maps to `DISCORD_LIMIT` (retryable later, no loop).

**Retention and erasure.** The side table holds no personal data and lives only
with its post, so organizer erasure (which deletes the post) and guild purge
cascade it. The audit row (actor, post, thread, outcome, no title) must be
deleted for the erased actor on both erasure executors, with no accountability
exemption, and be named in the manifest like other announcement audit rows. The
thread's messages, members and name are Discord-side content: erasure and backup
claims do not cover them. Event-store thread rows keep their existing retention
and member-ID erasure. Restored snapshots may contain `lfg_threads` rows for a
purged post; restore must drop rows whose post is absent.

**Audit facts** (one `announcements_audit_log` row each, no title, name or
message text): `lfg_thread_preview` is **not** audited (it writes nothing);
`lfg_thread_create`, `lfg_thread_end`, `lfg_thread_orphan`, with guild, actor
(null for sweep), post ID, thread ID, constant reason and outcome. Because the
audit seam is not atomic, an audit failure reports failure and the pending-row
recovery above reconciles; this proposal does not claim atomic audit.

**CISO proposal assessment:** acceptable to circulate for intent review with the
above controls; **not** a grant, independent approval of our own boundary
request, or runtime security acceptance. Blocking findings before any later
build/enable: (1) accept or narrow the thread-lifecycle boundary: a bot-created
public thread outlives the post by design and archive/lock is capability-gated,
so guaranteed lock is out unless `MANAGE_THREADS` is separately decided;
(2) accept the permission use of `CREATE_PUBLIC_THREADS` through preflight only,
with no grant request; (3) accept audit-actor deletion on erasure and the
no-member-data side table; (4) prove a build cannot create without the explicit
confirm (a code-path test, not a flag). If any of these needs automatic creation,
member auto-add, private threads or broader authority, stop this proposal. No
credential or permission is requested by this document. Later security-sensitive
changes still require independent applicable security disposition and exact-head
tests.

## 7. Private UX, corrections and agent-facing result

All replies are ephemeral and mention-suppressed; no color- or emoji-only state.

- Preview: "Create a thread from post `<id>` in `<#channel>` named `<title>`,
  archiving after 24 hours of inactivity? Nobody is added or pinged. If the post
  ends the thread stays; Discord will archive it. [Create thread]".
- Success: "Thread created: `<#thread>`. Nobody was added."
- Failures use one fixed correction line each, never a raw SQL/Discord message:
  grant the bot Create Public Threads in `<#channel>`; use a text channel post;
  the post is closed or its message is gone (the preview says whether the
  thread already existed); a thread already exists (mention it); the guild
  thread cap is reached (end a post or wait); try again later.

No new executable CLI/API is registered. Future adapters must discover this file
through the [documentation map](README.md); these are **proposed typed results**,
not commands to run on a guild. Stable envelope: `schema=lfg.thread.v1`,
`guild_id`, `post_id`, `thread_id` (equals the post's `message_id`, null while
unbound), `mode` (`preview|confirm`), `dry_run`, `state`
(`none|pending|created|ended|orphaned`), `preview` (`name`, `parent_channel_id`,
`auto_archive_minutes`, `permission` `ok|missing|unknown`, `live_threads`,
`cap`), `reason`, `retryable`, `next_action`, optional `retry_after_seconds`
bounded 1..300. Denied replies omit identity and preview fields. Previews carry
no mutation handle.

| reason | retryable / next_action |
| --- | --- |
| `PREVIEW` | false / `confirm_same_revision` |
| `CREATED`, `UNCHANGED` | false / `none` |
| `PERMISSION_MISSING` | false / `grant_create_public_threads` |
| `NOT_POSTED`, `UNSUPPORTED_CHANNEL` | false / `use_supported_post` |
| `POST_CLOSED`, `POST_DELETED` | false / `create_new_post` |
| `THREAD_EXISTS` | false / `open_existing_thread` |
| `CAP_REACHED` | false / `end_a_post_or_wait` |
| `FORBIDDEN` | false / `ask_authorized_staff` |
| `UNAVAILABLE` | false / `verify_access` (collapse missing/foreign/denied) |
| `STALE` | false / `rerun_preview` |
| `BUSY`, `DISCORD_LIMIT`, `STORE_UNAVAILABLE` | true / `retry_after_backoff` |
| `COMMIT_UNKNOWN` | true / `inspect_post_thread` |

This design's offline check has no environment selector, credentials, network,
database, Discord client, side effects or wrong-flag production path. No
staging/live pilot, thread creation or permission change is authorized. Future
execution cannot be reached by combining flags on this check.

## 8. Offline fixture oracle and runnable check

The following deterministic **fixture oracle**, not an implementation, uses
guild `g1`, post `p1`, message/thread id `m1`, channel `c1`, organizer `o`,
outsider `x`. Each row resets that fixture. Columns: `writes` is Discord write
calls (create/archive), `adds` is add-thread-member calls, `rows` is resulting
`lfg_threads` rows for `p1`. Every row requires `adds = 0`, zero mentions, zero
bot messages in the thread and unchanged signups/roles.

| Case | Controlled event / expected assertion |
| --- | --- |
| preview | Staff previews a healthy open post -> PREVIEW, 0 writes, 0 rows |
| create | Confirm same revision -> CREATED, 1 write, state created |
| no_authority | `x` without Manage Events -> FORBIDDEN, 0 writes |
| missing_permission | Bot lacks Create Public Threads -> PERMISSION_MISSING, correction, 0 writes |
| permission_race | Preflight ok then Discord 403 -> PERMISSION_MISSING, pending row removed, no retry |
| unreadable_perms | Overwrite read fails -> UNAVAILABLE, never a guess |
| unbound_post | `message_id` null -> NOT_POSTED, 0 writes |
| bad_channel | Parent is announcement/forum -> UNSUPPORTED_CHANNEL, 0 writes |
| post_closed_no_thread | Post closed by `/lfg-close` with no thread -> POST_CLOSED, no create, 0 rows |
| post_closed_with_thread | Post closed by `/lfg-close` with a `created` thread -> POST_CLOSED, no create; thread `created` -> `ended`, 1 row |
| post_expired | Sweep closes at start + 15 min -> thread ended in the same pass |
| post_deleted | Message gone (unknown message) -> POST_DELETED, state orphaned, no Discord write |
| foreign_guild | Guild `g2` -> UNAVAILABLE, 0 writes, no identifiers |
| foreign_post | Foreign post ID -> UNAVAILABLE, 0 writes |
| thread_exists_ours | Thread owned by bot, correct parent, no row -> UNCHANGED, row created, 0 writes |
| thread_exists_foreign | Thread owned by another member -> THREAD_EXISTS, 0 rows, 0 writes |
| double_confirm | Same interaction replayed -> stored CREATED, 1 write total |
| stale_confirm | Post closed between preview and confirm -> STALE, 0 writes |
| cap_guild | 10 created threads in guild -> CAP_REACHED, 0 writes |
| cap_hourly | 5 creates in the hour -> CAP_REACHED, 0 writes |
| orphan_not_counted | Orphaned and ended rows do not count toward the cap -> CREATED |
| restart_pending_present | Restart with pending row, thread exists and owned by bot -> state created, no second create |
| restart_pending_absent | Restart with pending row, unknown channel -> row removed, no thread |
| restart_ended | Restart after end -> no duplicate end, no Discord write |
| old_binary | Binary without the feature, row present -> ignored, thread untouched |
| optional_archive | Ended with Manage Threads already held -> archive + lock, 2 writes max; without it -> ended with `archive_skipped`, 0 writes |
| store_outage | Store failure before persist -> STORE_UNAVAILABLE, 0 writes |
| erasure | Erase organizer -> post, thread row and audit rows for the actor gone; Discord thread untouched |

Runnable offline document check (Python stdlib; from repo root):

```sh
python3 -m doctest -v docs/lfg-thread-spec.md
python3 scripts/check_docs_index.py --json
```

The doctest checks the **specified oracle's consistency** (matrix parity,
invariants, Discord constants), not real SQL locks, authentication, permission
resolution, rendering, erasure, outage handling or deployed behavior. Future
protected acceptance tests must implement barriers, injected clocks and
store/REST faults against actual callers in disposable fixtures; zero tests is
not proof. No acceptance/implementation task is created by this proposal.

```pycon
>>> import json, pathlib
>>> # (case, reason, writes, adds, rows); abstract expected snapshots
>>> oracle = [
...     ("preview", "PREVIEW", 0, 0, 0),
...     ("create", "CREATED", 1, 0, 1),
...     ("no_authority", "FORBIDDEN", 0, 0, 0),
...     ("missing_permission", "PERMISSION_MISSING", 0, 0, 0),
...     ("permission_race", "PERMISSION_MISSING", 1, 0, 0),
...     ("unreadable_perms", "UNAVAILABLE", 0, 0, 0),
...     ("unbound_post", "NOT_POSTED", 0, 0, 0),
...     ("bad_channel", "UNSUPPORTED_CHANNEL", 0, 0, 0),
...     ("post_closed_no_thread", "POST_CLOSED", 0, 0, 0),
...     ("post_closed_with_thread", "POST_CLOSED", 0, 0, 1),
...     ("post_expired", "POST_CLOSED", 0, 0, 1),
...     ("post_deleted", "POST_DELETED", 0, 0, 1),
...     ("foreign_guild", "UNAVAILABLE", 0, 0, 0),
...     ("foreign_post", "UNAVAILABLE", 0, 0, 0),
...     ("thread_exists_ours", "UNCHANGED", 0, 0, 1),
...     ("thread_exists_foreign", "THREAD_EXISTS", 0, 0, 0),
...     ("double_confirm", "CREATED", 1, 0, 1),
...     ("stale_confirm", "STALE", 0, 0, 0),
...     ("cap_guild", "CAP_REACHED", 0, 0, 0),
...     ("cap_hourly", "CAP_REACHED", 0, 0, 0),
...     ("orphan_not_counted", "CREATED", 1, 0, 1),
...     ("restart_pending_present", "UNCHANGED", 0, 0, 1),
...     ("restart_pending_absent", "UNCHANGED", 0, 0, 0),
...     ("restart_ended", "UNCHANGED", 0, 0, 1),
...     ("old_binary", "UNCHANGED", 0, 0, 1),
...     ("optional_archive", "POST_CLOSED", 2, 0, 1),
...     ("store_outage", "STORE_UNAVAILABLE", 0, 0, 0),
...     ("erasure", "UNAVAILABLE", 0, 0, 0),
... ]
>>> _lines = pathlib.Path("docs/lfg-thread-spec.md").read_text().splitlines()
>>> _start = next(i for i, l in enumerate(_lines) if l.startswith("| Case |"))
>>> _matrix = []
>>> for _l in _lines[_start + 2:]:
...     if not _l.startswith("| "):
...         break
...     _matrix.append(_l.split("|")[1].strip())
>>> assert set(_matrix) == {row[0] for row in oracle}, set(_matrix) ^ {row[0] for row in oracle}
>>> assert len({row[0] for row in oracle}) == len(oracle) == len(_matrix) == 28
>>> assert all(adds == 0 for _, _, _, adds, _ in oracle)
>>> assert all(rows in (0, 1) and writes <= 2 for _, _, writes, _, rows in oracle)
>>> # Only a confirm that reaches Discord, or the optional archive pair, writes
>>> assert {n for n, _, w, _, _ in oracle if w} == {"create", "permission_race", "double_confirm", "orphan_not_counted", "optional_archive"}
>>> assert all(r in ("CREATED", "PERMISSION_MISSING") for n, r, w, _, _ in oracle if w == 1)
>>> assert dict((n, w) for n, _, w, _, _ in oracle)["optional_archive"] == 2
>>> assert dict((n, r) for n, r, _, _, _ in oracle)["thread_exists_foreign"] == "THREAD_EXISTS"
>>> assert all(row[1:] == ("UNAVAILABLE", 0, 0, 0) for row in oracle if row[0].startswith("foreign_"))
>>> assert all(row[2:] == (0, 0, 0) for row in oracle if row[1] in ("FORBIDDEN", "CAP_REACHED", "STALE", "NOT_POSTED"))
>>> # §4 close transition: no thread persists nothing, created persists one ended row
>>> assert dict((n, rows) for n, _, _, _, rows in oracle)["post_closed_no_thread"] == 0
>>> assert dict((n, rows) for n, _, _, _, rows in oracle)["post_closed_with_thread"] == 1
>>> assert dict((n, rows) for n, _, _, _, rows in oracle)["post_expired"] == 1
>>> # Discord constants: one thread per message, id == message id, archive choices
>>> archive_choices, default_archive = (60, 1440, 4320, 10080), 1440
>>> assert default_archive in archive_choices
>>> thread_id = message_id = "m1"
>>> assert thread_id == message_id
>>> assert 1 <= len("x" * 100) <= 100  # LFG title cap fits the thread-name range
>>> assert 10 <= 20 and 5 < 10  # guild thread cap within the open-post cap; hourly < live cap
>>> print(json.dumps({"schema": "lfg.thread.oracle.v1", "fixtures": len(oracle), "status": "pass"}, sort_keys=True))
{"fixtures": 28, "schema": "lfg.thread.oracle.v1", "status": "pass"}

```

## 9. Acceptance boundary, review metric and undo

Record CEO spec acceptance **and** host intent alignment against the exact
revision before any implementation subtasks, protected test handoff or live
pilot. Route the decision brief through the President/COO to the CEO and the
existing host intent path; a queue/intake receipt is not alignment. The CISO
findings in §6 must receive an independent boundary disposition, not a
self-approved grant. No request should be sent directly to the owner or an
invented host executor.

The decision must accept or revise: A-versus-C recommendation and demand
evidence; explicit-confirm trigger only; derived thread ID equals message ID;
24-hour archive; explicit-join-only member policy; capability-gated archive/lock
with no `MANAGE_THREADS` request; Create Public Threads through preflight only;
proposed caps (10 live, 5 per hour); no-member-data side table; audit-actor
deletion on erasure; and orphan handling by bookkeeping, not deletion. On any
material revision, bind a fresh review request and alignment to that revision.

By 2026-10-31, metric is **all 28 fixture-matrix rows have agreed or explicitly
rejected outcomes, the four blocking findings have written disposition, and the
A-vs-C choice is recorded with demand evidence or its absence**. This measures
design completeness, not engagement. Keep the proposal only if the fixtures show
a bounded lifecycle with no unsolicited adds and no new spend; otherwise
recommend no-build (A). Default while unresolved is A; no timeout
auto-approval. A future demand experiment requires its own hypothesis, metric
and date, not inferred success from this document. No live experiment is run.

ACT: publish this proposed contract and its offline oracle for review. WHY:
bound the thread lifecycle and permission use before considering a thread
feature. UNDO: reviewed docs revert; there is no operational change to undo. A
later independently accepted build must keep admission dark until its own
tests/security/release gates pass; disabling future admission must preserve
refusal, end/orphan bookkeeping and erasure, not delete threads or history. No
production tests, migrations, credentials or activation here.
