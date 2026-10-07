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
3. **Reconcile.** Run the runnable reconciler over both files with the
   deployed revision and the window start (QA is an agent and cannot call
   the `EvidenceLedger` library directly):

   ```sh
   cargo build -p two-bot-core --example evidence_reconcile --locked
   ./target/debug/examples/evidence_reconcile \
     --expected qa_expected.json --rows staging_rows.json \
     --revision <deploy-sha> --window 2026-10-09T20-11-06Z
   ```

   (On the persistent controller, route the build through the cache
   wrapper per [build-cache.md](build-cache.md) and run the example
   binary it produces; the wrapper does not take `run`.)

   `--expected` is the QA JSON array
   (`[{alias, family, at, disposition?, reason?}]`, family one of
   `join` / `voice` / `message`, `disposition` only ever `excluded` or
   `failed` with a reason code); `--rows` is the sanitized rows artifact
   (`{rows: [{ordinal, event_type, recorded_at}], truncated}`, no
   idempotency keys and no raw IDs — extra per-row fields are refused).
   The runner assigns opaque per-row keys (`r<ordinal>`), writes `export()`
   to `evidence-soak_expected_committed-{window}.json` (see
   `evidence_packet_filename` and the rule-id spelling in
   [metrics.md](metrics.md)), and prints one summary line with the
   expected / matched / gaps counts. Exit 0 means the window is clean;
   exit 1 means `gaps > 0` or an overflow/truncation flag is set (attach
   the packet to the soak card and file a card; do not restart anything
   only to fill a table); exit 2 means malformed input or an IO failure,
   in which case nothing is written.

Offline fixture runs (`cargo test -p two-bot-core --lib evidence`) prove the
seam, not staging coverage. Only a packet from step 3 is live evidence.

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
