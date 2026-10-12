/**
 * Read-only ops dashboard (TOG-22861): static ungated Worker-served page
 * polling the gated `GET /ops/summary`, human auth via Cloudflare Access
 * service tokens on the summary only (both headers, strict mode),
 * strictly read-only, staging + production workers.dev break-glass until a
 * custom domain exists (pinned by scripts/test-env-bindings.py). Synthetic
 * bindings only — no endpoint, database or secret is contacted.
 *
 * Run: node --import ./test/cloudflare-loader.mjs --test test/ops-dashboard.test.ts
 */

import { test, type TestContext } from "node:test";
import assert from "node:assert/strict";
import { DatabaseSync } from "node:sqlite";
import worker, { TwoBotContainer, type Env } from "../src/index.ts";
import {
  DASHBOARD_CLIENT_ID_HEADER,
  DASHBOARD_CLIENT_SECRET_HEADER,
  OPS_DASHBOARD_PATH,
  OPS_SUMMARY_PATH,
} from "../src/ops-dashboard.ts";
import { isReservedInternal } from "../src/redirect.ts";
import { DEPLOYMENT_HEADER, OWNER_KEY } from "../src/ownership.ts";

const DEPLOYMENT = "synthetic-dashboard-deployment";
const CLIENT_ID = "synthetic-dashboard-client-id-0123456789";
const CLIENT_SECRET = "synthetic-dashboard-client-secret-0123456789abcdef";

function workerEnv(secrets?: { id?: string; secret?: string }) {
  const calls: Request[] = [];
  const env = {
    CF_VERSION_METADATA: { id: DEPLOYMENT },
    OPS_DASHBOARD_CLIENT_ID: secrets?.id,
    OPS_DASHBOARD_CLIENT_SECRET: secrets?.secret,
    REDIRECT_MAPPINGS_JSON: "[]",
    TWO_BOT: {
      getByName: () => ({
        fetch: async (r: Request) => {
          calls.push(r);
          return new Response(
            '{"components":[["process","ready"],["gateway","down"]],"jobs":{},"build_revision":"synthetic","build_id":"synthetic"}',
            {
              status: 503,
              headers: { "content-type": "application/json" },
            },
          );
        },
      }),
    },
  } as unknown as Env;
  return { env, calls };
}

const ctx = {} as ExecutionContext;
// The attempt cap is per caller: isolate every request on its own caller so
// one case cannot spend another's budget (the cap itself gets its own test).
let callerSeq = 0;
const freshCaller = () => `test-caller-${++callerSeq}`;
const creds = {
  [DASHBOARD_CLIENT_ID_HEADER]: CLIENT_ID,
  [DASHBOARD_CLIENT_SECRET_HEADER]: CLIENT_SECRET,
};
const configured = () => workerEnv({ id: CLIENT_ID, secret: CLIENT_SECRET });
const get = (env: Env, path: string, init?: RequestInit) => {
  const headers = new Headers(init?.headers);
  if (!headers.has("cf-connecting-ip")) headers.set("cf-connecting-ip", freshCaller());
  return worker.fetch(new Request(`https://w.invalid${path}`, { ...init, headers }), env, ctx);
};

test(`${OPS_SUMMARY_PATH}: 404 when the service-token pair is not configured`, async () => {
  for (const secrets of [
    undefined,
    { id: CLIENT_ID },
    { secret: CLIENT_SECRET },
    { id: "short", secret: CLIENT_SECRET },
    { id: CLIENT_ID, secret: "short" },
  ]) {
    const { env, calls } = workerEnv(secrets);
    const res = await get(env, OPS_SUMMARY_PATH, { headers: creds });
    assert.equal(res.status, 404, `${OPS_SUMMARY_PATH} ${JSON.stringify(secrets)} must 404`);
    await res.text();
    assert.equal(calls.length, 0, "unconfigured routes never reach the container");
  }
});

test(`${OPS_SUMMARY_PATH}: 401 without or with wrong service-token headers`, async () => {
  const { env, calls } = configured();
  assert.equal((await get(env, OPS_SUMMARY_PATH)).status, 401);
  assert.equal((await get(env, OPS_SUMMARY_PATH, { headers: { [DASHBOARD_CLIENT_ID_HEADER]: CLIENT_ID } })).status, 401);
  assert.equal((await get(env, OPS_SUMMARY_PATH, { headers: { [DASHBOARD_CLIENT_SECRET_HEADER]: CLIENT_SECRET } })).status, 401);
  assert.equal(
    (await get(env, OPS_SUMMARY_PATH, { headers: { [DASHBOARD_CLIENT_ID_HEADER]: "wrong", [DASHBOARD_CLIENT_SECRET_HEADER]: CLIENT_SECRET } })).status,
    401,
  );
  assert.equal(
    (await get(env, OPS_SUMMARY_PATH, { headers: { [DASHBOARD_CLIENT_ID_HEADER]: CLIENT_ID, [DASHBOARD_CLIENT_SECRET_HEADER]: "wrong" } })).status,
    401,
  );
  assert.equal(
    (await get(env, OPS_SUMMARY_PATH, { headers: { [DASHBOARD_CLIENT_ID_HEADER]: "x".repeat(4097), [DASHBOARD_CLIENT_SECRET_HEADER]: CLIENT_SECRET } })).status,
    401,
  );
  assert.equal(calls.length, 0, "unauthenticated requests never reach the container");
});

test(`${OPS_SUMMARY_PATH}: non-GET methods 404 and never reach the container`, async () => {
  const { env, calls } = configured();
  for (const method of ["POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"]) {
    const res = await get(env, OPS_SUMMARY_PATH, { method, headers: creds });
    assert.equal(res.status, 404, `${method} ${OPS_SUMMARY_PATH} must 404`);
    await res.text();
  }
  assert.equal(calls.length, 0, "mutations must never reach the container");
});

test(`${OPS_DASHBOARD_PATH}: static page is ungated so a browser can open it`, async () => {
  const { env, calls } = configured();
  for (const init of [{}, { headers: creds }]) {
    const res = await get(env, OPS_DASHBOARD_PATH, init);
    assert.equal(res.status, 200, "page must load with or without headers");
    await res.text();
  }
  assert.equal(calls.length, 0, "serving the page must not touch the container");
  const { env: bare } = workerEnv(undefined);
  const unconfigured = await get(bare, OPS_DASHBOARD_PATH);
  assert.equal(unconfigured.status, 200, "page stays reachable when secrets are unset");
  await unconfigured.text();
});

test(`${OPS_DASHBOARD_PATH}: non-GET methods 404 and never reach the container`, async () => {
  const { env, calls } = configured();
  for (const method of ["POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"]) {
    const res = await get(env, OPS_DASHBOARD_PATH, { method });
    assert.equal(res.status, 404, `${method} ${OPS_DASHBOARD_PATH} must 404`);
    await res.text();
  }
  assert.equal(calls.length, 0, "mutations must never reach the container");
});

test(`${OPS_DASHBOARD_PATH}: serves the static read-only page with the right headers`, async () => {
  const { env, calls } = configured();
  const res = await get(env, OPS_DASHBOARD_PATH);
  assert.equal(res.status, 200);
  assert.equal(res.headers.get("content-type"), "text/html; charset=utf-8");
  assert.equal(res.headers.get("cache-control"), "no-store");
  assert.equal(res.headers.get("x-content-type-options"), "nosniff");
  assert.equal(res.headers.get("referrer-policy"), "no-referrer");
  assert.equal(res.headers.get("x-frame-options"), "DENY");
  const html = await res.text();
  assert.ok(html.includes('fetch("/ops/summary"'), "page must poll the summary");
  assert.ok(html.includes("cf-access-client-id"), "page must send the service-token id");
  assert.ok(html.includes("cf-access-client-secret"), "page must send the service-token secret");
  assert.ok(!html.includes("localStorage"), "credentials stay in memory, never stored");
  for (const token of ['"POST"', '"PUT"', '"DELETE"', '"PATCH"', "method:"]) {
    assert.ok(!html.includes(token), `page must be strictly read-only (found ${token})`);
  }
  // Pins the /readyz shape: readiness comes from components (or the 200 vs
  // 503 status), never a `ready` field the bot never sends.
  assert.ok(!html.includes("data.ready"), "page must not read a ready field");
  assert.ok(html.includes("application/json"), "page must check the JSON content type");
  assert.ok(html.includes("res.status === 503"), "page must render the 503 readiness report");
  assert.ok(html.includes("status === 200"), "page must derive readiness from the status");
  assert.equal(calls.length, 0, "serving the page must not touch the container");
});

test(`${OPS_SUMMARY_PATH}: forwards to the DO with the stamped deployment id`, async () => {
  const { env, calls } = configured();
  const res = await get(env, OPS_SUMMARY_PATH, { headers: creds });
  assert.equal(res.status, 503);
  assert.equal(calls.length, 1);
  assert.equal(calls[0]!.headers.get(DEPLOYMENT_HEADER), DEPLOYMENT);
  assert.equal(new URL(calls[0]!.url).pathname, OPS_SUMMARY_PATH);
});

test("dashboard attempts are per-caller throttled before any comparison", async () => {
  const { env, calls } = configured();
  const ip = freshCaller();
  const attempt = () => get(env, OPS_SUMMARY_PATH, { headers: { ...creds, "cf-connecting-ip": ip } });
  for (let i = 0; i < 10; i++) {
    const res = await attempt();
    assert.equal(res.status, 503, `attempt ${i + 1} must forward`);
    await res.text();
  }
  const shed = await attempt();
  assert.equal(shed.status, 429);
  assert.ok(Number(shed.headers.get("retry-after")) >= 1);
  await shed.text();
  assert.equal(calls.length, 10, "throttled attempts never reach the container");
});

test("/ops/* paths are reserved and never become invite campaigns", () => {
  for (const path of ["/ops", "/ops/", "/ops/dashboard", "/ops/summary", "/ops/metrics", "/OPS/SUMMARY", "/ops/dashboard/extra"]) {
    assert.ok(isReservedInternal(path), `${path} must be reserved`);
  }
  assert.ok(!isReservedInternal("/opsfoo"), "near-miss slugs still resolve");
});

const DO_ID = "synthetic-dashboard-do-deployment";
const DO_ACTIVE_OWNER = {
  deploymentId: DO_ID,
  epoch: 1,
  phase: "active",
  actor: "fixture-operator",
  timestamp: "2026-10-01T00:00:00.000Z",
  oldEpoch: 0,
  oldDeploymentId: null,
};

async function summaryDoHarness(t: TestContext) {
  const database = new DatabaseSync(":memory:");
  t.after(() => database.close());
  for (const method of ["log", "warn", "error"] as const) {
    t.mock.method(console, method, () => {});
  }
  const values = new Map<string, unknown>([[OWNER_KEY, DO_ACTIVE_OWNER]]);
  const gates: Promise<unknown>[] = [];
  const ctx = {
    exports: {
      ContainerProxy: () => ({
        fetch: () => {
          throw new Error("unexpected outbound request");
        },
      }),
    },
    id: { toString: () => "synthetic-dashboard-do-id" },
    container: {
      running: false,
      async destroy() {},
      start() {},
      monitor: () => new Promise<void>(() => {}),
      getTcpPort: () => ({
        fetch: async () => new Response(null, { status: 500 }),
      }),
    },
    storage: {
      get: async (key: string) => values.get(key),
      put: async (key: string | Record<string, unknown>, value?: unknown) => {
        if (typeof key === "string") values.set(key, value);
        else for (const [k, v] of Object.entries(key)) values.set(k, v);
      },
      delete: async (key: string) => {
        values.delete(key);
      },
      kv: {
        get: (key: string) => values.get(key),
        put: (key: string, value: unknown) => {
          values.set(key, value);
        },
        delete: (key: string) => {
          values.delete(key);
        },
      },
      sql: {
        exec(query: string, ...bindings: (string | number | null)[]) {
          return database.prepare(query).all(...bindings);
        },
      },
      setAlarm: async () => {},
      deleteAlarm: async () => {},
      sync: async () => {},
    },
    blockConcurrencyWhile<T>(callback: () => Promise<T>) {
      const promise = callback();
      gates.push(promise);
      return promise;
    },
  };
  const bot = new TwoBotContainer(
    ctx as unknown as DurableObjectState<{}>,
    { CF_VERSION_METADATA: { id: DO_ID } } as Env,
  );
  await Promise.all(gates);
  return { bot };
}

function summaryRequest(): Request {
  const request = new Request(`https://worker.invalid${OPS_SUMMARY_PATH}`);
  request.headers.set(DEPLOYMENT_HEADER, DO_ID);
  return request;
}

test("DO /ops/summary: proxies readiness JSON with its status, hardened", async (t) => {
  const { bot } = await summaryDoHarness(t);
  for (const status of [200, 503]) {
    const components = status === 200
      ? [["process", "ready"]]
      : [["process", "ready"], ["gateway", "down"]];
    const body = JSON.stringify({
      components,
      jobs: {},
      build_revision: "synthetic",
      build_id: "synthetic",
    });
    t.mock.method(bot, "containerFetch", async () => new Response(body, {
      status,
      headers: { "content-type": "application/json" },
    }));
    const response = await bot.fetch(summaryRequest());
    assert.equal(response.status, status, "parked readiness must stay visible");
    assert.equal(response.headers.get("content-type"), "application/json");
    assert.equal(response.headers.get("cache-control"), "no-store");
    assert.equal(response.headers.get("x-content-type-options"), "nosniff");
    assert.equal(response.headers.get("x-frame-options"), "DENY");
    assert.equal(await response.text(), body);
  }
});

test("DO /ops/summary: failures are generic 504/502 bodies", async (t) => {
  const { bot } = await summaryDoHarness(t);
  t.mock.method(bot, "containerFetch", async () => {
    throw new Error("fixture transport failure");
  });
  const thrown = await bot.fetch(summaryRequest());
  assert.equal(thrown.status, 504);
  assert.equal(await thrown.text(), "summary unavailable\n");

  t.mock.restoreAll();
  t.mock.method(bot, "containerFetch", async () => new Response("boom", { status: 500 }));
  const nonBot = await bot.fetch(summaryRequest());
  assert.equal(nonBot.status, 504);
  assert.equal(await nonBot.text(), "summary unavailable\n");

  t.mock.restoreAll();
  t.mock.method(bot, "containerFetch", async () => new Response("m 1\n".repeat(20000)));
  const huge = await bot.fetch(summaryRequest());
  assert.equal(huge.status, 502);
  assert.equal(await huge.text(), "summary unavailable\n");
});
