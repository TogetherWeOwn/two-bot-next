# Membership chronology store contract

Source pin: [TogetherWeOwn/two-bot #387](https://github.com/TogetherWeOwn/two-bot/pull/387),
commit [`bffccf3e3a9f56a3da37de67c6f272ac10ecb3b3`](https://github.com/TogetherWeOwn/two-bot/tree/bffccf3e3a9f56a3da37de67c6f272ac10ecb3b3).
This slice implements the in-memory half, not sqlx wiring, backfill, migrations,
report CLIs, deployment, or a live Discord journey.

## Rules and read boundary

- Join attribution uses the latest **actual occurrence**, preserving its source.
- Presence uses valid `metadata.membershipObservedAt`, otherwise occurrence.
  A leave wins equal-observation ties. `left_at` remains its actual occurrence,
  even if earlier than the latest actual join.
- Reconfirmation advances observation monotonically but preserves the stored
  identity, occurrence, source, inviter and unrelated metadata. It does not
  count as a genuine rejoin or clear inactivity after that actual join.
  Explicit valid hints may precede occurrence: duplicate maxima compare only
  prior metadata hints, not the occurrence fallback. A duplicate without an
  explicit hint never updates stored metadata from its incoming payload.
  The page-start/join maximum is capture policy, not a store-wide floor.
- An actual join at/after the inactivity flag clears it, even if a leave wins
  presence. First-message and other milestones remain immutable.
- Comparisons normalize UTC to six fractional digits; database text/session
  offsets retain microseconds. No `parse_iso_millis` is used for projection.
- The in-memory event identity is the existing idempotency key rather than a
  synthetic SQL event UUID. Equal-type/equal-time ties use stable row identity,
  never insertion order. Original rows of the same identity must agree on
  occurrence/source; conflicting first-insert payloads are not an order-free
  stream (the legacy store also preserves the first accepted payload).
- `MemStore` serializes inserts and metadata max updates under one mutex and
  derives membership from those stored rows under the same lock. No mock SQL,
  optimistic CAS loop or fictional collision counter is introduced.

## Reuse with the sqlx store

The common entry point is
`crates/core/tests/support/membership_contract.rs::run(make_store)`.
It requires `S: MembershipStore`; that read/observation extension itself requires
`FunnelStore`. A future sqlx integration test can import it with:

```rust,ignore
#[path = "support/membership_contract.rs"]
mod contract;

#[test]
fn sqlx_membership_contract() {
    contract::run(|| make_isolated_sqlx_funnel_adapter());
}
```

Each factory call must provide an **empty isolated namespace/store**. The suite
makes many independent stores and uses scoped OS threads for simultaneous
writers; the future sync adapter must own a runtime compatible with this seam.
It must read actual persisted event/projection data, preserve microseconds
(e.g. SQL timestamp comparisons or text, not milliseconds), and expose duplicate
observation updates through `record_observed`. Never substitute `MemStore`
readbacks for the durable implementation under test.

The sqlx follow-up must additionally exercise its own member/event-row locking,
metadata compare-and-swap fallback and non-UTC **database session**. The generic
suite tests their observable guarantees, not a particular lock/CAS algorithm.
The PostgreSQL offset scenario here injects session-shaped text into `MemStore`;
it is not evidence of having run `SET TIME ZONE` against a database.

The clock is injectable: `MembershipClock::next_at(wall_millis)` follows
`max(wall_millis * 1000, previous + 1)`. Adapters must capture the observation
**before** asynchronous invite/REST work and carry that stamp into the store;
`member_observation(page_start, joined_at)` selects their maximum. This slice
exposes the seams and contract, not production gateway/sqlx/capture wiring.

`ActionExecutor::get_json_observed` adds request-start evidence on the existing
paced transport. It stamps after pacing, before sending, and repeats stamping
on retries. `get_json` still returns only data/None. The REST tests use real
loopback HTTP, delayed headers/body, 503 retry and 403 refusal. Full observed
roster pagination/capture consumers remain outside this store slice.

## Complete legacy scenario map

Paths below are relative to the pinned **legacy** repository. Every scenario
also carries a one-line source reference beside its Rust assertions. Expanded
legacy count: **28** (7 chronology, 8 precision, 9 replay, 1 clock, 3 REST).
The Rust harness groups parameterized scenarios rather than copying test names.

| Legacy file and line | Scenario(s) | Rust contract group |
|---|---|---|
| `test/unit.membership-chronology.test.ts:45` | 6 / 24 arrival orders; every prefix, duplicate replay, history | `permutations_contract` |
| `test/unit.membership-chronology.test.ts:79` | Historical join keeps latest-spell inactivity | `inactivity` |
| `test/unit.membership-chronology.test.ts:89` | Rejoin clears leave/flag, keeps first message | `inactivity` |
| `test/unit.membership-chronology.test.ts:105` | Prior-spell flag converges in both leave/rejoin orders | `inactivity` |
| `test/unit.membership-chronology.test.ts:124` | Known join reconfirmation keeps later flag and original identity | `inactivity` |
| `test/unit.membership-chronology.test.ts:142` | Concurrent 3 / 4 event journeys | `concurrent` |
| `test/unit.membership-precision.test.ts:55` | Microsecond and millisecond 6 / 24 permutations; exact history | `permutations_contract` |
| `test/unit.membership-precision.test.ts:81` | Concurrent microsecond attribution/presence | `concurrent` |
| `test/unit.membership-precision.test.ts:92` | Microsecond flag before/after actual join | `inactivity` |
| `test/unit.membership-precision.test.ts:109` | Non-UTC session-shaped timestamps, reverse arrival | `precision` (text-boundary equivalent) |
| `test/unit.membership-precision.test.ts:122` | Observation order independent of actual attribution | `precision` |
| `test/unit.membership-replay.test.ts:36` | Leave-first / join-first completion; stale backfill | `replay` |
| `test/unit.membership-replay.test.ts:66` | Same-tick join / leave / duplicate join | `replay` |
| `test/unit.membership-replay.test.ts:81` | Reconfirmation vs genuine rejoin, original inviter/identity | `replay` |
| `test/unit.membership-replay.test.ts:121` | Concurrent writes, new leave, stale duplicates | `concurrent` |
| `test/unit.membership-replay.test.ts:160` | Delayed older duplicate cannot overwrite newer maximum | `concurrent` |
| `test/unit.membership-replay.test.ts:217` | Five stale competing duplicates converge on newest stamp | `concurrent` (behavioral equivalent; SQL CAS-count test deferred) |
| `test/unit.membership-replay.test.ts:282` | Delayed leave / delayed invite-add completion | `dispatch_and_rest` (captured dispatch stamps) |
| `test/unit.membership-clock.test.ts:5` | Same tick, advancing wall time, backward wall correction | `clock` |
| `test/unit.membership-rest-observation.test.ts:9` | Request start survives delayed headers/body | `discord/tests/membership_observation.rs` plus store boundary fixture |
| `test/unit.membership-rest-observation.test.ts:31` | Successful retry's start, exactly two requests | `discord/tests/membership_observation.rs` plus store boundary fixture |
| `test/unit.membership-rest-observation.test.ts:55` | Failed page has no evidence; ordinary GET keeps data shape | `discord/tests/membership_observation.rs` |

Extra regression boundaries: equal-observation leave, equal-join flag reset,
invalid hints/non-membership hints, guild/member isolation, malformed dates and
sub-microsecond rejection. No database or Discord credentials are required.

## Verification

On the controller, from the isolated worktree:

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test membership_contract
python3 scripts/cargo_cache.py run -- test -p two-bot-discord --test membership_observation
```

Use the bounded wrapper; do not create a target directory or bypass a refusal.
Hosted CI's existing integration-test command includes both suites. A local
pool refusal is not a passing test result; record it and use authorized CI.
