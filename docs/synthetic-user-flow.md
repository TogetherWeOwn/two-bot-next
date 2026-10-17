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
| `prod-passive` | `scripts/prod_passive_probe.py` | `$PRODUCTION_WORKER_URL` (repo var) |

Each job writes one JSON evidence receipt (`synthetic-staging-<run>.json`,
`synthetic-prod-<run>.json`, 14-day artifacts) and fails on any check FAIL
or fence refusal. Production is passive: the prod job observes and alerts
only, and never blocks a deploy.

## Failure alerts

Two consecutive failed probes open one Discord ops-channel incident via
`scripts/synthetic_alert.py`. Once that alert is delivered, further failed
samples are suppressed until a successful probe posts one recovery message;
an undelivered alert is instead retried on later failed samples, and if none
was delivered a late ALERT (with the `probe failed` fallback summary, linking
that green run) posts on the first successful sample before any recovery.
This mirrors the Worker's failure-threshold and one-alert-per-incident
behavior. A decide step computes the transition and writes `event` to
`GITHUB_OUTPUT`; the post step runs only when
`!cancelled() && steps.decide.outputs.event != ''`, so a no-op is `skipped`
(never `success`) and a failed probe cannot skip the post via GitHub's
implicit `success()`. Only a successful post counts as delivered in
`scripts/synthetic_alert_history.py`; `skipped` or missing delivery leaves
the incident open, so a history-read no-op can never drop a recovery or fake
a delivery.

Incident state is rebuilt from this workflow's own completed scheduled runs
on `main` (the only state store; no extra permissions or infrastructure).
Reconstruction pages the runs list 15 runs at a time — a page is sized to
the 1 MiB cap on history responses, since one Actions run object measures
15–40 KB and a maximal 100-run page would exceed the cap; an oversized
response reports unknown history instead of being parsed truncated — and
reads jobs newest-first until the collected samples determine the incident
state by themselves — replaying them from any prior state converges,
confirmed by one further sample — or the history runs out. A sample whose
decide step succeeded with no event due (post step skipped) in a normally
finished job is an explicit decided-none record: on a healthy probe a
known-closed checkpoint, because with an open incident a healthy sample
always decides recovery or the late alert so the post step would have run;
on a failed probe a known-open record, because with unknown history it
would have fail-open alerted and with an undelivered incident it would have
re-alerted, so two such records in a row prove the incident is open and
delivered. A cancelled or timed-out job finishes neither way and its post
step is skipped by the `!cancelled()` gate even when an event was due, so
it records neither checkpoint. To keep the record honest, a healthy run
whose history read failed fails its decide step after writing the empty
event (its job shows red even though the probe passed); later
reconstruction treats such a sample as ambiguous, never as closed. (Runs
recorded before that decide-step record existed, during their own failed
history read, are indistinguishable from known-closed ones; that only
matters for an incident open across the change, and the window closes as
those runs age past the bound.) In the healthy steady state the newest
sample is a known-closed checkpoint and mid-outage the two newest samples
are known-open, so either way a decision costs one runs-list read plus a
couple of jobs reads — well inside the `GITHUB_TOKEN` hourly request
budget at the five-minute schedule for both environments; only histories
without decided-none records (for example jobs recorded before the decide
step existed) scan back toward the incident boundary, capped by the bound.
The scan is bounded at 200 prior runs
(about sixteen hours at the five-minute schedule). Delivery evidence on the
oldest retained failure counts, so an outage that outlives any fixed window
keeps its once-per-incident contract: one delivered opening alert, no
duplicate while it stays down, and exactly one recovery on the first healthy
sample (or the late ALERT above when the opening alert never delivered).
When reconstruction cannot be completed — the history read fails, or no
checkpoint is found within the bound — the incident state is unknown rather
than guessed: the documented fail-open policy applies (alert on a real probe
failure, send nothing on a healthy sample), and a fail-open alert that
delivers becomes fresh delivery evidence for later samples. One bound
exception: if all 200 scanned samples are healthy and no post step ever ran,
reconstruction settles closed (the same replay-from-closed as when history
runs out), so a long history outage cannot latch every later decision into
unknown; the accepted cost is that an incident whose evidence rolled past
the bound behind 200 such samples loses its recovery post. Skipped,
cancelled and manually dispatched runs are not probe samples and never move
the state. The manual `force_fail: true` staging proof bypasses the
threshold intentionally; it applies to staging only, and production stays
passive as described above:

```text
two-bot-next ALERT synthetic-user-flow:<staging|prod>: <failing checks>. Runbook: <runbook URL> | build <revision> <build_id> (git <sha>) | logs <github run URL>
```

`allowed_mentions` is off. The destination is Discord-only: any non-Discord
webhook URL refuses before any request, and the URL itself is never printed.
Runs with no incident transition send nothing and need no webhook. When an
alert or recovery transition is due, a missing webhook fails that step, so a
silent notification path cannot read as delivered.

## Operator bindings

- Repository variables `STAGING_WORKER_URL` and `PRODUCTION_WORKER_URL`:
  the staging and production Worker origins. `PRODUCTION_WORKER_URL` is the
  single source for deploy and passive synthetic checks; remove any production
  Environment override of the same name. A missing variable skips that target
  with a visible warning instead of producing recurring configuration failures.
- Environment `synthetic-alerts`, secret `SYNTHETIC_ALERT_WEBHOOK_URL`: the
  Discord webhook. Never paste it into a command, log, or report.

## Proving the path

Dispatch the workflow from `main` with `force_fail: true`: the staging probe
hits an unallowlisted origin and refuses before making a request. That fence
refusal produces no evidence receipt; the upload step tolerates the missing
file, and the alert posts with unknown build identity and the run link. This
proves the staging failure-to-alert path, not production alert delivery. Verify
the production schedule from its own runs, and verify production alert delivery
from an independently evidenced production failure and its matching alert.
