# Discord REST guard

The action executor shares a process-wide invalid-request circuit breaker and
global rate-limit deadline. Both paced and single-attempt lanes pass through it;
clones and separately constructed executors use the same `process_guard()`.
This is **not** a per-route bucket limiter, and it does not coordinate different
processes sharing an egress IP. Each process needs its own conservative budget;
Discord's IP-level invalid-request limit remains the outer safety boundary.

## Threshold and admission

- Count every observed HTTP **401, 403 and 429** over a rolling **600 seconds**.
  Successful responses, 404, 5xx, transport failures, and local refusals do not
  consume the budget. Account at headers, even if body reading fails/cancels.
- Open at **5,000** invalid responses by default. Override once at process
  startup with `DISCORD_INVALID_REQUEST_THRESHOLD` (positive integer). Missing,
  zero or malformed values keep the default. Do not raise this towards Discord's
  10,000-invalid-requests/10-minute IP ban limit to hide a permission loop.
- While open, non-essential REST attempts return
  `DiscordError::Guard(GuardError::CircuitOpen)` **before network I/O**. This is
  safe pre-mutation, unlike a network timeout. The legacy paced GET/kick APIs
  still render their existing string/value failure contracts and do not retry
  a guard refusal. There is no manual reset endpoint.
- Only interaction callbacks are essential, so the bot can acknowledge/refuse
  user commands while automation is stopped. Callbacks still consume the budget
  on invalid responses, obey global pauses, and cannot bypass a fatal token.
- Close automatically when old responses expire and the rolling count falls
  **below** the threshold. Housekeeping runs on admission or snapshot; the close
  log is emitted on the next access, not by an always-running timer.
- Requests already in flight can complete after opening. They are accounted,
  but cannot be unsent. This is a circuit breaker, not a reservation of IP quota.

## Global 429

Recognize `X-RateLimit-Global: true`, `X-RateLimit-Scope: global`, JSON
`global: true`, or JSON `scope: "global"` on a 429. Store one shared monotonic
deadline from body `retry_after` seconds (wins over a valid `Retry-After` header)
plus **250 ms** padding. Concurrent waiters sleep to that same deadline; they do
not each add another delay. Overlapping global responses can extend it, never
shorten it. Local 429s retain the executor's existing lane/retry policy.

The guard does **not** truncate a global pause to the legacy per-call 60-second
cap. Missing, invalid, or unrepresentable timing fails closed for 600 seconds.
If body reading fails/cancels after global headers arrive, use header timing or
that fallback. Single-attempt calls retain their existing 5-second deadline,
which can expire while waiting on a longer global cooldown; no wire attempt is
made in that case. No additional automatic moderation retries are introduced.

## Fatal bot token and readiness

A 401 on a bot-authenticated endpoint permanently latches `token_invalid` for
this process. New REST attempts, including essential acknowledgements, return
`GuardError::TokenInvalid`. A 401 on an interaction callback instead identifies
its short-lived interaction token: it counts as invalid but does **not** condemn
the bot token. `/readyz` adds the `token_invalid` component (status `down` on
fatal, `ready` otherwise), so fatal token state returns **503** even if the gateway
is connected. `/health` stays 200 while the process can answer.

Operator response:

1. On `discord_breaker_open`, stop the offending workload and inspect permission
   or rate-limit failures, role hierarchy, and action volume. Let the window cool;
   fix the missing permission or caller bug, rather than repeatedly restarting
   to erase the budget. Check sibling processes on the same egress too.
2. On `discord_global_pause`, let Discord's deadline elapse. Do not bypass the
   guard or resubmit concurrent copies of the same action.
3. On `discord_token_invalid`, stop retries. Have the authorized credential
   operator verify provisioning/token validity under the normal credential
   approval procedure, then restart with the corrected configuration. Never
   log, swap in an unrelated credential, or automatically rotate a token.

## Observability and testing

Structured tracing events: `discord_breaker_open`, `discord_breaker_close`,
`discord_global_pause`, and `discord_token_invalid`. No route IDs, response
bodies, or credentials are included. `RateLimitGuard::snapshot()` exposes the
rolling invalid count, lifetime invalid/refused/open/close/global-pause counters,
open/fatal flags and remaining global cooldown for the metrics exporter. No
metrics endpoint exists in this base slice; no new public endpoint is added.

Local fixtures only:

```sh
cargo test -p two-bot-discord --locked --lib --test ratelimit_guard \
  --test executor_acceptance --test executor_regressions
cargo test -p two-bot --locked server::tests
```

The new mock tests inject an explicit shared guard so token failures and short
test windows cannot poison another test. Existing executor regression assertions
and raw response shapes are unchanged. Virtual-time unit tests cover exact
rolling expiry and global timing beyond the legacy clamp.
