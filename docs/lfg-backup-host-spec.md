# LFG: explicit backup-host handoff for one game post

Status: **proposed, design only**. Written 2026-10-24; review by 2026-10-31.
No commands, storage, migrations, permissions, sends or runtime tests are added.
Merge publishes a proposal, not permission to implement or activate it. This file
is the single repository contract; review requests bind its commit and SHA-256.

## 1. Decision and evidence

Consumer: a game-night organizer who needs one other person to cover one session.
Hypothesis: one explicitly accepted, revocable per-post backup host avoids broad
server-wide Manage Events grants. No customer request is claimed.

[Apollo](https://docs.apollo.fyi/modifying_events) documents creators editing
their own events and Manage Server holders editing all; it does NOT document
co-host delegation at that page. Read 2026-10-24. Least-privilege backup hosting
is OUR product/security hypothesis, not a claimed vendor capability. TWO LFG
today requires Manage Events at both the builder (`feature_commands.rs:270-298`)
and the router/domain layers (`router.rs:666-675`, `lfg.rs:559-564`); any holder
can manage any LFG post in the guild, and the post creator holds no exception.

| Option | Value and cost | Disposition |
| --- | --- | --- |
| A. No build; current Manage Events policy | Zero new authority, storage or audit; covering a session still needs a guild-wide grant or an existing staffer | Standing default; retain regardless of review outcome |
| B. Handoff among already-authorized staff | Zero code: the covering staffer already holds Manage Events, so cover is a documented coordination step, not a grant; fails only when no second staffer exists | **Recommended supported answer**; specify as process, not a feature |
| C. Per-post accepted backup host | Materially narrower than a guild-wide grant (one post, close/metadata only, expiring, revocable), but a new bot-side authority invisible in Discord's permission UI, with storage, revocation, erasure and audit cost | Specified below to a reviewable boundary; **not approved for build**. Proceed only on accepted host intent plus an independent boundary disposition and a bounded storage/test plan |

C is not recommended now, with evidence: (1) the only post-management mutation
today is close (create/close/signup/leave; §2), so a delegation boundary would
currently delegate close-only — any Manage Events staffer already covers that;
(2) demand is unproven — no customer request, no vendor precedent; (3) the
authority would live only in bot state, so every use must re-prove identity and
revocation against races Discord never sees (§4-§5). If host review shows real
need for non-staffer cover, C's contract below is the bounded shape; otherwise
stop at B, not at a smaller unaccepted grant.

No Discord role/permission grant, moderation power, member signup mutation,
voice-room ownership transfer, calendar/ready-check/waitlist change, new spend
or live pilot. Never infer consent from signup or voice presence.

## 2. Verified baseline and missing seams

Inspected repository head: `0e904ee2f6cd17d5aa54fa35b2730154791ee1d3`.

- `lfg_posts.created_by` (`0170_lfg.sql:10-21`, `lfg.rs:314-326`) is a TEXT user
  ID set to the creator at creation (`lfg_interactions.rs:259`). It is record
  identity, **not authority**: `close_lfg` (`lfg_store.rs:380-402`) checks only
  guild fence + open status and never reads `created_by`. No owner field exists.
- Authority is Discord Manage Events only: builder defaults
  (`feature_commands.rs:268-296`), router refusals (`router.rs:666-675`),
  domain check (`lfg.rs:559-564`). Creator without it manages nothing; any
  holder manages any post. Preserve configured-guild, feature, privacy and
  application gates, and [wiring requirements](command-wiring-security.md).
- Post management today is create + close only. No `/lfg-edit` or `edit_lfg`
  exists in `crates/`; "metadata edit" delegation has no path to attach to.
- Post identity is `(guild_id, post_id)` plus channel and the accepted bot-owned
  `message_id` (`lfg_interactions.rs:249-342`). Unknown message ID means
  **unbound**: no invite, accept or delegated use, and no message-search bypass.
- `close_lfg`/`signup_lfg` serialize per post via `lfg:{guild}:{post}` advisory
  locks, but the [interaction contract](lfg-interaction-contract.md) explicitly
  provides no atomic mutation/audit inbox. Delegation writes would need a new
  serial seam, not an assumed transaction.
- `member_erasure_plan.json` covers `lfg_signups` (creator-or-member posts),
  `lfg_roles` (creator posts) and `lfg_posts` (`created_by`). [Privacy](privacy.md)
  covers operator erasure, queued deletion, guild purge and backup limits. New
  delegation rows would need explicit coverage in both deletion executors.

No live post, permission, deployment or host workflow was inspected. No sibling
LFG edit/list/standby feature is a dependency or an assumed capability. The
separate event-voice after-party handoff stays out of scope. If the seams below
cannot be small and coherent, stop at B instead of weakening existing fences.

## 3. Identity, authority and consent (specified C contract)

Post identity is `(guild_id, lfg_post_id)` plus the stored application/bot
author, channel and **existing accepted message ID**. Titles, start times and
caller-supplied links are not identity. Every delegated use rechecks the full
binding from authenticated context; missing/unknown evidence refuses.

"Host" means a current human guild member with fresh **Manage Events** and
access to that bound channel/post. Creator identity alone grants nothing.

- **Invite:** any current host may invite exactly one target per post: a human
  guild member who is not the inviter and does not already hold Manage Events
  (refuse redundant invites; the inviter can already perform every delegated
  op, so inviting escalates nothing at invite time). Invite is an explicit
  two-step action in the existing LFG management flow: private preview, then
  confirm against the same post revision. Preview writes nothing, shows exact
  post/delegation identity, expiry, allowed ops and refusals.
- **Accept/refuse:** explicit target-member action only, bound to the exact
  invite revision. Refusal and expiry are silent to everyone but the actor's own
  ephemeral acknowledgement. No auto-accept, no pre-consent, no inference.
- **Revoke:** any current host may revoke; the backup host may resign. Both are
  immediate, idempotent and privately acknowledged.
- **Single slot:** at most one `invited` or `active` delegation per post. A
  second invite while the slot is held refuses; a new invite needs a terminal
  slot. Revoke-wins on accept/revoke ties; accept binds the invite revision.
- **Loss of original authority:** the delegation does not depend on the
  inviter's continued Manage Events (avoids flapping); any current host can
  still revoke. Each delegated use re-proves the *backup host's* membership and
  channel access; unknown/stale proof refuses, and observed definitive
  departure ends the delegation.
- **Expiry/end:** invite expires at `min(invited_at + 24h, starts_at)`. An
  active delegation ends at `min(starts_at + 2h, accepted_at + 7d, post
  close/delete, revoke/resign)`, all in authoritative DB UTC. Post close/delete
  ends it atomically with that mutation. Staff metadata edits recompute expiry
  but do not end the delegation. Constants are proposed for host review.

Allowed-operation matrix (backup host, this post only):

| Operation | Verdict |
| --- | --- |
| Close this post | Allow; the one management mutation that exists |
| Edit this post's title/starts_at | Allow **only** if/when a post-edit path exists with the same binding checks; none exists, and none is proposed here |
| Change role slots/labels | Deny; capacity/label changes affect member signups |
| Add/remove/move any member signup | Deny |
| Any other post, any moderation, any Discord role/permission grant | Deny |
| Further delegation (invite another backup) | Deny |
| Voice-room ownership, message deletion outside the post lifecycle, audit writes | Deny |

Invitation delivery is pull, never push: the invite waits on a private surface
the target already opens (their own LFG interaction reply or post controls),
never a DM, ping, channel message or mention. The invite shows only post
title/starts_at plus the inviter's display name — no member history, signup
roster or other-post information. All actor acknowledgements are ephemeral and
mention-suppressed. No color/emoji-only state.

## 4. Lifecycle, serialization and replay

Proposed states: `none -> invited -> active -> ended`. Terminal reasons are
only `accepted_superseded | refused | invite_expired | revoked | resigned |
expired | post_closed | post_deleted | backup_departed | privacy_erasure`.
End is irreversible for that delegation; a later cover needs a fresh invite.

- Every invite/accept/refuse/revoke/resign/use/end serializes against the same
  delegation state. Required lock order: existing sorted erasure-compatible
  table locks, guild quota lock, existing post domain lock
  (`lfg:{guild}:{post}`), delegation row, member cursor. No HTTP inside that
  transaction. Close/delete, both erasure executors and any future edit writer
  must join the same seam; current independent store commits are insufficient.
- Preflight identity/access proof must be fresh (<=5 seconds), discarded on
  reconnect/restart or observed relevant invalidation. Commit rechecks post
  revision/binding, policy generation and expiry. Unknown proof refuses.
- Persist one bounded delegation row: post key, opaque delegation ID, revision,
  inviter/backup IDs (PII while live), state/reason, invite/accept/expiry/end
  times. One cursor row per `(delegation_id, member_id)`: latest authenticated
  interaction ID, canonical action fingerprint, constant outcome. No free text,
  interaction token, message body or history.
- Components are `two:lfg-backup:v1:<opaque-delegation-id>:1:<action>`
  (<=100 characters), actions `accept | refuse | resign` only. Invite/revoke
  stay in the host management flow. Parse strictly; look up the persisted
  guild/post binding and match actual application, bot author, channel and
  message. A component ID is **not a capability or secret**. The actor comes
  solely from the authenticated interaction; forged, foreign or stale bindings
  get generic unavailable with no IDs. No alternate HTTP/MCP write route or
  automation actor may invite, accept or use a delegation.
- Monotone authenticated interaction snowflakes order each member's actions:
  identical ID/fingerprint replays the stored outcome without a write;
  identical ID/different fingerprint refuses; older ID refuses. A late accept
  cannot resurrect a revoked invite; a late use cannot follow a committed end.
- Restart reloads saved state and expires overdue invites/delegations before
  admitting work; a callback never creates a missing delegation. A lost commit
  acknowledgement is `COMMIT_UNKNOWN`: inspect own status under the same
  binding first, never blind-reapply. Close-versus-use has two legal serial
  orders only: use-then-close, or close-then-refuse.
- Erasure of either the inviter or the backup host ends every live delegation
  in that guild naming them, inside the existing guarded erasure transaction,
  for both operator `erase_member` and queued `complete_request`. Three stores
  share one disposition, all included in dry-run counts and manifest coverage
  for both executors: (1) delegation rows clear inviter/backup subject fields
  and retain an identity-free spent marker (post/delegation/state/reason/
  times) with the parent post lifetime; parent erasure/guild purge removes it;
  (2) cursor rows `(delegation_id, member_id)` delete every row where
  `member_id` is the erased member, with no tombstone; (3) audit rows delete
  every row where actor or target is the erased member, with no
  retain-for-accountability exemption (unlike immutable `guild_settings_audit`,
  this audit is deletable). The existing `announcements_audit_log` manifest
  entry covers a different table/columns and does not carry this audit. Old
  snapshots may contain delegation/cursor/audit rows: before serving restored
  data, terminalize restored delegations, delete restored cursor rows naming
  erased members, and delete restored audit rows naming them.
- Audit invite/accept/refuse/revoke/resign/expire/end with actor/delegation/
  post IDs, bounded to the parent post lifetime — no indefinite copy. Erasure
  deletes audit rows naming the erased member (above); terminal clear does not
  otherwise delete audit for non-erased members. The
  current non-atomic LFG audit (§2) cannot carry this; atomic participation is
  a blocking gap (§5).

## 5. Bounds, retention and privacy/security findings

Proposed bounds tighten, never replace, existing LFG/admission limits: 10 open
delegations (invited + active)/guild; one live slot/post; 1,000 delegation +
cursor rows/guild; one in-flight execution using existing LFG queue/pool
bounds. At cap, refuse **new** invites, not refuse/revoke/resign/expiry or
erasure. Rate: at most one accepted delegation action/member/post/5 seconds;
replay, refusal and closure exempt. Sweep at most 10 overdue delegations/pass
on a one-minute healthy cadence; cleanup precedes new admission after restart.
Incremental spend target $0 on the existing process/database; unmeasured.

| Data / threat | Proposed control and remaining finding |
| --- | --- |
| Inviter/backup identities (PII) | Purpose is this one bounded delegation only; clear on terminal transaction; healthy overdue cleanup target <=1 minute, not a guaranteed physical TTL through outage. Reads refuse expired data regardless. No analytics, metrics label, trace, payload log or export |
| Invisible authority/confused deputy | Full binding + fresh proof on every use; staff UI must distinguish "backup host (one post)" from Manage Events; residual risk is a host mistaking delegation scope — host intent review must accept the matrix, not assume staff intuition |
| Replay/repudiation | One serial state/cursor transaction; close/edit/erasure writers must participate. Existing audit-only dedup is insufficient; no runtime approval until that gap is proved closed |
| Unsolicited contact/harassment | Pull-only delivery, no DM/ping/mention path, silent refusals, minimal invite disclosure. Repeated unwanted invites are rate-limited but not otherwise blocked; hosts can already contact members through existing channels |
| Erasure/restore | Same-guild end-and-clear inside both executors for delegation subject fields, cursor rows and audit rows naming the erased member (delete, no exemption); spent marker with post lifetime; restore terminalization plus cursor/audit deletion. No claim of backup destruction or Discord-side ephemeral erasure |
| Abuse/outage | Caps, existing lane admission, deny-by-default matrix. Database failure means unavailable/unknown, never an assumed grant or an assumed end |

**CISO proposal assessment:** acceptable to circulate for intent review with the
above controls; **not** a grant, independent approval of our own boundary
request, or runtime security acceptance. Blocking findings before any later
build/enable: (1) prove atomic writer/erasure/audit compatibility for the
delegation seam, (2) accept pull-only delivery, the close-only-today matrix and
the spent-marker lifetime, (3) confirm hosts genuinely need non-staffer cover —
without that, B answers the use case and C must stop. No credential or
permission is requested by this document. Later security-sensitive changes
still require independent applicable security disposition and exact-head tests.

## 6. Agent-facing result and offline fixture contract

No new executable CLI/API is registered. Future adapters must discover this
file through the [documentation map](README.md); these are **proposed typed
results**, not commands to run on a guild. Stable envelope:
`schema=lfg.backup.v1`, `post_id`, `delegation_id` (null only before
allocation), `revision`, `reason`, `retryable`, `next_action`; delegation
fields only after the relevant authorization. Denied replies omit identity
fields. No raw SQL/Discord errors. `retry_after_seconds` is bounded 1..300 when
retryable.

| reason | retryable / next_action |
| --- | --- |
| `PREVIEW` | false / `confirm_same_revision` |
| `INVITED`, `ACTIVE`, `UNCHANGED` | false / `view_own_status` |
| `ENDED`, `EXPIRED`, `ALREADY_USED` | false / `ask_authorized_staff` |
| `ALREADY_AUTHORIZED`, `FORBIDDEN` | false / `ask_authorized_staff` |
| `UNAVAILABLE` | false / `verify_access` (collapse missing/foreign/denied) |
| `STALE`, `CONFLICT` | false / `refresh_controls` |
| `BUSY`, `STORE_UNAVAILABLE` | true / `retry_after_backoff` |
| `COMMIT_UNKNOWN` | true / `inspect_own_status` |

Preview is typed read-only with no mutation handle; confirmation is a distinct
host-authorized operation rechecking revision/identity. This design's offline
check has no environment selector, credentials, network, database, Discord
client, side effects or wrong-flag production path. No staging/live pilot or
mutation is authorized. Future execution cannot be reached by combining flags.

The following deterministic **fixture oracle**, not an implementation, uses
fixed guild `g1`, post `p1`, message `m1`, host `h`, target `b`, outsider `c`,
invite at 0, `starts_at` 10000. Each row resets that fixture. All rows require
unchanged signups/roles and zero new messages/mentions/DMs/role grants.

| Case | Controlled event / expected assertion |
| --- | --- |
| no_authority_invite | `c` without Manage Events invites -> FORBIDDEN, no row |
| already_authorized | Invite a Manage Events holder -> ALREADY_AUTHORIZED, no row |
| invite | `h` previews + confirms -> INVITED, one invited slot |
| double_invite | Second invite while invited/active -> CONFLICT, slot unchanged |
| accept | `b` accepts exact revision -> ACTIVE |
| accept_replay | Replay same id/fingerprint -> UNCHANGED, no write |
| accept_after_revoke | Revoke then delayed accept -> ENDED, accept loses; late accept after resign likewise loses |
| revoke | Host revoke -> ENDED, idempotent repeat |
| resign | `b` resign -> ENDED, idempotent repeat |
| use_after_end | Delegated close after end -> ENDED, no mutation |
| foreign_guild | Foreign guild -> UNAVAILABLE, no row; shared ID cannot select a target actor |
| foreign_post | Foreign post -> UNAVAILABLE, no row |
| foreign_message | Foreign message binding -> UNAVAILABLE, no row |
| foreign_application | Foreign application -> UNAVAILABLE, no row |
| foreign_author | Foreign bot author -> UNAVAILABLE, no row |
| foreign_nonmember | Non-member actor -> UNAVAILABLE, no row |
| stale_revision | Accept/use at wrong revision -> STALE, state unchanged |
| prohibited_op | Backup moves a signup, invites another backup, or touches another post -> FORBIDDEN, state unchanged |
| close_ends | Post close ends delegation atomically; use-then-close or close-then-refuse only |
| expiry | Invite past `min(24h, starts)` or active past `min(starts+2h, accept+7d)` -> EXPIRED/ENDED; DB equality at deadline is expired |
| inviter_loses_authority | `h` loses Manage Events -> delegation persists ACTIVE; another host can still revoke; each use re-proves `b` |
| backup_departs | Observed definitive departure -> ENDED; unknown evidence refuses without ending |
| restart_expires | Restart expires overdue before admission; no callback creates a missing delegation |
| tampered_replay | Same id + altered action -> FORBIDDEN, state unchanged |
| erasure | Erase `h` or `b` -> live g1 delegations naming them end + subject/cursor/audit rows cleared per §4; g2 byte-identical; dry-run unchanged |
| store_outage | Failure before transaction -> STORE_UNAVAILABLE, no saved reply; never an assumed grant or end |

Runnable offline document check (Python stdlib; from repo root):

```sh
python3 -m doctest -v docs/lfg-backup-host-spec.md
python3 scripts/check_docs_index.py --json
```

The doctest checks the **specified oracle's consistency**, not real SQL locks,
authentication, rendering, erasure, outage handling or deployed behavior.
Future protected acceptance tests must implement the barriers/injected
clock/store/REST faults above against actual callers in disposable fixtures;
zero tests is not proof. No acceptance/implementation task is created by this
proposal.

```pycon
>>> import json, pathlib
>>> # (case, reason, invited, active, terminal); abstract expected snapshots
>>> oracle = [
...     ("no_authority_invite", "FORBIDDEN", 0, 0, False),
...     ("already_authorized", "ALREADY_AUTHORIZED", 0, 0, False),
...     ("invite", "INVITED", 1, 0, False),
...     ("double_invite", "CONFLICT", 1, 0, False),
...     ("accept", "ACTIVE", 0, 1, False),
...     ("accept_replay", "UNCHANGED", 0, 1, False),
...     ("accept_after_revoke", "ENDED", 0, 0, True),
...     ("revoke", "ENDED", 0, 0, True),
...     ("resign", "ENDED", 0, 0, True),
...     ("use_after_end", "ENDED", 0, 0, True),
...     ("foreign_guild", "UNAVAILABLE", 0, 0, False),
...     ("foreign_post", "UNAVAILABLE", 0, 0, False),
...     ("foreign_message", "UNAVAILABLE", 0, 0, False),
...     ("foreign_application", "UNAVAILABLE", 0, 0, False),
...     ("foreign_author", "UNAVAILABLE", 0, 0, False),
...     ("foreign_nonmember", "UNAVAILABLE", 0, 0, False),
...     ("stale_revision", "STALE", 0, 1, False),
...     ("prohibited_op", "FORBIDDEN", 0, 1, False),
...     ("close_ends", "ENDED", 0, 0, True),
...     ("expiry", "EXPIRED", 0, 0, True),
...     ("inviter_loses_authority", "ACTIVE", 0, 1, False),
...     ("backup_departs", "ENDED", 0, 0, True),
...     ("restart_expires", "EXPIRED", 0, 0, True),
...     ("tampered_replay", "FORBIDDEN", 0, 1, False),
...     ("erasure", "ENDED", 0, 0, True),
...     ("store_outage", "STORE_UNAVAILABLE", 0, 0, False),
... ]
>>> _lines = pathlib.Path("docs/lfg-backup-host-spec.md").read_text().splitlines()
>>> _start = next(i for i, l in enumerate(_lines) if l.startswith("| Case |"))
>>> _matrix = []
>>> for _l in _lines[_start + 2:]:
...     if not _l.startswith("| "):
...         break
...     _matrix.append(_l.split("|")[1].strip())
>>> assert set(_matrix) == {row[0] for row in oracle}, set(_matrix) ^ {row[0] for row in oracle}
>>> assert len({row[0] for row in oracle}) == len(oracle) == len(_matrix) == 26
>>> assert all(i + a <= 1 for _, _, i, a, _ in oracle)
>>> assert all((i, a) == (0, 0) for _, _, i, a, end in oracle if end)
>>> assert all(row[1:5] == ("UNAVAILABLE", 0, 0, 0) for row in oracle if row[0].startswith("foreign_"))
>>> assert sum(1 for row in oracle if row[0].startswith("foreign_")) == 6
>>> assert dict((name, i) for name, _, i, _, _ in oracle)["double_invite"] == 1
>>> assert dict((name, r) for name, r, _, _, _ in oracle)["inviter_loses_authority"] == "ACTIVE"
>>> assert dict((name, a) for name, _, _, a, _ in oracle)["tampered_replay"] == 1
>>> invited_at, starts_at, now = 0, 10000, 17200
>>> assert min(invited_at + 86400, starts_at) == 10000  # invite cannot outlive start
>>> assert min(starts_at + 7200, 0 + 604800) == 17200 and not now < 17200  # active ends at starts+2h
>>> print(json.dumps({"schema": "lfg.backup.oracle.v1", "fixtures": len(oracle), "status": "pass"}, sort_keys=True))
{"fixtures": 26, "schema": "lfg.backup.oracle.v1", "status": "pass"}

```

## 7. Acceptance boundary, review metric and undo

Record CEO spec acceptance **and** host intent alignment against the exact
revision before any implementation subtasks, protected test handoff or live
pilot. Route the decision brief through the President/COO to the CEO and the
existing host intent path; a queue/intake receipt is not alignment. The CISO
findings above must receive an independent boundary disposition, not a
self-approved grant. No request should be sent directly to the owner or an
invented host executor.

The decision must accept or revise: staff-handoff default, single-slot
invite/accept/revoke, pull-only delivery, close-only-today matrix, expiry
formula, inviter-independence with any-host revoke, serial seam with erasure
participation, cursor/audit delete-on-erasure in both executors with restore
coverage, and the parent-lifetime spent marker. No build timeline follows
acceptance. On any material revision, bind a fresh review request and alignment
to that revision.

By 2026-10-31, metric is **all 26 fixture-matrix rows have agreed or explicitly
rejected outcomes, the three blocking findings have written disposition, and
the B-vs-C choice is recorded with demand evidence or its absence**. This
measures design completeness, not engagement. Default while unresolved is B/A
(no build); no timeout auto-approval. A future demand experiment requires its
own hypothesis/metric/date, not inferred success from this document.

ACT: publish this proposed contract and its offline oracle for review. WHY:
bound authority/consent before considering a delegation feature. UNDO: reviewed
docs revert; there is no operational change to undo. A later independently
accepted build must keep admission dark until its own tests/security/release
gates pass; disabling future admission must preserve refusal/revoke/expiry/
erasure, not delete history or reset spent slots. No production tests,
migrations, credentials or activation here.
