/**
 * TOG-22862: admin control surface behind the least-privilege ops token.
 * Worker-gate tests use synthetic bindings only (the stub DO records what
 * the Worker forwards); OpsControl state tests use Map-backed storage.
 * No endpoint, database or secret is contacted.
 * Run: node --import ./test/cloudflare-loader.mjs --test test/ops-control.test.ts
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import worker, { type Env } from "../src/index.ts";
import {
  MAX_AUDIT_ROWS,
  OPS_AUDIT_PATH,
  OPS_CONFIG_PATH,
  OPS_KILLSWITCH_PATH,
  OpsControl,
  OpsRefused,
  parseConfigPut,
  parseKillswitchPost,
} from "../src/ops-control.ts";

const TOKEN = "synthetic-ops-control-token-0123456789";

function harness() {
  const forwarded: { method: string; url: string; headers: Record<string, string>; body: string }[] = [];
  const names: string[] = [];
  const env = {
    REDIRECT_MAPPINGS_JSON: "[]",
    REDIRECT_FALLBACK_CODE: "fallbackCode",
    CF_VERSION_METADATA: { id: "synthetic-ops-deployment" },
    TWO_AUTOMATIONS: "1",
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
            return Response.json({ deploymentId: "synthetic-ops-deployment", ok: true });
          },
        };
      },
    },
  } as unknown as Env;
  const ctx = { waitUntil() {} } as unknown as ExecutionContext;
  return { env, ctx, forwarded, names };
}

const authed = (path: string, init?: RequestInit) =>
  new Request(`https://ops.invalid${path}`, {
    ...init,
    headers: { authorization: `Bearer ${TOKEN}`, ...(init?.headers ?? {}) },
  });

test("dark without a configured token: all three paths 404 and never touch the DO", async () => {
  const h = harness();
  for (const path of [OPS_CONFIG_PATH, OPS_KILLSWITCH_PATH, OPS_AUDIT_PATH]) {
    const res = await worker.fetch(new Request(`https://ops.invalid${path}`), h.env, h.ctx);
    assert.equal(res.status, 404);
    assert.equal(await res.text(), "not found\n");
  }
  assert.deepEqual(h.names, []);
  assert.deepEqual(h.forwarded, []);
});

test("short token 404s as not configured with one redacted log line", async () => {
  const h = harness();
  const errors: string[] = [];
  const orig = console.error;
  console.error = (...args: unknown[]) => { errors.push(args.map(String).join(" ")); };
  try {
    const res = await worker.fetch(
      authed(OPS_CONFIG_PATH),
      { ...h.env, OPS_CONTROL_TOKEN: "short" } as Env,
      h.ctx,
    );
    assert.equal(res.status, 404);
    assert.equal(h.forwarded.length, 0);
  } finally {
    console.error = orig;
  }
  assert.equal(errors.length, 1);
  assert.ok(errors[0]!.includes("ops_control_token_misconfigured"));
  assert.ok(!errors[0]!.includes("short"));
});

test("missing or wrong bearer is 401 and never reaches the DO", async () => {
  const h = harness();
  const env = { ...h.env, OPS_CONTROL_TOKEN: TOKEN } as Env;
  assert.equal((await worker.fetch(new Request("https://ops.invalid/ops/config"), env, h.ctx)).status, 401);
  const wrong = await worker.fetch(
    new Request("https://ops.invalid/ops/audit", { headers: { authorization: "Bearer wrong" } }),
    env,
    h.ctx,
  );
  assert.equal(wrong.status, 401);
  assert.deepEqual(h.forwarded, []);
});

test("wrong methods are 405 with an Allow header before any auth comparison", async () => {
  const h = harness();
  const env = { ...h.env, OPS_CONTROL_TOKEN: TOKEN } as Env;
  const cases: [string, string, string][] = [
    [OPS_CONFIG_PATH, "POST", "GET, PUT"],
    [OPS_CONFIG_PATH, "DELETE", "GET, PUT"],
    [OPS_KILLSWITCH_PATH, "PUT", "GET, POST"],
    [OPS_AUDIT_PATH, "POST", "GET"],
  ];
  for (const [path, method, allow] of cases) {
    const res = await worker.fetch(new Request(`https://ops.invalid${path}`, { method }), env, h.ctx);
    assert.equal(res.status, 405, `${method} ${path}`);
    assert.equal(res.headers.get("allow"), allow);
    await res.text();
  }
  assert.deepEqual(h.forwarded, [], "refused methods must not reach the DO");
});

test("query strings are not this surface: 404 without touching the DO", async () => {
  const h = harness();
  const env = { ...h.env, OPS_CONTROL_TOKEN: TOKEN } as Env;
  const res = await worker.fetch(authed(`${OPS_CONFIG_PATH}?x=1`), env, h.ctx);
  assert.equal(res.status, 404);
  assert.deepEqual(h.forwarded, []);
});

test("PUT /ops/config is dark unless the writes switch is exactly 1", async () => {
  const h = harness();
  const put = (env: Env) =>
    worker.fetch(
      authed(OPS_CONFIG_PATH, {
        method: "PUT",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ overrides: {}, expectedEpoch: 0, actor: "fixture-operator" }),
      }),
      env,
      h.ctx,
    );
  assert.equal((await put({ ...h.env, OPS_CONTROL_TOKEN: TOKEN } as Env)).status, 404);
  assert.equal((await put({ ...h.env, OPS_CONTROL_TOKEN: TOKEN, OPS_CONFIG_WRITES_ENABLED: "0" } as Env)).status, 404);
  assert.deepEqual(h.forwarded, [], "dark writes must not reach the DO");
});

test("PUT /ops/config forwards when the writes switch is exactly 1", async () => {
  const h = harness();
  const open = await worker.fetch(
    authed(OPS_CONFIG_PATH, {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ overrides: {}, expectedEpoch: 0, actor: "fixture-operator" }),
    }),
    { ...h.env, OPS_CONTROL_TOKEN: TOKEN, OPS_CONFIG_WRITES_ENABLED: "1" } as Env,
    h.ctx,
  );
  assert.equal(open.status, 200);
  assert.equal(h.forwarded.length, 1);
  const [fwd] = h.forwarded;
  assert.equal(fwd?.method, "PUT");
});

test("POST /ops/kill-switch is dark unless its switch is exactly 1", async () => {
  const h = harness();
  const post = (env: Env) =>
    worker.fetch(
      authed(OPS_KILLSWITCH_PATH, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          fences: { automation: true, automod: false, publish: false, gateway_park: false },
          expectedEpoch: 0,
          actor: "fixture-operator",
        }),
      }),
      env,
      h.ctx,
    );
  assert.equal((await post({ ...h.env, OPS_CONTROL_TOKEN: TOKEN } as Env)).status, 404);
  assert.deepEqual(h.forwarded, []);
});

test("POST /ops/kill-switch forwards when its switch is exactly 1", async () => {
  const h = harness();
  const open = await worker.fetch(
    authed(OPS_KILLSWITCH_PATH, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        fences: { automation: true, automod: false, publish: false, gateway_park: false },
        expectedEpoch: 0,
        actor: "fixture-operator",
      }),
    }),
    { ...h.env, OPS_CONTROL_TOKEN: TOKEN, OPS_KILLSWITCH_ENABLED: "1" } as Env,
    h.ctx,
  );
  assert.equal(open.status, 200);
  assert.equal(h.forwarded.length, 1);
  const [fwd] = h.forwarded;
  assert.equal(fwd?.method, "POST");
});

test("authenticated GET stamps the deployment id; the DO re-checks the bearer", async () => {
  const h = harness();
  const env = { ...h.env, OPS_CONTROL_TOKEN: TOKEN } as Env;
  const res = await worker.fetch(authed(OPS_AUDIT_PATH), env, h.ctx);
  assert.equal(res.status, 200);
  assert.equal(h.forwarded.length, 1);
  const [fwd] = h.forwarded;
  assert.equal(fwd?.method, "GET");
  assert.equal(new URL(fwd?.url ?? "").pathname, OPS_AUDIT_PATH);
  // The Worker-stamped deployment id is always overwritten server-side; the
  // bearer travels with the forward so the DO re-authenticates (defense in
  // depth, mirroring CONTROL_PATH and OPS_METRICS_PATH).
  assert.equal(fwd?.headers["x-two-bot-deployment-id"], "synthetic-ops-deployment");
  assert.equal(fwd?.headers["authorization"], `Bearer ${TOKEN}`);
});

test("repeated wrong bearers throttle per caller; other callers unaffected", async () => {
  const h = harness();
  const env = { ...h.env, OPS_CONTROL_TOKEN: TOKEN } as Env;
  const get = (ip: string, auth?: string) =>
    worker.fetch(
      new Request("https://ops.invalid/ops/config", {
        headers: { ...(auth ? { authorization: auth } : {}), "cf-connecting-ip": ip },
      }),
      env,
      h.ctx,
    );
  for (let i = 0; i < 10; i++) {
    assert.equal((await get("10.20.30.40", "Bearer wrong")).status, 401, `failure ${i}`);
  }
  const throttled = await get("10.20.30.40", "Bearer wrong");
  assert.equal(throttled.status, 429);
  assert.ok(Number(throttled.headers.get("retry-after")) >= 1);
  const guessedRight = await get("10.20.30.40", `Bearer ${TOKEN}`);
  assert.equal(guessedRight.status, 429, "throttled caller is refused without a comparison");
  const other = await get("10.20.30.41", `Bearer ${TOKEN}`);
  assert.equal(other.status, 200);
  assert.equal(h.forwarded.length, 1);
});

test("config PUT rejects unknown keys, bad actors and bad epochs without storage", () => {
  assert.equal(parseConfigPut({ overrides: { TWO_NOPE: "1" }, expectedEpoch: 0, actor: "a" }), null);
  assert.equal(parseConfigPut({ overrides: {}, expectedEpoch: -1, actor: "a" }), null);
  assert.equal(parseConfigPut({ overrides: {}, expectedEpoch: 0, actor: "" }), null);
  assert.equal(parseConfigPut({ overrides: { TWO_AUTOMATIONS: "x".repeat(4097) }, expectedEpoch: 0, actor: "a" }), null);
  assert.equal(parseConfigPut({ overrides: "nope", expectedEpoch: 0, actor: "a" }), null);
  assert.deepEqual(
    parseConfigPut({ overrides: { TWO_AUTOMATIONS: "0" }, expectedEpoch: 0, actor: "fixture-operator" }),
    { overrides: { TWO_AUTOMATIONS: "0" }, expectedEpoch: 0, actor: "fixture-operator" },
  );
});

test("kill-switch POST rejects partial fences and non-boolean values", () => {
  assert.equal(
    parseKillswitchPost({ fences: { automation: true }, expectedEpoch: 0, actor: "a" }),
    null,
    "partial fences are refused",
  );
  assert.equal(
    parseKillswitchPost({
      fences: { automation: true, automod: "yes", publish: false, gateway_park: false },
      expectedEpoch: 0,
      actor: "a",
    }),
    null,
  );
  assert.deepEqual(
    parseKillswitchPost({
      fences: { automation: true, automod: false, publish: false, gateway_park: true },
      expectedEpoch: 0,
      actor: "fixture-operator",
    }),
    {
      fences: { automation: true, automod: false, publish: false, gateway_park: true },
      expectedEpoch: 0,
      actor: "fixture-operator",
    },
  );
});

function memoryStorage() {
  const map = new Map<string, unknown>();
  return {
    map,
    get: (async (key: string | string[]) => {
      if (Array.isArray(key)) {
        const out = new Map<string, unknown>();
        for (const k of key) if (map.has(k)) out.set(k, map.get(k));
        return out;
      }
      return map.get(key);
    }) as DurableObjectStorage["get"],
    put: (async (entries: Record<string, unknown> | string, value?: unknown) => {
      if (typeof entries === "string") map.set(entries, value);
      else for (const [k, v] of Object.entries(entries)) map.set(k, v);
    }) as DurableObjectStorage["put"],
    list: (async (opts?: { prefix?: string; limit?: number; reverse?: boolean }) => {
      let keys = [...map.keys()].filter((k) => !opts?.prefix || k.startsWith(opts.prefix)).sort();
      if (opts?.reverse) keys = keys.reverse();
      if (opts?.limit !== undefined) keys = keys.slice(0, opts.limit);
      return new Map(keys.map((k) => [k, map.get(k)]));
    }) as DurableObjectStorage["list"],
  };
}

function opsControl(env: Record<string, unknown> = { TWO_AUTOMATIONS: "1" }) {
  const storage = memoryStorage();
  return { storage, ops: new OpsControl(storage as unknown as DurableObjectStorage, env) };
}

test("config PUT commits at old+1 with an audit row; stale epochs conflict with no new row", async () => {
  const { storage, ops } = opsControl();
  const pristine = await ops.readConfig();
  assert.equal(pristine.epoch, 0);
  assert.deepEqual(pristine.effective, { TWO_AUTOMATIONS: "1" });
  const record = await ops.putConfig({
    overrides: { TWO_AUTOMATIONS: "0" },
    expectedEpoch: 0,
    actor: "fixture-operator",
  });
  assert.equal(record.epoch, 1);
  assert.deepEqual(record.overrides, { TWO_AUTOMATIONS: "0" });
  assert.ok(Number.isFinite(Date.parse(record.timestamp)));
  const after = await ops.readConfig();
  assert.equal(after.epoch, 1);
  assert.deepEqual(after.effective, { TWO_AUTOMATIONS: "0" });
  const audit = await ops.listAudit();
  assert.equal(audit.length, 1);
  assert.equal(audit[0]!.actor, "fixture-operator");
  assert.equal(audit[0]!.action, "config_put");
  assert.equal(audit[0]!.epoch, 1);
  assert.ok(Number.isFinite(Date.parse(audit[0]!.timestamp)));
  await assert.rejects(
    ops.putConfig({ overrides: {}, expectedEpoch: 0, actor: "fixture-operator" }),
    (error: unknown) => error instanceof OpsRefused && error.reason === "epoch_conflict",
  );
  assert.equal((await ops.listAudit()).length, 1, "conflicts write no audit row");
  assert.ok([...storage.map.keys()].filter((k) => k.startsWith("two-bot:ops-audit:v1:")).length === 1);
});

test("kill-switch POST stores fences with epoch semantics and audits every mutation", async () => {
  const { ops } = opsControl();
  const pristine = await ops.readKillswitch();
  assert.equal(pristine.epoch, 0);
  assert.deepEqual(pristine.fences, { automation: false, automod: false, publish: false, gateway_park: false });
  const first = await ops.postKillswitch({
    fences: { automation: true, automod: true, publish: false, gateway_park: true },
    expectedEpoch: 0,
    actor: "fixture-operator",
  });
  assert.equal(first.epoch, 1);
  assert.deepEqual((await ops.readKillswitch()).fences, first.fences);
  const second = await ops.postKillswitch({
    fences: { automation: false, automod: false, publish: false, gateway_park: false },
    expectedEpoch: 1,
    actor: "fixture-operator",
  });
  assert.equal(second.epoch, 2);
  const rows = await ops.listAudit();
  assert.equal(rows.length, 2);
  assert.deepEqual(rows.map((r) => r.action), ["kill_switch", "kill_switch"]);
  assert.deepEqual(rows.map((r) => r.epoch), [2, 1], "newest first");
});

test("audit listing is bounded and append-only: keys are unique per mutation", async () => {
  const { storage, ops } = opsControl();
  await ops.putConfig({ overrides: {}, expectedEpoch: 0, actor: "a" });
  await ops.postKillswitch({
    fences: { automation: false, automod: false, publish: false, gateway_park: false },
    expectedEpoch: 0,
    actor: "a",
  });
  const keys = [...storage.map.keys()].filter((k) => k.startsWith("two-bot:ops-audit:v1:"));
  assert.equal(keys.length, 2);
  assert.equal(new Set(keys).size, 2, "audit keys never collide");
  assert.ok((await ops.listAudit(1)).length === 1, "limit is honored");
  assert.ok((await ops.listAudit(1_000_000)).length === 2, `over-limit clamps to ${MAX_AUDIT_ROWS}`);
});

test("corrupt or unreadable state fails closed with no token in the refusal", async () => {
  const bad = opsControl();
  bad.storage.map.set("two-bot:ops-config:v1", { epoch: "one" });
  await assert.rejects(bad.ops.readConfig(), (error: unknown) =>
    error instanceof OpsRefused && error.reason === "storage_invalid");
  const failing = new OpsControl({
    get: async () => { throw new Error("synthetic storage failure"); },
    put: async () => { throw new Error("synthetic storage failure"); },
    list: async () => new Map(),
  } as unknown as DurableObjectStorage, {});
  await assert.rejects(failing.readConfig(), (error: unknown) =>
    error instanceof OpsRefused && error.reason === "storage_unavailable");
  await assert.rejects(
    failing.putConfig({ overrides: {}, expectedEpoch: 0, actor: "a" }),
    (error: unknown) => error instanceof OpsRefused && error.reason === "storage_unavailable",
  );
});
