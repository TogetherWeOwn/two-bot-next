# Scheduled-message due-queue order

`crates/core/tests/schedule_queue_order.rs` is hermetic acceptance for the
scheduled-message ticker queue: [parity](parity.md) §4 (15 s ticker,
`next_run_at` queue) and §8 (legacy `claimDueScheduled` /
`markScheduledRun`). It uses only the public pure API in
`two_bot_core::scheduled`. It needs no `db` feature, database, clock or Discord.

Run it with:

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test schedule_queue_order
```

## Queue model

The store keeps `next_run_at` as `TEXT` in the `format_iso_ms` shape
(`YYYY-MM-DDTHH:MM:SS.mmmZ`, always 24 bytes). The tests model the store's SQL
predicates in memory:

| Store call | SQL predicate | Test model |
| --- | --- | --- |
| `list_scheduled` | `ORDER BY next_run_at` | sort rows by the ISO string |
| `claim_due` | `enabled AND next_run_at <= now ORDER BY next_run_at, id LIMIT 1`, then park at `lease_until_ms(now)` | `Queue::claim` |
| `complete_run` | recurring rows advance from the run instant; one-shot rows disable | `Queue::complete` |
| `retry_scheduled` | re-queue at `now + clamp_retry_delay_ms(hint)` | `Queue::fail` on a retryable status |

Each ticker pass claims at most `TICKER_BATCH_LIMIT` (10) rows.

## Acceptance

1. **List order.** `/schedule-list` rows come back in ascending `next_run_at`
   order. Lexicographic order of the fixed-width strings equals instant
   order across millisecond carries and day, month, year and leap-day
   rollovers. `schedule_list_line`/`schedule_list_text` keep that order. The
   claim order breaks ties by `id`, and one tick takes at most the 10
   earliest due rows.
2. **No catch-up burst.** In the test, an hourly row is five intervals overdue
   when the bot resumes. It posts once. `advance_next_run_ms` sets the next run
   one interval after the run time, not after the missed schedule slot, so no
   tick in the next hour posts again. A one-shot row disables after its run.
3. **Lease parking.** `lease_until_ms` parks a claimed row
   `CLAIM_LEASE_MS` (60 s) ahead, which is longer than `SCHEDULER_TICK_MS`
   (15 s). The test lets the claiming process die before it records the run.
   A restarted process then treats the row as not due on every tick inside
   the lease, while it still drains the other due rows. When the lease
   expires, the row is reclaimed exactly once. The lease saturates at
   `u64::MAX` instead of wrapping.
4. **Bounded retries.** `post_failure_retryable` retries on no response, 429
   and 5xx. Other 4xx statuses are permanent. `clamp_retry_delay_ms` keeps
   every delay between 1 s and 15 min, and uses 30 s when the response has no
   hint. A retry-after of zero still waits the 1 s floor, so a tick never
   spins on the same row. A hostile `u64::MAX` hint is capped at 15 min, so
   the row is retried rather than parked forever. Failing rows at the head do
   not stop later due rows from posting in the same tick. A permanent failure
   advances or disables the row like a run.
5. **Prefix resolution.** `resolve_scheduled_id` returns `Unique` only for a
   single match. Missing prefixes, case mismatches and literal LIKE
   metacharacters (`%`, `_`) are refused as `Missing`. Shared prefixes are
   refused as `Ambiguous`, including an exact id that has a longer sibling
   (legacy behaviour) and an empty prefix over several rows. The verdict does
   not depend on input order. Both refusals share `no_unique_match_text`.

## Out of scope

These are pure-contract tests. They do not cover the sqlx statements,
`SKIP LOCKED` contention or the occurrence nonce. The store test
`crates/cutover/tests/scheduled_store.rs` covers those against a test
database.
