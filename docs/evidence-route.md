# Expected/processed event evidence route (B2 soak)

The B2 soak ([staging-soak.md](staging-soak.md)) requires **zero missed
events** across joins, voice, messages and slash. Counters
(`two_bot_gateway_*`, see [metrics.md](metrics.md)) and funnel milestones
cannot prove that on their own: the funnel keeps only the first three messages
per member, and checkpoint sequences include unrelated dispatches. The source
route described here covers join / voice / message only; it does not cover
slash or establish current live coverage.

`crates/core/src/evidence.rs` holds an offline seam. The documented live
collection example is not current runtime evidence and does not replace the
existing reviewed read workflow or protected binding; their current application
remains **NOT VERIFIED**. Use those existing paths only after their independent
verification. This page does not authorize a new collector, receipt path,
origin, overflow path or live fault/dispatch method. The current deployed build,
health, bindings, ACL state and live outcomes remain **NOT VERIFIED**.

The approved policy does not define an operational ACTIVE hour, the source or
record shape for the 120 expected one-minute samples in hours 3–4, or any
one-minute sample cadence for hours 1–2. The route documented here is an offline
seam, is not wired into the running bot, and covers join / voice / message only;
it does not establish slash coverage or provide a verified live source for the
120 samples. Its 15-minute packet bounds are not a substitute for a defined,
reviewed live sampling route. No currently verified route satisfies these
requirements; keep B2 **NOT VERIFIED** and `T` unset. The approved policy names
no owner for defining or authorizing a live sample method. The separate existing
technical-policy question remains pending with the CTO; this page does not
assign a new owner or approve a new evidence method.

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

## Live collection (staging only; B2 use is blocked)

**Do not execute this procedure for B2 under the current policy.** The deployed
build, health, bindings, migration/ACL state and live outcomes are **NOT
VERIFIED**; this three-family source omits slash and does not define or supply
the required 120 one-minute samples. The steps below are a conditional
procedure, not execution authorization. Do not perform fixture actions, a
staging SQL read or packet export for B2 until the existing reviewed live routes
and applicable sample method are independently verified and the separately
required authorization is recorded. No route, method or authorization is
created here; keep `T` unset.

Prerequisites for any separately authorized use: staging verified healthy,
the deployed revision is known (`/readyz` 200 plus the deploy run's SHA), and
the existing authorized fixture identity acts in **TWO Staging**
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
   recorded_at`) with an opaque ordinal key; the reconciler supplies no
   idempotency-key hint for these rows and matches the witnessed event kind
   and time. The DB cannot show redeliveries, so
   `duplicate` stays a fixture-only disposition. A refusal (bad window, a
   binding missing or aimed at another role, database, or host instead of the
   independently pinned staging endpoint, the live guild, or failed TLS
   verification) fails the job before any rows are read; a database error
   prints one fixed phrase, never the client's message. Only the known funnel
   `event_type` vocabulary is exported; an unknown event type fails closed
   before the artifact is written.
3. **Reconcile.** The workflow artifact is already sanitized; pass
   it directly as `--rows` to `evidence_reconcile`. It contains `rows` and
   `truncated` alongside its schema version, witnessed window and row count.
   Do not perform a second database read or expose raw keys. Download the
   `staging-events-read-<run_id>.json` artifact and set `EVENTS_READ_JSON` to
   its path. QA supplies
   `qa_expected.json` with the independently witnessed kind for each action,
   not a kind inferred from the observed rows:

   ```json
   [
     {"alias":"j1","family":"join","event_type":"member_join","at":"2026-10-09T20:00:00.000Z"},
     {"alias":"m1","family":"message","event_type":"first_message","at":"2026-10-09T20:01:00.000Z"},
     {"alias":"v1","family":"voice","event_type":"voice_session_start","at":"2026-10-09T20:02:00.000Z"},
     {"alias":"v2","family":"voice","event_type":"voice_session_end","at":"2026-10-09T20:05:00.000Z"},
     {"alias":"x1","family":"message","at":"2026-10-09T20:06:00.000Z","disposition":"excluded","reason":"bot_authored"}
   ]
   ```

   `family` is `join` / `voice` / `message`; `event_type` must belong to that
   family and is required unless the action is declared `excluded` or
   `failed` with a reason code. Only that exact event kind can match within
   60 s, even when a sibling-family event is closer. A missed join cannot
   match `member_leave` or `gate_cleared`; voice boundaries and message
   ladder rungs cannot substitute for one another. The kind is held only
   in memory and is not exported in the packet.

   Build and run with the deployed revision and witnessed window start:

   ```sh
   cargo build -p two-bot-core --example evidence_reconcile --locked
   ./target/debug/examples/evidence_reconcile \
     --expected qa_expected.json --rows "$EVENTS_READ_JSON" \
     --revision "$DEPLOY_SHA" --window 2026-10-09T20-00-00Z
   ```

   On the persistent controller, build through the bounded wrapper and get
   the actual executable path from Cargo's artifact message:

   ```sh
   set -o pipefail
   BIN=$(python3 scripts/cargo_cache.py run -- build -p two-bot-core \
     --example evidence_reconcile --message-format=json \
     | jq -r 'select(.reason == "compiler-artifact" and .target.name == "evidence_reconcile" and .executable != null) | .executable')
   test -n "$BIN" && "$BIN" \
     --expected qa_expected.json --rows "$EVENTS_READ_JSON" \
     --revision "$DEPLOY_SHA" --window 2026-10-09T20-00-00Z
   ```

   The wrapper does not take `run`; do not create a worktree-local target
   ([build-cache.md](build-cache.md)). Artifact `executable` is documented
   in [Cargo's JSON message contract](https://doc.rust-lang.org/cargo/reference/external-tools.html#json-messages).

   Extra per-row fields are refused; object envelopes require both `rows`
   and `truncated`. The runner assigns opaque per-row keys (`r<ordinal>`),
   writes `export()` to `evidence-soak_expected_committed-{window}.json`
   (see `evidence_packet_filename` and [metrics.md](metrics.md)), and prints
   expected / matched / gaps counts. Exit 0 means clean reconciliation of
   the witnessed actions, **not coverage of arbitrary gateway dispatches**.
   Exit 1 means gaps, declared failures or overflow/truncation; attach the
   packet and file a card, but do not restart anything only to fill a table.
   Exit 2 means malformed input or an IO failure. No valid packet is produced
   on exit 2. Both input files require complete UTC RFC3339 timestamps;
   trailing text or extra fractional components are refused. Valid times are
   canonicalized to the parsed instant's precision (at most nine fractional
   digits) before matching, so longer valid fractions cannot become false
   gaps. Parser diagnostics are fixed phrases that never quote rejected field
   names, values, aliases or keys.

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
   guild_id, recorded_at)` on `public.events` and nothing else. Do not grant
   `SELECT` on `idempotency_key`; this route does not need it and it can contain
   identity-bearing data.
2. Create the environment with a `main`-only deployment-branch policy and three
   environment **secrets**: `TWO_BOT_STAGING_EVENTS_RO_DATABASE_URL` (direct
   endpoint URL; port omitted or 5432),
   `STAGING_EVENTS_READ_EXPECTED_HOST` (the independently
   verified staging endpoint hostname), and `STAGING_FIXTURE_MEMBER_ID` (digits,
   15-22). Verify the host from the staging provider's control plane; do not
   derive the pin from the URL secret. The script requires an exact URL-host
   match and forces libpq `sslmode=verify-full` against the runner's system CA
   bundle, regardless of a weaker accepted `sslmode` parameter in the URL. The
   member and host are secrets, not variables, because a step's `env` block
   prints variable values on the public run page and masks secrets.

The script refuses before connecting unless the login role is
`two_bot_events_ro`, the database is `two_bot`, the URL host matches the
independent staging-host pin, and its certificate validates for that hostname.
After connecting it also checks `current_user` and `current_database()` from the
server; every statement is a `READ ONLY` transaction with a 10 s timeout.

Rollback: delete the workflow file. The Operator drops the role (`REVOKE` all,
`DROP OWNED BY two_bot_events_ro; DROP ROLE two_bot_events_ro;`) and deletes the
environment and its three secrets. Nothing else changes.

## Reconnect and memory observations

- **Workflow interval:** deploy finish to first `/readyz` 200, plus the
  `two_bot_gateway_reconnects_total` / `two_bot_gateway_resumes_total`
  deltas from `/metrics`, is a deployment/reconnect observation only. It is
  **not** outage-start-to-verified-recovery and cannot prove B2's under-60-second
  recovery requirement. Until observed, it is NOT MEASURED.
- **Memory:** `crates/core/examples/metrics_rss.rs` measures instrumentation
  cost only ([metrics.md](metrics.md#cardinality-and-memory)). Whole-bot
  loaded-guild RSS in B1 is historical evidence, not a numeric B2 criterion;
  flat memory remains required and numeric RSS/error definitions are unaccepted.

Intervals without a packet stay UNKNOWN. The September 30 attempt remains a
failed historical attempt, not the soak clock. `T` stays unset until all
qualifying preconditions and the reviewed live evidence routes are independently
verified.
