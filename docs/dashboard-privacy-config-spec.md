# Dashboard privacy configuration contract

Status: **proposed; design only**. Written 2026-10-26; intent review by
2026-11-01. This proposal adds no runtime, page, migration, permission, live write
or activation. Publication is not implementation or production approval.
This file is the single repository source, including the manifest and fixture
oracle. It extends neither the ticket, onboarding/self-role nor custom-response pages.

## Decision

A guild admin needs to know what analytics capture is configured, what opt-out
posture the guild holds, and whether the bot could be capturing — without ever
seeing a member identity. A stored toggle row is **not** stopped capture, and an
aggregate count is **not** a member list.

**Recommend aggregate-first, toggles-once-live, never per-member.** Inventory at
source commit `c6e5213b516d5ef62b357cc2f5feeaf8f59889a8` finds three toggle
columns behind an unaudited, CAS-less writer with no production caller, no signed
dashboard verb and no web read path — plus suppression and receipt tables whose
rows always carry a member ID. After intent acceptance, the smallest eventual
page can show three stored toggle rows read-only plus two guild-aggregate posture
counts, and enable nothing until W1/W2/W3 below land. Per-member controls,
per-member audit rows and any bare "capture stopped" claim are permanently out of
scope, not deferred. The in-flight per-guild toggles implementation is still
blocked, so every toggle row ships `read_only_blocked` here. If prerequisites
cannot be proved, choose **no-build**, not a form that writes toggles without
CAS and audit or renders member IDs. Nothing is built by this spec, including
the narrowed read-only page.

## Source-backed inventory

### Toggle store and writer: no dashboard path exists

- `guild_analytics_settings(guild_id, presence_enabled, activity_enabled,
  voice_enabled, updated_at)` holds one row per guild
  (`crates/cutover/migrations/0433_privacy_opt_out.sql:42`). A guild with no row
  has every family enabled: `analytics_settings`
  (`crates/cutover/src/privacy.rs:319`) returns `unwrap_or((true, true, true))`
  (`:335`). Each switch is independent.
- The writer `set_analytics_settings` (`crates/cutover/src/privacy.rs:340`) is an
  unconditional upsert: no expected-version/CAS token, no actor argument, no
  audit insert. Machine-checked below: `expected_version` is absent from the
  whole file, and the set region contains neither `audit` nor `actor`.
- The writer has **no production caller**: only tests call it. No slash command
  writes toggles: `/privacy status` reads through `analytics_settings` only
  (`crates/bot/src/privacy_command.rs:234`), and the member command acts solely
  on the invoker.
- No signed privacy verb exists: `crates/core/src/internal_settings.rs:37`
  accepts only `settings.get` / `settings.set`, and the word `privacy` is absent
  from that file. The settings receiver pins the staging guild constant
  (`crates/bot/src/internal_action_http.rs:2626`, `:2662`); there is no privacy
  receiver at all, and `settings.get` returns stored value plus CAS only
  (`crates/cutover/src/settings.rs:132-144`), never runtime state.
- No web read path exists: the embedded contract
  (`crates/store/sql/web_v1.sql:109`) defines ten views and none is
  privacy-related (machine-checked below). `two_web_reader` holds SELECT on the
  ten reviewed views and no bot-table access (`docs/database-roles.md:16`), while
  the three opt-out tables are ordinary runtime tables with no reader grant
  (`docs/database-roles.md:56`).

### Suppression and receipts: member IDs in every row

- `privacy_opt_outs(guild_id, member_id)` suppression tombstones and
  `privacy_opt_out_audit(id, guild_id, member_id, actor, opted_out_at)` one
  receipt row per purge (`crates/cutover/migrations/0433_privacy_opt_out.sql:35`,
  `:50`). The subject always lives in the scope columns.
- Both are retained through member erasure as control-plane tombstones: deleting
  suppression would silently resume refused persistence
  (`crates/cutover/src/member_erasure_plan.json:348`).
- Actor labels carry attribution only, never a subject ID, count, content or
  secret. Member self-serve uses the fixed `member:self-serve` constant
  (`crates/bot/src/privacy_command.rs:41`); the staging verify read publishes
  only that label plus `operator:fixture-check` and fails closed on any other
  (`docs/privacy.md:249`).
- The existing PII-safe read pattern is the `staging-privacy-verify-read`
  workflow (`.github/workflows/staging-privacy-verify-read.yml`): `COUNT(*)`
  projections only, plus capped receipt rows under a closed actor allowlist, with
  member and guild pins never reaching the artifact, log or summary
  (`docs/privacy.md:209-255`). The dashboard aggregate read (W2) must meet at
  least this bar; this proposal goes further — counts only, no actor labels in
  the browser at all.

### Consumers: what the toggles gate

The family taxonomy is `category_of_event` / `community_category` /
`toggle_column` (`crates/core/src/analytics_family.rs:34`, `:63`, `:49`).

| Family | Column | Gated event types |
| --- | --- | --- |
| Presence | `presence_enabled` | `presence_update` |
| Activity | `activity_enabled` | `first/second/third_message`, community `message_created` facts |
| Voice | `voice_enabled` | `voice_session_start/end`, `first_voice_session`, community `voice_session_started/ended` facts |

Gated seams (every one reads the toggle inside the write transaction, so a new
toggle value meets the next write — there is no poll):

- Cutover `record_event` (plus `record_earliest` and the backfill/community
  replays that call it): `ingestion_blocked_in` under the shared table lock
  (`crates/cutover/src/db.rs:343`); a suppressed inviter's `inviterId` is
  scrubbed from `member_join` metadata while the joiner's row persists.
- Store `record`: the same gate (`crates/store/src/store.rs:154`); the runtime
  funnel `touch_activity` honors suppression plus the matching family toggle
  (`crates/store/src/store.rs:599-603`).
- Gateway batch commit: one per-batch snapshot via `LEFT JOIN
  guild_analytics_settings` (`crates/cutover/src/gateway_session.rs:277`); a
  missing row reads as all-on for the batch.
- Community facts: suppression plus `community_category` to `category_off_in`
  (`crates/core/src/community_store.rs:89-94`); `member_joined` inviter scrub.
- Game-activity flush: the presence gate drains and drops the whole flush, the
  voice gate keeps play aggregates while `in_voice_ms` stays 0
  (`crates/core/src/game_activity_store.rs:407`, `:427`).
- Onboarding prompt guard: suppression-only `begin_prompt` plus a re-gate before
  the insert for opt-outs landing mid-welcome
  (`crates/core/src/onboarding_store.rs:159`, `:109`).
- Inactivity sweep: suppression-only skip inside the per-member transaction
  (`crates/core/src/inactivity_store.rs:301`); the RSVP host check-in is likewise
  suppression-only.

Residuals a toggle can never stop (the truthfulness fence):

- The cutover `touch_activity` / `mark_bot` helpers and the backfill binaries
  that call them are suppression-only: they carry no family signal
  (`crates/cutover/src/db.rs:518`, `:546`). The privacy inventory names this the
  remaining residual, not silent per-user analytics.
- Lifecycle rows are unconditional: joins, leaves, gate clears, onboarding
  prompts, invite clicks and routing serve safety, moderation and delivery.
  `category_of_event` returns `None` for them by design, and anonymous aggregate
  counters (`member_id = None`) are never blocked
  (`crates/core/src/privacy.rs:143`).
- Community join/rules facts are suppression-only; invite use counts keep
  counting with a blanked inviter field.
- In-flight transactions already past the gate still commit; buffered gateway
  touches, the hourly in-memory game-activity collector and the retention
  cadence/backups create lag between a toggle write and a fully quiet store.

## Control map: control to validator, permission, store, audit, consumer

Stable manifest IDs are proposal-local, not new bot keys or wire enums.
`read_only_blocked` is conditional on the in-flight toggles work landing first,
not launch authorization. Evidence anchors name real source readers; the offline
check validates their presence, not runtime effects.

| Stable ID / control | Validator (today, then proposed) | Permission | Store | Audit | Consumers |
| --- | --- | --- | --- | --- | --- |
| `PV-K1` / presence toggle | Today: Rust bool plus snowflake guild check only. Proposed: strict JSON boolean, opaque `expected_version` CAS equality, server-derived admin actor, trusted session-guild match before signing | Proposed Manage Server, checked server-side on reads and writes | `guild_analytics_settings.presence_enabled`, upsert plus `updated_at`; no null/unset (row delete would reset all three families) | None today; W1 append-only toggle-change audit | `presence_update` drops, gateway batch snapshot, game-activity flush drain; next write per transaction |
| `PV-K2` / activity toggle | Same as `PV-K1` | Same as `PV-K1` | `guild_analytics_settings.activity_enabled`, same no-unset rule | None today; W1 audit | `first/second/third_message`, community `message_created`, family-routed touches; next write per transaction |
| `PV-K3` / voice toggle | Same as `PV-K1` | Same as `PV-K1` | `guild_analytics_settings.voice_enabled`, same no-unset rule | None today; W1 audit | Voice sessions, community voice facts, game `in_voice_ms` zeroing; next write per transaction |
| `PV-P1` / opt-out posture aggregate | Guild allowlist plus count-only projection; any row-shaped read refuses | Proposed Manage Server, read-only | `COUNT(*)` over `privacy_opt_outs` for the pinned guild; never rows | None (reads); no per-member access log in the browser | Dashboard card only |
| `PV-P2` / purge receipt aggregate | Same count-only projection plus capped per-day histogram | Proposed Manage Server, read-only | `COUNT(*)` over `privacy_opt_out_audit` for the pinned guild; actor labels never leave the boundary | None (reads) | Dashboard card only |
| `PV-P3` / toggle-change audit view | Absent: no table, no viewer | None until W1 | Absent until W1 defines it | The audit itself is the deliverable | Dashboard shows own-action confirmations only until then |

Member self-serve stays in Discord (`/privacy`, open-to-everyone router gate with
a guild fence). The dashboard never acts for a member, never accepts a member ID
in any field, and never offers opt-out/opt-in/delete on anyone's behalf. Deletion
intake (`/privacy delete`, `deletion-status`, `deletion-receipt`) is member-bound
erasure and stays member-authenticated in Discord; no deletion surface is
proposed here.

## Minimal private page contract

Inherited web shell and signer are **not inspected in this bot-only change**.
Before any page build they must prove: private, non-cacheable admin responses;
CSRF protection; current guild membership and Manage Server authority checked
server-side on reads and writes; no signing key in browser code; and session
revocation/failure denial. No member ID, raw payload, Discord API body, role
mention, credential, transcript link or environment value appears in logs,
alerts, errors, previews or URLs. The guild is shown by name only when already
available to this admin; IDs are never rendered.

### View and change

- After W2/W3 the page reads the three stored toggle states (or the missing-row
  state) plus the two aggregate counts through signed count-only actions.
  `source=store` means **stored override**, `source=inherited_default_on` means
  **no row; all families on**. Never display a guessed value as current.
- Toggle rows stay read-only until W1; then one-row saves, not an invented
  atomic three-key action. No publish, purge, pause-service or restart button.
- There is no unset/delete control: deleting the row would silently reset all
  three families to on. Toggles are set-true/set-false only. Undo a change by
  another deliberate CAS save of the prior known value; never roll back a whole
  table or blindly overwrite another admin's save.
- No per-member search, table, export or drill-down exists at any stage. A count
  is never a link to a member list.

### Safe validation and transitions

The service validates only guild shape today; the boolean columns accept what
Rust sends. The proposed editor sends a strict JSON boolean and rejects
strings, numbers, null, arrays and objects locally; the store still validates.
No new ceiling or enum is invented here.

W1 must verify, at the server's trusted boundary: canonical guild identity,
admin authority, and CAS freshness at effect time. Missing or stale facts and
backend failures refuse safely with `VALIDATION_UNAVAILABLE`, not a guessed
pass. Re-check at effect time; write-time checks cannot eliminate a later row
change. No drain primitive is needed (gating happens per write), but the page
must disclose in-flight, buffered and hourly-collector lag with the bound
below; a browser warning alone does not make a lagging write safe to mislabel.

### CAS, stale handling, idempotency, dry-run and explicit confirmations

W1 must add an opaque `expected_version` token to the toggle writer (equality
only; the absent-row token is defined by W1). The dashboard always supplies the
observed token; omission or last-writer-wins is forbidden. Stale tokens return a
conflict: keep the draft, re-read, re-confirm. Never auto-retry a conflicted
save with a fresh token.

For each confirmed save allocate a fresh idempotency key and retain the exact
body bytes. Retry the same intent only with the **same key and body**, a fresh
nonce/timestamp and signature; editing the value or CAS means a new intent/key.
Replay never repeats the write or audit. A replay success is not proof the
current value still equals that save: re-read before editing or undo.
`needs_reconciliation` means unknown, never mint a fresh key to force a retry.

**Dry-run is local preview only:** deterministic
`{schema,guild_id,key,expected_version,would,reason,retryable,next_action}` for
identical inputs, without network, signing, nonce burn, idempotency reservation,
SQL, audit or Discord calls. It checks shape rules only, not live facts, CAS
freshness or actual application. There is no server dry-run verb; do not append
`dry_run:true` to a future write action and assume it prevents a write.

**Explicit confirmations are recorded server-side.** Disabling a family needs a
two-step confirm carrying a scope acknowledgment ("joins, leaves and other
safety rows still persist; stored rows are unchanged"); enabling needs a single
explicit confirm. Each confirmation records an intent with a server-generated
intent id, the server-derived admin id, the exact body bytes and the timestamp.
Browser input never supplies attribution.

### Saved-vs-live/unknown states and failure UX

No page state is named `applied`, `live` or `capture stopped`. A toggle commits
to the next write, not to the writes already in flight, buffered or collected;
per-transaction gating is not an immediacy SLA. On read failure keep an already
displayed value labeled stale with its observation time, disable editing until a
fresh read, and retain unsent input privately. Never substitute an empty/default
row for an unavailable read. No durable offline save queue.

| Stable page reason | Source / copy | Next action |
| --- | --- | --- |
| `SAVED_UNVERIFIED` | Confirmed save: "Saved to configuration. Capture effect is not confirmed." | `reread_before_edit` |
| `INHERITED_DEFAULT_ON` | No stored row: "Using inherited defaults: all families on. No stored override." | `none` |
| `PREVIEW_ONLY` | "Preview only. Nothing sent or saved." | `confirm_or_edit` |
| `VERSION_CONFLICT` | Stale CAS: "Someone changed this setting. Your draft is kept; reload before saving." | `reread_then_reconfirm` |
| `INVALID_VALUE` | Local validation refusal: field-level reason only, no echoed input | `fix_or_reread` |
| `VALIDATION_UNAVAILABLE` | Trusted facts unavailable; controls stay disabled | `wait_for_validation` |
| `NOT_EDITABLE` | Blocked/absent row or missing prerequisite; no signed write | `none` |
| `FOREIGN_GUILD` | Session guild not the pinned deployment guild; no signed read or write | `select_managed_guild` |
| `UNAUTHORIZED` | Session/admin/signature/gate refusal; do not expose policy details | `reauthenticate_or_stop` |
| `RUNTIME_UNAVAILABLE` | Read unavailable: "The bot did not answer. Editing is unavailable." | `retry_read` |
| `COMMIT_UNKNOWN` | Write timeout/5xx/`needs_reconciliation`: "We could not confirm the save. Do not submit a different save yet." | `reconcile_same_intent` |
| `TRUTH_GAP` | A residual class may be live beyond what the page observes: "Capture state cannot be confirmed from here." | `consult_runbook` |

These are **proposed adapter reasons**, not new bot wire fields. The settings
wire errors expose only `code,message,retryable` and `request_id` today, and no
privacy wire exists at all. Respect `Retry-After` where supplied; `in_progress`
retains the same intent. Use the matching reason's next_action, not automatic
retry of all conflicts.

Guild fence: the web must compare the trusted session guild to the deployment
guild **before signing**, for both reads and saves. This proposal offers no
production or multi-guild surface. Never retarget a receiver, reveal environment
identity or widen a signed scope to make the page work.

### Truthful capture claims (binding copy rules)

The dashboard cannot claim capture stopped while any write path is live:

1. Never render bare "capture stopped" or "analytics off". Every claim names the
   family and its residuals.
2. After disabling: "New `<family>` writes will be dropped. Rows already stored
   are unchanged. Joins, leaves, safety and moderation rows, anonymous counters
   and the other families still persist."
3. Missing row: "Using inherited defaults: all families on. No stored override."
4. Aggregates: "`N` guild members currently suppress analytics persistence in
   this guild." Never a per-member list, link or export.
5. Scope truthfulness beyond this page is owned separately; link that
   disposition instead of inventing scope claims here.

### PII-safe browser display and agent JSON shape

Browser display is counts, fixed reason strings and the admin's own draft only:
toggle booleans, missing-row state, two aggregate counts, capped per-day
histogram bars, and own-action confirmations. Never subject IDs, member lists,
actor labels, guild IDs, row contents or request internals.

Agent result shape (proposed, not a new endpoint):
`{schema,source_sha,guild_id,key,expected_version,request_id,reason,retryable,next_action}`.
`guild_id` is the deployment-pinned guild, never a member selector; `member_id`
must not appear in any dashboard request, response, log line or preview. No
value-bearing logs; previews may show only the admin's private configuration
draft. A source/hash mismatch requires re-inventory, not optimistic editing.

## Machine-readable contract and offline oracle

Stable manifest IDs are proposal-local, not new bot keys or wire enums.
`read_only_blocked` is conditional, not launch authorization. Evidence anchors
below name real source readers; the offline check validates their presence,
not runtime effects. Classification `never_per_member` has no control to send.

```json
{
  "schema": "dashboard.privacy_config.v1",
  "source_sha": "c6e5213b516d5ef62b357cc2f5feeaf8f59889a8",
  "keys": [
    {"id":"PV-K1","column":"presence_enabled","family":"presence","class":"dedicated_table","state":"read_only_blocked","apply":"next_write_per_transaction","gate":["PV-W1","PV-W2","PV-W3"],"evidence":["E-WRITE","E-READ","E-DEFAULT","E-FAMILY","E-COLUMN","E-CUTOVER-GATE","E-STORE-GATE","E-BATCH","E-GAME-PRESENCE"]},
    {"id":"PV-K2","column":"activity_enabled","family":"activity","class":"dedicated_table","state":"read_only_blocked","apply":"next_write_per_transaction","gate":["PV-W1","PV-W2","PV-W3"],"evidence":["E-WRITE","E-READ","E-DEFAULT","E-FAMILY","E-COLUMN","E-CUTOVER-GATE","E-STORE-GATE","E-STORE-TOUCH","E-BATCH","E-COMMUNITY","E-FACTS-GATE"]},
    {"id":"PV-K3","column":"voice_enabled","family":"voice","class":"dedicated_table","state":"read_only_blocked","apply":"next_write_per_transaction","gate":["PV-W1","PV-W2","PV-W3"],"evidence":["E-WRITE","E-READ","E-DEFAULT","E-FAMILY","E-COLUMN","E-CUTOVER-GATE","E-STORE-GATE","E-BATCH","E-COMMUNITY","E-FACTS-GATE","E-GAME-VOICE"]}
  ],
  "aggregates": [
    {"id":"PV-P1","metric":"suppressed_pair_count","state":"aggregate_only","gate":["PV-W2","PV-W3"],"evidence":["E-SUPPRESSION","E-TOMBSTONE","E-SUPPRESSED-FN","E-STATUS"]},
    {"id":"PV-P2","metric":"purge_receipt_count","state":"aggregate_only","gate":["PV-W2","PV-W3"],"evidence":["E-RECEIPTS","E-SELF-SERVE","E-STATUS"]},
    {"id":"PV-P3","metric":"toggle_change_audit","state":"absent_until_wired","gate":["PV-W1"],"evidence":["E-WRITE","E-RECEIVER-PIN"]}
  ],
  "other_states": [
    {"id":"PV-S1","state":"inherited_default_on","column":null,"reason":"missing_row_means_every_family_enabled","evidence":["E-READ","E-DEFAULT","E-BATCH"]},
    {"id":"PV-S2","state":"always_on_lifecycle","column":null,"reason":"joins_leaves_safety_rows_and_anonymous_counters_never_gate","evidence":["E-FAMILY","E-INGESTION-FN"]},
    {"id":"PV-S3","state":"suppression_only_residual","column":null,"reason":"cutover_touch_mark_bot_backfill_and_prompt_guards_carry_no_family_signal","evidence":["E-RESIDUAL-TOUCH","E-RESIDUAL-MARK","E-ONBOARDING","E-INACTIVITY"]},
    {"id":"PV-S4","state":"env_only","column":null,"reason":"deployment_guild_identity_pinned_server_side_never_shown_or_sent","evidence":["E-RECEIVER-PIN","E-VERBS"]},
    {"id":"PV-S5","state":"absent","column":null,"reason":"no_signed_privacy_verb_no_web_view_no_toggle_audit_table","evidence":["E-VERBS","E-WEBV1","E-WEBREADER","E-ROLES-OPT","E-WRITE"]},
    {"id":"PV-S6","state":"never_per_member","column":null,"reason":"no_member_controls_lists_exports_or_deletion_surface_ever","evidence":["E-SUPPRESSION","E-RECEIPTS","E-SELF-SERVE"]}
  ],
  "evidence": {
    "E-READ":{"path":"crates/cutover/src/privacy.rs","line":319,"contains":"pub async fn analytics_settings"},
    "E-DEFAULT":{"path":"crates/cutover/src/privacy.rs","line":335,"contains":"unwrap_or((true, true, true))"},
    "E-WRITE":{"path":"crates/cutover/src/privacy.rs","line":340,"contains":"pub async fn set_analytics_settings"},
    "E-FAMILY":{"path":"crates/core/src/analytics_family.rs","line":34,"contains":"pub fn category_of_event"},
    "E-COLUMN":{"path":"crates/core/src/analytics_family.rs","line":49,"contains":"pub fn toggle_column"},
    "E-COMMUNITY":{"path":"crates/core/src/analytics_family.rs","line":63,"contains":"pub fn community_category"},
    "E-CUTOVER-GATE":{"path":"crates/cutover/src/db.rs","line":343,"contains":"ingestion_blocked_in"},
    "E-STORE-GATE":{"path":"crates/store/src/store.rs","line":154,"contains":"ingestion_blocked_in"},
    "E-STORE-TOUCH":{"path":"crates/store/src/store.rs","line":603,"contains":"category_off_in"},
    "E-FACTS-GATE":{"path":"crates/core/src/community_store.rs","line":94,"contains":"category_off_in"},
    "E-GAME-PRESENCE":{"path":"crates/core/src/game_activity_store.rs","line":407,"contains":"AnalyticsCategory::Presence"},
    "E-GAME-VOICE":{"path":"crates/core/src/game_activity_store.rs","line":427,"contains":"AnalyticsCategory::Voice"},
    "E-RESIDUAL-TOUCH":{"path":"crates/cutover/src/db.rs","line":518,"contains":"suppressed_in"},
    "E-RESIDUAL-MARK":{"path":"crates/cutover/src/db.rs","line":546,"contains":"suppressed_in"},
    "E-BATCH":{"path":"crates/cutover/src/gateway_session.rs","line":277,"contains":"LEFT JOIN guild_analytics_settings"},
    "E-STATUS":{"path":"crates/bot/src/privacy_command.rs","line":234,"contains":"analytics_settings"},
    "E-SELF-SERVE":{"path":"crates/bot/src/privacy_command.rs","line":41,"contains":"member:self-serve"},
    "E-SUPPRESSION":{"path":"crates/cutover/migrations/0433_privacy_opt_out.sql","line":35,"contains":"CREATE TABLE IF NOT EXISTS privacy_opt_outs"},
    "E-TOGGLES":{"path":"crates/cutover/migrations/0433_privacy_opt_out.sql","line":42,"contains":"CREATE TABLE IF NOT EXISTS guild_analytics_settings"},
    "E-RECEIPTS":{"path":"crates/cutover/migrations/0433_privacy_opt_out.sql","line":50,"contains":"CREATE TABLE IF NOT EXISTS privacy_opt_out_audit"},
    "E-TOMBSTONE":{"path":"crates/cutover/src/member_erasure_plan.json","line":348,"contains":"suppression tombstone"},
    "E-VERBS":{"path":"crates/core/src/internal_settings.rs","line":37,"contains":"settings.get"},
    "E-RECEIVER-PIN":{"path":"crates/bot/src/internal_action_http.rs","line":2626,"contains":"TWO_STAGING_GUILD_ID"},
    "E-WEBREADER":{"path":"docs/database-roles.md","line":16,"contains":"two_web_reader"},
    "E-ROLES-OPT":{"path":"docs/database-roles.md","line":56,"contains":"privacy_opt_outs"},
    "E-WEBV1":{"path":"crates/store/sql/web_v1.sql","line":109,"contains":"CREATE OR REPLACE VIEW web_v1.live_counts AS"},
    "E-SUPPRESSED-FN":{"path":"crates/core/src/privacy.rs","line":37,"contains":"pub async fn suppressed_in"},
    "E-INGESTION-FN":{"path":"crates/core/src/privacy.rs","line":143,"contains":"pub async fn ingestion_blocked_in"},
    "E-ONBOARDING":{"path":"crates/core/src/onboarding_store.rs","line":159,"contains":"suppressed_in"},
    "E-INACTIVITY":{"path":"crates/core/src/inactivity_store.rs","line":301,"contains":"suppressed_in"}
  },
  "reasons": {
    "SAVED_UNVERIFIED":{"retryable":false,"next_action":"reread_before_edit"},
    "INHERITED_DEFAULT_ON":{"retryable":false,"next_action":"none"},
    "PREVIEW_ONLY":{"retryable":false,"next_action":"confirm_or_edit"},
    "VERSION_CONFLICT":{"retryable":false,"next_action":"reread_then_reconfirm"},
    "INVALID_VALUE":{"retryable":false,"next_action":"fix_or_reread"},
    "VALIDATION_UNAVAILABLE":{"retryable":true,"next_action":"wait_for_validation"},
    "NOT_EDITABLE":{"retryable":false,"next_action":"none"},
    "FOREIGN_GUILD":{"retryable":false,"next_action":"select_managed_guild"},
    "UNAUTHORIZED":{"retryable":false,"next_action":"reauthenticate_or_stop"},
    "RUNTIME_UNAVAILABLE":{"retryable":true,"next_action":"retry_read"},
    "COMMIT_UNKNOWN":{"retryable":false,"next_action":"reconcile_same_intent"},
    "TRUTH_GAP":{"retryable":true,"next_action":"consult_runbook"}
  }
}
```

Agent result shape (proposed, not a new endpoint):
`{schema,source_sha,guild_id,key,expected_version,request_id,reason,retryable,next_action}`.
No value-bearing logs; previews may show only the admin's private configuration
draft. A source/hash mismatch requires re-inventory, not optimistic editing.

| Fixture | Deterministic oracle for future protected acceptance tests |
| --- | --- |
| `PV-F01` | For each of three families, write via the future CAS writer to disposable guild A, then call its actual runtime consumer; the new value must affect the intended operation. Readback alone does not pass. Page writes stay disabled until W1/W2/W3. |
| `PV-F02` | Non-boolean shapes, bad guild, wrong types: no changed toggle row, audit or revision. Exercise the service refusal directly; do not assume the UI's stricter syntax is enforced there. |
| `PV-F03` | A/B read token t; A saves; B saves t: only A commits, B receives conflict and retains draft. Two creators on a missing row: one success. |
| `PV-F04` | Opt-out interplay: toggle during capture (old write before commit persists, new write drops), stale revision against a concurrent save, opt-out purge racing a disabled family, suppression surviving every toggle state. Pin zero toggle-driven suppression changes. |
| `PV-F05` | Replay same key/body: one change/audit; different body under key: refusal. After a later save, replay old success returns the current token; fresh read required. Side-read failure/terminal commit ambiguity: COMMIT_UNKNOWN, not success. |
| `PV-F06` | Foreign guild B: web blocks before signing, zero requests; a B row in fixture cannot affect A runtime. Independently pin the receiver's staging guild behavior, not an invented body-guild denial. |
| `PV-F07` | Missing-row default: a fresh guild reads inherited-default-on and every family persists; the first write creates the row. |
| `PV-F08` | No row-delete reset: setting all three true is not a delete; dashboard delete is forbidden; prove no three-key side effect from any single-toggle action. |
| `PV-F09` | Failed read, runtime unavailable, unparsable stored row: retain correct last-good/default behavior per consumer; UI stale/read-only with observation time, never applied. |
| `PV-F10` | PII absence: every dashboard response, log line, preview and error is scanned for planted member-ID fixtures; counts match exactly and zero IDs appear. Receipt actor labels never reach the browser. |
| `PV-F11` | Dry-run identical input gives identical result; zero network/signatures/nonce/idempotency/SQL/Discord effects. Blocked, env-only and absent controls have no edit action. |
| `PV-F12` | Private admin session missing/revoked, non-admin or cross-guild: no config value returned and no signature. Confirmation intents require re-authentication; errors cannot leak submitted input. |

All runtime fixtures above are **planned**, not executed by this proposal.
Use disposable agent-testdb/CI databases, isolated guild fixtures and mock
Discord. No staging/production DB or live settings writes. Future staging
application evidence is a separately authorized gate, not a test this document
claims to have run.

Runnable offline consistency check (Python stdlib, from repo root):

```sh
python3 -m doctest -v docs/dashboard-privacy-config-spec.md
python3 scripts/check_docs_index.py --json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
```

This checks toggle inventory, default-on/CAS-less proofs, no-verb/no-view
parity, evidence file/line anchors, state/gate structure and reason
remediations. It runs **none** of PV-F01–F12, performs no runtime/DB test, and
proves no apply timing or web authority. Changed source anchors intentionally
require re-inventory, not a waiver.

```pycon
>>> import json, pathlib, re
>>> doc = pathlib.Path("docs/dashboard-privacy-config-spec.md").read_text()
>>> fence = "`" * 3
>>> m = json.loads(re.search(fence + r"json\n(.*?)\n" + fence, doc, re.S).group(1))
>>> rows = m["keys"]; fam = pathlib.Path("crates/core/src/analytics_family.rs").read_text()
>>> cols = set(re.findall(r'AnalyticsCategory::\w+ => "(\w+)"', fam))
>>> assert {r["column"] for r in rows} == cols == {"presence_enabled", "activity_enabled", "voice_enabled"}
>>> assert len(rows) == len({r["id"] for r in rows}) == 3
>>> assert all(r["class"] == "dedicated_table" and r["state"] == "read_only_blocked" for r in rows)
>>> assert all("PV-W1" in r["gate"] and "PV-W3" in r["gate"] for r in rows)
>>> assert all(r["apply"] == "next_write_per_transaction" for r in rows)
>>> mig = pathlib.Path("crates/cutover/migrations/0433_privacy_opt_out.sql").read_text()
>>> assert all(r["column"] in mig for r in rows)
>>> agg = {a["id"]: a for a in m["aggregates"]}
>>> assert set(agg) == {"PV-P1", "PV-P2", "PV-P3"}
>>> assert agg["PV-P1"]["state"] == agg["PV-P2"]["state"] == "aggregate_only"
>>> assert agg["PV-P3"]["state"] == "absent_until_wired"
>>> assert {s["state"] for s in m["other_states"]} == {"inherited_default_on", "always_on_lifecycle", "suppression_only_residual", "env_only", "absent", "never_per_member"}
>>> assert all(a["evidence"] and all(e in m["evidence"] for e in a["evidence"]) for a in rows + m["aggregates"] + m["other_states"])
>>> assert all(e["contains"] in pathlib.Path(e["path"]).read_text().splitlines()[e["line"]-1] for e in m["evidence"].values())
>>> priv = pathlib.Path("crates/cutover/src/privacy.rs").read_text()
>>> assert "expected_version" not in priv
>>> setregion = priv.split("pub async fn set_analytics_settings", 1)[1].split("#[cfg(test)]", 1)[0]
>>> assert "audit" not in setregion and "actor" not in setregion
>>> assert "unwrap_or((true, true, true))" in priv.split("pub async fn analytics_settings", 1)[1].split("pub async fn set_analytics_settings", 1)[0]
>>> assert "privacy" not in pathlib.Path("crates/core/src/internal_settings.rs").read_text().lower()
>>> views = [l for l in pathlib.Path("crates/store/sql/web_v1.sql").read_text().splitlines() if l.startswith("CREATE OR REPLACE VIEW")]
>>> assert len(views) == 10 and not any("rivacy" in v.lower() for v in views)
>>> assert all(isinstance(r["retryable"], bool) and r["next_action"] for r in m["reasons"].values())
>>> assert set(m["reasons"]) == set(re.findall(r'^\| `([A-Z_]+)` \|', doc, re.M))
>>> assert len(set(re.findall(r'^\| `(PV-F\d+)` \|', doc, re.M))) == 12
>>> assert re.fullmatch(r"[0-9a-f]{40}", m["source_sha"])
>>> print(json.dumps({"schema": m["schema"], "keys": len(rows), "status": "pass"}, sort_keys=True))
{"keys": 3, "schema": "dashboard.privacy_config.v1", "status": "pass"}

```

## Intent gate and bounded follow-ups

| ID | Recommendation, not a build card or API |
| --- | --- |
| `PV-W1` | Add a CAS toggle writer with server-derived actor and append-only audit. Fail closed on stale tokens; never expose the raw table to the web. The dashboard writes only through this writer. |
| `PV-W2` | Add a PII-safe aggregate read path (signed count-only actions or a reviewed count-only projection meeting the verify-read bar). Counts only; member IDs and actor labels never leave the boundary. No direct table or view grants. |
| `PV-W3` | Before any page read, prove web private-admin/guild checks, signer, guild fence and session revocation against the pinned deployment guild. If no trusted read evidence is available, keep unknown-state copy or no-build. |
| `PV-W4` | Hand scope truthfulness to its separate owner with this inventory attached: residual seams, lag bounds and copy rules above. The dashboard links that disposition; it never invents scope claims. |

Cut-ready future slices (each one day or less, none started by this spec):

| Slice | Done when |
| --- | --- |
| `PV-S1` aggregate posture read | Signed count-only actions return exact counts on a disposable DB with planted member IDs; every response/log passes the PV-F10 scan |
| `PV-S2` CAS writer plus audit | Service-level CAS writer with actor and append-only audit passes PV-F01–F03 and PV-F05 on disposable DB; no web involved |
| `PV-S3` web guards proof | Private-admin checks, guild fence and signer pass PV-F06 and PV-F12 against the pinned guild; no values leak to unauthorized sessions |
| `PV-S4` read-only posture page | Toggles plus aggregates render with saved-vs-live/unknown states and pass PV-F07, PV-F09 and PV-F11; no edit control exists |
| `PV-S5` single-family edit | One toggle edits with two-step confirm, dry-run and CAS retry, passing PV-F03–F05 and PV-F08; other families stay read-only |
| `PV-S6` residual disclosure | Page copy and runbook link cover every residual and lag bound; PV-F04 interplay fixtures pass on disposable DB |

No-build alternative: keep `/privacy status` plus the operator-runbook
`staging-privacy-verify-read` counts as the only posture surfaces, and do not
ship a dashboard page. Choose this if W1/W2/W3 cannot be proved, if any slice
would expose a member ID to reach the browser, or if intent is not recorded.

Bind the CEO intent-alignment request to the exact task-document revision,
repository commit and this file's SHA-256. Ask them to record host intent
alignment through the existing authorized path, not a new owner approval hop.
A queued request is not alignment. No build cards before explicit acceptance
and recorded alignment; no activation, spend or permission is granted by either.
Only genuinely new access/data boundaries require CISO disposition. In
particular, the aggregate read path and any future toggle writer cross a new
data boundary and need that disposition before enablement; docs and a fenced
read-only sketch do not themselves authorize or require a new credential grant.

Review by 2026-11-01. Metric: all three toggles, both aggregates and the absent
audit have evidenced reader/state decisions, and all 12 fixture IDs have
accepted/rejected oracles. If evidence is incomplete, keep
aggregate-first/no-build; absent acceptance defaults to **no-build**, never
timeout approval. No live experiment or apply-delay measurement was performed.
Unverified: web guards, deployed capabilities/gates, operator state, runtime
availability and production behavior.

ACT: publish a source-backed proposal and offline consistency check.
WHY: prevent analytics toggles from masquerading as stopped capture, and member
opt-out rows from ever reaching a browser.
UNDO: revert these documentation changes; no operational state changes to undo.
