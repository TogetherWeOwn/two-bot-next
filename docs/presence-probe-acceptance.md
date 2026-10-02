# Presence probe-cycle and reopen-trigger acceptance

`crates/core/tests/presence_probe_acceptance.rs` pins the public
`two_bot_core::presence` contract through the public API only
(`crates/core/src/presence.rs`): hourly probe cadence, 24 h bot-floor max
age, the probe lease guard, `daily_peaks` / `latest_bot_floor` derivation,
and the 45-peak / 3-day / 14-day-window / 7-day-verdict reopen contract.
It is pure and offline: no Discord client, no SQL, no network.

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test presence_probe_acceptance
```

CI runs it with the other integration targets
(`cargo test --workspace --test '*'`).

Containment: guild aggregates only, never per-member rows, never published
(no `web_v1` view may read `presence_probe`); the only reader is an
operator trend report. This suite asserts decision values only, so it
cannot publish the series.

## Acceptance matrix

| # | Contract | Tests |
| --- | --- | --- |
| 1 | `decide_probe_cycle` honors the hourly cadence; `bot_floor_due` honors the 24 h max age (`>=` boundary rescans; fresh hourly tick keeps the cached floor; failed reads write nothing) | `probe_cadence_constants_match_spec`, `bot_floor_due_honors_24h_max_age`, `decide_probe_cycle_honors_hourly_cadence` |
| 2 | `decide_probe_lease` refuses overlapping cycles (`Run` / `Skip` / `Takeover` with `>=` takeover boundary) and `release_probe_lease` frees only the holder's own claim | `decide_probe_lease_refuses_overlapping_cycles`, `release_probe_lease_frees_only_own_claim` |
| 3 | `sanitize_presence_count` refuses negative/absurd-missing readings (`None` / negatives → `None`); zero and ordinary aggregates pass through; no upper-bound clamp (the 45 threshold is the guard) | `sanitize_presence_count_refuses_negative_and_missing` |
| 4 | `daily_peaks` derives per-UTC-day peak (max, never mean), low, reading count and the 45-boundary qualifying flag oldest-first; `latest_bot_floor` takes the newest observed floor from the whole series, or `None` | `daily_peaks_derive_peaks_lows_and_qualifying_flag`, `latest_bot_floor_uses_newest_observed_floor` |
| 5 | `evaluate_trigger` requires the full contract for a reopen verdict: 7 observed days minimum (`InsufficientData` below), 3 sustained qualifying days (`Closed` below), 14-day window (stale/future readings excluded, cutoff day inclusive), and `web_v1_live` (`Armed` without it, `Fires` with it) | `evaluate_trigger_requires_seven_day_verdict_window`, `evaluate_trigger_requires_three_sustained_peaks`, `evaluate_trigger_requires_14_day_window`, `evaluate_trigger_stays_silent_without_web_v1_and_fires_with_it` |

## Not covered here

- REST/store seams (`fetchPresenceCount` fetch, member-list roster scan,
  row persistence): this suite drives the pure decisions only.
- The overlap lease is in-memory per process; a restart starts unleased.
  Lease persistence across restarts is a parent obligation.
- `web_v1_live` is human-supplied; nothing in the bot observes it.
- Verdict `reason` strings are asserted non-empty only; exact wording is not
  pinned.
