# Dashboard self-role panel parity contract

Status: **proposed; design only**. Written 2026-10-26; intent review by
2026-11-02. This proposal adds no runtime, page, migration, permission, live
write or activation. Publication is not implementation or production approval.
This file is the single repository source for panel-parity semantics,
including the manifest and fixture oracle. It narrows the sibling
[onboarding/self-role proposal](dashboard-onboarding-self-roles-spec.md),
which treats the catalogue as one writable blob: blob writes stay, but the
page operates per panel with truthful publish. It extends neither the
voice/moderation pages nor the ticket config page.

## Decision

A guild admin needs to manage self-role panels (add, edit, publish,
unpublish) from the dashboard instead of host environment variables, while
members keep the same pick/verify/unpick buttons. The card premise named a
static env catalogue; re-inventory at source commit
`a6fa0fb7d40e2ee4eaa6bfce9834d639a25c166c` finds the bot-side seam already
landed: the catalogue key is Hot and hot-wired, every dispatch and recovery
sweep re-resolves it store-first with boot fallback, and catalogue writes
are validated by the strict boot parser plus a durable-work drain check.
What does not exist is panel granularity: one key, one CAS token, one
whole-array replace. A naive per-panel editor would read-modify-write the
whole blob, and a careless save could silently unpublish panels the admin
never opened.

**Recommend panel operations over blob writes with truthful publish, not a
raw JSON editor.** After intent acceptance, the smallest eventual surface is
a read-only panel inventory (SR-S1, the <=1-day slice); panel
create/edit/publish/unpublish follow only behind SR-W1/SR-W2. If the
prerequisites cannot be proved, choose **no-build**, not a form that can
blank live pickers. Nothing is built by this spec, including SR-S1.

## Source-backed inventory

### Present vs proposed, from the env catalogue

Only two catalogue keys exist; there is no third self-role setting, no
per-panel publish flag, and no role-existence oracle.

| Key (class / hot-wired) | Present: actual reader and apply timing | Proposed page state |
| --- | --- | --- |
| `TWO_SELF_ROLE_PANELS` (hot / yes) | Store-first live read per dispatch and sweep, boot fallback on absent or unparsable row (`crates/bot/src/self_role_handlers.rs:59-83`, `:220-227`); write-time strict-parser validation (`crates/core/src/settings.rs:1110-1117`); drain check refuses changes while member work is unfinished (`crates/cutover/src/settings.rs:214-240`). Service existence stays boot-gated: an empty boot catalogue builds no service (`self_role_handlers.rs:166-168`). Next poll (15 s) after commit, not immediately on save. | Per-panel operations (SR-C1..C6); whole blob stays the unit of write |
| `TWO_SELF_ROLE_DRY_RUN` (hot / no) | Set once at service construction from the process environment only (`SelfRoleGates::from_env`, `crates/core/src/self_roles.rs:776-790`; `dry_run: gates.dry_run`, `self_role_handlers.rs:146`). A stored row is read by nothing, not even after restart. | `read_only_unwired` (SR-C7) until SR-W3 |

**Shared live path:** the 15 s settings job publishes a snapshot only after
a successful fetch and whole-snapshot validation; a rejected snapshot keeps
the last good one (`crates/bot/src/settings_jobs.rs:152-174`). The live
reader serves hot-wired keys only, and the /ops overlay explicitly excludes
the panels catalogue because an expiring layer cannot join the DB drain
lock (`crates/core/src/settings.rs:373`). Stored `[]` parses to no panels,
which is a live unpublish-all, not a fallback (`self_roles.rs:812-851`,
`self_role_handlers.rs:55-58`).

**Boot/restart-only, not a live row:** construction requires a non-empty
boot catalogue plus the staging allowlist
(`self_role_handlers.rs:131-149`, `:186-215`). Stored rows cannot enable a
boot-empty deployment; enabling needs a restart with a non-empty boot
catalogue. Preflight proves boot panel channels postable and boot roles
existing, unmanaged and below the bot (`crates/bot/src/preflight.rs:291-311`).
SelfRoles is the one live-cleared capability
(`crates/core/src/activation.rs:52`), but serving stays staging-allowlisted
(`self_role_handlers.rs:44-46`).

**Validation layers, in order:** write-time runs the strict catalogue
parser, so a signed write can never store an unparsable row
(`settings.rs:1110-1117`). Dispatch-time revalidates against live Discord:
bot ManageRoles, role existence and safety, deployment-mask drift, color
presence, hierarchy (`self_roles.rs:1288-1394`). Nothing at write time
checks that a role or channel exists in the guild; a saved panel whose role
was deleted fails at dispatch with the vetted `missing_role` reply, and
preflight only covered the boot catalogue.

**Environment/security-only:** no self-role key is EnvOnly, but the guild
fence still applies: both settings handlers pin the staging guild constant
and ignore any body guild
(`crates/bot/src/internal_action_http.rs:2626`, `:2662`). Member picks,
granted roles, audit rows and claim leases are runtime records, not
configuration; the page never reads them.

### Control map

Each control names its validator, permission, store, audit and consumer.
No control grants a Discord role or permission; the bot's existing
ManageRoles, hierarchy, allowlist and deployment-mask approval remain the
sole grant authority.

| Control | Validator | Permission | Store | Audit | Consumer |
| --- | --- | --- | --- | --- | --- |
| SR-C1 panel inventory (read) | Client renders only strict-shape catalogues; server refusal is authority | Web admin session + live Manage Server check (SR-W1), staging-pinned guild | `settings.get` on `TWO_SELF_ROLE_PANELS` | None (reads are not audited) | Admin eyes only; no runtime effect |
| SR-C2 panel create/edit (mode, exclusive, color, channel, message) | Client shape pre-check; server strict parser (`settings.rs:1110-1117`); dispatch revalidation at use (`self_roles.rs:1288-1394`) | Same web checks; bot ManageRoles + hierarchy enforced at dispatch, not at save | Whole-blob `settings.set` with CAS | One `guild_settings_audit` row per commit | `live_panels` per dispatch/sweep (`self_role_handlers.rs:220-227`), next poll |
| SR-C3 option create/edit/remove (key, label, role, emoji, description) | Same as SR-C2; duplicate keys/roles/emojis refused by the parser | Same as SR-C2 | Same as SR-C2 | Same as SR-C2 | Same as SR-C2 |
| SR-C4 publish panel (add to stored catalogue) | Same as SR-C2; roles/channels not verified until the bot uses them | Same as SR-C2 | Same as SR-C2 | Same as SR-C2 | Same as SR-C2 |
| SR-C5 unpublish panel (remove from stored catalogue) | Same as SR-C2; explicit per-panel confirm; granted roles stay granted | Same as SR-C2 | Same as SR-C2 | Same as SR-C2 | Pickers stop serving the panel after the next poll; no role revocation |
| SR-C6 unset catalogue to boot (null) | None beyond CAS; boot contents are not visible to the page | Same as SR-C2 | `settings.set` null (audited delete) | Same as SR-C2 | Boot catalogue after the next poll |
| SR-C7 dry-run toggle | None: stored values are inert | None (no control rendered) | Must not write; a stored row would be silently ignored | None | None until SR-W3 |

### Env-vs-dashboard precedence rule

The rule below restates the shipped seam plus two page obligations. Order
matters; evaluate top to bottom:

1. **Stored row present, renders and strict-parses** (including stored
   `[]`): the stored catalogue is live after the next poll. Stored `[]`
   means no panels, and the page offers it only as an explicit
   unpublish-all with its own confirm, never as an empty-editor default.
2. **Stored row absent** (`source=unset`): the boot catalogue is live. Its
   contents are not visible to the page, so every unset confirm states
   that the deployment catalogue takes over unseen.
3. **Stored row present but unrenderable or unparsable**: reachable only
   via direct SQL, because signed writes validate. The bot falls back to
   boot; the page shows `STORED_INVALID` with the stored value withheld,
   not a guessed panel list.
4. **Service existence is boot-gated**: an empty boot catalogue means no
   service and no pickers no matter what is stored. The page cannot
   enable self-roles; enabling needs a host restart with a non-empty boot
   catalogue.
5. **No overlay, no epoch**: the /ops layer never masks this key
   (`settings.rs:373`), and guild rows carry CAS only, no epoch.

Deterministic examples: stored `[games]` over boot `[games, colors]` serves
`[games]` only; unsetting that row restores `[games, colors]` unseen;
storing `[]` serves nothing; a direct-SQL `"{"` row serves boot with a page
warning; boot-empty plus stored `[games]` serves nothing until restart.

## Minimal page contract

Inherited web shell and signer are **not inspected in this bot-only
change**. Before any page build they must prove: private, non-cacheable
admin responses; CSRF protection; current guild membership and Manage
Server authority checked server-side on reads and writes; no signing key
in browser code; and session revocation/failure denial. No raw payload,
Discord API body, member data, role mentions, signing material or
environment values in logs, alerts, errors or previews. Show role/channel
IDs, and names only when already available to this admin, as escaped inert
text, never pings.

### View and change

- Read the catalogue via `settings.get`; `source=store` means **stored
  override**, `source=unset` means **boot catalogue, contents not
  exposed**. Never display a guessed deployment value as current.
- The unit of UI is the panel; the unit of write is the whole blob. Every
  save is read (CAS token `t`) -> strict-shape check -> apply exactly the
  confirmed panel operations -> diff preview -> confirm -> `settings.set`
  with `expected_version=t`. The submitted diff must equal the confirmed
  operations; if a re-read shows the stored catalogue moved, the client
  stops and asks for re-confirm instead of submitting.
- Publish adds one validated panel; unpublish removes exactly the confirmed
  panels and names each removed panel, its option count and its roles in
  the preview, with the copy "members keep roles they already hold; only
  future picks stop." A save that would drop a panel the admin did not
  open is refused client-side; CAS is the backstop, not the UX.
- Unset is a distinct confirmed **Use deployment catalogue** action sending
  JSON `null`, not `[]` or an empty string. It is CAS-protected and
  audited. It restores the unseen boot catalogue after refresh; it does
  not undo Discord side effects and cannot enable a boot-empty service.
- No role grant, permission change, channel overwrite edit, member-role
  revocation, pause-service or restart button. No per-panel publish flag
  is invented: presence in the stored catalogue is published, absence is
  unpublished.

### Saved vs live vs fallback vs unknown

No page state is named `applied`. Save and later get confirm **storage
only**. A poll cadence is not a latency SLA: next successful poll plus the
next member interaction is the expected path; failed polls, boot fallback,
activation and the drain check can delay it indefinitely.

| Page state | Meaning | Copy |
| --- | --- | --- |
| SAVED_UNVERIFIED | Confirmed save; bot application not confirmed | "Saved to configuration. Takes effect within about a minute, when the bot picks it up." |
| INHERITED_BOOT | Get returned unset; boot catalogue is live but invisible | "Using the deployment catalogue; its panels are not visible here." |
| STORED_INVALID | Stored row fails the strict shape (direct SQL only) | "The stored catalogue is invalid, so the deployment catalogue is live. Save a valid catalogue to replace it." |
| UNKNOWN | Read failed or has not succeeded yet | "The bot did not answer. Nothing shown is current." |

On read failure keep an already displayed catalogue labeled stale with its
observation time, disable all saves until a fresh get, and retain unsent
input privately. Never substitute an empty panel list for an unavailable
read. No durable offline save queue.

### CAS, idempotency and dry-run

Get returns `{ok,result:{key,value,source},version,request_id}`; save
returns `{ok,result:{key,outcome},version,request_id}` with no submitted
value (`internal_action_http.rs:2782-2809`). Always supply the observed
integer `expected_version`; omission would allow legacy unconditional
writes. Treat it as opaque equality, never revision ordering. `0` expects
absence. Persisted CAS survives restarts, but absence cannot detect a
create/delete cycle.

For each confirmed save allocate a fresh idempotency key and retain the
exact body bytes. Retry the same intent only with the **same key and body**,
a fresh nonce/timestamp and signature; editing the catalogue or CAS means a
new intent/key. Receiver claims before writing; replay never repeats
DML/audit. A replay success side-reads the **current** CAS; it is not proof
the current catalogue still equals that save. Re-read before editing or
undo. `needs_reconciliation` means unknown, never create a fresh key to
force a retry.

**Dry-run is local preview only:** deterministic
`{schema,guild_id,key,expected_version,would,reason,retryable,next_action,
panels_added,panels_removed,panels_changed}` for identical inputs, without
network, signing, nonce burn, idempotency reservation, SQL, audit or
Discord calls. It checks manifest/shape rules only, not live role or
channel facts, CAS freshness, drain or actual application. There is no
server dry-run verb; do not append `dry_run:true` to `settings.set` and
assume it prevents a write. Unrelated to `TWO_SELF_ROLE_DRY_RUN`, which
stays read-only.

### Honest failure UX

| Stable page reason | Source / copy | Next action |
| --- | --- | --- |
| `SAVED_UNVERIFIED` | Confirmed save: storage only, application unconfirmed | `reread_before_edit` |
| `INHERITED_BOOT` | Get unset: deployment catalogue live, panels invisible | `none` |
| `STORED_INVALID` | Stored row fails the strict shape; boot is live | `save_valid_catalogue` |
| `PREVIEW_ONLY` | "Preview only. Nothing sent or saved." | `confirm_or_edit` |
| `VERSION_CONFLICT` | Wire 409 `version_conflict`: "Someone changed the panels. Your draft is kept; reload before saving." | `reread_then_reconfirm` |
| `INVALID_VALUE` | Local validation or wire refusal: field-level local reason only, no echoed input | `fix_or_reread` |
| `VALIDATION_UNAVAILABLE` | Role/channel existence unverifiable at write time; saves proceed with disclosure, dispatch remains the fence | `disclose_and_save_or_wait` |
| `RECOVERY_PENDING` | Wire `SelfRoleRecoveryPending`: member role changes still settling | `wait_for_drain` |
| `NOT_EDITABLE` | Dry-run row; no signed write | `none` |
| `FOREIGN_GUILD` | Session guild not the staging-pinned deployment guild; no signed get/set | `select_managed_guild` |
| `UNAUTHORIZED` | Session/admin/signature/gate refusal; do not expose policy details | `reauthenticate_or_stop` |
| `RUNTIME_UNAVAILABLE` | Read unavailable: "The bot did not answer. Editing is unavailable." | `retry_read` |
| `COMMIT_UNKNOWN` | Write timeout/5xx/409 `needs_reconciliation`: "We could not confirm the save. Do not submit a different save yet." | `reconcile_same_intent` |

These are **proposed adapter reasons**, not new bot wire fields. Guild
fence: a body `guild_id` cannot select a settings guild; the web must
compare trusted session guild to deployment guild **before signing**, for
reads and saves. This proposal offers no production or multi-guild
settings surface. Never retarget the receiver, reveal environment
identity, enable an action gate or widen a signed/bearer scope to make the
page work.

## Machine-readable contract and offline oracle

Stable control IDs are proposal-local, not new bot keys or wire enums.
`candidate_editable` is conditional, not launch authorization. Evidence
anchors below name real source readers; the offline check validates their
presence, not runtime effects.

```json
{
  "schema": "dashboard.self_role_panels.v1",
  "source_sha": "a6fa0fb7d40e2ee4eaa6bfce9834d639a25c166c",
  "controls": [
    {"id":"SR-C1","control":"panel_inventory","key":"TWO_SELF_ROLE_PANELS","state":"readable","apply":"none_read_only","gate":["SR-W1"],"evidence":["E-CLASS","E-RECEIVER","E-READRESP","E-SOURCE"]},
    {"id":"SR-C2","control":"panel_create_edit","key":"TWO_SELF_ROLE_PANELS","state":"candidate_editable","apply":"poll_then_dispatch_or_sweep","gate":["SR-W1","SR-W2"],"evidence":["E-CLASS","E-WIRED","E-SEAM","E-WRITEVAL","E-DRAIN","E-DISPATCH"]},
    {"id":"SR-C3","control":"option_create_edit_remove","key":"TWO_SELF_ROLE_PANELS","state":"candidate_editable","apply":"poll_then_dispatch_or_sweep","gate":["SR-W1","SR-W2"],"evidence":["E-CLASS","E-WIRED","E-SEAM","E-WRITEVAL","E-DRAIN","E-DISPATCH"]},
    {"id":"SR-C4","control":"publish_panel","key":"TWO_SELF_ROLE_PANELS","state":"candidate_editable","apply":"poll_then_dispatch_or_sweep","gate":["SR-W1","SR-W2"],"evidence":["E-CLASS","E-WIRED","E-SEAM","E-WRITEVAL","E-DRAIN","E-PREFLIGHT"]},
    {"id":"SR-C5","control":"unpublish_panel","key":"TWO_SELF_ROLE_PANELS","state":"candidate_editable","apply":"poll_then_dispatch_or_sweep","gate":["SR-W1","SR-W2"],"evidence":["E-CLASS","E-WIRED","E-SEAM","E-WRITEVAL","E-DRAIN","E-DISPATCH"]},
    {"id":"SR-C6","control":"unset_catalogue_to_boot","key":"TWO_SELF_ROLE_PANELS","state":"candidate_editable","apply":"poll_then_dispatch_or_sweep","gate":["SR-W1","SR-W2"],"evidence":["E-CLASS","E-FALLBACK","E-DRAIN","E-SOURCE","E-CAS"]},
    {"id":"SR-C7","control":"dry_run_toggle","key":"TWO_SELF_ROLE_DRY_RUN","state":"read_only_unwired","apply":"none","gate":["SR-W3"],"evidence":["E-DRYRUN-CLASS","E-FROMENV","E-DRYRUN-UNWIRED"]}
  ],
  "other_states": [
    {"id":"SR-S1","state":"boot_gated_existence","key":null,"reason":"empty_boot_builds_no_service_and_stored_rows_cannot_enable","evidence":["E-BOOTGATE","E-SEAM"]},
    {"id":"SR-S2","state":"activation_scoped","key":null,"reason":"staging_allowlist_serving_with_live_cleared_capability","evidence":["E-ALLOWLIST","E-ACTIVATION"]},
    {"id":"SR-S3","state":"absent","key":null,"reason":"no_per_panel_publish_flag_role_oracle_or_previous_editor_readback","evidence":["E-PARSE","E-DISPATCH","E-READRESP"]},
    {"id":"SR-S4","state":"no_overlay","key":null,"reason":"ops_layer_excluded_from_catalogue_drain_lock","evidence":["E-NOOPS","E-DRAIN"]}
  ],
  "evidence": {
    "E-CLASS":{"path":"crates/core/src/settings.rs","line":208,"contains":"SettingClass::Hot"},
    "E-WIRED":{"path":"crates/core/src/settings.rs","line":277,"contains":"pub const HOT_WIRED"},
    "E-WIRED-MEMBER":{"path":"crates/core/src/settings.rs","line":311,"contains":"TWO_SELF_ROLE_PANELS"},
    "E-DRYRUN-CLASS":{"path":"crates/core/src/settings.rs","line":265,"contains":"TWO_SELF_ROLE_DRY_RUN"},
    "E-NOOPS":{"path":"crates/core/src/settings.rs","line":373,"contains":"key != \"TWO_SELF_ROLE_PANELS\""},
    "E-WRITEVAL":{"path":"crates/core/src/settings.rs","line":1110,"contains":"if key == \"TWO_SELF_ROLE_PANELS\""},
    "E-SEAM":{"path":"crates/bot/src/self_role_handlers.rs","line":59,"contains":"fn live_panels_with"},
    "E-FALLBACK":{"path":"crates/bot/src/self_role_handlers.rs","line":68,"contains":"fn panels_from_snapshot"},
    "E-ALLOWLIST":{"path":"crates/bot/src/self_role_handlers.rs","line":44,"contains":"fn staging_allowlist"},
    "E-BOOTGATE":{"path":"crates/bot/src/self_role_handlers.rs","line":166,"contains":"if gates.panels.is_empty()"},
    "E-DRYRUN-UNWIRED":{"path":"crates/bot/src/self_role_handlers.rs","line":146,"contains":"dry_run: gates.dry_run"},
    "E-DRAIN":{"path":"crates/cutover/src/settings.rs","line":214,"contains":"let catalogue_changed"},
    "E-DRAIN-ERR":{"path":"crates/cutover/src/settings.rs","line":238,"contains":"SelfRoleRecoveryPending"},
    "E-PARSE":{"path":"crates/core/src/self_roles.rs","line":812,"contains":"pub fn parse_self_role_panels"},
    "E-FROMENV":{"path":"crates/core/src/self_roles.rs","line":776,"contains":"pub fn from_env"},
    "E-DISPATCH":{"path":"crates/core/src/self_roles.rs","line":1288,"contains":"pub fn validate_self_role_dispatch"},
    "E-PREFLIGHT":{"path":"crates/bot/src/preflight.rs","line":292,"contains":"env(\"TWO_SELF_ROLE_PANELS\")"},
    "E-RECEIVER":{"path":"crates/bot/src/internal_action_http.rs","line":2626,"contains":"TWO_STAGING_GUILD_ID"},
    "E-READRESP":{"path":"crates/bot/src/internal_action_http.rs","line":2782,"contains":"fn settings_read_response"},
    "E-WRITERESP":{"path":"crates/bot/src/internal_action_http.rs","line":2797,"contains":"fn settings_write_response"},
    "E-POLL":{"path":"crates/bot/src/settings_jobs.rs","line":152,"contains":"async fn poll_once"},
    "E-POLL-REJECT":{"path":"crates/bot/src/settings_jobs.rs","line":168,"contains":"settings_snapshot_rejected"},
    "E-ACTIVATION":{"path":"crates/core/src/activation.rs","line":52,"contains":"LiveCapability::SelfRoles"},
    "E-SOURCE":{"path":"crates/core/src/internal_settings.rs","line":121,"contains":"\"store\""},
    "E-CAS":{"path":"crates/core/src/internal_settings.rs","line":62,"contains":"expected_version"}
  },
  "reasons": {
    "SAVED_UNVERIFIED":{"retryable":false,"next_action":"reread_before_edit"},
    "INHERITED_BOOT":{"retryable":false,"next_action":"none"},
    "STORED_INVALID":{"retryable":false,"next_action":"save_valid_catalogue"},
    "PREVIEW_ONLY":{"retryable":false,"next_action":"confirm_or_edit"},
    "VERSION_CONFLICT":{"retryable":false,"next_action":"reread_then_reconfirm"},
    "INVALID_VALUE":{"retryable":false,"next_action":"fix_or_reread"},
    "VALIDATION_UNAVAILABLE":{"retryable":true,"next_action":"disclose_and_save_or_wait"},
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
`{schema,source_sha,guild_id,key,expected_version,request_id,reason,
retryable,next_action,panels_added,panels_removed,panels_changed}`.
No value-bearing logs; previews may show only the admin's private
configuration draft. A source/hash mismatch requires re-inventory, not
optimistic editing.

Save-intent example (publish one panel; whole blob stays the unit of
write):

```json
{
  "schema": "dashboard.self_role_panels.v1",
  "guild_id": "staging-pinned",
  "key": "TWO_SELF_ROLE_PANELS",
  "expected_version": 41,
  "would": "save",
  "reason": "PREVIEW_ONLY",
  "retryable": false,
  "next_action": "confirm_or_edit",
  "panels_added": ["colors"],
  "panels_removed": [],
  "panels_changed": []
}
```

Precedence example (unset over a boot catalogue the page cannot see):

```json
{
  "schema": "dashboard.self_role_panels.v1",
  "guild_id": "staging-pinned",
  "key": "TWO_SELF_ROLE_PANELS",
  "expected_version": 42,
  "would": "save",
  "reason": "INHERITED_BOOT",
  "retryable": false,
  "next_action": "none",
  "panels_added": [],
  "panels_removed": ["games", "colors"],
  "panels_changed": []
}
```

The `panels_removed` names stored panels that stop being overridden, not
boot panels (which are unknown); the confirm copy states the deployment
catalogue takes over unseen.

| Fixture | Deterministic oracle for future protected acceptance tests |
| --- | --- |
| `SR-F01` | Precedence: stored `[games]` over boot `[games, colors]` serves `[games]` after one poll; unset restores boot; stored `[]` serves no panels; direct-SQL `"{"` serves boot with `STORED_INVALID`; boot-empty plus stored `[games]` serves nothing until restart. Assert through `live_panels`-equivalent resolution, not get alone. |
| `SR-F02` | Publish: read token t, add one strict-valid panel, preview names it in `panels_added` with zero removals, save with t; dispatch on its message serves it after one poll; exactly one audit row. |
| `SR-F03` | Unpublish: read token t, remove panel P, preview names P with option/role counts and the keep-granted-roles copy, save with t; P's pickers stop after one poll while previously granted roles stay; exactly one audit row. |
| `SR-F04` | Truthful publish: a save whose diff drops a panel outside the confirmed operations is refused client-side before signing; prove the unsigned refusal, not only the CAS backstop. |
| `SR-F05` | Role deletion under a live panel: delete the role from the guild double, dispatch refuses `missing_role` with the vetted reply, page shows the panel `UNKNOWN` until re-read; no auto-repair, no silent unpublish. |
| `SR-F06` | Stale revision: A/B get token t; A saves; B saves t: only A commits, B receives conflict and retains draft. Two creators expecting 0: one success. |
| `SR-F07` | Access changes: drift the live role mask from the deployment approval, dispatch refuses `role_permissions_changed`; move the role above the bot, dispatch refuses `role_hierarchy`; the page discloses unverified roles/channels at save and never claims pre-verification. |
| `SR-F08` | Dry-run inert: a directly stored `TWO_SELF_ROLE_DRY_RUN=1` row leaves dispatch behavior unchanged; the page renders no toggle (`NOT_EDITABLE`). Flips only when SR-W3 lands. |
| `SR-F09` | Drain: catalogue save while a member change is processing refuses `RECOVERY_PENDING` with zero rows/audit; succeeds after drain. |
| `SR-F10` | Replay same key/body: one change/audit; different body under key: refusal. After a later save, replay old success returns current token; fresh get required. `needs_reconciliation` is `COMMIT_UNKNOWN`, never success. |
| `SR-F11` | Foreign guild B: web blocks before signing, zero requests; a B row cannot affect the A runtime. Independently pin receiver staging-guild behavior, not an invented body-guild denial. |
| `SR-F12` | PII and preview: no member data, audit history or signing material in any page payload; role/channel names render as escaped inert text with zero pings; identical preview input gives identical output with zero network/signatures/nonce/idempotency/SQL/Discord effects. |

All runtime fixtures above are **planned**, not executed by this proposal.
Use disposable agent-testdb/CI databases, isolated guild/channel fixtures,
an injected poll clock and mock Discord. No staging/production DB or live
settings writes. Future staging application evidence is a separately
authorized gate, not a test this document claims to have run.

Runnable offline consistency check (Python stdlib, from repo root):

```sh
python3 -m doctest -v docs/dashboard-self-roles-spec.md
python3 scripts/check_docs_index.py --json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
```

This checks catalogue class/hot-wired parity, evidence file/line anchors,
control state/gate structure and reason remediations. It runs **none** of
SR-F01–F12, performs no runtime/DB test, and proves no apply latency or web
authority. Changed source anchors intentionally require re-inventory, not a
waiver.

```pycon
>>> import json, pathlib, re
>>> doc = pathlib.Path("docs/dashboard-self-roles-spec.md").read_text()
>>> fence = "`" * 3
>>> m = json.loads(re.search(fence + r"json\n(.*?)\n" + fence, doc, re.S).group(1))
>>> rows = m["controls"]; src = pathlib.Path("crates/core/src/settings.rs").read_text()
>>> table = src.split("pub const SETTING_CLASSES", 1)[1].split("\n];", 1)[0]
>>> classes = dict(re.findall(r'\(\s*"(\w+)",\s*SettingClass::(\w+),?\s*\)', table))
>>> wired = set(re.findall(r'"(\w+)"', src.split("pub const HOT_WIRED", 1)[1].split("\n];", 1)[0]))
>>> assert classes["TWO_SELF_ROLE_PANELS"] == "Hot" and "TWO_SELF_ROLE_PANELS" in wired
>>> assert classes["TWO_SELF_ROLE_DRY_RUN"] == "Hot" and "TWO_SELF_ROLE_DRY_RUN" not in wired
>>> assert len(rows) == len({r["id"] for r in rows}) == 7
>>> assert [r["id"] for r in rows if r["state"] == "readable"] == ["SR-C1"]
>>> assert [r["id"] for r in rows if r["state"] == "read_only_unwired"] == ["SR-C7"]
>>> assert all(r["gate"] == ["SR-W1", "SR-W2"] for r in rows if r["state"] == "candidate_editable")
>>> assert rows[0]["gate"] == ["SR-W1"] and rows[6]["gate"] == ["SR-W3"]
>>> assert all(r["key"] == "TWO_SELF_ROLE_PANELS" for r in rows[:6]) and rows[6]["key"] == "TWO_SELF_ROLE_DRY_RUN"
>>> assert all(r["apply"] == "poll_then_dispatch_or_sweep" for r in rows if r["state"] == "candidate_editable")
>>> ids = [r["id"] for r in rows + m["other_states"]]; assert len(ids) == len(set(ids))
>>> assert {s["state"] for s in m["other_states"]} == {"boot_gated_existence", "activation_scoped", "absent", "no_overlay"}
>>> assert all(r["evidence"] and all(e in m["evidence"] for e in r["evidence"]) for r in rows + m["other_states"])
>>> assert all(e["contains"] in pathlib.Path(e["path"]).read_text().splitlines()[e["line"]-1] for e in m["evidence"].values())
>>> assert all(isinstance(r["retryable"], bool) and r["next_action"] for r in m["reasons"].values())
>>> assert set(m["reasons"]) == set(re.findall(r'^\| `([A-Z_]+)` \|', doc, re.M))
>>> assert len(set(re.findall(r'^\| `(SR-F\d+)` \|', doc, re.M))) == 12
>>> assert re.fullmatch(r"[0-9a-f]{40}", m["source_sha"])
>>> print(json.dumps({"schema": m["schema"], "controls": len(rows), "status": "pass"}, sort_keys=True))
{"controls": 7, "schema": "dashboard.self_role_panels.v1", "status": "pass"}

```

## Intent gate and bounded follow-ups

| ID | Recommendation, not a build card or API |
| --- | --- |
| `SR-W1` | Prove the web shell: private non-cacheable admin responses, CSRF, server-side guild membership plus live Manage Server checks on reads and writes, staging-guild comparison before signing, no signing key in browser code, session revocation denial. No page reads or writes before this. |
| `SR-W2` | Build the panel-operations client: read-modify-write over the whole blob with CAS, strict-shape pre-check, diff preview naming added/removed/changed panels, truthful unpublish confirm, unset-to-boot disclosure, and refusal of diffs outside the confirmed operations. |
| `SR-W3` | Apply `TWO_SELF_ROLE_DRY_RUN` live (store-first per-operation read, boot fallback, last-good on a bad row, plus pins) before any toggle; shared with the sibling proposal's dry-run wiring card, not duplicated here. |

Smallest future build slice (SR-S1, <=1 day): a read-only panel
inventory page behind SR-W1 only. It performs `settings.get`, renders the
strict-shape catalogue (or `INHERITED_BOOT` / `STORED_INVALID` /
`UNKNOWN`), and signs nothing. No writes, no migration, no new grant, no
member-data read. It proves the auth/guild-fence/signer/get plumbing and
gives staff visibility into what is stored, while every write stays with
operators until SR-W2.

Bind the CEO intent-alignment request to the exact task-document revision,
repository commit and this file's SHA-256. Ask them to record host intent
alignment through the existing authorized path, not a new owner approval
hop. A queued request is not alignment. No build cards before explicit
acceptance and recorded alignment; no activation, spend or permission is
granted by either. Only genuinely new access/data boundaries require CISO
disposition; in particular, a future catalogue editor changing which roles
members receive must resolve that boundary before enablement, while this
doc and a read-only inventory do not themselves authorize or require a new
credential/data grant.

**No-build alternative:** operators keep editing `TWO_SELF_ROLE_PANELS` in
host env with preflight proving boot panels at deploy; no dashboard
surface, no new write path, no per-panel visibility for staff. This stays
the default if intent is not granted. Evidence that would overturn it:
sustained operator panel-change volume (multiple host edits per month with
review pain), a panel misconfiguration incident traceable to blind env
editing, or a staging SR-S1 pilot showing staff can read the catalogue
truthfully. Any overturn re-enters through this same intent gate; it does
not authorize writes.

Review by 2026-11-02. Metric: all 7 controls have evidenced
validator/permission/store/audit/consumer decisions and all 12 fixture IDs
have accepted/rejected oracles. If evidence is incomplete, keep
read-only/no-build; absent acceptance defaults to **no-build**, never
timeout approval. No live experiment or apply-delay measurement was
performed. Unverified: web guards, deployed capabilities/gates, actual
guild objects, runtime availability and production behavior.

ACT: publish a source-backed proposal and offline consistency check.
WHY: prevent a per-panel dashboard from silently unpublishing live self-role
panels through whole-blob writes. UNDO: revert these documentation changes;
no operational state changes to undo.
