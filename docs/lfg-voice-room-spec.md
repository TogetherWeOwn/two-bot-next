# LFG: one explicit per-session voice room

Status: **proposed, design only**. Written 2026-10-24; review by 2026-10-31.
No commands, storage, migrations, permissions, sends, channel creation or runtime
tests are added. Merge publishes a proposal, not permission to implement or
activate it. This file is the single repository contract; review requests bind its
commit and SHA-256.

## 1. Decision and evidence

Consumer: a game-night organizer and the members who signed up on one existing
LFG post. Hypothesis: one organizer action replaces "make a room, set it up, post
the link, delete it later", and signed-up members reach the room from the post.
No customer validation is claimed.

[Teamplay's LFG bot](https://teamplay.gg/discord-lfg-bot) says "A temporary voice
room spins up for each session and cleans itself up afterwards. Your squad gets a
'Join voice' button right on the card" and lists 270+ servers (read 2026-10-24).
[GameTree's LFG docs](https://gametree.me/docs/lfg-bot-introduction/) were read the
same day; the page fetched shows no voice behavior, so it is cited for managed
sessions only. Both are vendor capability claims of a comparable flow, not
evidence of TWO demand or of those bots' real permission and cleanup behavior.

| Option | Value and cost | Disposition |
| --- | --- | --- |
| A. No build (manual rooms) | No new permission or lifecycle. Organizers can already open a room today by joining a voice creator channel, but nothing ties it to the post or its end | Safe default until acceptance; keep if the boundary below is not accepted |
| B. Link an existing room | Organizer attaches a voice channel the guild already owns; the post shows its link. No channel create/delete, no Manage Channels use, no lifecycle. Does not remove manual room setup or cleanup | Smallest build; **fallback** if C's boundary is not accepted |
| C. Bot-managed room per post | One explicit organizer action creates one room in a configured category, links it, and winds it down after the post ends. Removes setup and cleanup; adds a channel create/edit/delete lifecycle and a Manage Channels dependency | **Recommend for spec review, not build** |

Recommendation: C, narrowly. It is the only option that tests the whole
hypothesis (B leaves room setup and cleanup manual), and the narrow form keeps the
risk bounded: explicit trigger only, no member moves, no new Discord permission,
no per-member data, caps, and no-build as the default. Expected organizer benefit:
about four manual steps (create, configure, share, delete) become two clicks
(preview, confirm) plus automatic wind-down. Uncertainty is high: current voice
creator channels already give organizers a room, so the real delta is the linked
post and tied cleanup, which may not justify a new lifecycle. If the CEO or host
prefers the smaller change, adopt B and drop §4-§5 channel lifecycle entirely.

No automatic creation on post, no auto-move, no DM or mention of signups, no
cross-guild room, no recording or transcription, no stage channel, no per-member
channel overwrites, no new spend. Signing up grants no access. A room never
changes a signup, role slot or RSVP.

## 2. Verified baseline and missing seams

Inspected repository head: `89b26ae050ffba9fb409622ffd54d31cdc79e7fc`.

- `crates/core/src/lfg.rs:277-281,314-326` has the post (`id`, guild, channel,
  accepted `message_id`, `title`, `starts_at`, Open/Closed, `created_by`) with
  **no voice field**. Titles are organizer text up to 100 chars (`:36`).
- `crates/core/src/router.rs:666-675` gates `/lfg` and `/lfg-close` behind the
  Announcements gate and Manage Events; signup is an ordinary member action.
- `crates/discord/src/lfg_interactions.rs:63,507-523,577` closes open posts 15
  minutes after `starts_at` (`LFG_EXPIRY_GRACE`), at most 20 rows per 60 s pass,
  retaining posts and signups. `crates/core/src/lfg_store.rs:60-90` and
  `automation_quota.rs:16` cap open posts at 20/guild. `lfg_store.rs:225`
  (`delete_lfg`) hard-deletes a post (failed-post cleanup, erasure plan
  `crates/cutover/src/member_erasure_plan.json:19-32`).
- [Voice rooms](voice-rooms.md) and [delete guards](voice-delete-guards.md) own
  temporary rooms through `voice_rooms` (`0224_voice_rooms.sql`), tracked
  provenance, 40 rooms/guild, a 60 s continuous human-empty grace, protected IDs
  and "a channel the bot never tracked is never deleted". `voice_rooms` is keyed to
  a creator channel and an owner and carries owner commands (`/transfer`,
  `/private`); reusing it would give an LFG room owner semantics nobody asked for.
- `crates/bot/src/event_voice_category.rs:1-25,40-45` is the nearest precedent:
  a **required** configured category with no default or auto-pick, a per-tick
  Manage Channels + View Channel preflight, `perm_denied` when it fails, one staff
  alert per episode. `0455_event_voice_channels.sql` claims a unique row before any
  Discord call and marks a vanished channel `orphaned`; its reconcile never
  deletes. [Creation receipts](voice-room-creation-plan.md) treat a bodyless or
  contradictory 2xx as `UnknownOutcome`: no repost, reservation held.
- [Permission health](voice-permission-health-core.md) already resolves effective
  permissions per level (guild, category overwrite, channel overwrite).
- [Voice rooms](voice-rooms.md) lists bot permissions View Channel, Connect,
  Manage Channels, Move Members, Manage Roles; limits are 100-char names, 50
  channels/category, user limit 0-99. Rooms created without Manage Roles send no
  overwrites and stay category-synced.
- No link between an LFG post and any voice channel exists. The LFG
  [interaction contract](lfg-interaction-contract.md) offers no atomic
  mutation-plus-audit inbox; audit is a separate append, not a replay guard.

No live guild, permission, deployment, shared-database writer or host workflow was
inspected. Sibling designs ([ready check](lfg-ready-check-spec.md), [backup
host](lfg-backup-host-spec.md)) are adjacent and not prerequisites. If the room
binding cannot be made small, serialized with close/delete/erasure, and
independent of the owner-semantics of `voice_rooms`, stop this proposal.

## 3. Trigger, authority, naming and join policy

**Trigger.** Only an explicit organizer action. Creating, editing, signing up to or
reopening a post never creates a room. "Organizer" is a current human guild member
with fresh **Manage Events** and access to the post's bound channel (the same
authority as `/lfg-close`); creator identity alone grants nothing and no new role,
delegation or permission is proposed. Authority is rechecked at preview and
confirm; unknown membership or permissions fails closed.

**Two steps.** A private (ephemeral) **preview** writes nothing and reserves
nothing; **confirm** revalidates everything against the same post revision. Proposed
controls are not callable commands today. Preview shows: exact post, final room
name, category, user limit, the create window, remaining guild room capacity, "nobody
is moved, mentioned or messaged", how the room ends, and how to end it early.

**Eligibility** (all must hold at confirm): post open and bound to its accepted
message; `now` in `[starts_at - 60 min, starts_at + 15 min)` by DB UTC time (the
upper bound is the existing expiry grace, the lower bound keeps rooms for
far-future posts from holding capacity); no existing binding for the post; guild
below the live-room cap; preflight (§5) passes; feature, guild and admin
admission pass. At most **one room per post lifetime**: a spent binding is never
re-minted by edit, reopen, restart, replay or flag toggle. A second session needs a
new post.

**Room.** One guild voice channel (never stage) created once with a single create
request that includes all settings, in the configured category:

- Name: `LFG · <title>` with the title run through the existing
  [name filter](voice-name-filter-core.md), truncated to 48 chars, plus a short
  suffix of 4 hex digits derived from the post id so a recovered create can be
  matched. A rejected title falls back to `LFG session · <suffix>`. Never renamed
  later; a post edit changes nothing on the channel.
- Parent: the configured category only. Never the post's channel category, never
  inferred. No overwrites are sent, so the room is category-synced and the
  organizer's staff controls the audience. No @everyone or per-member deny.
- User limit: default `min(sum of role slots, 99)`, floored at 1; preview may
  set 0-99 where 0 means unlimited (Discord semantics). Confirm persists the
  preview value unchanged; an explicit 0 is stored as 0, never normalized.
- No bitrate, region or topic changes beyond Discord defaults.

**Join policy.** Explicit join only. After confirm the existing post gets one quiet
edit adding a voice line with a channel mention, mention-suppressed and with no
ping; members click the link. The bot never moves, invites, DMs or mentions anyone,
never joins the channel, and never needs Move Members. Signup, leave and close do
not move anyone. A member who cannot see the category cannot see the link.

## 4. Lifecycle, serialization and cleanup

States: `creating` (claimed, Discord call possibly in flight), `active`,
`winding_down`, `locked`, `deleted`, `gone` (channel vanished), `held` (unknown
evidence or ambiguous). Free-text state column with no CHECK, like the event voice
table, so a later or earlier binary does not fail on an unknown value; an unknown
value is treated as **held**: nothing is deleted or edited.

**Create.** Under a per-post lock: re-read post, insert a unique
`(guild_id, post_id)` claim in `creating` **before** the Discord call, make one
create request (never retried after an uncertain result), validate the receipt
(nonzero id, same guild, voice type, exact parent), then bind `active` and edit
the post. If the post edit fails, the binding stays `active` (room live and
counted); the one-minute sweep re-attempts the edit until it succeeds or the post
closes, with a staff-visible link-pending flag, and the confirm reply uses the
link-pending copy (§6), never asserting the link was added. A concurrent second
confirm returns the same room. A bodyless, malformed or contradictory 2xx or a
timeout leaves the claim `creating`/`held`; the room worker may **adopt** only when
the live snapshot shows exactly one channel in the configured category with the
reserved exact name created after the claim instant. Zero or several candidates
stay `held`, never repost and never delete; staff are told to check the category.
No lock is held across HTTP. A close (explicit, expiry or delete) arriving while
`creating` is outstanding records a pending-close on the claim; the in-flight
Discord request is not cancelled. When the response arrives, a validated channel
is bound then immediately moved through the close path for that event; an unknown
outcome stays `held` with the close still pending, and adopt-or-hold applies the
close after adoption.

**Wind-down** is driven by post state, never by channel guesses:

| Event | Effect |
| --- | --- |
| Post open, room never occupied or emptied | Room is kept. The ordinary 60 s empty grace does not apply while the post is open (max 75 minutes by the window) |
| Explicit `/lfg-close` | Room `locked`: user limit set to the current human headcount (at least 1), never a permission deny, never a disconnect. Deleted after the empty grace |
| Expiry sweep (start + 15 min) | Room `winding_down`: **no lock** (sessions run past start, late joiners are welcome). Deleted after the empty grace |
| Post hard-deleted (failed-post cleanup, erasure) | Treated as explicit close: lock, then delete after the empty grace |
| `starts_at + 6 h` with humans still present | Lock once so it is not joinable forever; still never disconnected or deleted while a human is present; listed to staff as overdue |
| Staff deleted the channel by hand | Row marked `gone`; no Discord delete; slot stays spent |
| Organizer "end room early" | Same as explicit close, requires the same Manage Events recheck |

Deletes reuse the [delete guards](voice-delete-guards.md): tracked provenance
read again before the write, protected IDs (the configured category and every
creator, lobby and generator channel) refused, human occupancy and the configured
empty grace re-checked at send time, unknown bot identity counts as human,
cancellation on a human join. A channel without a binding row is **never** touched,
even if its name matches. No bulk sweep of the category.

**Orphans.** (a) Binding to a 404 channel -> `gone`. (b) Channel alive, post
closed/expired/missing -> wind-down above. (c) Claim with no channel -> adopt or
`held`, as above. (d) Channel in the category with no row -> ignored, reported only
as a metadata-free count to staff. Cleanup of (a)-(c) runs from the existing
one-minute LFG sweep, at most 10 rows per pass, oldest first, and precedes new
creates after a restart. Sweep work for the room never blocks the post-close sweep.

**Restart and version.** All state is durable before each Discord write; every step
is idempotent (delete of a missing channel is success). Restart resumes `creating`
older than 5 minutes only through adopt-or-hold, never a second create. The cleanup
path stays enabled when the admission flag is off: turning the feature off refuses
new creates and keeps lock, delete, `gone` marking and erasure. A rolled-back
binary ignores the table and leaves rooms standing; rolling forward resumes them.

**Serialization.** The binding shares the per-post runtime lock with signup,
leave, close and refresh. It has no FK to `lfg_posts`, mirroring `voice_rooms`
("rooms outlive their config"), so post deletion can never strand a room. The
audit append is separate and non-atomic; the binding row is the authority, audit is
a record, and a failed audit never rolls back or fakes a success.

## 5. Permissions, isolation, bounds and retention

**Gates, all required, all dark by default:** Announcements (LFG), the voice
runtime gate (`TWO_VOICE`), a proposed new admission flag (default off), the
configured guild, and a **required, proposed** category setting with no default or
auto-pick (`TWO_LFG_VOICE_CATEGORY_ID` is a placeholder name; the event-voice key
is not reused). Missing, blank, non-canonical, zero, foreign-guild or non-category
values fail closed to `NO_CATEGORY`.

**Preflight (fail closed, every confirm and every wind-down write).** Resolve the
bot's *effective* permissions on the configured category (guild base, then
category overwrites, per [permission health](voice-permission-health-core.md)):
**View Channel and Manage Channels** are required to create, edit the user limit
and delete a voice channel. **Connect, Move Members, Manage Roles and Send Messages
are not needed or requested**; the card's "voice perms" are met by the voice
channel create/edit/delete path itself. If an effective bit is missing or the
evidence is unknown, nothing is written: the reply tells staff exactly which bit
and which category (for example "Ask a server admin to grant the bot Manage
Channels and View Channel on <category>"). The bot never grants itself anything.
A failed edit/delete at wind-down (403) stops that room's writes without retry
storms, leaves it `held`, and raises one metadata-free staff notice per episode
using the existing throttle (3 sends, 5 then 30 minute backoff).

**Isolation.** Every key is `(guild_id, post_id)`. The post, the accepted message,
the category and the created channel must all belong to the interaction's
configured guild; the actor comes only from the authenticated interaction, never
from caller-supplied IDs, titles or links. Foreign or unreadable evidence collapses
to one generic `UNAVAILABLE`.

**Bounds** (tighten, never replace, existing limits): one room/post lifetime; **5
live rooms/guild** (below the 20-open-post cap), where live means a binding in
`creating`, `active`, `winding_down`, `locked` or `held` (`deleted` and `gone` are
terminal and do not count, for both admission and recovery); 20 creates per guild
per UTC day (refused as `CAP_DAILY`);
one confirm per actor per 60 s (replay and refusal exempt); one in-flight
execution through the existing LFG queue/pool bounds; creates, edits and deletes go
through the per-guild voice queue with retry-after honored, one write at a time.
At the cap, refuse **new** creates only; lock, delete and `gone` marking always
proceed. Category full (Discord's 50/category) is `CATEGORY_FULL`. Estimated
incremental spend is **$0**; this is a sizing target, not measured headroom.

**Retention and erasure.** The binding stores `guild_id`, `post_id`, `channel_id`,
`state`, timestamps and the minimal reserved-name token, **no member identity, no
title, no occupancy, no join history**. Spent rows live as long as their post row.
Organizer and actor IDs appear only in audit rows, which must be deleted by member
erasure in both executors, per the existing audit coverage (to be re-verified at
build, not asserted here). A post erased with its creator ends the room through the
close path. Erasure of a non-creator leaves the room lifecycle unchanged; only
audit identity is removed. A new table is a schema tripwire: it must be declared in the erasure
plan as having no member columns, in the backup table lists, in the role plan and
in rollback notes. Restored snapshots can resurrect rows whose channel is gone;
the first 404 marks them `gone`. No claim is made about Discord-side data.

**Audit facts** (existing announcements audit seam; IDs, no free text):
`lfg.voice.preview`, `refused` (with reason), `created`, `bound`, `locked`,
`deleted`, `gone`, `held`, each with `guild_id`, `post_id`, `channel_id`, reason
code, actor ID for organizer actions, and the preflight result. Never the title,
member list, occupancy or permission integers.

| Threat / data | Proposed control and remaining finding |
| --- | --- |
| New channel create/edit/delete authority | Tracked provenance only, delete guards, category-scoped preflight; the bot already holds Manage Channels for voice rooms, but this is a **new use** of it keyed to LFG state, needing disposition |
| Unsolicited moves or contact | None: no Move Members, no DM, no mention, no auto-join; link click only |
| Wrong-channel delete | Never delete an untracked or protected channel; held on any ambiguity; no category sweep |
| Staff-ignored room that stays forever | Never forced out; lock at start + 6 h, listed to staff, cap of 5 bounds the damage |
| Organizer spam / cap exhaustion | Manage Events only, 1 room/post, 5 live/guild, 20/day, 60 s per actor |
| Title text in a channel name | Existing automod name filter, 48 chars, fallback name; titles with slurs never reach the channel |
| Cross-guild confusion | Composite keys; foreign evidence collapses to `UNAVAILABLE` |
| Lock via user limit is soft | Members already inside can leave and let others in up to the headcount; accepted, because a deny overwrite needs Manage Roles |
| Audit non-atomic with binding | Binding authoritative, audit best-effort and honest about failure |

**CISO proposal assessment:** acceptable to circulate for intent review with the
above controls; **not** a grant, independent approval of our own boundary request,
or runtime security acceptance. Blocking findings before any later build or
enable: (1) accept or narrow the new voice-lifecycle boundary: a bot-created,
bot-deleted channel keyed to LFG state and the Manage Channels dependency, with no
Move Members, Manage Roles or Connect; (2) accept the soft user-limit lock and the
"never disconnect, never delete an occupied room" rule instead of a deny overwrite;
(3) accept the audit and erasure plan (separate non-atomic audit append, identity
kept only in audit rows). If acceptance needs Manage Roles, a Move Members move, a
DM, a new gate beyond those named, or auto-creation, stop this proposal. No
credential or permission is requested by this document. Later security-sensitive
changes still require independent applicable security disposition and exact-head
tests.

## 6. Private UX, agent-facing result and offline fixture contract

**Private creation UX** (ephemeral, plain words, no internal codes shown):

1. Preview: "Create a voice room for <post>? Name <name>, category <category>,
   limit <n>. Opens now, ends when the post ends. Nobody is moved or pinged."
2. Confirm (link added): "Room created: <channel>. A link was added to the post. It is deleted
   shortly after the post ends and the room is empty."
3. Confirm (link pending): "Room created: <channel>. The post link could not be added; staff have been told. It is deleted
   shortly after the post ends and the room is empty."
4. Status: the room, state, "locked/overdue" and how to end it early.

**Failure corrections** are specific and actionable, and each names who acts next:

| reason | Member-visible correction | retryable / next_action |
| --- | --- | --- |
| `NOT_ENABLED` | "Voice rooms for LFG are off on this server." | false / `ask_staff_to_enable` |
| `NO_CATEGORY` | "No category is set for LFG voice rooms." | false / `staff_set_category` |
| `PERM_MISSING` | "The bot needs Manage Channels and View Channel on <category>." | false / `staff_grant_permission` |
| `PREFLIGHT_UNKNOWN` | "Could not check permissions right now." | true / `retry_after_backoff` |
| `FORBIDDEN` | "You need Manage Events for this post." | false / `ask_organizer` |
| `OUTSIDE_WINDOW` | "Rooms open 60 minutes before the start." | true / `retry_when_window_opens` |
| `POST_CLOSED` / `POST_EXPIRED` / `POST_MISSING` | "This post is closed/expired/gone." | false / `create_new_post` |
| `ROOM_EXISTS` | "This post already has a room." | false / `use_existing_room` |
| `CAP_GUILD` | "This server has the maximum number of LFG rooms." | true / `wait_or_end_other_room` |
| `CAP_DAILY` | "This server has used today's LFG room creates." | true / `retry_tomorrow` |
| `CATEGORY_FULL` | "The category is full." | false / `staff_free_category_slot` |
| `RATE_LIMITED` | "Try again shortly." | true / `retry_after_backoff` |
| `UNKNOWN_OUTCOME` | "Discord did not confirm the room; do not retry." | false / `staff_check_category` |
| `LINK_PENDING` | "Room created, but the post link could not be added." | false / `staff_check_post` |
| `UNAVAILABLE` | "That post is not available here." (collapses missing/foreign/denied) | false / `verify_post_id` |
| `STORE_UNAVAILABLE` | "Could not save; nothing changed." | true / `retry_after_backoff` |

No new executable CLI or API is registered; these are **proposed typed results**
for future adapters that discover this file through the [documentation
map](README.md). Stable envelope: `schema=lfg.voice_room.v1`, `guild_id`,
`post_id`, `room_channel_id` (null unless bound and visible to the actor), `state`,
`reason`, `retryable`, `next_action`, `dry_run`, `window{opens,closes}`, `name`
(preview only), `category_id`, `user_limit`, `caps{live,live_cap}`, `moved=0`.
Preview is the dry run. Denied replies keep the same envelope with `room_channel_id`
and `window` set to null, never omitted. No raw SQL or
Discord errors. Stable room/post IDs are the only identity.

This design's offline check has no environment selector, credentials, network,
database, Discord client, side effects or wrong-flag production path. No staging or
live pilot, room creation or permission change is authorized, and the check cannot
reach one by combining flags.

The deterministic **fixture oracle** (a specification, not an implementation) uses
guild `g1`, post `p1`, DB time `T`, category `c1`. Each row resets that fixture.
`created` counts channel-create calls; `locked` counts user-limit locks; `deleted`
counts channel deletes; `moved` counts member moves and must always be 0. All rows
require zero DMs, zero mentions and zero other-guild data.

| Case | Controlled event / expected assertion |
| --- | --- |
| flag_off | Admission flag off -> NOT_ENABLED, no write |
| no_category | Category unset -> NO_CATEGORY |
| foreign_category | Category belongs to `g2` or is not a category -> NO_CATEGORY |
| perm_missing | Bot lacks Manage Channels (or View) on `c1` -> PERM_MISSING, no Discord write |
| perm_unknown | Permission snapshot unavailable -> PREFLIGHT_UNKNOWN, no write |
| no_authority | Actor lacks Manage Events -> FORBIDDEN |
| preview | Valid preview -> PREVIEW, zero writes, no reservation |
| create | Confirm in window -> CREATED, one create, post edited |
| confirm_replay | Same confirm again -> UNCHANGED, no second create |
| concurrent_confirm | Two simultaneous confirms -> one CREATED, other returns same room |
| room_exists | Room already bound -> ROOM_EXISTS, no create |
| too_early | `T < starts_at - 60 min` -> OUTSIDE_WINDOW |
| post_closed | Post Closed -> POST_CLOSED |
| post_expired | `T >= starts_at + 15 min` -> POST_EXPIRED |
| post_deleted | Post row absent -> POST_MISSING |
| foreign_guild | Interaction guild `g2` for `p1` -> UNAVAILABLE |
| foreign_post | Post owned by `g2` -> UNAVAILABLE |
| guild_cap | 5 live rooms (creating/active/winding_down/locked/held) -> CAP_GUILD |
| daily_cap | 20 creates used today, fewer than 5 live -> CAP_DAILY, no create |
| held_counts | Held claim plus 4 live rooms -> CAP_GUILD on next create (held counts) |
| category_full | `c1` has 50 channels -> CATEGORY_FULL |
| rate_limited | Second confirm by actor within 60 s -> RATE_LIMITED |
| unknown_outcome | Create returns bodyless 2xx -> UNKNOWN_OUTCOME, claim held, no repost |
| edit_failed | Create succeeds, post edit fails -> LINK_PENDING, room active and counted, sweep re-edits |
| close_during_create | Close arrives with create in flight -> HELD, close pends, no repost, no delete |
| no_auto_create | Post created or edited -> NOOP, no create |
| signup_no_move | Member signs up while room live -> NOOP, no move |
| never_occupied | Open post, room empty past 60 s -> NOOP, room kept |
| close_empty | Explicit close, room empty -> DELETED after grace |
| close_occupied | Explicit close, humans present -> LOCKED to headcount, no delete |
| expire_empty | Sweep expires post, room empty -> DELETED after grace |
| expire_occupied | Sweep expires post, humans present -> WINDING_DOWN, no lock, no delete |
| overdue_lock | `T = starts_at + 6 h`, humans present -> LOCKED once, nobody disconnected |
| post_deleted_room | Post hard-deleted, room empty -> DELETED after grace |
| manual_delete | Staff deleted the channel -> GONE, no Discord delete |
| adopt_orphan | Claim stuck, exactly one matching channel -> ADOPTED |
| orphan_ambiguous | Two matching channels -> HELD, nothing deleted |
| untracked_channel | Same-named channel with no row -> NOOP, never touched |
| restart_delete | Restart mid-delete -> DELETED, idempotent on 404 |
| restart_claim | Restart with fresh `creating` claim -> HELD, no second create |
| unknown_state | Row has unknown state value (version skew) -> HELD, no write |
| flag_off_cleanup | Flag turned off with live room, post closed -> DELETED after grace |
| erasure_actor | Non-creator organizer erased, post survives -> UNCHANGED, audit identity removed |
| erasure_creator | Creator erased, post hard-deleted -> DELETED after grace (close path) |
| store_outage | Store fails before transaction -> STORE_UNAVAILABLE, never assumed created |

Runnable offline document check (Python stdlib; from repo root):

```sh
python3 -m doctest -v docs/lfg-voice-room-spec.md
python3 scripts/check_docs_index.py --json
```

The doctest checks the **specified oracle's consistency** and that it matches the
matrix above, not real SQL locks, Discord behavior, permissions, rendering,
erasure, outage handling or deployed behavior. Future protected acceptance tests
must implement barriers, an injected clock and store/REST faults against actual
callers in disposable fixtures; zero tests is not proof. No acceptance or
implementation task is created by this proposal.

```pycon
>>> import json, pathlib
>>> # (case, reason, created, locked, deleted, moved); abstract expected snapshots
>>> Z = (0, 0, 0, 0)
>>> oracle = [
...     ("flag_off", "NOT_ENABLED") + Z,
...     ("no_category", "NO_CATEGORY") + Z,
...     ("foreign_category", "NO_CATEGORY") + Z,
...     ("perm_missing", "PERM_MISSING") + Z,
...     ("perm_unknown", "PREFLIGHT_UNKNOWN") + Z,
...     ("no_authority", "FORBIDDEN") + Z,
...     ("preview", "PREVIEW") + Z,
...     ("create", "CREATED", 1, 0, 0, 0),
...     ("confirm_replay", "UNCHANGED") + Z,
...     ("concurrent_confirm", "CREATED", 1, 0, 0, 0),
...     ("room_exists", "ROOM_EXISTS") + Z,
...     ("too_early", "OUTSIDE_WINDOW") + Z,
...     ("post_closed", "POST_CLOSED") + Z,
...     ("post_expired", "POST_EXPIRED") + Z,
...     ("post_deleted", "POST_MISSING") + Z,
...     ("foreign_guild", "UNAVAILABLE") + Z,
...     ("foreign_post", "UNAVAILABLE") + Z,
...     ("guild_cap", "CAP_GUILD") + Z,
...     ("daily_cap", "CAP_DAILY") + Z,
...     ("held_counts", "CAP_GUILD") + Z,
...     ("category_full", "CATEGORY_FULL") + Z,
...     ("rate_limited", "RATE_LIMITED") + Z,
...     ("unknown_outcome", "UNKNOWN_OUTCOME") + Z,
...     ("edit_failed", "LINK_PENDING", 1, 0, 0, 0),
...     ("close_during_create", "HELD") + Z,
...     ("no_auto_create", "NOOP") + Z,
...     ("signup_no_move", "NOOP") + Z,
...     ("never_occupied", "NOOP") + Z,
...     ("close_empty", "DELETED", 0, 0, 1, 0),
...     ("close_occupied", "LOCKED", 0, 1, 0, 0),
...     ("expire_empty", "DELETED", 0, 0, 1, 0),
...     ("expire_occupied", "WINDING_DOWN") + Z,
...     ("overdue_lock", "LOCKED", 0, 1, 0, 0),
...     ("post_deleted_room", "DELETED", 0, 0, 1, 0),
...     ("manual_delete", "GONE") + Z,
...     ("adopt_orphan", "ADOPTED") + Z,
...     ("orphan_ambiguous", "HELD") + Z,
...     ("untracked_channel", "NOOP") + Z,
...     ("restart_delete", "DELETED", 0, 0, 1, 0),
...     ("restart_claim", "HELD") + Z,
...     ("unknown_state", "HELD") + Z,
...     ("flag_off_cleanup", "DELETED", 0, 0, 1, 0),
...     ("erasure_actor", "UNCHANGED") + Z,
...     ("erasure_creator", "DELETED", 0, 0, 1, 0),
...     ("store_outage", "STORE_UNAVAILABLE") + Z,
... ]
>>> _lines = pathlib.Path("docs/lfg-voice-room-spec.md").read_text().splitlines()
>>> _start = next(i for i, l in enumerate(_lines) if l.startswith("| Case |"))
>>> _matrix = []
>>> for _l in _lines[_start + 2:]:
...     if not _l.startswith("| "):
...         break
...     _matrix.append(_l.split("|")[1].strip())
>>> assert _matrix == [row[0] for row in oracle], set(_matrix) ^ {row[0] for row in oracle}
>>> assert len({row[0] for row in oracle}) == len(oracle) == 45
>>> assert all(row[5] == 0 for row in oracle)  # never moves a member
>>> assert all(row[2] <= 1 for row in oracle)  # at most one create
>>> assert all((row[2] == 1) == (row[1] in ("CREATED", "LINK_PENDING")) for row in oracle)
>>> assert all(row[3] + row[4] <= 1 for row in oracle)  # lock xor delete per snapshot
>>> assert all(row[2:] == Z for row in oracle if row[1] in ("UNAVAILABLE", "NO_CATEGORY", "PERM_MISSING", "CAP_GUILD", "CAP_DAILY", "UNKNOWN_OUTCOME", "STORE_UNAVAILABLE", "HELD"))
>>> assert sum(1 for row in oracle if row[0].startswith("foreign_")) == 3
>>> assert all(row[1] == "UNAVAILABLE" for row in oracle if row[0] in ("foreign_guild", "foreign_post"))
>>> assert dict((row[0], row[1]) for row in oracle)["flag_off_cleanup"] == "DELETED"
>>> assert dict((row[0], row[1]) for row in oracle)["expire_occupied"] == "WINDING_DOWN"
>>> _cc_rooms = {"caller_a": "ch_9f3a", "caller_b": "ch_9f3a"}  # concurrent_confirm: both callers get the same room
>>> assert _cc_rooms["caller_a"] == _cc_rooms["caller_b"]
>>> assert dict((row[0], row[2]) for row in oracle)["concurrent_confirm"] == 1
>>> assert dict((row[0], row[1]) for row in oracle)["erasure_actor"] == "UNCHANGED"
>>> assert dict((row[0], row[1]) for row in oracle)["erasure_creator"] == "DELETED"
>>> starts = 1_000_000
>>> opens, closes, overdue = starts - 3600, starts + 900, starts + 6 * 3600
>>> assert closes - opens == 4500  # a never-occupied room lives at most 75 minutes
>>> assert opens < starts < closes < overdue
>>> print(json.dumps({"schema": "lfg.voice_room.oracle.v1", "fixtures": len(oracle), "status": "pass"}, sort_keys=True))
{"fixtures": 45, "schema": "lfg.voice_room.oracle.v1", "status": "pass"}

```

## 7. Acceptance boundary, review metric and undo

Record CEO spec acceptance **and** host intent alignment against the exact
revision before any implementation subtasks, protected test handoff or live pilot.
Route the decision brief through the President/COO to the CEO and the existing host
intent path; a queue/intake receipt is not alignment. The CISO findings above must
receive an independent boundary disposition of the new voice-lifecycle and
permission boundary **before any build card**, not a self-approved grant. No
request should be sent directly to the owner or an invented host executor.

The decision must accept or revise: C versus the B fallback; the explicit-only
trigger; the 60-minute window and one room per post lifetime; View Channel and
Manage Channels as the sole bot bits; required configured category; user-limit
lock in place of a deny overwrite; never disconnect and never delete an occupied
room; the 5 live room / 20 per day caps; and the separate binding table without
member identity. On any material revision, bind a fresh review request and
alignment to that revision.

By 2026-10-31, metric is **all 45 fixture-matrix rows have agreed or explicitly
rejected outcomes, and the three blocking findings have written disposition**.
This measures design completeness, not engagement. Keep this proposal only if the
fixtures show a bounded lifecycle with no unsolicited moves and no new spend;
otherwise recommend no-build (A) or the B fallback. Default while unresolved is no
build; no timeout auto-approval. A future demand experiment requires its own
hypothesis, metric and date, not success inferred from this document.

ACT: publish this proposed contract and its offline oracle for review. WHY: bound
the channel-lifecycle and permission boundary before considering a per-session room.
UNDO: reviewed docs revert; there is no operational change to undo. A later
independently accepted build must keep admission dark until its own
tests/security/release gates pass; disabling future admission must preserve lock,
delete, `gone` marking and erasure behavior, not strand or delete rooms or erase
history. No production tests, migrations, credentials, live room creation or
activation here.
