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
| Durable gateway checkpoint read | `checkpoint_load_failed` |
| Milestone read | `milestones_load_failed` |
| Running gateway operation | `gateway_runtime_failed` |
| HTTP/gateway supervisor termination | `container_service_failed` |

These are operation classes, not raw database error codes or inferred root causes.
No SQLx source chain, URL, credential, or arbitrary exception text is formatted.

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
