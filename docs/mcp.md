# Bot HTTP MCP server (Streamable HTTP, read-only tools first)

Stateless MCP endpoint at `POST /mcp` on the Worker (ops host). No session
state, no SSE stream, no McpAgent/Durable Object session: each request is one
JSON-RPC message answered and closed. See `wrangler/src/mcp.ts` (handler) and
the `/mcp` route in `wrangler/src/index.ts` (method + bearer gate).

## Tools (phase 1, read-only)

| Tool | Source | Notes |
|---|---|---|
| `get_status` | `GET http://c/health` | Single bounded container read |
| `get_gateway` | `GET http://c/readyz` | Returns gateway excerpt only |
| `get_backfill_progress` | `GET http://c/readyz` | `{"implemented": false, "source": "readyz.jobs", "jobs": {...}}`; no backfill JSON API exists yet |
| `get_config` | Worker snapshot | Redacted non-secret vars only; never secrets |
| `get_audit` | Container `GET /ops/audit` | `{available, rows, truncated}`; newest 50 operational audit rows, allowlisted fields only, at most 64 KiB |

Inbound intake is bounded separately from container reads: after the bearer
and rate gates, at most 64 KiB of request bytes are collected under a
six-second body deadline before JSON parsing. The byte count is measured on
the bytes actually read, never on Content-Length alone. Oversize,
deadline-expired, aborted/errored and malformed bodies all produce the one
fixed non-echoing `-32700 parse error` refusal (`id: null`), and no container
request occurs. Cancellation is attempted once without awaiting its result;
reader locks are released where legal and the intake timer and client-abort
listener are removed. A stalled or rejecting cleanup hook cannot delay the
refusal or replace it with an exception, and the tail is never drained.

Every container-backed tool call is bounded: 6 s timeout plus a 64 KiB body
cap (same bound as the metrics scrape). Oversize surfaces a generic
`-32000` JSON-RPC error without echoing upstream bytes; timeouts and
non-bot upstreams surface a generic `-32000` without proxied error text.
The outbound budget covers headers and body together, separately from intake:
a valid intake can use nearly 6 s and then a healthy container read nearly
another 6 s. Neither chunks nor cleanup renew either deadline. Rejected or
aborted headers and incomplete/oversize bodies use best-effort non-awaited
cancellation with rejection handling; reader cleanup is not retried by the
caller. Cleanup failures never override the fixed tool failure.
`/readyz` accepts 200 and its routine 503 JSON (any component not ready)
only when the body is a readiness report: `components` must be an array and
**every row** must be an exact two-element `[name, status]` pair with a string
name and status exactly `ready`, `starting` or `down`. Both `get_gateway` and
`get_backfill_progress` refuse the whole report with the same generic `-32000`
error if any row is malformed, with no result or echoed upstream content.
Arbitrary component names, extra report fields and empty component lists remain
valid; no gateway component is required. Ownership-fence and junk bodies
(e.g. `{error:"ownership_fenced"}`), other statuses, and non-JSON stay failures.
`get_gateway` reads the `[name, status]` pairs in `components` plus `gateway_failure`.
`get_config.build_revision` is the Worker version (`CF_VERSION_METADATA.id`).
`initialize` echoes only a supported protocol version, else the default.
Unknown tool names return `-32601`. `GET /mcp` returns `405` with
`Allow: POST` and never executes tools. Client stream close aborts the
in-flight container fetch via the request signal.

## Request validation (before dispatch)

Every parsed body is validated before any Durable Object resolution or
container read. Malformed envelopes (non-object/array/batch roots, wrong
`jsonrpc`, non-string `method`, object/array/boolean `id`) return the fixed
`-32600 invalid request`; malformed `params` (anything but an omitted or
plain-object value) and malformed `tools/call` fields return the fixed
`-32602 invalid params`. Refusals echo only string/number correlation IDs
(`0` and `""` included), otherwise `null`; they never echo untrusted input
or credentials. All five tools take no arguments: `arguments` must be
omitted or `{}`, and any scalar, array, null, or unexpected-key value
(including falsy extras and an own `__proto__` key) is refused with
`-32602`. A missing or non-string tool `name` is `-32602`; a well-formed
but unknown name stays `-32601 unknown tool`. Harmless top-level padding
and extra object keys in method `params` remain allowed.

Notification policy is derived from the envelope as well as the method: a
missing or `null` ID on an ordinary method (`initialize`, `tools/list`,
`tools/call`, `ping`) is `-32600`, while a missing or `null` ID on a
`notifications/*` method with well-formed params is an empty `202` with no
dispatch (malformed params there are `-32602`, not a silent accept). Any
string/number ID — including `0` and `""` — makes a `notifications/*`
method a request, answered `-32601 method not found` with the ID
preserved; it never silently returns `202`.

## Audit excerpt boundary

The container excerpt is a fixed, read-only `SELECT` from
`operational_audit_log`, ordered by `created_at DESC, entry_id DESC`.
It reads at most 51 rows in one snapshot (50 returned, the extra row determines
`truncated`) under a two-second database deadline. It never loads metadata,
delivery nonces, claim tokens, error text or message bodies. Fields are limited
to entry identity, kind, guild/member/channel/message IDs, UTC occurrence time
(`YYYY-MM-DDTHH:mm:ss.sssZ`, including `.000Z` for whole seconds), a
fixed-vocabulary action and delivery state. Oversized or invalid stored
facts fail closed to `{available:false, rows:[], truncated:false}`; database
absence, failure and timeout use that same unavailable result. No row claims,
delivery updates or Discord calls occur. Responses are `no-store`; only GET
is served. This is a global operator excerpt, not a tenant-facing API.

The MCP handler stamps an internal dispatch header on its fixed
`http://c/ops/audit` GET. The singleton DO requires the current deployment's
ownership fence before it reads the container. Public ingress strips this
header, so the existing public `/ops/audit` remains the separate DO control
history behind `OPS_CONTROL_TOKEN`; the MCP bearer cannot read that admin
surface. Neither the caller bearer nor dispatch header reaches Rust.

This uses the existing private container binding and listener, **not a new
OS-loopback-only listener**. The shared Rust health listener binds all container
interfaces so the Container SDK can reach it. No public Worker route forwards
to this excerpt. A literal loopback bind would need a separate in-container
relay; it is not provided by this change.

The Worker accepts only exact 200 JSON audit envelopes with the three expected
keys, at most 50 rows, the eleven allowlisted row keys and bounded typed values.
Schema drift, non-JSON, oversize, aborts and read errors produce generic
JSON-RPC errors, never a partial excerpt or upstream error text.

## Auth (phase 1: shared `mcp:read` bearer; staging target: OAuth resource server)

Phase 1 enforces a shared bearer secret (`MCP_TOKEN` Worker secret, minimum
32 characters like the scrape token):

- Unconfigured or short token → `404` (the route does not exist).
- Missing or wrong bearer → `401` with `WWW-Authenticate: Bearer`, before
  the Container is touched (constant-time digest comparison, per-caller
  10-burst/1-per-second buckets shared with nothing else).
- The caller bearer is never forwarded to the container; only the
  Worker-stamped deployment id header crosses the fence.

Set the secret per environment (never in `wrangler.toml`):

```sh
wrangler versions secret put --env staging MCP_TOKEN --message <sha>
wrangler versions deploy --env staging
```

Stamp with the live `/readyz` `build_revision` of staging: a plain `wrangler secret put`
publishes an unstamped version that can never form a rollback receipt
(see `docs/production-deploy.md`). Precheck first: `versions secret put` builds on the
newest-created version, not necessarily the serving one — confirm the last block of
`wrangler versions list --env staging` (the list prints oldest first, so the last
block is the newest version) is the serving version (same id as
`wrangler deployments status --env staging`), and if they differ redeploy the intended
SHA through the workflow first (full rule in `docs/production-deploy.md`).

Staging target (follow-up): `workers-oauth-provider` in resource-server
mode with dynamic client registration, PKCE, and scopes. Phase-1 scope is
`mcp:read` (all five tools). Control tools are deferred behind a step-up
scope plus a control token plus an audit write, and no control tool is
registered until that lands.

## Staging prove-out

1. Deploy this branch to staging and confirm `GET /mcp` → `405`.
2. Confirm unauthenticated `POST /mcp` → `401` with `WWW-Authenticate`, and
   the container shows no fetch.
3. With the staging `MCP_TOKEN`: `initialize` → `protocolVersion`,
   `tools/list` → exactly the five tools, `tools/call get_status` → the
   container `/health` JSON.
4. `get_audit` reads the real container excerpt. The manual
   `staging-mcp-proveout` workflow checks its `{available, rows, truncated}`
   envelope, including explicit unavailable results. While staging is fenced,
   all four container-backed tools may return the expected `-32000` refusal;
   `get_config` must still succeed Worker-side. The backfill tool remains
   explicitly unimplemented; its readiness jobs excerpt is not backfill progress.

No dashboard or admin UI duplication: MCP reuses the same container read
APIs and never adds UI.

### Proof admission contract (offline-verifiable)

`scripts/staging_mcp_proveout.py` admits the original bounded response, not
its redacted display. Duplicate decoded JSON keys at any depth fail with
`JSON_DUPLICATE_KEY`, even when values match or names use different escapes.
Recording keeps bounded redacted text, never a last-key-wins dictionary.

The initialize request offers `2025-06-18`; either shipped string version
(`2025-06-18` or `2025-03-26`) may negotiate. Missing, non-string, unknown or
refused negotiation fails with `PROTOCOL_UNSUPPORTED`. Tool discovery must
advertise exactly the five unique names above, each with the shipped
`inputSchema`: `type: "object"`, empty `properties`, and literal boolean
`additionalProperties: false`. Missing or incompatible schemas fail with
`TOOL_SCHEMA_INVALID`. An optional empty `required` list and nonasserting
metadata/unused definitions remain compatible; unknown schema constraints
are refused rather than interpreted by a generic schema engine.

All authenticated replies retain JSON-RPC 2.0, exact request-ID and exclusive
result/error framing checks. Tool-content compatibility (including empty
lists), the audit excerpt bounds, explicit fence corroboration, body/depth
caps, bearer redaction, exact staging URL and redirect refusal are unchanged.
Failures retain the probe's stage ID, a fixed reason code and bounded `next:`
action; exit 1 agrees with artifact `proof_disposition: "failed"`. Exit 0
requires no failures and `"proved"`; preflight refusal exits 2 without sending.
A recorded response alone is not proof, and offline checks are not live or
installed acceptance.

Run the entire offline proof family (synthetic responses; no live service or
credentials, with localhost-only transport controls):

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/ci -p 'test_staging_mcp_proveout*.py' -v
```
