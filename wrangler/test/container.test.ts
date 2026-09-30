/**
 * Regression for TOG-10153: SDK 0.3.7 auto-start bypasses start() overrides.
 * Exercise the installed SDK, not a fake Container implementing our assumptions.
 * Runtime storage/TCP are in-memory; all env values are synthetic. No Discord,
 * database, deployed Worker, Docker daemon or secret bindings are contacted.
 * Run: node --import ./test/cloudflare-loader.mjs --test test/container.test.ts
 */
import { test, type TestContext } from "node:test";
import assert from "node:assert/strict";
import { TwoBotContainer, type Env } from "../src/index.ts";

const WORKER_ENV = {
  DISCORD_TOKEN: "synthetic-discord-token",
  DATABASE_URL: "synthetic-database-value",
  GUILD_ID: "111222333444555666",
  KEEPALIVE_SECONDS: "60",
  REDIRECT_FALLBACK_CODE: "not-a-container-var",
  REDIRECT_MAPPINGS_JSON: "[]",
  OPS_ALERT_WEBHOOK_URL: "https://ops.invalid/synthetic-secret",
  UNREADY_ALERT_FAILURES: "10",
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

async function harness(
  t: TestContext,
  env: Partial<Env> = WORKER_ENV,
  values = new Map<string, unknown>(),
) {
  const logs: string[] = [];
  for (const method of ["log", "warn", "error"] as const) {
    t.mock.method(console, method, (...args: unknown[]) => {
      logs.push(args.map(String).join(" "));
    });
  }
  const starts: StartConfig[] = [];
  const requests: { pathname: string; port: number }[] = [];
  const gates: Promise<unknown>[] = [];
  let listenerPort = 8080;
  const runtime = {
    running: false,
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
    container: runtime,
    storage: {
      get: async (key: string) => values.get(key),
      put: async (key: string, value: unknown) => { values.set(key, value); },
      kv: { get: (key: string) => values.get(key) },
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
    ctx as unknown as DurableObjectState,
    env as Env,
  );
  await Promise.all(gates);
  return { bot, runtime, starts, requests, logs, values, ctx };
}

for (const path of ["/health", "/readyz"]) {
  test(`cold fetch ${path} passes Worker env through SDK auto-start`, async (t) => {
    const h = await harness(t);
    const response = await h.bot.fetch(new Request(`https://worker.invalid${path}`));
    assert.equal(response.status, path === "/readyz" ? 503 : 200);
    assert.equal(h.starts.length, 1);
    assert.deepEqual(h.starts[0]?.env, EXPECTED_ENV);
    assert.equal(h.starts[0]?.enableInternet, true);
    assert.ok(h.requests.some((r) => r.pathname === path && r.port === 8080));
    assert.ok(h.logs.every((line) =>
      !line.includes(WORKER_ENV.DISCORD_TOKEN) && !line.includes(WORKER_ENV.DATABASE_URL)));

    await h.bot.fetch(new Request(`https://worker.invalid${path}`));
    assert.equal(h.starts.length, 1, "warm probe must not restart the container");
  });
}

test("cold keepalive passes env through the SDK's string-URL fetch path", async (t) => {
  const h = await harness(t);
  await h.bot.keepalive({ startedAt: 0 });
  assert.equal(h.starts.length, 1);
  assert.deepEqual(h.starts[0]?.env, EXPECTED_ENV);
  assert.ok(h.logs.includes("two-bot /readyz unhealthy: 503"));
});

for (const port of ["9090", "1", "65535", "09090"]) {
  for (const path of ["/health", "/readyz", "keepalive"]) {
    test(`cold ${path} uses BOT_PORT=${port} for both listener and SDK target`, async (t) => {
      const h = await harness(t, { ...WORKER_ENV, BOT_PORT: port });
      if (path === "keepalive") {
        await h.bot.keepalive({ startedAt: 0 });
        assert.ok(h.logs.includes("two-bot /readyz unhealthy: 503"));
        assert.ok(!h.logs.some((line) => line.includes("probe failed")));
      } else {
        const response = await h.bot.fetch(new Request(`https://worker.invalid${path}`, {
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
  await h.bot.fetch(new Request("https://worker.invalid/health"));
  assert.deepEqual(h.starts[0]?.env, {
    DISCORD_TOKEN: WORKER_ENV.DISCORD_TOKEN,
    GUILD_ID: WORKER_ENV.GUILD_ID,
    LISTEN_ADDR: "0.0.0.0:8080",
  });
});

test("health-only config never fabricates credentials or gateway readiness", async (t) => {
  const h = await harness(t, { DISCORD_TOKEN: "", DATABASE_URL: "", GUILD_ID: "" });
  const response = await h.bot.fetch(new Request("https://worker.invalid/readyz"));
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

test("non-probe route remains 404 without starting a container", async (t) => {
  const h = await harness(t);
  const response = await h.bot.fetch(new Request("https://worker.invalid/debug"));
  assert.equal(response.status, 404);
  assert.equal(h.starts.length, 0);
});

// Readiness monitoring uses synthetic responses only. Global fetch is stubbed
// in every alert test: no configured webhook, Worker or database is contacted.
async function alertHarness(t: TestContext, env: Partial<Env> = {}, values?: Map<string, unknown>) {
  const h = await harness(t, { KEEPALIVE_SECONDS: "60", ...env }, values);
  let status: number | Error = 503;
  t.mock.method(h.bot, "containerFetch", async () => {
    if (status instanceof Error) throw status;
    return new Response(null, { status });
  });
  const schedules = t.mock.method(h.bot, "schedule", async () => ({}));
  const posts: { url: string; init: RequestInit }[] = [];
  t.mock.method(globalThis, "fetch", async (url: URL, init: RequestInit) => {
    posts.push({ url: String(url), init });
    return new Response(null, { status: 204 });
  });
  return {
    ...h,
    posts,
    schedules,
    setStatus: (next: number | Error) => { status = next; },
    tick: () => h.bot.keepalive({ startedAt: 0 }),
    events: () => h.logs.filter((line) => line.startsWith("{")).map((line) => JSON.parse(line)),
  };
}

const ALERT_ENV = { UNREADY_ALERT_FAILURES: "3", OPS_ALERT_WEBHOOK_URL: WORKER_ENV.OPS_ALERT_WEBHOOK_URL };

test("default threshold is ten failed minute ticks; absent binding is log-only", async (t) => {
  const h = await alertHarness(t);
  for (let i = 0; i < 9; i++) await h.tick();
  assert.equal(h.events().length, 0);
  await h.tick();
  assert.equal(h.events().length, 1);
  assert.equal(h.events()[0].event, "container_unready_alert");
  assert.equal(h.events()[0].consecutive_failures, 10);
  assert.equal(h.events()[0].threshold, 10);
  assert.equal(h.events()[0].status, 503);
  assert.equal(typeof h.events()[0].first_failure_at, "number");
  await h.tick();
  h.setStatus(200);
  await h.tick();
  await h.tick();
  assert.deepEqual(h.events().map((e) => e.event), ["container_unready_alert", "container_unready_recovery"]);
  assert.equal(h.posts.length, 0);
  assert.equal(h.schedules.mock.callCount(), 13);
});

test("configured threshold sends one mention-free alert and recovery per incident", async (t) => {
  const h = await alertHarness(t, ALERT_ENV);
  await h.tick();
  await h.tick();
  assert.equal(h.posts.length, 0);
  await h.tick();
  await h.tick();
  assert.equal(h.posts.length, 1);
  const alert = h.posts[0]!;
  assert.equal(alert.url, ALERT_ENV.OPS_ALERT_WEBHOOK_URL);
  assert.equal(alert.init.method, "POST");
  assert.equal(alert.init.redirect, "error");
  assert.ok(alert.init.signal instanceof AbortSignal);
  const body = JSON.parse(String(alert.init.body));
  assert.deepEqual(body.allowed_mentions, { parse: [], replied_user: false });
  assert.ok(!body.content.includes("@") && !body.content.includes("<@"));
  assert.ok(body.content.includes("repeatedly failed"));
  h.setStatus(200);
  await h.tick();
  await h.tick();
  assert.equal(h.posts.length, 2);
  const recovery = JSON.parse(String(h.posts[1]!.init.body));
  assert.ok(recovery.content.includes("ready again"));
  assert.deepEqual(recovery.allowed_mentions, body.allowed_mentions);
  assert.equal(h.events()[1].consecutive_failures, 4);
  assert.equal(h.events()[1].status, 200);
  h.setStatus(503);
  for (let i = 0; i < 3; i++) await h.tick();
  assert.equal(h.posts.length, 3, "recovery arms a new incident");
  assert.ok(h.logs.every((line) => !line.includes(ALERT_ENV.OPS_ALERT_WEBHOOK_URL)));
});

test("an intermittent ready result resets the streak without a recovery message", async (t) => {
  const h = await alertHarness(t, ALERT_ENV);
  await h.tick();
  await h.tick();
  h.setStatus(204);
  await h.tick();
  assert.equal(h.events().length, 0);
  h.setStatus(503);
  await h.tick();
  await h.tick();
  assert.equal(h.posts.length, 0);
  await h.tick();
  assert.equal(h.posts.length, 1);
  assert.equal(h.events()[0].consecutive_failures, 3);
});

test("timeouts and thrown probes count as failures without logging error details", async (t) => {
  const h = await alertHarness(t, { ...ALERT_ENV, UNREADY_ALERT_FAILURES: "2" });
  h.setStatus(new Error(`timeout ${WORKER_ENV.DISCORD_TOKEN} ${ALERT_ENV.OPS_ALERT_WEBHOOK_URL}`));
  await h.tick();
  await h.tick();
  assert.equal(h.events()[0].status, null);
  assert.equal(h.events()[0].consecutive_failures, 2);
  assert.equal(h.posts.length, 1);
  assert.equal(h.schedules.mock.callCount(), 2);
  assert.ok(h.logs.every((line) => !line.includes(WORKER_ENV.DISCORD_TOKEN) && !line.includes(ALERT_ENV.OPS_ALERT_WEBHOOK_URL)));
});

test("failure streak, alert de-dup and recovery survive DO reconstruction", async (t) => {
  const first = await alertHarness(t, ALERT_ENV);
  await first.tick();
  await first.tick();
  const second = await alertHarness(t, ALERT_ENV, first.values);
  await second.tick();
  assert.equal(second.posts.length, 1);
  assert.equal(second.events()[0].consecutive_failures, 3);
  const third = await alertHarness(t, ALERT_ENV, first.values);
  await third.tick();
  assert.equal(third.posts.length, 0, "an eviction must not resend an alert");
  third.setStatus(200);
  await third.tick();
  await third.tick();
  assert.equal(third.posts.length, 1);
  assert.equal(third.events()[0].event, "container_unready_recovery");
  const fourth = await alertHarness(t, ALERT_ENV, first.values);
  fourth.setStatus(200);
  await fourth.tick();
  assert.equal(fourth.posts.length, 0, "an eviction must not resend recovery");
});

for (const failure of [429, 500, "throw"] as const) {
  test(`webhook ${failure} cannot break keepalive or trigger duplicate attempts`, async (t) => {
    const h = await alertHarness(t, { ...ALERT_ENV, UNREADY_ALERT_FAILURES: "1" });
    const webhook = t.mock.method(globalThis, "fetch", async () => {
      if (failure === "throw") throw new Error(ALERT_ENV.OPS_ALERT_WEBHOOK_URL);
      return new Response("synthetic error", { status: failure });
    });
    await h.tick();
    await h.tick();
    assert.equal(webhook.mock.callCount(), 1);
    assert.equal(h.events()[0].event, "container_unready_alert");
    assert.equal(h.events()[1].event, "container_unready_webhook_failed");
    h.setStatus(200);
    await h.tick();
    await h.tick();
    assert.equal(webhook.mock.callCount(), 2);
    assert.equal(h.schedules.mock.callCount(), 4);
    assert.ok(h.logs.every((line) => !line.includes(ALERT_ENV.OPS_ALERT_WEBHOOK_URL)));
  });
}

for (const binding of ["http://ops.invalid/secret", "not a URL", "https://user:secret@ops.invalid/"]) {
  test("invalid webhook binding is sanitized and not fetched: " + binding, async (t) => {
    const h = await alertHarness(t, { UNREADY_ALERT_FAILURES: "1", OPS_ALERT_WEBHOOK_URL: binding });
    await h.tick();
    await h.tick();
    assert.equal(h.posts.length, 0);
    assert.equal(h.events()[1].event, "container_unready_webhook_failed");
    assert.ok(h.logs.every((line) => !line.includes(binding)));
    assert.equal(h.schedules.mock.callCount(), 2);
  });
}

for (const count of [undefined, "", "0", "-1", "1.5", "NaN", "Infinity", "1e1", " 2 ", "9007199254740992"]) {
  test(`invalid or absent threshold ${JSON.stringify(count)} defaults to a ten-minute tick count`, async (t) => {
    const h = await alertHarness(t, { KEEPALIVE_SECONDS: "120", UNREADY_ALERT_FAILURES: count });
    for (let i = 0; i < 4; i++) await h.tick();
    assert.equal(h.events().length, 0);
    await h.tick();
    assert.equal(h.events()[0].threshold, 5);
  });
}

test("storage failure still rearms keepalive but does not send an unpersisted alert", async (t) => {
  const h = await alertHarness(t, { ...ALERT_ENV, UNREADY_ALERT_FAILURES: "1" });
  t.mock.method(h.ctx.storage, "put", async () => { throw new Error("synthetic storage failure"); });
  await assert.rejects(h.tick(), /synthetic storage failure/);
  assert.equal(h.schedules.mock.callCount(), 1);
  assert.equal(h.posts.length, 0);
  assert.equal(h.events().length, 0);
});
