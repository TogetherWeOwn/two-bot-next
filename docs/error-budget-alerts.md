# Error-budget burn alerts (48h watch)

Paging thresholds for error-budget consumption on the bot container during
the 48h production watch. Log volume and cardinality are covered by a
separate guard; this page says when to wake someone. The burn math below
is pinned by the offline
test `crates/bot/src/burn_rate_tests.rs`; a threshold change here without
the matching test change fails the suite. No staging, secret or live
paging test is involved.

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

Two multi-window alerts. Each fires only when both its long window (the
burn rate) and its short window (the confirmation) exceed the burn rate,
so a single bad minute pages nobody and a sustained bleed tickets fast.

| Alert | Severity | Burn rate | Long window | Short window | Budget spent if sustained |
| --- | --- | --- | --- | --- | --- |
| Fast burn | page | 12 | 15 min | 2 min | 6.25% per 15 min |
| Slow burn | ticket | 3 | 2 h | 15 min | 12.5% per 2 h |

Budget math: fraction spent = burn x window / period.

- Fast: 12 x 900 s / 172,800 s = 0.0625 (6.25%).
- Slow: 3 x 7,200 s / 172,800 s = 0.125 (12.5%).

At full fast burn the 48h budget lasts 4 hours (48 / 12); at full slow
burn it lasts 16 hours (48 / 3). Either alert firing means the watch
budget is on track to exhaust before cutover completes.

## Worked example (10,000 eligible events/hour)

Assumed rate ~2.8 events/s. 48h gives 480,000 eligible events, so the
watch budget is 480 errors. Allowed errors per window = burn x 0.001 x
window events:

- Fast burn pages at 30 errors in 15 minutes (12 x 0.001 x 2,500),
  confirmed by 4 errors in 2 minutes (12 x 0.001 x ~333, rounded up).
- Slow burn tickets at 60 errors in 2 hours (3 x 0.001 x 20,000),
  confirmed by 8 errors in 15 minutes (3 x 0.001 x 2,500, rounded up).

Scale to real traffic by replacing 10,000/hour with the observed
`two_bot_rest_requests_total` rate; the ratios and windows do not change.

## Response

- Fast-burn page: acknowledge, check recent deploys and the Discord API
  status, then follow the rollback one-pager. Silence the page only with
  a recorded decision.
- Slow-burn ticket: triage within the watch shift. Escalate to a page if
  half the 48h budget (240 errors at the example rate) is spent, or if
  the slow burn persists across two consecutive long windows.
- Both alerts reset when neither window exceeds its burn rate. A
  re-firing alert opens a new ticket; it never reopens a closed one.

## Related immediate alerts (not burn)

The fast/slow burn rules above stay silent on a single bad minute by
design. Three Worker alert rules cover gaps the burn math cannot see
without consuming budget (the first two page immediately; receiver
refusals need a three-window streak so one forged request never pages):

- `gateway_missed_events`: any increase of
  `two_bot_gateway_missed_events_total` between two keepalive samples
  pages at once (zero threshold), because a sequence gap fails the
  zero-missed-events acceptance outright. Runbook:
  [runbook](runbook.md#alert-gateway-missed-events).
- `ticker_stale`: a 15 s ticker (`scheduled_messages`, `settings`) with no
  success for more than 10 minutes pages at once, because skipped busy
  deadlines are neither success nor failure and never spend burn budget.
  Runbook: [runbook](runbook.md#alert-ticker-stale).
- `receiver_refusals:<family>`: refused
  `two_bot_internal_actions_total` outcomes grow in 3 consecutive
  keepalive samples per family, because a receiver-abuse or refusal
  storm stays quiet through the burn math while a single refusal must
  not page. Runbook:
  [runbook](runbook.md#alert-receiver-refusals).

None of these rules changes the burn thresholds, windows or budget above.

## Maintenance

Thresholds, windows, the budget ratio and the worked example move with
`crates/bot/src/burn_rate.rs` and `crates/bot/src/burn_rate_tests.rs`.
Change a number here: update the test constants in the same PR.
