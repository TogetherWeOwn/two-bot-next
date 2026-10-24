# Events: an opt-in per-event attendee role

Status: **proposed, design only**. Written 2026-10-24; narrowed per
independent review 2026-10-24; review by 2026-10-31.
No commands, storage, migrations, permissions, role grants, sends or runtime
tests are added. Merge publishes a proposal, not permission to implement,
enable, or grant anything. This file is the single repository contract; review
requests bind its commit and SHA-256.

## 1. Decision and evidence

Consumer: an organizer who wants to mention only the members who said they are
coming, and a member who wants that mention and no other. Hypothesis: a role
that exists only for one event's confirmed attendees saves organizers manual
role upkeep. No customer validation is claimed.

The [sesh manual](https://sesh.fyi/manual/) lists attendee roles as a core
event feature, and a [sesh-alternatives comparison](https://peakbot.pro/blog/sesh-alternatives)
says Apollo advertises them. Both are vendor capability claims read 2026-10-24,
not evidence of TWO demand. Discord's native event interest already notifies
interested members at start; whether that meets the need is unmeasured.

| Option | Value and cost | Disposition |
| --- | --- | --- |
| A. No build: staff keep a manual mentionable role | No new write boundary; costs staff upkeep and stale holders | Default until acceptance; retain if the boundary below is not accepted |
| B. Static role pool: admin pre-creates and allowlists up to 5 inert roles; staff bind one to one upcoming event; the bot only adds or removes members | Smallest boundary: no role create/edit/delete, no permission change; a pool of one is a valid start; role count caps concurrent events | Smallest option that tests the hypothesis; recommend for **spec review**, not build, as narrowed in §6 |
| C. Bot-managed role per event: the bot creates, grants and deletes `event-*` roles | No admin pre-work, but needs Manage Roles to create and delete, event-name-derived names (injection, 100-char limit), the 250-role guild cap, orphan deletion, and a permission-bearing create path | Reject: larger blast radius for the same member value |

Benefit if B works: per-event mention of confirmed attendees with no staff
upkeep. Uncertainty: demand, whether native interest suffices, and whether
members accept a **publicly visible** attendance marker (§3). "One static role
reused by every event" is rejected: mentioning it would ping other events'
attendees, so B binds a role to at most one active event at a time, and a role
stays unavailable until its previous event's rows drain (§4).

Out of scope: auto-grant on interest, voice presence or message activity;
recurring-series roles; custom role colors or icons; DMs, calendars, new spend.

## 2. Verified baseline and missing seams

Inspected repository head: `89b26ae050ffba9fb409622ffd54d31cdc79e7fc`.

- **Premise gap:** join roles at onboarding ([#1643](https://github.com/TogetherWeOwn/two-bot-next/pull/1643))
  are **not on this head**; that PR is open, unmerged and conflicting, on branch
  `feat/onboarding-role-verification`. The proposed grant path reuses its
  *pattern* (per-member advisory lock, write-ahead `operational_audit_log`
  attempt then settle, fresh recipient and role-policy read before mutation),
  not merged code. Reconcile with #1643 before any build.
- `crates/core/src/rsvp.rs:38-48` has Going/Interested/Declined; `:446-456`
  caps **1,000 rows/event**, 20 writes/member/guild/UTC minute, 90-day RSVP and
  365-day audit retention. `crates/cutover/migrations/0160_rsvp.sql:24-34` keys
  `event_rsvps` by `(guild_id,event_id,user_id)`. There is no role field.
- `crates/discord/src/rsvp.rs:158-239,298-389` checks live actor membership
  before event lookup, and compensates the write on a failed recheck
  (`compensate_rsvp_write`, `crates/core/src/rsvp_store.rs:494`). RSVP is an
  ephemeral slash reply, not a post-bound button.
- `crates/core/src/rsvp.rs:229-236` is the server-side Manage Events gate for
  check-in; [seats](rsvp-capacity-spec.md) reuses it for configuration.
- Role mutation exists only for self-roles: `crates/discord/src/self_roles_rest.rs:228`
  force-fetches member, roles and channels after lane acquisition;
  `:366-384` adds or removes one member role. `crates/core/src/self_roles.rs:1220-1330`
  refuses missing Manage Roles, @everyone, managed or not-below-bot roles, and
  `find_unsafe_channel_grant` (`:661`) refuses new channel access. Self-roles
  allow a permission mask (`SELF_ROLE_ALLOWED_MASK`); this feature needs the
  **stricter zero-permission** rule and must not change that mask.
- `crates/core/src/scheduled_events.rs:31,38` polls the event mirror every 10
  minutes, at most 100 rows; it stores status and `starts_at` but **no end
  time**. Mirror status words are `scheduled|active|completed|cancelled`
  (`:46-61`, canonical `EventStatus::Cancelled`). No scheduled-event gateway
  handler exists, and no role-event handler feeds RSVP (member add/remove/update
  are in `crates/bot/src/onboarding.rs:320-334`;
  `crates/bot/src/containment_runtime.rs:76` classifies audit-log role deletes
  for destructive-action detection only). Cancellation and external role
  deletion are therefore seen by poll or fresh fetch.
- Audit retention is split: `operational_audit_log` rows are swept after 45
  days (`crates/cutover/src/retention.rs:94`, applied `:605-607`), while
  `announcements_audit_log` keeps 365 days (`crates/core/src/rsvp.rs:456`) but
  `prune_rsvp_history` deletes only RSVP/checkin actions
  (`crates/core/src/rsvp_store.rs:704-713`). §5 picks one table and names the
  required prune change.
- `crates/cutover/src/member_erasure_plan.json:144-147` deletes `event_rsvps`
  by `(guild_id,user_id)`; any new table needs its own entry.

No live guild, permission, deployment or shared-database writer was inspected.
If a build cannot keep grants provably scoped, revocable and reconciled, stop
and keep option A.

## 3. Grant, revoke and consent contract

Identity is `(guild_id, discord_scheduled_event_id, discord_user_id, role_id)`
from the authenticated interaction and Discord IDs, never names or titles.
Reschedules keep identity. An ambiguous series/template ID is unsupported.

**Disclosure finding.** Today a member's RSVP is private: they see only their
own answer and event totals. A role is visible to other members (member list,
profile). Granting a role discloses attendance, so a Going RSVP alone must not
grant it.

**Grant trigger (all required).** (1) the member holds a saved Going RSVP on the
event; (2) an active binding exists; (3) the member clicks an explicit
ephemeral **Get event role — visible to all members** control in their own
`/rsvp` Going reply — the label itself names the public visibility, not only a
preceding paragraph; if the role is mentionable the reply adds that anyone who
can ping it may notify the holder; (4) the fresh preflight in §4 passes,
including before-expiry. Never inferred from Interested, voice presence,
message activity, native event interest, or an RSVP made before enabling.
Opt-in is per event, never sticky across events, never staff-settable and
never bulk. Existing Going members see the control the next time they run
`/rsvp status:going`; there are no retroactive grants. A custom ID grants no
authority: every click re-resolves actor, binding, RSVP and live state.
*Alternative for review:* Going alone grants. Recommended against because of
the disclosure finding above.

**Revoke triggers.** RSVP becomes Interested or Declined or its row is
compensated away; the member clicks **Remove event role** (RSVP stays Going);
the mirror shows the event Canceled, Completed or deleted; staff unbind;
`starts_at + 24 h` passes (hard stop; the mirror has no end time); the binding
is suspended (§5). Consent withdrawal revokes inline: the Remove click and an
RSVP flip to Interested/Declined attempt the remove in the same request path
(best effort, under the advisory lock, re-reading after REST); the sweep is
only the backstop. The other triggers resolve through the sweep within the
bound below. Revoke never requires the safety predicates, only live Manage
Roles and a role below the bot; when those are missing the row is loud per the
cleanup rule, never dropped. A member who left the guild needs no REST call.

**Bounds.** At most one role per event and one active event per role; a role
stays bound-or-draining until every row is terminal (`revoked`, `member_left`,
`externally_removed`, `expired_unconfirmed`). The predicate is allowlist-shaped:
a `pending_grant` may already have reached Discord, and an unknown future state
blocks the drain (fail closed, per the inert rule in §4) instead of releasing
the role. Rebind while draining is refused (`ROLE_DRAINING`). Pool cap 5
roles per guild, so at most 5 active bindings and 5,000 non-terminal ledger
rows (1,000 grants/event, the RSVP cap). Terminal rows are kept 90 days (§5),
so total stored rows have no fixed bound without a limit on sequential events;
the build sizes and indexes that prune. A role is eligible only if it exists, is not @everyone or
managed, has `permissions == 0`, appears in no channel overwrite (any
appearance, stricter than the self-role new-access helper), has `hoist ==
false`, is referenced by no other TWO config (self-role panels, join roles,
staff or mod mappings, ticket, voice or announcement role settings), sits
below the bot's highest role, and is mentionable at enable time. Allowlisting
carries an admin attestation that no external bot, AutoMod exemption or
integration keys off the role (`permissions == 0` does not mean "no
authority"). The bot never creates, renames, edits or deletes any role, never
changes mentionability, and never touches a role outside the allowlist or any
member role it did not grant. Pre-existing manual holders of the role are
unknown and are never removed; use a dedicated unused role. Anyone who can
post in a channel can ping a mentionable role there; staff enable copy
discloses this (not staff-only).

**Stale-grant cleanup.** A level-triggered sweep ticks at most every 60 s and
handles at most 50 ledger rows per tick guild-global (one shared budget, not
per event), revoke rows oldest-first ahead of finishing any intent. It
computes desired state `binding active ∧ RSVP Going ∧ opted in ∧ member
present ∧ before expiry ∧ role still safe` against fresh member and role
reads; a granted row whose member presence cannot be positively read is kept
for retry, never asserted desired. Worst-case drain, oldest-first: one full
event draining alone (≤ 1,000 rows, otherwise empty ledger) finishes in ≤ 20
ticks (≤ 20 min); under a full 5,000-row ledger any single event's rows can
finish as late as tick ~100 (~100 min). Cancel, complete and expiry therefore
land within one 10-minute mirror poll plus ≤ 20 min isolated, up to ~100 min
under a full ledger — when every revoke is executable. A row whose revoke
cannot execute has no fixed completion bound; it stays visible via the
`revoke_blocked` count and the 1 h overdue alert below. The sweep **only
revokes**. It never grants and never re-grants a role a member or staff removed
(`externally_removed` is terminal until a new explicit click). It retries
`pending_revoke` rows: a crash between the row write and the REST remove must
not strand them. A `pending_grant` row that is stale or no longer desired
revokes when the member holds the role (the add may have reached Discord) and
only then goes terminal. The one exception is finishing a `pending_grant`
intent recorded within the last 15 minutes while every condition still holds,
including positive member presence and a fresh safe role snapshot; older or
unsafe intents become `expired_unconfirmed` (revoking first when held). Rows
whose revoke cannot execute (bot lost Manage Roles, role moved above the bot)
stay visible: bounded retry with backoff, a staff-visible `revoke_blocked`
count in status, and an operational alert once any row is older than 1 h past
expiry. The 24 h hard stop bounds desired state, not silent dropping: nothing
disappears quietly.

## 4. Authorization, preflight and write path

- **Allowlist** (`allow`/`disallow` a role): Manage Server, as onboarding admin
  commands use; the allow action records an attestation that no external bot,
  AutoMod exemption or integration keys off the role. **Bind/unbind**: freshly
  evaluated Manage Events on the invoking interaction plus a matching mirror
  event, choosing only from the allowlist. Event creator or Manage Roles alone
  is not authority. **Member controls**: the authenticated member acts only on
  their own row.
- Gate order: configured guild from interaction context, Announcements enabled,
  current human membership (live), then the check list below. Unknown evidence
  fails closed. Foreign guild, missing and denied collapse into one reply.
- **Preflight** returns the first failure with an actionable correction:
  `MISSING_MANAGE_EVENTS`, `EVENT_NOT_ELIGIBLE` (only Scheduled may be bound;
  mirror word `cancelled`), `EVENT_ALREADY_BOUND`, `ROLE_NOT_ALLOWED`,
  `ROLE_MISSING`, `ROLE_UNSAFE` (@everyone/managed), `ROLE_HAS_PERMISSIONS`,
  `ROLE_CHANNEL_OVERWRITE` (any overwrite appearance; fresh channel fetch on
  bind and on every grant), `ROLE_HOISTED`, `ROLE_REFERENCED` (another TWO
  config uses it), `BOT_MISSING_MANAGE_ROLES`, `ROLE_HIERARCHY` (raise the
  bot's role above it), `ROLE_IN_USE`, `ROLE_DRAINING` (previous event's rows
  not yet settled), `ROLE_NOT_MENTIONABLE`, plus `ANNOUNCEMENTS_DISABLED`,
  `MEMBER_UNAVAILABLE` (invoker not a live member) and, on the grant path
  only, `EXPIRED` (past the hard stop). Bind runs it once; every grant
  re-runs the safety checks on a fresh snapshot, because a role can gain
  permissions after binding.
- **Write path** (proposed ledger `event_role_bindings` and `event_role_grants`,
  one row per member per event, states `pending_grant`, `granted`,
  `pending_revoke`, `revoked`, `member_left`, `externally_removed`,
  `expired_unconfirmed`): take the per-`(guild,event,member)` advisory lock;
  re-read the RSVP; write `pending_grant` plus the write-ahead audit row; one
  `add_guild_member_role`; settle `granted`; after the REST call re-read the RSVP
  and, if no longer Going, revoke (the committed row, not REST completion order,
  decides concurrent RSVP/leave). Consent withdrawal (Remove click, RSVP flip)
  takes the same lock and attempts one `remove_guild_member_role` inline,
  settling `revoked` and writing the audit row; on failure the row becomes
  `pending_revoke` for the sweep to retry. Two write-time predicates back the
  §3 bounds: unique `(guild_id, role_id)` where bound-or-draining enforces one
  event per role until the drain completes, and unique `(guild_id, event_id)`
  where bound-or-draining enforces one role per event. Both predicates use the
  §3 allowlist definition: a row counts as settled only in a terminal state, so
  an unknown state keeps the binding draining. Binds take a
  per-`(guild,event)` advisory lock before the preflight re-check and insert,
  so concurrent staff binds serialize instead of double-binding. An
  unknown future state is inert: never granted, never revoked, never deleted,
  and it blocks the drain.
- Guild isolation: every key, lock and query carries `guild_id`; a role or event
  from another guild is `UNAVAILABLE`.

## 5. Lifecycle, reconciliation and privacy

| Condition | Behavior |
| --- | --- |
| Event rescheduled | Binding follows the event ID; expiry recomputed from the mirror; no grant, revoke or message |
| Event canceled/completed | Revoke all `granted` rows within the sweep; worst-case latency is one 10-minute mirror poll plus ≤ 20 min isolated, up to ~100 min under a full ledger (§3), when each revoke is executable; blocked rows have no fixed bound and use the status/alert path, disclosed to staff |
| Role deleted externally | Binding becomes `SUSPENDED_ROLE_MISSING`; no recreation, no grant; staff see it in status |
| Role changes (permissions, overwrite presence, managed flag, hoist, hierarchy) | Change detection runs on every sweep tick and every grant; any change suspends the binding (`SUSPENDED_ROLE_CHANGED`); no new grants; revoke continues. Residual TOCTOU between the fresh read and the REST add is accepted |
| Member left guild | Row becomes `member_left`; no REST call |
| Revoke cannot execute | Row stays `pending_revoke` (or granted-but-undesired); bounded retry with backoff; status shows a `revoke_blocked` count; operational alert once any row is older than 1 h past expiry |
| Duplicate click | Row already `granted`: no REST, no second audit |
| Restart / rolling deploy | The advisory lock serializes overlap; `pending_*` rows are re-resolved against fresh reads (`pending_revoke` retries; `pending_grant` revokes when held) |
| Version skew / rollback | An older binary treats unknown states as inert; disabling the feature stops new grants and keeps the revoke sweep and ledger |
| Erasure | Add a `(guild_id,user_id)` plan entry; the bot first tries to revoke a present member, otherwise reports `role_revoke_unconfirmed`; keep an ID-only revoke obligation until revoke is confirmed; a leftover is only a role staff can remove |

Audit facts (`operational_audit_log`, no event title, name or message text):
`event_role.bind|unbind|grant|revoke|suspend` with guild, actor, event ID, role
ID, member ID and a `cause`. That table's sweep deletes rows after 45 days
(`crates/cutover/src/retention.rs:94`), so the build must exempt
`event_role.*` actions until their ledger row is terminal plus 90 days; grant
evidence must outlive the ledger row it explains. Do not use
`announcements_audit_log` for these facts: its 365-day prune covers only
RSVP/checkin actions (`rsvp_store.rs:704-713`). Ledger rows are deleted 90 days
after a terminal state. The resulting attendance trail (member + event IDs)
outlives RSVP rows; state that in the privacy docs at build.

Member copy (ephemeral): grant `You now have <@&role> until the event ends.
Other members can see this role.`; revoke `Removed. You are still Going.`
Staff enable copy states the role, event, bounds, that existing Going members
are not granted, that a mentionable role can be pinged by anyone who can post
(not only staff), and that the bot sends no ping, DM or channel message. The
bot never mentions the role itself.

## 6. Security findings, CISO assessment and boundary

| Threat | Proposed control and remaining finding |
| --- | --- |
| Privilege escalation through the role | Zero permissions, no overwrites (any appearance), not hoisted, below the bot, no other TWO reference, allowlist attestation, fresh re-check per grant and per sweep tick; residual TOCTOU accepted |
| Staff binding an arbitrary role | Manage Events staff choose only admin-allowlisted roles; allowlisting needs Manage Server plus the no-external-use attestation |
| Attendance disclosure | Separate explicit opt-in; the control label itself names the visibility; role visibility is a new public marker the bot cannot hide |
| Bot touching unrelated roles/members | Only allowlisted roles and only rows the bot granted; manual holders untouched |
| Grant after consent withdrawn | Locked re-read before and after REST; compensating revoke; inline withdrawal with sweep backstop |
| Stale cancel (up to 10 minutes plus drain) | Disclosed (≤ 10 min poll + ≤ 20 min isolated, up to ~100 min under a full ledger, executable revokes only; blocked rows unbounded via status/alert); mention of a canceled event's role may still reach holders |
| Role rebound before the drain | Bound-or-draining uniqueness plus `ROLE_DRAINING`; rebind refused until every row is terminal; unknown states block the drain |
| Silent stuck revoke | Bounded retry, staff-visible `revoke_blocked` count, alert past 1 h overdue |
| Cross-guild leakage | Guild-keyed rows and locks; foreign IDs collapse to `UNAVAILABLE` |

**Independent CISO disposition (2026-10-24, on the prior revision): ACCEPT,
NARROWED.** Option B stands; the following narrowing is written into this
revision and its oracle: (N1) the sweep retries `pending_revoke`; (N2) stale
or undesired `pending_grant` rows revoke when the role is held; (N3)
eligibility adds `hoist == false`, no other TWO config reference, and an
allowlist attestation; (N4) change detection covers permissions, overwrites,
managed flag, hoist and hierarchy on every sweep tick and grant; (N5)
unexecutable revokes are counted, surfaced and alerted past 1 h overdue; (N6)
consent copy names public visibility on the control; (N7) the sweep ticks at
most every 60 s with stated drain bounds; (N8) consent withdrawal revokes
inline with the sweep as backstop. Reaffirmed stops: role creation,
permission-bearing roles, retroactive grants, sweep-side granting, bot DM or
ping, and auto-grant from interest, voice presence or activity — any of them
re-opens review. Non-blocking notes: the 7-day grant window stays a build-time
option; the longer audit trail is disclosed in §5; erasure keeps an ID-only
revoke obligation; Manage Events role-squatting is a noted accepted abuse;
#1643 reconciliation is still required before build. This disposition grants
nothing and authorises no build, role or permission change. If a build needs
role creation, a permission-bearing role, retroactive grants, or sweep-side
granting, stop this proposal. No credential or permission is requested here.

## 7. Agent-facing contract and offline fixtures

No executable CLI/API is registered. These are **proposed typed results**, not
commands for a guild. Envelope: `schema=events.attendee_role.v1`, `guild_id`,
`event_id`, `role_id`, `mode` (`preview`|`apply`), `binding_state`, `checks`
(ordered `{id, pass}`), `bounds` (pool 5, 1,000 grants/event, 24 h hard stop),
`reason`, `retryable`, `next_action`. `preview` is the default and performs no
Discord or database write; `apply` is a separate explicit call. One repo source
(this file) and one runnable check (below). No raw SQL or Discord errors appear.
The envelope schema is `events.attendee_role.v1`; the oracle below prints
`events.attendee_role.oracle.v1`, intentionally distinct: the oracle output is
a test-harness report (fixture count plus pass), not a result envelope. A build
maps oracle decisions onto envelope results field by field.

The check has no environment selector, credentials, network, database, Discord
client or side effect; no staging or live pilot is authorized. The doctest runs a
pure reference model of the rules above and asserts each fixture; it proves the
**specified rules are consistent**, not SQL, REST, or deployed behavior.

```sh
python3 -m doctest -v docs/event-role-spec.md
python3 scripts/check_docs_index.py --json
```

```pycon
>>> import json
>>> SAFE = dict(managed=False, everyone=False, perms=0, pos=3, mentionable=True,
...             hoist=False, overwrites=False, referenced=False)
>>> OK_INPUT = dict(guild_ok=True, announcements=True, member="present", staff=True,
...                 event="scheduled", event_bound=False, allowlisted=True, role=SAFE,
...                 bot_manage_roles=True, bot_pos=9, role_bound=False, draining=False)
>>> def preflight(i):
...     # fail closed: every missing key behaves as denied
...     if not i.get("guild_ok", False):
...         return "UNAVAILABLE", "verify_access"
...     if not i.get("announcements", False):
...         return "ANNOUNCEMENTS_DISABLED", "enable_announcements"
...     if i.get("member", "unknown") != "present":
...         return "MEMBER_UNAVAILABLE", "verify_membership"
...     if not i.get("staff", False):
...         return "MISSING_MANAGE_EVENTS", "ask_staff_with_manage_events"
...     if i.get("event", "unknown") != "scheduled":
...         return "EVENT_NOT_ELIGIBLE", "pick_scheduled_event"
...     if i.get("event_bound", False):
...         return "EVENT_ALREADY_BOUND", "unbind_first"
...     if not i.get("allowlisted", False):
...         return "ROLE_NOT_ALLOWED", "ask_admin_to_allow_role"
...     role = i.get("role")
...     if role is None:
...         return "ROLE_MISSING", "pick_existing_role"
...     if role.get("everyone", True) or role.get("managed", True):
...         return "ROLE_UNSAFE", "pick_ordinary_role"
...     if role.get("perms", -1) != 0:
...         return "ROLE_HAS_PERMISSIONS", "use_zero_permission_role"
...     if role.get("overwrites", True):
...         return "ROLE_CHANNEL_OVERWRITE", "use_role_without_overwrites"
...     if role.get("hoist", True):
...         return "ROLE_HOISTED", "use_non_hoisted_role"
...     if role.get("referenced", True):
...         return "ROLE_REFERENCED", "use_dedicated_role"
...     if not i.get("bot_manage_roles", False):
...         return "BOT_MISSING_MANAGE_ROLES", "grant_bot_manage_roles"
...     if role.get("pos", 1 << 30) >= i.get("bot_pos", 0):
...         return "ROLE_HIERARCHY", "raise_bot_role"
...     if i.get("role_bound", False):
...         return "ROLE_IN_USE", "wait_or_use_other_role"
...     if i.get("draining", False):
...         return "ROLE_DRAINING", "wait_for_drain"
...     if not role.get("mentionable", False):
...         return "ROLE_NOT_MENTIONABLE", "make_role_mentionable"
...     return "OK", "confirm_apply"
>>> def grant(i):
...     # returns (decision, reason, rest_writes); every input is read fresh
...     if i.get("rsvp") != "going":
...         return "refuse", "NOT_GOING", 0
...     if not i.get("opt_in"):
...         return "refuse", "NO_OPT_IN", 0
...     if i.get("binding") != "active":
...         return "refuse", "BINDING_INACTIVE", 0
...     if i.get("expired"):
...         return "refuse", "EXPIRED", 0
...     if i.get("member") != "present":
...         return "refuse", "MEMBER_UNAVAILABLE", 0
...     rs = i.get("role_state", "unknown")
...     if rs != "safe":
...         return "refuse", "ROLE_" + str(rs).upper(), 0
...     if i.get("held"):
...         return "noop", "ALREADY_HELD", 0
...     return "grant", "GRANTED", 1
>>> def sweep(i):
...     # revoke-only; pending_revoke retries; a pending grant finishes only while fresh and safe
...     if i.get("state") not in ("granted", "pending_grant", "pending_revoke"):
...         return "keep", "INERT_STATE", 0
...     if i.get("member") == "left":
...         return "mark", "MEMBER_LEFT", 0
...     if i["state"] == "granted" and i.get("held") is False:
...         return "mark", "EXTERNALLY_REMOVED", 0
...     if i["state"] == "pending_revoke":
...         if i.get("held") is False:
...             return "mark", "REVOKE_CONFIRMED", 0
...         if i.get("bot_can_revoke", True) is False:
...             return "keep", "REVOKE_BLOCKED", 0
...         return "revoke", "RETRY_REVOKE", 1
...     if (i.get("binding") != "active" or i.get("rsvp") != "going"
...             or not i.get("opt_in", False) or i.get("expired")
...             or i.get("role_state", "unknown") != "safe"):
...         if i["state"] == "granted":
...             if i.get("bot_can_revoke", True) is False:
...                 return "keep", "REVOKE_BLOCKED", 0
...             return "revoke", "NO_LONGER_DESIRED", 1
...         if i.get("held"):
...             if i.get("bot_can_revoke", True) is False:
...                 return "keep", "REVOKE_BLOCKED", 0
...             return "revoke", "REVOKE_UNCONFIRMED_GRANT", 1
...         return "mark", "EXPIRED_UNCONFIRMED", 0
...     if i["state"] == "pending_grant":
...         # still desired here; finish only while fresh, present and safe
...         if (i.get("age_min", 0) <= 15 and i.get("member") == "present"
...                 and i.get("role_state") == "safe"):
...             return "grant", "FINISH_INTENT", 1
...         if i.get("held"):
...             if i.get("bot_can_revoke", True) is False:
...                 return "keep", "REVOKE_BLOCKED", 0
...             return "revoke", "REVOKE_UNCONFIRMED_GRANT", 1
...         return "mark", "EXPIRED_UNCONFIRMED", 0
...     if i.get("member") != "present":
...         # granted row, presence unreadable: keep for retry, never assert desired
...         return "keep", "PRESENCE_UNKNOWN", 0
...     return "keep", "DESIRED", 0
>>> def status(rows):
...     # staff-visible revoke health: blocked count plus 1 h overdue alert
...     blocked = [r for r in rows if r.get("revoke_blocked")]
...     return len(blocked), any(r.get("overdue_min", 0) > 60 for r in blocked)
>>> TERMINAL = ("revoked", "member_left", "externally_removed",
...             "expired_unconfirmed")
>>> def drained(rows):
...     # bound-or-draining release: every row must be terminal; unknown states block
...     return all(r.get("state") in TERMINAL for r in rows)
>>> pre = [
...     ("ok", OK_INPUT, "OK"),
...     ("missing_evidence", {}, "UNAVAILABLE"),
...     ("no_manage_events", dict(OK_INPUT, staff=False), "MISSING_MANAGE_EVENTS"),
...     ("bot_no_manage_roles", dict(OK_INPUT, bot_manage_roles=False), "BOT_MISSING_MANAGE_ROLES"),
...     ("hierarchy", dict(OK_INPUT, bot_pos=3), "ROLE_HIERARCHY"),
...     ("role_perms", dict(OK_INPUT, role=dict(SAFE, perms=8)), "ROLE_HAS_PERMISSIONS"),
...     ("managed", dict(OK_INPUT, role=dict(SAFE, managed=True)), "ROLE_UNSAFE"),
...     ("everyone", dict(OK_INPUT, role=dict(SAFE, everyone=True)), "ROLE_UNSAFE"),
...     ("role_deleted", dict(OK_INPUT, role=None), "ROLE_MISSING"),
...     ("role_overwrites", dict(OK_INPUT, role=dict(SAFE, overwrites=True)), "ROLE_CHANNEL_OVERWRITE"),
...     ("role_hoisted", dict(OK_INPUT, role=dict(SAFE, hoist=True)), "ROLE_HOISTED"),
...     ("role_referenced", dict(OK_INPUT, role=dict(SAFE, referenced=True)), "ROLE_REFERENCED"),
...     ("role_in_use", dict(OK_INPUT, role_bound=True), "ROLE_IN_USE"),
...     ("role_draining", dict(OK_INPUT, draining=True), "ROLE_DRAINING"),
...     ("event_bound", dict(OK_INPUT, event_bound=True), "EVENT_ALREADY_BOUND"),
...     ("event_cancelled", dict(OK_INPUT, event="cancelled"), "EVENT_NOT_ELIGIBLE"),
...     ("not_allowlisted", dict(OK_INPUT, allowlisted=False), "ROLE_NOT_ALLOWED"),
...     ("not_mentionable", dict(OK_INPUT, role=dict(SAFE, mentionable=False)), "ROLE_NOT_MENTIONABLE"),
...     ("foreign_guild", dict(OK_INPUT, guild_ok=False), "UNAVAILABLE"),
...     ("announcements_disabled", dict(OK_INPUT, announcements=False), "ANNOUNCEMENTS_DISABLED"),
...     ("member_not_present", dict(OK_INPUT, member="left"), "MEMBER_UNAVAILABLE"),
... ]
>>> for name, case, want in pre:
...     assert preflight(case)[0] == want, name
>>> assert all(preflight(c)[1] for _, c, _ in pre)
>>> GOOD = dict(rsvp="going", opt_in=True, binding="active", member="present",
...             role_state="safe")
>>> GOOD_NOROLE = dict((k, v) for k, v in GOOD.items() if k != "role_state")
>>> gr = [
...     ("grant", GOOD, ("grant", "GRANTED", 1)),
...     ("duplicate", dict(GOOD, held=True), ("noop", "ALREADY_HELD", 0)),
...     ("interested_only", dict(GOOD, rsvp="interested"), ("refuse", "NOT_GOING", 0)),
...     ("no_opt_in", dict(GOOD, opt_in=False), ("refuse", "NO_OPT_IN", 0)),
...     ("voice_presence", dict(GOOD, rsvp=None, opt_in=False), ("refuse", "NOT_GOING", 0)),
...     ("concurrent_declined", dict(GOOD, rsvp="declined"), ("refuse", "NOT_GOING", 0)),
...     ("member_left", dict(GOOD, member="left"), ("refuse", "MEMBER_UNAVAILABLE", 0)),
...     ("canceled", dict(GOOD, binding="ended"), ("refuse", "BINDING_INACTIVE", 0)),
...     ("expired", dict(GOOD, expired=True), ("refuse", "EXPIRED", 0)),
...     ("role_deleted", dict(GOOD, role_state="missing"), ("refuse", "ROLE_MISSING", 0)),
...     ("role_changed", dict(GOOD, role_state="changed"), ("refuse", "ROLE_CHANGED", 0)),
...     ("grant_role_evidence_missing", GOOD_NOROLE, ("refuse", "ROLE_UNKNOWN", 0)),
... ]
>>> for name, case, want in gr:
...     assert grant(case) == want, name
>>> assert all(w == 0 for _, _, (d, _, w) in gr if d != "grant")
>>> ST = dict(state="granted", binding="active", rsvp="going", member="present",
...           held=True, opt_in=True, role_state="safe")
>>> ST_NOROLE = dict((k, v) for k, v in ST.items() if k != "role_state")
>>> sw = [
...     ("desired", ST, ("keep", "DESIRED", 0)),
...     ("declined", dict(ST, rsvp="declined"), ("revoke", "NO_LONGER_DESIRED", 1)),
...     ("canceled", dict(ST, binding="ended"), ("revoke", "NO_LONGER_DESIRED", 1)),
...     ("hard_stop", dict(ST, expired=True), ("revoke", "NO_LONGER_DESIRED", 1)),
...     ("rescheduled", dict(ST), ("keep", "DESIRED", 0)),
...     ("member_left", dict(ST, member="left"), ("mark", "MEMBER_LEFT", 0)),
...     ("manual_removal", dict(ST, held=False), ("mark", "EXTERNALLY_REMOVED", 0)),
...     ("optout_revokes", dict(ST, opt_in=False), ("revoke", "NO_LONGER_DESIRED", 1)),
...     ("suspended_revokes", dict(ST, binding="suspended"), ("revoke", "NO_LONGER_DESIRED", 1)),
...     ("role_unsafe_revokes", dict(ST, role_state="changed"), ("revoke", "NO_LONGER_DESIRED", 1)),
...     ("granted_blocked", dict(ST, rsvp="declined", bot_can_revoke=False), ("keep", "REVOKE_BLOCKED", 0)),
...     ("restart_fresh", dict(ST, state="pending_grant", age_min=3), ("grant", "FINISH_INTENT", 1)),
...     ("restart_stale", dict(ST, state="pending_grant", age_min=90), ("revoke", "REVOKE_UNCONFIRMED_GRANT", 1)),
...     ("restart_stale_unheld", dict(ST, state="pending_grant", age_min=90, held=False), ("mark", "EXPIRED_UNCONFIRMED", 0)),
...     ("pending_declined", dict(ST, state="pending_grant", rsvp="declined"), ("revoke", "REVOKE_UNCONFIRMED_GRANT", 1)),
...     ("pending_declined_unheld", dict(ST, state="pending_grant", rsvp="declined", held=False), ("mark", "EXPIRED_UNCONFIRMED", 0)),
...     ("pending_declined_blocked", dict(ST, state="pending_grant", rsvp="declined", bot_can_revoke=False), ("keep", "REVOKE_BLOCKED", 0)),
...     ("unsafe_finish_refused", dict(ST, state="pending_grant", age_min=3, role_state="changed", held=False), ("mark", "EXPIRED_UNCONFIRMED", 0)),
...     ("unknown_member_revokes", dict(ST, state="pending_grant", age_min=3, member="unknown"), ("revoke", "REVOKE_UNCONFIRMED_GRANT", 1)),
...     ("pending_revoke_retry", dict(ST, state="pending_revoke"), ("revoke", "RETRY_REVOKE", 1)),
...     ("pending_revoke_confirmed", dict(ST, state="pending_revoke", held=False), ("mark", "REVOKE_CONFIRMED", 0)),
...     ("pending_revoke_blocked", dict(ST, state="pending_revoke", bot_can_revoke=False), ("keep", "REVOKE_BLOCKED", 0)),
...     ("future_state", dict(ST, state="v9_quarantine"), ("keep", "INERT_STATE", 0)),
...     ("unknown_member_retry", dict(ST, member="unknown"), ("keep", "PRESENCE_UNKNOWN", 0)),
...     ("sweep_role_evidence_missing", ST_NOROLE, ("revoke", "NO_LONGER_DESIRED", 1)),
... ]
>>> for name, case, want in sw:
...     assert sweep(case) == want, name
>>> assert all(d != "grant" or r == "FINISH_INTENT" for _, _, (d, r, _) in sw)
>>> bl = [
...     ("empty", [], (0, False)),
...     ("blocked_no_alert", [{"revoke_blocked": True, "overdue_min": 10}], (1, False)),
...     ("blocked_alert", [{"revoke_blocked": True, "overdue_min": 61}], (1, True)),
... ]
>>> for name, rows, want in bl:
...     assert status(rows) == want, name
>>> dr = [
...     ("empty", [], True),
...     ("terminal_only", [{"state": "revoked"}, {"state": "expired_unconfirmed"}], True),
...     ("granted_blocks", [{"state": "granted"}], False),
...     ("pending_grant_blocks", [{"state": "pending_grant"}], False),
...     ("pending_revoke_blocks", [{"state": "pending_revoke"}], False),
...     ("future_state_blocks", [{"state": "v9_quarantine"}], False),
...     ("missing_state_blocks", [{}], False),
... ]
>>> for name, rows, want in dr:
...     assert drained(rows) == want, name
>>> names = ([n for n, _, _ in pre] + [n for n, _, _ in gr] + [n for n, _, _ in sw]
...          + [n for n, _, _ in bl] + [n for n, _, _ in dr])
>>> assert len(pre) + len(gr) + len(sw) + len(bl) + len(dr) == 68
>>> assert 5 * 1000 == 5000 and 24 * 60 > 15
>>> # 1200 s: one full event draining alone; 6000 s: any single event under a full ledger
>>> assert 1000 // 50 * 60 == 1200 and 5000 // 50 * 60 == 6000
>>> print(json.dumps({"schema": "events.attendee_role.oracle.v1", "fixtures": len(names), "status": "pass"}, sort_keys=True))
{"fixtures": 68, "schema": "events.attendee_role.oracle.v1", "status": "pass"}

```

Fixtures the reference model cannot express need future protected acceptance
tests with barriers, an injected clock and REST fault injection against the
real callers in disposable fixtures: concurrent bind/bind interleavings (the
per-(guild,event) lock plus the event-side unique predicate), concurrent
RSVP/leave interleavings (before-REST and after-REST re-read), duplicate
delivery of one click, inline-withdraw versus sweep races, the per-guild
isolation of locks, and rollback to an older binary. Zero tests is not proof;
none are created here.

## 8. Acceptance boundary, review metric and undo

Record CEO spec acceptance **and** host intent alignment against the exact
revision, and an independent CISO disposition of the role-grant boundary,
before any build card, protected test handoff or pilot. A queue/intake receipt
is not alignment, and no request goes to the owner or an invented executor.
The decision must accept or revise: option B with the allowlist, zero-permission
role, explicit opt-in with disclosure, sweep-only-revokes, eventual revoke
bounded by the mirror poll plus the stated drain for executable revokes
(blocked rows surface via status and alert, §3), inline consent withdrawal,
and no DM, ping, role creation or retroactive grant.
Any material revision rebinds review to the new revision.

By 2026-10-31 the metric is **all 68 fixtures have agreed or explicitly
rejected outcomes and the narrowing conditions and review findings have written
disposition** (see §6 and the PR thread). This measures design completeness,
not demand. Keep this proposal only if grants are provably scoped, revocable
and reconciled; otherwise recommend option A (manual role, no build). Default
while unresolved is no build; there is no timeout auto-approval. A demand
experiment needs its own hypothesis, metric and date; none is run here.

ACT: publish this proposed contract and its offline oracle. WHY: bound a
security-sensitive role boundary before considering any build. UNDO: revert the
docs; no operational change exists to undo. A later accepted build must ship
dark, and disabling it must keep the revoke sweep and erasure behavior rather
than delete history. No production tests, migrations, credentials or role changes.
