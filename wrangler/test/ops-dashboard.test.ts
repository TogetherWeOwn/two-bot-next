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
import { createHash } from "node:crypto";
import vm from "node:vm";
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
const SCRAPE_TOKEN = "synthetic-dual-scrape-token-0123456789abcdef";

function workerEnv(secrets?: { id?: string; secret?: string; scrape?: string }) {
  const calls: Request[] = [];
  const env = {
    CF_VERSION_METADATA: { id: DEPLOYMENT },
    OPS_DASHBOARD_CLIENT_ID: secrets?.id,
    OPS_DASHBOARD_CLIENT_SECRET: secrets?.secret,
    METRICS_SCRAPE_TOKEN: secrets?.scrape,
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
    assert.equal(res.headers.get("content-security-policy"), null, "non-HTML refusals have no CSP");
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

test(`${OPS_DASHBOARD_PATH}: CSP authorizes only the unchanged page's inline blocks`, async () => {
  const { env } = configured();
  const res = await get(env, OPS_DASHBOARD_PATH);
  const csp = res.headers.get("content-security-policy");
  assert.ok(csp, "the HTML response must carry a CSP");
  assert.doesNotMatch(csp, /unsafe-inline|unsafe-eval/);
  const directives = csp.split(";").map((part) => part.trim().split(/\s+/));
  const expected: Record<string, string[]> = {
    "default-src": ["'none'"],
    "connect-src": ["'self'"],
    "base-uri": ["'none'"],
    "form-action": ["'none'"],
    "frame-ancestors": ["'none'"],
  };
  const html = await res.text();
  for (const tag of ["script", "style"]) {
    const blocks = [...html.matchAll(new RegExp(`<${tag}\\b([^>]*)>([\\s\\S]*?)</${tag}\\s*>`, "gi"))];
    assert.equal(blocks.length, 1, `page must contain one inline ${tag} block`);
    expected[`${tag}-src`] = blocks.map((block) => {
      assert.equal(block[1], "", `${tag} must remain inline without attributes`);
      return `'sha256-${createHash("sha256").update(block[2]!).digest("base64")}'`;
    });
  }
  assert.equal(directives.length, Object.keys(expected).length, "no duplicate or extra directives");
  assert.deepEqual(Object.fromEntries(directives.map(([name, ...sources]) => [name, sources])), expected);
  assert.doesNotMatch(html, /\s(?:on[\w:-]+|style)\s*=/i, "no inline event handlers or style attributes");
  // CSP hashes are header-only, so they do not rewrite the body. Re-pin when the static page changes.
  assert.equal(
    createHash("sha256").update(html).digest("hex"),
    "2651b00c52e19eba4acbd611a97f083a17f9ed836fe1b37b8d64d585179b33b7",
  );
  const headers = new Headers(res.headers);
  headers.delete("content-security-policy");
  assert.deepEqual(Object.fromEntries(headers), {
    "cache-control": "no-store",
    "content-type": "text/html; charset=utf-8",
    "referrer-policy": "no-referrer",
    "x-content-type-options": "nosniff",
    "x-frame-options": "DENY",
  }, "all existing page headers stay unchanged");
});

function extractPageScript(html: string): string {
  const match = html.match(/<script>([\s\S]*?)<\/script>/i);
  assert.ok(match, "page must contain one inline script");
  return match![1]!;
}

test(`${OPS_DASHBOARD_PATH}: served inline page script parses`, async () => {
  const { env } = configured();
  const html = await (await get(env, OPS_DASHBOARD_PATH)).text();
  const script = extractPageScript(html);
  assert.doesNotThrow(
    () => new vm.Script(script),
    "served page script must parse (a SyntaxError leaves Connect dead)",
  );
});

type StubFetch = (url: string, init?: RequestInit) => Promise<{
  status: number;
  headers: { get: (name: string) => string | null };
  json: () => Promise<unknown>;
}>;

function runPageScript(script: string, fetchImpl: StubFetch) {
  const listeners = new Map<string, () => void>();
  const elements: Record<string, {
    value?: string;
    textContent?: string;
    innerHTML?: string;
    addEventListener?: (event: string, fn: () => void) => void;
  }> = {
    summary: { innerHTML: "" },
    status: { textContent: "" },
    clientId: { value: "test-id" },
    clientSecret: { value: "test-secret" },
    connect: {},
    disconnect: {},
  };
  for (const id of ["connect", "disconnect"]) {
    elements[id]!.addEventListener = (event: string, fn: () => void) => {
      listeners.set(`${id}:${event}`, fn);
    };
  }
  const sandbox: Record<string, unknown> = {
    document: {
      getElementById: (id: string) => elements[id] ?? null,
    },
    fetch: fetchImpl,
    setInterval: () => 0,
    clearInterval: () => {},
    setTimeout: (fn: () => void, ms: number) => setTimeout(fn, ms).unref(),
    clearTimeout,
    AbortController,
    console,
  };
  vm.createContext(sandbox);
  // Expose poll/render for the drive below; the parse test above covers the
  // raw served script, so appending a hook here cannot hide a SyntaxError.
  vm.runInContext(`${script}\n;globalThis.__hook = { poll, render };`, sandbox);
  const hook = vm.runInContext("__hook", sandbox) as {
    poll: () => Promise<void>;
    render: (data: unknown, status: unknown) => string;
  };
  return { elements, listeners, hook, sandbox };
}

function readinessFetch(
  status: number,
  contentType: string,
  data?: unknown,
): StubFetch {
  return async () => ({
    status,
    headers: { get: (name: string) => name.toLowerCase() === "content-type" ? contentType : null },
    json: async () => {
      if (data === undefined) throw new Error("no json body");
      return data;
    },
  });
}

test(`${OPS_DASHBOARD_PATH}: page poll renders 200 ready, 503 report, 401 and 504 failures`, async () => {
  const { env } = configured();
  const html = await (await get(env, OPS_DASHBOARD_PATH)).text();
  const script = extractPageScript(html);

  {
    const { elements, hook } = runPageScript(
      script,
      readinessFetch(200, "application/json", {
        components: [["process", "ready"]],
        jobs: {},
        build_revision: "r",
        build_id: "b",
      }),
    );
    await hook.poll();
    assert.ok(elements["summary"]!.innerHTML!.includes(">READY<"), "200 must show READY");
    assert.ok(!elements["summary"]!.innerHTML!.includes("NOT READY"), "200 must not show NOT READY");
    assert.ok(elements["status"]!.textContent!.startsWith("updated "), "200 must stamp updated");
  }

  {
    const { elements, hook } = runPageScript(
      script,
      readinessFetch(503, "application/json", {
        components: [["process", "ready"], ["gateway", "down"]],
        jobs: {},
        gateway_failure: { phase: "connect", class: "timeout" },
        build_revision: "r",
        build_id: "b",
      }),
    );
    await hook.poll();
    assert.ok(elements["summary"]!.innerHTML!.includes("NOT READY"), "503 must show NOT READY");
    assert.ok(elements["summary"]!.innerHTML!.includes("gateway"), "503 must list the down component");
    assert.ok(elements["summary"]!.innerHTML!.includes("down"), "503 must show the down state");
  }

  for (const status of [401, 504]) {
    const { elements, hook } = runPageScript(script, readinessFetch(status, "text/plain"));
    await hook.poll();
    assert.equal(elements["status"]!.textContent, `summary ${status}`);
    assert.ok(
      elements["summary"]!.innerHTML!.includes(`summary request failed: ${status}`),
      `${status} must render the failure line`,
    );
  }
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

test("DO /ops/summary: fetches the summary roll-up and maps ready:false to 503", async (t) => {
  const { bot } = await summaryDoHarness(t);
  const urls: string[] = [];
  let upstreamStatus = 200;
  let upstreamBody = "";
  t.mock.method(bot, "containerFetch", async (input: string | Request) => {
    urls.push(typeof input === "string" ? input : input.url);
    return new Response(upstreamBody, {
      status: upstreamStatus,
      headers: { "content-type": "application/json" },
    });
  });
  // 200 + ready:false -> 503: the roll-up always answers 200, and the
  // dashboard renders readiness from the HTTP status alone.
  upstreamStatus = 200;
  upstreamBody = JSON.stringify({
    ready: false,
    components: [["process", "ready"], ["gateway", "down"]],
    jobs: {},
  });
  let res = await bot.fetch(summaryRequest());
  assert.equal(res.status, 503);
  assert.equal(res.headers.get("content-type"), "application/json");
  assert.equal(await res.text(), upstreamBody);
  // 200 + ready:true stays 200.
  upstreamBody = JSON.stringify({
    ready: true,
    components: [["process", "ready"]],
    jobs: {},
  });
  res = await bot.fetch(summaryRequest());
  assert.equal(res.status, 200);
  assert.equal(await res.text(), upstreamBody);
  // 200 with a non-JSON body proxies untouched (the roll-up never sends it).
  upstreamBody = "not-json-at-all";
  res = await bot.fetch(summaryRequest());
  assert.equal(res.status, 200);
  assert.equal(await res.text(), "not-json-at-all");
  // Every fetch hit the roll-up, never /readyz (which would silently drop
  // the gateway, pool, event-store and backfill sections).
  assert.equal(urls.length, 3);
  for (const u of urls) {
    assert.equal(new URL(u).pathname, OPS_SUMMARY_PATH);
  }
});

test(`${OPS_SUMMARY_PATH}: accepts either credential with scrape 429 as hard fence`, async () => {
  const both = workerEnv({ id: CLIENT_ID, secret: CLIENT_SECRET, scrape: SCRAPE_TOKEN });
  // Bearer only opens the summary.
  let res = await get(both.env, OPS_SUMMARY_PATH, {
    headers: { authorization: `Bearer ${SCRAPE_TOKEN}` },
  });
  assert.equal(res.status, 503);
  await res.text();
  // Service-token creds only open the summary.
  res = await get(both.env, OPS_SUMMARY_PATH, { headers: creds });
  assert.equal(res.status, 503);
  await res.text();
  // Neither credential is 401 while either secret is configured (not 404).
  res = await get(both.env, OPS_SUMMARY_PATH);
  assert.equal(res.status, 401);
  await res.text();
  // Wrong bearer + valid creds falls through to the dashboard gate.
  res = await get(both.env, OPS_SUMMARY_PATH, {
    headers: { ...creds, authorization: "Bearer wrong-bearer-not-a-secret-0123456789" },
  });
  assert.equal(res.status, 503);
  await res.text();
  assert.equal(both.calls.length, 3, "only forwards reach the container");
  // Scrape 429 is a hard fence for bearer-presenting requests: exhaust the
  // scrape budget with 10 bearer forwards on one caller (dashboard budget
  // untouched, since bearer-only requests never spend it and scrape success
  // returns before it), then both credentials on the same caller still 429.
  // A dashboard-only request presents no bearer, so it skips the spent scrape
  // gate (per-credential budgets, TOG-29408).
  const ip = freshCaller();
  const onIp = (headers: Record<string, string>) =>
    get(both.env, OPS_SUMMARY_PATH, { headers: { ...headers, "cf-connecting-ip": ip } });
  const bearer = { authorization: `Bearer ${SCRAPE_TOKEN}` };
  for (let i = 0; i < 10; i++) {
    const forwarded = await onIp(bearer);
    assert.equal(forwarded.status, 503, `bearer forward ${i + 1} must succeed`);
    await forwarded.text();
  }
  assert.equal(both.calls.length, 13);
  const fenced = await onIp({ ...bearer, ...creds });
  assert.equal(fenced.status, 429);
  await fenced.text();
  assert.equal(both.calls.length, 13, "fenced attempts never reach the container");
  // Scrape-only configured + dashboard creds is 401.
  const scrapeOnly = workerEnv({ scrape: SCRAPE_TOKEN });
  res = await get(scrapeOnly.env, OPS_SUMMARY_PATH, { headers: creds });
  assert.equal(res.status, 401);
  await res.text();
  assert.equal(scrapeOnly.calls.length, 0);
  // Dashboard-only configured + bearer is 401.
  const dashOnly = workerEnv({ id: CLIENT_ID, secret: CLIENT_SECRET });
  res = await get(dashOnly.env, OPS_SUMMARY_PATH, {
    headers: { authorization: `Bearer ${SCRAPE_TOKEN}` },
  });
  assert.equal(res.status, 401);
  await res.text();
  assert.equal(dashOnly.calls.length, 0);
});
