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

import { test, type TestContext } from "node:test";
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

test("tools/call get_audit returns a real empty excerpt with one fenced GET", async () => {
  const excerpt = { available: true, rows: [], truncated: false };
  const h = harness(() => Response.json(excerpt));
  const res = await worker.fetch(
    rpc("tools/call", { name: "get_audit", arguments: {} }),
    h.env,
    h.ctx,
  );
  assert.equal(res.status, 200);
  const body = await rpcBody(res);
  assert.deepEqual(body.result?.structuredContent, excerpt);
  assert.equal(h.forwarded.length, 1);
  assert.equal(h.forwarded[0]!.method, "GET");
  assert.equal(new URL(h.forwarded[0]!.url).pathname, "/ops/audit");
  assert.equal(h.forwarded[0]!.headers["x-two-bot-deployment-id"], "synthetic-mcp-deployment");
  assert.equal(h.forwarded[0]!.headers.authorization, undefined);
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

// ------------------------------------------------------------ inbound intake
//
// Bounded authenticated request bodies, before JSON parsing (test files
// only — no src changes). Contract under test (builder: implement exactly
// this on POST /mcp, after the existing bearer/rate gate):
// - Collect at most 64 KiB of inbound bytes under a six-second body
//   deadline before JSON parsing. The byte count is measured on the bytes
//   actually read, never on Content-Length alone: streamed bodies with no
//   declared length and bodies whose declared length lies are held to the
//   same cap. Multi-byte characters count by encoded bytes, not characters.
//   This bounds inbound intake only; the existing 64 KiB / ~6 s bounds on
//   outbound container reads (pinned above) are a separate budget.
// - Oversize, deadline-expired, aborted/errored and malformed bodies all
//   produce the one fixed, non-echoing refusal the malformed path already
//   returns (HTTP 200, JSON-RPC -32700 "parse error", id null), the body
//   reader is cancelled/released, and no container request occurs.
// - Authentication and rate refusal still happen before the body is read:
//   a hostile stalled body must not be pulled on those paths.
// - Notifications, refused methods and bearer non-forwarding are unchanged
//   for bodies inside the bound; an oversize notification is still refused
//   at intake, not accepted.

const INBOUND_CAP_BYTES = 64 * 1024;
const FIXED_INTAKE_REFUSAL = {
  jsonrpc: "2.0",
  id: null,
  error: { code: -32700, message: "parse error" },
};

let rawCallerSeq = 0;
/** Raw POST /mcp with a caller-isolated IP, like rpc() above. */
const rawMcp = (
  body: BodyInit,
  opts: { auth?: string | null; headers?: Record<string, string>; signal?: AbortSignal; ip?: string } = {},
) => {
  rawCallerSeq += 1;
  const auth = opts.auth === undefined ? `Bearer ${MCP_TOKEN}` : opts.auth;
  const init: RequestInit & { duplex?: "half" } = {
    method: "POST",
    headers: {
      ...(auth ? { authorization: auth } : {}),
      "content-type": "application/json",
      "cf-connecting-ip": opts.ip ?? `198.51.100.${(rawCallerSeq % 200) + 1}`,
      ...opts.headers,
    },
    body,
  };
  if (typeof body === "object" && body !== null && "getReader" in body) init.duplex = "half";
  if (opts.signal) init.signal = opts.signal;
  return new Request("https://probe.invalid/mcp", init);
};

const encoder = new TextEncoder();
const byteLength = (s: string) => encoder.encode(s).byteLength;

/** A valid JSON-RPC body of exactly totalBytes bytes (ASCII pad). */
function paddedRpcBody(
  totalBytes: number,
  opts: { method?: string; notification?: boolean; marker?: string } = {},
): string {
  const marker = opts.marker ?? "";
  const prefix = opts.notification
    ? `{"jsonrpc":"2.0","method":"${opts.method ?? "notifications/initialized"}","params":{"pad":"${marker}`
    : `{"jsonrpc":"2.0","id":1,"method":"${opts.method ?? "tools/list"}","params":{"pad":"${marker}`;
  const suffix = '"}}';
  const fill = totalBytes - byteLength(prefix) - byteLength(suffix);
  assert.ok(fill >= 0, "fixture target smaller than its own framing");
  const body = prefix + "x".repeat(fill) + suffix;
  assert.equal(byteLength(body), totalBytes, "fixture must be exactly the target byte count");
  return body;
}

/** Pull-driven chunk stream with delivered/cancelled bookkeeping. */
function trackedChunks(chunks: Uint8Array[], highWaterMark = 1) {
  const state = { pulls: 0, delivered: 0, cancelled: false };
  const stream = new ReadableStream<Uint8Array>(
    {
      pull(controller) {
        state.pulls++;
        if (state.delivered < chunks.length) {
          controller.enqueue(chunks[state.delivered++]!);
        } else {
          controller.close();
        }
      },
      cancel() {
        state.cancelled = true;
      },
    },
    { highWaterMark },
  );
  return { stream, state };
}

/** A stream that yields one prefix chunk and then stalls forever. */
function stalledBody(prefix: string) {
  const state = { pulls: 0, cancelled: false };
  let sent = false;
  const stream = new ReadableStream<Uint8Array>(
    {
      pull(controller) {
        state.pulls++;
        if (!sent) {
          sent = true;
          controller.enqueue(encoder.encode(prefix));
          return;
        }
        return new Promise<void>(() => {});
      },
      cancel() {
        state.cancelled = true;
      },
    },
    { highWaterMark: 0 },
  );
  return { stream, state };
}

/** Fail rather than hang when the Worker does not settle inside ms. */
async function settleWithin<T>(work: Promise<T>, label: string, ms: number): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const guard = new Promise<never>((_, reject) => {
    timer = setTimeout(() => reject(new Error(`${label} did not settle within ${ms}ms`)), ms);
  });
  try {
    return await Promise.race([work, guard]);
  } finally {
    clearTimeout(timer);
  }
}

/** Model runtime cleanup throws; a native source cancel hook only rejects. */
function syncThrowingReaderCleanup(
  t: TestContext,
  stream: ReadableStream<Uint8Array>,
  method: "cancel" | "releaseLock",
  marker: string,
) {
  const state = { attempts: 0 };
  const getReader = stream.getReader.bind(stream);
  t.mock.method(stream, "getReader", () => {
    const reader = getReader();
    const cancel = reader.cancel.bind(reader);
    const release = reader.releaseLock.bind(reader);
    t.mock.method(reader, method, () => {
      state.attempts++;
      throw new Error(marker);
    });
    // Use the original methods so even a guard-removal mutant leaves no lock.
    t.after(async () => {
      if (stream.locked) {
        await cancel();
        release();
      } else {
        await stream.cancel();
      }
    });
    return reader;
  });
  return state;
}

for (const method of ["cancel", "releaseLock"] as const) {
  test(`inbound oversize keeps the parse refusal when reader ${method} throws synchronously`, async (t) => {
    const marker = `synthetic-intake-sync-${method}`;
    const h = harness(() => Response.json({ synthetic: true }));
    const { stream } = trackedChunks([encoder.encode(paddedRpcBody(INBOUND_CAP_BYTES + 1))], 0);
    const state = syncThrowingReaderCleanup(t, stream, method, marker);
    const res = await settleWithin(worker.fetch(rawMcp(stream), h.env, h.ctx), marker, 3_000);
    await assertIntakeRefusal(res, marker);
    assert.equal(state.attempts, 1, "throwing cleanup is attempted exactly once");
    assert.deepEqual(h.names, [], "refused intake must not resolve the DO");
    assert.deepEqual(h.forwarded, [], "refused intake must not reach the container");
    // node:test also fails on any unhandled rejection from this request.
    await new Promise<void>((resolve) => setImmediate(resolve));
  });

  test(`outbound oversize keeps the limit refusal when reader ${method} throws synchronously`, async (t) => {
    const marker = `synthetic-outbound-sync-${method}`;
    const { stream } = trackedChunks([encoder.encode(`${marker}${"x".repeat(INBOUND_CAP_BYTES)}`)], 0);
    const state = syncThrowingReaderCleanup(t, stream, method, marker);
    const h = harness(() => new Response(stream));
    const res = await settleWithin(
      worker.fetch(rpc("tools/call", { name: "get_status", arguments: {} }), h.env, h.ctx),
      marker,
      3_000,
    );
    assert.equal(res.status, 200);
    assert.deepEqual(await rpcBody(res), {
      jsonrpc: "2.0", id: 1, error: { code: -32000, message: "container body exceeds limit" },
    }, "cleanup must not turn overflow into a generic failure or echo the marker");
    assert.equal(state.attempts, 1, "throwing cleanup is attempted exactly once");
    assert.equal(h.forwarded.length, 1, "one bounded container request, no cleanup retry");
    await new Promise<void>((resolve) => setImmediate(resolve));
  });
}

test("rejected upstream keeps the tool refusal when body cancel throws synchronously", async (t) => {
  const marker = "synthetic-response-sync-cancel";
  const { stream } = trackedChunks([encoder.encode(marker)], 0);
  const cancel = stream.cancel.bind(stream);
  const cleanup = t.mock.method(stream, "cancel", () => { throw new Error(marker); });
  t.after(async () => { await cancel(); });
  const h = harness(() => new Response(stream, { status: 500 }));
  const res = await settleWithin(
    worker.fetch(rpc("tools/call", { name: "get_status", arguments: {} }), h.env, h.ctx),
    marker,
    3_000,
  );
  assert.equal(res.status, 200);
  assert.deepEqual(await rpcBody(res), {
    jsonrpc: "2.0", id: 1, error: { code: -32000, message: "container request failed" },
  });
  assert.equal(cleanup.mock.callCount(), 1, "throwing body cleanup is attempted exactly once");
  assert.equal(h.forwarded.length, 1);
  await new Promise<void>((resolve) => setImmediate(resolve));
});

/** The one fixed intake refusal: same shape for every intake failure. */
async function assertIntakeRefusal(res: Response, marker?: string) {
  assert.equal(res.status, 200, "intake refusal keeps the JSON-RPC error channel");
  const text = await res.text();
  if (marker) assert.ok(!text.includes(marker), "refusal must never echo body bytes");
  assert.ok(!text.includes(MCP_TOKEN), "refusal must never echo the bearer");
  assert.deepEqual(JSON.parse(text), FIXED_INTAKE_REFUSAL);
}

test("inbound body of exactly 64 KiB is parsed and answered", async () => {
  const h = harness(() => Response.json({ synthetic: true }));
  const res = await worker.fetch(rawMcp(paddedRpcBody(INBOUND_CAP_BYTES)), h.env, h.ctx);
  assert.equal(res.status, 200);
  const body = await rpcBody(res);
  const names = (body.result?.tools as { name: string }[] | undefined)?.map((t) => t.name).sort();
  assert.deepEqual(names, [...EXPECTED_TOOL_NAMES].sort(), "at-cap body must behave like any valid call");
});

test("inbound body of 64 KiB + 1 byte is refused before parsing, zero container fetches", async () => {
  const marker = "synthetic-intake-oversize-marker";
  const h = harness(() => Response.json({ synthetic: true }));
  const res = await worker.fetch(
    rawMcp(paddedRpcBody(INBOUND_CAP_BYTES + 1, { marker })),
    h.env,
    h.ctx,
  );
  await assertIntakeRefusal(res, marker);
  assert.deepEqual(h.names, [], "oversize intake must not resolve the DO");
  assert.deepEqual(h.forwarded, [], "oversize intake must never reach the container");
});

test("a Content-Length that understates the body does not lift the cap", async () => {
  const marker = "synthetic-intake-lying-length-marker";
  const h = harness(() => Response.json({ synthetic: true }));
  const req = rawMcp(paddedRpcBody(INBOUND_CAP_BYTES + 1, { marker }), {
    headers: { "content-length": "10" },
  });
  assert.equal(req.headers.get("content-length"), "10", "fixture must lie about its length");
  const res = await worker.fetch(req, h.env, h.ctx);
  await assertIntakeRefusal(res, marker);
  assert.deepEqual(h.forwarded, [], "oversize intake must never reach the container");
});

test("streamed oversize without Content-Length is cancelled at the cap, tail unread", async () => {
  const marker = "synthetic-intake-streamed-marker";
  const h = harness(() => Response.json({ synthetic: true }));
  const bytes = encoder.encode(paddedRpcBody(INBOUND_CAP_BYTES + 8192, { marker }));
  const chunks = [
    bytes.subarray(0, 32 * 1024),
    bytes.subarray(32 * 1024, INBOUND_CAP_BYTES),
    bytes.subarray(INBOUND_CAP_BYTES, INBOUND_CAP_BYTES + 1),
  ];
  // Split the post-cap tail into small chunks so every extra read past
  // the cap is visible as one more delivered chunk (see the bound below).
  for (let off = INBOUND_CAP_BYTES + 1; off < bytes.length; off += 1024) {
    chunks.push(bytes.subarray(off, off + 1024));
  }
  // highWaterMark 0 disables this fixture's own one-chunk-ahead pull (the
  // default HWM of 1), so delivered counts the Worker's demand reads only.
  const { stream, state } = trackedChunks(chunks, 0);
  const res = await worker.fetch(rawMcp(stream), h.env, h.ctx);
  await assertIntakeRefusal(res, marker);
  // Stopping at the cap costs exactly three demand reads: the 32 KiB
  // chunk, the rest up to the cap, and the cap+1 byte that proves the
  // body is oversize. Any read past that point delivers a fourth chunk
  // and fails here, as does draining (all chunks).
  assert.ok(
    state.delivered <= 3,
    `tail past cap+1 must stay unread (delivered ${state.delivered}/${chunks.length})`,
  );
  assert.equal(state.cancelled, true, "oversize streamed body must be cancelled, not drained");
  assert.deepEqual(h.names, [], "oversize intake must not resolve the DO");
  assert.deepEqual(h.forwarded, [], "oversize intake must never reach the container");
});

test("the cap counts encoded bytes: a multi-byte body under the character cap is refused", async () => {
  const marker = "synthetic-intake-multibyte-marker";
  const h = harness(() => Response.json({ synthetic: true }));
  // "é" is one character but two UTF-8 bytes: this body is well under
  // 64 Ki characters while exceeding 64 KiB on the wire.
  const body = JSON.stringify({
    jsonrpc: "2.0",
    id: 1,
    method: "tools/list",
    params: { pad: `${marker}${"é".repeat(33_000)}` },
  });
  assert.ok(body.length < INBOUND_CAP_BYTES, `fixture chars ${body.length} must be under the cap`);
  assert.ok(byteLength(body) > INBOUND_CAP_BYTES, `fixture bytes ${byteLength(body)} must exceed the cap`);
  const res = await worker.fetch(rawMcp(body), h.env, h.ctx);
  await assertIntakeRefusal(res, marker);
  assert.deepEqual(h.forwarded, [], "oversize intake must never reach the container");
});

test("a multi-byte character split across chunks inside the cap still parses and proxies", async () => {
  const marker = "synthetic-intake-split-ok";
  const h = harness(() => Response.json({ status: "ok", marker }));
  const body = JSON.stringify({
    jsonrpc: "2.0",
    id: 1,
    method: "tools/call",
    params: { name: "get_status", arguments: {}, note: "café — €" },
  });
  const bytes = encoder.encode(body);
  assert.ok(bytes.byteLength < INBOUND_CAP_BYTES, "fixture must be inside the cap");
  // Split between the two UTF-8 bytes of "é" (0xC3 0xA9).
  const at = bytes.indexOf(0xc3);
  assert.ok(at > 0 && bytes[at + 1] === 0xa9, "fixture must locate the split point");
  const { stream, state } = trackedChunks([bytes.subarray(0, at + 1), bytes.subarray(at + 1)]);
  const res = await worker.fetch(rawMcp(stream), h.env, h.ctx);
  assert.equal(res.status, 200);
  const parsed = await rpcBody(res);
  assert.ok(JSON.stringify(parsed).includes(marker), "split-chunk call must return the container JSON");
  assert.equal(state.cancelled, false, "an in-bounds body is read to the end, not cancelled");
  assert.equal(h.forwarded.length, 1, "one tool call proxies exactly one container fetch");
  assert.equal(
    h.forwarded[0]!.headers["authorization"],
    undefined,
    "caller bearer must not be forwarded",
  );
});

test("a multi-byte character split across chunks inside the string id echoes exactly", async () => {
  const marker = "synthetic-intake-split-id-ok";
  const h = harness(() => Response.json({ status: "ok", marker }));
  const splitId = "café-€";
  const body = JSON.stringify({
    jsonrpc: "2.0",
    id: splitId,
    method: "tools/call",
    params: { name: "get_status", arguments: {} },
  });
  const bytes = encoder.encode(body);
  assert.ok(bytes.byteLength < INBOUND_CAP_BYTES, "fixture must be inside the cap");
  assert.ok(body.includes(splitId), "fixture must carry the split id");
  // Split between the two UTF-8 bytes of "é" in the id (0xC3 0xA9). The
  // fixture carries no other non-ASCII bytes, so this split is inside the id.
  const at = bytes.indexOf(0xc3);
  assert.ok(at > 0 && bytes[at + 1] === 0xa9, "fixture must locate the id split point");
  const { stream, state } = trackedChunks([bytes.subarray(0, at + 1), bytes.subarray(at + 1)]);
  const res = await worker.fetch(rawMcp(stream), h.env, h.ctx);
  assert.equal(res.status, 200);
  const parsed = await rpcBody(res);
  assert.equal(parsed.id, splitId, "split string id must decode exactly, preserving JSON-RPC correlation");
  assert.ok(JSON.stringify(parsed).includes(marker), "split-id call must return the container JSON");
  assert.equal(state.cancelled, false, "an in-bounds body is read to the end, not cancelled");
  assert.equal(h.forwarded.length, 1, "one tool call proxies exactly one container fetch");
  assert.equal(
    h.forwarded[0]!.headers["authorization"],
    undefined,
    "caller bearer must not be forwarded",
  );
});

test("a stalled inbound body is refused at the six-second deadline and cancelled", async () => {
  const h = harness(() => Response.json({ synthetic: true }));
  const { stream, state } = stalledBody('{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"pad":"');
  const start = Date.now();
  const res = await settleWithin(
    worker.fetch(rawMcp(stream), h.env, h.ctx),
    "stalled inbound body",
    15_000,
  );
  const elapsed = Date.now() - start;
  assert.ok(elapsed >= 4_000, `stalled body must be held to the deadline, refused after ${elapsed}ms`);
  assert.ok(elapsed < 12_000, `stalled body must be refused near 6s, took ${elapsed}ms`);
  await assertIntakeRefusal(res);
  assert.equal(state.cancelled, true, "deadline refusal must cancel the reader");
  assert.deepEqual(h.names, [], "stalled intake must not resolve the DO");
  assert.deepEqual(h.forwarded, [], "stalled intake must never reach the container");
});

test("a client abort mid-body settles as the fixed refusal well before the deadline", async () => {
  const h = harness(() => Response.json({ synthetic: true }));
  const { stream, state } = stalledBody('{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"pad":"');
  const ac = new AbortController();
  const start = Date.now();
  const pending = settleWithin(
    worker.fetch(rawMcp(stream, { signal: ac.signal }), h.env, h.ctx),
    "aborted inbound body",
    4_500,
  );
  setTimeout(() => ac.abort(), 100);
  const res = await pending;
  const elapsed = Date.now() - start;
  assert.ok(elapsed < 4_500, `abort must settle promptly, took ${elapsed}ms`);
  await assertIntakeRefusal(res);
  await new Promise<void>((resolve) => setImmediate(resolve));
  assert.equal(state.cancelled, true, "abort refusal must release/cancel the reader");
  assert.deepEqual(h.names, [], "aborted intake must not resolve the DO");
  assert.deepEqual(h.forwarded, [], "aborted intake must never reach the container");
});

test("a complete body that then stalls is still refused at the deadline with no forward", async () => {
  const h = harness(() => Response.json({ status: "ok", marker: "synthetic-intake-complete-stall" }));
  // Complete valid tools/call body: without the deadline/abort discriminator
  // a cancelled read resolving done:true would look like EOF, parse, and
  // forward. The intake must refuse instead.
  const complete = JSON.stringify({
    jsonrpc: "2.0",
    id: 1,
    method: "tools/call",
    params: { name: "get_status", arguments: {} },
  });
  const { stream, state } = stalledBody(complete);
  const start = Date.now();
  const res = await settleWithin(
    worker.fetch(rawMcp(stream), h.env, h.ctx),
    "complete-then-stalled inbound body",
    15_000,
  );
  const elapsed = Date.now() - start;
  assert.ok(elapsed >= 4_000, `complete-then-stalled body must be held to the deadline, refused after ${elapsed}ms`);
  assert.ok(elapsed < 12_000, `complete-then-stalled body must be refused near 6s, took ${elapsed}ms`);
  await assertIntakeRefusal(res);
  assert.equal(state.cancelled, true, "deadline refusal must cancel the reader");
  assert.deepEqual(h.names, [], "complete-then-stalled intake must not resolve the DO");
  assert.deepEqual(h.forwarded, [], "complete-then-stalled intake must never reach the container");
});

test("a complete body that then aborts settles as the fixed refusal with no forward", async () => {
  const h = harness(() => Response.json({ synthetic: true }));
  const complete = JSON.stringify({
    jsonrpc: "2.0",
    id: 1,
    method: "tools/call",
    params: { name: "get_status", arguments: {} },
  });
  const { stream, state } = stalledBody(complete);
  const ac = new AbortController();
  const pending = settleWithin(
    worker.fetch(rawMcp(stream, { signal: ac.signal }), h.env, h.ctx),
    "complete-then-aborted inbound body",
    4_500,
  );
  setTimeout(() => ac.abort(), 100);
  const res = await pending;
  await assertIntakeRefusal(res);
  await new Promise<void>((resolve) => setImmediate(resolve));
  assert.equal(state.cancelled, true, "abort refusal must release/cancel the reader");
  assert.deepEqual(h.names, [], "complete-then-aborted intake must not resolve the DO");
  assert.deepEqual(h.forwarded, [], "complete-then-aborted intake must never reach the container");
});

test("a body stream that errors mid-body gets the fixed refusal with no echo", async () => {
  const marker = "synthetic-intake-stream-error-marker";
  const h = harness(() => Response.json({ synthetic: true }));
  let step = 0;
  const stream = new ReadableStream<Uint8Array>({
    pull(controller) {
      step++;
      if (step === 1) {
        controller.enqueue(encoder.encode('{"jsonrpc":"2.0","id":1'));
      } else {
        controller.error(new Error(marker));
      }
    },
  });
  const res = await settleWithin(worker.fetch(rawMcp(stream), h.env, h.ctx), "errored body", 4_500);
  await assertIntakeRefusal(res, marker);
  assert.deepEqual(h.forwarded, [], "errored intake must never reach the container");
});

test("malformed and empty bodies keep the fixed parse-error refusal with no echo", async () => {
  const marker = "synthetic-intake-malformed-marker";
  const h = harness(() => Response.json({ synthetic: true }));
  for (const body of [`{not json ${marker}`, ""]) {
    const res = await worker.fetch(rawMcp(body), h.env, h.ctx);
    await assertIntakeRefusal(res, marker);
  }
  assert.deepEqual(h.names, [], "malformed intake must not resolve the DO");
  assert.deepEqual(h.forwarded, [], "malformed intake must never reach the container");
});

test("authentication refusal happens before a hostile body is read", async () => {
  const h = harness(() => Response.json({ synthetic: true }));
  for (const auth of [null, "Bearer wrong-bearer-value"] as const) {
    const hostile = stalledBody('{"jsonrpc":"2.0"');
    const res = await settleWithin(
      worker.fetch(rawMcp(hostile.stream, { auth }), h.env, h.ctx),
      "unauthenticated stalled body",
      3_000,
    );
    assert.equal(res.status, 401);
    assert.match(res.headers.get("www-authenticate") ?? "", /Bearer/);
    await res.text();
    assert.equal(hostile.state.pulls, 0, "unauthenticated requests must not read the body");
  }
  assert.deepEqual(h.names, [], "unauthenticated calls must not resolve the DO");
  assert.deepEqual(h.forwarded, [], "unauthenticated calls must never reach the container");
});

test("rate refusal happens before a hostile body is read", async () => {
  const h = harness(() => Response.json({ synthetic: true }));
  // Own TEST-NET-3 caller, far from the rawMcp()/rpc() pools and from the
  // budget test above: 10-burst, so the 11th rapid call is shed.
  const ip = "203.0.113.99";
  const valid = JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list", params: {} });
  for (let i = 0; i < 10; i++) {
    const res = await worker.fetch(rawMcp(valid, { ip }), h.env, h.ctx);
    await res.text();
  }
  const hostile = stalledBody('{"jsonrpc":"2.0"');
  const res = await settleWithin(
    worker.fetch(rawMcp(hostile.stream, { ip }), h.env, h.ctx),
    "rate-limited stalled body",
    3_000,
  );
  assert.equal(res.status, 429, "11th rapid call on a 10-burst must be shed before intake");
  await res.text();
  assert.equal(hostile.state.pulls, 0, "rate-limited requests must not read the body");
});

test("notifications inside the bound keep the empty 202 acknowledgement", async () => {
  const h = harness(() => Response.json({ synthetic: true }));
  const res = await worker.fetch(
    rawMcp(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" })),
    h.env,
    h.ctx,
  );
  assert.equal(res.status, 202);
  assert.equal(await res.text(), "", "notification acknowledgement carries no body");
  assert.deepEqual(h.forwarded, [], "notifications must not reach the container");
});

test("an oversize notification is refused at intake, not acknowledged", async () => {
  const marker = "synthetic-intake-oversize-notification-marker";
  const h = harness(() => Response.json({ synthetic: true }));
  const res = await worker.fetch(
    rawMcp(paddedRpcBody(INBOUND_CAP_BYTES + 1, { notification: true, marker })),
    h.env,
    h.ctx,
  );
  await assertIntakeRefusal(res, marker);
  assert.deepEqual(h.forwarded, [], "oversize notifications must never reach the container");
});
