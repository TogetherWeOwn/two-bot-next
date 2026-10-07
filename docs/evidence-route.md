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
2. **Processed (staging DB read, operator-held credential).** One read-only
   query over the bot's staging `two_bot` database, limited to the fixture
   member and the window:

   ```sql
   SELECT idempotency_key, event_type, recorded_at
   FROM events
   WHERE guild_id = '1545644954272137297'
     AND member_id = $fixture_member
     AND recorded_at BETWEEN $window_start AND $window_end
   ORDER BY id
   LIMIT 60;
   ```

   Each row is a committed receipt (`inserted: true`, `received_at =
   recorded_at`). The DB cannot show redeliveries, so `duplicate` stays a
   fixture-only disposition. Raw rows stay with the reader and are discarded
   after the export.
3. **Sanitize and reconcile.** Step 2 returns raw keys, not the runner's
   input schema. The authorized reader wraps that same query as `raw_rows`
   and exports its result as a JSON array; the staging connection, fixture
   member and witnessed window remain the ones authorized for step 2.
   Save this query as `fixture-events.sql` (psql variables are quoted as
   values, not interpolated SQL):

   ```sql
   WITH raw_rows AS (
     SELECT id, idempotency_key, event_type, recorded_at
     FROM events
     WHERE guild_id = '1545644954272137297'
       AND member_id = :'fixture_member'
       AND recorded_at BETWEEN :'window_start' AND :'window_end'
     ORDER BY id
     LIMIT 60
   )
   SELECT COALESCE(json_agg(json_build_object(
     'idempotency_key', idempotency_key,
     'event_type', event_type,
     'recorded_at', recorded_at
   ) ORDER BY id), '[]'::json) FROM raw_rows;
   ```

   Pipe directly into the offline sanitizer: **never save or print the raw
   JSON**. Supply the existing authorized read-only staging login to psql
   without placing credentials in argv or the shell history. The sanitizer
   makes no connection itself; it drops keys, assigns ordinal numbers and
   converts timezone-aware receipt times to UTC regardless of the PostgreSQL
   session timezone. Naive times are refused. A full 60-row result sets `truncated: true`
   conservatively: `LIMIT 60` cannot prove that row 61 does not exist.

   ```sh
   set -o pipefail
   psql -X -qAt -v ON_ERROR_STOP=1 \
     -v fixture_member="$FIXTURE_MEMBER" \
     -v window_start="$WINDOW_START" -v window_end="$WINDOW_END" \
     --file fixture-events.sql \
     | python3 scripts/sanitize_evidence_rows.py > staging_rows.json
   ```

   Stop on any nonzero exit; do not retry a refused read or reuse an old
   artifact. No workflow from another PR is required for this path. If the
   reader already supplies `{rows: [{ordinal, event_type, recorded_at}],
   truncated}` with no raw keys or IDs, use that artifact directly instead.

   QA supplies `qa_expected.json` with the independently witnessed kind for
   each action, not a kind inferred from the observed rows:

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
     --expected qa_expected.json --rows staging_rows.json \
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
     --expected qa_expected.json --rows staging_rows.json \
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
   Exit 1 means gaps, declared failures or overflow/truncation (attach the packet and file a
   card; do not restart anything only to fill a table). Exit 2 means
   malformed input or an IO failure. No valid packet is produced on exit 2.
   Both input files require complete UTC RFC3339 timestamps; trailing text
   or extra fractional components are refused. Valid times are canonicalized
   to the parsed instant's precision (at most nine fractional digits) before
   matching, so longer valid fractions cannot become false gaps. Parser
   diagnostics are fixed phrases that never quote rejected field names,
   values, aliases or keys.

Offline fixture runs (`cargo test -p two-bot-core --lib evidence`) prove the
seam, not staging coverage. Only a packet from step 3 is live evidence.

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
