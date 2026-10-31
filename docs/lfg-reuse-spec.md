# LFG: reuse a game night's title and roles, never its participation

Status: **proposed, design only**. Written 2026-10-24; review by 2026-10-31.
No commands, storage, migrations, permissions, sends or runtime tests are added.
Merge publishes a proposal, not permission to implement or activate it. This file
is the single repository contract; review requests bind its commit and SHA-256.

## 1. Decision and evidence

Consumer: an organizer who runs the same kind of game night repeatedly.
Hypothesis: re-entering the same title and role list for every `/lfg` is avoidable
setup, and avoiding it must not carry last session's people, time or consent. No
organizer request or measured setup cost is claimed.

Comparable vendor capability, read 2026-10-24, not validated TWO demand:
[sesh, January 2024](https://sesh.fyi/blog/january-2024-update/) lets organizers
"use existing events as a template" and "save event templates", unlimited for
Premium and three for non-Premium. [Apollo, February 2026](https://apollo.fyi/news/event-duplication)
duplicates an event into a draft "with everything carried over" before publishing,
a Premium feature. Both carry more than this proposal will; "everything" is
exactly what TWO must not copy.

| Option | Value and cost | Disposition |
| --- | --- | --- |
| A. No build; documented copy-paste | Zero code, storage or boundary. The organizer keeps the `title` and `roles` text of a good post (for example in a private note) and pastes it into `/lfg`, typing a fresh `starts-at`. Cost: the organizer maintains the note; the `roles` string is the only awkward part | **Recommended now**; also the standing default and fallback if review rejects B |
| B. Reuse from a post: read-only private preview of a source post's title and roles | Zero new storage or migration: reads existing `lfg_posts`/`lfg_roles`. Renders the fields as copy-ready `/lfg` inputs plus the organizer's fresh `starts-at`; **creates nothing**. The real create stays the unchanged `/lfg` | **Smallest build candidate**, spec review only; build only if review judges the benefit below worth a command |
| C. Named preset catalog | Names and ergonomic discovery, but new guild-scoped persisted data with quota, versioning, rename/delete, erasure and a new security boundary (§6) | Specified to a reviewable boundary; **not recommended now** |

Expected benefit, stated plainly. A needs three inputs per use (title, time,
roles), pasted from a note. B needs two to run the preview (source ID, time) and
then three pastes into `/lfg`, so it does **not** reduce the input count. Its
benefit is not keeping a note, finding the source by the ID already in a post's
footer, and typo-free exact role strings (up to 20 entries, a string of up to
2,339 characters; the short `tank:Tank:2,dps:DPS:6` is 21). That benefit is small
for short lists and unmeasured. **Uncertainty:** whether organizers repeat
setups often enough, whether they keep notes already, and whether a preview is
better than a note. No create-from-source command is proposed: it would add a
second create path whose preview/confirm step the current `/lfg` lacks, and it
is the only variant that would truly cut inputs. If review wants it, it is a new
spec, not a revision of this one.

Out of scope: copying signups or member identities, notification consent,
standby, ready checks, creator authority, message IDs, histories, replay fences
or source timestamps; automatic or recurring posts; cross-guild or legacy
import; DMs, notifications, calendar; spend; live pilots.

## 2. Verified baseline and missing seams

Inspected repository head: `89b26ae050ffba9fb409622ffd54d31cdc79e7fc`.

- `/lfg` takes three required options, `title` (max 100), `starts-at` and `roles`
  (`crates/core/src/feature_commands.rs:270-289`), gated by Manage Events.
  There is **no create preview**: `/lfg` validates and posts in one step
  (`crates/discord/src/lfg_interactions.rs:241-260`).
- Validation to preserve verbatim: title trimmed with ECMAScript whitespace, 1-100
  UTF-16 units (`crates/core/src/lfg.rs:222`); `starts-at` RFC-3339 and strictly
  future (`lfg.rs:247`); roles at most 20, key `[a-z0-9_-]{1,32}` and not
  `__leave__`, label 1-80 units, slots 1-99, duplicate keys refused
  (`lfg.rs:36-46,110-160`). Labels cannot contain `,` or `:`, because the spec
  splits on both, so a stored label with either did not come from the parser.
- Open posts are capped (`OpenLfgPosts`, 20, checked atomically at insert in
  `crates/core/src/lfg_store.rs:60-85`). Posts render with
  `allowed_mentions: {parse: []}` (`lfg_interactions.rs:718`).
- Source data already exists: `lfg_posts(id, guild_id, channel_id, message_id,
  title, starts_at, status, created_by, created_at, closed_at)` and
  `lfg_roles(lfg_id, role_key, label, slots, position)`
  (`crates/cutover/migrations/0170_lfg.sql:10-31`). Posts and original signups are
  retained, never deleted by the sweep (`docs/lfg-port.md`, bounded lifecycle).
  The post ID is in the member-visible footer for `/lfg-close`.
- Erasure removes posts where `created_by` is the erased member, with their roles
  and signups (`crates/cutover/src/member_erasure_plan.json:18-31`). A source
  vanishes with its organizer; nothing else retains it.
- No `/lfg-edit`, list or search over posts exists (see the
  [backup-host proposal](lfg-backup-host-spec.md)). Any later edit makes a source
  mutable; B reads it live and is unaffected (§4).

Not inspected: live guild, deployment, permissions or any shared database. No
usage counts of repeated titles or role strings exist. Runtime assumptions that
stay unverified: a channel-visibility read for the invoker is available at
preview time (§3), and the post-ID lookup stays guild-fenced and indexed.

## 3. Reusable fields, authority and disclosure (candidate B)

**Reusable fields, and only these:** `title`, and the ordered role entries
`(key, label, slots)`. `slots` is the seat capacity that makes a role valid; it is
configuration, not participation. Reading "labels only" strictly (re-enter seat
counts each time) is the alternative; it is a **reviewable decision** in §8.
**Never reused:** signups, member identities, `created_by`, `created_at`,
`closed_at`, `starts_at`, `message_id`, `channel_id`, status, audit receipts,
replay fences, nonce, and any notification or consent state.

Source identity is `(guild_id, post_id)` taken from the interaction's guild plus
one organizer-typed `source` option (a post ID, at most 128 UTF-16 units, the
existing `valid_post_id` rule). There is no search, "last post" or autocomplete:
those would enumerate titles across channels.

Admission, in this order, before any disclosure: configured guild; LFG enabled;
live Manage Events (the `/lfg` rule, no creator exception); `starts-at` parses
and is in the future (no store access yet); then the source lookup guild-fenced
by `guild_id`. Then the invoker must have **current view access to the source
post's channel**: Manage Events is guild-wide and can reach posts in channels the
invoker cannot read. Unknown or errored visibility fails closed. Missing,
foreign-guild, erased, malformed-ID and channel-denied sources collapse to one
`SOURCE_UNAVAILABLE` reply so the reply is not an oracle for IDs, guilds or private
channels. The reply is ephemeral to the invoker.

## 4. Preview, correction and staleness

The preview is **read-only and idempotent**: it writes nothing, takes no lock,
consumes no `/lfg` admission slot, audit row or open-post capacity, sends no
public message or DM, and mentions nothing (`allowed_mentions: {parse: []}`).

Rendered, ephemeral, bounded to the 2,000-unit message cap:

1. A line stating source ID, status (open or closed) and that people, time and
   consent are **not** carried.
2. The fields as copy-ready values: `title:`, `starts-at:` (the organizer's
   normalized UTC value, never the source's), and `roles:` as the canonical
   `key:Label:slots,...` string, each in its own code block.
3. A one-line correction hint: "Edit any value, then run `/lfg` with these;
   nothing is posted by this preview."
4. An advisory line only if the open-post cap is currently full ("`/lfg` will be
   refused until a post closes"). It is a boolean, not a count, and non-binding:
   `/lfg` re-checks atomically.

Code blocks use a fence one backtick longer than the longest backtick run in the
value (at least three). A value containing control or format characters (Cc/Cf,
including bidi overrides and newlines) is shown with visible `\u{XXXX}` escapes and
flagged `copy_ready=false`; the organizer retypes it rather than pasting
something invisible. Output never echoes raw upstream or SQL errors.

**Immutable preview versus mutable source.** The preview is an immutable message
carrying a `source_digest` (§5); it binds nothing and authorizes nothing. If the
source later changes or disappears, an earlier preview is unchanged and a re-run
shows the new digest or `SOURCE_UNAVAILABLE`. The real `/lfg` re-validates every
field and re-checks the cap, so a stale preview cannot create an invalid post,
exceed a limit, or revive a closed post. Where a private create preview ships
later, B hands the resolved fields to it instead of printing a command; that
change needs its own review.

**Stored source no longer valid.** Fields are re-validated on read with the
current rules above. Failure returns `INVALID_SOURCE` with no field values, and
the organizer retypes the post as today. Nothing is repaired or rewritten.

## 5. Bounds, retention and privacy/security findings (candidate B)

| Item | Proposed bound |
| --- | --- |
| Storage | **None.** No table, column, migration, cache, queue or timer; the preview is computed per call and forgotten |
| Reads | At most two indexed point reads (post by ID with guild fence, its roles) plus one cap count and one channel-visibility check |
| Output | One ephemeral message under 2,000 UTF-16 units; roles at most 20 |
| Rate | Admit through the existing per-actor automation window; whether `/lfg` create is covered by it is not verified here. A refusal shows no source data |
| Retention | Nothing new. Source posts follow the existing post/signup retention, and erasure of the source organizer removes the source |
| Spend | $0 estimated on the current process and Postgres; a sizing target, not measured headroom |

`source_digest` is SHA-256 over a versioned, length-prefixed encoding (domain
`lfg-reuse:v1`, guild, source ID, title, then each role's key, label and slots in
`position` order, each field prefixed by its UTF-8 length as a big-endian u64,
the existing LFG digest convention). It excludes signups, `created_by`,
`message_id`, timestamps and status, so it changes only when reusable fields
change. It is a display and agent identity, not a credential or capability.

| Data / threat | Control and remaining finding |
| --- | --- |
| Cross-channel disclosure of a private post's title and roles | Live view check on the source channel, fail closed; unavailable, denied and foreign collapse to one reply. Needs boundary disposition |
| Stale participation leaks into a new post | Participation fields are never read, digested or rendered; fixtures assert absence |
| Unsolicited send | None: ephemeral only, no DM, no public message, no create path |
| Mention/markup injection from a hostile label | `allowed_mentions` empty, fence-length rule, control/bidi escaping, 2,000-unit cap |
| Source churn between preview and `/lfg` | Preview is advisory; `/lfg` revalidates fully and still enforces the cap atomically |
| Privilege escalation | Same Manage Events gate as `/lfg`; preview grants nothing; creator identity grants nothing |
| Erasure | No copy exists. Erasing the source organizer removes the only source |
| Restart/version | Stateless; same inputs give the same digest after restart; unknown future `schema` is refused, not guessed |

**CISO proposal assessment:** B introduces **no new persisted data** and one new
read boundary (a Manage Events holder reading a title and role list by post ID).
Acceptable to circulate for intent review with the controls above; **not** a
grant, an independent approval of this document's own boundary request, or
runtime security acceptance. Blocking findings before any build: (1) accept or
narrow the channel-visibility gate and its fail-closed collapse; (2) confirm the
preview needs no new audit row, or name one; (3) confirm the read-only,
stateless claim on the real query path (point reads only). No credential or
permission is requested here. Any later change that adds storage (option C),
messages or authority needs its own independent disposition.

## 6. If a named preset catalog (C) is ever promoted

Not recommended now. If review shows organizers want names, discovery or reuse
after the source post is gone, C must meet these rules before any card:

- **Visibility/authority:** guild-scoped; list and use need Manage Events; save,
  rename and delete need Manage Events too, with no creator exception. No
  cross-guild read, import, export or legacy import.
- **Saved fields:** `name`, `title`, role entries, `version`, opaque `preset_id`,
  and an editor ID for audit only. Never signups, times, consent or message IDs.
- **Immutable preview versus mutable version:** a preview binds
  `(preset_id, version, digest)`. Edit creates the next version; using a
  preview whose version is no longer current returns `STALE_VERSION` and
  requires a fresh preview. Use still ends in the unchanged `/lfg` validation.
- **Quota:** at most 10 presets per guild and 3 retained versions per preset (a
  vendor figure of 3 free templates is a comparison, not a recommendation).
  Over quota refuses with `PRESET_QUOTA`; nothing is evicted silently.
- **Collision/rename/delete:** names are trimmed, 1-32 units, compared
  case-insensitively after NFC; a collision refuses, never overwrites. Rename keeps
  `preset_id`. Delete is a hard delete of configuration with one existing audit
  row; a later preview of a deleted ID is `SOURCE_UNAVAILABLE`.
- **Stale source:** a preset is a copy, so edits to the original post never
  change it; a preset created from a post stores no link back.
- **Retention/erasure:** guild configuration outlives its editor, so erasure
  anonymizes the editor ID rather than deleting the preset (a CISO decision);
  guild removal must follow existing guild-data retention (not verified).
  A new table needs an erasure-plan entry and the existing schema tripwires.
- Authoritative storage would be one new additive Postgres table in the shared
  database, created only after the §8 dispositions. None is proposed here.

## 7. Agent-facing result and offline fixture contract

No executable CLI or API is registered. These are **proposed typed results**, not
commands to run on a guild. Discover this file through the
[documentation map](README.md). Stable envelope: `schema=lfg.reuse.v1`,
`guild_id`, `source_id`, `source_digest`, `title`, `roles` as `(key, label,
slots)`, `starts_at` (the organizer's value), `copy_ready`, `at_cap`, `reason`,
`retryable`, `next_action`. Non-preview replies omit every source field. Preset
IDs and `version` exist only if C is promoted (§6).

| reason | retryable / next_action |
| --- | --- |
| `PREVIEW` | false / `run_lfg_with_values` |
| `INVALID_TIME` | false / `provide_future_starts_at` |
| `SOURCE_UNAVAILABLE` | false / `check_source_id_and_access` (missing, foreign, erased, denied, unknown access) |
| `INVALID_SOURCE` | false / `retype_with_lfg` |
| `DENIED` | false / `ask_manage_events_holder` |
| `STORE_UNAVAILABLE` | true / `retry_after_backoff` |

The offline check has no environment selector, credentials, network, database,
Discord client, side effect or wrong-flag production path. No staging or live
pilot is authorized, and flags cannot combine into one.

Check order is permission, time, store, source, validity; an invalid time never
touches the store. The Python model below mirrors the validation rules in §2 and
the fence and digest rules in §4-§5. Cases: ok, closed source (PREVIEW, nothing
carried), past and malformed and equal-now time, invalid stored roles, deleted or
erased source, foreign guild, channel denied, unknown access, full cap, hostile
labels, source changed (new digest), restart and version.

Runnable offline document check (Python stdlib; from the repository root):

```sh
python3 -m doctest -v docs/lfg-reuse-spec.md
python3 scripts/check_docs_index.py --json
```

The doctest checks the **specified model's consistency**, not real SQL, Discord
visibility, rendering, erasure or deployed behavior. Future protected acceptance
tests must exercise the real caller against a disposable database with injected
clocks and transport faults; zero tests is not proof. No acceptance or
implementation task is created here.

```pycon
>>> import hashlib, json, re, unicodedata
>>> WS = "\t\n\v\f\r \u00a0\u1680\u2000\u2001\u2002\u2003\u2004\u2005\u2006\u2007\u2008\u2009\u200a\u2028\u2029\u202f\u205f\u3000\ufeff"
>>> u16 = lambda s: len(s.encode("utf-16-le")) // 2
>>> def title_ok(t): return 0 < u16(t.strip(WS)) <= 100 and t == t.strip(WS)
>>> def role_ok(k, label, n):
...     return (re.fullmatch(r"[a-z0-9_-]{1,32}", k) is not None and k != "__leave__"
...             and 0 < u16(label) <= 80 and label == label.strip(WS)
...             and "," not in label and ":" not in label and isinstance(n, int) and 1 <= n <= 99)
>>> def roles_ok(roles):
...     keys = [k for k, _, _ in roles]
...     return 1 <= len(roles) <= 20 and len(set(keys)) == len(keys) and all(role_ok(*r) for r in roles)
>>> lp = lambda s: len(s.encode()).to_bytes(8, "big") + s.encode()
>>> def digest(guild, source, title, roles):
...     h = hashlib.sha256()
...     for part in ("lfg-reuse:v1", guild, source, title):
...         h.update(lp(part))
...     for k, label, n in roles:
...         for part in (k, label, str(n)):
...             h.update(lp(part))
...     return h.hexdigest()
>>> def fence(v):
...     run = max((len(m) for m in re.findall("`+", v)), default=0)
...     return "`" * max(3, run + 1)
>>> def copy_ready(v): return not any(unicodedata.category(c) in ("Cc", "Cf") for c in v)
>>> def preview(src, *, now, starts, manage=True, same_guild=True, can_view=True, store=True):
...     if not manage: return "DENIED"
...     if not (isinstance(starts, int) and starts > now): return "INVALID_TIME"
...     if not store: return "STORE_UNAVAILABLE"
...     if src is None or not same_guild or can_view is not True: return "SOURCE_UNAVAILABLE"
...     if not (title_ok(src["title"]) and roles_ok(src["roles"])): return "INVALID_SOURCE"
...     return "PREVIEW"
>>> good = {"title": "Raid night", "roles": [("tank", "Tank", 2), ("dps", "DPS", 6)],
...         "signups": ["u1", "u2"], "created_by": "u1", "message_id": "m9", "starts_at": 1, "status": "open"}
>>> N, S = 1000, 2000
>>> cases = [
...     ("ok", preview(good, now=N, starts=S), "PREVIEW"),
...     ("closed_source", preview(dict(good, status="closed"), now=N, starts=S), "PREVIEW"),
...     ("time_past", preview(good, now=N, starts=500), "INVALID_TIME"),
...     ("time_equal_now", preview(good, now=N, starts=N), "INVALID_TIME"),
...     ("time_malformed", preview(good, now=N, starts="soon"), "INVALID_TIME"),
...     ("bad_role_key", preview(dict(good, roles=[("Tank", "Tank", 2)]), now=N, starts=S), "INVALID_SOURCE"),
...     ("bad_slots", preview(dict(good, roles=[("tank", "Tank", 0)]), now=N, starts=S), "INVALID_SOURCE"),
...     ("dup_key", preview(dict(good, roles=[("a", "A", 1), ("a", "B", 1)]), now=N, starts=S), "INVALID_SOURCE"),
...     ("label_comma", preview(dict(good, roles=[("a", "A,B", 1)]), now=N, starts=S), "INVALID_SOURCE"),
...     ("too_many_roles", preview(dict(good, roles=[("r%d" % i, "R", 1) for i in range(21)]), now=N, starts=S), "INVALID_SOURCE"),
...     ("deleted_or_erased", preview(None, now=N, starts=S), "SOURCE_UNAVAILABLE"),
...     ("foreign_guild", preview(good, now=N, starts=S, same_guild=False), "SOURCE_UNAVAILABLE"),
...     ("channel_denied", preview(good, now=N, starts=S, can_view=False), "SOURCE_UNAVAILABLE"),
...     ("access_unknown", preview(good, now=N, starts=S, can_view=None), "SOURCE_UNAVAILABLE"),
...     ("no_manage_events", preview(good, now=N, starts=S, manage=False), "DENIED"),
...     ("store_outage", preview(good, now=N, starts=S, store=False), "STORE_UNAVAILABLE"),
...     ("time_before_store", preview(good, now=N, starts=1, store=False), "INVALID_TIME"),
... ]
>>> assert all(got == want for _, got, want in cases), [c for c in cases if c[1] != c[2]]
>>> assert len({c[0] for c in cases}) == len(cases) == 17
>>> d = digest("g1", "p1", good["title"], good["roles"])
>>> assert d == "f79721dad30839aaa1505496e1f30714f4e23e04c5e43a179f65cb10ce15c5ca"
>>> # participation, authority and time never reach the digest; reusable fields and bindings do
>>> noisy = dict(good, signups=[], created_by="u9", message_id="m1", starts_at=2, status="closed")
>>> assert digest("g1", "p1", noisy["title"], noisy["roles"]) == d  # restart and noise: same digest
>>> assert digest("g1", "p1", "Raid night!", good["roles"]) != d  # source changed: new digest
>>> assert digest("g1", "p1", good["title"], [("tank", "Tank", 3), ("dps", "DPS", 6)]) != d
>>> assert digest("g2", "p1", good["title"], good["roles"]) != d and digest("g1", "p2", good["title"], good["roles"]) != d
>>> assert digest("g1", "p1", good["title"], good["roles"][::-1]) != d  # order is part of the identity
>>> assert preview(dict(good, title="  padded"), now=N, starts=S) == "INVALID_SOURCE"  # untrimmed is not parser output
>>> # hostile labels: fence outgrows any backtick run; control/bidi values are not copy-ready
>>> hostile = ["@everyone", "<@123456789012345678>", "a`b``c```d", "\u202eevil", "line\nbreak"]
>>> assert [len(fence(v)) for v in hostile] == [3, 3, 4, 3, 3]
>>> assert [copy_ready(v) for v in hostile] == [True, True, True, False, False]
>>> assert all(fence(v) not in v for v in hostile)
>>> assert all(preview(dict(good, roles=[("a", v, 1)]), now=N, starts=S) == "PREVIEW" for v in hostile if "," not in v)
>>> # version: an unknown consumer schema is refused rather than guessed
>>> supported = {"lfg.reuse.v1"}
>>> assert "lfg.reuse.v2" not in supported and digest("g1", "p1", "t", []) != hashlib.sha256(b"").hexdigest()
>>> print(json.dumps({"schema": "lfg.reuse.oracle.v1", "fixtures": len(cases), "status": "pass"}, sort_keys=True))
{"fixtures": 17, "schema": "lfg.reuse.oracle.v1", "status": "pass"}

```

## 8. Acceptance boundary, review metric and undo

Record CEO spec acceptance **and** host intent alignment against the exact
revision before any implementation subtasks, protected test handoff or live pilot.
Route the decision brief through the President/COO to the CEO and the existing
host intent path; a queue or intake receipt is not alignment. The CISO findings in
§5 must receive an independent boundary disposition, not a self-approved grant.
No request goes directly to the owner or an invented host executor.

The decision must accept or revise: A as the answer now; whether B is worth
building at all; if so, B as a read-only preview with no create path, `source` as
an explicit post ID only, the channel-visibility gate and its collapse, seat counts
as reusable (or labels only), and a fresh organizer-chosen future time on every
use; and that C stays unpromoted. On any material revision, bind a fresh review
request and alignment to that revision.

By 2026-10-31 the metric is **all 17 fixtures have agreed or explicitly rejected
outcomes, the three blocking findings have written disposition, and the typed-input
comparison in §1 is recorded** (A: three inputs; B: two preview inputs plus three
pastes). Keep B only if review judges its note-free, exact-string benefit worth a
command **and** the fixtures show no stale participation carried and no unsolicited
send or new spend. This measures design completeness, not engagement or demand.
Otherwise the outcome is A (no-build): stop this proposal, not LFG or the project.
Default while unresolved is no build; no timeout auto-approval. A future demand
experiment needs its own hypothesis, metric and date; none is authorized.

ACT: publish this proposed contract and its offline model for review. WHY: bound
reusable fields and the read boundary before any preset work. UNDO: reviewed docs
revert; there is no operational change to undo. A later independently accepted
build must keep admission dark until its own tests, security and release gates
pass. No production tests, migrations, credentials or activation here.
