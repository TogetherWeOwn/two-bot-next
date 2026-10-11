# Worker gateway-failure alert copy and dashboard queries (offline)

Offline content for the `container_gateway_failure` Worker event. It installs
nothing, provisions no secret, and runs no live paging test. The installer
adds one rule to `wrangler/src/alert-rules.ts`, one runbook section, and the
packet spelling, then proves delivery on staging.

Source contract: [metrics](metrics.md) (Worker log fields bullet),
[self-diagnosing gateway failures](startup-diagnostics.md#self-diagnosing-gateway-failures-readyz-gatewayfailure),
keepalive emission in `wrangler/src/index.ts` (keepalive), vocabulary in
`crates/bot/src/gateway_failure.rs`.

## Catalogued fields (exact spellings)

Every query below uses these spellings verbatim. The event reaches Workers
Logs only, never `/metrics`.

- `event="container_gateway_failure"`, emitted by the Container DO keepalive
  in `wrangler/src/index.ts` once per tick while the gateway task is failing.
- `phase` is `durable_gateway` (the only phase today).
- `class` is one of the twelve `FailureClass` tokens:

| `class` | Failing step |
| --- | --- |
| `store_unavailable` | shared store missing |
| `gateway_pool_connect_failed` | checkpoint pool connect |
| `checkpoint_load_failed` | durable checkpoint read |
| `onboarding_gates_invalid` | onboarding gate parsing |
| `onboarding_init_failed` | onboarding runtime init |
| `custom_commands_init_failed` | custom-command bootstrap |
| `milestones_load_failed` | milestone read |
| `automod_config_invalid` | automod config rejected |
| `automod_executor_failed` | automod REST executor build |
| `raid_executor_failed` | raid-watch REST executor build |
| `gateway_runtime_failed` | running gateway operation |
| `gateway_task_panicked` | gateway task panicked |

Only `[a-z0-9_]{1,32}` tokens are ever logged; anything else is dropped, and
no SQL error, URL, credential, or exception text reaches the line.

## Alert-rule copy

Proposed rule id (mirrors `job_stale:<job>`): `gateway_failure:<class>`
(one firing key per class, e.g. `gateway_failure:checkpoint_load_failed`).

| Item | Copy |
| --- | --- |
| Fires when | at least one `event="container_gateway_failure"` line with that `class` in the trailing window `max(5 minutes, 3 x KEEPALIVE_SECONDS)` (15 minutes at the default 60 s tick covers a missed tick or two) |
| Severity | critical (page): the gateway task is fatal, the process exits 1 after a 15 s linger on startup failures (75 s on a running-gateway failure), and the guild has no gateway until supervision restarts it |
| Runbook | proposed `runbook.md#alert-gateway-failure` (installer adds the section); until then follow [logs and keepalive](runbook.md#logs-and-keepalive), [sustained-unready alerts](runbook.md#sustained-unready-alerts), and [startup diagnostics](startup-diagnostics.md#self-diagnosing-gateway-failures-readyz-gatewayfailure) |
| Fire message | `two-bot-next ALERT gateway_failure:<class>: gateway task failed (<phase>:<class>). Runbook: <full runbook URL>` |
| Resolve message | `two-bot-next RESOLVED gateway_failure:<class>.` |

Delivery notes for the installer (matches the existing readiness monitor):

- Persist the transition before notifying; at most one fire and one resolve
  per incident, no re-fire while the key is firing.
- Resolve when the window holds zero matching lines; confirm with one
  `/readyz` 200 before calling the incident recovered.
- A startup failure typically emits exactly one line: the 15 s linger is
  shorter than the 60 s tick. A running-gateway failure lingers 75 s, so one
  or two keepalive ticks land in it and each re-emits the line; the dedupe is
  by firing key, so the incident still pages once.
- A short streak that clears below the window still paged once; that is
  intended, because the process already restarted.
- Class changes the diagnosis, not the severity. First checks:
  `store_unavailable`, `gateway_pool_connect_failed`, `checkpoint_load_failed`
  point at the store/database path; `onboarding_gates_invalid`,
  `onboarding_init_failed`, `custom_commands_init_failed`,
  `milestones_load_failed`, `automod_config_invalid`, `automod_executor_failed`,
  `raid_executor_failed` point at bootstrap/config from the last deploy;
  `gateway_runtime_failed`, `gateway_task_panicked` point at the running shard
  (see [common failures](runbook.md#common-failures)).

## Dashboard queries

Read-only. Open the affected environment's Workers Logs in the Cloudflare
dashboard (7-day retention) or the Query Builder over the same dataset, and
filter to the incident window. All filters below are literal matches on the
catalogued field names.

Q1 — any gateway failure in the window:

```text
event="container_gateway_failure"
```

Zero lines means not observed, never proven quiet: the keepalive emits only
while the gateway task is failing, and retention is 7 days.

Q2 — breakdown by class (repeat per class, or group by `class`):

```text
event="container_gateway_failure" class="checkpoint_load_failed"
```

Replace the value with each observed token from the table above. Record the
count per class; the class with the count is the diagnosis branch.

Q3 — latest line with both tokens (confirms the full shape):

```text
event="container_gateway_failure" phase="durable_gateway"
```

Each returned line carries `phase="durable_gateway"` plus one `class` token.
A line matching Q1 but not Q3 means vocabulary drift (a new phase); escalate
to the vocabulary owner instead of acting on the class.

Q4 — phase drift (should return nothing today):

```text
event="container_gateway_failure" NOT phase="durable_gateway"
```

Any hit is a new phase the catalog does not list; file it as a catalog gap
with the observed value, not as a routine failure.

Q5 — correlate with the existing readiness signal over the same window:

```text
event="container_unready_alert"
event="container_unready_recovery"
```

A `container_gateway_failure` line followed by `container_unready_alert`
means the same outage tripped both; a failure line with no unready alert
means the streak cleared below its threshold (the gateway still restarted).
Confirm recovery with `/readyz` 200, not with `/health` alone.

## Installer checklist (not done here)

- Add `gateway_failure` to `RULES` in `wrangler/src/alert-rules.ts` with
  `runbook: "runbook.md#alert-gateway-failure"`, plus the `<class>` subject
  evaluation and packet spelling.
- Add the `Alert: gateway failure` runbook section carrying the copy above
  (threshold, severity, first checks, escalation).
- Extend the Worker fixture tests (rule evaluation, anchor check, hostile
  class rejection) mirroring `alert-rules.test.ts` and `container.test.ts`.
- Prove delivery on staging with the operator-held webhook; no credential
  step is authorized by this page.

## Limits

- No threshold is installed here and no secret is provisioned.
- No live paging test was run from this change.
- No `/ops/metrics` scrape is involved; this event has no counter series.
- Counters reset on process restart; a zero delta means restart or quiet.
- Absence of a class in a window is not proof of a regression: classes log
  only when their step fails.
