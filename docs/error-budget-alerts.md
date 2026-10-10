# Error-budget burn alerts (48h watch)

Manual checkpoint thresholds for error-budget consumption on the bot container
during the 48h production watch. **Status: the multi-window burn alerts are not
installed.** The burn math has no runtime caller, and the Worker alert state
retains only the previous sample, not the history needed for these windows.
No automatic fast-burn page or slow-burn ticket exists; silence is not health.
Log volume and cardinality are covered by a separate guard. The Rust burn
constants and math are pinned by the offline test
`crates/bot/src/burn_rate_tests.rs`; this document must stay aligned with them
(the test does not parse this page). No staging, secret or live paging test
is involved.

## SLO and budget

- SLO: 99.9% of eligible events succeed, measured over the 48h watch.
- Budget ratio: 0.001 (0.1% of eligible events may fail).
- Budget period: 48h = 172,800 s. Budget = 0.001 x eligible events in 48h.
- Eligible events: REST calls counted in
  `two_bot_rest_requests_total{route,result}` plus gateway session outcomes
  (`two_bot_gateway_disconnects_total`, `two_bot_gateway_missed_events_total`).
  Error classes: `result="5xx"` or `result="transport"`, and session failures
  (disconnect without resume, missed events). `4xx` and `429` are caller or
  rate-limit outcomes, not budget spend.

## Alert thresholds

Two multi-window thresholds, **not installed alerts**. At each watch
checkpoint (+15 min, +1 h, +6 h, +24 h, +48 h), evaluate both the long window
(the burn rate) and the short window (the confirmation). Escalate manually
only when both meet the threshold; one bad minute alone is not a burn trigger.
The page/ticket severities below describe the manual response, not delivery.

| Alert (not installed) | Manual severity | Burn rate | Long window | Short window | Budget spent if sustained |
| --- | --- | --- | --- | --- | --- |
| Fast burn | page | 12 | 15 min | 2 min | 6.25% per 15 min |
| Slow burn | ticket | 3 | 2 h | 15 min | 12.5% per 2 h |

Budget math: fraction spent = burn x window / period.

- Fast: 12 x 900 s / 172,800 s = 0.0625 (6.25%).
- Slow: 3 x 7,200 s / 172,800 s = 0.125 (12.5%).

At full fast burn the 48h budget lasts 4 hours (48 / 12); at full slow
burn it lasts 16 hours (48 / 3). Either manually confirmed threshold means
the watch budget is on track to exhaust before cutover completes.

## Worked example (10,000 eligible events/hour)

Assumed rate ~2.8 events/s. 48h gives 480,000 eligible events, so the
watch budget is 480 errors. Allowed errors per window = burn x 0.001 x
window events:

- Fast burn calls for a manual page at 30 errors in 15 minutes
  (12 x 0.001 x 2,500), confirmed by 4 errors in 2 minutes
  (12 x 0.001 x ~333, rounded up).
- Slow burn calls for a manually opened ticket at 60 errors in 2 hours
  (3 x 0.001 x 20,000), confirmed by 8 errors in 15 minutes
  (3 x 0.001 x 2,500, rounded up).

Scale to real traffic by replacing 10,000/hour with the observed
`two_bot_rest_requests_total` rate; the ratios and windows do not change.

## Manual checkpoint evaluation

Use a Prometheus-compatible store already scraping the authenticated Worker
`/ops/metrics` read path ([metrics read path](metrics.md#off-container-scrape-and-alert-rules)),
scoped to **one deployment/environment**. `/ops/metrics` itself returns a current
snapshot, not a PromQL query service or a historical window. At each checkpoint,
record the evaluation time, window coverage, request/error counts and both burn
values in the [watch log](production-deploy.md#watch-rows-one-per-finding-and-per-checkpoint).
Do not call a missing, incomplete or zero-traffic window green; record it as
unknown and extend the watch. In particular, +15 min and +1 h do not yet cover
a full 2 h watch window.

These queries evaluate the **REST portion only**, with `4xx`/`429` contributing
requests but not errors. They do not certify the combined REST/gateway SLO:
raw disconnect counts do not say whether a disconnect later resumed, and there
is no eligible gateway-session outcome denominator in these series. Correlate
gateway failures separately before recording a budget disposition.

```promql
# 15 min: fast long window and slow short window
sum(increase(two_bot_rest_requests_total{result=~"5xx|transport"}[15m]))
/ (0.001 * sum(increase(two_bot_rest_requests_total[15m])))
```

```promql
# 2 min: fast short window
sum(increase(two_bot_rest_requests_total{result=~"5xx|transport"}[2m]))
/ (0.001 * sum(increase(two_bot_rest_requests_total[2m])))
```

```promql
# 2 h: slow long window
sum(increase(two_bot_rest_requests_total{result=~"5xx|transport"}[2h]))
/ (0.001 * sum(increase(two_bot_rest_requests_total[2h])))
```

Fast burn requires 15 min **and** 2 min values >= 12; slow burn requires 2 h
**and** 15 min values >= 3. Inspect `sum(increase(two_bot_rest_requests_total[15m]))`
(and the corresponding `[2m]` / `[2h]` queries) to verify each denominator is
positive. Counter resets are handled by `increase`, but scrape gaps or a
missing error series are unknown, not zero. Without an existing history store,
retain timestamped read-only snapshots at the normal scrape cadence and compute
each window's error/request deltas by hand; split on resets and reject uncovered
windows. The Worker's one previous sample cannot supply these windows.

For gateway evidence, inspect these counts at the same evaluation time
(repeat with `[2m]` / `[2h]` for the other windows):

```promql
sum(increase(two_bot_gateway_disconnects_total[15m]))
sum(increase(two_bot_gateway_resumes_total[15m]))
sum(increase(two_bot_gateway_events_total{event="READY"}[15m]))
sum(increase(two_bot_gateway_missed_events_total[15m]))
```

Treat these as four separate queries, not a ratio: correlate each disconnect
with a later RESUMED/READY receipt and moderator-observed recovery in the same
window. Never subtract total resumes/READYs from disconnects to certify pairing.
Any unpaired loss or missed-event increase needs its own finding; absence of
identity-paired evidence leaves the combined budget unknown.

## Response

- Fast-burn manual page: notify the watch responder, check recent deploys
  and the Discord API status, then follow the rollback one-pager. Record
  the acknowledgement and disposition in the watch log.
- Slow-burn manual ticket: open and triage it within the watch shift.
  Escalate manually to a page if half the 48h budget (240 errors at the
  example rate) is spent, or if slow burn persists across two consecutive
  long windows.
- Record recovery when neither window meets its burn threshold. If burn
  recurs after a closed disposition, manually open a new finding; there is
  no automatic reset, ticket creation or reopening.

## Related immediate alerts (not burn)

Unlike the uninstalled fast/slow burn alerts above, these three **installed
Worker rules** evaluate keepalive samples independently of the burn math.
They fire on their own conditions; notification additionally requires the
configured [alert-forwarding path](metrics.md#off-container-scrape-and-alert-rules).
An installed rule is not proof that a page was delivered:

- `gateway_missed_events`: any increase of
  `two_bot_gateway_missed_events_total` between two keepalive samples
  pages at once (zero threshold), because a sequence gap fails the
  zero-missed-events acceptance outright. Runbook:
  [runbook](runbook.md#alert-gateway-missed-events).
- `ticker_stale`: a 15 s ticker (`scheduled_messages`, `settings`) with no
  success for more than 10 minutes pages at once, because skipped busy
  deadlines are neither success nor failure and never spend burn budget.
  Runbook: [runbook](runbook.md#alert-ticker-stale).
- `receiver_refusals:<family>`: refused `two_bot_internal_actions_total`
  outcomes rising in 3 consecutive keepalive samples pages (a single
  forged pre-auth probe, always family `other`, stays silent), because a
  receiver-abuse or refusal storm stays quiet through the burn math.
  Runbook: [runbook](runbook.md#alert-receiver-refusals).

None of these rules changes the burn thresholds, windows or budget above.

## Maintenance

Thresholds, windows, the budget ratio and the worked example move with
`crates/bot/src/burn_rate.rs` and `crates/bot/src/burn_rate_tests.rs`.
Change a number here: update the test constants in the same PR.
