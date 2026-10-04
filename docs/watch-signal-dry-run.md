# Watch signal query dry-run (staging read-only)

Dry-run record for the DB-error (§6) and send-admission (§7) queries in
[watch-signal-queries.md](watch-signal-queries.md). Staging read-only: no
alert, monitor, threshold, or config change; no SQL against staging or
production; no credential created, rotated, or substituted.

## Tested revision

- Staging serving revision (from `/readyz` at probe time):
  `9f0835e970a3cb04e8865fbaf8ba19d55e7f6215`
- Worker build id from the same response: `37167366273-1`
- Probe time: 2026-10-04 ~01:31 UTC

## Staging read-only probes

Operator inputs: the `STAGING_WORKER_URL` repo variable only. No token was
used except one invalid placeholder bearer for the boolean-only
scrape-token check (401 means armed, 404 means unconfigured).

| Probe | Result |
|---|---|
| `GET /health` | 200 `{"status":"ok"}` |
| `GET /readyz` | 503; `process` ready, `gateway` starting, `database` ready; same revision and build id as above |
| `GET /ops/metrics` (no auth) | 404 `not found` |
| `GET /ops/metrics` (invalid placeholder bearer) | 404: the scrape token is not provisioned (the route 404s when unset; see `wrangler/src/index.ts`) |

Consequence: the §§6–7 PromQL queries are not executable against staging
because there is no exposition to evaluate. Nothing was retried with another
credential, and no SQL was run (the pack forbids staging SQL).

## Offline rehearsal (exact shipped evaluation)

Ran the exact shipped path (`parseExposition` → `evaluateMetrics` →
`transitionMessages` from `wrangler/src/alert-rules.ts`) against synthetic
exposition shaped like §§6–7. No staging system was touched:

- DB-error (§6): `admission=0,other=0` silent; a `+2` trickle silent; a `+5`
  burst fires `db_errors` with the exact webhook line `two-bot-next ALERT
  db_errors: database errors reached 3+ between samples. Runbook: <deep
  link>`; a counter reset (restart) skips the window instead of firing.
- Send-admission (§7): new `blocked` refusals in windows 1–2 silent, window 3
  fires `send_admission_blocked` (window 4 still firing); a quiet window
  clears the streak to 0.
- Full Worker suite on the same checkout: 354 tests, 0 failures
  (`npm --prefix wrangler test`).

## Threshold comparison

The signal-thresholds table in
[production-deploy.md](production-deploy.md#signal-thresholds-budgets-and-rollback-triggers)
covers DB pool pressure only; it has no numeric row for `db_errors` or
`send_admission_blocked`. The authoritative numbers for these two signals
live in the alert-rules table in
[metrics.md](metrics.md#off-container-scrape-and-alert-rules) (3 or more
storage failures between samples; new refusals in 3 consecutive samples)
and the matching [runbook](runbook.md#alert-db-errors) sections. The missing
budget rows are filed as a follow-up card on the planning thread (this note
carries no tracker ID by public-repo hygiene). Until that lands, the runbook
numbers govern, and the pack's sketches agree with them.

Staging note: the deploy gate was red at probe time (`rollout_timeout`,
zero healthy instances, gateway `starting`). That is already tracked on the
planning thread; no new card here.

## Replay recipe (reviewer: run one)

```bash
npm --prefix wrangler ci --include=dev
node --input-type=module --import ./wrangler/test/cloudflare-loader.mjs -e "
import { EMPTY_STATE, evaluateMetrics, parseExposition, transitionMessages } from './wrangler/src/alert-rules.ts';
const body = (l) => ['# HELP x y', '# TYPE x gauge', ...l, ''].join('\n');
const ev = (lines, prev = EMPTY_STATE) => evaluateMetrics(parseExposition(body(lines)), prev, 1791077493);
const s0 = ev(['two_bot_db_errors_total{op="admission"} 0','two_bot_db_errors_total{op="other"} 0']);
const burst = ev(['two_bot_db_errors_total{op="admission"} 5','two_bot_db_errors_total{op="other"} 0'], s0.state);
console.log(JSON.stringify(burst.firing));
console.log(transitionMessages([], burst.firing).join('\n'));
"
```

Expected output: `["db_errors"]` followed by the exact `two-bot-next ALERT
db_errors: ...` line. The read-only staging probes above can be repeated with
`${STAGING_WORKER_URL}`; the placeholder-bearer check must return 404 while
the scrape token stays unprovisioned.
