# Dashboard LFG configuration contract

Status: **proposed; design only**. Written 2026-10-26; intent review by
2026-11-02. This proposal adds no runtime, page, migration, permission, live
write or activation. Publication is not implementation or production approval.
This file is the single repository source, including the manifest and fixture
oracle. It extends neither the ticket/onboarding pages nor the LFG lifecycle
specs. Seats, signup-member and reminder scopes stay with their siblings
(`rsvp-capacity-spec.md`, `rsvp-withdrawal-spec.md`,
`event-reminder-preferences-spec.md`).

## Decision

An organizer needs to see a post's saved state separately from its live
Discord signup post, fix a title or start time without losing signups, and
close a post knowing exactly what happens to the signup rows. A stored row
is **not** a live message: refresh can lag or fail, signups keep arriving,
and the lifecycle sweep can close a post between a dashboard read and a
dashboard write.

**Recommend read-first, not a post editor over today's verbs.** Inventory
at source commit `1441cbef76a1c8237a3a91a8944e2d25b66c4ce4` finds four
shipped organizer operations, but no guild configuration, no signed LFG
verb or revision/CAS column. Writes use advisory locks, and metadata
edit also takes a row lock (`lfg_store.rs:166`); neither supplies a stale
snapshot token. After
intent acceptance, the smallest eventual page can list channel-scoped
open-post rows read-only (slice S1 below). Edit and close wait for LG-W2
(CAS) plus LG-W3 (least-scope write verbs) plus LG-W4 (count-only
readback). If prerequisites cannot be
proved, choose **no-build**, not a form that silently last-writer-wins. Nothing
is built by this spec, including S1.

## Source-backed inventory

Present-vs-proposed, one row per shipped operation. LFG has no guild
configuration: `crates/core/src/settings.rs` contains no `lfg`
(case-insensitive), so there is no stored key to edit, only per-post rows.
There is no signed LFG verb either: `crates/bot/src/internal_action_http.rs`
contains no `lfg` (case-insensitive), so every dashboard read and write
needs a new verb (LG-W1/LG-W3).

| Shipped op and source | Permission and gates | Store, audit and consumer | Proposed page state |
| --- | --- | --- | --- |
| `/lfg` create (`crates/core/src/feature_commands.rs:452-476`): title, future-only starts-at, role spec, preview flag | Manage Events (`crates/core/src/commands.rs:287`) + Announcements row gate (`crates/core/src/router.rs:734-738`) + configured-guild fence; quota 20 open posts per guild (`crates/core/src/automation_quota.rs:9-16`) | `put_lfg` upsert with guild fence and quota lock (`crates/core/src/lfg_store.rs:54-138`); audit `lfg.create`/`posted`; Discord post with signup Select; nonce recovery; `delete_lfg` cascade only on failed-post cleanup (`:420-431`) | `not_exposed`: create needs quota/nonce/Discord post; stays slash-only |
| `/lfg-close` (`feature_commands.rs:477-485`): post id | Same Manage Events + Announcements + guild gates (`router.rs:739-743`) | `close_lfg` open-only update, signups and roles retained (`lfg_store.rs:578-600`); audit `lfg.close`/`closed` or `already_closed_or_missing`; refresh removes the Select (`crates/discord/src/lfg_interactions.rs:784-808`); reply `LFG closed.` / `LFG was already closed or missing.` (`crates/core/src/lfg.rs:691-699`) | `gated_write` after LG-W2 through LG-W4; dry-run plus explicit confirm |
| `/lfg-edit` (`feature_commands.rs:486-518`): post id plus title and/or starts-at, or role+seats together to increase one existing role's seats (never mixed with metadata; `crates/discord/src/interactions.rs:296-337`) | Same gates (`router.rs:744-748`); at least one metadata field (`lfg_interactions.rs:423-430`) or a complete role+seats pair | Metadata: `edit_lfg_metadata` under advisory + row lock; Missing/Closed/Unchanged/Updated; roles/signups/ids preserved; never reopens (`lfg_store.rs:153-203`); audit `lfg.edit`/`updated`; no-op writes nothing (`lfg_interactions.rs:467-470`). Seats: `increase_lfg_seats` is increase-only, refuses started/shrink/missing-role, and commits its `lfg.edit.seats` audit in the capacity transaction (`lfg_store.rs:207-288`) | `gated_write` metadata only after LG-W2 through LG-W4; signup preservation is the confirm copy; seat increase stays slash-only |
| `/lfg-list` (`feature_commands.rs:519-529`): optional cursor | Same gates (`router.rs:749-753`); current channel only; no pause gate exists on this path | `list_lfg_channel`: open + confirmed canonical message id, keyset order, no signup/role/member columns (`lfg_store.rs:376-415`); cursor MAC-bound to guild/channel/query with a process-local key (`crates/core/src/lfg_list.rs:11-66`); each process mints its own key, so restarts and other instances invalidate issued pages (`:19-30`); past/unknown labels, never fabricated (`:157-179`) | `candidate_read` after LG-W1: read-only rows, same predicates |
| Member Select signup/leave (component surface) | Any member; 5 s signup cooldown; capacity adjudication order missing/closed/role/same/full/moved/joined (`lfg.rs:392-423`) | `signup_lfg` treats `starts_at <= joined_at` as closed (`lfg_store.rs:500-514`); receipt `lfg:signup:<interaction>` with versioned binding; leave is id-only after a guild-fence read | `member_only`: the dashboard never signs members up or removes them |
| Lifecycle sweep (`lfg_sweep` job) | Supervisor-only caller; bounded 20 rows; try-lock leaves contended rows (`lfg_interactions.rs:673-782`) | Signups stop at start; sweep closes after a 15-minute grace (`:81`); orphans reconciled by nonce, unrecovered retained rows closed; expired posts lose signup controls on refresh | `automatic`: no control; the page surfaces `expired` truthfully |

**Concurrency without CAS:** `lfg_posts` has no revision column
(migration `crates/cutover/migrations/0170_lfg.sql` carries no
`revision`/`version`). Ordering comes from three advisory locks:
`lfg:{guild}:{id}` for domain writes, `lfg-runtime:{guild}:{id}` for
mutation-plus-refresh, and `lfg-select:{interaction}` for replay identity.
Metadata edit additionally locks the post row with `SELECT ... FOR UPDATE`
(`lfg_store.rs:166`); seat increase locks both post and role rows
(`:241`, `:254`). These serialize writes, not a browser's observed version.
Every dashboard write is read-then-act until LG-W2 lands; see
`VERSION_UNKNOWN` below.

**Audit is shared; seat increase aside, independent:** close and metadata
edit append to the shared `announcements_audit_log`
(`crates/core/src/rsvp_store.rs:286`) with actor, action
(`lfg.close`/`lfg.edit`), target post and outcome
(`lfg_interactions.rs:811-843`). Non-select audit ids embed nanosecond time,
so they are not idempotent receipts. Their store commits stand alone: audit
loss keeps applied state and appends the `Audit could not be recorded`
suffix (`:640-649`); it is never silent rollback and never a second write.
The slash seat increase is the exception: its `lfg.edit.seats` row inserts
in the capacity transaction, and an audit-identity collision rolls the
increase back instead of committing it unaudited (`lfg_store.rs:279-286`).

**Expiry has three faces:** signup-stop at start (store `CASE` plus the
`started` reply), sweep-close after the grace, and list display (`past` /
`Start time unknown`). Metadata-editing a past-start open post is permitted
— that store path checks status only — and moving `starts_at` back to the
future re-opens signups, because signup adjudication reads the live value
at execution. Seat increases differ: the store refuses once
`starts_at <= now`, checked after its locks (`Started`,
`lfg_store.rs:260-264`). The confirm copy must say so; it must also warn
the sweep may still close a post whose start passed the grace.

**The role set is fixed after create; seats can only grow:**
`replace_roles` runs only on the create path (`lfg_store.rs:112-135`); no
shipped function adds, removes or renames a role
(no `edit_lfg_roles`/`update_lfg_roles`). `/lfg-edit` may raise one
existing role's `slots` in place without replacing role or signup rows
(`increase_lfg_seats`, `lfg_store.rs:221-288`); shrinking is refused.
`delete_lfg` cascades to roles and signups and belongs to failed-post
cleanup only. The dashboard exposes neither role/capacity editing nor
delete.

**Reminder interplay is verified none:** `crates/core/src/event_reminder_store.rs`
reads `scheduled_events` plus `event_rsvps` and references no `lfg_posts`.
Editing LFG `starts_at` never reschedules a reminder, and no reminder
consumer reads LFG rows. LG-F06 pins this.

**Activation fence:** `LIVE_CLEARED_CAPABILITIES` contains SelfRoles only
(`crates/core/src/activation.rs:52`). Announcements-gated LFG is not
cleared for the recognized live identity. Preserve that fence.

### Control map

Each proposed control names its exact validator, permission chain, store
call, audit action and downstream consumer. Absent controls have no row to
send and no page affordance.

| Proposed control | Validator (reused, never reimplemented) | Permission chain | Store call | Audit action | Consumers of the write |
| --- | --- | --- | --- | --- | --- |
| Channel list read (S1) | `ListCursor::decode` shape/MAC/scope; canonical snowflakes (`lfg_list.rs:56-66`, `:157-164`); decode failure surfaces `STALE_CURSOR`, never an empty list | Web admin + same-guild Manage Events + Announcements + configured guild + channel-visible, all server-side | `list_lfg_channel` | none (read; matches `/lfg-list`) | Organizer browser only; same columns as the slash list |
| Edit title / starts-at | `valid_post_id` (`lfg.rs:608`), `validate_title` trimmed 1-100 UTF-16 (`:222`), `normalize_starts_at` future-only with the server clock at confirm (`:247`) | Same chain plus a fresh read before confirm; LG-W2 token once it exists | `edit_lfg_metadata` | `lfg.edit`/`updated` (no-op: no audit) | Message refresh; signup adjudication (new starts-at); sweep |
| Close post | `valid_post_id`; dry-run plus explicit confirm naming the observed snapshot | Same chain plus fresh read; close frees one of 20 quota slots | `close_lfg` open-only | `lfg.close`/`closed` or `already_closed_or_missing` | Refresh (Select removed); sweep skips closed; quota release |
| No control: create, delete, role/capacity edit, member signup/remove | absent by design | not exposed | `put_lfg` / `delete_lfg` / `increase_lfg_seats` / `signup_lfg` / `leave_lfg` stay slash/component/sweep-only | no dashboard rows | none |

## Minimal private page contract

Inherited web shell and signer are **not inspected in this bot-only
change**. Before any page build they must prove: private, non-cacheable
admin responses; CSRF protection; current guild membership and Manage
Events authority checked server-side on reads and writes; no signing key
in browser code; and session revocation/failure denial. No raw payload,
Discord API body, member data, signup list or role mentions in logs,
alerts, errors or previews. Titles render as escaped inert text following
the shipped `inert_title` plus `neutralize_mentions` rules, never pings.
Jump links use the `https://discord.com/channels/{guild}/{channel}/{message}`
shape only for channels this admin may already open.

### View and change

- S1 reads one organizer-selected channel through the new signed read
  verb, applying every `list_lfg_channel` predicate in SQL before LIMIT:
  same guild and channel, `status = 'open'`, confirmed canonical message
  id, finite timestamps, keyset order. A documented channel the admin
  cannot open is refused, not listed.
- Each row carries its observed snapshot: status, title, starts-at,
  observed-at, and — for write confirmations only, after LG-W4 — a
  signup **count** from `COUNT(*)`, never identities. The shipped message
  already renders public per-role `n/slots` counts, so a count is not new
  exposure; `created_by` and signup rows never leave the server.
- One-post actions only: edit metadata, or close. No create, delete, role,
  capacity, member-signup, pause-service or restart control.
- Saved-vs-live is explicit: the form edits a snapshot taken at
  observed-at; the live post may since have gained signups, been edited or
  closed by slash, or been sweep-closed. Saving never claims the live post
  still matches the snapshot; the post-write readback says what changed.

### Saved, open, closed, expired, unknown

| Page state | Meaning and source |
| --- | --- |
| `draft_unsent` | Organizer typed an edit or close intent; nothing sent. Dashboard-local. |
| `saved_open` | Last successful read observed `status = 'open'`, with observed-at. |
| `saved_closed` | Last successful read observed closed, with `closed_at` when present. |
| `expired` | Observed `starts_at` passed: signups stopped even if status is still open; the sweep owns the pending close. |
| `unknown` | Read failed, row missing, or stored values unreadable. Never rendered as an empty or default row. |
| `saved_unverified` | A write was acknowledged by the store; channel refresh and application are not confirmed. No state is ever named `applied`. |

### CAS, idempotency and dry-run

No revision token exists today, so reads carry no token and writes cannot
do optimistic concurrency. Until LG-W2: re-read immediately before
confirm, echo the observed snapshot (status, starts-at, count) in the
confirm copy, and refuse to send when the fresh read differs. After a
write, read back before any further edit; a replayed success is not proof
the current value still equals that save. `needs_reconciliation` means
unknown, never a fresh intent to force a retry.

**Dry-run is local preview only:** deterministic
`{schema,guild_id,channel_id,post_id,observed_at,would,reason,retryable,next_action}`
for identical inputs, without network, signing, SQL, audit or Discord
calls. It checks shape/validator rules only, not live status, freshness,
capacity or application. There is no server dry-run verb; do not append a
dry-run flag to a future write verb and assume it prevents a write.

```text
example dry-run close: {"schema":"dashboard.lfg_config.v1","guild_id":"771111111111111111",
"channel_id":"772222222222222222","post_id":"lfg-1401","observed_at":"2026-10-26T04:30:00.000Z",
"would":"close","reason":"PREVIEW_ONLY","retryable":false,"next_action":"confirm_or_edit"}
```

```text
example agent result: {"schema":"dashboard.lfg_config.v1","source_sha":"1441cbef76a1c8237a3a91a8944e2d25b66c4ce4",
"guild_id":"771111111111111111","post_id":"lfg-1401","request_id":"example-request-1","reason":"SAVED_UNVERIFIED",
"retryable":false,"next_action":"reread_before_edit"}
```

Snowflakes above are illustrative fixtures, not live guilds.

### Honest state and failure UX

| Stable page reason | Source / copy | Next action |
| --- | --- | --- |
| `SAVED_UNVERIFIED` | Confirmed write: "Saved. Channel refresh is not confirmed." | `reread_before_edit` |
| `DRAFT_ONLY` | "Nothing sent yet. This draft is only in your browser." | `confirm_or_edit` |
| `PREVIEW_ONLY` | "Preview only. Nothing sent or saved." | `confirm_or_edit` |
| `VERSION_UNKNOWN` | No CAS token exists: "This post has no revision. Re-read right before saving." | `reread_then_reconfirm` |
| `STALE_OBSERVATION` | Fresh read differs from the snapshot: "Someone changed this post. Your draft is kept; review before saving." | `reread_then_reconfirm` |
| `STALE_CURSOR` | List page token invalid or stale (restart or another instance served this page): "This list page is invalid or stale. Start the list again." | `restart_from_first_page` |
| `ALREADY_CLOSED` | Wire `already_closed_or_missing` or fresh read closed: "This post is already closed. Nothing was changed." | `reread_before_edit` |
| `POST_MISSING` | Missing/wrong-guild id: "No such post in this server." | `fix_or_reread` |
| `EXPIRED_START` | Start passed: "Signups already stopped. A future start re-opens them; the sweep may still close this post." | `confirm_or_edit` |
| `INVALID_VALUE` | Local validator or wire refusal: field-level reason only, no echoed input | `fix_or_reread` |
| `VALIDATION_UNAVAILABLE` | Trusted facts unavailable; controls stay disabled | `wait_for_validation` |
| `NOT_EDITABLE` | Create/delete/role/member controls; no signed write | `none` |
| `FOREIGN_GUILD` | Session guild is not the deployment guild; no signed verb | `select_managed_guild` |
| `UNAUTHORIZED` | Session/admin/signature/gate refusal; no policy details | `reauthenticate_or_stop` |
| `RUNTIME_UNAVAILABLE` | "The bot did not answer. Reading and editing are unavailable." | `retry_read` |
| `COMMIT_UNKNOWN` | Write timeout/5xx/ambiguity: "We could not confirm the save. Do not submit a different save yet." | `reconcile_same_intent` |

These are **proposed adapter reasons**, not new bot wire fields. Close and
edit refusals today surface as ephemeral slash replies
(`LFG was already closed or missing.`, `LFG post is closed and cannot be
edited.`); the adapter maps future verb outcomes onto this table without
inventing wire enums.

Guild fence: post ids are globally unique (`id TEXT PRIMARY KEY`) while
every store call fences `guild_id`. The web must compare trusted session
guild to deployment guild **before signing**, for reads and writes. This
proposal offers no production or multi-guild surface. Never retarget a
receiver, reveal environment identity, enable an action gate or widen a
scope to make the page work.

## Machine-readable contract and offline oracle

Stable manifest IDs are proposal-local, not new bot keys, commands or wire
enums. `candidate_read` and `gated_write` are conditional, not launch
authorization. Evidence anchors below name real source readers; the
offline check validates their presence, not runtime effects. `check`
entries name doctest absence assertions, not line anchors.

```json
{
  "schema": "dashboard.lfg_config.v1",
  "source_sha": "1441cbef76a1c8237a3a91a8944e2d25b66c4ce4",
  "controls": [
    {"id":"LG-C1","op":"lfg_create","present":"slash_only","state":"not_exposed","gate":[],"evidence":["E-CREATE","E-PERM","E-ROUTE","E-QUOTA","E-PUT","E-DELETE"]},
    {"id":"LG-C2","op":"lfg_close","present":"slash","state":"gated_write","gate":["LG-W2","LG-W3","LG-W4"],"evidence":["E-CLOSE-DEF","E-PERM","E-CLOSE-ROUTE","E-CLOSE","E-CLOSE-REPLY","E-AUDIT","E-REFRESH"]},
    {"id":"LG-C3","op":"lfg_edit","present":"slash","state":"gated_write","gate":["LG-W2","LG-W3","LG-W4"],"evidence":["E-EDIT-DEF","E-PERM","E-EDIT-ROUTE","E-EDIT","E-SEAT-EDIT","E-VALID-ID","E-VALID-TITLE","E-STARTS","E-AUDIT","E-REFRESH"]},
    {"id":"LG-C4","op":"lfg_list","present":"slash","state":"candidate_read","gate":["LG-W1"],"evidence":["E-LIST-DEF","E-PERM","E-LIST-ROUTE","E-LIST","E-CURSOR","E-CURSOR-KEY","E-CURSOR-MINT","E-CURSOR-SHAPE","E-CURSOR-DECODE","E-RENDER","E-PAST","E-UNKNOWN","E-LIST-HANDLER"]},
    {"id":"LG-C5","op":"member_select_leave","present":"component","state":"member_only","gate":[],"evidence":["E-SIGNUP","E-ADJUDICATE"]},
    {"id":"LG-C6","op":"lifecycle_sweep","present":"job","state":"automatic","gate":[],"evidence":["E-EXPIRY","E-SWEEP","E-CLOSE"]}
  ],
  "other_states": [
    {"id":"LG-S1","state":"config_absent","reason":"no_lfg_guild_config_key_exists","check":"settings_absent"},
    {"id":"LG-S2","state":"verbs_absent","reason":"no_signed_lfg_verb_exists","check":"internal_absent"},
    {"id":"LG-S3","state":"cas_absent","reason":"lfg_posts_has_no_revision_column","check":"ddl_no_revision"},
    {"id":"LG-S4","state":"reminder_decoupled","reason":"no_reminder_consumer_reads_lfg_posts","check":"reminder_no_lfg"},
    {"id":"LG-S5","state":"role_keys_fixed","reason":"roles_cannot_be_added_removed_or_renamed_seats_increase_only","check":"store_seat_increase_only"},
    {"id":"LG-S6","state":"activation_fenced","reason":"announcements_not_cleared_for_live","evidence":["E-ACTIVATION"]},
    {"id":"LG-S7","state":"status_closed_set","reason":"stored_status_is_open_or_closed","evidence":["E-DDL","E-STATUS"]}
  ],
  "evidence": {
    "E-CREATE":{"path":"crates/core/src/feature_commands.rs","line":452,"contains":"CommandDefinition::new(\"lfg\", \"Post a raid/LFG signup with role slots\")"},
    "E-CLOSE-DEF":{"path":"crates/core/src/feature_commands.rs","line":477,"contains":"CommandDefinition::new(\"lfg-close\", \"Close a raid/LFG signup\")"},
    "E-EDIT-DEF":{"path":"crates/core/src/feature_commands.rs","line":488,"contains":"Update LFG metadata or increase one role's seats"},
    "E-LIST-DEF":{"path":"crates/core/src/feature_commands.rs","line":519,"contains":"CommandDefinition::new(\"lfg-list\", \"List open game posts in this channel\")"},
    "E-PERM":{"path":"crates/core/src/commands.rs","line":287,"contains":"pub const PERM_MANAGE_EVENTS: u64 = 8589934592;"},
    "E-ROUTE":{"path":"crates/core/src/router.rs","line":734,"contains":"\"lfg\" => Row {"},
    "E-CLOSE-ROUTE":{"path":"crates/core/src/router.rs","line":739,"contains":"\"lfg-close\" => Row {"},
    "E-EDIT-ROUTE":{"path":"crates/core/src/router.rs","line":744,"contains":"\"lfg-edit\" => Row {"},
    "E-LIST-ROUTE":{"path":"crates/core/src/router.rs","line":749,"contains":"\"lfg-list\" => Row {"},
    "E-VALID-ID":{"path":"crates/core/src/lfg.rs","line":608,"contains":"pub fn valid_post_id(post_id: &str) -> bool {"},
    "E-VALID-TITLE":{"path":"crates/core/src/lfg.rs","line":222,"contains":"pub fn validate_title(title: &str) -> Result<String, TitleError> {"},
    "E-STARTS":{"path":"crates/core/src/lfg.rs","line":247,"contains":"pub fn normalize_starts_at(value: &str, now_unix_ms: i64) -> Result<String, StartsAtError> {"},
    "E-STATUS":{"path":"crates/core/src/lfg.rs","line":277,"contains":"pub enum LfgStatus {"},
    "E-CLOSE-REPLY":{"path":"crates/core/src/lfg.rs","line":691,"contains":"pub fn close_reply(closed: bool) -> String {"},
    "E-ADJUDICATE":{"path":"crates/core/src/lfg.rs","line":392,"contains":"pub fn adjudicate_signup("},
    "E-PUT":{"path":"crates/core/src/lfg_store.rs","line":54,"contains":"pub async fn put_lfg("},
    "E-DELETE":{"path":"crates/core/src/lfg_store.rs","line":420,"contains":"pub async fn delete_lfg("},
    "E-EDIT":{"path":"crates/core/src/lfg_store.rs","line":153,"contains":"pub async fn edit_lfg_metadata("},
    "E-SEAT-EDIT":{"path":"crates/core/src/lfg_store.rs","line":221,"contains":"pub async fn increase_lfg_seats("},
    "E-LIST":{"path":"crates/core/src/lfg_store.rs","line":376,"contains":"pub async fn list_lfg_channel("},
    "E-SIGNUP":{"path":"crates/core/src/lfg_store.rs","line":487,"contains":"pub async fn signup_lfg("},
    "E-CLOSE":{"path":"crates/core/src/lfg_store.rs","line":578,"contains":"pub async fn close_lfg("},
    "E-CURSOR":{"path":"crates/core/src/lfg_list.rs","line":11,"contains":"pub const PAGE_ROWS: usize = 5;"},
    "E-CURSOR-KEY":{"path":"crates/core/src/lfg_list.rs","line":21,"contains":"static CURSOR_KEY: OnceLock<Secret<[u8; 32]>> = OnceLock::new();"},
    "E-CURSOR-MINT":{"path":"crates/core/src/lfg_list.rs","line":42,"contains":"fn encode(&self, guild: &str, channel: &str) -> String {"},
    "E-CURSOR-SHAPE":{"path":"crates/core/src/lfg_list.rs","line":61,"contains":"if bytes.len() < 51 || bytes.len() > 305 || bytes[0] != 1 {"},
    "E-CURSOR-DECODE":{"path":"crates/core/src/lfg_list.rs","line":65,"contains":"cursor_mac(guild, channel, payload).verify_slice(tag).ok()?;"},
    "E-RENDER":{"path":"crates/core/src/lfg_list.rs","line":157,"contains":"pub fn render_page("},
    "E-PAST":{"path":"crates/core/src/lfg_list.rs","line":177,"contains":"(past)"},
    "E-UNKNOWN":{"path":"crates/core/src/lfg_list.rs","line":179,"contains":"Start time unknown"},
    "E-EXPIRY":{"path":"crates/discord/src/lfg_interactions.rs","line":81,"contains":"pub const LFG_EXPIRY_GRACE: time::Duration = time::Duration::minutes(15);"},
    "E-SWEEP":{"path":"crates/discord/src/lfg_interactions.rs","line":673,"contains":"pub async fn sweep_lifecycle("},
    "E-REFRESH":{"path":"crates/discord/src/lfg_interactions.rs","line":784,"contains":"async fn refresh("},
    "E-AUDIT":{"path":"crates/discord/src/lfg_interactions.rs","line":811,"contains":"async fn audit("},
    "E-LIST-HANDLER":{"path":"crates/discord/src/lfg_list.rs","line":5,"contains":"pub(crate) async fn list("},
    "E-QUOTA":{"path":"crates/core/src/automation_quota.rs","line":16,"contains":"Self::OpenLfgPosts => 20,"},
    "E-ACTIVATION":{"path":"crates/core/src/activation.rs","line":52,"contains":"&[LiveCapability::SelfRoles]"},
    "E-DDL":{"path":"crates/cutover/migrations/0170_lfg.sql","line":17,"contains":"CHECK (status IN ('open', 'closed'))"}
  },
  "reasons": {
    "SAVED_UNVERIFIED":{"retryable":false,"next_action":"reread_before_edit"},
    "DRAFT_ONLY":{"retryable":false,"next_action":"confirm_or_edit"},
    "PREVIEW_ONLY":{"retryable":false,"next_action":"confirm_or_edit"},
    "VERSION_UNKNOWN":{"retryable":false,"next_action":"reread_then_reconfirm"},
    "STALE_OBSERVATION":{"retryable":false,"next_action":"reread_then_reconfirm"},
    "STALE_CURSOR":{"retryable":false,"next_action":"restart_from_first_page"},
    "ALREADY_CLOSED":{"retryable":false,"next_action":"reread_before_edit"},
    "POST_MISSING":{"retryable":false,"next_action":"fix_or_reread"},
    "EXPIRED_START":{"retryable":false,"next_action":"confirm_or_edit"},
    "INVALID_VALUE":{"retryable":false,"next_action":"fix_or_reread"},
    "VALIDATION_UNAVAILABLE":{"retryable":true,"next_action":"wait_for_validation"},
    "NOT_EDITABLE":{"retryable":false,"next_action":"none"},
    "FOREIGN_GUILD":{"retryable":false,"next_action":"select_managed_guild"},
    "UNAUTHORIZED":{"retryable":false,"next_action":"reauthenticate_or_stop"},
    "RUNTIME_UNAVAILABLE":{"retryable":true,"next_action":"retry_read"},
    "COMMIT_UNKNOWN":{"retryable":false,"next_action":"reconcile_same_intent"}
  }
}
```

Agent result shape (proposed, not a new endpoint):
`{schema,source_sha,guild_id,post_id,request_id,reason,retryable,next_action}`.
No value-bearing logs; previews may show only the organizer's private draft.
A source/hash mismatch requires re-inventory, not optimistic editing.

| Fixture | Deterministic oracle for future protected acceptance tests |
| --- | --- |
| `LG-F01` | Channel list read returns exactly the `list_lfg_channel` predicates in SQL before LIMIT, keyset order, five rows plus lookahead; `past`/`unknown` labels match `render_page`; query text selects no signup/role/member column; a cursor minted before a restart or by another process decodes to None and surfaces `STALE_CURSOR`, never an empty list or a generic error. |
| `LG-F02` | Interleave a member signup with a metadata edit: both apply; roles, signups and every `joined_at` are byte-identical after the edit; the edit touches title/starts-at only. |
| `LG-F03` | Close a post with signups: all signup/role rows retained; refresh removes the Select; reply `LFG closed.`; a second close surfaces `already_closed_or_missing`, never silent success. |
| `LG-F04` | Observe open, then slash-close: dashboard close surfaces `ALREADY_CLOSED`; dashboard edit refuses without resurrection; the draft is kept for review. |
| `LG-F05` | Revoke Manage Events between read and confirm: server-side recheck refuses; zero store, audit or Discord effects. |
| `LG-F06` | Edit `starts_at`: prove zero reminder reads/writes; `event_reminder_store` still references no `lfg_posts`; RSVP and reminder-preference rows untouched. |
| `LG-F07` | Validator boundaries: empty/oversized ids, UTF-16 vs byte length, blank title, past starts-at, empty optionals; every refusal causes no store/audit/channel effect. |
| `LG-F08` | Past-start open post: signup adjudicates closed with the `started` reply; edit with a future starts-at is permitted yet warns signups re-open and the sweep may still close after the grace. |
| `LG-F09` | Dry-run identical input gives an identical envelope; zero network/signing/SQL/audit/Discord effects. Absent controls have no dry-run action. |
| `LG-F10` | Reads return no `user_id`/`created_by`/signup rows; count-only readback; titles inert with no pings; errors echo no submitted input. |
| `LG-F11` | Create, delete, role/capacity edit (including the shipped slash seat increase) and member signup/remove have no dashboard control path; `delete_lfg` stays failed-post-cleanup-only. |
| `LG-F12` | Close and metadata edit append `lfg.close`/`lfg.edit` with actor and outcome; audit loss keeps applied state with the shipped suffix; refresh failure keeps `saved_unverified`, never `applied`. |

All runtime fixtures above are **planned**, not executed by this proposal.
Use disposable agent-testdb/CI databases, isolated guild/channel fixtures and
mock Discord. No staging/production DB or live LFG writes. Future staging
application evidence is a separately authorized gate, not a test this
document claims to have run.

Runnable offline consistency check (Python stdlib, from repo root):

```sh
python3 -m doctest -v docs/dashboard-lfg-config-spec.md
python3 scripts/check_docs_index.py --json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
```

This checks control inventory, evidence file/line anchors, absence pins,
state/gate structure and reason remediations. Evidence anchors must match
both at `source_sha` (read via `git show`) and in the working tree, so a
stale manifest fails either way. It runs **none** of LG-F01–F12,
performs no runtime/DB test, and proves no apply latency or web authority.
Changed source anchors intentionally require re-inventory, not a waiver.

```pycon
>>> import json, pathlib, re, subprocess
>>> doc = pathlib.Path("docs/dashboard-lfg-config-spec.md").read_text()
>>> fence = "`" * 3
>>> m = json.loads(re.search(fence + r"json\n(.*?)\n" + fence, doc, re.S).group(1))
>>> rows = m["controls"]; states = m["other_states"]
>>> assert [r["id"] for r in rows] == ["LG-C1","LG-C2","LG-C3","LG-C4","LG-C5","LG-C6"]
>>> assert [r["state"] for r in rows] == ["not_exposed","gated_write","gated_write","candidate_read","member_only","automatic"]
>>> assert all(r["evidence"] and all(e in m["evidence"] for e in r["evidence"]) for r in rows)
>>> assert rows[1]["gate"] == rows[2]["gate"] == ["LG-W2","LG-W3","LG-W4"] and rows[3]["gate"] == ["LG-W1"]
>>> assert [s["id"] for s in states] == ["LG-S1","LG-S2","LG-S3","LG-S4","LG-S5","LG-S6","LG-S7"]
>>> assert all(("check" in s) != ("evidence" in s) for s in states)
>>> assert all(e in m["evidence"] for s in states if "evidence" in s for e in s["evidence"])
>>> assert {s["check"] for s in states if "check" in s} == {"settings_absent","internal_absent","ddl_no_revision","reminder_no_lfg","store_seat_increase_only"}
>>> assert "lfg" not in pathlib.Path("crates/core/src/settings.rs").read_text().lower()
>>> assert "lfg" not in pathlib.Path("crates/bot/src/internal_action_http.rs").read_text().lower()
>>> assert "lfg_posts" not in pathlib.Path("crates/core/src/event_reminder_store.rs").read_text()
>>> assert "pub async fn increase_lfg_seats(" in (s := pathlib.Path("crates/core/src/lfg_store.rs").read_text()) and "edit_lfg_roles" not in s and "update_lfg_roles" not in s
>>> assert "revision" not in (d := pathlib.Path("crates/cutover/migrations/0170_lfg.sql").read_text()) and "version" not in d
>>> assert all(e["contains"] in pathlib.Path(e["path"]).read_text().splitlines()[e["line"]-1] for e in m["evidence"].values())
>>> _commit = subprocess.run(["git","rev-parse","--verify",m["source_sha"]+"^{commit}"],check=True,capture_output=True,text=True).stdout.strip()
>>> assert _commit == m["source_sha"]
>>> _at = lambda p: subprocess.run(["git","show",_commit+":"+p],check=True,capture_output=True,text=True).stdout.splitlines()
>>> assert all(e["contains"] in _at(e["path"])[e["line"]-1] for e in m["evidence"].values())
>>> assert all(isinstance(r["retryable"], bool) and r["next_action"] for r in m["reasons"].values())
>>> assert set(m["reasons"]) == set(re.findall(r'^\| `([A-Z_]+)` \|', doc, re.M))
>>> assert len(set(re.findall(r'^\| `(LG-F\d+)` \|', doc, re.M))) == 12
>>> assert re.fullmatch(r"[0-9a-f]{40}", m["source_sha"])
>>> print(json.dumps({"schema": m["schema"], "controls": len(rows), "status": "pass"}, sort_keys=True))
{"controls": 6, "schema": "dashboard.lfg_config.v1", "status": "pass"}

```

## Intent gate and bounded follow-ups

| ID | Recommendation, not a build card or API |
| --- | --- |
| `LG-W1` | Add one signed LFG read verb reusing `list_lfg_channel` predicates, plus web private-admin/guild/channel checks. Fail closed on unvalidated channels; no new data beyond the slash list. Reuse the `ListCursor` shape/MAC with process affinity (single-container deployment); expose a crate-visible mint for the verb (`encode` is module-private today); decode failure maps to `STALE_CURSOR`. S1 prerequisite. |
| `LG-W2` | Add a revision token to `lfg_posts` with conditional edit/close (last-writer-wins ends here), ordered with slash/select/sweep locks. Independent acceptance tests first. Write prerequisite. |
| `LG-W3` | Add signed least-scope write verbs (close + metadata edit only) reusing the exact validators, with server-derived attribution and shipped audit actions. No create/delete/role/capacity/member verbs. |
| `LG-W4` | Add count-only readback (`COUNT(*)`, no identities) plus refresh/application-unknown surfacing, so write confirmations prove signup preservation without PII. |

Smallest future build slice (S1, at most one day, after intent
acceptance): LG-W1 plus a read-only channel list with
saved/open/expired/unknown states and keyset pagination. `saved_closed`
arrives only with the gated-write readback (LG-W2 through LG-W4), never
by inferring close from disappearance. Zero
writes, zero new data, zero migrations. Edit/close builds wait for LG-W2
through LG-W4 with their own acceptance.

Bind the intent-alignment request to the exact task-document revision,
repository commit and this file's SHA-256. Ask for host intent alignment
through the existing authorized path, not a new approval hop. A queued
request is not alignment. No build cards before explicit acceptance and
recorded alignment; no activation, spend or permission is granted by
either. Only genuinely new access/data boundaries require security
disposition; docs and a fenced read-only list do not themselves authorize
or require a new credential/data grant.

### No-build alternative

Organizers already list, edit and close in Discord under Manage Events;
signup and leave are separate component operations available to any
member. A dashboard page adds a new PII-adjacent read surface and — without LG-W2 — a last-writer-wins
write path, for organizers who can already do the work in-channel. The
default recommendation is therefore **no-build until S1 prerequisites are
proven**, and no writes until LG-W2 through LG-W4 are proven.

Overturn evidence: measured cross-channel organizer pain (`/lfg-list` is
per-channel, so multi-channel organizers repeat the read), explicit host
intent alignment with the W-items accepted, or a proven CAS primitive that
removes the silent-overwrite risk. Any of those re-opens S1 only; writes
still wait for their own gates.

Review by 2026-11-02. Metric: all six controls have evidenced
validator/permission/store/audit/consumer decisions and all 12 fixture IDs
have accepted/rejected oracles. If evidence is incomplete, keep
read-first/no-build; absent acceptance defaults to **no-build**, never
timeout approval. No live experiment or apply-delay measurement was
performed. Unverified: web guards, deployed capabilities/gates, actual
guild objects, runtime availability and production behavior.

ACT: publish a source-backed proposal and offline consistency check.
WHY: prevent a dashboard LFG editor from silently dropping signups,
leaking member lists, or last-writer-wins over slash and sweep changes.
UNDO: revert these documentation changes; no operational state changes to
undo.
