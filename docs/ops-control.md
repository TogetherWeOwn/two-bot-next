# Ops control audit contract

`GET /ops/audit` reads the Durable Object's append-only admin change history;
[it is not the MCP container audit excerpt](mcp.md). It requires the existing
`OPS_CONTROL_TOKEN` bearer and current deployment fence. Both mutation switches
may stay off. No query parameters, repair, writes, deletes, new permissions or
runtime enforcement are introduced by this read.

## Successful read

HTTP 200, JSON, `Cache-Control: no-store`:

```json
{"deploymentId":"fixture","rows":[{"id":"two-bot:ops-audit:v1:0000000000000001","actor":"operator","epoch":1,"timestamp":"2026-10-03T00:00:00.000Z","action":"config_put","detail":{"overrides":{"TWO_AUTOMATIONS":"0"}}}],"truncated":false}
```

Only `id`, `actor`, `action`, `epoch`, `timestamp` and `detail` are exposed per
row. IDs are the existing storage keys (16-digit, positive safe sequence), not
stored row IDs. Rows are ordered by descending append key, never timestamp or
per-control epoch. Repeated reads of unchanged storage return the same IDs.

Validation matches writer-supported values: the actor grammar is
`[a-zA-Z0-9_.:@/-]{1,128}`, epochs are positive safe integers, timestamps are
canonical `Date.toISOString()` strings, and detail matches the action:

- `config_put`: only `detail.overrides`, using the forwarded flag allowlist,
  string values at most 4096 characters, printable ASCII or tab (empty allowed).
- `kill_switch`: only `detail.fences`, with exactly the boolean keys
  `automation`, `automod`, `publish`, `gateway_park`.

Unknown row/detail fields are discarded; invalid supported fields, unknown
override/fence keys or mixed action details refuse the listing. Stored content
never supplies an arbitrary response object. The full bounded listing is
validated before selecting output, including its completeness lookahead.

The **entire UTF-8 encoded JSON body is at most 65,536 bytes**, including the
deployment ID, IDs, escapes, commas and metadata. At most 100 whole rows are
returned. A 101-row storage window distinguishes complete 100-row history from
older history. `truncated=true` means rows were omitted by the row or byte cap;
`false` means the bounded listing proves completeness. This is not a cursor or
a full-history export. The longest fitting newest-first prefix is returned:
no string/JSON clipping and no skipping an oversized newest row to show older
ones. Such a newest row yields `rows:[]` with `truncated:true`.

## Refusals

A malformed row anywhere in the listed window refuses the entire read, never a
successful partial prefix. Fixed HTTP 503 JSON, no-store, under 1024 bytes:

| `error` | `reason` and `code` | `next_action` | Operator response |
| --- | --- | --- | --- |
| `ops_unavailable` | `storage_invalid` | `escalate_storage` | Preserve evidence and escalate for authorized storage investigation; do not repair/delete via this read. |
| `ops_unavailable` | `storage_unavailable` | `retry_read` (GET reads) | Retry only the read under the existing access policy. |
| `ops_unavailable` | `storage_unavailable` | `reconcile_then_retry` (PUT/POST mutations) | Re-read state, reconcile the intended change, then retry the mutation. |

The `storage_unavailable` token is scoped by operation kind so a machine
consumer never answers a failed fence change with a plain read retry that
would not re-apply it. Mutation refusals carry the same fixed `code` field
as reads. Neither refusal includes rows, exception text or raw stored
content in JSON or logs. An oversized deployment envelope alone refuses with fixed
`reason=code=response_too_large` and `next_action=escalate_deployment` (investigate
its deployment metadata under the existing policy); no oversized successful body is sent. Existing
bearer, deployment, method and mutation refusals remain in force.

Stored config/kill-switch records and mutation replies still report
`enforcement=pending`: neither audit readback nor a successful mutation proves
that the Rust runtime applied the override. Epoch conflicts, atomic appends,
sequence allocation and both mutation switches are unchanged.

## Offline verification

From the repository root, with locked dev dependencies installed:

```sh
node --import ./wrangler/test/cloudflare-loader.mjs --test wrangler/test/ops-control.test.ts wrangler/test/ops-audit-window.test.ts
npm --prefix wrangler run typecheck
npm --prefix wrangler test
```

The protected ops-control tests exercise actual DO `Response` bytes, independent
65,535/65,536/65,537-byte escape-heavy fixtures, hostile extra fields, malformed
rows, list rejection, row boundaries and zero storage writes. The window tests
cover invalid append keys/canonical timestamps, invalid lookahead, multibyte
metadata, a too-large empty envelope, after-drop exact-cap byte boundaries,
kill_switch mixed-detail/extra-fence refusals and operation-scoped mutation
refusals through actual DO fetches. All use offline storage doubles, not
live services. These checks prove source behavior, not installed deployment.

Source: [`ops-control.ts`](../wrangler/src/ops-control.ts),
[`DO route`](../wrangler/src/index.ts).
