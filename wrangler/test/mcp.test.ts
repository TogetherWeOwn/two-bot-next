/**
 * TOG-22958: failing-first acceptance for the Bot HTTP MCP server
 * (Streamable HTTP, read-only tools first). Test files only — no src changes.
 *
 * Synthetic harness only, following wrangler/test/health-probes.test.ts: the
 * stub TWO_BOT records what the Worker forwards; no endpoint, database or
 * secret is contacted.
 *
 * Contract under test (builder: implement exactly this on POST /mcp):
 * - Bearer gate: env MCP_TOKEN (synthetic value below). Missing/invalid
 *   bearer -> 401 with WWW-Authenticate, zero container fetches.
 * - Only POST is served: GET /mcp -> 405 with Allow: POST.
 * - JSON-RPC 2.0 bodies: initialize answers with protocolVersion; tools/list
 *   returns exactly get_status, get_gateway, get_backfill_progress,
 *   get_config, get_audit; tools/call proxies one container GET and returns
 *   its JSON.
 * - Upstream bodies over 64 KiB surface a JSON-RPC error with no upstream
 *   text echoed; a hung stub aborts at ~6 s with a JSON-RPC error.
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import worker, { type Env } from "../src/index.ts";

const MCP_TOKEN = "synthetic-mcp-bearer-token-0123456789abcd";
const EXPECTED_TOOL_NAMES = [
  "get_status",
  "get_gateway",
  "get_backfill_progress",
  "get_config",
  "get_audit",
];

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
    CF_VERSION_METADATA: { id: "synthetic-mcp-deployment" },
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

const rpc = (method: string, params: unknown, auth: string | null = `Bearer ${MCP_TOKEN}`) =>
  new Request("https://probe.invalid/mcp", {
    method: "POST",
    headers: {
      ...(auth ? { authorization: auth } : {}),
      "content-type": "application/json",
    },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
  });

async function rpcBody(res: Response): Promise<any> {
  assert.match(res.headers.get("content-type") ?? "", /application\/json/);
  return (await res.json()) as any;
}

test("POST /mcp initialize -> 200 with protocolVersion", async () => {
  const h = harness(() => Response.json({ synthetic: true }));
  const res = await worker.fetch(
    rpc("initialize", {
      protocolVersion: "2025-06-18",
      capabilities: {},
      clientInfo: { name: "synthetic-acceptance", version: "0" },
    }),
    h.env,
    h.ctx,
  );
  assert.equal(res.status, 200);
  const body = await rpcBody(res);
  assert.equal(body.jsonrpc, "2.0");
  assert.ok(
    body.result && typeof body.result.protocolVersion === "string" && body.result.protocolVersion.length > 0,
    "initialize result must carry a protocolVersion",
  );
  assert.equal(body.error, undefined);
});

test("tools/list returns exactly the five read-only tools", async () => {
  const h = harness(() => Response.json({ synthetic: true }));
  const res = await worker.fetch(rpc("tools/list", {}), h.env, h.ctx);
  assert.equal(res.status, 200);
  const body = await rpcBody(res);
  const names = (body.result?.tools as { name: string }[] | undefined)?.map((t) => t.name).sort();
  assert.deepEqual(names, [...EXPECTED_TOOL_NAMES].sort());
});

test("tools/call get_status proxies one container GET and returns its JSON", async () => {
  const marker = "synthetic-get-status-marker";
  const h = harness(() => Response.json({ status: "ok", marker }));
  const res = await worker.fetch(
    rpc("tools/call", { name: "get_status", arguments: {} }),
    h.env,
    h.ctx,
  );
  assert.equal(res.status, 200);
  const body = await rpcBody(res);
  assert.ok(JSON.stringify(body).includes(marker), "tool result must carry the container JSON");
  assert.equal(h.forwarded.length, 1, "one tool call proxies exactly one container fetch");
  assert.equal(h.forwarded[0]!.method, "GET");
  assert.equal(
    h.forwarded[0]!.headers["authorization"],
    undefined,
    "caller bearer must not be forwarded",
  );
});

test("missing/invalid bearer -> 401 with WWW-Authenticate, zero container fetches", async () => {
  const h = harness(() => Response.json({ synthetic: true }));
  for (const auth of [null, "Bearer wrong-bearer-value"] as const) {
    const res = await worker.fetch(
      rpc("initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: {} }, auth),
      h.env,
      h.ctx,
    );
    assert.equal(res.status, 401);
    assert.match(res.headers.get("www-authenticate") ?? "", /Bearer/);
    await res.text();
  }
  assert.deepEqual(h.names, [], "unauthenticated calls must not resolve the DO");
  assert.deepEqual(h.forwarded, [], "unauthenticated calls must never reach the container");
});

test("GET /mcp -> 405 with Allow: POST", async () => {
  const h = harness(() => Response.json({ synthetic: true }));
  const res = await worker.fetch(
    new Request("https://probe.invalid/mcp", {
      headers: { authorization: `Bearer ${MCP_TOKEN}` },
    }),
    h.env,
    h.ctx,
  );
  assert.equal(res.status, 405);
  assert.equal(res.headers.get("allow"), "POST");
  await res.text();
  assert.deepEqual(h.names, [], "refused methods must not resolve the DO");
  assert.deepEqual(h.forwarded, [], "refused methods must never reach the container");
});

test("stub body over 64 KiB -> JSON-RPC error with no upstream text echoed", async () => {
  const marker = "synthetic-oversize-marker";
  const h = harness(() => new Response(`${marker}${"x".repeat(64 * 1024)}`));
  const res = await worker.fetch(
    rpc("tools/call", { name: "get_status", arguments: {} }),
    h.env,
    h.ctx,
  );
  assert.equal(res.status, 200);
  const text = await res.text();
  assert.ok(!text.includes(marker), "oversize upstream text must never be echoed");
  const body = JSON.parse(text) as any;
  assert.equal(body.jsonrpc, "2.0");
  assert.ok(body.error, "oversize upstream must surface a JSON-RPC error");
});

test("stub hang -> abort at 6s with a JSON-RPC error", async () => {
  const h = harness(() => new Promise<Response>(() => {}));
  const start = Date.now();
  const res = (await Promise.race([
    worker.fetch(rpc("tools/call", { name: "get_status", arguments: {} }), h.env, h.ctx),
    new Promise<never>((_, reject) =>
      setTimeout(() => reject(new Error("mcp hang was not aborted within 20s")), 20_000),
    ),
  ])) as Response;
  const elapsed = Date.now() - start;
  assert.ok(elapsed < 15_000, `hung stub must abort near 6s, took ${elapsed}ms`);
  assert.equal(h.forwarded.length, 1, "the hung container fetch must have been attempted");
  assert.equal(res.status, 200);
  const body = await rpcBody(res);
  assert.ok(body.error, "hung stub must surface a JSON-RPC error");
});
