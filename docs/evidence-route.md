# Expected/processed event evidence route (B2 soak)

The B2 soak ([staging-soak.md](staging-soak.md)) needs **zero missed gateway
events** for join / voice / message. Counters (`two_bot_gateway_*`, see
[metrics.md](metrics.md)) and funnel milestones cannot prove that on their own:
the funnel keeps only the first three messages per member, and checkpoint
sequences include unrelated dispatches. This route pairs an independently
witnessed **expected** action list against the bot's **durably committed**
funnel rows and reports every expected action as matched, excluded, failed or
a gap. ([TOG-11019](/TOG/issues/TOG-11019).)

`crates/core/src/evidence.rs` holds the offline seam; this page is the
collection procedure that uses it.

## What the seam does

- `EvidenceLedger` takes expected actions keyed by **opaque aliases** (`j1`,
  `m1`, `v1`, …), plus store receipts (idempotency key, event type, receipt
  time, inserted/duplicate).
- `reconcile()` pairs each expected action with an exact idempotency-key hint
  first, then the nearest unclaimed same-family receipt within
  `MATCH_WINDOW_MS` (60 s). Each receipt is claimed at most once. Receipts
  that match nothing are counted as collateral and never attributed.
- `export()` writes the sanitized QA packet: schema version, revision, caps,
  truncation flags, per-alias family / timestamps / disposition / reason, and
  counts. Idempotency keys, guild, member, channel and message IDs, sources
  and contents never enter the packet; tests assert this.
- `ReceiptingStore` wraps any `FunnelStore` and captures per-write receipts
  (`inserted: true` = committed, `false` = idempotent redelivery). It is used
  by the offline fixture test; it is **not** wired into the running bot.

Dispositions: `committed`, `duplicate` (redelivery only), `excluded` / `failed`
(declared by the procedure with a reason code such as `bot_authored`,
`ladder_full`, `fixture_step_failed`; never inferred), and `unknown` (expected
but not observed, which is a soak gap until proven otherwise).

## Bounds

| Bound | Value |
|---|---|
| Expected actions per window | 20 (`MAX_EXPECTED_ACTIONS`) |
| Receipts per window | 60 (`MAX_RECEIPTS`) |
| Window length | 15 minutes |
| Packet size | ~10 KiB sanitized JSON |
| Retention | the packet only, attached to the soak card; raw rows are not kept |

Overflow sets `truncated.*_overflow` and stops recording. A truncated packet
reads as UNKNOWN coverage, never as zero loss.

## Live collection (staging only)

Prerequisites: staging verified healthy ([TOG-11131](/TOG/issues/TOG-11131)),
the deployed revision is known (`/readyz` 200 plus the deploy run's SHA), and the
existing authorized fixture identity acts in **TWO Staging**
(`1545644954272137297`). Production is out of scope. Never retry a denied
channel or substitute credentials.

1. **Expected (QA).** Within one 15-minute window, the fixture identity
   performs a finite script (for example one join, three messages in a
   human channel, one voice join/leave) and QA records each step as
   `alias, family, UTC` in its ledger. Steps the bot is meant to ignore
   (bot or webhook messages, a fourth message after the ladder is full) are
   declared `excluded` with a reason code.
2. **Processed (staging DB read, CI route).** Dispatch the manual
   `staging-events-read` workflow (setup and rollback below) from `main`:

   ```sh
   gh workflow run staging-events-read.yml --ref main \
     -f window_start=2026-10-07T14:00:00Z -f window_end=2026-10-07T14:15:00Z
   ```

   QA (or whoever dispatches) passes only the UTC window. The window must
   already have ended, `end > start`, and be at most 15 minutes. The fixture
   member is bound in the environment, never an input, so the public run
   page never shows it and a dispatch cannot aim the read at another member.
   The job connects as the read-only role `two_bot_events_ro` and runs one
   query over the bot's staging `two_bot` database, limited to that member,
   the pinned TWO Staging guild and the window (`recorded_at` is compared as
   a timestamp, so the same text works whether the column is the legacy ISO
   text or `timestamptz`):

   ```sql
   SELECT event_type,
          to_char(recorded_at::timestamptz AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
   FROM events
   WHERE guild_id = '1545644954272137297'
     AND member_id = :'member'
     AND recorded_at::timestamptz BETWEEN :'window_start'::timestamptz AND :'window_end'::timestamptz
   ORDER BY id
   LIMIT 61;
   ```

   The extra row only detects overflow. The run uploads
   `staging-events-read-<run_id>.json` (14 days, `gh run download <run_id>`):

   ```json
   {"schema_version": 1,
    "window": {"start": "2026-10-07T14:00:00.000Z", "end": "2026-10-07T14:15:00.000Z"},
    "row_count": 3, "truncated": false,
    "rows": [{"ordinal": 1, "event_type": "member_join", "recorded_at": "2026-10-07T14:01:00.000Z"}]}
   ```

   `rows` holds at most 60 rows in table order; `truncated: true` means more
   than 60 matched, which reads as UNKNOWN coverage, never zero loss. The
   idempotency key (it embeds `guild:member`), member, guild, source and
   metadata are in no artifact, log or step summary: the repo is public. Each
   row is a committed receipt (`inserted: true`, `received_at =
   recorded_at`) with an opaque ordinal key; QA reconciles by family and time,
   as `reconcile()` already does after its exact-key-hint pass, and exact key
   hints are not available on this route. The DB cannot show redeliveries, so
   `duplicate` stays a fixture-only disposition. A refusal (bad window, a
   binding missing or aimed at another role, database or a production-like
   host, the live guild) fails the job before any connection; a database error
   prints one fixed phrase, never the client's message.
3. **Reconcile.** Feed both lists to `EvidenceLedger` with the deployed
   revision and attach `export()` to the soak card, stamped as
   `evidence-soak_expected_committed-{window}.json` (see
   `evidence_packet_filename` and the rule-id spelling in
   [metrics.md](metrics.md)). `gaps > 0` or any
   overflow flag fails that window. File a card; do not restart anything
   only to fill a table.

Offline fixture runs (`cargo test -p two-bot-core --lib evidence`) prove the
seam, not staging coverage. Only a packet from step 3 is live evidence.

### `staging-events-read` setup and rollback

The workflow (`.github/workflows/staging-events-read.yml`) runs only from
`main`, in the `staging-events-read` GitHub environment (deployment branches:
`main` only; no reviewer, because the role is read-only, column-limited and the
output is row-capped). It has its own non-cancelling concurrency group and a
5-minute timeout, and it takes `psql` from the hosted runner.

Setup is an Operator step and creates nothing from the workflow itself:

1. On staging `two_bot`, create the login role `two_bot_events_ro`
   (`default_transaction_read_only = on`, `statement_timeout = '10s'`,
   connection limit 2) with column-level `SELECT (id, event_type, member_id,
   guild_id, recorded_at, idempotency_key)` on `public.events` and nothing else.
2. Create the environment with a `main`-only deployment-branch policy and two
   environment **secrets**: `TWO_BOT_STAGING_EVENTS_RO_DATABASE_URL` (direct host,
   `sslmode=require`; the script accepts only `sslmode`, `channel_binding` and
   `connect_timeout` on the URL) and `STAGING_FIXTURE_MEMBER_ID` (digits, 15-22).
   The member is a secret, not a variable, because a step's `env` block prints
   variable values on the public run page and masks secrets.

The script checks the login before connecting (role `two_bot_events_ro`,
database `two_bot`, not production-like) and again from the server
(`current_user`, `current_database()`); every statement is a `READ ONLY`
transaction with a 10 s timeout.

Rollback: delete the workflow file. The Operator drops the role (`REVOKE` all,
`DROP OWNED BY two_bot_events_ro; DROP ROLE two_bot_events_ro;`) and deletes the
environment and its secrets. Nothing else changes.

## Reconnect and RSS

- **Reconnect gap:** on the next deployment that is justified on its own,
  record deploy finish to first `/readyz` 200 (see
  [staging-soak.md](staging-soak.md)), plus the
  `two_bot_gateway_reconnects_total` / `two_bot_gateway_resumes_total`
  deltas from `/metrics`. Until then it is NOT MEASURED.
- **RSS:** `crates/core/examples/metrics_rss.rs` measures instrumentation
  cost only ([metrics.md](metrics.md#cardinality-and-memory)). Whole-bot
  loaded-guild RSS still comes from B1's placement measurement; it is not
  claimed here.

Intervals without a packet stay UNKNOWN. The September 30 attempt is not the
soak clock; QA records a new start T only after healthy, observable staging.
