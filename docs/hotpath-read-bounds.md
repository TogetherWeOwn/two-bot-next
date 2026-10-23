# Hot-path store read bounds

These are row-materialization bounds, not query-plan or latency guarantees.
No migration, index, cadence, deployment setting or claim token changes are
required.

| Read | SQL bound | Result / continuation |
| --- | --- | --- |
| Per-event settings | `WHERE guild_id = $1` | `load_guild_snapshot` reads the global revision and only the target guild's settings from one repeatable-read snapshot. No row cap: all of that guild's overrides participate. The supervisor keeps its global `load_snapshot`. Per-guild runtime poll counts use the same guild predicate. |
| Seasonal decay | Member-id keyset pages, fixed at most 25 rows independently of the update quota | One `apply_seasonal_decay` call changes at most 25 members. SQL filters positive-XP rows owing another week before taking update locks, so active, zero-XP and already-decayed rows do not block awards. The cutoff retains the existing millisecond floor and `decayed_weeks` watermark; eligibility is rechecked after a contended lock. Repeat with the same tick until zero; zero means no decayable member remains, not merely that a page contained unchanged rows. Row locks cover read, math, update and audit insertion. Lifetime XP and the 28-day/10-percent math are unchanged. |
| Pending retention deletions | Request-id keyset pages, 25 rows plus one lookahead | A run completes at most 25 eligible requests. Guarded requests are counted and skipped without retaining their IDs, so a guarded prefix cannot starve later requests. `pending_truncated=true` means pending requests remain beyond the probed prefix. Deferred/completed per-request counters cover that prefix; the overdue `COUNT(*)` stays exact. Execute drains further on later ordinary passes, keeping the existing savepoints, receipts and guard refusals. |
| Burst-history inspection | Newest-first `LIMIT 26`, return at most 25 | `BurstHistoryPage.truncated` is true only when another in-window row exists. Ties use the reservation ID. Admission does not infer counts from this diagnostic page: exact guild/user `COUNT(*)` aggregates and the cooldown maximum run under the existing advisory lock. Older user reservations still win the `user_burst` refusal before `guild_burst`. |
| Funnel membership | Event-id keyset pages, 25 rows plus one lookahead | `MembershipPage.truncated` says the first page was insufficient, **not** that the membership is partial. The hot-path `membership` drains all pages from one repeatable-read snapshot into a constant-size chronological fold. Join attribution, observation-based presence, leave tie-breaking, microseconds and inactivity semantics stay shared with the in-memory store. |

Retention's read-only preview, executing drain and raw-expiry exclusion use the
same capped eligible-request selection. This prevents the preview from excluding
all pending users' raw rows when only the first 25 eligible requests would
actually settle. Guild purge and raw-expiry policies retain their own existing
bounds and semantics.

`MembershipStore::membership_rows` remains an explicit full-history diagnostic
readback for the reusable store contract; it is not used by the production
membership hot path. It deliberately does not claim the bounded-memory guarantee
of `membership` / `membership_paged`.

The truncation vocabulary is fixed boolean fields (`pending_truncated` and
`truncated`), never a dynamic metric label or a raw database error. The retention
job includes `pending_truncated` in its structured report, and treats a truncated
run as pending work rather than reporting a fully drained queue.

## Verification

The protected acceptance targets cover settings isolation and the unchanged
global supervisor, 30-row / 25-row boundary cases, repeat-pass decay and deletion
drains, decay math/audits, burst refusal precedence and complete funnel
membership. Run against a disposable test bootstrap only:

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db --test seasonal_decay_paging_acceptance --test seasonal_decay_contention_acceptance -- --test-threads=1
python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --test hotpath_bounds_behavioral_acceptance --test hotpath_bounds_marker_acceptance --test hotpath_review_regressions -- --test-threads=1
python3 scripts/cargo_cache.py run -- test -p two-bot --bin two-bot hotpath -- --test-threads=1
```

Set the authorized test URL as described in `CONTRIBUTING.md`. An absent URL can
skip local database bodies; a successful compile or skipped run is not behavioral
GREEN evidence. A bounded-cache refusal is also not a test verdict.
