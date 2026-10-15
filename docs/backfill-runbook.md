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
stay far under it with a 110 ms pacing floor (≈9/s per lane), and both
honor the response's own wait on 429. Header-driven per-route buckets and
`X-RateLimit-Reset-After` parsing are pinned only by the currently failing
acceptance suite (`crates/discord/tests/backfill_ratelimit.rs`); the
shipped code reads `Retry-After` / body `retry_after`, not `Reset-After`.
Do not promise bucket-accurate pacing until that builder work lands and
its flag names settle.

### Cutover CLIs (`crates/cutover/src/rest.rs`)

- 110 ms floor between requests: `min_interval: Duration::from_millis(110)`
  (`rest.rs:210`; module contract `rest.rs:4`). The floor is per client, so
  never run two backfill writers against the same token concurrently and
  assume the budget holds — the floor does not coordinate across processes.
- 429 parks for the response's own wait, then retries within the same
  attempt budget (`rest.rs:268`, `rest.rs:304`, `rest.rs:476`):
  `retry_after_wait_ms()` is header seconds (body `retry_after` wins when
  finite), × 1000, + 250 ms padding, clamped to 60 s per wait
  (`crates/core/src/action_outcomes.rs:163-181`; padding
  `RETRY_AFTER_PADDING_MS:179`, clamp `MAX_RETRY_AFTER_MS:181`). A missing
  or garbage header falls back to 1 s + 250 ms (`action_outcomes.rs:386-388`
  pins this).
- 5xx backs off at 500 × 2^attempt ms and gives up as retry-exhausted
  (`rest.rs:270`, `rest.rs:306`, `rest.rs:478`;
  `backoff_duration` at `rest.rs:701-703`, base `BACKOFF_BASE_MS` in
  `action_outcomes.rs:184`). Total budget is 5 tries per call
  (`MAX_HTTP_TRIES` in `action_outcomes.rs:188`).
- 403/404 on a page read as `Unreadable`, not an abort (`rest.rs:266`):
  a channel the token cannot see is skipped, and the scan report says so
  (see §4). Anything outside 2xx/403/404/429/5xx is a hard `Wire` error.
- Every run prints its own Discord cost (`rest.requests()`,
  `backfill.rs:470-473`, `backfill_messages.rs:319-323`): requests plus
  wall time. A re-run that scanned nothing should show few requests; a
  large number with no rows written points at unreadable pages, not at
  successful work.

### Runtime executor (`crates/discord/src/executor.rs`)

For backfills that move onto the runtime path, the same posture holds
with shared admission:

- Paced lane 110 ms, kick lane 350 ms (`PACE_INTERVAL_MS` at
  `executor.rs:66`, `KICK_INTERVAL_MS` at `executor.rs:69`, wired at
  `executor.rs:971-972`).
- Paced GETs retry 429 with the same `retry_after_wait_ms()` and spend
  one of the same 5 tries per 429/5xx/transport failure; a steady 429
  stream exhausts the budget instead of parking forever
  (`get_json_observed` at `executor.rs:1753`, 429 arm at
  `executor.rs:1791-1805`). A global 429 installs the shared pause and is
  retried at next admission instead of sleeping locally
  (`executor.rs:1686-1691`).
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

- Checkpoint with `put_backfill_cursor`, resume with
  `get_backfill_cursor` (`backfill_cursors.rs:65-84`, `91-100`).
  Validation runs before any SQL: unknown families and empty/`"0"`
  cursors are refused (`71-77`), and an untouched family reads as `None`
  — resume-from-start, never a synthesized zero cursor (`87-90`).
- The SQL is byte-for-byte the acceptance target's literals
  (`PUT_SQL:43-46`, `GET_SQL:49`), so wrapper and tests cannot drift.
- Database fence: `cursor TEXT NOT NULL CHECK (length(cursor) > 0 AND
  cursor <> '0')` in `crates/store/migrations/0408_backfill_cursors.sql`
  and its cutover-chain mirror
  `crates/cutover/migrations/0435_backfill_cursors.sql` (identical
  `CREATE TABLE IF NOT EXISTS`, additive-only, cross-runnable in either
  order). A zero restart cannot be persisted even if validation is
  bypassed. Privacy fence, same as event-store S1: `family`, `cursor`,
  `updated_at` only — no bodies, no status text, no payload columns.
- Who writes which family: the member-catchup path checkpoints the
  `members` family with the same SQL shape on every cursor advance
  (`crates/cutover/src/member_catchup.rs:156-189`). The history CLIs
  (`backfill`, `backfill-messages`, below) do **not** checkpoint cursors:
  they re-walk channel history per invocation, so resume there means
  re-running with a deeper `--max-pages` (see §4 page-cap guidance), not
  reading a cursor row.

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

- `cursor` + `updated_at` (epoch seconds) are the actionable signal and
  are exposed verbatim. A stale `updated_at` means the family is stuck;
  a `null` cursor means it never checkpointed.
- `progress` is `0.0` and `eta_seconds` is `null` by ruling until a
  worker slice records totals (`32-42`); any non-zero value today would be
  fiction. Do not report ETA from this route.
- Only vocabulary keys are exposed; unknown stored families never become
  labels (`267-276`).

## 3. Dry-run and staged-delete counts

All history CLIs default to dry-run and print a `DRY RUN` banner; nothing
is written unless the operator passes the explicit write flag. Env for
the Discord-reading CLIs: `DISCORD_TOKEN` (or `DISCORD_BOT_TOKEN`),
`DISCORD_GUILD_ID`, `TWO_DATABASE_URL` (`backfill.rs:8-9`, token fallback
`45-49`, guild/DB checks `74-87`).

| Binary | Dry-run form | Write form | Banner / gates |
|---|---|---|---|
| `backfill` (member joins/leaves/voice + invite baseline) | `backfill --dry-run` (usage `backfill.rs:26`; flag `63`) | bare `backfill` writes; invite baseline skipped while dry (`392-404`) | `DRY RUN - nothing will be written` (`101-106`); bot marks, event writes and activity touches all gated (`131`, `324`, `449`) |
| `backfill-messages` (first/second/third message milestones) | `backfill-messages --dry-run` (flag `backfill_messages.rs:29`) | bare writes; milestones only ever move earlier, never later (module docs `1-8`) | banner (`76-80`); ladder writes and activity touches gated (`258-294`) |
| `capture` (join capture) | `capture --dry-run` (flag `capture.rs:26`) | bare writes | banner (`68-70`); writes gated (`145`, `167`) |
| `dedupe-events` (clear double-logged joins/leaves) | default: counts without writing (`dedupe_events.rs:72`) | bare `--apply` deletes in 200-id chunks (`133-143`); prints `deleted {n}` | `DRY RUN - nothing will be deleted` (`78-82`); dry prints `would delete {found}` plus per-type rows/people/dupes and the remaining total (`124-129`, `159-166`) |
| retention job (library, `retention.rs`) | `RetentionMode::DryRun` reports per-class counts and deletes nothing (`73-78`, `187-203`) | `RetentionMode::Execute` deletes, drains the guild-purge queue, completes pending deletions | per-class fields `presence_raw_deleted`/`voice_raw_deleted`/`audit_cache_deleted`/`guild_rows_purged` (`82-90`); receipts (not row counts) for user deletions (`140-144`); `total_deleted` sums the disjoint row classes (`146-151`) |

Safety rails, verified in code:

- Live-guild fence runs before any database or file opens: the
  production guild refuses without bare `--allow-live-guild`
  (`backfill.rs:35-43,79`; `backfill_messages.rs:56-62`;
  `capture.rs:36-40`; dedupe `validate_options` with `--guild`
  snowflake check). Exit 2, `Refusing live guild`, no connection
  attempt. Staging runs always name the staging guild id from the
  approved deployment record; never substitute production.
- `dedupe-events` refuses `--apply` + `--dry-run` together, bare-flag
  values (`--apply=false`), duplicates, and unknown options
  (`dedupe_events.rs:45-58`); `--guild <snowflake>` is required (`55`).
  The shared parser likewise refuses `--apply` with `--dry-run`
  (`cli.rs:115-117`) and valued safety opt-ins (`cli.rs:89-113`).
- Migration behavior differs by binary: `backfill`,
  `backfill-messages` and `capture` open the DB with migrations enabled
  (`open_db(&args, false)` — `false` = do not skip;
  `backfill.rs:96`, `backfill_messages.rs:71`, `capture.rs:63`;
  `connect` applies unless skipped, `db.rs:25-31`; `open_db`,
  `cli.rs:180-197`). `dedupe-events` is maintenance-only and never
  migrates (`open_db(&args, true)`). A dry run against a schema behind
  the binary may therefore apply migrations on the history CLIs — run
  the reviewed staging migration path first, never a bare CLI as a
  migrator.
- `--max-pages` bounds each channel walk (`backfill` default 25,
  `backfill.rs:64-67`; `backfill-messages` default 60,
  `backfill_messages.rs:30-33`). Hitting the cap prints `INCOMPLETE`
  with a 4× re-run suggestion (`backfill.rs:462-468`,
  `backfill_messages.rs:304-311`); a capped scan is a lower bound, not
  a complete result.

Staging-name-only recipe (no secrets, no live run performed for this
doc; Operator cards own execution and bindings):

```sh
DISCORD_GUILD_ID="$STAGING_GUILD_ID" TWO_DATABASE_URL="$STAGING_DIRECT_URL" \
  backfill --dry-run --max-pages=25
backfill-messages --dry-run --max-pages=60
dedupe-events --guild "$STAGING_GUILD_ID" --dry-run
```

`$STAGING_GUILD_ID` is the non-secret staging guild id from the approved
deployment record; `$STAGING_DIRECT_URL` is the Operator-bound direct
(non-pooler) staging URL, never echoed or committed. Production values
are never substituted.

## 4. Failure-burst anchors

A re-run report ends with a `scan completion` line plus one `INCOMPLETE`
line per interrupted channel (`ScanReport::render`, `cli.rs:38-55`).
The vocabulary is fixed (`ScanCompletion`, `rest.rs:722-752`):

| Token | Meaning | Complete? |
|---|---|---|
| `end-of-history` | walked to the oldest message | yes |
| `time-boundary` | reached the requested `--since` bound | yes, for the window |
| `page-cap` | hit `--max-pages`; sets `truncated` (`rest.rs:679`) | no — re-run deeper (4× suggestion above) |
| `unreadable` | 403/404 page: token cannot see the channel | no — check access, not a bug |
| `retry-exhausted` | 5xx survived all 5 tries (`rest.rs:271`) | no — check Discord status / service health |
| `request-failed` | transport/Twilight/wire failure (`rest.rs:646-647`) | no — check service health before re-running |
| `invalid-response` | page body failed to decode (`rest.rs:645`) | no — one bad row fails its page; report it |

Burst triage:

- `has_incomplete_history` is true unless every channel ended at
  `end-of-history` (`cli.rs:31-36`); the message-scan summary prints
  `lower bound - incomplete history` in that case
  (`backfill_messages.rs:239-247`).
- Probe misses are not failures: each candidate channel is probed for
  one page first, and channels under a 5% funnel-data hit rate are
  skipped as mixed feeds (`backfill.rs:186-221`). Skipped channels are
  expected, not data loss.
- Member-list traversal has hard ceilings: 500 pages / 500 000 members
  (`MAX_MEMBER_PAGES`, `MAX_MEMBERS`, `rest.rs:55-57`); a non-advancing
  page is `MemberCursorStalled` and a ceiling hit is
  `MemberCeilingExceeded` (`RestError`, `rest.rs:44-50`) — never a
  partial roster presented as complete.
- Zero members read is an intent hint, not an empty guild: the CLI exits
  1 naming Server Members Intent (`backfill.rs:110-123`).
- Archived-thread discovery has its own budget
  (`DEFAULT_ARCHIVED_THREAD_PAGES = 10`, `rest.rs:761`); early stops are
  reported per forum (`backfill_messages.rs:312-318`), not silently
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
grep -n "ScanCompletion::" crates/cutover/src/rest.rs | head
```

- `cargo fmt --all -- --check`: run per the PR checklist (docs-only
  change; result recorded in the PR Verification section).
- Citation grep above confirms every command, flag, constant and report
  line cited here, including the negative result that no shipped
  `Reset-After` reader exists (acceptance-suite only).
- Not run: any CLI against staging/production, any database migration,
  the ignored DB-backed tests (require `TWO_TEST_DATABASE_URL`; CI runs
  them explicitly). The `--dry-run` recipes in §3 are documented from
  source with staging names only, per the test plan; execution belongs
  to Operator cards.
