# REST executor pacing/backoff/kick-status acceptance

`crates/core/tests/pacing_backoff_acceptance.rs` pins the public
`two_bot_core::action_outcomes` helpers the executor calls. It is pure and
offline: no database, no Discord client, no feature flags.

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test pacing_backoff_acceptance
```

CI runs it with the other integration targets (`cargo test --workspace --test '*'`).

Parity: [parity](parity.md) §6 Discord REST row — 110 ms pacing,
429 `retry-after + 250 ms`, 5xx exponential backoff; kicks 350 ms pacing,
4 retries. Legacy `rest.ts` / `kick.ts`. Retry loops and lane floors
(`PACE_INTERVAL_MS` / `KICK_INTERVAL_MS`) live in the executor
(`two-bot-discord`) and are not covered here; this suite pins the pure
helpers the executor feeds them.

## Acceptance matrix

| # | Contract | Tests |
| --- | --- | --- |
| 1 | `pace_wait_ms`: 0 once the interval has elapsed (boundary included), the exact remaining ms otherwise; saturating `u64` arithmetic, so never negative and never a wrap-around early fire | `pace_wait_is_zero_once_the_interval_has_elapsed`, `pace_wait_returns_the_exact_remaining_ms`, `pace_wait_never_wraps_at_the_u64_edges` |
| 2 | `backoff_ms`: 500 × 2^attempt across the 5-try budget (500…8000), doubling per attempt, saturating past attempt 20; `retry_after_ms`: header seconds × 1000 + 250 ms, body wins, fractions ceil, clamp at 60 s | `backoff_grows_exponentially_within_the_retry_budget`, `backoff_saturates_instead_of_overflowing`, `retry_after_adds_250ms_padding_to_header_seconds`, `retry_after_body_wins_over_header`, `retry_after_clamps_at_sixty_seconds`, `retry_after_refuses_negative_and_non_finite_values` |
| 3 | `classify_kick_status`: 200/204 → Removed, 404 → AlreadyGone, 403 → Forbidden, 401 → Unauthorized, 429 → RateLimited, 5xx → ServerError, everything else → Other | `classify_kick_status_maps_documented_statuses`, `classify_kick_status_leaves_everything_else_other` |
| 4 | `set_send_bit`/`clear_send_bit` touch only bit 11; `lockdown_overwrite` denies send and drops the allow bit; `unlock_overwrite` clears only the send bit; unlock undoes a lockdown's deny | `send_bit_helpers_preserve_unrelated_bits`, `lockdown_overwrite_denies_send_and_preserves_other_bits`, `unlock_overwrite_clears_only_the_send_bit`, `unlock_undoes_lockdown_deny_while_allow_stays_cleared` |
| 5 | `parse_retry_after_secs`: `None` for missing/blank/non-numeric, numerics (incl. negatives) parse; the refusal of negative and non-finite values lives in `retry_after_ms`'s legacy 1 s fallback, which the suite pins end to end | `parse_retry_after_secs_refuses_garbage`, `negative_retry_after_values_fall_back_to_one_second_plus_padding` |

## Not covered here

- The executor's retry loops (same-attempt 429 park vs. attempt-counting 5xx retry), lane floors, the 5 s moderation abort, and the no-auto-retry moderation path — all `two-bot-discord` behavior, not `action_outcomes` values.
- `KickOutcome`/`KickResult` construction (the executor builds these from `KickStatus`); the unit tests in `action_outcomes.rs` cover the outcome/action mapping.
