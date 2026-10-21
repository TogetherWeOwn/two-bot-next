/**
 * Worker security headers (TOG-22701): every Worker-constructed response
 * carries MIME-sniffing / clickjacking hardening.
 *
 * Decided set: `x-content-type-options: nosniff`,
 * `referrer-policy: no-referrer` (matches the existing 302), and
 * `x-frame-options: DENY` (probe/metrics/control/redirect bodies are
 * machine-consumed text or JSON). Only the HTML dashboard adds a CSP;
 * ops-dashboard.test.ts checks that page-specific policy.
 */

import { test, type TestContext } from "node:test";
import assert from "node:assert/strict";
import { DatabaseSync } from "node:sqlite";
import { SECURITY_HEADERS } from "../src/security-headers.ts";
import { handleRedirect } from "../src/redirect.ts";
import worker, { TwoBotContainer, type Env } from "../src/index.ts";
import {
  CONTROL_PATH,
  DEPLOYMENT_HEADER,
  OWNER_KEY,
  OwnershipRefused,
  refused,
} from "../src/ownership.ts";
import {
  ACTIONS_PATH,
  envelope as actionsEnvelope,
  handleActionsIngress,
  relayReceiverResponse,
} from "../src/internal-actions.ts";
import {
  EVENTSUB_PATH,
  envelope as eventsubEnvelope,
  handleEventsubIngress,
  relayEventsubResponse,
} from "../src/eventsub.ts";
import { newRequestId } from "../src/request-id.ts";

const EXPECTED: Record<string, string> = {
  "x-content-type-options": "nosniff",
  "referrer-policy": "no-referrer",
  "x-frame-options": "DENY",
};

function assertHardened(headers: Headers, where: string): void {
  assert.equal(headers.get("content-security-policy"), null, `${where}: CSP is dashboard-only`);
  for (const [name, value] of Object.entries(EXPECTED)) {
    assert.equal(headers.get(name), value, `${where}: ${name} must be ${value}`);
  }
}

test("canonical helper carries the decided set", () => {
  assert.deepEqual({ ...SECURITY_HEADERS }, EXPECTED);
});

test("redirect text() responses carry the decided set", async () => {
  const deps = {
    guildId: "synthetic-guild",
    fallbackInviteCode: null,
    lookup: async () => null,
    recordClick: async () => {},
  };
  for (const [method, path] of [
    ["GET", "/never-created"],
    ["GET", "/healthz"],
    ["POST", "/reddit"],
    ["GET", "/favicon.ico"],
  ] as const) {
    const res = await handleRedirect(method, path, "caller", deps);
    for (const [name, value] of Object.entries(EXPECTED)) {
      assert.equal(res.headers[name], value, `${method} ${path}: ${name}`);
    }
  }
  const outage = await handleRedirect("GET", "/reddit", "caller", {
    ...deps,
    lookup: async () => { throw new Error("fixture outage"); },
  });
  assert.equal(outage.status, 503);
  for (const [name, value] of Object.entries(EXPECTED)) {
    assert.equal(outage.headers[name], value, `outage 503: ${name}`);
  }
});

test("redirect 302 keeps location/cache-control and gains hardening", async () => {
  const deps = {
    guildId: "synthetic-guild",
    fallbackInviteCode: "fallbackCode",
    lookup: async () => ({ slug: "reddit", inviteCode: "freshCode" }),
    recordClick: async () => {},
  };
  const res = await handleRedirect("GET", "/reddit", "caller", deps);
  assert.equal(res.status, 302);
  assert.equal(res.headers["location"], "https://discord.gg/freshCode");
  assert.match(res.headers["cache-control"] ?? "", /no-store/);
  assert.equal(res.headers["referrer-policy"], "no-referrer");
  assert.equal(res.headers["x-content-type-options"], "nosniff");
  assert.equal(res.headers["x-frame-options"], "DENY");
});

test("Worker-constructed responses carry the decided set", async () => {
  const env = {
    REDIRECT_MAPPINGS_JSON: "[]",
    REDIRECT_FALLBACK_CODE: "fallbackCode",
    CF_VERSION_METADATA: { id: "synthetic-headers-deployment" },
    TWO_BOT: {
      getByName: () => ({
        fetch: async () => new Response("ok"),
      }),
    },
  } as unknown as Env;
  const ctx = { waitUntil() {} } as unknown as ExecutionContext;
  const get = (path: string, init?: RequestInit) =>
    worker.fetch(new Request(`https://worker.invalid${path}`, init), env, ctx);

  // Probe method gate (405), probe throttle (429), metrics auth (401),
  // unconfigured metrics (404), reserved slug (404).
  assertHardened((await get("/health", { method: "POST" })).headers, "probe 405");
  const burst = (ip: string) => get("/health", { headers: { "cf-connecting-ip": ip } });
  for (let i = 0; i < 60; i++) await (await burst("192.0.2.50")).text();
  assertHardened((await burst("192.0.2.50")).headers, "probe 429");

  const token = "synthetic-headers-scrape-token-0123456789";
  const authed = { ...env, METRICS_SCRAPE_TOKEN: token } as Env;
  const metrics = (auth?: string) =>
    worker.fetch(
      new Request("https://worker.invalid/ops/metrics", {
        headers: { ...(auth ? { authorization: auth } : {}), "cf-connecting-ip": "192.0.2.51" },
      }),
      authed,
      ctx,
    );
  assertHardened((await metrics()).headers, "metrics 401");
  assertHardened(
    (await worker.fetch(new Request("https://worker.invalid/ops/metrics"), env, ctx)).headers,
    "metrics 404",
  );
  assertHardened((await get("/metrics")).headers, "reserved 404");

  // Control path without a token (401) and misconfigured redirect (503).
  assertHardened(
    (await get("/internal/ownership", { method: "GET" })).headers,
    "control 401",
  );
  const misconfigured = { ...env, REDIRECT_FALLBACK_CODE: "has space" } as Env;
  const bad = await worker.fetch(
    new Request("https://worker.invalid/reddit"),
    misconfigured,
    ctx,
  );
  assert.equal(bad.status, 503);
  assertHardened(bad.headers, "redirect 503");

  // Container-unavailable 500: a non-bot upstream becomes the fixed 500.
  const failing = {
    ...env,
    TWO_BOT: { getByName: () => ({ fetch: async () => new Response("boom", { status: 500 }) }) },
  } as unknown as Env;
  const unavailable = await worker.fetch(
    new Request("https://worker.invalid/health", { headers: { "cf-connecting-ip": "192.0.2.52" } }),
    failing,
    ctx,
  );
  assert.equal(unavailable.status, 500);
  assertHardened(unavailable.headers, "ready 500");
});

test("Worker probe edge copy stamps the decided set on bot-shaped upstream", async () => {
  // Pins wrangler/src/index.ts:1010: the /health and /readyz edge copy must
  // harden the Worker's answer even when the DO/container bytes arrive
  // without the headers. Deleting the applySecurityHeaders line leaves the
  // upstream headers bare, so these assertions fail without the stamp.
  // Fresh caller IPs: the probe budget is module-level per isolate and the
  // earlier burst test already exhausts 192.0.2.50.
  const ctx = { waitUntil() {} } as unknown as ExecutionContext;
  const botUpstream = (upstream: Response) =>
    ({
      REDIRECT_MAPPINGS_JSON: "[]",
      REDIRECT_FALLBACK_CODE: "fallbackCode",
      CF_VERSION_METADATA: { id: "synthetic-edge-copy-deployment" },
      TWO_BOT: { getByName: () => ({ fetch: async () => upstream }) },
    }) as unknown as Env;

  // Bot-shaped 200: status 200 always counts as a bot probe response.
  const ok = await worker.fetch(
    new Request("https://worker.invalid/health", {
      headers: { "cf-connecting-ip": "192.0.2.60" },
    }),
    botUpstream(new Response("bot-ok-body")),
    ctx,
  );
  assert.equal(ok.status, 200);
  assert.equal(await ok.text(), "bot-ok-body");
  assertHardened(ok.headers, "edge 200");
  assert.equal(
    ok.headers.get("x-two-worker-version"),
    "synthetic-edge-copy-deployment",
  );

  // Parked readiness: JSON 503 also counts as a bot probe response.
  const parkedUpstream = Response.json({ ready: false }, { status: 503 });
  assert.equal(parkedUpstream.headers.get("x-content-type-options"), null);
  const parked = await worker.fetch(
    new Request("https://worker.invalid/readyz", {
      headers: { "cf-connecting-ip": "192.0.2.61" },
    }),
    botUpstream(parkedUpstream),
    ctx,
  );
  assert.equal(parked.status, 503);
  assertHardened(parked.headers, "edge 503");
  assert.deepEqual(await parked.json(), { ready: false });
});

const INGRESS_CASES = [
  {
    name: "actions",
    path: ACTIONS_PATH,
    handle: handleActionsIngress,
    envelope: actionsEnvelope,
    relay: relayReceiverResponse,
    caller: "192.0.2.70",
    media: "application/json; charset=utf-8",
  },
  {
    name: "EventSub",
    path: EVENTSUB_PATH,
    handle: handleEventsubIngress,
    envelope: eventsubEnvelope,
    relay: relayEventsubResponse,
    caller: "192.0.2.71",
    media: "application/json",
  },
];

for (const ingress of INGRESS_CASES) {
  const url = `https://worker.invalid${ingress.path}`;
  const neverForward = async (): Promise<Response> => {
    assert.fail("refused ingress must not reach the container");
  };

  test(`${ingress.name} path 404 carries the decided set`, async () => {
    const response = await ingress.handle(new Request(url), ingress.caller, neverForward);
    assert.equal(response.status, 404);
    assert.equal(await response.text(), "not found");
    assertHardened(response.headers, `${ingress.name} 404`);
  });

  test(`${ingress.name} path 415 carries the decided set`, async (t) => {
    t.mock.method(console, "warn", () => {});
    const requestId = newRequestId();
    const response = await ingress.handle(new Request(url, {
      method: "POST",
      headers: { "x-request-id": requestId },
    }), ingress.caller, neverForward);
    assert.equal(response.status, 415);
    assert.equal(await response.text(), JSON.stringify({
      ok: false,
      request_id: requestId,
      error: { code: "unsupported_media_type", message: "request refused", retryable: false },
    }));
    assert.equal(response.headers.get("cache-control"), "no-store");
    assertHardened(response.headers, `${ingress.name} 415`);
  });

  test(`${ingress.name} path 429 carries the decided set and retry-after`, async (t) => {
    t.mock.method(console, "warn", () => {});
    const requestId = newRequestId();
    // A separate caller keeps this burst independent of the preceding 415.
    const burstCaller = ingress.name === "actions" ? "192.0.2.72" : "192.0.2.73";
    const post = () => ingress.handle(new Request(url, {
      method: "POST",
      headers: { "x-request-id": requestId },
    }), burstCaller, neverForward);
    for (let i = 0; i < 20; i++) {
      const admitted = await post();
      assert.equal(admitted.status, 415);
      await admitted.text();
    }
    const response = await post();
    assert.equal(response.status, 429);
    assert.equal(await response.text(), JSON.stringify({
      ok: false,
      request_id: requestId,
      error: { code: "rate_limited", message: "request refused", retryable: true },
    }));
    assert.match(response.headers.get("retry-after") ?? "", /^\d{1,6}$/);
    assert.equal(response.headers.get("cache-control"), "no-store");
    assertHardened(response.headers, `${ingress.name} 429`);
  });

  test(`${ingress.name} envelope callers cannot weaken the decided set`, async () => {
    for (const name of ["x-frame-options", "X-Frame-Options"]) {
      const requestId = newRequestId();
      const response = ingress.envelope(503, "unavailable", true, {
        [name]: "SAMEORIGIN",
        "referrer-policy": "unsafe-url",
        "x-content-type-options": "weakened",
        "retry-after": "7",
      }, requestId);
      assert.equal(response.status, 503);
      assert.equal(response.headers.get("retry-after"), "7");
      assert.equal(response.headers.get("cache-control"), "no-store");
      assertHardened(response.headers, `${ingress.name} envelope ${name}`);
      await response.text();
    }
  });

  test(`${ingress.name} relayed JSON keeps supported content and header filtering with hardening`, async (t) => {
    t.mock.method(console, "warn", () => {});
    const requestId = newRequestId();
    const ack = ingress.name === "actions"
      ? { ok: true, request_id: requestId, result: { message_id: "1" } }
      : { ok: true, request_id: requestId };
    const json = JSON.stringify({ ...ack, text: "café" });
    for (const body of [new TextEncoder().encode(json), null]) {
      for (const retry of ["7", "7.5"]) {
        await t.test(`${body === null ? "empty" : "streamed"} body, retry-after ${retry}`, async () => {
          const response = await ingress.relay(new Response(body, {
            headers: {
              "content-type": "application/json; charset=utf-8",
              "retry-after": retry,
              "idempotent-replay": "true",
              "cache-control": "public",
              "x-frame-options": "SAMEORIGIN",
              "referrer-policy": "unsafe-url",
              "x-content-type-options": "weakened",
              "set-cookie": "fixture=value",
            },
          }), requestId);
          const refusedEmpty = body === null;
          const projected = refusedEmpty
            ? { ok: false, request_id: requestId, error: { code: "unavailable", message: "request refused", retryable: true } }
            : ack;
          const expectedBody = new TextEncoder().encode(JSON.stringify(projected));
          assert.equal(response.status, refusedEmpty ? 503 : 200);
          assert.deepEqual(new Uint8Array(await response.arrayBuffer()), expectedBody);
          assert.equal(response.headers.get("content-type"), refusedEmpty ? "application/json" : ingress.media);
          assert.equal(response.headers.get("retry-after"), !refusedEmpty && retry === "7" ? "7" : null);
          assert.equal(response.headers.get("idempotent-replay"), ingress.name === "actions" && !refusedEmpty ? "true" : null);
          assert.equal(response.headers.get("cache-control"), "no-store");
          assert.equal(response.headers.get("set-cookie"), null);
          assertHardened(response.headers, `${ingress.name} relayed 200`);
        });
      }
    }
  });
}

test("EventSub relayed challenge keeps bytes and gains hardening", async () => {
  const challenge = new TextEncoder().encode("synthetic-challenge-headers");
  const response = await relayEventsubResponse(new Response(challenge, {
    headers: { "content-type": "text/plain; charset=utf-8", "retry-after": "9" },
  }));
  assert.equal(response.status, 200);
  assert.deepEqual(new Uint8Array(await response.arrayBuffer()), challenge);
  assert.equal(response.headers.get("content-type"), "text/plain");
  assert.equal(response.headers.get("retry-after"), "9");
  assert.equal(response.headers.get("cache-control"), "no-store");
  assertHardened(response.headers, "EventSub challenge 200");
});

test("EventSub invalid challenge becomes a hardened refusal without upstream headers", async (t) => {
  t.mock.method(console, "warn", () => {});
  const requestId = newRequestId();
  const response = await relayEventsubResponse(new Response("synthetic-challenge-headers\n", {
    headers: { "content-type": "text/plain", "retry-after": "9", "set-cookie": "fixture=value" },
  }), requestId);
  assert.equal(response.status, 503);
  assert.deepEqual(await response.json(), {
    ok: false,
    request_id: requestId,
    error: { code: "unavailable", message: "request refused", retryable: true },
  });
  assert.equal(response.headers.get("content-type"), "application/json");
  assert.equal(response.headers.get("retry-after"), null);
  assert.equal(response.headers.get("set-cookie"), null);
  assert.equal(response.headers.get("cache-control"), "no-store");
  assertHardened(response.headers, "EventSub invalid challenge 503");
});

for (const [reason, status] of [["epoch_conflict", 409], ["not_owner", 503]] as const) {
  test(`ownership_fenced ${status} carries the decided set`, async (t) => {
    t.mock.method(console, "warn", () => {});
    const response = refused(new OwnershipRefused(reason));
    assert.equal(response.status, status);
    assert.equal(await response.text(), JSON.stringify({ error: "ownership_fenced", reason }));
    assert.equal(response.headers.get("cache-control"), "no-store");
    assertHardened(response.headers, `ownership_fenced ${status}`);
  });
}

const DO_ID = "synthetic-headers-do-deployment";
const DO_CONTROL_TOKEN = "synthetic-headers-control-token-0123456789";
const DO_ACTIVE_OWNER = {
  deploymentId: DO_ID,
  epoch: 1,
  phase: "active",
  actor: "fixture-operator",
  timestamp: "2026-10-01T00:00:00.000Z",
  oldEpoch: 0,
  oldDeploymentId: null,
};

async function headersDoHarness(t: TestContext) {
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
    id: { toString: () => "synthetic-headers-do-id" },
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
    {
      CF_VERSION_METADATA: { id: DO_ID },
      OWNERSHIP_CONTROL_TOKEN: DO_CONTROL_TOKEN,
    } as Env,
  );
  await Promise.all(gates);
  return { bot };
}

function doRequest(path: string, init?: RequestInit): Request {
  const request = new Request(`https://worker.invalid${path}`, init);
  request.headers.set(DEPLOYMENT_HEADER, DO_ID);
  return request;
}

test("Durable Object responses carry the decided set", async (t) => {
  // Pins the ten withSecurityHeaders call sites inside TwoBotContainer:
  // the DO 404, the metrics 200/502/503/504 answers, the control
  // 401/405/400 answers and both control JSON answers. Reverting those
  // calls to plain objects leaves the suite green without these cases.
  const { bot } = await headersDoHarness(t);

  // DO 404 for unknown paths.
  const missing = await bot.fetch(doRequest("/no-such-route"));
  assert.equal(missing.status, 404);
  assertHardened(missing.headers, "do 404");
  await missing.text();

  // DO metrics answers via a mocked container fetch.
  t.mock.method(bot, "containerFetch", async () => new Response("# HELP x\n", { status: 200 }));
  const metricsOk = await bot.fetch(doRequest("/ops/metrics"));
  assert.equal(metricsOk.status, 200);
  assertHardened(metricsOk.headers, "do metrics 200");
  assert.equal(await metricsOk.text(), "# HELP x\n");

  t.mock.method(bot, "containerFetch", async () => {
    throw new Error("fixture transport failure");
  });
  const metricsThrow = await bot.fetch(doRequest("/ops/metrics"));
  assert.equal(metricsThrow.status, 504);
  assertHardened(metricsThrow.headers, "do metrics 504 throw");
  assert.equal(await metricsThrow.text(), "metrics unavailable\n");

  t.mock.method(bot, "containerFetch", async () => new Response("boom", { status: 500 }));
  const metricsNonOk = await bot.fetch(doRequest("/ops/metrics"));
  assert.equal(metricsNonOk.status, 504);
  assertHardened(metricsNonOk.headers, "do metrics 504 non-ok");
  await metricsNonOk.text();

  t.mock.method(bot, "containerFetch", async () => new Response("m 1\n".repeat(20000)));
  const metricsHuge = await bot.fetch(doRequest("/ops/metrics"));
  assert.equal(metricsHuge.status, 502);
  assertHardened(metricsHuge.headers, "do metrics 502");
  assert.equal(await metricsHuge.text(), "metrics unavailable\n");

  // DO control answers: 401/405/400 before the gate, JSON answers inside it.
  const controlNoAuth = await bot.fetch(doRequest(CONTROL_PATH));
  assert.equal(controlNoAuth.status, 401);
  assertHardened(controlNoAuth.headers, "do control 401");
  await controlNoAuth.text();

  const controlMethod = await bot.fetch(
    doRequest(CONTROL_PATH, {
      method: "PUT",
      headers: { authorization: `Bearer ${DO_CONTROL_TOKEN}` },
    }),
  );
  assert.equal(controlMethod.status, 405);
  assertHardened(controlMethod.headers, "do control 405");
  assert.equal(controlMethod.headers.get("allow"), "GET, POST");
  await controlMethod.text();

  const controlBadChange = await bot.fetch(
    doRequest(CONTROL_PATH, {
      method: "POST",
      headers: {
        authorization: `Bearer ${DO_CONTROL_TOKEN}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({ action: "not-a-change" }),
    }),
  );
  assert.equal(controlBadChange.status, 400);
  assertHardened(controlBadChange.headers, "do control 400");
  await controlBadChange.text();

  const controlGet = await bot.fetch(
    doRequest(CONTROL_PATH, {
      headers: { authorization: `Bearer ${DO_CONTROL_TOKEN}` },
    }),
  );
  assert.equal(controlGet.status, 200);
  assertHardened(controlGet.headers, "do control get json");
  assert.equal(controlGet.headers.get("cache-control"), "no-store");
  await controlGet.json();

  const controlPost = await bot.fetch(
    doRequest(CONTROL_PATH, {
      method: "POST",
      headers: {
        authorization: `Bearer ${DO_CONTROL_TOKEN}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({
        action: "takeover",
        expectedEpoch: 1,
        actor: "fixture-operator",
      }),
    }),
  );
  assert.equal(controlPost.status, 200);
  assertHardened(controlPost.headers, "do control post json");
  await controlPost.json();
});
