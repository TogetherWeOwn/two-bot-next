/**
 * Protected failing-first MCP request-validation acceptance. No source changes.
 * Exercise the real Worker with the synthetic request/DO-recorder patterns from
 * mcp.test.ts (its helpers are file-local); no network or live services.
 *
 * Policy: malformed envelopes/IDs -> -32600; malformed method params/tool
 * arguments -> -32602. Only string/number IDs are echoed; otherwise use null.
 * Errors are fixed and non-echoing, before any DO lookup/container read.
 * Preserve the existing null/missing-ID policy: ordinary methods -> -32600;
 * notification-prefixed methods -> empty 202, without dispatch. A string/number
 * ID makes a notification-named method a request: -32601, not silent 202.
 * Harmless envelope/method-param padding remains allowed; the strict empty
 * object schema applies to tool arguments, not every object in the request.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import worker, { type Env } from "../src/index.ts";

const MCP_TOKEN = "synthetic-mcp-bearer-token-0123456789abcd";
const SENTINEL = "synthetic-mcp-request-do-not-echo";
const TOOLS = ["get_status", "get_gateway", "get_backfill_progress", "get_config", "get_audit"];
const METHODS = ["initialize", "tools/list", "tools/call", "ping", "notifications/initialized"];
const SAFE_ID = "safe-correlation";
const UPSTREAM = {
  status: "ok",
  components: [["process", "ready"], ["gateway", "ready"], ["database", "ready"]],
  jobs: { backfill: { parked: false, running: false, consecutive_failures: 0 } },
};
const AUDIT_EXCERPT = { available: true, rows: [], truncated: false };

function harness() {
  const names: string[] = [];
  const forwarded: { method: string; url: string; authorization: string | null; body: string }[] = [];
  const env = {
    REDIRECT_MAPPINGS_JSON: "[]",
    REDIRECT_FALLBACK_CODE: "fallbackCode",
    CF_VERSION_METADATA: { id: "synthetic-mcp-deployment" },
    MCP_TOKEN,
    TWO_BOT: {
      getByName(name: string) {
        names.push(name);
        return {
          async fetch(req: Request) {
            forwarded.push({ method: req.method, url: req.url,
              authorization: req.headers.get("authorization"), body: await req.text() });
            const path = new URL(req.url).pathname;
            return Response.json(path === "/ops/audit" ? AUDIT_EXCERPT : UPSTREAM);
          },
        };
      },
    },
  } as unknown as Env;
  const ctx = { waitUntil() {} } as unknown as ExecutionContext;
  return { env, ctx, names, forwarded };
}

let callerSeq = 0;
function request(body: unknown): Request {
  // Each case gets its own documented IPv6 caller, never a shared rate bucket.
  return new Request("https://probe.invalid/mcp", {
    method: "POST",
    headers: { authorization: `Bearer ${MCP_TOKEN}`, "content-type": "application/json",
      "cf-connecting-ip": `2001:db8::${(++callerSeq).toString(16)}` },
    body: JSON.stringify(body),
  });
}

function envelope(method: string, extra: Record<string, unknown> = {}) {
  return { jsonrpc: "2.0", id: SAFE_ID, method,
    params: method === "tools/call" ? { name: "get_status", arguments: {} } : {}, ...extra };
}

async function responseBody(res: Response) {
  assert.equal(res.status, 200, "requests use the JSON-RPC response channel");
  assert.match(res.headers.get("content-type") ?? "", /application\/json/);
  return res.json() as Promise<any>;
}

async function assertRefused(input: unknown, code: number, id: string | number | null, message: string) {
  const h = harness();
  const res = await worker.fetch(request(input), h.env, h.ctx);
  assert.deepEqual(h.names, [], "invalid input must not resolve the DO");
  assert.deepEqual(h.forwarded, [], "invalid input must not read the container");
  const body = await responseBody(res);
  assert.ok(!JSON.stringify(body).includes(SENTINEL), "refusal must not echo untrusted values");
  assert.ok(!JSON.stringify(body).includes(MCP_TOKEN), "refusal must not echo credentials");
  assert.deepEqual(body, { jsonrpc: "2.0", id, error: { code, message } });
}

// Catch accepting non-object roots/batches, malformed protocol/method fields,
// or reflecting unsafe correlation values on the early envelope-error path.
const BAD_ENVELOPES: [string, unknown, string | number | null][] = [
  ["null root", null, null], ["string root", SENTINEL, null], ["number root", 42, null],
  ["boolean root", true, null], ["array root", [], null],
  ["batch root", [envelope("tools/call")], null], ["empty object", {}, null],
  ["missing protocol", { id: SAFE_ID, method: "tools/call", params: { name: "get_status" } }, SAFE_ID],
  ["wrong protocol", envelope("tools/call", { jsonrpc: SENTINEL }), SAFE_ID],
  ["numeric protocol", envelope("tools/call", { jsonrpc: 2 }), SAFE_ID],
  ["missing method", { jsonrpc: "2.0", id: SAFE_ID, params: { name: "get_status" } }, SAFE_ID],
  ["null method", envelope("tools/call", { method: null }), SAFE_ID],
  ["object method", envelope("tools/call", { method: { value: SENTINEL } }), SAFE_ID],
  ["array method", envelope("tools/call", { method: [SENTINEL] }), SAFE_ID],
  ["numeric method", envelope("tools/call", { method: 42 }), SAFE_ID],
  ["boolean method", envelope("tools/call", { method: false }), SAFE_ID],
  ["bad protocol and object ID", envelope("tools/call", { jsonrpc: SENTINEL, id: { value: SENTINEL } }), null],
  ["bad method and array ID", envelope("tools/call", { method: { value: SENTINEL }, id: [SENTINEL] }), null],
  ["bad protocol and boolean ID", envelope("tools/call", { jsonrpc: 2, id: true }), null],
  ["bad protocol with zero ID", envelope("tools/call", { jsonrpc: SENTINEL, id: 0 }), 0],
  ["bad method with empty string ID", envelope("tools/call", { method: null, id: "" }), ""],
];
for (const [label, input, id] of BAD_ENVELOPES) {
  test(`envelope: ${label} is invalid request with safe response ID`, async () => {
    await assertRefused(input, -32600, id, "invalid request");
  });
}

test("valid: unknown method keeps a fixed non-echoing method-not-found error", async () => {
  await assertRefused(envelope(SENTINEL), -32601, SAFE_ID, "method not found");
});
test("valid: unknown tool keeps a fixed non-echoing unknown-tool error", async () => {
  await assertRefused(envelope("tools/call", { params: { name: SENTINEL, arguments: {} } }),
    -32601, SAFE_ID, "unknown tool");
});

for (const method of METHODS) {
  for (const [label, id] of [["object", { value: SENTINEL }], ["array", [SENTINEL]],
    ["true", true], ["false", false]] as const) {
    test(`ID: ${method} refuses ${label} before dispatch`, async () => {
      await assertRefused(envelope(method, { id }), -32600, null, "invalid request");
    });
  }
}

const BAD_PARAMS: [string, unknown][] = [
  ["null", null], ["string", SENTINEL], ["number", 42], ["true", true], ["false", false],
  ["empty array", []], ["array", [SENTINEL]],
];
for (const method of METHODS) {
  for (const [label, params] of BAD_PARAMS) {
    test(`params: ${method} refuses ${label} with correlated invalid params`, async () => {
      await assertRefused(envelope(method, { params }), -32602, SAFE_ID, "invalid params");
    });
  }
}

for (const [label, params] of [
  ["omitted", undefined], ["missing name", { note: SENTINEL }], ["null name", { name: null }],
  ["object name", { name: { value: SENTINEL } }], ["array name", { name: [SENTINEL] }],
  ["number name", { name: 1 }], ["boolean name", { name: true }],
] as [string, unknown][]) {
  test(`params: tools/call refuses ${label} before dispatch`, async () => {
    await assertRefused(envelope("tools/call", { params }), -32602, SAFE_ID, "invalid params");
  });
}

// All five tools advertise the same no-argument object schema. Include keys
// that would be missed by an inherited-property or truthiness-only check.
const BAD_ARGUMENTS: [string, unknown][] = [
  ["null", null], ["string", SENTINEL], ["number", 1], ["true", true], ["false", false],
  ["empty array", []], ["array", [SENTINEL]], ["unexpected key", { unexpected: SENTINEL }],
  ["falsy extra", { unexpected: false }],
  ["own __proto__ key", JSON.parse(`{"__proto__":{"value":"${SENTINEL}"}}`)],
];
for (const name of TOOLS) {
  for (const [label, args] of BAD_ARGUMENTS) {
    test(`arguments: ${name} refuses ${label} before dispatch`, async () => {
      await assertRefused(envelope("tools/call", { params: { name, arguments: args } }),
        -32602, SAFE_ID, "invalid params");
    });
  }
}

test("params: invalid params preserve numeric zero correlation", async () => {
  await assertRefused(envelope("tools/call", { id: 0, params: SENTINEL }),
    -32602, 0, "invalid params");
});
test("params: invalid arguments preserve empty string correlation", async () => {
  await assertRefused(envelope("tools/call", { id: "", params: { name: "get_status", arguments: [SENTINEL] } }),
    -32602, "", "invalid params");
});

for (const method of ["initialize", "tools/list", "tools/call", "ping"]) {
  for (const [label, identity] of [["missing", { id: undefined }], ["null", { id: null }]] as const) {
    test(`notification policy: ${method} with ${label} ID remains invalid request`, async () => {
      await assertRefused(envelope(method, identity), -32600, null, "invalid request");
    });
  }
}
for (const method of ["notifications/initialized", "notifications/synthetic"]) {
  for (const [label, identity] of [["missing", { id: undefined }], ["null", { id: null }]] as const) {
    test(`notification policy: ${method} with ${label} ID keeps empty 202`, async () => {
      const h = harness();
      const res = await worker.fetch(request(envelope(method, identity)), h.env, h.ctx);
      assert.equal(res.status, 202);
      assert.equal(await res.text(), "");
      assert.deepEqual(h.names, []);
      assert.deepEqual(h.forwarded, []);
    });
    test(`notification policy: ${method} with ${label} ID still validates params`, async () => {
      await assertRefused(envelope(method, { ...identity, params: SENTINEL }),
        -32602, null, "invalid params");
    });
  }
  for (const id of [SAFE_ID, "", 0, 17]) {
    test(`notification policy: ${method} with request ID ${JSON.stringify(id)} gets a response`, async () => {
      await assertRefused(envelope(method, { id }), -32601, id, "method not found");
    });
  }
}

// Positive controls kill overbroad refusal fixes and preserve correlation,
// version negotiation, catalog, existing padding and the actual tool read path.
for (const id of [SAFE_ID, "café-€", "", 0, 17, -3]) {
  test(`valid: tools/call preserves ${JSON.stringify(id)} and performs one bounded GET`, async () => {
    const h = harness();
    const body = await responseBody(await worker.fetch(request(envelope("tools/call", { id })), h.env, h.ctx));
    assert.equal(body.jsonrpc, "2.0");
    assert.equal(body.id, id);
    assert.equal(body.error, undefined);
    assert.deepEqual(body.result?.structuredContent, UPSTREAM);
    assert.deepEqual(h.names, ["two-bot"]);
    assert.deepEqual(h.forwarded, [{ method: "GET", url: "http://c/health", authorization: null, body: "" }]);
  });
}

for (const version of ["2025-03-26", "2025-06-18", "1999-01-01"]) {
  test(`valid: initialize negotiates ${version} with harmless method metadata`, async () => {
    const h = harness();
    const body = await responseBody(await worker.fetch(request(envelope("initialize", { params: {
      protocolVersion: version, capabilities: {}, clientInfo: { name: "synthetic-acceptance", version: "0" },
      pad: SENTINEL, _meta: { progressToken: "synthetic-progress" },
    } })), h.env, h.ctx));
    assert.equal(body.id, SAFE_ID);
    assert.equal(body.error, undefined);
    assert.equal(body.result?.protocolVersion, version === "1999-01-01" ? "2025-06-18" : version);
    assert.deepEqual(body.result?.capabilities, { tools: {} });
    assert.deepEqual(h.names, []);
    assert.deepEqual(h.forwarded, []);
  });
}

for (const params of [undefined, {}, { pad: SENTINEL }]) {
  test(`valid: tools/list with ${JSON.stringify(params)} preserves the advertised empty schemas`, async () => {
    const h = harness();
    const body = await responseBody(await worker.fetch(request(envelope("tools/list", { params,
      pad: SENTINEL })), h.env, h.ctx));
    assert.equal(body.id, SAFE_ID);
    assert.equal(body.error, undefined);
    assert.deepEqual(body.result?.tools?.map((tool: any) => tool.name).sort(), [...TOOLS].sort());
    for (const tool of body.result.tools) {
      assert.deepEqual(tool.inputSchema, { type: "object", properties: {}, additionalProperties: false });
    }
    assert.deepEqual(h.names, []);
    assert.deepEqual(h.forwarded, []);
  });
  test(`valid: ping with ${JSON.stringify(params)} returns correlated empty result`, async () => {
    const h = harness();
    const body = await responseBody(await worker.fetch(request(envelope("ping", { params })), h.env, h.ctx));
    assert.deepEqual(body, { jsonrpc: "2.0", id: SAFE_ID, result: {} });
    assert.deepEqual(h.names, []);
    assert.deepEqual(h.forwarded, []);
  });
  test(`valid: notification with ${JSON.stringify(params)} stays empty 202`, async () => {
    const h = harness();
    const res = await worker.fetch(request(envelope("notifications/initialized", { id: undefined, params,
      pad: SENTINEL })), h.env, h.ctx);
    assert.equal(res.status, 202);
    assert.equal(await res.text(), "");
    assert.deepEqual(h.names, []);
    assert.deepEqual(h.forwarded, []);
  });
}

for (const name of TOOLS) {
  for (const args of [undefined, {}]) {
    test(`valid: ${name} supports ${args === undefined ? "omitted" : "empty"} arguments and method padding`, async () => {
      const h = harness();
      const body = await responseBody(await worker.fetch(request(envelope("tools/call", {
        pad: SENTINEL, params: { name, arguments: args, note: "café — €", _meta: {} },
      })), h.env, h.ctx));
      assert.equal(body.id, SAFE_ID);
      assert.equal(body.error, undefined);
      assert.ok(body.result?.structuredContent, "valid tool must actually execute, not just acknowledge");
      const path = name === "get_status" ? "/health"
        : name === "get_gateway" || name === "get_backfill_progress" ? "/readyz"
        : name === "get_audit" ? "/ops/audit" : null;
      assert.deepEqual(h.names, path === null ? [] : ["two-bot"]);
      assert.deepEqual(h.forwarded, path === null ? []
        : [{ method: "GET", url: `http://c${path}`, authorization: null, body: "" }]);
    });
  }
}
