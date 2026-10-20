import { test, type TestContext } from "node:test";
import assert from "node:assert/strict";
import { DatabaseSync } from "node:sqlite";
import worker, { TwoBotContainer, type Env } from "../src/index.ts";
import { MCP_AUDIT_READ_HEADER } from "../src/mcp.ts";
import { DEPLOYMENT_HEADER, OWNER_KEY } from "../src/ownership.ts";

const ID = "synthetic-audit-deployment";
const MCP_TOKEN = "synthetic-mcp-audit-routing-0123456789";
const OPS_TOKEN = "synthetic-ops-audit-routing-0123456789";
const excerpt = { available: true, rows: [], truncated: false };

async function harness(t: TestContext) {
  const db = new DatabaseSync(":memory:");
  t.after(() => db.close());
  const values = new Map<string, unknown>([[OWNER_KEY, {
    deploymentId: ID, epoch: 1, phase: "active", actor: "fixture-operator",
    timestamp: "2026-10-01T00:00:00.000Z", oldEpoch: 0, oldDeploymentId: null,
  }]]);
  const gates: Promise<unknown>[] = [];
  const context = {
    id: { toString: () => "synthetic-audit-do" },
    container: { running: false },
    storage: {
      get: async (key: string) => values.get(key),
      list: async ({ prefix }: { prefix: string }) => new Map([...values].filter(([key]) => key.startsWith(prefix))),
      put: async () => { throw new Error("read-only audit must not write storage"); },
      kv: { get: (key: string) => values.get(key) },
      sql: { exec: (query: string, ...bindings: (string | number | null)[]) => db.prepare(query).all(...bindings) },
      setAlarm: async () => {}, deleteAlarm: async () => {}, sync: async () => {},
    },
    blockConcurrencyWhile<T>(callback: () => Promise<T>) {
      const promise = callback();
      gates.push(promise);
      return promise;
    },
  };
  const env = {
    CF_VERSION_METADATA: { id: ID }, MCP_TOKEN, OPS_CONTROL_TOKEN: OPS_TOKEN,
    REDIRECT_MAPPINGS_JSON: "[]", REDIRECT_FALLBACK_CODE: "fallbackCode",
  } as unknown as Env;
  const bot = new TwoBotContainer(context as unknown as DurableObjectState<{}>, env);
  await Promise.all(gates);
  const containerReads: { url: string; headers: Headers }[] = [];
  t.mock.method(bot, "containerFetch", async (input: Request | string) => {
    const request = typeof input === "string" ? new Request(input) : input;
    containerReads.push({ url: request.url, headers: request.headers });
    return Response.json(excerpt);
  });
  const forwarded: Request[] = [];
  env.TWO_BOT = { getByName: () => ({ fetch: (request: Request) => {
    forwarded.push(request);
    return bot.fetch(request);
  } }) } as unknown as DurableObjectNamespace;
  const ctx = { waitUntil() {} } as unknown as ExecutionContext;
  return { bot, env, ctx, containerReads, forwarded };
}

test("MCP traverses the real DO audit dispatch, preserving ownership and stripping caller headers", async (t) => {
  const h = await harness(t);
  const response = await worker.fetch(new Request("https://ops.invalid/mcp", {
    method: "POST", headers: { authorization: `Bearer ${MCP_TOKEN}`, "cf-connecting-ip": "203.0.113.81" },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/call", params: { name: "get_audit" } }),
  }), h.env, h.ctx);
  const body = await response.json() as any;
  assert.deepEqual(body.result?.structuredContent, excerpt);
  assert.equal(h.containerReads.length, 1);
  assert.equal(h.containerReads[0]!.url, "http://c/ops/audit");
  assert.equal(h.containerReads[0]!.headers.get("authorization"), null);
  assert.equal(h.containerReads[0]!.headers.get(MCP_AUDIT_READ_HEADER), null);
  assert.equal(h.forwarded[0]!.headers.get(DEPLOYMENT_HEADER), ID);
});

test("MCP internal audit dispatch cannot bypass a mismatched deployment", async (t) => {
  const h = await harness(t);
  const response = await h.bot.fetch(new Request("http://c/ops/audit", {
    headers: { [MCP_AUDIT_READ_HEADER]: "1", [DEPLOYMENT_HEADER]: "synthetic-wrong-deployment" },
  }));
  assert.equal(response.status, 503);
  assert.equal(h.containerReads.length, 0);
});

test("public audit keeps its admin bearer gate and strips spoofed internal dispatch", async (t) => {
  const h = await harness(t);
  const forged = { [MCP_AUDIT_READ_HEADER]: "1", [DEPLOYMENT_HEADER]: ID, "cf-connecting-ip": "203.0.113.82" };
  const refused = await worker.fetch(new Request("http://c/ops/audit", { headers: forged }), h.env, h.ctx);
  assert.equal(refused.status, 401);
  assert.equal(h.forwarded.length, 0);
  const allowed = await worker.fetch(new Request("http://c/ops/audit", {
    headers: { ...forged, authorization: `Bearer ${OPS_TOKEN}` },
  }), h.env, h.ctx);
  assert.equal(allowed.status, 200);
  const body = await allowed.json() as any;
  assert.equal(body.available, undefined, "public route remains the DO control audit, not the Rust excerpt");
  assert.equal(h.forwarded[0]!.headers.get(MCP_AUDIT_READ_HEADER), null);
  assert.equal(h.containerReads.length, 0);
});
