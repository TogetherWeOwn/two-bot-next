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
  safe pre-mutation, unlike a network timeout. Sleeping admission callers wake
  immediately when the breaker opens or the bot token becomes fatal, including
  callers holding a paced lane or waiting for a global response body.
  The legacy paced GET/kick APIs
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
plus **250 ms** padding. Global headers install a pause **before reading the
body**, atomically with invalid-response accounting and before notifying waiters:
header timing when usable, otherwise a provisional full-window pause.
All response-body reads have a **5-second** deadline, including paced GET/kick.
Expiry automatically drops response accounting and clears its pending entry,
committing the header-anchored timing or conservative full-window fallback;
GET/kick handle the body error within their existing transport retry budget.
Admission stays closed until all global-header bodies resolve (or are cancelled),
**even if provisional header timing expires first**. Body timing replaces only
that response's provisional pause; it cannot shorten another response's
restriction. Deadlines are anchored at header receipt, not restarted when a
delayed body finishes or is cancelled. Concurrent waiters use the same maximum
deadline; GET, kick and command-publish retries do not start an additional local
429 delay after a global response body. Body-only global signals are recognized
when the body arrives. Paced callers retain their lane
reservation through global admission and recheck after pacing, so cooldown release
preserves the 110 ms GET / 350 ms kick dispatch spacing. Local 429s retain the
executor's existing lane/retry policy.

The guard does **not** truncate a global pause to the legacy per-call 60-second
cap. Missing, invalid, or unrepresentable timing fails closed for 600 seconds.
If body reading fails/cancels after global headers arrive, use header timing or
that fallback. Single-attempt calls retain their existing 5-second deadline,
which can expire while waiting on a longer global cooldown; no wire attempt is
made in that case: return `DiscordError::Guard(GuardError::AdmissionTimeout)`,
which is safe pre-mutation and does not increment the executor's wire counter.
Once dispatch starts, expiry remains uncertain `DiscordError::Timeout`. The
5-second budget includes admission and wire time, not two separate budgets. No
additional automatic moderation retries are introduced. Idempotent command-publish
sync instead waits for paced admission **outside** its 5-second wire deadline, so
long global pauses do not abort a retry. Fatal/breaker refusals still interrupt
that wait promptly. Kick results count only dispatched HTTP attempts: pre-wire
build/guard refusals report zero, and a refusal after one exchange reports one.

Audit mirror posts retain their paced lane through late DB authorization and the
bounded send. All admission waits precede authorization. If a global restriction
arrives during authorization, the final no-wait check returns
`GuardError::GlobalPaused` before dispatch; breaker/fatal closures likewise refuse
locally. The audit adapter maps these guard refusals to provably-unsent `Rejected`,
never the uncertain result used for post-dispatch timeouts. **Reads differ**:
a local guard refusal of channel metadata/history is `MirrorError::Uncertain`,
not channel permission evidence. The audit service defers preflight or holds
reconciliation, preserving its boundary/cursor rather than permanently
quarantining a valid delivery. Genuine Discord 403/404 reads still reject.

Internal member actions retain typed admission refusals until their durable
claim is disposed. Temporary refusal becomes retryable `rate_limited`/429;
fatal bot-token refusal becomes `discord_unavailable`/502, not a permanent 422.
Only the typed guard path consumes/releases a provably-unsent claim into the
migration `0352` `not_sent` state. A fresh authenticated retry can claim the same
key, exact payload and pinned subject after cooldown. Role actions perform only
GETs before their sole final PUT, so refusal at any admission boundary proves
that mutation was not dispatched. A dispatched 429, 5xx or timeout still retains
the unknown fence and cannot automatically execute again. Fatal token recovery
still requires the operator procedure below, never a guard reset.

## Fatal bot token and readiness

A 401 on a bot-authenticated endpoint permanently latches `token_invalid` for
this process. New REST attempts, including essential acknowledgements, return
`GuardError::TokenInvalid`. A 401 on an interaction callback or original-response
webhook edit instead identifies its short-lived interaction token: it counts as
invalid but does **not** condemn
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
open/fatal flags, remaining global cooldown and `pending_global_responses` for
the metrics exporter. A pending global body still blocks admission when its
provisional remaining time is zero. No metrics endpoint exists in this base slice; no new public endpoint is added.

Local fixtures only:

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-discord --lib \
  --test ratelimit_guard --test executor_acceptance --test executor_regressions
# Authorized test-container DB only; the member suite is explicitly run in CI.
python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db \
  --test internal_action_store
python3 scripts/cargo_cache.py run -- test -p two-bot-discord --features db \
  --test internal_member_store -- --ignored
python3 scripts/cargo_cache.py run -- test -p two-bot --test startup
python3 scripts/cargo_cache.py run -- test -p two-bot server::tests
```

The new mock tests inject an explicit shared guard so token failures and short
test windows cannot poison another test. Existing executor regression assertions
and raw response shapes are unchanged. Virtual-time unit tests cover exact
rolling expiry and global timing beyond the legacy clamp.
