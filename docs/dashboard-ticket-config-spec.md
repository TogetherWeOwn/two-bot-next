# Dashboard ticket configuration contract

Status: **Deferred to the two-web-next roadmap** (owner decision 2026-11-01).
Build the bot half only when a two-web-next goal claims this page. Original
proposal: written 2026-10-25. This proposal adds no runtime, page, migration,
permission, live write or activation. Publication is not implementation or
production approval. This file is the single repository source, including the
manifest and fixture oracle. It extends neither the voice/moderation pages nor
the separate [onboarding/self-role
proposal](dashboard-onboarding-self-roles-spec.md).

## Decision

A guild admin needs to know what a ticket setting controls, what was stored,
and whether the bot could consume it. A stored row is **not** an applied value.
Ticket contents, members, transcripts, collaborators and the staff queue are
not configuration; this page never reads those records.

**Recommend wiring-first, not a destination editor over today's store.**
Inventory at source commit `0b8f8c6427859a606c4be710224dc68649fabe00` finds four
live ticket keys, but boot/activation fences, unvalidated destinations, recovery
transitions and an unobservable override layer prevent a truthful general editor.
After intent acceptance, the smallest eventual page can show four stored
configuration rows, enable only cooldown save/reset, and keep destination rows
read-only. Destination editing waits for W1/W2 below. If prerequisites cannot be
proved, choose **no-build**, not a nonfunctional settings form. Nothing is built
by this spec, including the narrowed cooldown page.

## Source-backed inventory

All four keys are `Hot` and `HOT_WIRED`, not merely permitted by the catalogue
(`crates/core/src/settings.rs:209`, `:223-226`, `:285-287`, `:309`). They share the
settings writer and live snapshot below. There is no ticket-specific `Cold`
key or stored-unwired member of these four at the inspected commit. Boot-only
prerequisites are a different state from a live row in an existing runtime.

| Stable ID / key | Actual reader, default and apply timing | Proposed page state |
| --- | --- | --- |
| `TC-K1` / `DISCORD_TICKET_CATEGORY_ID` | No default; required canonical nonzero u64 ID, distinct from panel. `TicketConfig::from_vars` (`crates/bot/src/ticket_runtime.rs:424-429`), then live config on Open (`:1074-1087`) validates same-guild category type 4 and supplies new-channel placement (`:1127-1134`). Next relevant operation after poll, not an existing-channel move. | `read_only_transition` until W1/W2 |
| `TC-K2` / `DISCORD_TICKET_PANEL_CHANNEL_ID` | No default; same parser. `ensure_panel` reads live config (`ticket_runtime.rs:1831-1855`), validates exact guild/ID and type 0 GuildText (`:1265-1283`), proves history access and checks 50 recent messages. Runs on startup/recovery, nominal 300 s cadence (`crates/core/src/tickets.rs:12`; `ticket_runtime.rs:1498-1502`), not immediately on save. No old-panel removal. | `read_only_transition` until W1/W2 |
| `TC-K3` / `DISCORD_TICKET_STAFF_ROLE_ID` | No default; same parser, rejects guild/@everyone ID. Live authorization (`ticket_runtime.rs:951-981`) accepts this role OR ManageChannels OR Administrator (`crates/core/src/tickets.rs:106-125`); channel-create ACL uses it (`crates/discord/src/executor/tickets.rs:46-94`). Next authority check/new create after poll; existing ACLs are not migrated. | `read_only_transition` until W1/W2 and any new access-boundary disposition |
| `TC-K4` / `TWO_TICKET_COOLDOWN_SECONDS` | Boot default 300 s (`crates/core/src/tickets.rs:11`); parsed at boot (`ticket_runtime.rs:395-406`) and refresh (`:908-915`); passed to `reserve` on Open (`:1085-1087`; `crates/cutover/src/tickets.rs:51-82`). Eligibility uses last-created time and saturating arithmetic; active ticket always wins (`crates/core/src/tickets.rs:143-163`). Next admission after poll, including prior creation history. | `candidate_editable` after W3/intent; never applied acknowledgement |

**Shared live path:** `layered_vars` selects only these four keys for the
configured guild (`crates/bot/src/ticket_runtime.rs:355-381`); `live_config`
refreshes lazily per operation (`:897-929`). The 15 s settings job publishes a
snapshot only after a successful fetch/validation
(`crates/core/src/settings.rs:51`; `crates/bot/src/settings_jobs.rs:152-173`).
A failed poll retains the published snapshot. An invalid destination tuple keeps
the **entire** previous ticket config, potentially holding back a valid cooldown
change too (`ticket_runtime.rs:912-920`). A malformed live cooldown retains the
current cooldown (`:908-911`); neither condition is a successful apply.

**Boot/restart-only, not a cold row:** construction requires a valid environment
destination triple and cooldown (`ticket_runtime.rs:395-438`;
`crates/bot/src/command_runtime.rs:489-509`). Stored values cannot bootstrap an
absent runtime. The triple also influences privileged gateway intents at boot
(`crates/bot/src/gateway.rs:152-163`; `crates/discord/src/intents.rs:64-72`).
Tickets are not cleared for the recognized live identity at this source head:
`LIVE_CLEARED_CAPABILITIES` contains SelfRoles only
(`crates/core/src/activation.rs:51-52`, `:78-96`). Preserve that fence.

**Environment/security-only:** `DISCORD_GUILD_ID`/`DISCORD_STAGING_GUILD_ID`
are declared EnvOnly (`crates/core/src/settings.rs:105-106`);
`TWO_INTERNAL_*` is prefix-refused (`:1071-1080`). Get and set both refuse these
keys, so show only a deployment-managed reason, never a value or edit control.
No tickets-enable or ticket-dry-run setting exists. Maximum active tickets
(one per opener/guild), panel copy/buttons, naming, recovery/purge cadence and
retention are code-owned, not hidden stored settings
(`crates/core/src/tickets.rs:8-18`, `:143-163`). Intake forms/multiple panels
have no configuration reader in the four-key config (`ticket_runtime.rs:385-392`).
Show absent controls as unavailable, not empty editable fields.

**Operator precedence:** Rust permits an /ops overlay for all four live keys,
ahead of the guild row (`crates/core/src/settings.rs:367-374`, `:601-608`,
`:630-638`). Normal Worker /ops PUT allowlisting uses `FORWARDED_FLAGS`
(`wrangler/src/ops-control.ts:70`): cooldown is included
(`wrangler/src/container-env.ts:66`), destinations are separately forwarded
Discord IDs (`:156-158`), not ordinary /ops PUT flags. The overlay can mask a
cooldown write/reset; settings.get does not expose it or its epoch/expiry. Do
not add an override editor or assume no overlay just because get says store.

**Destination transition evidence:** `verify_creation_channel` compares old
rows against the **current** category/staff config
(`ticket_runtime.rs:1286-1308`); rollback and control repair use that proof
(`:1315-1338`, `:1774-1803`). Existing tests explicitly pin old-destination
uncertainty (`crates/bot/src/ticket_destination_move_tests.rs:1-4`). Old staff
can retain channel access while new staff lack it. The only settings drain
check guards self-role panels (`crates/cutover/src/settings.rs:211-240`), not
tickets. Gateway shutdown cancellation is not a config-edit drain primitive.

### Stores and writers

- **Configuration:** `guild_settings`, its CAS allocator and revision, plus
  `guild_settings_audit`; schema starts at
  `crates/cutover/migrations/0330_guild_settings.sql` and CAS is
  `crates/cutover/migrations/0334_guild_settings_cas.sql`.
  `SettingsStore::set_if_version` validates before SQL, takes the revision lock,
  compares CAS, writes/deletes and audits in one transaction
  (`crates/cutover/src/settings.rs:177-209`, `:242-288`). Get selects stored
  value and CAS only (`:132-144`), not runtime state or environment defaults.
- **Transport:** existing signed `settings.get` / `settings.set` only
  (`crates/core/src/internal_settings.rs:35-82`). Both handlers pin the staging
  guild constant, not a requested guild
  (`crates/bot/src/internal_action_http.rs:2626`, `:2662`). The private receiver
  and `TWO_INTERNAL_ALLOW_SETTINGS` gate remain unchanged
  (`crates/core/src/internal_action_config.rs:35-86`;
  `crates/core/src/internal_actions.rs:846-848`). A MAC authenticates a caller,
  not an admin session; the web's permission checks are a separate prerequisite.
- **Attribution:** derive `updated_by` from the authenticated admin's Discord
  identity on the server, never editable browser input. The receiver validates
  ID shape, not that identity's authority (`internal_settings.rs:46`);
  each committed change adds an audit transition with actor/time
  (`crates/cutover/src/settings.rs:277-288`). `settings.get` exposes neither
  previous editor nor timestamp. Show no guessed name, history or audit rows;
  metadata readback needs its own accepted scope, not direct web DB grants.
- **Runtime records:** ticket lifecycle/admission rows are not settings, and
  must not be queried to populate this page. Recovery remains with the ticket
  service and authorized operator, not a dashboard ticket-data endpoint.

## Minimal private page contract

Inherited web shell and signer are **not inspected in this bot-only change**.
Before any page build they must prove: private, non-cacheable admin responses;
CSRF protection; current guild membership and Manage Server authority checked
server-side on reads and writes; no signing key in browser code; and session
revocation/failure denial. No raw payload, Discord API body, ticket/member data
or role mentions in logs, alerts, errors or previews. Show role/channel names
as escaped inert text only when already available to this admin, never pings.
No credentials, transcript links or environment values are rendered.

### View and change

- Read only the four whitelisted stored keys via separate `settings.get`
  actions; `source=store` means **stored override**, `source=unset` means
  **inherited; value not exposed**. The runtime fallback is not returned, so do
  not display a guessed deployment value as current. A documented code default
  (cooldown 300 s) may be explanatory text, not an effective-value claim.
- Destination rows show their stored ID (or inherited status), purpose and
  read-only reason. Cooldown is editable **only after accepted prerequisites**;
  changing it affects subsequent open eligibility, not retroactive ticket data.
  Use one-row saves, not an invented atomic multi-key action. No publish/move
  panel, create category, grant role, close ticket, pause service or restart button.
- Unset is a distinct confirmed **Use inherited value** action sending JSON
  `null`, not `0` or an empty string. It is a CAS-protected, audited change. It
  restores environment/code fallback after refresh, subject to the same gates
  and override precedence; it does not undo Discord side effects. Destination
  unsets are disabled with destination edits. Undo a cooldown change by another
  deliberate CAS save of the prior known stored value, or null if previously
  absent; never roll back a whole DB or blindly overwrite another admin's save.

### Safe validation and transitions

Existing generic validation bounds JSON-encoded values to 8192 bytes and rejects
decoded U+0000 (`crates/core/src/internal_actions.rs:1469-1486`;
`crates/core/src/internal_settings.rs:54-73`). Cooldown's store validator accepts
values rendering as `u64` seconds (`crates/core/src/settings.rs:1096-1105`);
this is not a business-approved maximum or type-safe UI contract. The proposed
editor sends a canonical decimal **string**, `0..18446744073709551615`, using
string/BigInt parsing, never a lossy JavaScript number. Zero is no cooldown,
not reset. Reject signs, fractions, exponent notation, whitespace, booleans,
arrays, objects, empty text, overflow and negative values locally; store still
validates. Show the unusually large duration before confirmation. Null alone
means reset. No new upper ceiling is invented here.

Before destination controls could be enabled, W1 must verify a canonical nonzero
Discord ID, same guild and live object existence/type/permissions at the
server's trusted boundary, not a browser-only dropdown. Category must be a
category; panel must be a distinct, usable GuildText destination; staff role
must be same-guild, non-managed, below the bot and not `@everyone`. Check bot
capabilities and private channel overwrites so changes cannot expose tickets.
Do not grant a role or change permission overwrites as validation. Missing or
stale facts, inaccessible objects and Discord failures refuse safely with
`VALIDATION_UNAVAILABLE`, not a guessed pass; no raw foreign-object oracle.
Re-check at effect time; write-time checks cannot eliminate later deletion or
permission changes. Current storage validates none of those object facts.

W2 must define safe destination transitions and drain/reconciliation behavior
before those writes **or resets** are exposed. Do not silently repoint existing
channels, orphan a pending create/delete/control repair, or bypass an unfinished
operation. A per-row CAS token is not a ticket drain lock, nor an atomic triple
transition. A browser warning alone is insufficient. Changing destinations is
not a substitute for enabling a runtime or clearing the activation fence.

### CAS, idempotency and dry-run

Get returns `{ok,result:{key,value,source},version,request_id}`; save returns
`{ok,result:{key,outcome},version,request_id}` with no submitted value
(`crates/bot/src/internal_action_http.rs:2782-2809`). Always supply the observed
integer `expected_version`; omission would allow legacy unconditional writes.
Treat it as opaque equality, never revision ordering. `0` expects absence;
negative tokens are normal. Persisted CAS survives restarts, but absence cannot
detect a create/delete cycle (`docs/internal-action-settings.md`).

For each confirmed save allocate a fresh idempotency key and retain the exact
body bytes. Retry the same intent only with the **same key and body**, a fresh
nonce/timestamp and signature; editing the value or CAS means a new intent/key.
Receiver claims before writing (`internal_action_http.rs:2672-2688`); replay
never repeats DML/audit. A replay success rebuilds the earlier outcome but
side-reads the **current** CAS (`:2729-2776`); it is not proof the current value
still equals that save. Re-read before editing or undo. `needs_reconciliation`
means unknown, never create a fresh key to force a retry.

**Dry-run is local preview only:** deterministic
`{schema,guild_id,key,expected_version,would,reason,retryable,next_action}` for
identical inputs, without network, signing, nonce burn, idempotency reservation,
SQL, audit or Discord calls. It checks manifest/shape rules only, not live role
or channel facts, CAS freshness, drain or actual application. There is no
server dry-run verb/guarantee; do not append `dry_run:true` to `settings.set`
and assume it prevents a write. Ticket dry-run as a runtime gate is absent.

### Honest state and failure UX

No page state is named `applied`. Save and later get confirm **storage only**.
A poll cadence is not a latency SLA: next successful poll plus the next relevant
operation is the expected path; paused/failed polls, boot fallback, activation
and operator overrides can delay or mask it indefinitely. On read failure keep
an already displayed value labeled stale with its observation time, disable
save/reset until fresh get, and retain unsent input privately. Never substitute
an empty/default row for an unavailable read. No durable offline save queue.

| Stable page reason | Source / copy | Next action |
| --- | --- | --- |
| `STORED_UNVERIFIED` | Confirmed save: "Saved to configuration. Bot application is not confirmed." | `reread_before_edit` |
| `INHERITED_UNVERIFIED` | Get unset: "Using inherited configuration; its value is not available here." | `none` |
| `PREVIEW_ONLY` | "Preview only. Nothing sent or saved." | `confirm_or_edit` |
| `VERSION_CONFLICT` | Wire 409 `version_conflict`: "Someone changed this setting. Your draft is kept; reload before saving." | `reread_then_reconfirm` |
| `INVALID_VALUE` | Local validation or generic wire `malformed`: field-level local reason only, no echoed input | `fix_or_reread` |
| `VALIDATION_UNAVAILABLE` | Trusted object facts unavailable; destination controls stay disabled | `wait_for_validation` |
| `RECOVERY_PENDING` | Future W2 refusal, not an existing ticket settings error | `wait_for_drain` |
| `NOT_EDITABLE` | Destination/boot/env/absent row; no signed write | `none` |
| `FOREIGN_GUILD` | Session guild not staging-pinned deployment guild; no signed get/set | `select_managed_guild` |
| `UNAUTHORIZED` | Session/admin/signature/gate refusal; do not expose policy details | `reauthenticate_or_stop` |
| `RUNTIME_UNAVAILABLE` | Read unavailable: "The bot did not answer. Editing is unavailable." | `retry_read` |
| `COMMIT_UNKNOWN` | Write timeout/5xx/409 `needs_reconciliation`: "We could not confirm the save. Do not submit a different save yet." | `reconcile_same_intent` |

These are **proposed adapter reasons**, not new bot wire fields. Existing wire
errors expose only `code,message,retryable` and `request_id`; `log_reason` is
not forwarded (`internal_action_http.rs:3267-3271`, `:3298-3321`). Do not pretend
the client receives `settings_value_invalid` or a ticket drain reason today.
Respect `Retry-After` where supplied; `in_progress` retains the same intent.
Use the matching reason's next_action, not automatic retry of all 409s.

Guild fence: a body `guild_id` cannot select a settings guild; an authorized but
wrong-guild caller would otherwise write staging. The web must compare trusted
session guild to deployment guild **before signing**, for both reads and saves.
This proposal offers no production or multi-guild settings surface. Never
retarget the receiver, reveal environment identity, enable an action gate or
widen a signed/bearer scope to make the page work.

## Machine-readable contract and offline oracle

Stable manifest IDs are proposal-local, not new bot keys or wire enums.
`candidate_editable` is conditional, not launch authorization. Evidence anchors
below name real source readers; the offline check validates their presence,
not runtime effects. Classification `absent` has no catalogue key to send.

```json
{
  "schema": "dashboard.ticket_config.v1",
  "source_sha": "0b8f8c6427859a606c4be710224dc68649fabe00",
  "keys": [
    {"id":"TC-K1","key":"DISCORD_TICKET_CATEGORY_ID","class":"hot","hot_wired":true,"state":"read_only_transition","apply":"poll_then_open_or_recovery","gate":["TC-W1","TC-W2"],"evidence":["E-CATALOG","E-WRITE","E-LAYER","E-CONFIG","E-OPEN","E-RECEIPT"]},
    {"id":"TC-K2","key":"DISCORD_TICKET_PANEL_CHANNEL_ID","class":"hot","hot_wired":true,"state":"read_only_transition","apply":"poll_then_panel_recovery","gate":["TC-W1","TC-W2"],"evidence":["E-CATALOG","E-WRITE","E-LAYER","E-CONFIG","E-PANEL"]},
    {"id":"TC-K3","key":"DISCORD_TICKET_STAFF_ROLE_ID","class":"hot","hot_wired":true,"state":"read_only_transition","apply":"poll_then_authorize_or_create","gate":["TC-W1","TC-W2"],"evidence":["E-CATALOG","E-WRITE","E-LAYER","E-CONFIG","E-AUTH","E-RECEIPT"]},
    {"id":"TC-K4","key":"TWO_TICKET_COOLDOWN_SECONDS","class":"hot","hot_wired":true,"state":"candidate_editable","apply":"poll_then_open","gate":["TC-W3"],"evidence":["E-CATALOG","E-WRITE","E-LAYER","E-REFRESH","E-OPEN","E-COOLDOWN"]}
  ],
  "other_states": [
    {"id":"TC-S1","state":"restart_only","key":null,"reason":"runtime_construction_and_gateway_intents_use_boot_env","evidence":["E-BOOT","E-CONSTRUCT"]},
    {"id":"TC-S2","state":"env_only","key":"DISCORD_GUILD_ID","reason":"deployment_identity_never_get_or_set","evidence":["E-GUILD"]},
    {"id":"TC-S3","state":"env_only","key":"DISCORD_STAGING_GUILD_ID","reason":"deployment_identity_never_get_or_set","evidence":["E-STAGING-GUILD"]},
    {"id":"TC-S4","state":"env_only","key":"TWO_INTERNAL_*","reason":"security_namespace_prefix_refused","evidence":["E-ENV-FENCE"]},
    {"id":"TC-S5","state":"stored_unwired","key":null,"reason":"none_of_the_four_existing_keys_is_permanently_unwired","evidence":["E-LAYER","E-REFRESH"]},
    {"id":"TC-S6","state":"absent","key":null,"reason":"no_ticket_enable_dry_run_form_panel_copy_or_limit_setting","evidence":["E-KEYSET","E-CONFIG","E-CONSTANTS"]},
    {"id":"TC-S7","state":"activation_denied","key":null,"reason":"recognized_live_identity_not_cleared_for_tickets","evidence":["E-ACTIVATION"]}
  ],
  "evidence": {
    "E-CATALOG":{"path":"crates/core/src/settings.rs","line":277,"contains":"pub const HOT_WIRED"},
    "E-WRITE":{"path":"crates/cutover/src/settings.rs","line":177,"contains":"pub async fn set_if_version"},
    "E-LAYER":{"path":"crates/bot/src/ticket_runtime.rs","line":369,"contains":"fn layered_vars"},
    "E-CONFIG":{"path":"crates/bot/src/ticket_runtime.rs","line":419,"contains":"fn from_vars"},
    "E-OPEN":{"path":"crates/bot/src/ticket_runtime.rs","line":1087,"contains":"config.cooldown_seconds"},
    "E-RECEIPT":{"path":"crates/bot/src/ticket_runtime.rs","line":1286,"contains":"fn verify_creation_channel"},
    "E-PANEL":{"path":"crates/bot/src/ticket_runtime.rs","line":1831,"contains":"fn ensure_panel"},
    "E-AUTH":{"path":"crates/bot/src/ticket_runtime.rs","line":956,"contains":"self.live_config()"},
    "E-REFRESH":{"path":"crates/bot/src/ticket_runtime.rs","line":897,"contains":"fn refresh_config_with"},
    "E-COOLDOWN":{"path":"crates/core/src/tickets.rs","line":11,"contains":"COOLDOWN_SECONDS: u64 = 300"},
    "E-BOOT":{"path":"crates/bot/src/ticket_runtime.rs","line":395,"contains":"fn from_env"},
    "E-CONSTRUCT":{"path":"crates/bot/src/command_runtime.rs","line":492,"contains":"LiveCapability::Tickets"},
    "E-GUILD":{"path":"crates/core/src/settings.rs","line":105,"contains":"SettingClass::EnvOnly"},
    "E-STAGING-GUILD":{"path":"crates/core/src/settings.rs","line":106,"contains":"SettingClass::EnvOnly"},
    "E-ENV-FENCE":{"path":"crates/core/src/settings.rs","line":1071,"contains":"ENV_ONLY_KEY_PREFIXES"},
    "E-KEYSET":{"path":"crates/bot/src/ticket_runtime.rs","line":355,"contains":"const LIVE_KEYS"},
    "E-CONSTANTS":{"path":"crates/core/src/tickets.rs","line":17,"contains":"PANEL_TEXT"},
    "E-ACTIVATION":{"path":"crates/core/src/activation.rs","line":52,"contains":"&[LiveCapability::SelfRoles]"}
  },
  "reasons": {
    "STORED_UNVERIFIED":{"retryable":false,"next_action":"reread_before_edit"},
    "INHERITED_UNVERIFIED":{"retryable":false,"next_action":"none"},
    "PREVIEW_ONLY":{"retryable":false,"next_action":"confirm_or_edit"},
    "VERSION_CONFLICT":{"retryable":false,"next_action":"reread_then_reconfirm"},
    "INVALID_VALUE":{"retryable":false,"next_action":"fix_or_reread"},
    "VALIDATION_UNAVAILABLE":{"retryable":true,"next_action":"wait_for_validation"},
    "RECOVERY_PENDING":{"retryable":true,"next_action":"wait_for_drain"},
    "NOT_EDITABLE":{"retryable":false,"next_action":"none"},
    "FOREIGN_GUILD":{"retryable":false,"next_action":"select_managed_guild"},
    "UNAUTHORIZED":{"retryable":false,"next_action":"reauthenticate_or_stop"},
    "RUNTIME_UNAVAILABLE":{"retryable":true,"next_action":"retry_read"},
    "COMMIT_UNKNOWN":{"retryable":false,"next_action":"reconcile_same_intent"}
  }
}
```

Agent result shape (proposed, not a new endpoint):
`{schema,source_sha,guild_id,key,expected_version,request_id,reason,retryable,next_action}`.
No value-bearing logs; previews may show only the admin's private configuration
draft. A source/hash mismatch requires re-inventory, not optimistic editing.

| Fixture | Deterministic oracle for future protected acceptance tests |
| --- | --- |
| `TC-F01` | For each of four keys, write via signed settings/CAS to disposable guild A, poll with injected clock, call its actual runtime consumer; value must affect the intended operation. Readback via settings.get alone does not pass. Destination page writes remain disabled until W1/W2. |
| `TC-F02` | Cooldown invalids above, JSON NUL and >8192-byte value: no changed settings/audit/revision. Exercise direct generic store behavior separately; do not assume UI's stricter canonical syntax is enforced there. |
| `TC-F03` | A/B get token t; A saves; B saves t: only A commits, B receives conflict and retains draft. Two creators expecting 0: one success. |
| `TC-F04` | Slash/dashboard writer premise: no ticket slash writer exists; `/setup` is voice-only (`crates/core/src/voice_rooms.rs:1557-1566`), and tickets use Open/Claim/Close components (`crates/core/src/tickets.rs:20-35`). Pin zero ticket-setting DML from `/setup`; race actual Open component consumption with cooldown save/poll (old snapshot before publication, new afterward). Separately race another signed settings writer to prove CAS/actor attribution; invent no slash settings verb. |
| `TC-F05` | Replay same key/body: one change/audit; different body under key: refusal. After a later save, replay old success returns current token; fresh get required. Side-read failure/terminal commit ambiguity: COMMIT_UNKNOWN, not success. |
| `TC-F06` | Foreign guild B: web blocks before signing, zero requests; a B row in fixture cannot affect A runtime. Independently pin receiver's staging guild behavior, not an invented body-guild denial. |
| `TC-F07` | Restart with valid boot triple and stored overrides: boot fallback before first successful poll, stored values afterward, CAS persists. Restart missing boot triple or denied capability: stored rows cannot create/activate runtime. |
| `TC-F08` | Unset cooldown: audited delete, version 0, fallback after refresh. With operator overlay still active, inherited fallback is masked; never label reset applied. |
| `TC-F09` | Failed/parked poll, runtime unavailable, rejected snapshot or bad stored value: retain correct last-good/boot behavior per consumer; UI stale/read-only, never applied. |
| `TC-F10` | Future destination edits/unsets with pending create/repair/open work: refuse/drain safely before change; live role/category/channel foreign/deleted/wrong-kind/permissions failure: no save. Prove existing unsafe behavior first; do not modify Test Author tests to fit it. |
| `TC-F11` | Dry-run identical input gives identical result; zero network/signatures/nonce/idempotency/SQL/Discord effects. EnvOnly, boot-only, absent controls have no edit action. |
| `TC-F12` | Private admin session missing/revoked, non-admin or cross-guild: no config value returned and no signature. Mention text cannot notify, errors cannot leak submitted input. |

All runtime fixtures above are **planned**, not executed by this proposal.
Use disposable agent-testdb/CI databases, isolated guild/channel fixtures, an
injected poll clock and mock Discord. No staging/production DB or live settings
writes. Future staging application evidence is a separately authorized gate,
not a test this document claims to have run.

Runnable offline consistency check (Python stdlib, from repo root):

```sh
python3 -m doctest -v docs/dashboard-ticket-config-spec.md
python3 scripts/check_docs_index.py --json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
```

This checks inventory/class/hot-wired parity, evidence file/line anchors,
state/gate structure and reason remediations. It runs **none** of TC-F01–F12,
performs no runtime/DB test, and proves no apply latency or web authority.
Changed source anchors intentionally require re-inventory, not a waiver.

```pycon
>>> import json, pathlib, re
>>> doc = pathlib.Path("docs/dashboard-ticket-config-spec.md").read_text()
>>> fence = "`" * 3
>>> m = json.loads(re.search(fence + r"json\n(.*?)\n" + fence, doc, re.S).group(1))
>>> rows = m["keys"]; src = pathlib.Path("crates/core/src/settings.rs").read_text()
>>> table = src.split("pub const SETTING_CLASSES", 1)[1].split("\n];", 1)[0]
>>> classes = dict(re.findall(r'\(\s*"(\w+)",\s*SettingClass::(\w+),?\s*\)', table))
>>> wired = set(re.findall(r'"(\w+)"', src.split("pub const HOT_WIRED", 1)[1].split("\n];", 1)[0]))
>>> assert {r["key"] for r in rows} == {k for k in classes if "TICKET" in k}
>>> assert len(rows) == len({r["id"] for r in rows}) == 4
>>> assert all(r["class"] == "hot" and classes[r["key"]] == "Hot" and r["hot_wired"] == (r["key"] in wired) for r in rows)
>>> assert [r["id"] for r in rows if r["state"] == "candidate_editable"] == ["TC-K4"]
>>> assert all(r["state"] == "read_only_transition" and r["gate"] == ["TC-W1", "TC-W2"] for r in rows[:3])
>>> assert rows[3]["gate"] == ["TC-W3"]
>>> ids = [r["id"] for r in rows + m["other_states"]]; assert len(ids) == len(set(ids))
>>> assert {s["state"] for s in m["other_states"]} == {"restart_only", "env_only", "stored_unwired", "absent", "activation_denied"}
>>> assert all(r["evidence"] and all(e in m["evidence"] for e in r["evidence"]) for r in rows + m["other_states"])
>>> assert all(e["contains"] in pathlib.Path(e["path"]).read_text().splitlines()[e["line"]-1] for e in m["evidence"].values())
>>> assert all(isinstance(r["retryable"], bool) and r["next_action"] for r in m["reasons"].values())
>>> assert set(m["reasons"]) == set(re.findall(r'^\| `([A-Z_]+)` \|', doc, re.M))
>>> assert len(set(re.findall(r'^\| `(TC-F\d+)` \|', doc, re.M))) == 12
>>> assert re.fullmatch(r"[0-9a-f]{40}", m["source_sha"])
>>> print(json.dumps({"schema": m["schema"], "keys": len(rows), "status": "pass"}, sort_keys=True))
{"keys": 4, "schema": "dashboard.ticket_config.v1", "status": "pass"}

```

## Intent gate and bounded follow-ups

| ID | Recommendation, not a build card or API |
| --- | --- |
| `TC-W1` | Reuse actual parsers and add trusted same-guild object/type/permission validation for destinations. Fail closed when facts unavailable; no new data reader or permission grant is assumed. |
| `TC-W2` | Resolve destination-change/reset versus pending lifecycle/recovery work under a defined transition fence. Decide whether safe drain or per-ticket immutable configuration is required; independent acceptance tests first. |
| `TC-W3` | Before even cooldown editing, prove web private-admin/guild checks, signer, existing activation, and unobservable /ops precedence handling. If no trusted effective-state evidence is available, explicitly keep storage-only copy or no-build; invent no diagnostics endpoint. |

Bind the CEO intent-alignment request to the exact task-document revision,
repository commit and this file's SHA-256. Ask them to record host intent
alignment through the existing authorized path, not a new owner approval hop.
A queued request is not alignment. No build cards before explicit acceptance
and recorded alignment; no activation, spend or permission is granted by either.
Only genuinely new access/data boundaries require CISO disposition. In
particular, a future staff-role/category editor changing who can see tickets
must resolve that boundary before enablement; docs and a fenced cooldown form
do not themselves authorize or require a new credential/data grant.

Review by 2026-10-31. Metric: all four keys have evidenced reader/state decisions
and all 12 fixture IDs have accepted/rejected oracles. If evidence is incomplete,
keep wiring-first/no-build; absent acceptance defaults to **no-build**, never
timeout approval. No live experiment or apply-delay measurement was performed.
Unverified: web guards, deployed capabilities/gates, actual guild objects,
operator overlay state, runtime availability and production behavior.

ACT: publish a source-backed proposal and offline consistency check.
WHY: prevent stored-but-unusable ticket configuration from masquerading as live
application. UNDO: revert these documentation changes; no operational state
changes to undo.
