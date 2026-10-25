# Dashboard leveling reward configuration

Status: **proposed; design only**, 2026-10-25. Review by 2026-11-01.
This is the authoritative repository contract, including its offline fixture
oracle. No page, runtime, schema, permission, activation or production change is
implemented or authorized. Bot-source baseline:
`c3af7333df80a766f5d4c229506a3bce53680828`. The separate web project has not been
inspected; this inventory does not establish whether it already has a page.

## Decision and alternatives

Guild staff need to configure existing level-to-role rewards without mistaking
**saved configuration** for **roles applied**. Recommend a minimal private ladder
editor only after the missing read/write, CAS, authority and attribution contracts
below are accepted and implemented. Until then choose **no-build**, not an editor
that writes an invented settings key or uses a role-grant endpoint.

Current/no-build alternative: the operator `levels-role-rewards` CLI reads or
replaces a ladder; `/rank` and `/leaderboard` read XP. This avoids a new surface,
but offers neither staff self-service nor stale-edit protection. A read-only page
would still require a bounded authorized read contract; it is not free wiring.
Reject a general XP economy editor, new admin gateway, import wizard or
reconciliation console. Existing [ticket](dashboard-ticket-config-spec.md),
[onboarding](dashboard-onboarding-self-roles-spec.md), voice and moderation
contracts retain their ownership.

## What exists, with source evidence

| ID / data class | Actual fields, owner and consumer |
| --- | --- |
| `LR-C1` / saved reward rows | `level_role_rewards(guild_id, level, role_id)`; positive INT4 level, primary key guild+level, unique guild+role; no revision, editor, time or enabled flag (`crates/cutover/migrations/0002_leveling.sql:50-56`). Both stores read ascending levels (`crates/core/src/leveling_store.rs:1030-1048`; `crates/cutover/src/db.rs:645-663`). |
| `LR-C2` / constants, read-only | `crates/core/src/leveling.rs:20-37`: message base 15 XP; independent source cooldown 60 s; voice 5 XP/completed minute; XP ceiling 9007199254740991; message daily-cap guard 150, voice daily cap 120; short-message floor 8 trimmed Unicode scalars; decay grace 28 days. Message policy is first five awards at 15, next five at 5, then zero, with same-channel burst halving; not a flat 15 per message (`:153-205`). Curve is fixed integer arithmetic (`:57-83`). None is a dashboard preference. |
| `LR-C3` / deployment gates | `TWO_ONBOARDING_MODE` is EnvOnly; session suppresses leveling role writes. `TWO_ONBOARDING_DRY_RUN` is catalogued Hot but this consumer receives a **boot-time copy**, not a live guild lookup (`crates/core/src/onboarding.rs:74-131`; `crates/bot/src/main.rs:608-628`; `crates/discord/src/leveling_runtime.rs:684-703`, `:738-740`). Both suppress role writes, not XP. Show deployment-managed/unknown, not an activation toggle or guessed effective value. |
| `LR-C4` / guild settings | The catalogue has no leveling ladder, XP-rate or cooldown key (`crates/core/src/settings.rs:87-270`). `guild_settings` CAS/audit belongs to settings, not reward rows (`crates/cutover/src/settings.rs:172-289`). Storing `TWO_ONBOARDING_DRY_RUN` does not prove a live change to leveling's copied gate. |
| `LR-C5` / actual runtime | Qualifying gateway message/measured voice award commits XP, then an actual lifetime level-up may reread ladder/current roles and grant **all earned missing roles**. No revoke fence is supplied (`crates/discord/src/leveling_runtime.rs:706-781`). No reward polling delay or apply-on-save exists. |
| `LR-C6` / current controls | `levels-role-rewards --guild … [--set level:roleId,...]`: empty set clears, no preview split, live-guild guard retained (`crates/cutover/src/bin/levels_role_rewards.rs:13-71`; `crates/cutover/src/cli.rs:222-239`). `/rank [member]` and `/leaderboard` are registered readers (`crates/core/src/commands.rs:289-302`); runtime makes rank private/lifetime and leaderboard public/current-season top ten (`crates/discord/src/leveling_runtime.rs:811-884`), not a reward editor. |

### Existing validation and replacement are not authority

Core normalization accepts levels 1..2147483647 and 17–20 ASCII-digit role IDs,
including leading zeroes/u64 overflow; every input row is checked, then **last
row per level wins**, ascending (`crates/core/src/leveling.rs:568-611`). Cutover
uses canonical IDs (no leading zero, fits u64), then rejects duplicate roles in
the surviving ladder (`crates/cutover/src/lib.rs:108-116`;
`crates/cutover/src/db.rs:707-741`). Core leaves duplicate-role rejection to SQL's
UNIQUE constraint. Neither limits ladder row count or checks Discord objects,
actor authority, managed roles or hierarchy. Empty replacement is valid.

Both writers take the same guild advisory transaction lock, DELETE then INSERT,
and commit one complete ladder; insert failure rolls back. Serialization prevents
unions, **not stale overwrites**. Neither takes expected-version, writes editor
audit nor changes the settings revision
(`crates/core/src/leveling_store.rs:1051-1092`; `crates/cutover/src/db.rs:666-704`).

Role planning caps each grant/revoke list at ten, not the stored ladder; it
refuses an over-cap plan, never truncates (`crates/core/src/leveling.rs:651-738`).
Execution is sequential, grants first, and returns applied/failed/skipped on first
refusal; prior grants remain (`crates/discord/src/executor.rs:1503-1571`). The
explicit whole-set hierarchy preflight is on **revokes**, not ordinary grants
(`:1542-1544`, `:1584-1625`). A successful save or syntax check cannot promise
Discord will accept a later grant. XP stays committed after grant failure
(`crates/discord/src/leveling_runtime.rs:738-747`). There is no durable reward
application status/outbox in this save path; do not invent a green delivery badge.

### Existing transfer and transport ownership

- The 19 implemented signed verbs contain no leveling verb
  (`crates/core/src/internal_actions.rs:674-694`). `settings.get/set` owns
  catalogued settings only. `automations.import/export` owns custom commands
  (`crates/core/src/automation_transfer.rs:1-16`). Voice `/import`/`/export`
  owns its codec, not rewards (`crates/core/src/voice_config.rs:35-54`).
- The MEE6 reward probe is snapshot-only, not import/application
  (`crates/cutover/src/bin/levels_import_rewards_probe.rs:1-8`). XP import is a
  separate operator tool, not part of this page. Do not revive legacy import or
  activate imported XP/rewards. Saving this ladder creates no imported XP.
- Reuse the existing signed receiver machinery only through an accepted narrow
  extension. Current v1 authenticates raw body/timestamp/nonce, **not** an admin
  session; signing-v2 caller/audience/idempotency binding is still proposed
  ([signing](internal-action-signing-v2.md)). Current settings handlers pin the
  staging guild (`crates/bot/src/internal_action_http.rs:2611-2663`). The Worker
  does not forward optional settings gates; production has no receiver ingress
  ([receiver](internal-actions-receiver.md)). Do not enable gates/ingress here.
- [Admin scope helpers](admin-registry.md) are not a shipped bearer gateway.
  Preserve named-holder, expiry, revocation and guild restrictions; no AI admin
  scope, no new token, secret, DB grant or scope widening. `role.assign` is not
  a configuration save API. Transport authentication is never role authority.

## Proposed minimal page and validation

One guild, one private **Level rewards** section: Level / Role / Remove rows,
Add reward, local Preview, then Save configuration. Show fixed-policy explanation
and the deployment-managed role-write caveat. No XP editor, enable toggle, Apply,
Backfill, Reconcile, Import, role creation or removal-from-members button.

**Trusted boundary:** before read/sign/save, the web server must prove current
session, CSRF protection for mutation, membership in the exact deployment guild
and Manage Guild authority. Writes additionally require Manage Roles and target
roles strictly below the editor's highest role (guild owner may bypass only this
editor hierarchy comparison). Never bypass bot hierarchy, managed-role or
privilege restrictions. Verify bot Manage Roles and same-guild target existence,
not @everyone, not managed, strictly below bot. Reject roles carrying
Administrator, Manage Guild, Manage Roles, Manage Channels, Manage Webhooks,
Ban Members, Kick Members or Moderate Members. Use permission bits, not names.
This conservative deny-set is **proposed policy for CISO disposition**, not a
claim about today's writers or proof that every other permission is harmless.

Resolve role metadata at the trusted server; bound external lookups and fail
closed on unavailable/incomplete facts. Do not fetch members/XP/leaderboards for
this form. Role lookup must be guild-scoped, paginated/bounded by its existing
owner; no new unrestricted enumeration grant is assumed. Recheck facts on save;
they may still change before an organic grant. Do not test authority by granting
a role. Missing authority reveals no config or foreign-object existence.
Private non-cacheable responses, escaped inert names, no mentions or raw remote
errors, no credential/env rendering, no value-bearing logs are prerequisites.
Web guards/signing are **unverified**, not presumed present.

Proposed editor bounds: at most **10 rows**, body at most **8192 UTF-8 bytes**,
level JSON integer 1..2147483647 (not boolean/fraction/string), canonical role ID
string of 17–20 ASCII digits, no leading zero, <=18446744073709551615. Both level
and role must be unique; duplicate levels are an explicit error rather than a
silent last-wins UI. Sort ascending level; row identity is `(guild_id,level)`;
editing a level is remove+add, not an array-index identity. Reject unknown fields,
nulls, numeric IDs, exponents in submitted level tokens, signs, NUL, overflow and
extra nesting at parsing. These are **new narrow editor bounds**, not current
store guarantees. Ten avoids creating a fresh ladder whose entire grant set
exceeds the current planner cap; it does not promise grants will succeed.

A bounded read may inspect 11 ordered rows, return at most 10 plus `complete`.
If more exist or any stored row violates this contract, show read-only
`UNSUPPORTED_LADDER`; never silently truncate into a replacement. Existing larger
ladders remain untouched. Empty is a real empty ladder only after a successful
complete read, not after a timeout. Removing or changing an existing pair,
including Clear all, requires exact-target confirmation of guild, observed
version, removed/added pairs and draft hash. Copy: **“Replace saved rewards for
this guild? Existing member roles will not be removed.”** Recheck binding server-
side; any draft/version/guild change invalidates confirmation. No bulk role undo.

Keyboard: labelled fields/buttons; table order matches ascending levels; error
summary links to fields; focus first invalid field; Add focuses its Level and
Remove returns focus to the neighboring row/Add. Announce save/error status via
a polite live region, never color alone. Keep private drafts on conflict/error,
not in URLs, shared caches or a durable offline mutation queue. Stale read shows
observation time and disables save until reload/reconfirmation; no auto-merge.
Read-only explains why and has no enabled save control. Empty state explains
that no roles are configured, not that the feature or XP is disabled.
Human errors name the remedy without echoing input: “Choose a unique level/role”;
“Choose a non-managed role below you and the bot”; “Role checks are unavailable;
nothing was saved”; “Someone changed these rewards; reload and review your draft.”
On missing staff authority say “You cannot edit this guild's rewards”, without
revealing configuration. Read failure never renders an empty success state.

## Missing narrow contract (not an implemented API)

Propose **`leveling.rewards.get` / `leveling.rewards.set`** on the existing signed
channel, not new generic SQL/settings access. Initial receiver remains pinned to
the existing staging identity. Compare trusted session guild before signing AND
requested guild against receiver identity before reading/writing. Signed body
must bind guild, authenticated human actor, intent ID, expected version, draft
and confirmation; never trust browser `updated_by`. Preserve existing gates and
scope fences; any future bearer integration maps read to read and save to admin
under that integration's accepted contract, never broadens credentials here.

Versioned bounded envelopes (proposed; identifiers below are synthetic):

```text
get request: {schema:"leveling.rewards.v1",guild_id:"10000000000000001"}
get result: {schema,guild_id,version,rows:[{level:1,role_id:"10000000000000002"}],complete:true,observed_at,request_id,application:"unconfirmed"}
set request: {schema,guild_id,actor_id,intent_id,expected_version,rows,confirmation:null|{guild_id,expected_version,draft_sha256}}
set result: {schema,guild_id,intent_id,request_id,committed_version,outcome:"saved"|"unchanged",application:"unconfirmed"}
error: {schema,guild_id,request_id,intent_id?,code,message,retryable,next_action,field?}
```

Version is an opaque bounded decimal **string**, not XP, settings CAS or a body
hash. Proposed persistence must allocate a non-reused per-guild revision including
empty/tombstone state: A→B→A or create→clear cannot revive an old version. Under
the existing guild lock, compare version, validate complete normalized snapshot,
replace rows, increment revision and append attributable before/after config
audit in **one transaction**. Audit binds human actor, stable caller/intent,
guild, versions, server time and prior/new ladders for authorized recovery; it
contains no XP, member-role lists or credentials. Metadata is not a public log.
All writers, including both store paths/operator CLI, must participate or be
explicitly fenced from concurrent writes before enabling the editor. A dashboard-
only CAS overlay is insufficient. No schema or grants are changed by this spec.

Unchanged canonical rows: fresh expected version and authority still required;
return `unchanged`, no reward DML/revision bump/config-change audit. Outer request
security audit may remain. For a real change, retain exact body/key per intent;
replay must not repeat replacement, revision or audit. Same key/different body
refuses. Retry with same key/body, fresh transport nonce/time/signature. The
committed result/version must be durable with intent completion, or ambiguity
must stay `COMMIT_UNKNOWN` and prohibit a fresh-key resubmission. Current outer
idempotency infrastructure alone is **not** proof of atomic config commit and
terminal result. Re-read after replay before editing; another admin may have
subsequently saved. Missing schema/contract/authority means unavailable, not
fallback to the old unconditional writer.

Dry-run means **local deterministic preview** only: identical snapshot/draft/facts
produce identical decision/diff/hash, with zero signing, nonce burn, idempotency
reservation, SQL/audit or Discord effects. It cannot prove live role facts, CAS
freshness or runtime application. No dry-run field sent to an existing mutating
verb makes it inert. Save can commit while a later role grant fails: label
**“Configuration saved. Member role application is not confirmed.”** Never offer
an automatic grant retry, backfill or XP activation. Removing a rung does not
remove granted roles. Future grant/reconciliation authority is a separate slice.

## Reason registry and fixture matrix

These are proposed stable adapter/contract codes, not current bot wire errors.
Every result carries stable guild/request/intent/version IDs where known. Missing
IDs are explicit null/absent, never invented. Retryable read/validation failures
permit a bounded retry with backoff/Retry-After; not automatic write retries.

```json
{
  "schema":"dashboard.leveling_rewards.spec.v1",
  "source_sha":"c3af7333df80a766f5d4c229506a3bce53680828",
  "max_rows":10,
  "max_body_bytes":8192,
  "reasons":{
    "PREVIEW_ONLY":[false,"confirm_or_edit"],
    "UNCHANGED":[false,"none"],
    "STORED_UNVERIFIED":[false,"reread_before_edit"],
    "INVALID_VALUE":[false,"fix_fields"],
    "DUPLICATE_LEVEL":[false,"choose_unique_levels"],
    "DUPLICATE_ROLE":[false,"choose_unique_roles"],
    "ROLE_UNAVAILABLE":[false,"choose_available_role"],
    "ROLE_FORBIDDEN":[false,"choose_eligible_role"],
    "UNAUTHORIZED":[false,"reauthenticate_or_stop"],
    "FOREIGN_GUILD":[false,"select_managed_guild"],
    "VALIDATION_UNAVAILABLE":[true,"retry_validation"],
    "UNSUPPORTED_LADDER":[false,"use_authorized_operator_path"],
    "VERSION_CONFLICT":[false,"reload_then_reconfirm"],
    "CONFIRM_REPLACEMENT":[false,"confirm_exact_target"],
    "READ_UNAVAILABLE":[true,"retry_read"],
    "COMMIT_UNKNOWN":[false,"reconcile_same_intent"]
  },
  "evidence":[
    ["crates/core/src/leveling.rs",591,"pub fn normalize_role_rewards"],
    ["crates/core/src/leveling.rs",655,"ROLE_WRITE_CAP: usize = 10"],
    ["crates/core/src/leveling_store.rs",1063,"pub async fn replace_role_rewards"],
    ["crates/cutover/src/db.rs",710,"pub fn normalize_role_rewards"],
    ["crates/discord/src/leveling_runtime.rs",738,"if result.leveled_up"],
    ["crates/core/src/internal_actions.rs",674,"IMPLEMENTED_ACTIONS"]
  ],
  "fixtures":[
    ["LR-F01","valid","PREVIEW_ONLY"],
    ["LR-F02","empty","UNCHANGED"],
    ["LR-F03","duplicate_level","DUPLICATE_LEVEL"],
    ["LR-F04","duplicate_role","DUPLICATE_ROLE"],
    ["LR-F05","invalid_level","INVALID_VALUE"],
    ["LR-F06","invalid_id","INVALID_VALUE"],
    ["LR-F07","missing_role","ROLE_UNAVAILABLE"],
    ["LR-F08","managed","ROLE_FORBIDDEN"],
    ["LR-F09","above_bot","ROLE_FORBIDDEN"],
    ["LR-F10","privileged","ROLE_FORBIDDEN"],
    ["LR-F11","foreign_guild","FOREIGN_GUILD"],
    ["LR-F12","no_staff_authority","UNAUTHORIZED"],
    ["LR-F13","unchanged","UNCHANGED"],
    ["LR-F14","stale_version","VERSION_CONFLICT"],
    ["LR-F15","facts_unavailable","VALIDATION_UNAVAILABLE"],
    ["LR-F16","clear_existing","CONFIRM_REPLACEMENT"],
    ["LR-F17","confirmed_clear","PREVIEW_ONLY"],
    ["LR-F18","oversize","INVALID_VALUE"],
    ["LR-F19","existing_oversize","UNSUPPORTED_LADDER"],
    ["LR-F20","read_unavailable","READ_UNAVAILABLE"],
    ["LR-F21","write_unknown","COMMIT_UNKNOWN"],
    ["LR-F22","everyone","ROLE_FORBIDDEN"],
    ["LR-F23","above_editor","ROLE_FORBIDDEN"],
    ["LR-F24","no_bot_permission","ROLE_FORBIDDEN"]
  ]
}
```

### Runnable offline oracle (not backend acceptance)

From repo root: `python3 -m doctest -v docs/dashboard-leveling-rewards-spec.md`.
This executes the 24 synthetic decision rows plus lexical bounds, saved/failed-
application and disabled-write presentation below. It checks source anchors,
not real session/Discord facts or DB atomicity. Pure trusted-fact flags are test
inputs, **not browser authority**. Production implementations must not use this
Python model or its simplified confirmation boolean as an authorization layer.

```pycon
>>> import copy, hashlib, json, pathlib, re
>>> text = pathlib.Path("docs/dashboard-leveling-rewards-spec.md").read_text()
>>> fence = "`" * 3
>>> m = json.loads(re.search(fence + r"json\n(.*?)\n" + fence, text, re.S).group(1))
>>> assert all(anchor in pathlib.Path(path).read_text().splitlines()[line-1] for path, line, anchor in m["evidence"])
>>> assert len({r[0] for r in m["fixtures"]}) == len(m["fixtures"]) == 24
>>> assert all(code in m["reasons"] for _, _, code in m["fixtures"])
>>> assert all(type(retry) is bool and action for retry, action in m["reasons"].values())
>>> role = "10000000000000002"
>>> def canonical_id(s):
...     return type(s) is str and re.fullmatch(r"[1-9][0-9]{16,19}", s) is not None and int(s) <= 18446744073709551615
>>> def decide(rows, old=(), *, guild=True, staff=True, complete=True, read=True, unknown=False, version=True, facts="safe", confirmed=False):
...     if not staff: return "UNAUTHORIZED"
...     if not guild: return "FOREIGN_GUILD"
...     if unknown: return "COMMIT_UNKNOWN"
...     if not read: return "READ_UNAVAILABLE"
...     if not complete: return "UNSUPPORTED_LADDER"
...     if not version: return "VERSION_CONFLICT"
...     if type(rows) is not list or len(rows) > m["max_rows"]: return "INVALID_VALUE"
...     if len(json.dumps(rows, ensure_ascii=False).encode()) > m["max_body_bytes"]: return "INVALID_VALUE"
...     for row in rows:
...         if type(row) is not dict or set(row) != {"level", "role_id"}: return "INVALID_VALUE"
...         if type(row["level"]) is not int or not 1 <= row["level"] <= 2147483647 or not canonical_id(row["role_id"]): return "INVALID_VALUE"
...     if len({r["level"] for r in rows}) != len(rows): return "DUPLICATE_LEVEL"
...     if len({r["role_id"] for r in rows}) != len(rows): return "DUPLICATE_ROLE"
...     if facts == "unknown": return "VALIDATION_UNAVAILABLE"
...     if facts == "missing": return "ROLE_UNAVAILABLE"
...     if facts != "safe": return "ROLE_FORBIDDEN"
...     pairs = lambda rs: sorted((r["level"], r["role_id"]) for r in rs)
...     if pairs(rows) == pairs(old): return "UNCHANGED"
...     if set(pairs(old)) - set(pairs(rows)) and not confirmed: return "CONFIRM_REPLACEMENT"
...     return "PREVIEW_ONLY"
>>> a = {"level": 1, "role_id": role}; b = {"level": 2, "role_id": "10000000000000003"}
>>> cases = {name: ([a], {}) for _, name, _ in m["fixtures"]}
>>> cases.update(empty=([], {}), duplicate_level=([a,a], {}), duplicate_role=([a,dict(a,level=2)], {}), invalid_level=([dict(a,level=0)], {}), invalid_id=([dict(a,role_id="0"*17)], {}), missing_role=([a], {"facts":"missing"}), managed=([a], {"facts":"managed"}), above_bot=([a], {"facts":"above_bot"}), privileged=([a], {"facts":"privileged"}), foreign_guild=([a], {"guild":False}), no_staff_authority=([a], {"staff":False}), unchanged=([a], {"old":[a]}), stale_version=([a], {"version":False}), facts_unavailable=([a], {"facts":"unknown"}), clear_existing=([], {"old":[a]}), confirmed_clear=([], {"old":[a],"confirmed":True}), oversize=([a]*11, {}), existing_oversize=([a], {"complete":False}), read_unavailable=([a], {"read":False}), write_unknown=([a], {"unknown":True}), everyone=([a], {"facts":"everyone"}), above_editor=([a], {"facts":"above_editor"}), no_bot_permission=([a], {"facts":"no_bot_permission"}))
>>> before = copy.deepcopy(cases)
>>> for ident, name, expected in m["fixtures"]:
...     rows, flags = cases[name]
...     assert decide(rows, **flags) == expected, ident
...     assert decide(rows, **flags) == decide(rows, **flags), ident
>>> assert before == cases
>>> assert all(decide([dict(a,level=x)]) == "INVALID_VALUE" for x in [True,1.0,"1",-1,2147483648,None])
>>> assert all(not canonical_id(x) for x in [1,None,"", "0"*17,"18446744073709551616","１"*17,"+10000000000000002"])
>>> assert decide([b,a]) == decide([a,b]) == "PREVIEW_ONLY"
>>> draft_hash = lambda rows: hashlib.sha256(json.dumps(sorted(rows,key=lambda r:r["level"]),sort_keys=True,separators=(",",":")).encode()).hexdigest()
>>> assert draft_hash([a,b]) == draft_hash([b,a]) and draft_hash([a]) != draft_hash([b])
>>> assert decide([], old=[a], confirmed=False) == "CONFIRM_REPLACEMENT"  # stale target must be unconfirmed
>>> def presentation(saved, observed_role_failure=False, role_writes_enabled=True):
...     return {"configuration":"STORED_UNVERIFIED" if saved else "COMMIT_UNKNOWN", "application":"unconfirmed", "apply_action":None}
>>> assert presentation(True, True) == presentation(True, False, False) == presentation(True)
>>> print(json.dumps({"schema":m["schema"],"fixtures":len(m["fixtures"]),"status":"pass"},sort_keys=True))
{"fixtures": 24, "schema": "dashboard.leveling_rewards.spec.v1", "status": "pass"}

```

Future protected acceptance must additionally prove with disposable DB/mock
Discord: concurrent A/B save only one commits; create/clear ABA rejects old
version; every writer participates; failed replacement rolls back; exact-target
hash/version/guild tampering refuses; same-intent replay adds one revision/audit;
different-body replay refuses; lost terminal receipt never resubmits under a new
key; unauthorized read returns no data; local preview emits zero external effects;
save emits zero role writes even with imported XP present; next organic level-up
and partial grant refusal preserve XP and truthful saved/application separation.
For each failure assert unchanged config/revision/config-audit, allowing separate
outer security audit. Run keyboard/stale/empty/read-only cases in the web project.
Performance envelope: 10 editable rows/8192-byte request/11-row lookahead, bounded
role metadata fetch; no measured UI latency or runtime SLA claimed. Runtime,
browser, DB, role-grant and staging/production acceptance were **not run here**.

## Exact-revision intent gate and follow-up ownership

Bind the proposed issue spec to this file's commit and SHA-256; request CEO/host
intent alignment and an **independent CISO boundary disposition** against that
exact revision before bot/web build cards. Queued requests, merged docs and silence
are not acceptance. Default on 2026-11-01 without both dispositions: **no-build**.
Metric: every inventory class has source/state evidence and all 24 decision
fixtures have an explicit oracle; not measured demand or production parity.

After both accepted dispositions only: bot contract/CAS/audit and protected tests
belong in this bot project; a separate web-page card belongs in its single-repo
web project and consumes the accepted bot contract. Neither is created here.
Authority, effect-time grant policy and any reconciliation remain independently
gated; no implementation card may imply permission to bypass the receiver,
staging guild, token, imported-XP or existing role-write fences.

ACT: publish proposed source inventory, bounded contract and offline oracle.
WHY: enable truthful staff configuration without equating save with authority or
delivery. UNDO: revert the documentation PR; no operational state was changed.
