# Five-minute user-flow synthetic (staging + passive prod)

The `synthetic-user-flow` workflow probes the main HTTP user flow every five
minutes: `GET /health` plus `GET /readyz` with build identity, on staging and
— passively — on production. Both probes are read-only, send no credentials,
follow no redirects, and refuse any origin outside their own allowlist before
a request is sent. No Gatus or third-party monitor is used.

## What runs

| Job | Probe | Target |
| --- | --- | --- |
| `staging` | `scripts/staging_health_contract_probe.py` | `$STAGING_WORKER_URL` (repo var) |
| `prod-passive` | `scripts/prod_passive_probe.py` | `$PROD_WORKER_URL` (repo var) |

Each job writes one JSON evidence receipt (`synthetic-staging-<run>.json`,
`synthetic-prod-<run>.json`, 14-day artifacts) and fails on any check FAIL
or fence refusal. Production is passive: the prod job observes and alerts
only, and never blocks a deploy.

## Failure alerts

A failing job posts one Discord ops-channel message via
`scripts/synthetic_alert.py`, reusing the Worker alert pattern:

```text
two-bot-next ALERT synthetic-user-flow:<staging|prod>: <failing checks>. Runbook: <runbook URL> | build <revision> <build_id> (git <sha>) | logs <github run URL>
```

`allowed_mentions` is off. The destination is Discord-only: any non-Discord
webhook URL refuses before any request, and the URL itself is never printed.
A green probe sends nothing, so green runs need no secret. A failed probe
with no webhook fails the alert step, so a silent alert path cannot read as
delivered.

## Operator bindings

- Repo vars `STAGING_WORKER_URL`, `PROD_WORKER_URL`: the two Worker origins.
- Environment `synthetic-alerts`, secret `SYNTHETIC_ALERT_WEBHOOK_URL`: the
  Discord webhook. Never paste it into a command, log, or report.

## Proving the path

Dispatch the workflow from `main` with `force_fail: true`: the staging probe
hits an unallowlisted origin, the fence refuses, the evidence upload keeps
the refusal, and the alert step posts. The prod schedule and delivery are
verified independently the same way: one forced staging failure proves the
alert path, and the prod job's own green/red runs prove its schedule.
