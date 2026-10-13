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

let rpcCallerSeq = 0;
const rpc = (method: string, params: unknown, auth: string | null = `Bearer ${MCP_TOKEN}`) => {
  // mcpAuthBuckets is keyed by cf-connecting-ip (fallback "unknown") with a
  // 10-burst/1-per-second budget: isolate callers per request so budgets never
  // bleed across cases. The cap itself is pinned by the 429 test below.
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

test("exhausted per-caller budget -> 429 with retry-after", async () => {
  const h = harness(() => Response.json({ synthetic: true }));
  // Fixed caller on a separate TEST-NET-3 address, far from the rpc() pool.
  const fixed = () =>
    new Request("https://probe.invalid/mcp", {
      method: "POST",
      headers: {
        authorization: `Bearer ${MCP_TOKEN}`,
        "content-type": "application/json",
        "cf-connecting-ip": "203.0.113.7",
      },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list", params: {} }),
    });
  let last: Response | null = null;
  for (let i = 0; i < 11; i++) {
    last = await worker.fetch(fixed(), h.env, h.ctx);
    await last.text();
  }
  assert.equal(last!.status, 429, "11th rapid call on a 10-burst must be shed");
  assert.match(last!.headers.get("retry-after") ?? "", /[0-9]/);
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

// Real /readyz shapes, copied from crates/bot/src/server.rs readyz tests:
// components is Vec<(String, ComponentStatus)>, serialized as [name, status]
// pairs; gateway_failure is present only after a recorded failure.
const READY_COMPONENTS = [
  ["process", "ready"],
  ["gateway", "ready"],
  ["database", "ready"],
  ["token_invalid", "ready"],
];
const DOWN_COMPONENTS = [
  ["process", "ready"],
  ["gateway", "down"],
  ["database", "down"],
  ["token_invalid", "ready"],
];
const JOBS_FIXTURE = {
  backfill: {
    parked: false,
    running: false,
    last_start: null,
    last_success: null,
    last_error_class: null,
    consecutive_failures: 0,
  },
};

test("tools/call get_gateway on ready 200 returns the gateway pair", async () => {
  const h = harness(() =>
    Response.json({ components: READY_COMPONENTS, jobs: JOBS_FIXTURE }),
  );
  const res = await worker.fetch(
    rpc("tools/call", { name: "get_gateway", arguments: {} }),
    h.env,
    h.ctx,
  );
  assert.equal(res.status, 200);
  const body = await rpcBody(res);
  assert.equal(body.error, undefined);
  assert.deepEqual(body.result?.structuredContent, { gateway: "ready" });
});

test("tools/call get_gateway on gateway-down 503 returns gateway plus failure", async () => {
  const failure = { phase: "durable_gateway", class: "timeout" };
  const h = harness(() =>
    Response.json(
      {
        components: DOWN_COMPONENTS,
        jobs: JOBS_FIXTURE,
        gateway_failure: failure,
      },
      { status: 503 },
    ),
  );
  const res = await worker.fetch(
    rpc("tools/call", { name: "get_gateway", arguments: {} }),
    h.env,
    h.ctx,
  );
  assert.equal(res.status, 200);
  const body = await rpcBody(res);
  assert.equal(body.error, undefined);
  assert.deepEqual(body.result?.structuredContent, {
    gateway: "down",
    gateway_failure: failure,
  });
});

test("tools/call get_backfill_progress pins the jobs excerpt on 200 and 503", async () => {
  for (const status of [200, 503] as const) {
    const h = harness(() =>
      Response.json({ components: status === 200 ? READY_COMPONENTS : DOWN_COMPONENTS, jobs: JOBS_FIXTURE }, { status }),
    );
    const res = await worker.fetch(
      rpc("tools/call", { name: "get_backfill_progress", arguments: {} }),
      h.env,
      h.ctx,
    );
    assert.equal(res.status, 200);
    const body = await rpcBody(res);
    assert.equal(body.error, undefined, `503 readyz must parse, got error on ${status}`);
    assert.deepEqual(body.result?.structuredContent, {
      implemented: false,
      source: "readyz.jobs",
      jobs: JOBS_FIXTURE,
    });
  }
});

test("tools/call get_gateway rejects non-report /readyz bodies with an error", async () => {
  const cases: { name: string; respond: () => Response }[] = [
    {
      name: "500 JSON stays a failure",
      respond: () => Response.json({ synthetic: true }, { status: 500 }),
    },
    {
      name: "text/plain 503 stays a failure",
      respond: () =>
        new Response("not ready", {
          status: 503,
          headers: { "content-type": "text/plain" },
        }),
    },
    {
      name: "ownership-fence 503 stays a failure",
      respond: () =>
        Response.json(
          { error: "ownership_fenced", reason: "shutdown_unconfirmed" },
          { status: 503 },
        ),
    },
    {
      name: "junk 200 array stays a failure",
      respond: () => Response.json(["process", "ready"], { status: 200 }),
    },
  ];
  for (const c of cases) {
    const h = harness(c.respond);
    const res = await worker.fetch(
      rpc("tools/call", { name: "get_gateway", arguments: {} }),
      h.env,
      h.ctx,
    );
    assert.equal(res.status, 200);
    const body = await rpcBody(res);
    assert.ok(body.error, `${c.name}: must surface a JSON-RPC error`);
    assert.equal(body.result, undefined, `${c.name}: must carry no result`);
    assert.ok(
      !JSON.stringify(body).includes("structuredContent"),
      `${c.name}: must carry no structuredContent`,
    );
  }
});

test("tools/call get_backfill_progress rejects non-report /readyz bodies with an error", async () => {
  const cases: { name: string; respond: () => Response }[] = [
    {
      name: "500 JSON stays a failure",
      respond: () => Response.json({ synthetic: true }, { status: 500 }),
    },
    {
      name: "text/plain 503 stays a failure",
      respond: () =>
        new Response("not ready", {
          status: 503,
          headers: { "content-type": "text/plain" },
        }),
    },
    {
      name: "ownership-fence 503 stays a failure",
      respond: () =>
        Response.json(
          { error: "ownership_fenced", reason: "shutdown_unconfirmed" },
          { status: 503 },
        ),
    },
  ];
  for (const c of cases) {
    const h = harness(c.respond);
    const res = await worker.fetch(
      rpc("tools/call", { name: "get_backfill_progress", arguments: {} }),
      h.env,
      h.ctx,
    );
    assert.equal(res.status, 200);
    const body = await rpcBody(res);
    assert.ok(body.error, `${c.name}: must surface a JSON-RPC error`);
    assert.equal(body.result, undefined, `${c.name}: must carry no result`);
    assert.ok(
      !JSON.stringify(body).includes("structuredContent"),
      `${c.name}: must carry no structuredContent`,
    );
  }
});

test("tools/call get_status on a 503 stays a failure", async () => {
  const h = harness(() => Response.json({ status: "degraded" }, { status: 503 }));
  const res = await worker.fetch(
    rpc("tools/call", { name: "get_status", arguments: {} }),
    h.env,
    h.ctx,
  );
  assert.equal(res.status, 200);
  const body = await rpcBody(res);
  assert.ok(body.error, "get_status 503 must surface a JSON-RPC error");
  assert.equal(body.result, undefined);
});

test("tools/call get_config reports the Worker version, never unknown here", async () => {
  const h = harness(() => Response.json({ synthetic: true }));
  const res = await worker.fetch(
    rpc("tools/call", { name: "get_config", arguments: {} }),
    h.env,
    h.ctx,
  );
  assert.equal(res.status, 200);
  const body = await rpcBody(res);
  assert.deepEqual(body.result?.structuredContent, {
    build_revision: "synthetic-mcp-deployment",
    bot_port: "8080",
    keepalive_seconds: "60",
  });
  assert.deepEqual(h.forwarded, [], "get_config must not touch the container");
});

test("tools/call get_audit pins the implemented=false stub", async () => {
  const h = harness(() => Response.json({ synthetic: true }));
  const res = await worker.fetch(
    rpc("tools/call", { name: "get_audit", arguments: {} }),
    h.env,
    h.ctx,
  );
  assert.equal(res.status, 200);
  const body = await rpcBody(res);
  assert.deepEqual(body.result?.structuredContent, { implemented: false });
  assert.deepEqual(h.forwarded, [], "get_audit must not touch the container");
});

test("initialize falls back to a supported version for unknown requests", async () => {
  const h = harness(() => Response.json({ synthetic: true }));
  const supported = await worker.fetch(
    rpc("initialize", {
      protocolVersion: "2025-03-26",
      capabilities: {},
      clientInfo: { name: "synthetic-acceptance", version: "0" },
    }),
    h.env,
    h.ctx,
  );
  assert.equal((await rpcBody(supported)).result?.protocolVersion, "2025-03-26");
  const unknown = await worker.fetch(
    rpc("initialize", {
      protocolVersion: "1999-01-01",
      capabilities: {},
      clientInfo: { name: "synthetic-acceptance", version: "0" },
    }),
    h.env,
    h.ctx,
  );
  assert.equal((await rpcBody(unknown)).result?.protocolVersion, "2025-06-18");
});
