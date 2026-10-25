# Dashboard feed configuration contract

Status: **proposed; design only**, 2026-10-25. Intent review by 2026-10-31.
No page, signed verb, runtime change, permission, migration, live write or
activation is implemented or authorized here. This file is the single repository
source, including the JSON manifest and offline oracle. The separate
[ticket](dashboard-ticket-config-spec.md) and
[onboarding/self-role](dashboard-onboarding-self-roles-spec.md) proposals are not
extended by this change.

## Decision

**Wiring-first; otherwise no-build.** An admin needs to distinguish stored global
configuration, saved relay definitions and actual delivery behavior. A catalogue
`Hot` label, successful save, snapshot log or elapsed poll interval proves none
of the others. The eventual smallest page is private, read-only configuration
inspection; even that waits for an accepted transport/authority contract. Keep
cadence edits read-only until typed validation is shared with its real reader.
Relay editing needs missing signed verbs and concurrency rules, not a direct
website database writer. This is not an approval to build those prerequisites.

Source inventory is pinned to `c3af7333df80a766f5d4c229506a3bce53680828`, not a
claim about a deployed image. Existing runtime readers are source witnesses,
not an acknowledgement that any particular stored revision was consumed.

## Inventory: global controls versus relay records

| Stable ID / field | Writer → actual reader | State and limitation |
| --- | --- | --- |
| `FC-K1` / `TWO_FEED_POLL_SECONDS` | Environment or settings store → `live_poll_seconds_with` (`crates/bot/src/feed_jobs.rs:206-236`), supervisor cadence and per-tick schedule (`:276-327`). Both catalogue Hot and HOT_WIRED (`crates/core/src/settings.rs:267`, `:299`). | Conditionally live, boot default 300 s, reader accepts 60..86400. Missing/deleted, wrong-type, unparsable or out-of-range snapshot value falls back to **boot**, not last-good. Invalid boot interval refuses job construction even if stored interval is valid (`feed_jobs.rs:294-299`). Generic write validation has no cadence-specific bound (`settings.rs:1061-1140`): keep edit/reset read-only; validation-first. |
| `FC-K2` / `TWO_ANNOUNCEMENTS` | Environment or settings store → `live_announcements_with` and pre-pass gate (`feed_jobs.rs:239-266`, `:402-418`). Hot/HOT_WIRED (`settings.rs:191`, `:310`); store gate validation (`:1035-1042`, `:1088-1094`). | Conditionally live for an **existing** boot-enabled, identity-permitted poller: rendering `1` runs, `0` parks; unset falls back to boot. Not an immediate cancellation: a pass already running can finish. Parking skips pending-delivery recovery too. |
| `FC-S1` / job existence and slash surface | Boot `FeatureGates` plus activation constrain registration (`feed_jobs.rs:329-395`); command publication uses boot map (`crates/bot/src/commands_cli.rs:110-130`); routing uses boot gates (`crates/bot/src/command_runtime.rs:419-470`, `:2268-2282`). | **Restart/boot-only** prerequisites. Boot-off cannot become a job through stored `1`. Changing slash surface requires a permitted boot configuration and publication; stored gate is **unwired for slash dispatch at this head**. Restart alone does not turn stored data into environment. |
| `FC-R1` / `id,guild_id,channel_id,kind,source` | `/feed-add` planner sets context guild/channel and normalized source (`crates/core/src/feeds.rs:121-164`); insert-only store (`crates/core/src/feeds_store.rs:25-50`). Poller uses guild-scoped enabled rows and source/kind/destination. | Runtime-read relay definition; no in-place edit. Add validates but never fetches. Destination is invoking channel. Duplicate guild/channel/kind/source tuple refused; 25 definitions/guild including disabled rows. No dashboard verb. |
| `FC-R2` / `enabled` | Schema default true and add initializes true (`crates/cutover/migrations/0180_feeds.sql:8`; `feeds.rs:140`); list/claim/recovery readers honor it (`feeds_store.rs:53-84`, `:188-285`). | Reader exists, **update surface absent**, not stored-unwired. No supported pause/resume writer on this head. Do not use remove/re-add as pause: removal refuses pending evidence, otherwise cascades settled ledger history (`feeds_store.rs:87-149`). |
| `FC-R3` / `last_checked_at` | Poller `mark_checked` after processing, including fetch failure (`feed_jobs.rs:431-489`); inspect/list render qualified last attempt (`feeds.rs:295-319`, `:546-574`). | Read-only attempt metadata, not success/health or a consumed-config revision. Cancellation can leave it unchanged. |
| `FC-R4` / `created_by,created_at,updated_at` | Add supplies actor/time (`feeds.rs:142-144`); store loads these; ordering uses created_at/id (`feeds_store.rs:41-81`). `mark_checked` changes only last_checked_at (`:331-339`). | Stored metadata; not settings or a concurrency token. No editor is exposed. Do not call updated_at the last poll time. |

Schema: `feed_relays` has exactly these ten fields
(`crates/cutover/migrations/0180_feeds.sql:2-14`); `feed_deliveries` is a separate
runtime claim/history ledger, never page-editable. There is **no per-relay
interval**, template, source-sampling setting or feed runtime dry-run key.
Kinds are RSS, YouTube and Twitch feed URLs; YouTube channel IDs normalize to
a feed URL (`feeds.rs:179-195`), not a new provider/Helix integration.

**Other classifications:** no separate feed-specific Cold catalogue row exists;
restart-only above describes construction/publication, not a made-up Cold key.
No whole feed key is stored-unwired: the **slash consumer** of the shared
announcements key is. Deployment identity (`DISCORD_GUILD_ID`,
`DISCORD_STAGING_GUILD_ID`) and `TWO_INTERNAL_*` are EnvOnly and refused by
both get/set (`settings.rs:105-106`, `:1071-1080`). EventSub ingress/delivery,
announcement destination and Twitch credentials/callbacks stay EnvOnly
(`settings.rs:149-164`), outside this feed editor. SSRF policy is code-owned,
not a configurable network allowlist. The recognized live identity is cleared
only for SelfRoles, not announcements (`crates/core/src/activation.rs:51-96`);
no page or stored gate can clear it.

**Refresh/precedence:** the nominal 15 s settings job publishes only successful
snapshots (`crates/core/src/settings.rs:51`;
`crates/bot/src/settings_jobs.rs:152-173`). Both keys can be masked by the
independently expiring operator overlay ahead of guild rows (`settings.rs:367-374`,
`:598-608`; forwarded flags at `wrangler/src/container-env.ts:22-25`). Get does
not reveal that layer. Supervisor cadence rechecks every 5 s and reschedules
last-run + changed interval, including decreases, without overlap
(`crates/bot/src/jobs.rs:188-307`). This is not an apply SLA; failed/parked polls,
boot/identity refusal and an in-flight pass invalidate a wall-clock assertion.

**Read-only precedent and safety:** `/feed-inspect` makes a bounded guild read,
not a source fetch, audit, schedule edit, claim renewal or repair
(`command_runtime.rs:2220-2253`). All four feed commands require ManageGuild
and the existing guild/announcements gate (`feeds.rs:121-129`). Preserve HTTPS
443/no credentials, 2048-byte source bound, all-public DNS answers and repinned
same-host redirects (maximum 3), no proxy/second-resolution bypass
(`crates/core/src/feeds_http.rs:8-11`, `:58-190`;
`crates/core/src/feeds_connector.rs:1-33`). No page fetch to test a source.

**In flight, not merged evidence:** [pause/resume #1720](https://github.com/TogetherWeOwn/two-bot-next/pull/1720)
and [live command dispatch #1639](https://github.com/TogetherWeOwn/two-bot-next/pull/1639)
were open at the 2026-10-25 inventory. Do not borrow their behavior or tests as
HEAD guarantees. Re-inventory after merge before changing this contract.

## Existing signed settings contract

Use only actual `settings.get` / `settings.set`, with the private receiver,
existing signing/replay protocol and `TWO_INTERNAL_ALLOW_SETTINGS` gate
(`crates/core/src/internal_actions.rs:674-694`, `:846-848`).
`announcement.post` is a one-message action, not relay CRUD or configuration.
The implemented-action registry contains no feed-relay read/create/edit/delete
or pause/resume verb. Do not invent a route or grant web DB access to fill that
hole. Existing CLI/slash inspection is not automatically a website transport.

Both settings handlers select **TWO_STAGING_GUILD_ID**, ignoring body guild
selection (`crates/bot/src/internal_action_http.rs:2626`, `:2662`). Before any
signature, the future web server must compare the trusted session guild with
the configured managed guild for reads AND writes. A foreign guild request
otherwise risks targeting staging; do not claim the receiver rejects it by
body guild. No production or multiguild surface, receiver retargeting, expanded
bearer scope or capability enablement is granted here.

Get is `{ok,result:{key,value,source},version,request_id}`. `source=store` means
stored override only; `source=unset,value=null,version=0` means inherited, with
the actual fallback hidden. It returns neither effective environment nor runtime
status, actor/history or operator-overlay metadata. Set is
`{ok,result:{key,outcome},version,request_id}` with `saved` or `unset`, not the
submitted value (`internal_action_http.rs:2782-2809`;
`crates/core/src/internal_settings.rs:117-138`).

For any future permitted one-key save/reset, require the observed integer
`expected_version`, even though omission currently permits unconditional writes.
CAS is opaque equality, not ordering; negative tokens are normal; zero expects
absence. Absence cannot detect a create/delete cycle. Store validation precedes
SQL; the revision lock orders comparison, value/delete and audit in one
transaction (`crates/cutover/src/settings.rs:177-208`, `:242-288`). This token
does not cover `feed_relays`, create an atomic multi-key save or serialize a
slash relay writer. Derive `updated_by` from the authenticated admin's Discord
identity server-side: the parser checks ID shape, not the actor's authority
(`crates/core/src/internal_settings.rs:46`).

A confirmed intent gets one fresh idempotency key and exact body bytes. Retry
that intent only with the same key/body and fresh nonce/timestamp/signature;
a different value/CAS is a new intent. The receiver claims before execution
(`internal_action_http.rs:2672-2688`). Replay never repeats DML/audit, but success
side-reads the **current** CAS, not necessarily the version first saved
(`:2729-2776`). Reread before another edit or undo. Timeout, terminal persistence
failure or `needs_reconciliation` is unknown; never force it with a new key.

## Honest admin behavior

**Authority prerequisite, not an inspected web implementation:** private,
non-cacheable admin responses; current membership and Manage Server authority
checked server-side on reads/writes; CSRF protection; revoked/expired sessions
denied; signing keys stay server-side. Preserve the existing bot guild, command,
feature and activation gates. Inspect no member content or delivery bodies.
Escape channel/source text, disable mentions, bound all labels and errors; no
raw URL, payload or remote error body in logs, alerts or previews. Sources may
contain credentials: show a redacted inert summary, never an active URL/link.
Viewing, refreshing and previewing never fetch a feed or validate it by network.

Show global controls separately from relay rows. Each row needs a stable ID,
observation time, stored source (override/inherited), read-only reason and
consumer limitation. An inherited default may have a documented example, never
a guessed effective value. Saved relay presence is not an active delivery state;
a last-seen cursor is not proof of current health. Keep unknown, disabled,
paused and unavailable distinct. Never show `applied` without a correlated
consumer witness for that guild, key/relay and revision; no such acknowledgement
is exposed by today's settings API.

A fresh get may replace stale display data, but never silently replace an unsent
draft. On read failure retain the last observation labeled stale, disable
save/reset and offer retry-read. Never turn an unavailable read into an empty
relay list, inherited setting or successful no-change. No offline save queue.
An unchanged local draft is `NO_CHANGE` and sends nothing, rather than assuming
that repeated identical store writes skip audit/version allocation.

Future reset means explicit JSON `null` plus CAS, not `0` or empty text. Undo
means another deliberate CAS save of the prior stored value (or null if absent)
after fresh get, not DB rollback or a blind overwrite. Inheritance can still be
masked by operator overrides. Global gate rollback does not undo messages,
change relay cursors or recreate a boot-disabled poller; relay pause/resume is
not a global gate reset.

**Dry-run means local no-effect preview only.** Identical manifest revision and
inputs yield the same `{schema,guild_id,key_or_relay_id,expected_version,would,
reason,retryable,next_action}`. It makes zero network/signing/nonce/idempotency,
SQL/audit, source-fetch or Discord calls. It checks shape and the read-only
policy, not live CAS/permissions, source reachability or delivery. No server
settings dry-run exists: adding `dry_run:true` or an unrecognized flag to a
mutating request is not a safety mechanism. This proposal sends no mutations.

The following are **proposed adapter reasons**, not bot wire enums. Actual
errors expose `code,message,retryable,request_id`, not internal `log_reason`
(`internal_action_http.rs:3267-3271`, `:3298-3321`). Respect Retry-After when
present; `in_progress` retains the same intent. Do not retry every 409.

| Reason | Private, bounded copy | next_action |
| --- | --- | --- |
| `STORED_UNVERIFIED` | Saved configuration; bot application is not confirmed. | `reread_before_edit` |
| `INHERITED_UNVERIFIED` | Inherited configuration; effective value is unavailable here. | `none` |
| `NO_CHANGE` | Nothing changed; nothing sent. | `none` |
| `PREVIEW_ONLY` | Preview only; nothing fetched, sent or saved. | `confirm_or_edit` |
| `NOT_EDITABLE` | This control is read-only pending validation or wiring. | `wait_for_wiring` |
| `INVALID_VALUE` | Invalid shape or out-of-range value; no input echoed. | `fix_input` |
| `VERSION_CONFLICT` | Configuration changed; your draft is kept. | `reread_then_reconfirm` |
| `FOREIGN_GUILD` | This guild is not managed by this configuration surface. | `select_managed_guild` |
| `UNAUTHORIZED` | Configuration access is unavailable for this session. | `reauthenticate_or_stop` |
| `RUNTIME_UNAVAILABLE` | Bot read unavailable; previous data is stale. | `retry_read` |
| `COMMIT_UNKNOWN` | Save outcome unknown; do not submit a different save. | `reconcile_same_intent` |

## Machine-readable inventory and fixture oracle

IDs below are stable proposal IDs, not new wire fields. Relay identity is the
stored `(guild_id, id)`, never a list index, source URL or destination name.
The manifest's states classify source capability; they do not report a live
instance. `stored_unwired` means no evidenced reader, not an invalid value that
an existing reader rejects or a boot-only prerequisite.

```json
{
  "schema":"dashboard.feed_config.v1",
  "source_sha":"c3af7333df80a766f5d4c229506a3bce53680828",
  "decision":"wiring_first_or_no_build",
  "relay_identity":["guild_id","id"],
  "keys":[
    {"id":"FC-K1","key":"TWO_FEED_POLL_SECONDS","class":"Hot","hot_wired":true,"state":"conditional_live","edit":false,"writer":"settings.set_or_boot_env","evidence":["E-SETTINGS-WRITE","E-SETTINGS-FENCE"],"reader":["E-INTERVAL","E-CADENCE"],"gate":["FC-W1","FC-W3"]},
    {"id":"FC-K2","key":"TWO_ANNOUNCEMENTS","class":"Hot","hot_wired":true,"state":"conditional_live","edit":false,"writer":"settings.set_or_boot_env","evidence":["E-SETTINGS-WRITE","E-SETTINGS-FENCE"],"reader":["E-GATE","E-TICK"],"gate":["FC-W3"]}
  ],
  "relays":[
    {"id":"FC-R1","fields":["id","guild_id","channel_id","kind","source"],"state":"runtime_read_no_dashboard_verb","edit":false,"writer":"slash_add_insert_only","reader":["E-RELAY-READ","E-FETCH"],"evidence":["E-ADD","E-SCHEMA"]},
    {"id":"FC-R2","fields":["enabled"],"state":"reader_wired_update_absent","edit":false,"writer":"insert_only","reader":["E-RELAY-READ"],"evidence":["E-ADD","E-COMMANDS"]},
    {"id":"FC-R3","fields":["last_checked_at"],"state":"attempt_only","edit":false,"writer":"runtime_mark_checked","reader":["E-INSPECT"],"evidence":["E-MARK"]},
    {"id":"FC-R4","fields":["created_by","created_at","updated_at"],"state":"stored_metadata","edit":false,"writer":"slash_add","reader":["E-RELAY-READ"],"evidence":["E-ADD"]}
  ],
  "other_states":[
    {"id":"FC-S1","state":"restart_only","evidence":["E-BOOT","E-SLASH"]},
    {"id":"FC-S2","state":"stored_unwired_for_slash","key":"TWO_ANNOUNCEMENTS","evidence":["E-SLASH"]},
    {"id":"FC-S3","state":"env_only","evidence":["E-ENV","E-PREFIX","E-EVENTSUB"]},
    {"id":"FC-S4","state":"absent","evidence":["E-SCHEMA","E-VERBS"]},
    {"id":"FC-S5","state":"in_flight_not_merged","pull_requests":[1720,1639],"evidence":["E-COMMANDS","E-SLASH"]},
    {"id":"FC-S6","state":"activation_denied_for_live","evidence":["E-ACTIVATION"]}
  ],
  "evidence":{
    "E-SETTINGS-WRITE":{"path":"crates/cutover/src/settings.rs","line":177,"contains":"pub async fn set_if_version"},
    "E-SETTINGS-FENCE":{"path":"crates/bot/src/internal_action_http.rs","line":2662,"contains":"TWO_STAGING_GUILD_ID"},
    "E-INTERVAL":{"path":"crates/bot/src/feed_jobs.rs","line":212,"contains":"fn live_poll_seconds_with"},
    "E-CADENCE":{"path":"crates/bot/src/feed_jobs.rs","line":306,"contains":"cadence_source:"},
    "E-GATE":{"path":"crates/bot/src/feed_jobs.rs","line":250,"contains":"fn live_announcements_with"},
    "E-TICK":{"path":"crates/bot/src/feed_jobs.rs","line":412,"contains":"if !gate()"},
    "E-BOOT":{"path":"crates/bot/src/feed_jobs.rs","line":386,"contains":"if !gates.announcements"},
    "E-SLASH":{"path":"crates/bot/src/command_runtime.rs","line":2274,"contains":"self.interactions.router.gates().announcements"},
    "E-RELAY-READ":{"path":"crates/core/src/feeds_store.rs","line":63,"contains":"WHERE guild_id = $1 AND (NOT $2 OR enabled)"},
    "E-FETCH":{"path":"crates/bot/src/feed_jobs.rs","line":455,"contains":"fetch.fetch(feed.clone())"},
    "E-ADD":{"path":"crates/core/src/feeds_store.rs","line":29,"contains":"pub async fn add_feed"},
    "E-SCHEMA":{"path":"crates/cutover/migrations/0180_feeds.sql","line":2,"contains":"CREATE TABLE IF NOT EXISTS feed_relays"},
    "E-COMMANDS":{"path":"crates/core/src/feeds.rs","line":88,"contains":"pub enum FeedCommand"},
    "E-MARK":{"path":"crates/bot/src/feed_jobs.rs","line":485,"contains":"store::mark_checked"},
    "E-INSPECT":{"path":"crates/bot/src/command_runtime.rs","line":2225,"contains":"async fn feed_inspect"},
    "E-ENV":{"path":"crates/core/src/settings.rs","line":105,"contains":"SettingClass::EnvOnly"},
    "E-PREFIX":{"path":"crates/core/src/settings.rs","line":1071,"contains":"ENV_ONLY_KEY_PREFIXES"},
    "E-EVENTSUB":{"path":"crates/core/src/settings.rs","line":149,"contains":"SettingClass::EnvOnly"},
    "E-VERBS":{"path":"crates/core/src/internal_actions.rs","line":674,"contains":"pub const IMPLEMENTED_ACTIONS"},
    "E-ACTIVATION":{"path":"crates/core/src/activation.rs","line":52,"contains":"&[LiveCapability::SelfRoles]"}
  },
  "reasons":{
    "STORED_UNVERIFIED":{"retryable":false,"next_action":"reread_before_edit"},
    "INHERITED_UNVERIFIED":{"retryable":false,"next_action":"none"},
    "NO_CHANGE":{"retryable":false,"next_action":"none"},
    "PREVIEW_ONLY":{"retryable":false,"next_action":"confirm_or_edit"},
    "NOT_EDITABLE":{"retryable":false,"next_action":"wait_for_wiring"},
    "INVALID_VALUE":{"retryable":false,"next_action":"fix_input"},
    "VERSION_CONFLICT":{"retryable":false,"next_action":"reread_then_reconfirm"},
    "FOREIGN_GUILD":{"retryable":false,"next_action":"select_managed_guild"},
    "UNAUTHORIZED":{"retryable":false,"next_action":"reauthenticate_or_stop"},
    "RUNTIME_UNAVAILABLE":{"retryable":true,"next_action":"retry_read"},
    "COMMIT_UNKNOWN":{"retryable":false,"next_action":"reconcile_same_intent"}
  }
}
```

| Fixture ID | Oracle for future protected acceptance tests; no live writes |
| --- | --- |
| `FC-F01` | For both keys, get stored/unset, permitted signed CAS write in an isolated store, injected settings publication, then actual interval/gate consumer. Only the consumer effect witnesses runtime use; get/saved/snapshot log alone must not pass. Page controls remain read-only under this proposal. |
| `FC-F02` | Interval 59/86401, negative/overflow, boolean/object/array, fractions and malformed text fall back to boot under the current reader; 60/86400 are boundaries. Direct generic store accepts some unusable intervals: demonstrate that gap, then FC-W1 must refuse before SQL. Future UI uses canonical decimal integer seconds, null only for reset; generic 8192-byte/NUL refusals remain. Test gate 0/1/booleans versus malformed renderings separately. |
| `FC-F03` | Two signed settings writers read t; only one save at t commits/audits, the loser retains draft and rereads. Two creators expect 0: one wins. Slash add/remove are **relay**, not settings writers: settings CAS cannot protect them. Future FC-W2 must race slash versus dashboard on the same relay token; until then dashboard relay mutation is refused. |
| `FC-F04` | Inject interval increase/decrease during pending timer: target last-run + new interval, no overlap or phase drift for unchanged value; boot-invalid registration still fails despite a valid stored value. No sleep-based apply guarantee. |
| `FC-F05` | Existing permitted poller consumes gate 0 then 1: no new pass while parked, already-running work may finish, recovery resumes only when passes resume. Boot-off or denied identity + stored 1 creates no job. Slash routing/publication still uses boot gate at this head. |
| `FC-F06` | Stored enabled true/false controls selection/admission; missing pause/resume surface yields NOT_EDITABLE, never remove/re-add. After #1720 merges, separately prove paused relay retains history and revalidate in-flight send/recovery behavior before updating this spec; do not borrow unmerged tests. |
| `FC-F07` | Foreign session guild: zero signatures/requests and no values. Isolated guild B rows cannot affect guild A readers. Pin receiver's staging constant independently; do not test a fictional body-guild refusal. |
| `FC-F08` | Same idempotency key/body: one DML/audit. Different body: refusal. Replay after later write returns current CAS; failed side-read or terminal ambiguity yields COMMIT_UNKNOWN. No fresh-key forced retry. |
| `FC-F09` | CAS unset/prior-value rollback: audited change then fallback after publication, subject to operator overlay. Invalid/deleted stored interval returns **boot**, not last-good. Missing gate returns boot; reset does not undo sent messages or relay history. |
| `FC-F10` | Unavailable runtime/failed snapshot poll: no invented empty/default/applied state. Stale observation retained, save/reset disabled. Last_checked_at may advance after a failed fetch, so it cannot become a healthy badge. |
| `FC-F11` | Identical local previews identical; zero network, signing, nonce/claim, SQL, audit or Discord effects. NO_CHANGE sends nothing. Cold/EnvOnly, absent and boot-only controls have no dashboard write; generic API's Cold storage support is not permission to expose it. Unknown flags never reach a mutating request. |
| `FC-F12` | Missing/revoked/non-admin session denies before values/signing. Source viewing/inspect/preview never fetches. SSRF literals, mixed public/private DNS and cross-host redirects stay refused in fetch-policy fixtures; escaped source/channel text cannot ping or leak secrets. |

Agent handoff shape: `{schema,source_sha,spec_commit,spec_sha256,protected_files,
key_or_relay_id,outcome,reason,next_action,check}`. Record SHA-256 for unchanged
protected acceptance files and constraints in the task delivery record; never
edit them to satisfy this proposal. Source/spec hash mismatch requires
re-inventory, not optimistic application.

## Offline verification

Run from the repository root, with Python stdlib only:

```sh
python3 -m doctest -v docs/dashboard-feed-config-spec.md
python3 scripts/check_docs_index.py --json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
```

```pycon
>>> import json, pathlib, re
>>> doc = pathlib.Path("docs/dashboard-feed-config-spec.md").read_text()
>>> fence = "`" * 3
>>> m = json.loads(re.search(fence + r"json\n(.*?)\n" + fence, doc, re.S).group(1))
>>> src = pathlib.Path("crates/core/src/settings.rs").read_text()
>>> table = src.split("pub const SETTING_CLASSES", 1)[1].split("\n];", 1)[0]
>>> classes = dict(re.findall(r'\(\s*"(\w+)",\s*SettingClass::(\w+),?\s*\)', table))
>>> wired = set(re.findall(r'"(\w+)"', src.split("pub const HOT_WIRED", 1)[1].split("\n];", 1)[0]))
>>> assert {r["key"] for r in m["keys"]} == {"TWO_ANNOUNCEMENTS", "TWO_FEED_POLL_SECONDS"}
>>> assert all(r["class"] == classes[r["key"]] == "Hot" and r["hot_wired"] == (r["key"] in wired) for r in m["keys"])
>>> assert all(not r["edit"] and r["writer"] and r["reader"] for r in m["keys"] + m["relays"])
>>> relay = pathlib.Path("crates/core/src/feeds.rs").read_text().split("pub struct FeedRelay {", 1)[1].split("\n}", 1)[0]
>>> assert {f for r in m["relays"] for f in r["fields"]} == set(re.findall(r"pub (\w+):", relay))
>>> assert m["relay_identity"] == ["guild_id", "id"]
>>> rows = m["keys"] + m["relays"] + m["other_states"]
>>> assert len(rows) == len({r["id"] for r in rows}) == 12
>>> assert {r["state"] for r in m["other_states"]} == {"restart_only", "stored_unwired_for_slash", "env_only", "absent", "in_flight_not_merged", "activation_denied_for_live"}
>>> assert all(all(e in m["evidence"] for e in r.get("evidence", []) + r.get("reader", [])) for r in rows)
>>> assert all(e["contains"] in pathlib.Path(e["path"]).read_text().splitlines()[e["line"]-1] for e in m["evidence"].values())
>>> verbs = pathlib.Path("crates/core/src/internal_actions.rs").read_text().split("pub const IMPLEMENTED_ACTIONS", 1)[1].split("\n];", 1)[0]
>>> assert not any(v.startswith("feed") for v in re.findall(r'"([a-z_.]+)"', verbs))
>>> assert all(classes[k] == "EnvOnly" for k in ("DISCORD_GUILD_ID", "DISCORD_STAGING_GUILD_ID", "TWO_EVENTSUB", "TWO_EVENTSUB_DELIVERY", "TWO_EVENTSUB_ANNOUNCEMENTS_CHANNEL_ID"))
>>> assert set(m["reasons"]) == set(re.findall(r'^\| `([A-Z_]+)` \|', doc, re.M))
>>> assert all(isinstance(r["retryable"], bool) and r["next_action"] for r in m["reasons"].values())
>>> assert set(re.findall(r'^\| `(FC-F\d+)` \|', doc, re.M)) == {f"FC-F{n:02}" for n in range(1, 13)}
>>> assert re.fullmatch(r"[0-9a-f]{40}", m["source_sha"])
>>> print(json.dumps({"schema":m["schema"],"keys":len(m["keys"]),"relay_fields":sum(len(r["fields"]) for r in m["relays"]),"status":"pass"}, sort_keys=True))
{"keys": 2, "relay_fields": 10, "schema": "dashboard.feed_config.v1", "status": "pass"}

```

The offline check verifies manifest/classification, source anchors and fixture
coverage only. Runtime fixture oracles are proposed for independent acceptance
tests using mock Discord/source IO, injected clocks and disposable agent-testdb
or CI stores. None were executed here. No staging/production DB tests, settings
mutations, source sampling or delivery probes were performed.

## Intent gate and bounded follow-ups

Before **any** build card, bind CEO/host intent alignment to the exact task
specification revision, repository commit and this file's SHA-256. A request
queued for review is not acceptance. Record alignment through the existing
intent path, not a new owner approval hop. Review by 2026-10-31; no answer means
**no-build**, never timeout approval. New auth/data/network boundaries require
actual CISO disposition; documenting existing fences creates no new permission.

After acceptance only: `FC-W1` shares typed cadence validation with the real
reader; `FC-W2` defines missing signed relay reads and mutations with scoped
validation, audit, idempotency and a concurrency token honored by **all** slash
and dashboard writers; `FC-W3` proves web authority and truthful storage/runtime
state, including overlays, boot and activation. Each prerequisite needs its own
single-repo card and independent tests. A later web page belongs in its own web
project/card, not this bot checkout. If evidence is insufficient, keep every
control read-only or choose no-build; do not substitute generic settings for
relay CRUD. Pending pause/resume and command-live-gate PRs remain independently
owned and are not prerequisites for writing this design.

Metric at intent review: both existing global keys and all relay field groups
have reader/state decisions; every fixture has an accepted or rejected oracle.
Unverified: web guards, live deployment identity/gates, source/channel validity,
operator overrides, runtime availability and production behavior.

ACT: publish a source-backed proposal and offline check. WHY: prevent stored
configuration from masquerading as applied delivery. UNDO: revert these docs;
there is no operational state change to undo.
