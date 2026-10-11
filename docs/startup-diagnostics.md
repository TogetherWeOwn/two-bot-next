# Startup diagnostics and Neon URL compatibility

The runtime accepts Neon's libpq `channel_binding` query option, including
URL-decoded and repeated keys. The shared `database_url::connect_options` boundary
validates every key, then removes **all** `channel_binding` pairs before SQLx 0.9
parses the URL. Neither keys' values nor raw driver errors are logged. Unknown
options still fail closed; SSL-mode validation and TLS configuration are unchanged.

This is compatibility handling, **not** enforcement of SCRAM channel binding:
SQLx does not implement libpq's option. In particular, `channel_binding=require`
is not a security guarantee here. `sslmode=require` is preserved.

## Fatal process diagnostics

Configured failures remain fatal; the process exits 1 after supervisor cleanup.
Missing token, URL, or guild prerequisites still park the gateway and return
readiness 503, never fabricated 200.

Rust logs fixed `startup_phase` / `error_class` fields before fatal exits:

| Boundary | Error class |
| --- | --- |
| HTTP listener bind | `listener_bind_failed` |
| Invalid gateway override | `gateway_override_invalid` |
| Database URL validation, options, or connection | `database_connect_failed` |
| Gateway task: shared store missing | `store_unavailable` |
| Gateway task: checkpoint pool connect | `gateway_pool_connect_failed` |
| Durable gateway checkpoint read | `checkpoint_load_failed` |
| Onboarding gate parsing | `onboarding_gates_invalid` |
| Onboarding runtime initialization | `onboarding_init_failed` |
| Custom-command registry bootstrap | `custom_commands_init_failed` |
| Milestone read | `milestones_load_failed` |
| Automod configuration rejected | `automod_config_invalid` |
| Automod REST executor build | `automod_executor_failed` |
| Running gateway operation | `gateway_runtime_failed` |
| Gateway task panicked | `gateway_task_panicked` |
| HTTP/gateway supervisor termination | `container_service_failed` |

These are operation classes, not raw database error codes or inferred root causes.
No SQLx source chain, URL, credential, or arbitrary exception text is formatted.

## Self-diagnosing gateway failures (`/readyz` `gateway_failure`)

Container stdout is not in the `cloudflare-workers` Workers Logs dataset (it is in
the `containers` one, see [Reading container stdout](#reading-container-stdout)),
so the gateway task's class is also published where a probe can read it. The gateway task rows above (`store_unavailable`
through `gateway_task_panicked`) are an enum in `crates/bot/src/gateway_failure.rs`;
no free-form text can reach the field. After a failure `/readyz` adds

```json
{"gateway_failure":{"phase":"durable_gateway","class":"checkpoint_load_failed"}}
```

(omitted while there is none) and keeps serving for 75 seconds
(`shutdown::FAILURE_LINGER`) before the drain and exit-1 restart. A shutdown
signal ends the linger at once. Readers:

- Worker keepalive: one `console.warn` JSON line
  `{"event":"container_gateway_failure","phase":…,"class":…}` per tick, only when
  both values are `[a-z0-9_]{1,32}` tokens, so it reaches Workers Logs.
- `scripts/staging_rollout.py`: the rollout gate's `last observation before
  timeout` line ends with `gateway_failure=<phase>:<class>`. The gate also
  probes `/readyz` while this build's rollout exists but has not converged,
  accepting the value only from the expected Worker version and build identity.

`database_init`, `listener_bind` and the other pre-gateway exits happen before the
HTTP server runs, so they remain visible only in container logs.

## Reading container stdout

The Rust process's stdout/stderr (the JSON lifecycle lines in
[`logging.md`](logging.md), including `durable gateway failed` with its
`error_class`) reaches Workers Observability as its own **`containers`** dataset,
not the Worker's `cloudflare-workers` one. The `service` is the Container
application's id, not the Worker name. A read-only `CLOUDFLARE_API_TOKEN` can query
it with the same call as the Worker logs
(`POST /accounts/<id>/workers/observability/telemetry/query`); only the dataset
differs:

```json
{"queryId":"<uuid>","timeframe":{"from":<ms>,"to":<ms>},"view":"events","limit":100,
 "parameters":{"datasets":["containers"],"filters":[],"calculations":[],"groupBys":[],
               "orderBy":{"value":"timestamp","order":"desc"}}}
```

Structured fields are filterable by key (`error_class exists`, `startup_phase exists`);
the message is `$metadata.message`. Each process start logs `listening` first, with a
fresh `run_id`, so one run's events share a `run_id` and a start-to-ready time is
`listening` to `gateway ready; checkpoint committed`. Telemetry returns the newest 100
events per query: keep windows under a few minutes around a deploy. Lines logged before
the JSON format (`logging.md`) arrive as one ANSI-coloured text message each.

## Gateway bootstrap and the send lane

After a start, `custom_commands_init_failed` or `gateway_runtime_failed` within
about ten seconds of `listening`, followed 15 s later by `container service
failed`, was the durable send lane being held while the gateway's first reads
asked for it (see [send admission](discord-send-admission.md#boot-window)): each
refusal cost a full container restart (about 25-30 s) and a staging redeploy
needed two or three. The bootstrap reads and registry publish now wait out a held
lane inside the boot window. A wait logs one warn, `boot send admission was held;
waited for the lane`, with `blocked_attempts`, `waited_ms` and `recovered`; a read
that still fails logs `gateway bootstrap read failed` with the read (`step`) and a
fixed `cause` token (`admission_blocked`, `timeout`, `rate_limited`,
`unavailable`, `rejected`, `guard_*`).

## Worker readiness boundary

The Worker returns JSON HTTP 500 on a Container/DO fetch failure:

```json
{"ready":false,"error_class":"container_unavailable"}
```

The installed Container SDK 0.3.7 can either reject a fetch **or return a
synthetic text response built from the startup exception**: 500, 429 carrying the
raw exception message, or 503 when no instance is available. The DO therefore
forwards only the bot's own probe answers, HTTP 200 or a parked readiness 503
with `application/json` content type. Every other status or media type, whether
from the SDK or from the container port, is drained and replaced with the JSON
500 above. The application lifecycle hook logs/throws only
`container_lifecycle_failed`, without an exception-derived message or cause.

Rust stderr is **not** a structured SDK diagnostic channel. The Worker therefore
cannot distinguish `database_connect_failed` from another container startup or
proxy failure; `container_unavailable` is deliberately coarse. Consult the fixed
Rust class in container logs for the internal operation. SDK/platform diagnostics
outside the application's hook are not claimed to be controlled by this wrapper.

## Regression evidence boundaries

- Core URL tests preserve `sslmode=require`, reject unknown parameters and invalid
  SSL mode, and remove decoded/repeated channel-binding keys.
- Cutover tracing-capture tests parse synthetic Neon URLs without opening a socket,
  and prove the removed values do not reach SQLx unknown-parameter warnings.
- The startup binary test rejects an unknown synthetic URL option before connecting,
  requires exit 1 and fixed diagnostic classes, and checks secret sentinels.
- Worker tests execute the installed SDK against in-memory storage/TCP doubles;
  native startup throws are converted by the SDK to 500, 429 or text 503 and
  sanitized by the DO. Container-port responses other than 200 or JSON 503 are
  also sanitized, while a JSON 503 (with or without a charset) is forwarded.
  A separate Worker/DO rejection test covers the outer RPC boundary.

These tests are not a live Rust → Cloudflare crash reproduction, a deployment,
staging recovery, or migration acceptance. Staging schema migration is a separate
operator-owned operation. Production databases are never test targets.
