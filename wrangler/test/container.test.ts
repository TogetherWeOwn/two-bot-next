/**
 * Regression for TOG-10153: SDK 0.3.7 auto-start bypasses start() overrides.
 * Exercise the installed SDK, not a fake Container implementing our assumptions.
 * Runtime storage/TCP are in-memory; all env values are synthetic. No Discord,
 * database, deployed Worker, Docker daemon or secret bindings are contacted.
 * Run: node --import ./test/cloudflare-loader.mjs --test test/container.test.ts
 */
import { test, type TestContext } from "node:test";
import assert from "node:assert/strict";
import worker, { TwoBotContainer, type Env } from "../src/index.ts";
import { OWNER_KEY, AUDIT_PREFIX, DEPLOYMENT_HEADER, CONTROL_PATH } from "../src/ownership.ts";

const ID = "deployment-A";
const CONTROL_TOKEN = "synthetic-control-token-not-a-secret-12345";
const ACTIVE_OWNER = {
  deploymentId: ID, epoch: 1, phase: "active", actor: "fixture-operator",
  timestamp: "2026-10-01T00:00:00.000Z", oldEpoch: 0, oldDeploymentId: null,
};
const KEEPALIVE = { startedAt: 0, deploymentId: ID, epoch: 1 };
function probeRequest(input: string, init?: RequestInit): Request {
  const req = new Request(input, init);
  req.headers.set(DEPLOYMENT_HEADER, ID);
  return req;
}

const WORKER_ENV = {
  CF_VERSION_METADATA: { id: ID },
  OWNERSHIP_CONTROL_TOKEN: CONTROL_TOKEN,
  DISCORD_TOKEN: "synthetic-discord-token",
  DATABASE_URL: "synthetic-database-value",
  GUILD_ID: "111222333444555666",
  KEEPALIVE_SECONDS: "60",
  REDIRECT_FALLBACK_CODE: "not-a-container-var",
  REDIRECT_MAPPINGS_JSON: "[]",
};
const EXPECTED_ENV = {
  DISCORD_TOKEN: WORKER_ENV.DISCORD_TOKEN,
  DATABASE_URL: WORKER_ENV.DATABASE_URL,
  GUILD_ID: WORKER_ENV.GUILD_ID,
  LISTEN_ADDR: "0.0.0.0:8080",
};

type StartConfig = {
  env?: Record<string, string>;
  enableInternet?: boolean;
  entrypoint?: string[];
  labels?: Record<string, string>;
};

async function harness(t: TestContext, env: Partial<Env> = WORKER_ENV, options: {
  owner?: unknown; readError?: boolean; stopError?: boolean; running?: boolean;
} = {}) {
  const logs: string[] = [];
  for (const method of ["log", "warn", "error"] as const) {
    t.mock.method(console, method, (...args: unknown[]) => {
      logs.push(args.map(String).join(" "));
    });
  }
  const starts: StartConfig[] = [];
  const requests: { pathname: string; port: number }[] = [];
  const values = new Map<string, unknown>([[OWNER_KEY, "owner" in options ? options.owner : ACTIVE_OWNER]]);
  const gates: Promise<unknown>[] = [];
  let listenerPort = 8080;
  const runtime = {
    running: options.running ?? false,
    destroys: 0,
    async destroy() {
      this.destroys++;
      if (options.stopError) throw new Error("synthetic shutdown failure");
      this.running = false;
    },
    start(options: StartConfig) {
      assert.equal(this.running, false, "must not start an already-running container");
      starts.push(structuredClone(options));
      // Model LISTEN_ADDR wiring (or the image default), not the SDK target.
      // Rust's invalid-gateway-config fallback is tested by bot/tests/startup.rs.
      listenerPort = Number(options.env?.LISTEN_ADDR?.split(":").at(-1) ?? 8080);
      this.running = true;
    },
    // A running container's monitor stays pending until it stops.
    monitor: () => new Promise<void>(() => {}),
    getTcpPort: (port: number) => ({
      async fetch(input: string | Request) {
        const pathname = new URL(typeof input === "string" ? input : input.url).pathname;
        requests.push({ pathname, port });
        if (port !== listenerPort) throw new Error("connection refused: wrong container port");
        // Liveness is not readiness, even with a configured token. Preserve
        // gateway-down; a synthetic env fixture is NOT proof of a READY event.
        const response = new Response(null, {
          status: pathname === "/readyz" ? 503 : 200,
        });
        Object.defineProperty(response, "webSocket", { value: null });
        return response;
      },
    }),
  };
  const ctx = {
    // SDK reconstruction obtains a loopback callback even without host rules.
    // No outbound request is allowed by this local native-runtime fixture.
    exports: { ContainerProxy: () => ({ fetch: () => { throw new Error("unexpected outbound request"); } }) },
    id: { toString: () => "synthetic-do-id" },
    container: runtime,
    storage: {
      get: async (key: string) => {
        if (key === OWNER_KEY && options.readError) throw new Error("synthetic storage error");
        return values.get(key);
      },
      put: async (key: string | Record<string, unknown>, value?: unknown) => {
        if (typeof key === "string") values.set(key, value);
        else for (const [k, v] of Object.entries(key)) values.set(k, v);
      },
      delete: async (key: string) => values.delete(key),
      kv: {
        get: (key: string) => values.get(key),
        put: (key: string, value: unknown) => { values.set(key, value); },
        delete: (key: string) => values.delete(key),
      },
      // Scheduling persistence is outside this regression; the SDK still
      // executes its SQL, state transitions and alarm calls against this seam.
      sql: { exec: () => [] },
      setAlarm: async () => {},
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
    { CF_VERSION_METADATA: { id: ID }, OWNERSHIP_CONTROL_TOKEN: CONTROL_TOKEN, ...env } as Env,
  );
  await Promise.all(gates);
  return { bot, runtime, starts, requests, logs, values };
}

for (const path of ["/health", "/readyz"]) {
  test(`cold fetch ${path} passes Worker env through SDK auto-start`, async (t) => {
    const h = await harness(t);
    const response = await h.bot.fetch(probeRequest(`https://worker.invalid${path}`));
    assert.equal(response.status, path === "/readyz" ? 503 : 200);
    assert.equal(h.starts.length, 1);
    assert.deepEqual(h.starts[0]?.env, EXPECTED_ENV);
    assert.equal(h.starts[0]?.enableInternet, true);
    assert.ok(h.requests.some((r) => r.pathname === path && r.port === 8080));
    assert.ok(h.logs.every((line) =>
      !line.includes(WORKER_ENV.DISCORD_TOKEN) && !line.includes(WORKER_ENV.DATABASE_URL)));

    await h.bot.fetch(probeRequest(`https://worker.invalid${path}`));
    assert.equal(h.starts.length, 1, "warm probe must not restart the container");
  });
}

test("ownership failure after native startup destroys the unconfirmed process", async (t) => {
  const h = await harness(t);
  const start = h.runtime.start.bind(h.runtime);
  t.mock.method(h.runtime, "start", (config: StartConfig) => {
    start(config);
    h.values.delete(OWNER_KEY);
  });
  await assert.rejects(h.bot.start(), /not_owner/);
  assert.equal(h.starts.length, 1);
  assert.equal(h.runtime.destroys, 1);
  assert.equal(h.runtime.running, false);
  assert.equal(h.values.has("two-bot:keepalive:v1"), false);
  assert.equal((await h.bot.fetch(probeRequest("https://worker.invalid/health"))).status, 503);
  assert.equal(h.starts.length, 1, "no second start after loss of ownership");
});

test("cold keepalive passes env through the SDK's string-URL fetch path", async (t) => {
  const h = await harness(t);
  await h.bot.keepalive(KEEPALIVE);
  assert.equal(h.starts.length, 1);
  assert.deepEqual(h.starts[0]?.env, EXPECTED_ENV);
  assert.ok(h.logs.includes("two-bot /readyz unhealthy: 503"));
});

for (const port of ["9090", "1", "65535", "09090"]) {
  for (const path of ["/health", "/readyz", "keepalive"]) {
    test(`cold ${path} uses BOT_PORT=${port} for both listener and SDK target`, async (t) => {
      const h = await harness(t, { ...WORKER_ENV, BOT_PORT: port });
      if (path === "keepalive") {
        await h.bot.keepalive(KEEPALIVE);
        assert.ok(h.logs.includes("two-bot /readyz unhealthy: 503"));
        assert.ok(!h.logs.some((line) => line.includes("probe failed")));
      } else {
        const response = await h.bot.fetch(probeRequest(`https://worker.invalid${path}`, {
          signal: AbortSignal.timeout(1000),
        }));
        assert.equal(response.status, path === "/readyz" ? 503 : 200);
      }
      assert.equal(h.starts.length, 1);
      assert.deepEqual(h.starts[0]?.env, {
        ...EXPECTED_ENV,
        LISTEN_ADDR: `0.0.0.0:${Number(port)}`,
      });
      assert.equal(h.bot.defaultPort, Number(port));
      assert.ok(h.requests.length > 0);
      assert.ok(h.requests.every((request) => request.port === Number(port)));
    });
  }
}

for (const port of ["", "0", "65536", "-1", "8080.5", "NaN", "Infinity", " 9090 ", "0x2382", "1e3"]) {
  test(`invalid BOT_PORT ${JSON.stringify(port)} fails before native startup`, async (t) => {
    await assert.rejects(harness(t, { ...WORKER_ENV, BOT_PORT: port }), {
      name: "Error",
      message: "BOT_PORT must be an integer between 1 and 65535",
    });
  });
}

test("missing optionals are omitted; token and guild work without DATABASE_URL", async (t) => {
  const h = await harness(t, {
    DISCORD_TOKEN: WORKER_ENV.DISCORD_TOKEN,
    GUILD_ID: WORKER_ENV.GUILD_ID,
  });
  await h.bot.fetch(probeRequest("https://worker.invalid/health"));
  assert.deepEqual(h.starts[0]?.env, {
    DISCORD_TOKEN: WORKER_ENV.DISCORD_TOKEN,
    GUILD_ID: WORKER_ENV.GUILD_ID,
    LISTEN_ADDR: "0.0.0.0:8080",
  });
});

test("health-only config never fabricates credentials or gateway readiness", async (t) => {
  const h = await harness(t, { DISCORD_TOKEN: "", DATABASE_URL: "", GUILD_ID: "" });
  const response = await h.bot.fetch(probeRequest("https://worker.invalid/readyz"));
  assert.equal(response.status, 503);
  assert.deepEqual(h.starts[0]?.env, { LISTEN_ADDR: "0.0.0.0:8080" });
});

test("explicit start uses defaults when only non-env options are passed", async (t) => {
  const h = await harness(t);
  await h.bot.start({ enableInternet: false, entrypoint: ["synthetic-entrypoint"] });
  assert.deepEqual(h.starts[0]?.env, EXPECTED_ENV);
  assert.equal(h.starts[0]?.enableInternet, false);
  assert.deepEqual(h.starts[0]?.entrypoint, ["synthetic-entrypoint"]);
});

test("explicit startAndWaitForPorts also uses the class env defaults", async (t) => {
  const h = await harness(t);
  await h.bot.startAndWaitForPorts({ startOptions: { labels: { test: "synthetic" } } });
  assert.deepEqual(h.starts[0]?.env, EXPECTED_ENV);
  assert.deepEqual(h.starts[0]?.labels, { test: "synthetic" });
});

for (const method of ["start", "startAndWaitForPorts"] as const) {
  test(`${method} preserves SDK envVars replacement precedence`, async (t) => {
    const h = await harness(t);
    const envVars = { ONLY_EXPLICIT: "synthetic-override" };
    if (method === "start") {
      await h.bot.start({ envVars });
    } else {
      await h.bot.startAndWaitForPorts({ startOptions: { envVars } });
    }
    assert.deepEqual(h.starts[0]?.env, envVars);
    assert.deepEqual(h.bot.envVars, EXPECTED_ENV, "per-start override must not mutate defaults");
  });
}

for (const path of ["/metrics", "/metrics?token=synthetic", "/metrics/", "/metrics/extra", "/METRICS", "/%6detrics", "//metrics", "/metrics//extra"]) {
  for (const method of ["GET", "HEAD", "POST"]) {
    test(`${method} ${path} is never publicly routed or sent to the container`, async (t) => {
      const h = await harness(t);
      const request = probeRequest(`https://worker.invalid${path}`, { method });
      const env = {
        ...WORKER_ENV,
        // Even a configured invite campaign cannot make the reserved route public.
        REDIRECT_MAPPINGS_JSON: JSON.stringify([{ slug: "metrics", invite_code: "synthetic" }]),
        TWO_BOT: { getByName: () => { throw new Error("metrics must not access the DO"); } },
      } as unknown as Env;
      const ctx = { waitUntil: () => { throw new Error("metrics must not record clicks"); } } as unknown as ExecutionContext;
      const response = await worker.fetch(request, env, ctx);
      assert.equal(response.status, 404);
      assert.equal(response.headers.get("location"), null);
      assert.equal((await h.bot.fetch(request)).status, 404);
      assert.equal(h.starts.length, 0);
      assert.equal(h.requests.length, 0);
    });
  }
}

test("non-probe route remains 404 without starting a container", async (t) => {
  const h = await harness(t);
  const response = await h.bot.fetch(probeRequest("https://worker.invalid/debug"));
  assert.equal(response.status, 404);
  assert.equal(h.starts.length, 0);
});

for (const options of [
  { owner: undefined },
  { owner: { ...ACTIVE_OWNER, deploymentId: "deployment-B" } },
  { owner: { ...ACTIVE_OWNER, phase: "fenced", deploymentId: null } },
  { readError: true },
  { owner: { epoch: -1 } },
]) {
  test(`SDK all startup/proxy/keepalive paths fail closed: ${JSON.stringify(options)}`, async (t) => {
    const h = await harness(t, WORKER_ENV, options);
    for (const path of ["/health", "/readyz"]) {
      assert.equal((await h.bot.fetch(probeRequest(`https://worker.invalid${path}`))).status, 503);
    }
    await assert.rejects(h.bot.start());
    await assert.rejects(h.bot.startAndWaitForPorts());
    await assert.rejects(h.bot.containerFetch("https://container.invalid/health"));
    await assert.rejects(h.bot.onStart());
    await h.bot.keepalive(KEEPALIVE);
    assert.equal(h.starts.length, 0);
    assert.equal(h.requests.length, 0);
    assert.equal(h.values.has("two-bot:keepalive:v1"), false);
  });
}

test("stale and baseline keepalive payloads neither start nor rearm", async (t) => {
  const h = await harness(t);
  await h.bot.keepalive({ startedAt: 0 });
  await h.bot.keepalive({ ...KEEPALIVE, epoch: 0 });
  await h.bot.keepalive({ ...KEEPALIVE, deploymentId: "deployment-B" });
  assert.equal(h.starts.length, 0);
  assert.equal(h.requests.length, 0);
  assert.equal(h.values.has("two-bot:keepalive:v1"), false);
});

test("DO reconstruction destroys an inactive running process before any probe", async (t) => {
  const h = await harness(t, { ...WORKER_ENV, CF_VERSION_METADATA: { id: "deployment-B" } }, { running: true });
  assert.equal(h.runtime.destroys, 1);
  assert.equal(h.runtime.running, false);
  assert.equal((await h.bot.fetch(probeRequest("https://worker.invalid/health"))).status, 503);
  assert.equal(h.starts.length, 0);
});

function controlRequest(body?: object, token = CONTROL_TOKEN): Request {
  return probeRequest(`https://worker.invalid${CONTROL_PATH}`, {
    method: body ? "POST" : "GET",
    headers: { authorization: `Bearer ${token}` },
    ...(body ? { body: JSON.stringify(body) } : {}),
  });
}

test("authenticated takeover is CAS audited, tears down first and never auto-starts", async (t) => {
  const h = await harness(t);
  await h.bot.fetch(probeRequest("https://worker.invalid/health"));
  const result = await h.bot.fetch(controlRequest({ action: "takeover", expectedEpoch: 1, actor: "fixture-operator" }));
  assert.equal(result.status, 200);
  const state = await result.json();
  assert.equal(state.owner.epoch, 2);
  assert.equal(state.owner.oldEpoch, 1);
  assert.equal(state.owner.actor, "fixture-operator");
  assert.ok(Number.isFinite(Date.parse(state.owner.timestamp)));
  assert.equal(state.running, false);
  assert.equal(h.runtime.destroys, 1);
  assert.equal(h.starts.length, 1, "control must not start replacement");
  assert.deepEqual(h.values.get(`${AUDIT_PREFIX}2:active`), state.owner);
  assert.equal((await h.bot.fetch(controlRequest({ action: "takeover", expectedEpoch: 1, actor: "fixture-operator" }))).status, 409);
  await h.bot.keepalive(KEEPALIVE);
  assert.equal(h.starts.length, 1, "old epoch is harmless even on the same deployment");
  await h.bot.fetch(probeRequest("https://worker.invalid/health"));
  assert.equal(h.starts.length, 2);
});

test("shutdown failure retains durable revocation and refuses warm proxying", async (t) => {
  const h = await harness(t, WORKER_ENV, { stopError: true });
  await h.bot.fetch(probeRequest("https://worker.invalid/health"));
  assert.equal((await h.bot.fetch(controlRequest({ action: "fence", expectedEpoch: 1, actor: "fixture-operator" }))).status, 503);
  assert.equal((h.values.get(OWNER_KEY) as any).phase, "fenced");
  const requests = h.requests.length;
  assert.equal((await h.bot.fetch(probeRequest("https://worker.invalid/health"))).status, 503);
  assert.equal(h.requests.length, requests);
});

for (const token of ["", "invalid-token", "x".repeat(4096)]) {
  test(`unauthenticated control refuses before storage or startup (length ${token.length})`, async (t) => {
    const h = await harness(t);
    assert.equal((await h.bot.fetch(controlRequest({ action: "takeover", expectedEpoch: 1, actor: "fixture" }, token))).status, 401);
    assert.deepEqual(h.values.get(OWNER_KEY), ACTIVE_OWNER);
    assert.equal(h.starts.length, 0);
    assert.ok(h.logs.every((line) => !line.includes(CONTROL_TOKEN)));
  });
}

test("malformed, oversized and unsafe-actor control input cannot change ownership", async (t) => {
  const h = await harness(t);
  for (const body of [
    { action: "takeover", expectedEpoch: -1, actor: "fixture" },
    { action: "takeover", expectedEpoch: 1, actor: "fixture\nsecret" },
    { action: "takeover", expectedEpoch: 1, actor: "fixture", extra: "x".repeat(1024) },
  ]) assert.equal((await h.bot.fetch(controlRequest(body))).status, 400);
  assert.deepEqual(h.values.get(OWNER_KEY), ACTIVE_OWNER);
  assert.equal(h.starts.length, 0);
});

test("stale Worker ingress cannot impersonate the current deployment through headers", async (t) => {
  const h = await harness(t, { ...WORKER_ENV, CF_VERSION_METADATA: { id: "deployment-B" } }, {
    owner: { ...ACTIVE_OWNER, deploymentId: "deployment-B" },
  });
  const request = probeRequest("https://worker.invalid/health");
  request.headers.set(DEPLOYMENT_HEADER, "deployment-B");
  const env = { ...WORKER_ENV, TWO_BOT: { getByName: () => h.bot } } as unknown as Env;
  const response = await worker.fetch(request, env, {} as ExecutionContext);
  assert.equal(response.status, 503, "Worker must overwrite B with its own A identity");
  assert.equal(h.starts.length, 0);
});

test("Worker refuses unauthenticated control without ever looking up the DO", async () => {
  const env = { ...WORKER_ENV, TWO_BOT: { getByName: () => { throw new Error("must not access DO"); } } } as unknown as Env;
  const response = await worker.fetch(controlRequest(undefined, "invalid"), env, {} as ExecutionContext);
  assert.equal(response.status, 401);
});

for (const path of ["/INTERNAL/OWNERSHIP", "/internal/%6fwnership", "//internal/ownership", "/internal/ownership/extra"]) {
  test(`${path} cannot fall through to an invite redirect`, async () => {
    const env = { ...WORKER_ENV, REDIRECT_MAPPINGS_JSON: JSON.stringify([{ slug: "internal/ownership", invite_code: "synthetic" }]) } as Env;
    const response = await worker.fetch(probeRequest(`https://worker.invalid${path}`), env, {} as ExecutionContext);
    assert.equal(response.status, 404);
    assert.equal(response.headers.get("location"), null);
  });
}
