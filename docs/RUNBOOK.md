# two-bot-next operations runbook

## Container stays unready

`/health` is process liveness, not proof that the Discord gateway is connected.
The singleton Durable Object's existing keepalive probes `/readyz` on each tick
and records the result in its own durable storage (not the bot database).
Non-2xx responses and exceptions/timeouts count as failures. A 2xx result resets
the streak. Inbound `/health` and `/readyz` requests do **not** count as monitoring
samples; the keepalive loop is the single sampling source. Arming checks the
SDK's persisted schedule before inserting a task and coalesces concurrent
callers. Container startup, health traffic and DO reconstruction reuse the
pending task without postponing it. Each firing replaces its task (and any old
duplicate chains) with one successor; stale callbacks from the SDK's due-task
snapshot do not probe or record another sample. Nonempty readiness JSON is
drained, not cancelled, so the SDK's response proxy can settle cleanly.

### Threshold and notifications

- `KEEPALIVE_SECONDS` defaults to 60. Without tuning, the alert threshold is
  `max(1, ceil(600 / KEEPALIVE_SECONDS))` consecutive failed samples: ten failures
  at the default cadence, approximately ten minutes after arming the loop.
  Probes, cold startup, webhook latency and delayed alarms can extend that time;
  this is a sample-count threshold, not a wall-clock deadline.
- Optional plain Worker var `UNREADY_ALERT_FAILURES` overrides that count. Use a
  positive safe-integer string, e.g. `"5"`. Configure each named environment
  independently; omission or invalid runtime values use the default.
- At the threshold, one JSON log has `event: "container_unready_alert"`,
  `service: "two-bot-next"`, `consecutive_failures`, `threshold`, `status`,
  `first_failure_at` and `observed_at` (Unix milliseconds). `status: null` means
  the probe threw/timed out, rather than returning an HTTP response.
- Continued failures do not produce another alert for that incident. The first
  subsequent ready sample produces one `container_unready_recovery` log; a
  new failure streak can then trigger a new alert. A short streak that recovers
  below the threshold produces no alert or recovery notification.
- Optional **Worker secret binding** `OPS_ALERT_WEBHOOK_URL` enables a single
  Discord-compatible JSON webhook POST for each alert and recovery. Without it,
  monitoring is log-only; it is not a required deployment binding. The URL must
  be HTTPS without userinfo. Redirects are rejected, requests time out after six
  seconds, and `allowed_mentions.parse` is empty. Messages contain no mentions,
  credentials, guild/user identifiers or probe response bodies. The binding is
  not forwarded into the Rust Container.
- Only the operator creates/configures the webhook and per-environment Worker
  secret. Never put the URL in `wrangler.toml`, a plain var, a PR or a log.
  `wrangler/scripts/check-env-bindings.py` rejects plaintext use of this binding
  and validates optional threshold vars, without requiring the secret.

The streak and notification latch survive Container restarts and Durable Object
eviction. Each transition is persisted **before** logging/posting to suppress
replays. Delivery is **at most one attempt**, not guaranteed exactly-once:
a failed/timed-out webhook is not retried because it may already have accepted
the message. A crash between persistence and notification can lose that
notification. The structured transition log is the primary operational signal
when emitted; a webhook error emits `container_unready_webhook_failed` with only
notification type and HTTP status (or null). Recovery is attempted once even if
the alert webhook failed. Both webhook and storage failures still re-arm the
keepalive; storage failures propagate to the scheduler rather than falsely
claiming an alert was recorded.

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
