# Events: an opt-in per-event attendee role

Status: **proposed, design only**. Written 2026-10-24; review by 2026-10-31.
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
| B. Static role pool: admin pre-creates and allowlists up to 5 inert roles; staff bind one to one upcoming event; the bot only adds or removes members | Smallest boundary: no role create/edit/delete, no permission change; a pool of one is a valid start; role count caps concurrent events | Smallest option that tests the hypothesis; recommend for **spec review**, not build |
| C. Bot-managed role per event: the bot creates, grants and deletes `event-*` roles | No admin pre-work, but needs Manage Roles to create and delete, event-name-derived names (injection, 100-char limit), the 250-role guild cap, orphan deletion, and a permission-bearing create path | Reject: larger blast radius for the same member value |

Benefit if B works: per-event mention of confirmed attendees with no staff
upkeep. Uncertainty: demand, whether native interest suffices, and whether
members accept a **publicly visible** attendance marker (§3). "One static role
reused by every event" is rejected: mentioning it would ping other events'
attendees, so B binds a role to at most one active event at a time.

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
  time**. No scheduled-event gateway handler exists, and no role-event handler
  feeds RSVP (member add/remove/update are in `crates/bot/src/onboarding.rs:320-334`;
  `crates/bot/src/containment_runtime.rs:76` classifies audit-log role deletes
  for destructive-action detection only). Cancellation and external role
  deletion are therefore seen by poll or fresh fetch.
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
ephemeral **Get event role** control in their own `/rsvp` Going reply, which
states that other members will see the role; (4) the fresh preflight in §4
passes. Never inferred from Interested, voice presence, message activity,
native event interest, or an RSVP made before enabling. Existing Going members
see the control the next time they run `/rsvp status:going`; there are no
retroactive grants. A custom ID grants no authority: every click re-resolves
actor, binding, RSVP and live state. *Alternative for review:* Going alone
grants. Recommended against because of the disclosure finding above.

**Revoke triggers.** RSVP becomes Interested or Declined or its row is
compensated away; the member clicks **Remove event role** (RSVP stays Going);
the mirror shows the event Canceled, Completed or deleted; staff unbind;
`starts_at + 24 h` passes (hard stop; the mirror has no end time); the binding
is suspended (§5). Revoke never requires the safety predicates, only live Manage
Roles and a role below the bot. A member who left the guild needs no REST call.

**Bounds.** At most one role per event and one active event per role. Pool
cap 5 roles per guild, so at most 5 active bindings and 5,000 ledger rows
(1,000 grants/event, the RSVP cap). A role is eligible only if it exists, is not
@everyone or managed, has `permissions == 0`, appears in no channel overwrite,
sits below the bot's highest role, and is mentionable at enable time. The bot
never creates, renames, edits or deletes any role, never changes mentionability,
and never touches a role outside the allowlist or any member role it did not
grant. Pre-existing manual holders of the role are unknown and are never
removed; use a dedicated unused role.

**Stale-grant cleanup.** A level-triggered sweep, at most 50 ledger rows per
tick, computes desired state `binding active ∧ RSVP Going ∧ opted in ∧ member
present ∧ before expiry` against fresh member and role reads. The sweep **only
revokes**. It never grants and never re-grants a role a member or staff removed
(`externally_removed` is terminal until a new explicit click). The one exception
is finishing a `pending_grant` intent recorded within the last 15 minutes while
every condition still holds; older intents become `expired_unconfirmed`.

## 4. Authorization, preflight and write path

- **Allowlist** (`allow`/`disallow` a role): Manage Server, as onboarding admin
  commands use. **Bind/unbind**: freshly evaluated Manage Events on the
  invoking interaction plus a matching mirror event, choosing only from the
  allowlist. Event creator or Manage Roles alone is not authority. **Member
  controls**: the authenticated member acts only on their own row.
- Gate order: configured guild from interaction context, Announcements enabled,
  current human membership (live), then the check list below. Unknown evidence
  fails closed. Foreign guild, missing and denied collapse into one reply.
- **Preflight** returns the first failure with an actionable correction:
  `MISSING_MANAGE_EVENTS`, `EVENT_NOT_ELIGIBLE` (only Scheduled may be bound),
  `EVENT_ALREADY_BOUND`, `ROLE_NOT_ALLOWED`, `ROLE_MISSING`, `ROLE_UNSAFE`
  (@everyone/managed), `ROLE_HAS_PERMISSIONS`, `BOT_MISSING_MANAGE_ROLES`,
  `ROLE_HIERARCHY` (raise the bot's role above it), `ROLE_IN_USE`,
  `ROLE_NOT_MENTIONABLE`. Bind runs it once; every grant re-runs the safety
  checks on a fresh snapshot, because a role can gain permissions after binding.
- **Write path** (proposed ledger `event_role_bindings` and `event_role_grants`,
  one row per member per event, states `pending_grant`, `granted`,
  `pending_revoke`, `revoked`, `member_left`, `externally_removed`,
  `expired_unconfirmed`): take the per-`(guild,event,member)` advisory lock;
  re-read the RSVP; write `pending_grant` plus the write-ahead audit row; one
  `add_guild_member_role`; settle `granted`; after the REST call re-read the RSVP
  and, if no longer Going, revoke (the committed row, not REST completion order,
  decides concurrent RSVP/leave). Unique `(guild_id, role_id)` where active
  enforces one event per role. An unknown future state is inert: never
  granted, never revoked, never deleted.
- Guild isolation: every key, lock and query carries `guild_id`; a role or event
  from another guild is `UNAVAILABLE`.

## 5. Lifecycle, reconciliation and privacy

| Condition | Behavior |
| --- | --- |
| Event rescheduled | Binding follows the event ID; expiry recomputed from the mirror; no grant, revoke or message |
| Event canceled/completed | Revoke all `granted` rows within the sweep; worst-case latency is one 10-minute mirror poll plus sweep ticks, disclosed to staff |
| Role deleted externally | Binding becomes `SUSPENDED_ROLE_MISSING`; no recreation, no grant; staff see it in status |
| Role gains permissions | Binding `SUSPENDED_ROLE_CHANGED`; no new grants; revoke continues |
| Member left guild | Row becomes `member_left`; no REST call |
| Duplicate click | Row already `granted`: no REST, no second audit |
| Restart / rolling deploy | The advisory lock serializes overlap; `pending_*` rows are re-resolved against fresh reads |
| Version skew / rollback | An older binary treats unknown states as inert; disabling the feature stops new grants and keeps the revoke sweep and ledger |
| Erasure | Add a `(guild_id,user_id)` plan entry; the bot first tries to revoke a present member, otherwise reports `role_revoke_unconfirmed`; a leftover is only a role staff can remove |

Audit facts (`operational_audit_log`, no event title, name or message text):
`event_role.bind|unbind|grant|revoke|suspend` with guild, actor, event ID, role
ID, member ID and a `cause`. Ledger rows are deleted 90 days after a terminal
state; audit follows the existing 365-day retention.

Member copy (ephemeral): grant `You now have <@&role> until the event ends.
Other members can see this role.`; revoke `Removed. You are still Going.`
Staff enable copy states the role, event, bounds, that existing Going members
are not granted, and that the bot sends no ping, DM or channel message. The
bot never mentions the role itself. Staff may mention it as they do today.

## 6. Security findings, CISO assessment and boundary

| Threat | Proposed control and remaining finding |
| --- | --- |
| Privilege escalation through the role | Zero permissions, no overwrites, below the bot, fresh re-check per grant; an admin who later empowers the role is caught only at the next grant or sweep |
| Staff binding an arbitrary role | Manage Events staff choose only admin-allowlisted roles; allowlisting needs Manage Server |
| Attendance disclosure | Separate explicit opt-in with disclosure; role visibility is a new public marker the bot cannot hide |
| Bot touching unrelated roles/members | Only allowlisted roles and only rows the bot granted; manual holders untouched |
| Grant after consent withdrawn | Locked re-read before and after REST; compensating revoke |
| Stale cancel (up to 10 minutes) | Disclosed; mention of a canceled event's role may still reach holders |
| Cross-guild leakage | Guild-keyed rows and locks; foreign IDs collapse to `UNAVAILABLE` |

**CISO proposal assessment:** acceptable to circulate for intent review with
these controls; **not** a grant, independent approval, or runtime acceptance.
Blocking findings before any build: (1) accept or narrow the role-grant
boundary: allowlisted, zero-permission, opt-in, bot adds and removes only;
(2) accept the public attendance marker or require the opt-in control;
(3) accept eventual revoke bounded by the mirror poll, or stop. If a build
needs role creation, a permission-bearing role, retroactive grants, or
sweep-side granting, stop this proposal. No credential or permission is
requested here.

## 7. Agent-facing contract and offline fixtures

No executable CLI/API is registered. These are **proposed typed results**, not
commands for a guild. Envelope: `schema=events.attendee_role.v1`, `guild_id`,
`event_id`, `role_id`, `mode` (`preview`|`apply`), `binding_state`, `checks`
(ordered `{id, pass}`), `bounds` (pool 5, 1,000 grants/event, 24 h hard stop),
`reason`, `retryable`, `next_action`. `preview` is the default and performs no
Discord or database write; `apply` is a separate explicit call. One repo source
(this file) and one runnable check (below). No raw SQL or Discord errors appear.

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
>>> SAFE = dict(managed=False, everyone=False, perms=0, pos=3, mentionable=True)
>>> def preflight(i):
...     if not i.get("guild_ok", True):
...         return "UNAVAILABLE", "verify_access"
...     if not i.get("staff", True):
...         return "MISSING_MANAGE_EVENTS", "ask_staff_with_manage_events"
...     if i.get("event", "scheduled") != "scheduled":
...         return "EVENT_NOT_ELIGIBLE", "pick_scheduled_event"
...     if i.get("event_bound"):
...         return "EVENT_ALREADY_BOUND", "unbind_first"
...     if not i.get("allowlisted", True):
...         return "ROLE_NOT_ALLOWED", "ask_admin_to_allow_role"
...     role = i.get("role", SAFE)
...     if role is None:
...         return "ROLE_MISSING", "pick_existing_role"
...     if role["everyone"] or role["managed"]:
...         return "ROLE_UNSAFE", "pick_ordinary_role"
...     if role["perms"] != 0:
...         return "ROLE_HAS_PERMISSIONS", "use_zero_permission_role"
...     if not i.get("bot_manage_roles", True):
...         return "BOT_MISSING_MANAGE_ROLES", "grant_bot_manage_roles"
...     if role["pos"] >= i.get("bot_pos", 9):
...         return "ROLE_HIERARCHY", "raise_bot_role"
...     if i.get("role_bound"):
...         return "ROLE_IN_USE", "wait_or_use_other_role"
...     if not role["mentionable"]:
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
...     if i.get("member") != "present":
...         return "refuse", "MEMBER_UNAVAILABLE", 0
...     if i.get("role_state", "safe") != "safe":
...         return "refuse", "ROLE_" + i["role_state"].upper(), 0
...     if i.get("held"):
...         return "noop", "ALREADY_HELD", 0
...     return "grant", "GRANTED", 1
>>> def sweep(i):
...     # revoke-only; a pending intent may finish only while fresh and valid
...     if i.get("state") not in ("granted", "pending_grant"):
...         return "keep", "INERT_STATE", 0
...     if i.get("member") == "left":
...         return "mark", "MEMBER_LEFT", 0
...     if i["state"] == "granted" and i.get("held") is False:
...         return "mark", "EXTERNALLY_REMOVED", 0
...     if i.get("binding") != "active" or i.get("rsvp") != "going" or i.get("expired"):
...         return ("revoke", "NO_LONGER_DESIRED", 1) if i["state"] == "granted" else ("mark", "EXPIRED_UNCONFIRMED", 0)
...     if i["state"] == "pending_grant":
...         if i.get("age_min", 0) <= 15 and i.get("opt_in"):
...             return "grant", "FINISH_INTENT", 1
...         return "mark", "EXPIRED_UNCONFIRMED", 0
...     return "keep", "DESIRED", 0
>>> pre = [
...     ("ok", {}, "OK"),
...     ("no_manage_events", dict(staff=False), "MISSING_MANAGE_EVENTS"),
...     ("bot_no_manage_roles", dict(bot_manage_roles=False), "BOT_MISSING_MANAGE_ROLES"),
...     ("hierarchy", dict(bot_pos=3), "ROLE_HIERARCHY"),
...     ("role_perms", dict(role=dict(SAFE, perms=8)), "ROLE_HAS_PERMISSIONS"),
...     ("managed", dict(role=dict(SAFE, managed=True)), "ROLE_UNSAFE"),
...     ("everyone", dict(role=dict(SAFE, everyone=True)), "ROLE_UNSAFE"),
...     ("role_deleted", dict(role=None), "ROLE_MISSING"),
...     ("role_in_use", dict(role_bound=True), "ROLE_IN_USE"),
...     ("event_bound", dict(event_bound=True), "EVENT_ALREADY_BOUND"),
...     ("event_canceled", dict(event="canceled"), "EVENT_NOT_ELIGIBLE"),
...     ("not_allowlisted", dict(allowlisted=False), "ROLE_NOT_ALLOWED"),
...     ("not_mentionable", dict(role=dict(SAFE, mentionable=False)), "ROLE_NOT_MENTIONABLE"),
...     ("foreign_guild", dict(guild_ok=False), "UNAVAILABLE"),
... ]
>>> for name, case, want in pre:
...     assert preflight(case)[0] == want, name
>>> assert all(preflight(c)[1] for _, c, _ in pre)
>>> GOOD = dict(rsvp="going", opt_in=True, binding="active", member="present")
>>> gr = [
...     ("grant", GOOD, ("grant", "GRANTED", 1)),
...     ("duplicate", dict(GOOD, held=True), ("noop", "ALREADY_HELD", 0)),
...     ("interested_only", dict(GOOD, rsvp="interested"), ("refuse", "NOT_GOING", 0)),
...     ("no_opt_in", dict(GOOD, opt_in=False), ("refuse", "NO_OPT_IN", 0)),
...     ("voice_presence", dict(GOOD, rsvp=None, opt_in=False), ("refuse", "NOT_GOING", 0)),
...     ("concurrent_declined", dict(GOOD, rsvp="declined"), ("refuse", "NOT_GOING", 0)),
...     ("member_left", dict(GOOD, member="left"), ("refuse", "MEMBER_UNAVAILABLE", 0)),
...     ("canceled", dict(GOOD, binding="ended"), ("refuse", "BINDING_INACTIVE", 0)),
...     ("role_deleted", dict(GOOD, role_state="missing"), ("refuse", "ROLE_MISSING", 0)),
...     ("role_changed", dict(GOOD, role_state="changed"), ("refuse", "ROLE_CHANGED", 0)),
... ]
>>> for name, case, want in gr:
...     assert grant(case) == want, name
>>> assert all(w == 0 for _, _, (d, _, w) in gr if d != "grant")
>>> ST = dict(state="granted", binding="active", rsvp="going", member="present", held=True)
>>> sw = [
...     ("desired", ST, ("keep", "DESIRED", 0)),
...     ("declined", dict(ST, rsvp="declined"), ("revoke", "NO_LONGER_DESIRED", 1)),
...     ("canceled", dict(ST, binding="ended"), ("revoke", "NO_LONGER_DESIRED", 1)),
...     ("hard_stop", dict(ST, expired=True), ("revoke", "NO_LONGER_DESIRED", 1)),
...     ("rescheduled", dict(ST), ("keep", "DESIRED", 0)),
...     ("member_left", dict(ST, member="left"), ("mark", "MEMBER_LEFT", 0)),
...     ("manual_removal", dict(ST, held=False), ("mark", "EXTERNALLY_REMOVED", 0)),
...     ("restart_fresh", dict(ST, state="pending_grant", opt_in=True, age_min=3), ("grant", "FINISH_INTENT", 1)),
...     ("restart_stale", dict(ST, state="pending_grant", opt_in=True, age_min=90), ("mark", "EXPIRED_UNCONFIRMED", 0)),
...     ("pending_declined", dict(ST, state="pending_grant", rsvp="declined"), ("mark", "EXPIRED_UNCONFIRMED", 0)),
...     ("future_state", dict(ST, state="v9_quarantine"), ("keep", "INERT_STATE", 0)),
... ]
>>> for name, case, want in sw:
...     assert sweep(case) == want, name
>>> assert all(d != "grant" or r == "FINISH_INTENT" for _, _, (d, r, _) in sw)
>>> names = [n for n, _, _ in pre] + [n for n, _, _ in gr] + [n for n, _, _ in sw]
>>> assert len(pre) + len(gr) + len(sw) == 35
>>> assert 5 * 1000 == 5000 and 24 * 60 > 15
>>> print(json.dumps({"schema": "events.attendee_role.oracle.v1", "fixtures": len(names), "status": "pass"}, sort_keys=True))
{"fixtures": 35, "schema": "events.attendee_role.oracle.v1", "status": "pass"}

```

Fixtures the reference model cannot express need future protected acceptance
tests with barriers, an injected clock and REST fault injection against the
real callers in disposable fixtures: concurrent RSVP/leave interleavings
(before-REST and after-REST re-read), duplicate delivery of one click, a crash
between `pending_grant` and `granted`, the per-guild isolation of locks, and
rollback to an older binary. Zero tests is not proof; none are created here.

## 8. Acceptance boundary, review metric and undo

Record CEO spec acceptance **and** host intent alignment against the exact
revision, and an independent CISO disposition of the role-grant boundary,
before any build card, protected test handoff or pilot. A queue/intake receipt
is not alignment, and no request goes to the owner or an invented executor.
The decision must accept or revise: option B with the allowlist, zero-permission
role, explicit opt-in with disclosure, sweep-only-revokes, eventual revoke
bounded by the mirror poll, and no DM, ping, role creation or retroactive grant.
Any material revision rebinds review to the new revision.

By 2026-10-31 the metric is **all 35 fixtures have agreed or explicitly
rejected outcomes and the three blocking findings have written disposition**.
This measures design completeness, not demand. Keep this proposal only if
grants are provably scoped, revocable and reconciled; otherwise recommend
option A (manual role, no build). Default while unresolved is no build; there is
no timeout auto-approval. A demand experiment needs its own hypothesis, metric
and date; none is run here.

ACT: publish this proposed contract and its offline oracle. WHY: bound a
security-sensitive role boundary before considering any build. UNDO: revert the
docs; no operational change exists to undo. A later accepted build must ship
dark, and disabling it must keep the revoke sweep and erasure behavior rather
than delete history. No production tests, migrations, credentials or role changes.
