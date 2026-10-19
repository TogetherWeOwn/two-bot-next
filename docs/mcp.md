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
| `get_audit` | Stub | `{"implemented": false}`; real audit read is follow-up work, never faked data |

Inbound intake is bounded separately from container reads: after the bearer
and rate gates, at most 64 KiB of request bytes are collected under a
six-second body deadline before JSON parsing. The byte count is measured on
the bytes actually read, never on Content-Length alone. Oversize,
deadline-expired, aborted/errored and malformed bodies all produce the one
fixed non-echoing `-32700 parse error` refusal (`id: null`), the body reader
is cancelled/released, and no container request occurs.

Every container-backed tool call is bounded: 6 s timeout plus a 64 KiB body
cap (same bound as the metrics scrape). Oversize surfaces a generic
`-32000` JSON-RPC error without echoing upstream bytes; timeouts and
non-bot upstreams surface a generic `-32000` without proxied error text.
`/readyz` accepts 200 and its routine 503 JSON (any component not ready)
only when the body is a readiness report (`components` is an array);
ownership-fence and junk bodies (e.g. `{error:"ownership_fenced"}`),
other statuses, and non-JSON stay failures. `get_gateway` reads the `[name, status]`
pairs in `components` plus `gateway_failure`. `get_config.build_revision`
is the Worker version (`CF_VERSION_METADATA.id`).
`initialize` echoes only a supported protocol version, else the default.
Unknown tool names return `-32601`. `GET /mcp` returns `405` with
`Allow: POST` and never executes tools. Client stream close aborts the
in-flight container fetch via the request signal.

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
4. Follow-up Rust read endpoints (`/ops/status`, `/ops/backfill`,
   `/ops/config`, `/ops/audit`) replace the `implemented: false` stubs;
   until then the stubs stay pinned by `wrangler/test/mcp.test.ts`.

No dashboard or admin UI duplication: MCP reuses the same container read
APIs and never adds UI.
