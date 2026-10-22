/**
 * Protected failing-first acceptance for the admin API read slice.
 * Test file only — no src changes. The implementation must make these tests
 * pass without editing this file.
 *
 * Contract under test (guild-first reads over both Worker surfaces):
 * - MCP read tools under mcp:read: get_guild, get_channel, get_role,
 *   get_member, get_message. Every tool takes guild_id first; reads are
 *   guild-scoped, so the guild parameter is always explicit, even where the
 *   Discord API address omits it. Member reads key members by user_id, the
 *   Discord member identity.
 * - REST reads: GET /admin/v1/guilds/:guild_id plus the nested
 *   channels/:id, roles/:id, members/:user_id and
 *   channels/:id/messages/:message_id routes.
 * - Contract decision: the phase-1 MCP bearer carries mcp:read, so it can
 *   call the new read tools, and it predates guild scoping, so it is not
 *   guild-restricted. Scoped admin tokens (guild allowlist plus
 *   read/moderate/admin scopes) arrive with the token-registry/gateway
 *   foundation, which also owns the cross-guild/wrong-scope refusal tables:
 *   those ship as ready-to-paste cases plus the staging prove-out in the
 *   card handoff, not as pins in this file.
 * - The stub container serves internal-action receipt envelopes
 *   ({ok:true,result:{outcome,...},request_id}), the existing read-verb wire
 *   shape (cf. event.read); tool results must pass the resource payload
 *   through. The Worker must never forward the caller bearer nor echo it.
 * - Existing bounds are kept: the 64 KiB / 6 s intake on /mcp, fixed
 *   non-echoing refusals, and the five phase-1 tools unchanged.
 *
 * Exercise the real Worker with the synthetic request/DO-recorder pattern
 * from mcp.test.ts (its helpers are file-local); no network or live services.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import worker, { type Env } from "../src/index.ts";

const MCP_TOKEN = "synthetic-admin-read-bearer-0123456789abcdef";
const SENTINEL = "synthetic-admin-read-do-not-echo";
const CANNED_MARKER = "synthetic-admin-read-container-payload";
const SAFE_ID = "safe-admin-read-correlation";
const LEGACY_TOOLS = [
  "get_status",
  "get_gateway",
  "get_backfill_progress",
  "get_config",
  "get_audit",
];

const GUILD_ID = "111111111111111111";
const CHANNEL_ID = "222222222222222222";
const ROLE_ID = "333333333333333333";
const USER_ID = "444444444444444444";
const MESSAGE_ID = "555555555555555555";

interface ReadTool {
  name: string;
  args: Record<string, string>;
  resourceId: string;
  markerField: string;
}

const NEW_TOOLS: ReadTool[] = [
  { name: "get_guild", args: { guild_id: GUILD_ID }, resourceId: GUILD_ID, markerField: "name" },
  { name: "get_channel", args: { guild_id: GUILD_ID, channel_id: CHANNEL_ID }, resourceId: CHANNEL_ID, markerField: "name" },
  { name: "get_role", args: { guild_id: GUILD_ID, role_id: ROLE_ID }, resourceId: ROLE_ID, markerField: "name" },
  { name: "get_member", args: { guild_id: GUILD_ID, user_id: USER_ID }, resourceId: USER_ID, markerField: "nick" },
  { name: "get_message", args: { guild_id: GUILD_ID, channel_id: CHANNEL_ID, message_id: MESSAGE_ID }, resourceId: MESSAGE_ID, markerField: "content" },
];

const REST_ROUTES = [
  `/admin/v1/guilds/${GUILD_ID}`,
  `/admin/v1/guilds/${GUILD_ID}/channels/${CHANNEL_ID}`,
  `/admin/v1/guilds/${GUILD_ID}/roles/${ROLE_ID}`,
  `/admin/v1/guilds/${GUILD_ID}/members/${USER_ID}`,
  `/admin/v1/guilds/${GUILD_ID}/channels/${CHANNEL_ID}/messages/${MESSAGE_ID}`,
];

function cannedReceipt(tool: ReadTool) {
  return {
    ok: true,
    result: {
      outcome: "read",
      id: tool.resourceId,
      guild_id: GUILD_ID,
      [tool.markerField]: CANNED_MARKER,
    },
    request_id: "synthetic-admin-read-request",
  };
}

function harness(canned: unknown = {}) {
  const names: string[] = [];
  const forwarded: { method: string; url: string; authorization: string | null; body: string }[] = [];
  const env = {
    REDIRECT_MAPPINGS_JSON: "[]",
    REDIRECT_FALLBACK_CODE: "fallbackCode",
    CF_VERSION_METADATA: { id: "synthetic-admin-read-deployment" },
    MCP_TOKEN,
    TWO_BOT: {
      getByName(name: string) {
        names.push(name);
        return {
          async fetch(req: Request) {
            forwarded.push({ method: req.method, url: req.url,
              authorization: req.headers.get("authorization"), body: await req.text() });
            return Response.json(canned);
          },
        };
      },
    },
  } as unknown as Env;
  const ctx = { waitUntil() {} } as unknown as ExecutionContext;
  return { env, ctx, names, forwarded };
}

let callerSeq = 0;
function callerIp(): string {
  // Each case gets its own documented IPv6 caller, never a shared rate bucket.
  return `2001:db8::${(++callerSeq).toString(16)}`;
}

function mcpRequest(body: unknown, bearer: string | null = MCP_TOKEN): Request {
  const headers: Record<string, string> = {
    "content-type": "application/json",
    "cf-connecting-ip": callerIp(),
  };
  if (bearer !== null) headers["authorization"] = `Bearer ${bearer}`;
  return new Request("https://probe.invalid/mcp", {
    method: "POST",
    headers,
    body: typeof body === "string" ? body : JSON.stringify(body),
  });
}

function restRequest(method: string, path: string): Request {
  return new Request(`https://probe.invalid${path}`, {
    method,
    headers: { "cf-connecting-ip": callerIp() },
  });
}

function toolCall(name: string, args: unknown): Record<string, unknown> {
  return { jsonrpc: "2.0", id: SAFE_ID, method: "tools/call", params: { name, arguments: args } };
}

async function responseBody(res: Response) {
  assert.equal(res.status, 200, "requests use the JSON-RPC response channel");
  assert.match(res.headers.get("content-type") ?? "", /application\/json/);
  return res.json() as Promise<any>;
}

async function assertRefused(input: unknown, code: number, id: string | number | null, message: string) {
  const h = harness();
  const res = await worker.fetch(mcpRequest(input), h.env, h.ctx);
  assert.deepEqual(h.names, [], "invalid input must not resolve the DO");
  assert.deepEqual(h.forwarded, [], "invalid input must not read the container");
  const body = await responseBody(res);
  assert.ok(!JSON.stringify(body).includes(SENTINEL), "refusal must not echo untrusted values");
  assert.ok(!JSON.stringify(body).includes(MCP_TOKEN), "refusal must not echo credentials");
  assert.deepEqual(body, { jsonrpc: "2.0", id, error: { code, message } });
}

test("tools/list advertises the five admin read tools alongside the five phase-1 tools", async () => {
  const h = harness();
  const res = await worker.fetch(
    mcpRequest({ jsonrpc: "2.0", id: SAFE_ID, method: "tools/list", params: {} }),
    h.env, h.ctx,
  );
  const body = await responseBody(res);
  const names = (body.result.tools as { name: string }[]).map((t) => t.name);
  for (const name of [...LEGACY_TOOLS, ...NEW_TOOLS.map((t) => t.name)]) {
    assert.ok(names.includes(name), `tools/list must advertise ${name}`);
  }
});

for (const tool of NEW_TOOLS) {
  test(`tools/list schema for ${tool.name} requires ${Object.keys(tool.args).join(", ")}`, async () => {
    const h = harness();
    const res = await worker.fetch(
      mcpRequest({ jsonrpc: "2.0", id: SAFE_ID, method: "tools/list", params: {} }),
      h.env, h.ctx,
    );
    const body = await responseBody(res);
    const entry = (body.result.tools as any[]).find((t) => t.name === tool.name);
    assert.ok(entry, `${tool.name} must be listed`);
    const schema = entry.inputSchema;
    assert.equal(schema.type, "object");
    assert.equal(schema.additionalProperties, false, "read tools take a closed argument set");
    for (const key of Object.keys(tool.args)) {
      assert.equal(schema.properties?.[key]?.type, "string", `${tool.name} schema must type ${key} as string`);
      assert.ok((schema.required as string[]).includes(key), `${tool.name} schema must require ${key}`);
    }
  });
}

for (const tool of NEW_TOOLS) {
  test(`tools/call ${tool.name} dispatches once and passes the resource payload through`, async () => {
    const h = harness(cannedReceipt(tool));
    const res = await worker.fetch(mcpRequest(toolCall(tool.name, tool.args)), h.env, h.ctx);
    const body = await responseBody(res);
    assert.deepEqual(h.names, ["two-bot"], "read tools use the fenced container singleton");
    assert.equal(h.forwarded.length, 1, "one tool call proxies exactly one container read");
    assert.notEqual(h.forwarded[0]!.authorization, `Bearer ${MCP_TOKEN}`, "the caller bearer must never cross the fence");
    assert.equal(body.id, SAFE_ID);
    assert.ok(body.result, "a valid read returns a result");
    assert.equal(body.result.content?.[0]?.type, "text");
    assert.ok(JSON.stringify(body.result).includes(CANNED_MARKER), "the tool result must carry the container payload");
    assert.ok(!JSON.stringify(body).includes(MCP_TOKEN), "results must not echo credentials");
  });
}

function malformedParams(tool: ReadTool): [string, Record<string, unknown>][] {
  const keys = Object.keys(tool.args);
  const cases: [string, Record<string, unknown>][] = [
    ["omitted arguments", { name: tool.name }],
    ["empty arguments", { name: tool.name, arguments: {} }],
    ["null arguments", { name: tool.name, arguments: null }],
    ["array arguments", { name: tool.name, arguments: [] }],
    ["scalar arguments", { name: tool.name, arguments: SENTINEL }],
    ["wrong-typed guild_id", { name: tool.name, arguments: { ...tool.args, guild_id: 42 } }],
    ["extra key", { name: tool.name, arguments: { ...tool.args, extra_synthetic: SENTINEL } }],
    ["own __proto__ key", { name: tool.name, arguments: { ...tool.args, ...JSON.parse(`{"__proto__":{"value":"${SENTINEL}"}}`) } }],
  ];
  if (keys.length > 1) {
    const last = keys[keys.length - 1]!;
    const dropped = { ...tool.args };
    delete dropped[last];
    cases.push([`missing ${last}`, { name: tool.name, arguments: dropped }]);
  }
  return cases;
}

for (const tool of NEW_TOOLS) {
  for (const [label, params] of malformedParams(tool)) {
    test(`${tool.name}: ${label} is invalid params without dispatch`, async () => {
      await assertRefused(
        { jsonrpc: "2.0", id: SAFE_ID, method: "tools/call", params },
        -32602, SAFE_ID, "invalid params",
      );
    });
  }
}

test("control: an unknown tool name keeps the fixed unknown-tool error", async () => {
  const h = harness();
  const res = await worker.fetch(mcpRequest(toolCall("get_synthetic_never_registered", {})), h.env, h.ctx);
  assert.deepEqual(h.names, [], "unknown tools must not resolve the DO");
  assert.deepEqual(h.forwarded, [], "unknown tools must not read the container");
  const body = await responseBody(res);
  assert.deepEqual(body, { jsonrpc: "2.0", id: SAFE_ID, error: { code: -32601, message: "unknown tool" } });
});

test("control: tools/call without a bearer is unauthorized before dispatch", async () => {
  const h = harness(cannedReceipt(NEW_TOOLS[0]!));
  const res = await worker.fetch(mcpRequest(toolCall(NEW_TOOLS[0]!.name, NEW_TOOLS[0]!.args), null), h.env, h.ctx);
  assert.equal(res.status, 401);
  assert.equal(res.headers.get("www-authenticate"), "Bearer");
  assert.deepEqual(h.names, [], "unauthenticated calls must not resolve the DO");
  assert.deepEqual(h.forwarded, [], "unauthenticated calls must never reach the container");
});

test("control: tools/list without a bearer is unauthorized", async () => {
  const h = harness();
  const res = await worker.fetch(
    mcpRequest({ jsonrpc: "2.0", id: SAFE_ID, method: "tools/list", params: {} }, null),
    h.env, h.ctx,
  );
  assert.equal(res.status, 401);
  assert.equal(res.headers.get("www-authenticate"), "Bearer");
  assert.deepEqual(h.names, []);
  assert.deepEqual(h.forwarded, []);
});

test("control: an oversize body keeps the fixed non-echoing parse error", async () => {
  const h = harness();
  const res = await worker.fetch(mcpRequest("x".repeat(70 * 1024)), h.env, h.ctx);
  assert.deepEqual(h.names, [], "refused intake must not resolve the DO");
  assert.deepEqual(h.forwarded, [], "refused intake must not reach the container");
  const body = await responseBody(res);
  assert.deepEqual(body, { jsonrpc: "2.0", id: null, error: { code: -32700, message: "parse error" } });
});

test("control: a malformed JSON body keeps the fixed parse error", async () => {
  const h = harness();
  const res = await worker.fetch(mcpRequest("{not json"), h.env, h.ctx);
  assert.deepEqual(h.names, []);
  assert.deepEqual(h.forwarded, []);
  const body = await responseBody(res);
  assert.deepEqual(body, { jsonrpc: "2.0", id: null, error: { code: -32700, message: "parse error" } });
});

test("control: get_status still proxies its container read", async () => {
  const health = { status: "synthetic-healthy", probe: CANNED_MARKER };
  const h = harness(health);
  const res = await worker.fetch(mcpRequest(toolCall("get_status", {})), h.env, h.ctx);
  const body = await responseBody(res);
  assert.equal(h.forwarded.length, 1, "one tool call proxies exactly one container read");
  assert.deepEqual(body.result?.structuredContent, health);
});

test("control: get_config still answers Worker-side without a container read", async () => {
  const h = harness();
  const res = await worker.fetch(mcpRequest(toolCall("get_config", {})), h.env, h.ctx);
  const body = await responseBody(res);
  assert.deepEqual(h.forwarded, [], "get_config must not touch the container");
  assert.equal(body.result?.structuredContent?.build_revision, "synthetic-admin-read-deployment");
});

test("control: GET /mcp still refuses without executing tools", async () => {
  const h = harness();
  const res = await worker.fetch(
    new Request("https://probe.invalid/mcp", { method: "GET", headers: { "cf-connecting-ip": callerIp() } }),
    h.env, h.ctx,
  );
  assert.equal(res.status, 405);
  assert.equal(res.headers.get("allow"), "POST");
  assert.deepEqual(h.names, []);
  assert.deepEqual(h.forwarded, []);
});

for (const route of REST_ROUTES) {
  test(`composition: GET ${route} without a bearer is unauthorized`, async () => {
    const h = harness();
    const res = await worker.fetch(restRequest("GET", route), h.env, h.ctx);
    assert.equal(res.status, 401);
    assert.equal(res.headers.get("www-authenticate"), "Bearer");
    assert.deepEqual(h.names, []);
    assert.deepEqual(h.forwarded, []);
  });
}
