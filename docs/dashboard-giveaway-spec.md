# Dashboard giveaway configuration contract

Status: **proposed; design only**. Written 2026-10-26; intent review by
2026-11-01. This proposal adds no runtime, page, migration, verb, permission,
live write or activation. Publication is not implementation or production
approval. This file is the single repository source, including the manifest and
fixture oracle. It owns no sibling scope: entry/withdrawal, eligibility,
inspection, preview, audit-export and strict-mode slices keep theirs.

## Decision

An organizer needs to prepare a giveaway, see whether it is a draft or a live
draw, and run its lifecycle without rewriting history. A browser draft is
**not** a giveaway, and a stored giveaway row is **not** a drawn result.

**Recommend wiring-first, not a dashboard editor over today's surface.**
Inventory at source commit `5e870a8251b3382c7b2f7f72c97f785a6710238e` finds
eight shipped slash subcommands, but no guild template/config store, no signed
giveaway read or lifecycle verb, no revision column, and unaudited
pause/resume/extend. After intent acceptance, the smallest eventual page is a
read-only active list plus detail behind one new least-privilege read verb
(GW-B1 below). Lifecycle writes wait for GW-W1/GW-W2. If prerequisites cannot
be proved, choose **no-build**, not a form that cannot act truthfully. Nothing
is built by this spec, including the narrowed read-only page.

## Source-backed inventory

Giveaways are per-giveaway slash options only: one `giveaway` command with
eight subcommands (`crates/core/src/feature_commands.rs:284`), all gated by
`PERM_MANAGE_GUILD` (`:286`; bit 32, `crates/core/src/commands.rs:286`).
Entry buttons stay open to members. There is no guild template, default, or
configuration row: create carries prize, duration, winners, one required role
and two age gates as invocation options
(`crates/discord/src/giveaway.rs:291-321`). The word "giveaway" appears nowhere
in `crates/core/src/internal_actions.rs`, so no signed internal verb reads or
mutates giveaways today; the web cannot reuse a transport the way the ticket
page reuses `settings.get`/`settings.set`.

The lifecycle vocabulary is five statuses (`crates/core/src/giveaway.rs:63`):
open, paused, drawing, drawn, cancelled. But `drawing` is vocabulary-only:
`set_giveaway_status` has no production callers, and the store flips claimed
rows open/paused-to-drawn directly
(`crates/core/src/giveaway_store.rs:505-509`). The in-flight marker is
`claim_token IS NOT NULL`, never a stored `drawing` row. The quota counts
open/paused/drawing (`giveaway_store.rs:721`) with a 10-active cap (`:702`);
entry capacity is 1,000 per giveaway (`:707`), sized so one 300 s draw run can
re-validate every entrant.

The 15 s draw ticker claims due rows under a 600 s lease
(`crates/bot/src/giveaway_runtime.rs:16`;
`crates/discord/src/giveaway.rs:44`), re-validates every entrant against fresh
membership reads (`:874-912`), commits the draw row plus the drawn flip
atomically (`:842`), audits before announcing (`:860-869`), and retries
announce from the stored row. A transport failure aborts the draw; it never
silently shrinks the eligible set (`:923-929`). A crash between the draw-row
insert and the drawn flip replays the stored row and never re-draws
(`:761-788`).

### Present vs proposed

| Shipped slash behavior | Proposed dashboard control | Page state |
| --- | --- | --- |
| GW-C1 `create`: validates prize/winners/duration (`giveaway.rs:201`), admits under quota+idempotency (`giveaway_store.rs:711-744`), posts the entry message, neutralizes orphans on attach failure (`giveaway.rs:348-375`) | Draft composer; drafts stay browser-local until a submit verb exists | `draft_only` until GW-W1 |
| GW-C2 `list`: open+paused only, ends-first, max 10, 32-char prize preview (`giveaway.rs:154-170`) | Read-only active list + active-row detail (status, `updated_at`, `latest_draw_no`, `announced_at`, deadline, counts) | `candidate_readonly` after GW-W1 |
| GW-C3 `cancel`: open/paused-to-cancelled, clears claims, audited (`giveaway_store.rs:873-887`); can win a race against a draw in flight | Explicit confirm; re-read after commit because cancel-vs-draw races | `disabled_lifecycle` until GW-W1+GW-W2 |
| GW-C4 `end`: claims open/paused (`:371-397`), draws under claim, commits from prior status (`:478-509`), audits, announces (`giveaway.rs:381-422`) | Explicit confirm; refused while claimed; re-read names the actual state | `disabled_lifecycle` until GW-W1+GW-W2 |
| GW-C5 `pause`: open-to-paused only when unclaimed (`giveaway_store.rs:899-916`); leaves the due scan; **no audit row** | Explicit confirm; refused mid-draw | `disabled_lifecycle` until GW-W1+GW-W2 |
| GW-C6 `resume`: paused-to-open with deadline shifted by the paused span (`:924-961`); **no audit row** | Explicit confirm; shows the recomputed deadline before commit | `disabled_lifecycle` until GW-W1+GW-W2 |
| GW-C7 `extend`: open/paused deadline += 1 s–30 d (`giveaway.rs:325-330`; `:969-1007`); paused stays paused; **no audit row** | Explicit confirm; shows the new deadline before commit | `disabled_lifecycle` until GW-W1+GW-W2 |
| GW-C8 `reroll`: requires a stored draw; re-validates members; appends `draw_no+1` (`giveaway.rs:430-524`); full or `single:<id>`; audited (`:1062-1080`) | Explicit confirm; refused before first draw; appends, never rewrites | `disabled_lifecycle` until GW-W1+GW-W2 |

### Control map

| Control | Validator | Permission | Store | Audit | Consumer |
| --- | --- | --- | --- | --- | --- |
| GW-C1 create | `validate_new_giveaway`; slash ranges 1–200 chars, 1–100 winners, 1–43200 min, ages 0–3650 | Manage Server (bit 32) | `create_admitted`, `attach_message` | `giveaway.create` | entry button `gw:` + `admit_entry`; countdown/draw jobs |
| GW-C2 list | none (read) | Manage Server | `list_active` | none | organizer only |
| GW-C3 cancel | status open/paused; id charset `[A-Za-z0-9_-]{1,64}` | Manage Server | `cancel_audited` | `giveaway.cancel` | message refresh; tick skips cancelled |
| GW-C4 end | claim free/expired; prior status open/paused | Manage Server | `claim_specific`, `commit_draw_from` | `giveaway.draw` | announce sweep; members replay from seed |
| GW-C5 pause | status open + unclaimed | Manage Server | `pause_giveaway` | **none shipped** | due scan skips paused |
| GW-C6 resume | status paused; deadline math saturates | Manage Server | `resume_giveaway` | **none shipped** | due scan resumes |
| GW-C7 extend | 1–2592000 s via `extend_deadline_ms`; status open/paused | Manage Server | `extend_giveaway` | **none shipped** | countdown refresh; due scan |
| GW-C8 reroll | prior draw exists; target (if any) is a current winner | Manage Server | `insert_reroll_draw` | `giveaway.reroll` | announce sweep; replay per round seed |

Commands and the supervised ticker serialize on one effects mutex
(`giveaway.rs:110-136`), and every mutating store path takes row locks or
claim-conditional updates, so the dashboard's future verbs must join the same
exclusion rather than invent a parallel writer.

## Minimal private page contract

Inherited web shell and signer are **not inspected in this bot-only change**.
Before any page build they must prove: private, non-cacheable admin responses;
CSRF protection; current guild membership and Manage Server authority checked
server-side on reads and writes; no signing key in browser code; and session
revocation/failure denial. No nonce, claim token or entry identity anywhere
in the page; no prize prose or role mentions in logs, alerts, errors,
dry-run results or any server/read preview. The single prize-text exception
is the browser-local composer preview of the admin's own unsent draft, shown
only to that admin and never sent or logged. Everywhere prize text renders,
it is escaped inert text only, never pings or embeds.

### UI states

| Proposed state | Meaning | Source of truth |
| --- | --- | --- |
| `draft` (saved) | Browser-local unsent draft; explicitly "not a giveaway" | Browser only; no server draft verb exists |
| `live` | Accepting entries | status `open`, unclaimed, before deadline |
| `draw_pending` | Entries closed; deadline passed, draw not yet claimed | status `open`, unclaimed, at/after deadline |
| `paused` | Entries closed, deadline frozen | status `paused` |
| `claimed` | Draw in flight; lifecycle controls locked | `claim_token IS NOT NULL`; lease age vs 600 s |
| `ended` | Drawn; winners final for that round | status `drawn`; substate `unannounced` while `announced_at IS NULL` |
| `cancelled` | Terminal; entries closed, never drawn | status `cancelled` |
| `unknown` | Read failed, row missing, or data damaged | transport error, `None`, or `DrawError::Data` |

Notes the page must not get wrong: `draw_pending` is the normal pre-claim
interval (up to one 15 s ticker cadence, unbounded if the ticker stalls):
entries already refuse as closed at `ends_at`, so it renders never as
accepting entries and never as `unknown`; `claimed` takes precedence once
`claim_token` is set. There is **no** stored `drawing` row to
display (claim-derived only); there is **no** prize-claim/fulfillment state in
the shipped schema (`claimed` here means the draw-claim lease, and the page
says so); `ended` with `announced_at NULL` is final but undelivered, not
pending; `paused` rows show the frozen deadline plus the resume-shifted
preview.

### Truthful draw, cancel, end

A dashboard action can never rewrite an admitted draw: the draw PK
(`guild_id`, `giveaway_id`, `draw_no`) makes a second write of one round a
23505 (`crates/cutover/migrations/0460_giveaways.sql:66`), rerolls append, and
the sweep replays stored rows instead of re-drawing. Cancel clears claims but
only moves open/paused rows; a cancel racing a draw resolves on re-read, and
the page reports the stored outcome, never its intent. `end` on an already
claimed row refuses with `RACE_LOST`; on a lost race the page re-reads and
names cancelled vs drawn. Pause during a draw refuses (claim guard); end during
a pause draws from paused. Every lifecycle reply pattern follows the shipped
`end` shape: commit, then re-read, then name the actual state
(`giveaway.rs:396-413`).

### CAS, idempotency and dry-run

No `giveaways` revision column ships. The proposed expected-state guard is
`(status, updated_at, latest_draw_no, announced_at)` supplied with every
lifecycle intent and re-checked server-side under the row lock before the
mutation; any mismatch refuses with `STALE_STATE` and keeps the draft.
`updated_at` stamps on every mutation (`:404-423` release path included), and
inserts carry monotonic `draw_no`, so the tuple detects lifecycle races,
reroll races and announce transitions without a schema change. The existing
`set_giveaway_status` CAS primitive has no production callers; the proposal
does not wire it silently.

Create-submit has no row to guard, so the tuple does not apply: its
precondition is the explicit absent state — no `giveaways` row exists yet for
the intent's idempotency key, the role the interaction id plays for shipped
create today (`create_admitted` admits nothing on a duplicate id). A
duplicate submit with identical key and body replays the recorded outcome
with no second row, audit or entry message; the same key with a different
body refuses; only a fresh key is a new create intent, admitted under quota.

Idempotency keys are per confirmed intent, allocated fresh, with exact body
bytes retained; retries reuse key+body with fresh nonce/timestamp/signature.
`COMMIT_UNKNOWN` (timeout/5xx/ambiguous claim outcome) never invents a new key
to force a retry: reconcile the same intent, then re-read.

**Dry-run is local preview only:** deterministic
`{schema,guild_id,giveaway_id,expected_state,would,reason,retryable,next_action}`
for identical inputs, without network, signing, nonce burn, idempotency
reservation, SQL, audit or Discord calls. The result carries shape/reason
only, never prize prose. It checks shape/transition rules
only, not membership facts, claim freshness or actual application. There is no
server dry-run verb; do not append `dry_run:true` to a future lifecycle verb
and assume it prevents a write.

### Least scope and attribution

Reads need a new least-privilege verb returning redacted rows only: no
`draw_nonce` (it predicts every winner of an open giveaway), no `claim_token`,
no entrant identities (counts via `entry_count` only), no requirements beyond
what the admin could already set. One narrow exception: `winner_ids` for a
round only when that round's `announced_at IS NOT NULL`, enforced server-side
in the read verb — the field is omitted, not client-hidden, while
unannounced. Announced winners are already published as mentions in the
giveaway channel (`giveaway.rs:1025`), so this reads back facts the guild
already saw; drawn-but-unannounced winners are never returned. Writes split
by effect: create vs
lifecycle-transition vs reroll-append, each refusing outside its transition
set. Attribution derives `created_by`/actor from the authenticated admin's
Discord identity on the server, never editable browser input, matching the
shipped `actor_id` derivation (`giveaway.rs:1129`). Lifecycle audit rows that
do not exist today (pause/resume/extend) must exist before those controls
enable; the page shows no guessed history.

### Honest state and failure UX

No page state is named `applied`. A lifecycle reply confirms **stored commit
only**; announce delivery follows via the sweep and can lag. On read failure
keep an already displayed value labeled stale with its observation time,
disable lifecycle controls until a fresh read, and retain unsent input
privately. Never substitute an empty row for an unavailable read. No durable
offline queue.

| Stable page reason | Source / copy | Next action |
| --- | --- | --- |
| `DRAFT_UNSENT` | "Draft only. Nothing created." | `confirm_or_edit` |
| `PREVIEW_ONLY` | "Preview only. Nothing sent or saved." | `confirm_or_edit` |
| `DRAWN_IMMUTABLE` | Draw row exists: "This round is final. Reroll appends a new round." | `reroll_or_none` |
| `RACE_LOST` | Claim/refusal: "Another draw or cancel won. Re-read before acting." | `reread_before_edit` |
| `STALE_STATE` | Expected-state mismatch: "This giveaway changed. Your draft is kept; reload." | `reread_then_reconfirm` |
| `INVALID_VALUE` | Local validation; field-level reason, no echoed input | `fix_or_reread` |
| `VALIDATION_UNAVAILABLE` | Membership/Discord facts unavailable; controls stay disabled | `wait_for_validation` |
| `NOT_EDITABLE` | Terminal/draft/claimed row; no signed write | `none` |
| `FOREIGN_GUILD` | Session guild not the managed guild; no signed verb | `select_managed_guild` |
| `UNAUTHORIZED` | Session/admin/signature/gate refusal; no policy details | `reauthenticate_or_stop` |
| `RUNTIME_UNAVAILABLE` | "The bot did not answer. Editing is unavailable." | `retry_read` |
| `COMMIT_UNKNOWN` | "We could not confirm the change. Do not submit a different change yet." | `reconcile_same_intent` |

These are **proposed adapter reasons**, not new bot wire fields. There is no
giveaway verb today to carry them; the future adapter maps stored outcomes to
these reasons client-side.

Guild fence: the web must compare trusted session guild to the managed guild
**before signing**, for reads and lifecycle. This proposal offers no
production or multi-guild surface. Never retarget a receiver, reveal
environment identity, enable an action gate or widen a scope to make the page
work.

## Machine-readable contract and offline oracle

Stable manifest IDs are proposal-local, not new bot keys or wire enums.
`candidate_readonly` and `draft_only` are conditional, not launch
authorization. Evidence anchors name real source lines, except explicit
absence assertions (`absent`) naming text the cited file must not contain;
the offline check validates both forms, not runtime effects. `source_sha`
records inventory provenance — the prose inventory commit and the manifest
value must match (asserted below) — while anchors are deliberately checked
against the current checkout, so later source drift fails the check and
forces re-inventory instead of passing on a stale snapshot.

```json
{
  "schema": "dashboard.giveaway.v1",
  "source_sha": "5e870a8251b3382c7b2f7f72c97f785a6710238e",
  "controls": [
    {"id":"GW-C1","slash":"create","state":"draft_only","gate":["GW-W1"],"evidence":["E-CMD","E-PERM","E-VALIDATE","E-CREATE","E-QUOTA","E-AUDIT-CREATE"]},
    {"id":"GW-C2","slash":"list","state":"candidate_readonly","gate":["GW-W1"],"evidence":["E-LIST","E-PERM"]},
    {"id":"GW-C3","slash":"cancel","state":"disabled_lifecycle","gate":["GW-W1","GW-W2"],"evidence":["E-CANCEL","E-PERM","E-AUDIT-CANCEL"]},
    {"id":"GW-C4","slash":"end","state":"disabled_lifecycle","gate":["GW-W1","GW-W2"],"evidence":["E-END","E-COMMIT","E-AUDIT","E-PERM"]},
    {"id":"GW-C5","slash":"pause","state":"disabled_lifecycle","gate":["GW-W1","GW-W2"],"evidence":["E-PAUSE","E-PERM"]},
    {"id":"GW-C6","slash":"resume","state":"disabled_lifecycle","gate":["GW-W1","GW-W2"],"evidence":["E-RESUME","E-PERM"]},
    {"id":"GW-C7","slash":"extend","state":"disabled_lifecycle","gate":["GW-W1","GW-W2"],"evidence":["E-EXTEND","E-VALIDATE-EXTEND","E-PERM"]},
    {"id":"GW-C8","slash":"reroll","state":"disabled_lifecycle","gate":["GW-W1","GW-W2"],"evidence":["E-REROLL","E-ELIG","E-AUDIT-REROLL","E-PERM"]}
  ],
  "other_states": [
    {"id":"GW-S1","state":"draft","reason":"no_server_draft_verb_browser_local_only","evidence":["E-NOVERB"]},
    {"id":"GW-S2","state":"live","reason":"open_unclaimed_before_deadline","evidence":["E-STATUS","E-TRANSITION"]},
    {"id":"GW-S3","state":"paused","reason":"entries_closed_deadline_frozen","evidence":["E-PAUSE","E-TRANSITION"]},
    {"id":"GW-S4","state":"claimed","reason":"claim_token_marks_draw_in_flight_no_stored_drawing_row","evidence":["E-CLAIM","E-COMMIT"]},
    {"id":"GW-S5","state":"ended","reason":"drawn_final_per_round_announce_may_lag","evidence":["E-COMMIT","E-DRAWPK","E-WINNERS-POST"]},
    {"id":"GW-S6","state":"cancelled","reason":"terminal_never_drawn_claims_cleared","evidence":["E-CANCEL"]},
    {"id":"GW-S7","state":"unknown","reason":"read_failure_missing_row_or_damaged_data","evidence":["E-END","E-END-DATA"]},
    {"id":"GW-S8","state":"absent","reason":"no_template_config_history_or_prize_claim_store","evidence":["E-CMD","E-LIST"]},
    {"id":"GW-S9","state":"draw_pending","reason":"open_unclaimed_at_or_after_deadline_entries_closed_draw_not_yet_claimed","evidence":["E-DUE","E-ENTRY-CLOSED"]}
  ],
  "evidence": {
    "E-CMD":{"path":"crates/core/src/feature_commands.rs","line":284,"contains":"giveaway_command"},
    "E-PERM":{"path":"crates/core/src/feature_commands.rs","line":286,"contains":"PERM_MANAGE_GUILD"},
    "E-VALIDATE":{"path":"crates/core/src/giveaway.rs","line":201,"contains":"pub fn validate_new_giveaway"},
    "E-VALIDATE-EXTEND":{"path":"crates/core/src/giveaway.rs","line":325,"contains":"pub fn extend_deadline_ms"},
    "E-STATUS":{"path":"crates/core/src/giveaway.rs","line":63,"contains":"ALL: [Self; 5]"},
    "E-TRANSITION":{"path":"crates/core/src/giveaway.rs","line":299,"contains":"pub fn transition"},
    "E-CREATE":{"path":"crates/discord/src/giveaway.rs","line":348,"contains":"create_admitted"},
    "E-LIST":{"path":"crates/discord/src/giveaway.rs","line":155,"contains":"list_active"},
    "E-CANCEL":{"path":"crates/discord/src/giveaway.rs","line":176,"contains":"cancel_audited"},
    "E-END":{"path":"crates/discord/src/giveaway.rs","line":384,"contains":"claim_specific"},
    "E-END-DATA":{"path":"crates/discord/src/giveaway.rs","line":418,"contains":"Err(DrawError::Data)"},
    "E-WINNERS-POST":{"path":"crates/discord/src/giveaway.rs","line":1025,"contains":"winners_posts("},
    "E-DUE":{"path":"crates/core/src/giveaway.rs","line":334,"contains":"pub fn is_due"},
    "E-ENTRY-CLOSED":{"path":"crates/core/src/giveaway_store.rs","line":800,"contains":"row.ends_at <= now"},
    "E-PAUSE":{"path":"crates/discord/src/giveaway.rs","line":195,"contains":"pause_giveaway"},
    "E-RESUME":{"path":"crates/discord/src/giveaway.rs","line":210,"contains":"resume_giveaway"},
    "E-EXTEND":{"path":"crates/discord/src/giveaway.rs","line":236,"contains":"extend_giveaway"},
    "E-REROLL":{"path":"crates/discord/src/giveaway.rs","line":502,"contains":"insert_reroll_draw"},
    "E-COMMIT":{"path":"crates/discord/src/giveaway.rs","line":842,"contains":"commit_draw_from"},
    "E-ELIG":{"path":"crates/discord/src/giveaway.rs","line":905,"contains":"adjudicate_eligibility"},
    "E-CLAIM":{"path":"crates/discord/src/giveaway.rs","line":44,"contains":"CLAIM_LEASE_SECS"},
    "E-AUDIT":{"path":"crates/core/src/giveaway_store.rs","line":1039,"contains":"audit_draw"},
    "E-AUDIT-REROLL":{"path":"crates/core/src/giveaway_store.rs","line":1062,"contains":"audit_reroll"},
    "E-AUDIT-CREATE":{"path":"crates/core/src/giveaway_store.rs","line":734,"contains":"surface_audit"},
    "E-AUDIT-CANCEL":{"path":"crates/core/src/giveaway_store.rs","line":883,"contains":"surface_audit"},
    "E-QUOTA":{"path":"crates/core/src/giveaway_store.rs","line":702,"contains":"MAX_OPEN_GIVEAWAYS"},
    "E-REDACT":{"path":"crates/core/src/giveaway_store.rs","line":70,"contains":"[REDACTED]"},
    "E-DRAWPK":{"path":"crates/cutover/migrations/0460_giveaways.sql","line":66,"contains":"PRIMARY KEY (guild_id, giveaway_id, draw_no)"},
    "E-NOVERB":{"path":"crates/core/src/internal_actions.rs","absent":"giveaway"}
  },
  "reasons": {
    "DRAFT_UNSENT":{"retryable":false,"next_action":"confirm_or_edit"},
    "PREVIEW_ONLY":{"retryable":false,"next_action":"confirm_or_edit"},
    "DRAWN_IMMUTABLE":{"retryable":false,"next_action":"reroll_or_none"},
    "RACE_LOST":{"retryable":false,"next_action":"reread_before_edit"},
    "STALE_STATE":{"retryable":false,"next_action":"reread_then_reconfirm"},
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
`{schema,source_sha,guild_id,giveaway_id,expected_state,request_id,reason,retryable,next_action}`.
Replay inputs for a round (member replay from the announcement post, not a
dashboard read field set): `{draw_no,seed,eligible_hash,eligible_count,winner_ids,reroll_kind}`.
A dashboard read exposes `winner_ids` from that set only under the
announced-only rule above; before announcement it returns round/count
metadata only. No value-bearing logs; server/read previews, dry-run results
and errors carry no prize prose — only the browser-local composer may
preview the admin's own private draft, including its prize text, to that
admin. A source/hash mismatch requires re-inventory, not optimistic editing.

| Fixture | Deterministic oracle for future protected acceptance tests |
| --- | --- |
| `GW-F01` | Draft vs live: a browser draft creates no row, audit or message; submitting an identical create via the shipped slash path admits exactly one audited row under quota. |
| `GW-F02` | Pause during draw: claimed open row refuses pause; cancel clears the claim and the racing draw reports raced/skipped with no orphan draw row; re-read names cancelled. |
| `GW-F03` | Stale revision: A/B read `(status,updated_at,latest_draw_no,announced_at)`; A pauses; B's resume-with-old-tuple refuses `STALE_STATE` and keeps its draft; B re-reads and reconfirms. |
| `GW-F04` | End races: end on a claimed row refuses `RACE_LOST`; end on paused commits from paused; end after a racing cancel reports closed, never drawn. |
| `GW-F05` | Truthful draw: the prior draw row's stored fields are unchanged after reroll; reroll appends `draw_no+1` with a distinct per-round seed; `single:<id>` keeps other winners; a concurrent writer losing the draw-PK 23505 reports `RACE_LOST`, re-reads and names the stored round, never a partial write. |
| `GW-F06` | Eligibility re-validation: a member who left or lost the role between entry and draw is excluded; a membership transport failure aborts the draw with the claim released, never a shrunk draw. |
| `GW-F07` | Access changes: revoked Manage Server or foreign guild yields no row data and no signature; zero signed verbs fire. |
| `GW-F08` | Refused/uncertain saves: invalid prize/winners/duration/extension/age/id charset changes no row or audit; `COMMIT_UNKNOWN` reconciles the same intent, never a fresh key. |
| `GW-F09` | Announce lag (detail reads beyond GW-B1's active list): drawn-but-unannounced renders `ended/unannounced` (final, undelivered) with no `winner_ids` until `announced_at` is set; the sweep announces from the stored row; no re-draw. GW-B1 itself only drops the row from the active list. |
| `GW-F10` | Resume math: resume shifts the deadline by the paused span (backwards clock: no shift); extend on paused keeps paused; both show the new deadline pre-confirm. |
| `GW-F11` | Dry-run identical input gives identical result; zero network/signatures/nonce/idempotency/SQL/Discord effects. Terminal/claimed rows have no lifecycle action. |
| `GW-F12` | PII/secrecy: read responses carry no nonce, claim token or other entrant identities; `winner_ids` appear only for rounds with `announced_at` set (server-side omission while unannounced); prize renders inert; errors leak no input. |
| `GW-F13` | Draw pending: an open unclaimed row at/after its deadline renders `draw_pending` (entries closed, draw not yet claimed), never `live` or `unknown`; entries refuse closed; after the ticker claims it, the row renders `claimed`, then the stored `ended`/`cancelled` outcome; a lifecycle intent spanning the claim refuses `STALE_STATE`/`RACE_LOST` and re-reads. |

All runtime fixtures above are **planned**, not executed by this proposal.
Use disposable agent-testdb/CI databases, isolated guild/channel fixtures, an
injected draw clock and mock Discord. No staging/production DB or live
giveaway writes. Future staging application evidence is a separately
authorized gate, not a test this document claims to have run.

Runnable offline consistency check (Python stdlib, from repo root):

```sh
python3 -m doctest -v docs/dashboard-giveaway-spec.md
python3 scripts/check_docs_index.py --json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
```

This checks slash/manifest parity, evidence file/line anchors, state/gate
structure and reason remediations. It runs **none** of GW-F01–F13, performs no
runtime/DB test, and proves no apply latency or web authority. Changed source
anchors intentionally require re-inventory, not a waiver.

```pycon
>>> import json, pathlib, re
>>> doc = pathlib.Path("docs/dashboard-giveaway-spec.md").read_text()
>>> fence = "`" * 3
>>> m = json.loads(re.search(fence + r"json\n(.*?)\n" + fence, doc, re.S).group(1))
>>> rows = m["controls"]; src = pathlib.Path("crates/core/src/feature_commands.rs").read_text()
>>> span = src.split("pub fn giveaway_command", 1)[1].split("\n}\n\n/// Announcement", 1)[0]
>>> subs = re.findall(r'CommandOption::new\(\s*"(\w+)",\s*"[^"\n]*",\s*CommandOptionType::SubCommand', span)
>>> assert [r["slash"] for r in rows] == subs == ["create","list","cancel","end","pause","resume","extend","reroll"]
>>> assert len(rows) == len({r["id"] for r in rows}) == 8
>>> assert [r["id"] for r in rows if r["state"] == "draft_only"] == ["GW-C1"]
>>> assert [r["id"] for r in rows if r["state"] == "candidate_readonly"] == ["GW-C2"]
>>> assert all(r["state"] == "disabled_lifecycle" and r["gate"] == ["GW-W1","GW-W2"] for r in rows[2:])
>>> assert rows[0]["gate"] == rows[1]["gate"] == ["GW-W1"]
>>> ids = [r["id"] for r in rows + m["other_states"]]; assert len(ids) == len(set(ids)) == 17
>>> assert {s["state"] for s in m["other_states"]} == {"draft","live","draw_pending","paused","claimed","ended","cancelled","unknown","absent"}
>>> assert all(r["evidence"] and all(e in m["evidence"] for e in r["evidence"]) for r in rows + m["other_states"])
>>> real = {k: e for k, e in m["evidence"].items() if "line" in e}
>>> absent = {k: e for k, e in m["evidence"].items() if "absent" in e}
>>> assert all(e["contains"] in pathlib.Path(e["path"]).read_text().splitlines()[e["line"]-1] for e in real.values())
>>> assert absent and all(e["absent"] not in pathlib.Path(e["path"]).read_text().lower() for e in absent.values())
>>> prod_calls = [str(p) for p in pathlib.Path("crates").rglob("*.rs") if "src" in p.parts and "set_giveaway_status(" in p.read_text()]
>>> assert prod_calls == ["crates/core/src/giveaway_store.rs"]
>>> assert all(isinstance(r["retryable"], bool) and r["next_action"] for r in m["reasons"].values())
>>> assert set(m["reasons"]) == set(re.findall(r'^\| `([A-Z_]+)` \|', doc, re.M))
>>> assert set(re.findall(r'^\| `(GW-F\d+)` \|', doc, re.M)) == {f"GW-F{i:02}" for i in range(1, 14)}
>>> assert re.fullmatch(r"[0-9a-f]{40}", m["source_sha"])
>>> assert re.search(r"Inventory at source commit `([0-9a-f]{40})`", doc).group(1) == m["source_sha"]
>>> print(json.dumps({"schema": m["schema"], "controls": len(rows), "status": "pass"}, sort_keys=True))
{"controls": 8, "schema": "dashboard.giveaway.v1", "status": "pass"}

```

## Smallest future build slice

GW-B1 (read-only active list + active-row detail, <=1 day after GW-W1): one
new signed read verb returning redacted active rows only — `list_active`
parity (open/paused; claimed rows included until the flip) — with status,
`updated_at`, `latest_draw_no`, `announced_at` (the exact expected-state
fields a later lifecycle intent must supply), prize, deadline and counts;
never nonce, claim token or other entrant identities, and `winner_ids`
only under the announced-only rule. GW-B1 renders only the states reachable
in that
scope — `draft`, `live`, `draw_pending`, `paused`, `claimed`, `unknown`;
`ended`/`cancelled` are defined above for later detail/history slices, and
GW-B1 shows a vanished row only as no-longer-active, never a guessed
`ended`. Stale-on-failure reads and local dry-run preview. No create
submit, no lifecycle write, no reroll, no history beyond `list_active`
parity, no audit reader. Anything larger is a new card.

## No-build alternative

Keep slash-only giveaways: organizers already create, list, cancel, end,
pause, resume, extend and reroll from Discord with shipped validation,
claim-lease safety and audit for create/cancel/draw/reroll. A dashboard that
cannot read rows (no verb), cannot prove freshness (no revision), and cannot
attribute lifecycle edits (missing pause/resume/extend audit) adds confusion,
not parity. This alternative stands unless **all** overturn evidence lands:
recorded host/CEO intent alignment for a giveaway page; a reviewed, least-scope
signed read verb with redaction proofs; expected-state guards proven against
races GW-F02–F04; pause/resume/extend audit before those controls enable; and
measured organizer demand (support volume or explicit requests) that slash
cannot serve. Absent acceptance defaults to **no-build**, never timeout
approval.

## Intent gate and bounded follow-ups

| ID | Recommendation, not a build card or API |
| --- | --- |
| `GW-W1` | Add one least-privilege signed giveaway read verb with redaction proofs (no nonce/claim token/other entrant identities; `winner_ids` only for announced rounds), active rows first (`list_active` parity; any history/detail read beyond that is a separate decision), guild-pinned like the existing receiver, plus server-side Manage Server checks. No lifecycle verb is assumed. |
| `GW-W2` | Define lifecycle verbs (create-submit, transition, reroll-append) with expected-state guards, per-effect scopes, server-side attribution, pause/resume/extend audit, and race fixtures GW-F02–F05 green before any control enables. |
| `GW-W3` | Before any build, prove web private-admin/guild checks, signer, session revocation, and inert PII-safe rendering. If no trusted effective-state evidence is available, explicitly keep read-only copy or no-build; invent no diagnostics endpoint. |

Bind the CEO intent-alignment request to the exact task-document revision,
repository commit and this file's SHA-256. Ask them to record host intent
alignment through the existing authorized path, not a new owner approval hop.
A queued request is not alignment. No build cards before explicit acceptance
and recorded alignment; no activation, spend or permission is granted by either.
Only genuinely new access/data boundaries require CISO disposition: a future
entrant-identity or audit reader would cross one, but this proposal's reads
— counts, plus `winner_ids` only for already-announced rounds — disclose
nothing the giveaway channel has not already published, so they do not.

Review by 2026-11-01. Metric: all eight controls have evidenced
validator/permission/store/audit/consumer decisions and all 13 fixture IDs
have accepted/rejected oracles. No live experiment or draw-latency measurement
was performed. Unverified: web guards, deployed capabilities/gates, actual
guild objects, operator overlay state, runtime availability and production
behavior.

ACT: publish a source-backed proposal and offline consistency check.
WHY: prevent a dashboard from presenting drafts as live draws or rewriting
admitted history. UNDO: revert these documentation changes; no operational
state changes to undo.
