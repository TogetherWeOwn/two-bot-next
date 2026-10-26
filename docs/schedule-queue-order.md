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

## Schedule at a known instant

`/schedule` accepts an optional `at` RFC3339 string with an explicit `Z` UTC
marker or numeric offset. These two copyable examples name the same instant;
choose a future date within 365 days of the command:

```text
/schedule body:Meeting reminder at:2027-01-15T18:30:00+05:30
/schedule body:Meeting reminder at:2027-01-15T13:00:00Z
```

The compiled command definition owns the picker bounds and `/help command:schedule`
examples; the [generated command reference](commands.md#schedule) comes from that
same registry. `at` accepts at most 64 ASCII characters. Fractional seconds are
truncated to milliseconds, then persisted as `YYYY-MM-DDTHH:MM:SS.mmmZ`. The
persisted instant must be strictly future and at most 525600 minutes (365 days)
ahead. It is not rounded to a delay in minutes. Offset-free local times, invalid
dates/offsets, overflowing years and past/due-now instants refuse with guidance
before any schedule read, write or audit. Correct the timestamp; retrying the
same invalid input will not help.

`at` is one-off only. Supply either `at` or `in-minutes`, never both, and do not
combine `at` with `every-minutes`. To recur, keep using `every-minutes:60` or
`in-minutes:5 every-minutes:60`; the existing relative ranges and first-run
semantics are unchanged. Body safety, Manage Guild authority, the guild/feature
fences and the 25-schedule quota still apply.

Explicit-time confirmations and enabled list rows show the canonical UTC instant
and a Discord timestamp rendered in each reader's local timezone. Disabled list
rows keep their canonical timestamp, stable ID and disabled marker without a
pending-delivery hint. Use `/schedule-remove id:<id>` to cancel, and check the
named ID in the cancellation receipt. Delivery runs on the existing 15-second
ticker and may be delayed by backlog, restarts, rate limits or outages. This is
not an exact-second delivery promise. No timezone profile, natural-language
parsing or DST-aware recurrence is implied.

Absolute schedules use the same stored due instant, claim, occurrence nonce and
uncertain-send fence as relative schedules. Restart does not recalculate the
instant. Completed one-offs remain disabled; an uncertain send remains fenced
through backup/restore rather than blindly posting again.

## Correct an unsent one-off's time

Use `/schedule-retime id:<full-id> at:2027-01-15T13:00:00Z` in the schedule's
original channel. Copy the full stable ID from `/schedule-list`. This private
Manage Server operation uses the same feature/guild fences and explicit-offset,
64-character, future-within-365-days time contract as creation. Equivalent
offsets normalize to the same millisecond UTC instant.

Only enabled, never-run one-offs qualify. Recurring, disabled, completed,
previously run, claimed or delivery-uncertain rows refuse with guidance. Unknown,
foreign-guild and other-channel IDs share the same safe miss. Duplicate,
wrong-typed, missing or extra options refuse before any store access or audit.

A saved correction changes only the due time and update attribution, with one
content-free audit in the same transaction. It preserves the ID, body, destination,
creator, creation time, quota occupancy, occurrence nonce and every delivery fence.
A same-instant retry changes nothing, including metadata and audit. A failed audit
rolls the correction back; an unconfirmed store result asks staff to inspect the
list before retrying the same ID/time. The saved receipt shows old/new localized
times, not delivery. No retime invocation sends or edits a public message.

Retime and due-claim admission serialize on the same schedule row: if retime wins,
the old deadline cannot claim; if claiming wins, retime refuses without releasing
its claim or uncertainty. Expiry is not proof of no delivery. Inspect retained
delivery evidence rather than deleting/recreating to bypass a busy or uncertain
row. The existing 15-second ticker may run late; retime makes no exact-second
promise and cannot replay a completed occurrence.

## Preview a proposed post

`/schedule` accepts an optional `preview` boolean. With `preview:true` the
handler runs the same body, mention, interval and absolute-time validation as
create, then answers privately with the proposed body, the invoking channel,
the first due instant in canonical UTC plus a Discord-local timestamp, and the
one-off versus recurring shape — labelled `Preview — not scheduled`. The reply
says outright that a later create revalidates timing and availability; it names
no saved id, links no message and promises no delivery.

A preview is read-only by construction: valid and invalid previews return
before minting a persistent id, reading or writing the store, auditing,
taking quota, or sending anything to the channel. Rendering the body never
executes it — hostile input renders as inert, mention-safe data within the
2,000-unit reply cap. A duplicated or wrong-typed `preview` value refuses with
a private correction, never a write. Omitted or false keeps create behavior,
including its rejection audits, unchanged. Staff, guild, feature and
activation fences apply before any of this runs.

## Queue model

The store keeps `next_run_at` as `TEXT` in the `format_iso_ms` shape
(`YYYY-MM-DDTHH:MM:SS.mmmZ`, always 24 bytes). The tests model the store's SQL
predicates in memory:

| Store call | SQL predicate | Test model |
| --- | --- | --- |
| `list_scheduled` | `ORDER BY next_run_at, id LIMIT 25` (the per-guild quota; `count_scheduled` sizes the overflow line) | sort rows by the ISO string, ties by id, then take the first 25 |
| `list_scheduled_window` | `ORDER BY next_run_at, id LIMIT 25 OFFSET 25` (one bounded hidden window, only for `page` past the first window when the count proves overflow) | skip the first 25 of the sorted rows, then take the next 25 |
| `claim_due` | `enabled AND next_run_at <= now ORDER BY next_run_at, id LIMIT 1`, then park at `lease_until_ms(now)` | `Queue::claim` |
| `complete_run` | recurring rows advance from the run instant; one-shot rows disable | `Queue::complete` |
| `retry_scheduled` | re-queue at `now + clamp_retry_delay_ms(hint)`; preserve any armed uncertainty unless no effect/cleanup is confirmed | `Queue::fail` models delay only, not delivery safety |

Each ticker pass claims at most `TICKER_BATCH_LIMIT` (10) rows.

The supervised ticker exists only while `TWO_AUTOMATIONS=1` **and** the boot
identity permits the `automations` capability (`scheduled_jobs::register` in
`crates/bot`). The ticker posts under the token's identity, so it obeys the same
live-identity fence as `/schedule`: the staging guild/application pair may run
it, the live pair (cleared for `self_roles` only), an unknown or mismatched
pair and an unparseable token may not. With either gate off no job is built, so
no row is claimed, posted or audited; the `/readyz` status map lists
`scheduled_messages` as parked.

## Acceptance

1. **List order.** `/schedule-list` rows come back in ascending
   (`next_run_at`, `id`) order. Lexicographic order of the fixed-width strings
   equals instant order across millisecond carries and day, month, year and
   leap-day rollovers, with tied instants ordered by id.
   `schedule_list_line`/`schedule_list_text` keep that order, and the paged
   list (`schedule_list_pages`/`schedule_list_page` with the optional `page`
   option) preserves it across bounded whole-row pages. The claim order breaks
   ties by `id`, and one tick takes at most the 10 earliest due rows.
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
4. **Bounded retries, not blind replay.** The pure `post_failure_retryable`
   helper models legacy delay policy, not proof of delivery absence. The runtime
   retries only proven no-effect refusals (including local admission blocking
   and 429) or confirmed cleanup. Timeout, transport loss, 5xx, unusable success
   receipts and unconfirmed completion retain the durable fence instead.
   `clamp_retry_delay_ms` bounds delays to 1 s–15 min (default 30 s); reclaim
   parks rows at the 60 s lease. At most ten claims per tick, with due-order
   parking, keep unrelated rows progressing. A definitive permanent refusal
   still advances/disables the occurrence, with a failure audit.
5. **Prefix resolution.** `resolve_scheduled_id` returns `Unique` only for a
   single match. Missing prefixes, case mismatches and literal LIKE
   metacharacters (`%`, `_`) are refused as `Missing`. Shared prefixes are
   refused as `Ambiguous`, including an exact id that has a longer sibling
   (legacy behaviour) and an empty prefix over several rows. The verdict does
   not depend on input order. Both refusals share `no_unique_match_text`.
6. **Bounded listing.** `/schedule-list` reads at most the 25 earliest rows
   per window (the per-guild schedule quota). A guild holding more —
   possible only via legacy rows or direct repairs, since quota admission
   never writes past 25 — renders those 25 plus a final `(truncated, N
   more)` line with N = total minus 25, or `(truncated)` when the overflow
   count itself fails, so a cut is never silent. `page` past the first
   window's packed pages reads one bounded hidden window
   (`list_scheduled_window`, `OFFSET 25 LIMIT 25`), so rows through the
   first 50 stay discoverable on higher pages and every reply stays
   bounded; the hidden window carries its own overflow line when still
   more remain. Hidden pages carry continuing global numbers (never
   restarted), and the last first-window page of a multi-page window names
   the next page (`Continued on page N.`), so no footer reads as the end
   of the list. A failed hidden-window or overflow-count read fails closed
   ("Schedule command failed; try again.") instead of reporting a
   possibly-false out-of-range page. Rows past the cap keep their order
   and keep firing via `claim_due`. Enabled runtime rows include localized due
   times and delivery uncertainty before packing, so enrichment cannot overflow
   a page or silently split an ID.

## Out of scope

These are pure-contract tests. They do not cover the sqlx statements,
`SKIP LOCKED` contention or the occurrence nonce. The store test
`crates/cutover/tests/scheduled_store.rs` covers those against a test
database.
