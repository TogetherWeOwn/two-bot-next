/**
 * Failing-first acceptance for a real, bounded `get_audit` excerpt.
 * Test files only — no src changes. The Builder starts from this head and
 * must not edit this file.
 *
 * Contract under test (builder: implement exactly this):
 * - `tools/call get_audit` performs exactly one bounded container read,
 *   `GET http://c/ops/audit`, through the fenced singleton DO (deployment
 *   header stamped, caller bearer never forwarded) and returns the
 *   container excerpt as the tool result, verbatim and unaugmented.
 * - The excerpt shape is `{available: boolean, rows: [...], truncated:
 *   boolean}`; each row carries only the allowlisted id/metadata-free
 *   fields from the container endpoint (entry_id, kind, guild_id,
 *   occurred_at, actor_id, target_id, source_channel_id,
 *   destination_channel_id, message_id, action, delivery_state). The
 *   `{implemented: false}` stub outcome never occurs, and `tools/list`
 *   no longer describes `get_audit` as a stub.
 * - An unavailable excerpt (`available: false`, empty rows) is a normal
 *   success: never an error, never the stub marker, never faked rows.
 * - Malformed container answers fail closed like the other tools:
 *   non-bot statuses, non-JSON bodies, JSON that is not an audit excerpt
 *   (no rows array), and bodies over the existing 64 KiB outbound cap all
 *   surface a generic JSON-RPC error with no result and no upstream bytes
 *   echoed.
 * - The existing `/mcp` bearer and per-caller rate gates run before any
 *   container byte is read: refused callers resolve no DO and forward
 *   nothing, even for `get_audit`.
 *
 * Synthetic harness only, following wrangler/test/mcp.test.ts: the stub
 * TWO_BOT records what the Worker forwards; no endpoint, database or
 * secret is contacted.
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import worker, { type Env } from "../src/index.ts";

const MCP_TOKEN = "synthetic-mcp-audit-bearer-token-0123456789";

interface Forwarded {
  method: string;
  url: string;
  headers: Record<string, string>;
  body: string;
}

function harness(respond: (req: Request) => Response | Promise<Response>) {
  const forwarded: Forwarded[] = [];
  const names: string[] = [];
  const env = {
    REDIRECT_MAPPINGS_JSON: "[]",
    REDIRECT_FALLBACK_CODE: "fallbackCode",
    // The ownership fence refuses forwards without a deployment id.
    CF_VERSION_METADATA: { id: "synthetic-mcp-audit-deployment" },
    MCP_TOKEN,
    TWO_BOT: {
      getByName: (name: string) => {
        names.push(name);
        return {
          fetch: async (r: Request) => {
            const headers: Record<string, string> = {};
            r.headers.forEach((value, key) => {
              headers[key] = value;
            });
            forwarded.push({ method: r.method, url: r.url, headers, body: await r.text() });
            return respond(r);
          },
        };
      },
    },
  } as unknown as Env;
  const ctx = { waitUntil() {} } as unknown as ExecutionContext;
  return { env, ctx, forwarded, names };
}

let rpcCallerSeq = 0;
const rpc = (method: string, params: unknown, auth: string | null = `Bearer ${MCP_TOKEN}`) => {
  // mcpAuthBuckets is keyed by cf-connecting-ip (fallback "unknown") with a
  // 10-burst/1-per-second budget: isolate callers per request so budgets
  // never bleed across cases. The cap itself is pinned by the 429 test
  // below, from its own fixed address.
  rpcCallerSeq += 1;
  return new Request("https://probe.invalid/mcp", {
    method: "POST",
    headers: {
      ...(auth ? { authorization: auth } : {}),
      "content-type": "application/json",
      "cf-connecting-ip": `198.51.100.${(rpcCallerSeq % 200) + 1}`,
    },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
  });
};

const callGetAudit = (auth: string | null = `Bearer ${MCP_TOKEN}`) =>
  rpc("tools/call", { name: "get_audit", arguments: {} }, auth);

async function rpcBody(res: Response): Promise<any> {
  assert.match(res.headers.get("content-type") ?? "", /application\/json/);
  return (await res.json()) as any;
}

/** Two newest-first excerpt rows with exactly the allowlisted fields. */
const AUDIT_ROWS = [
  {
    entry_id: "synthetic-audit-entry-2",
    kind: "voice_join",
    guild_id: "424242",
    occurred_at: "2026-10-19T00:00:02.000Z",
    actor_id: "111111",
    target_id: "111111",
    source_channel_id: null,
    destination_channel_id: "222222",
    message_id: null,
    action: null,
    delivery_state: "delivered",
  },
  {
    entry_id: "synthetic-audit-entry-1",
    kind: "message_delete",
    guild_id: "424242",
    occurred_at: "2026-10-19T00:00:01.000Z",
    actor_id: null,
    target_id: "333333",
    source_channel_id: "444444",
    destination_channel_id: null,
    message_id: "555555",
    action: null,
    delivery_state: "none",
  },
];

const AUDIT_EXCERPT = { available: true, rows: AUDIT_ROWS, truncated: false };

test("tools/call get_audit proxies one container GET to /ops/audit and returns the excerpt", async () => {
  const h = harness(() => Response.json(AUDIT_EXCERPT));
  const res = await worker.fetch(callGetAudit(), h.env, h.ctx);
  assert.equal(res.status, 200);
  const body = await rpcBody(res);
  assert.equal(body.error, undefined);
  assert.deepEqual(
    body.result?.structuredContent,
    AUDIT_EXCERPT,
    "get_audit must return the container excerpt verbatim, not the implemented=false stub",
  );
  assert.equal(h.forwarded.length, 1, "one tool call proxies exactly one container fetch");
  assert.equal(h.forwarded[0]!.method, "GET", "the audit read is a GET, never a mutation");
  assert.equal(
    new URL(h.forwarded[0]!.url).pathname,
    "/ops/audit",
    "get_audit reads the container audit endpoint",
  );
  assert.equal(h.forwarded[0]!.body, "", "a read-only GET forwards no body");
  assert.equal(
    h.forwarded[0]!.headers["authorization"],
    undefined,
    "caller bearer must not be forwarded",
  );
});

test("tools/call get_audit returns an unavailable excerpt as a plain success, never the stub", async () => {
  const unavailable = { available: false, rows: [], truncated: false };
  const h = harness(() => Response.json(unavailable));
  const res = await worker.fetch(callGetAudit(), h.env, h.ctx);
  assert.equal(res.status, 200);
  const body = await rpcBody(res);
  assert.equal(body.error, undefined, "unavailability is data, not a transport failure");
  assert.deepEqual(body.result?.structuredContent, unavailable);
  assert.equal(h.forwarded.length, 1, "even the empty answer comes from a real container read");
});

test("tools/list no longer describes get_audit as an implemented=false stub", async () => {
  const h = harness(() => Response.json(AUDIT_EXCERPT));
  const res = await worker.fetch(rpc("tools/list", {}), h.env, h.ctx);
  assert.equal(res.status, 200);
  const body = await rpcBody(res);
  const getAudit = (body.result?.tools as { name: string; description?: string }[] | undefined)?.find(
    (t) => t.name === "get_audit",
  );
  assert.ok(getAudit, "get_audit stays registered");
  assert.ok(
    !(getAudit.description ?? "").includes("implemented=false"),
    `get_audit description must describe the real excerpt, got: ${getAudit.description}`,
  );
});

test("tools/call get_audit fails closed on malformed container answers", async () => {
  const marker = "synthetic-audit-malformed-marker";
  const cases: { name: string; respond: () => Response }[] = [
    {
      name: "500 JSON stays a failure",
      respond: () => Response.json({ available: true, rows: AUDIT_ROWS, marker }, { status: 500 }),
    },
    {
      name: "non-JSON 200 stays a failure",
      respond: () => new Response(marker, { status: 200, headers: { "content-type": "text/plain" } }),
    },
    {
      name: "JSON without a rows array stays a failure",
      respond: () => Response.json({ available: true, excerpt: marker }),
    },
    {
      name: "JSON array instead of excerpt object stays a failure",
      respond: () => Response.json([marker]),
    },
  ];
  for (const c of cases) {
    const h = harness(c.respond);
    const res = await worker.fetch(callGetAudit(), h.env, h.ctx);
    assert.equal(res.status, 200);
    const text = await res.text();
    assert.ok(!text.includes(marker), `${c.name}: upstream bytes must never be echoed`);
    const body = JSON.parse(text) as any;
    assert.ok(body.error, `${c.name}: must surface a JSON-RPC error`);
    assert.equal(body.result, undefined, `${c.name}: must carry no result`);
    assert.equal(h.forwarded.length, 1, `${c.name}: the container read must have been attempted`);
  }
});

test("tools/call get_audit: stub body over 64 KiB -> JSON-RPC error with no upstream text echoed", async () => {
  const marker = "synthetic-audit-oversize-marker";
  const h = harness(() => new Response(`${marker}${"x".repeat(64 * 1024)}`));
  const res = await worker.fetch(callGetAudit(), h.env, h.ctx);
  assert.equal(res.status, 200);
  const text = await res.text();
  assert.ok(!text.includes(marker), "oversize upstream text must never be echoed");
  const body = JSON.parse(text) as any;
  assert.ok(body.error, "oversize upstream must surface a JSON-RPC error");
  assert.equal(body.result, undefined);
  assert.equal(h.forwarded.length, 1, "the bounded container read must have been attempted");
});

test("get_audit: missing/invalid bearer -> 401 before any container read", async () => {
  const h = harness(() => Response.json(AUDIT_EXCERPT));
  for (const auth of [null, "Bearer wrong-bearer-value"] as const) {
    const res = await worker.fetch(callGetAudit(auth), h.env, h.ctx);
    assert.equal(res.status, 401);
    assert.match(res.headers.get("www-authenticate") ?? "", /Bearer/);
    await res.text();
  }
  assert.deepEqual(h.names, [], "unauthenticated get_audit must not resolve the DO");
  assert.deepEqual(h.forwarded, [], "unauthenticated get_audit must never reach the container");
});

test("get_audit: exhausted per-caller budget -> 429 and no further container read", async () => {
  const h = harness(() => Response.json(AUDIT_EXCERPT));
  // Own TEST-NET-3 caller, far from the rpc() pool: 10-burst, so the 11th
  // rapid get_audit is shed by the rate gate before intake or any forward.
  const fixedGetAudit = () =>
    new Request("https://probe.invalid/mcp", {
      method: "POST",
      headers: {
        authorization: `Bearer ${MCP_TOKEN}`,
        "content-type": "application/json",
        "cf-connecting-ip": "203.0.113.77",
      },
      body: JSON.stringify({
        jsonrpc: "2.0",
        id: 1,
        method: "tools/call",
        params: { name: "get_audit", arguments: {} },
      }),
    });
  for (let i = 0; i < 10; i++) {
    const res = await worker.fetch(fixedGetAudit(), h.env, h.ctx);
    assert.equal(res.status, 200, `admitted get_audit ${i + 1} must be served`);
    await res.text();
  }
  assert.equal(h.forwarded.length, 10, "each admitted get_audit performs one container read");
  const shed = await worker.fetch(fixedGetAudit(), h.env, h.ctx);
  assert.equal(shed.status, 429, "11th rapid get_audit on a 10-burst must be shed");
  assert.match(shed.headers.get("retry-after") ?? "", /[0-9]/);
  await shed.text();
  assert.equal(h.forwarded.length, 10, "the shed caller must not read the container again");
});
