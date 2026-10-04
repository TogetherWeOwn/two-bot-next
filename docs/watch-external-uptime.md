# External uptime check: covering a missing Worker or alarm

Companion to the 48h alert checklist. The in-Worker keepalive watches a
running loop; it cannot page for its own absence. This sheet specifies the
separate external check that covers those modes, and records the
retry-policy decision for the at-most-once webhook.

Failure modes the keepalive cannot report (no log line is emitted):

- Worker undeployed, misrouted, or serving a stale version whose control
  fence refuses every probe.
- Singleton Durable Object evicted with the alarm chain never re-armed
  (no inbound traffic and no container start to run the arming path).
- Total monitoring outage: the schedule lookup/insert keeps failing, or
  Worker logs are unavailable, so even the arm-failed signal goes unseen.
- Webhook secret missing or misconfigured: monitoring is log-only and a
  log nobody reads pages nobody.

## Check spec (operator-provisioned, outside Cloudflare)

Poll from a vantage that shares fate with neither the Worker nor this
repo's CI: an independent uptime monitor or operator cron.

- Target: `GET /health` on the production Worker origin (liveness), plus
  `GET /readyz` where the monitor supports a second probe (readiness
  breakdown). Unauthenticated `GET` only; no query string, no headers,
  no body.
- Cadence 60 s, per-probe timeout 10 s. One poller per environment; two
  at most. Public probes are per-caller rate-capped (60 burst, 1/s
  refill), so a 60 s poller uses a negligible share — but a fleet of
  pollers behind one egress IP shares one bucket and would 429 itself.
- Down: non-2xx status, timeout, or TLS/DNS failure. Page after 10
  consecutive failures (~10 min), matching the in-band unready
  threshold, so deploys, cold starts and ownership changeovers do not
  page.
- Notify on a path independent of the in-band webhook secret: a separate
  secret and, where possible, a separate channel. Message lines mirror
  the in-band shape (`two-bot-next ALERT external_health: ...` /
  `two-bot-next RESOLVED external_health.`), with no mentions,
  identifiers or probe bodies.
- A healthy external poll is necessary but not sufficient: inbound
  `/health` also re-arms the keepalive chain, so external traffic keeps
  the loop armed — yet only the absence of
  `container_keepalive_arm_failed` in Worker logs proves the loop is
  armed. The operator checks both.

## Expected-down windows (notes, not incidents)

Production deploys, ownership changeover (brief refused/503 probes) and
staging redeploys trip single samples; the 10-failure threshold absorbs
them. Record them on the watch log as notes with the deployment id, not
as incidents.

## Provisioning check (names only, never values)

- [ ] Monitor target origin per environment (production required,
  staging recommended).
- [ ] Cadence, timeout and consecutive-failure threshold confirmed.
- [ ] Notify secret provisioned in the external monitor's own store
  (independent of the Worker webhook secret) with a received test page.
- [ ] No `container_keepalive_arm_failed` in recent Worker logs (the
  in-band loop is actually armed; the external check does not replace
  it).
- [ ] Watch log records the monitor name, owner and rotation contact.

All URLs, secrets and channel names are operator-held. None of them go
in `wrangler.toml`, a PR or a log.

## Retry-policy decision: keep at-most-once (no webhook retry)

The Worker persists each alert transition before notifying and makes one
webhook attempt per transition: a failed or timed-out POST is not
retried, and a crash between persistence and notification loses that
notification. See
[container readiness](container-readiness.md#threshold-and-notifications).

Decision: keep this behavior. A timed-out POST may already have been
accepted, and the webhook carries no idempotency key, so a retry can
double-page the operator. Retries issued from the same failing loop
share fate with the outage they report, and every retry is another
chance to leak the secret-bearing URL into an error log. The
compensating signals are independent of webhook fate:

| Loss mode | What still signals it |
|---|---|
| Webhook POST failed or timed out | the transition log line (primary signal) plus `container_unready_webhook_failed` / `metrics_alert_webhook_failed` with type and status only |
| Crash between persist and notify | the latched state: the next tick sees no transition and stays silent, but the transition log is absent from the tail while state reads alerted — and the later `RESOLVED` still fires |
| Unpaired `RESOLVED` arrives | treat as "the alert page was lost, investigate": the incident happened, the page did not |

Revisit trigger: post-incident evidence of a lost page with operator
impact. The fix then is a notified-flag plus bounded retry with an
idempotency key — explicitly deferred, not this sheet.

## Why not a scheduled workflow in this repo

An in-repo scheduled poll would hold the paging secret in repo secrets,
run on the same automation the watch already depends on for
verification, and still need operator provisioning for the secret —
without gaining an independent vantage. The external monitor stays
operator-owned so its fate is independent of both Cloudflare and this
repo's automation. Live provisioning is a governed operator step, not a
PR in this repo.
