# Container readiness monitoring

Part of the [operations runbook](runbook.md#sustained-unready-alerts).

## Container stays unready

`/health` is process liveness, not proof that the Discord gateway is connected.
The singleton Durable Object's existing keepalive probes `/readyz` on each tick
and records the result in its own durable storage (not the bot database).
Non-2xx responses and exceptions/timeouts count as failures. A 2xx result resets
the streak. Each probe's fetch and body read share one end-to-end six-second
deadline that does not rely on the SDK honoring abort, and body cancellation
is best-effort: a probe that stalls, or a valid answer that only arrives after
the deadline, is recorded as a failed sample — never as readiness or recovery
— and the tick still rearms. Inbound `/health` and `/readyz` requests do **not** count as monitoring
samples; the keepalive loop is the single sampling source. Arming checks the
SDK's persisted schedule before inserting a task and coalesces concurrent
callers. Container startup, health traffic and DO reconstruction reuse the
pending task without postponing it. Each firing replaces its task (and any old
duplicate chains) with one successor; stale callbacks from the SDK's due-task
snapshot do not probe or record another sample. Nonempty readiness JSON is
drained, not cancelled, so the SDK's response proxy can settle cleanly.

A schedule lookup or insertion failure emits the sanitized structured event
`container_keepalive_arm_failed` without error details. It does not reject
inbound health/readiness forwarding or the SDK's `onStart` startup hook; these
still return the Container's own status/body. The coalesced arming attempt is
released so later requests or startup can retry. This log is a monitoring outage
signal, not a readiness sample or proof that a successor has been scheduled.

### Threshold and notifications

- `KEEPALIVE_SECONDS` defaults to 60. Without tuning, the alert threshold is
  `max(1, ceil(120 / KEEPALIVE_SECONDS))` consecutive failed samples: two failures
  at the default cadence, approximately two minutes after arming the loop.
  Probes, cold startup, webhook latency and delayed alarms can extend that time;
  this is a sample-count threshold, not a wall-clock deadline.
- Optional plain Worker var `UNREADY_ALERT_FAILURES` overrides that count. Use a
  positive safe-integer string, e.g. `"5"`. Configure each named environment
  independently; omission or invalid runtime values use the default.
- At the threshold, one JSON log has `event: "container_unready_alert"`,
  `service: "two-bot-next"`, `consecutive_failures`, `threshold`, `status`,
  `first_failure_at`, `observed_at` (Unix milliseconds), `build_revision`,
  `failed_step`, `error_code` and `log_url`. `status: null` means
  the probe threw/timed out, rather than returning an HTTP response.
- Continued failures do not produce another alert for that incident. The first
  subsequent ready sample produces one `container_unready_recovery` log; a
  new failure streak can then trigger a new alert. A short streak that recovers
  below the threshold produces no alert or recovery notification.
- Optional **Worker secret binding** `OPS_ALERT_WEBHOOK_URL` supplies the Discord
  destination, but installing it alone never sends. Plain Worker var
  `OPS_ALERT_FORWARDING` must equal exactly `"on"` for either readiness or metrics
  forwarding. The checked-in default is `"off"` in every environment; absent or
  any other runtime value is log-only. Neither binding is forwarded into the
  Rust Container, and neither is required for structured monitoring.
- Both senders accept only literal `https://discord.com/api/webhooks/<id>/<token>`
  URLs: a numeric webhook ID and a nonempty token of letters, digits, `_` or `-`.
  An explicit default port `:443` is allowed. Other hosts/ports/paths, userinfo,
  query/fragment delimiters (even empty), whitespace, encoded paths and normalized
  aliases are rejected before fetch. A valid URL is not proof of approved channel
  ACLs or webhook ownership; the operator must verify those separately. Redirects
  are rejected. All of a tick's readiness, metric and remediation notifications
  share **one six-second forwarding allowance**, not six seconds per POST; fetch
  and response-body cancellation race that same deadline even if they ignore abort.
  `allowed_mentions.parse` is empty. Messages contain no mentions, credentials,
  guild/user identifiers or probe response bodies.
- Only the operator creates/configures the webhook and per-environment Worker
  secret. Never put the URL in `wrangler.toml`, a plain var, a PR or a log.
  `wrangler/scripts/check-env-bindings.py` rejects plaintext use of this binding
  and validates optional threshold vars, without requiring the secret.

To disable forwarding non-destructively, the operator sets that environment's
`OPS_ALERT_FORWARDING` var to `"off"` through its authorized deployment path,
retaining the webhook secret. Enabling or disabling the var changes the Worker
version and still requires normal version/ownership preflight and transfer; it
is not an in-place toggle or permission to bypass the fence. Structured logs,
readiness/metrics state and keepalive scheduling continue while forwarding is off.
Re-enabling does not replay muted transitions: only future transitions can send.
This switch is not authorization to install a credential, deploy or run a live page.

The streak and notification latch survive Container restarts and Durable Object
eviction. Each transition is persisted **before** logging/posting to suppress
replays. Delivery is **at most one attempt**, not guaranteed exactly-once:
a failed/timed-out webhook is not retried because it may already have accepted
the message. A crash between persistence and notification can lose that
notification. The structured transition log is the primary operational signal
when emitted; a webhook error emits `container_unready_webhook_failed` with the
notification type, HTTP status (or null), release, worker version and log URL —
never the binding, error text or body. Recovery remains eligible for one attempt
even if the alert webhook failed, subject to its own tick's allowance. Both
webhook and storage failures still re-arm the keepalive; storage failures
propagate to the scheduler rather than falsely claiming an alert was recorded.

Each owned tick finishes readiness/metric/remediation persistence and primary
logs, then inserts its successor **before** optional forwarding starts. A local
list retains notification order only for this tick: it is not a durable outbox.
The two existing probes each have a six-second budget; forwarding adds at most
six seconds total (18 seconds of network waits, leaving 12 seconds of the
[30-second ownership gate](https://developers.cloudflare.com/durable-objects/api/state/#blockconcurrencywhile)
for essential work). Forwarding is further clipped to end no later than 24 seconds
from entering the owned callback, reserving six seconds of gate headroom when
storage/arming took longer. This bounds optional transport, not storage outages,
scheduler failures or runtime reset behavior. Ownership ordering is unchanged.

When the allowance expires, remaining optional POSTs are omitted without losing
already-recorded state/logs, and are never replayed on a later tick. A timed-out
fetch has a rejection sink; a late response is cancelled best-effort without
awaiting cleanup. A late fetch or cancellation settlement cannot restart the
notification list, retry delivery or hold the next owned turn.

### Responding to an alert

1. Locate `container_unready_alert` in the affected environment's Worker logs.
   Check the streak and status; do not treat a healthy `/health` as recovery.
2. Correlate existing Container/gateway logs for disconnect/reconnect, invalid
   gateway configuration, startup failure or repeated identification errors.
   Do not fetch or paste credentials. This monitor observes readiness only;
   it does not restart the bot or change Discord permissions/intents.
3. If a bad deployment caused the outage, use the recorded staging/production
   rollback for that deployment under its normal gate. Keep the old artifact
   available; do not improvise a database restore or secret rotation.
4. Confirm the keepalive emits `container_unready_recovery` after a ready result.
   Repeated webhook error logs require the operator to verify the configured
   destination. Never expose the secret URL while diagnosing delivery.

This alert covers a running keepalive loop, not a missing Worker/alarm or a total
monitoring outage. Separate external uptime checks are still needed for those
failure modes. See [staging soak](staging-soak.md) for deployment readiness gates.

### Staging idle sleep

Staging does not run the keepalive (owner decision 2026-10-27: staging can shut
down when idle). Its vars set `KEEPALIVE_ENABLED = "0"` and
`IDLE_SLEEP_AFTER = "15m"`; production and the top level carry neither, so
production keeps the 60-second keepalive and the 30-minute `sleepAfter`
unchanged. `wrangler/scripts/check-env-bindings.py` refuses either key outside
staging, any `KEEPALIVE_ENABLED` value other than `"0"`, and an idle window
outside whole minutes 5-60.

With the keepalive off, nothing arms or rearms the schedule, and a chain left by
an earlier version drains on its next tick without probing. Only inbound
requests renew activity, so the container stops 15 minutes after the last one.
The next owned request (`/health`, `/readyz`, the ops reads) starts it again;
deploy verify and the drills poll `/readyz` and wake it the same way. A cold
probe answers `500` while the container starts (the six-second probe deadline
trips long before the two-minute auto-start finishes), then `503` until the
gateway connects. The two five-minute scheduled monitors (the
`synthetic-user-flow` staging probe and `slow-burn-tracker`) renew activity on
every scrape. Both run on main every five minutes, shorter than the 15-minute
window, so **while they run staging never reaches the idle timeout and this
change alone saves no container time**; it only removes the keepalive that
would otherwise hold the container awake. Savings need those two monitors to
stop waking the container (a no-wake probe path plus a bounded wake-and-wait),
which changes pinned workflow contracts and is tracked as follow-up work. 15
minutes outlasts the post-deploy voice synthetic, a 10-minute job that sends no
Worker traffic. `container_unready_alert` has one emitter, the keepalive tick,
so staging emits no readiness alert at all, awake or asleep; staging readiness
is observed by the `synthetic-user-flow` probe and its Discord alert, and the
staging soak gate reads that probe's runs instead
([soak entry gates](soak-entry-gates.md)). While staging sleeps it also has no
metrics alerts, and the bot is offline in the staging guild until something
wakes it. An armed
remediation drill drops both vars from the generated staging config
(`scripts/staging_rollout.py`), because its fault injector runs on keepalive
ticks. Rollback: delete both vars from `[env.staging.vars]` and redeploy.

### Offline verification

From the repository root:

```sh
npm ci --prefix wrangler --include=dev
npm run typecheck --prefix wrangler
npm test --prefix wrangler
python3 wrangler/scripts/check-env-bindings.py wrangler/wrangler.toml
python3 wrangler/scripts/test-env-bindings.py
```

Worker tests use the installed Container SDK with in-memory runtime/KV, real
in-memory SQLite for schedule persistence, and synthetic probe/webhook responses.
Fake-clock regressions invoke the SDK's actual alarm handler to prove one sample
per interval through concurrent traffic, startup, eviction and old duplicate
chains; nonempty JSON exercises its real response pipe. They never contact
Discord or deployed Workers, and never use staging/production databases.
The protected notification-budget acceptance covers a six-notification burst
with 5.5-second probes/replies, uncooperative fetch, stuck/rejecting cleanup,
late settlement, fast delivery, no replay, durable logs/state and exactly one
successor. Its fake clock observes real owned callback promises, not Cloudflare
admission/reset machinery; passing tests do not demonstrate installed health or
live webhook delivery.

### API references

- Container subclasses retain Durable Object storage and use `schedule()`, not
  an overridden `alarm()`: <https://developers.cloudflare.com/containers/api/container-class/#schedule>.
- Pinned SDK 0.3.7 source (`schedule`, `listSchedules`, `deleteSchedules`,
  due-task snapshots and the HTTP response pipe):
  <https://github.com/cloudflare/containers/blob/v0.3.7/src/lib/container.ts>.
  The public API docs do not describe deduplication; tests exercise the pinned
  implementation rather than assuming that identical callbacks coalesce.
- Storage input/output gates protect the persisted state transition:
  <https://developers.cloudflare.com/durable-objects/api/sqlite-storage-api/#access-storage>.
- Discord webhook JSON supports `content` and `allowed_mentions`:
  <https://docs.discord.com/developers/resources/webhook#execute-webhook>.
