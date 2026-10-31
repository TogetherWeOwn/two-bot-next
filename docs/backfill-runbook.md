# Backfill operator runbook

Operator guide to the cutover backfill path: Discord rate-limit behavior,
per-family resume cursors, dry-run staged-delete counts, and the failure
anchors a re-run report can print. This covers the backfill CLIs and the
read-only progress route; it extends the alert coverage in
[runbook.md](runbook.md) (REST 429, DB pool/errors, gateway missed events)
and does not repeat it.

Status: this describes shipped source, not proof that any backfill ran or
that staging/production data is complete. Execution is out of scope here:
staging execution and secret binding belong to Operator cards, and the
live-guild fence below refuses the production guild without an
owner-approved rollout flag. No command in this file was run against a
live database or Discord to write it; see [Verification](#verification).

## 1. Rate limits: the 50/s budget and `Retry-After`

Discord caps bots at 50 requests/second globally. Both REST paths below
have a 110 ms local pacing floor (≈9/s per lane), and both honor the
response's own wait on 429. The durable send lane remains the token-wide
admission authority. The runtime executor also shares a 20 ms global pacing
floor plus a 2 ms safety pad and header-driven bucket deadlines across
executors on one guard. It stamps actual pacing releases and rechecks the
interval after every wake, so delayed waiters cannot spend overdue reservations
together. A bare 429 has `Retry-After` but no `X-RateLimit-*` headers or body
`retry_after`/`global`/`scope` signals. After body classification, its usable
header installs or extends the learned route's deadline from header receipt by
the observed wait plus 250 ms. A shorter hint never shortens a live deadline,
and unrelated routes are not globally paused. Responses with body signals keep
their existing retry policy, including its wait ceilings. The tests in
`crates/discord/tests/backfill_ratelimit.rs` cover header-driven per-route
buckets and `Retry-After` / `X-RateLimit-Reset-After` retries, including
delayed-observer and startup-delay regressions. The unchanged acceptance suite
`crates/discord/tests/bare_429_shared_state_acceptance.rs` covers the bare
header, concurrent executors, and existing single-worker/full-header paths.
These runtime guarantees do not add bucket handling to the separate cutover CLI
client below.

### Cutover CLIs (`crates/cutover/src/rest.rs`)

- The client also has a 110 ms local floor between requests
  (`rest.rs:210`; module contract `rest.rs:4`). This is not the admission
  authority: `RestClient::from_env` binds the durable token-wide PostgreSQL
  lane (`rest.rs:146-187`; [send-admission runbook](discord-send-admission.md#transportbootstrap-handoff)).
  The same token and database share that lane with the live bot. A dry run
  of a Discord-reading CLI also acquires and releases lane occupancy;
  `dedupe-events` does not use Discord or this lane. Concurrent same-token
  writers are refused by the lane rather than coordinated by the local floor;
  backfill traffic also competes with live-bot traffic.
- A 429 uses local `retry_after_wait_ms()` pacing inside the CLI attempt
  budget (`rest.rs:268`, `rest.rs:304`, `rest.rs:476`): header seconds (body
  `retry_after` wins when finite), × 1000, + 250 ms padding, locally clamped
  to 60 s (`crates/discord/src/executor.rs:319`; delegated calculation
  `crates/core/src/action_outcomes.rs:163-181`; padding
  `RETRY_AFTER_PADDING_MS:179`, clamp `MAX_RETRY_AFTER_MS:181`). Missing or
  garbage timing falls back locally to 1 s + 250 ms (`action_outcomes.rs:386-388`).
  Separately, the durable lane promotes Discord cooldowns to the token-wide
  hold; a valid finite hold is not capped at 60 s, and missing or
  unrepresentable timing installs an indefinite hold (`discord-send-admission.md:53-58`).
  The CLI's shorter local sleep does not clear that hold: a later attempt can
  fail closed as admission-blocked, with no self-healing of an indefinite
  hold. A 429 can therefore pause live-bot sends too. Do not restart the
  process, delete lane rows, or treat a local retry as proof the lane reopened;
  use the [send admission blocked alert](runbook.md#alert-send-admission-blocked) and Operator recovery path.
  CLI startup prints `send admission bootstrap failed: ...` when the
  required authority cannot connect (`backfill.rs:90-93`,
  `backfill_messages.rs:65-68`, `capture.rs:57-60`). A missing admission
  table instead surfaces as an admission storage failure on the first
  request; it is not the bootstrap error (`crates/core/src/send_admission/postgres.rs:110-139`).
- 5xx backs off at 500 × 2^attempt ms and gives up as retry-exhausted
  (`rest.rs:270`, `rest.rs:306`, `rest.rs:478`;
  `backoff_duration` at `rest.rs:707-709`, base `BACKOFF_BASE_MS` in
  `action_outcomes.rs:184`). Total budget is 5 tries per call
  (`MAX_HTTP_TRIES` in `action_outcomes.rs:188`).
- 403/404 on a page read as `Unreadable`, not an abort (`rest.rs:266`):
  a channel the token cannot see is skipped, and the scan report says so
  (see §4). Anything outside 2xx/403/404/429/5xx is a hard `Wire` error.
- Every run prints its own Discord cost (`rest.requests()`,
  `backfill.rs:472-475`, `backfill_messages.rs:326-330`): requests plus
  wall time. A successful re-run over already-populated history can scan
  pages without inserting rows. Read the scan-completion and
  `INCOMPLETE: unreadable` entries to distinguish a complete no-op from
  unreadable pages; request count alone is not proof of successful work.

### Runtime executor (`crates/discord/src/executor.rs`)

For backfills that move onto the runtime path, the same posture holds
with shared admission:

- Paced lane 110 ms, kick lane 350 ms (`PACE_INTERVAL_MS` at
  `executor.rs:66`, `KICK_INTERVAL_MS` at `executor.rs:69`, wired at
  `executor.rs:971-972`).
- The backfill evidence reads (`get_json_observed`, `get_json_checked`)
  retry a per-route 429 by waiting out the header unclamped — one sleep per
  429 — bounded by a 5 min ceiling per wait
  (`MAX_BACKFILL_RETRY_AFTER_MS` in `crates/core/src/action_outcomes.rs`),
  and spend one of the same 5 tries per 429/5xx/transport failure; a steady
  429 stream exhausts the budget instead of parking forever. The general
  `get_json` read (containment snapshot, website jobs, onboarding probe)
  keeps the 60 s `retry_after_wait_ms()` clamp, as do the kick, publish
  and registry lanes. In the kick path, a global 429 uses the
  header-anchored shared deadline at the next admission, while route-local
  429s sleep locally.
- The process-wide guard (`crates/discord/src/ratelimit_guard.rs`) counts
  401/403/429 toward the invalid-request budget — 10-minute rolling window
  (`INVALID_REQUEST_WINDOW:15`), default threshold 5 000 (production
  default; `DEFAULT_INVALID_REQUEST_THRESHOLD:16`) — and opens the breaker
  past it. A global 429 extends (never shortens) a shared pause; missing
  or invalid timing fails closed for a full window rather than
  re-hitting 429 (`observe_global:246-250`, `global_until:261-270`;
  `missing_global_timing_fails_closed` pins this). All executors share one
  guard through `process_guard` (`ratelimit_guard.rs:330-346`), including
  API-proxy callers.

First response to a 429 burst: read the run's request count, confirm the
wait came from the response (not a guessed sleep), honor it, and do not
add concurrency. Never replay uncertain writes to "catch up".

## 2. Per-family cursors and resume

Nine endpoint families share one vocabulary
(`crates/store/src/backfill_cursors.rs:24-34`): `audit`, `members`,
`history`, `event_users`, `reactions`, `pins`, `bans`, `invites`,
`onboarding`.

- `put_backfill_cursor` and `get_backfill_cursor` define the checkpoint
  interface (`backfill_cursors.rs:65-100`), but as of this head no shipped
  binary or runtime job calls them. The route in §2 therefore reports all
  nine families as `cursor: null` / `updated_at: null` in normal operation;
  these are schema and route contracts, not usable live checkpoints. The history CLIs
  (`backfill`, `backfill-messages`) do not checkpoint cursors: resume today
  means re-running the relevant history scan with a deeper `--max-pages`
  (see §4), which re-walks channel history.
- The member-catchup module defines a separate
  `member_catchup_cursors` resume table (`member_catchup.rs:156-199`); its
  database operations are exercised by tests, but no shipped binary or
  runtime job currently invokes that worker path. The `members` row in
  `backfill_cursors` is only a progress mirror in the module contract, not
  the member worker's resume state.
- The `backfill_cursors` SQL is byte-for-byte the acceptance target's
  literals (`PUT_SQL:43-46`, `GET_SQL:49`). Database fence:
  `cursor TEXT NOT NULL CHECK (length(cursor) > 0 AND cursor <> '0')` in
  `crates/store/migrations/0408_backfill_cursors.sql` and its cutover-chain
  mirror `crates/cutover/migrations/0435_backfill_cursors.sql` (identical
  `CREATE TABLE IF NOT EXISTS`, additive-only, cross-runnable in either
  order). A zero restart cannot be persisted even if validation is
  bypassed. Privacy fence: `family`, `cursor`, `updated_at` only — no
  bodies, status text, or payload columns.

### Reading the frontier: `GET /ops/backfill/progress`

The private route (`crates/bot/src/ops_backfill.rs:71`) exposes one entry
per family for operators. Auth reuses the `TWO_INTERNAL_KEYS` ring with
`x-two-*` headers over a path-bound canonical string
(`GET\n/ops/backfill/progress\n{ts}\n{nonce}\n{sha256_hex(body)}`,
`ops_backfill.rs:195-204`); a signature cut for `/internal/actions` never
authorizes a backfill read (`210-248`, pinned by
`actions_signature_never_authorizes_a_backfill_read`). Every auth failure
is one indistinguishable 401 with no family data (`299-314`); a
configured-but-unreadable database is a 503, never a zeroed 200
(`316-331`, `398-404`). With no database the route reports the
empty-store baseline (`376-386`).

Interpreting the body (`families_body`, `270-297`):

- `cursor` + `updated_at` (epoch seconds) are exposed verbatim. At this
  head, all nine families return null because no shipped process writes
  these rows; null is not evidence that a family has never run. Once a
  wired worker writes a row, its timestamp can help locate a stalled
  checkpoint, but it cannot currently diagnose one.
- `progress` is `0.0` and `eta_seconds` is `null` by ruling until a
  worker slice records totals (`ops_backfill.rs:32-42`); do not report ETA
  from this route.
- Only vocabulary keys are exposed; unknown stored families never become
  labels (`267-276`).

## 3. Dry-run and staged-delete counts

`backfill`, `backfill-messages`, and `capture` WRITE by default; pass
`--dry-run` to suppress backfill data writes. This does not prevent those
CLIs from applying migrations when the schema is behind (see the migration
gate below). `dedupe-events` is the only CLI here that defaults to a dry
run (writes require `--apply`); its `--dry-run` suppresses delete writes,
not migrations (it does not migrate). The history CLIs print a `DRY RUN`
banner when that flag is passed. Env for the
Discord-reading CLIs: `DISCORD_TOKEN` (or `DISCORD_BOT_TOKEN`),
`DISCORD_GUILD_ID`, `TWO_DATABASE_URL` (`backfill.rs:8-9`, token fallback
`45-49`, guild/DB checks `74-87`). Even dry runs of those
Discord-reading CLIs require the shared send-admission authority described
in §1.

| Binary | Dry-run form | Write form | Banner / gates |
|---|---|---|---|
| `backfill` (member joins/leaves/voice + invite baseline) | `backfill --dry-run` (usage `backfill.rs:26`; flag `63`) | bare `backfill` writes; invite baseline skipped while dry (`392-404`) | `DRY RUN - nothing will be written` (`101-106`); bot marks, event writes and activity touches all gated (`131`, `326`, `451`) |
| `backfill-messages` (first/second/third message milestones) | `backfill-messages --dry-run` (flag `backfill_messages.rs:29`) | bare writes; milestones only ever move earlier, never later (module docs `1-8`) | banner (`76-80`); ladder writes and activity touches gated (`265-301`) |
| `capture` (join capture) | `capture --dry-run` (flag `capture.rs:26`) | bare writes | banner (`68-70`); writes gated (`145`, `167`) |
| `dedupe-events` (clear double-logged joins/leaves) | default: counts without writing (`dedupe_events.rs:72`) | bare `--apply` deletes in 200-id chunks (`133-143`); prints `deleted {n}` | `DRY RUN - nothing will be deleted` (`78-82`); dry prints `would delete {found}` plus per-type rows/people/dupes and the remaining total (`124-129`, `159-166`) |
| retention job (supervised bot job; `retention_jobs.rs`) | `TWO_RETENTION_MODE` unset (default) or `dry-run` reports per-class counts without deleting; runs every 15 minutes (`retention_jobs.rs:23,29,33-43`) | `TWO_RETENTION_MODE=execute` enables deletes; `off` parks the job. See [privacy retention settings](privacy.md#retention-and-explicit-exceptions) | per-class counts are `presence_raw_deleted`/`voice_raw_deleted`/`audit_cache_deleted`/`guild_rows_purged` (`retention.rs:82-90`); receipts, not row counts, for user deletions (`140-144`); `total_deleted` sums disjoint row classes (`146-151`). Stale game-activity aggregates log a `game_activity_aggregate_expiry` event in both modes (`stale_rows`, `horizon_days`, `mode`, `guilds_visited`, `guilds_deferred`): dry run counts the visited page's full stale rows without deleting (a same-predicate upper bound per pass), execute deletes at most the per-guild row batch per visited guild |

Safety rails, verified in code:

- Live-guild fence runs before any database or file opens: the
  production guild refuses without bare `--allow-live-guild`
  (`backfill.rs:37-45,90`; `backfill_messages.rs:67-78`;
  `capture.rs:40-55`; dedupe `validate_options` with `--guild`
  snowflake check). Exit 2, `Refusing live guild`, no connection
  attempt. Staging runs always name the staging guild id from the
  approved deployment record; never substitute production.
- `dedupe-events` refuses `--apply` + `--dry-run` together, bare-flag
  values (`--apply=false`), duplicates, and unknown options
  (`dedupe_events.rs:45-58`); `--guild <snowflake>` is required (`55`).
  The shared parser likewise refuses `--apply` with `--dry-run`
  (`cli.rs:162-164`) and valued safety opt-ins (`cli.rs:128-149`).
- Migration behavior differs by binary: `backfill`,
  `backfill-messages` and `capture` open the DB with migrations enabled
  (`open_db(&args, false)` — `false` = do not skip;
  `backfill.rs:107`, `backfill_messages.rs:87`, `capture.rs:76`;
  `connect` applies unless skipped, `db.rs:25-31`; `open_db`,
  `cli.rs:260-273`). `dedupe-events` is maintenance-only and never
  migrates (`open_db(&args, true)`). A dry run against a schema behind
  the binary may therefore apply migrations on the history CLIs — run
  the reviewed staging migration path first, never a bare CLI as a
  migrator.
- `--max-pages` accepts integers in `1..=10000` and bounds each channel walk
  (`backfill` default 25, `backfill.rs:75-78`; `backfill-messages` default 60,
  `backfill_messages.rs:42-45`). Hitting the cap prints `INCOMPLETE` with a
  saturating 4× re-run suggestion clamped to 10000 (`backfill.rs:475-481`,
  `backfill_messages.rs:326-332`); a capped scan is a lower bound, not a
  complete result. At 10000, review scan scope rather than increasing the cap.
- `capture` and `backfill-messages` reject noncanonical `DISCORD_GUILD_ID`
  before the live fence. A supplied message `--since` must be valid RFC3339;
  malformed values never silently remove the time bound. Shared CLI parsing
  rejects `TWO_DB_POOL_MAX` outside `1..=32` (absent defaults to 5) before
  admission, file or database I/O, including rollback import plan output.
  These fixed refusals exit 2, name `code=` and `next_action=`, and never echo
  rejected values. See [input preflight](cutover.md#cutover-cli-input-preflight).

Staging-name-only recipe (no secrets, no live run performed for this
doc; Operator cards own execution and bindings):

```sh
export DISCORD_GUILD_ID="$STAGING_GUILD_ID"
export TWO_DATABASE_URL="$STAGING_DIRECT_URL"
backfill --dry-run --max-pages=25
backfill-messages --dry-run --max-pages=60
dedupe-events --guild "$STAGING_GUILD_ID" --dry-run
```

`$STAGING_GUILD_ID` is the non-secret staging guild id from the approved
deployment record; `$STAGING_DIRECT_URL` is the Operator-bound direct
(non-pooler) staging URL, never echoed or committed. Production values
are never substituted.

## 4. Failure-burst anchors

Guild-channel discovery is required by both `backfill` and
`backfill-messages`; active-thread discovery is also required by
`backfill-messages`. An unavailable result (including 403/404, an unreadable
response, or a request/transport error) stops the CLI immediately with exit 1
and a fixed stderr diagnostic:

- `Guild-channel discovery unavailable; backfill stopped.`
- `Active-thread discovery unavailable; backfill stopped.`

These failures do not print a completed scan report or an `exactly` claim,
and diagnostics do not include remote error text, response bodies or
credentials. Check access and REST/send-admission health before re-running.
A valid empty discovery result is not a failure: it keeps the normal
successful scan/count output. This refusal is not a rollback of earlier
setup: the CLIs still open/migrate the database before discovery, and
`backfill` may already have marked listed bots outside dry-run mode.
Archived-thread discovery and its existing per-forum incomplete-history
reporting are unchanged.

A re-run report ends with a `scan completion` line plus one `INCOMPLETE`
line per interrupted channel (`ScanReport::render`, `cli.rs:38-55`).
The vocabulary is fixed (`ScanCompletion`, `rest.rs:751-783`):

| Token | Meaning | Complete? |
|---|---|---|
| `end-of-history` | walked to the oldest message | yes |
| `time-boundary` | reached the requested `--since` bound | yes, for the window |
| `page-cap` | hit `--max-pages`; sets `truncated` (`rest.rs:679`) | no — re-run deeper (4× suggestion above) |
| `unreadable` | 403/404 page: token cannot see the channel | no — check access, not a bug |
| `retry-exhausted` | 5xx survived all 5 tries (`rest.rs:271`) | no — check Discord status / service health |
| `request-failed` | transport/Twilight/wire failure; includes exhausted 429s (`HTTP 429`) and send-admission refusals mapped to `Wire` (`rest.rs:263, 647`; `crates/cutover/src/rest.rs:272`) | no — check REST 429 and [send-admission blocked](runbook.md#alert-send-admission-blocked) alerts before treating it as a Discord outage |
| `invalid-response` | page body failed to decode (`rest.rs:645`) | no — one bad row fails its page; report it |

Burst triage:

- `has_incomplete_history` is true unless every channel ended at
  `end-of-history` (`cli.rs:31-36`); the message-scan summary prints
  `lower bound - incomplete history` in that case
  (`backfill_messages.rs:246-254`).
- Probe misses are not failures: each candidate channel is probed for
  one page first, and channels under a 5% funnel-data hit rate are
  skipped as mixed feeds (`backfill.rs:188-223`). Skipped channels are
  expected, not data loss.
- Member-list traversal has hard ceilings: 500 pages / 500 000 members
  (`MAX_MEMBER_PAGES`, `MAX_MEMBERS`, `rest.rs:55-57`); a non-advancing
  page is `MemberCursorStalled` and a ceiling hit is
  `MemberCeilingExceeded` (`RestError`, `rest.rs:44-50`). These variants
  are library-level errors and are not printed by `backfill` or `capture`.
  Both binaries discard any member-list error and print a
  `Read zero members ... Server Members Intent` hint before exiting 1
  (`backfill.rs:110-123`, `capture.rs:120-128`); the wording differs between
  them. The hint can also mask a stalled cursor, ceiling, exhausted 429/5xx
  retries, or admission refusal; check REST and send-admission alerts before
  concluding the intent is off. This is not proof of an empty guild or a
  complete roster.
- Archived-thread discovery has its own budget
  (`DEFAULT_ARCHIVED_THREAD_PAGES = 10`, `rest.rs:798`); early stops are
  reported per forum (`backfill_messages.rs:319-325`), not silently
  complete.
- Route-level anchors: progress-route 503 means the database is
  unreadable (do not read it as "not started"); uniform 401s (bare,
  wrong-key, forged, stale, replayed) share one refusal shape with no
  family data, pinned by `bare_and_wrong_key_share_one_refusal` and
  `unsigned_and_mistaken_backfill_callers_share_one_401_refusal`.
- For sustained REST pressure, DB trouble, or gateway gaps behind a
  stuck backfill, use the existing [runbook.md](runbook.md) alerts (REST
  429 share, DB pool saturation / error bursts, gateway missed events and
  capture-gap guidance) — this file adds the backfill layer, not a
  second copy of those pages.

## Verification

Verified read-only against the tree at this PR's head; no Discord call,
no database connection, no secret use:

```sh
cargo fmt --all -- --check
grep -rn "BACKFILL_FAMILIES\|put_backfill_cursor\|get_backfill_cursor" crates/store/src/backfill_cursors.rs
grep -rn "X-RateLimit-Reset-After\|reset_after\|x-ratelimit-reset" crates/discord/src crates/cutover/src || echo "no shipped Reset-After reader"
grep -n "would delete\|chunks(200)\|DRY RUN" crates/cutover/src/bin/dedupe_events.rs
grep -n "ScanCompletion::" crates/cutover/src/rest.rs
```

- `cargo fmt --all -- --check`: run per the PR checklist (docs-only
  change; result recorded in the PR Verification section).
- The checks above cover the cursor vocabulary and store interface, the
  shipped `Reset-After` reader search, dedupe output terms, and all
  `ScanCompletion` occurrences in `crates/cutover/src/rest.rs`. They do not
  verify every command, flag, constant, or citation in this runbook.
- Not run: any CLI against staging/production, any database migration,
  the ignored DB-backed tests (require `TWO_TEST_DATABASE_URL`; CI runs
  them explicitly). The `--dry-run` recipes in §3 are documented from
  source with staging names only, per the test plan; execution belongs
  to Operator cards.
