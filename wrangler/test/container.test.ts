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

const WORKER_ENV = {
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

const PUBLICATION_ENV = {
  DISCORD_APPLICATION_ID: "222333444555666777",
  TWO_COMMANDS_PUBLISH_ON_BOOT: "1",
  TWO_COMMANDS_ALLOW_LIVE_GUILD: "1",
  TWO_AUTOMATIONS: "1",
  TWO_TEXT_COMMANDS: "0",
  TWO_ANNOUNCEMENTS: "1",
  TWO_FEED_POLL_SECONDS: "300",
  TWO_MODERATION: "1",
  TWO_OWEN_USER_ID: "333444555666777888",
  TWO_MODERATION_PROTECTED_ROLE_IDS: "444555666777888999, 555666777888999000",
  TWO_COMMUNITY_SCORECARD: "1",
};
const PUBLICATION_WORKER_ENV = {
  ...WORKER_ENV,
  ...PUBLICATION_ENV,
  DISCORD_API_BASE: "https://not-forwarded.invalid",
  UNRELATED_SETTING: "not-a-container-var",
};

type StartConfig = {
  env?: Record<string, string>;
  enableInternet?: boolean;
  entrypoint?: string[];
  labels?: Record<string, string>;
};

async function harness(t: TestContext, env: Partial<Env> = WORKER_ENV) {
  const logs: string[] = [];
  for (const method of ["log", "warn", "error"] as const) {
    t.mock.method(console, method, (...args: unknown[]) => {
      logs.push(args.map(String).join(" "));
    });
  }
  const starts: StartConfig[] = [];
  const requests: { pathname: string; port: number }[] = [];
  const values = new Map<string, unknown>();
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
  return { bot, runtime, starts, requests, logs };
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

for (const path of ["/health", "/readyz", "keepalive", "start", "startAndWaitForPorts"]) {
  test(`${path} startup forwards only allowlisted publication settings`, async (t) => {
    const h = await harness(t, PUBLICATION_WORKER_ENV);
    if (path === "keepalive") {
      await h.bot.keepalive({ startedAt: 0 });
    } else if (path === "start") {
      await h.bot.start();
    } else if (path === "startAndWaitForPorts") {
      await h.bot.startAndWaitForPorts();
    } else {
      await h.bot.fetch(new Request(`https://worker.invalid${path}`));
    }
    assert.equal(h.starts.length, 1);
    assert.deepEqual(h.starts[0]?.env, { ...EXPECTED_ENV, ...PUBLICATION_ENV });
    assert.ok(h.logs.every((line) =>
      !line.includes(WORKER_ENV.DISCORD_TOKEN) && !line.includes(WORKER_ENV.DATABASE_URL)));
  });
}

test("registry feature bindings do not opt in to publication or acknowledge live writes", async (t) => {
  const { TWO_COMMANDS_PUBLISH_ON_BOOT, TWO_COMMANDS_ALLOW_LIVE_GUILD, ...features } = PUBLICATION_ENV;
  const h = await harness(t, { ...WORKER_ENV, ...features });
  await h.bot.fetch(new Request("https://worker.invalid/health"));
  assert.deepEqual(h.starts[0]?.env, { ...EXPECTED_ENV, ...features });
  assert.equal(h.starts[0]?.env?.["TWO_COMMANDS_PUBLISH_ON_BOOT"], undefined);
  assert.equal(h.starts[0]?.env?.["TWO_COMMANDS_ALLOW_LIVE_GUILD"], undefined);
});

for (const value of ["0", "true", ""]) {
  test(`publication flags preserve exact binding value ${JSON.stringify(value)}`, async (t) => {
    const h = await harness(t, {
      ...WORKER_ENV,
      TWO_COMMANDS_PUBLISH_ON_BOOT: value,
      TWO_COMMANDS_ALLOW_LIVE_GUILD: value,
    });
    await h.bot.fetch(new Request("https://worker.invalid/health"));
    assert.deepEqual(h.starts[0]?.env, {
      ...EXPECTED_ENV,
      TWO_COMMANDS_PUBLISH_ON_BOOT: value,
      TWO_COMMANDS_ALLOW_LIVE_GUILD: value,
    });
  });
}

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
    const h = await harness(t, PUBLICATION_WORKER_ENV);
    const envVars = { ONLY_EXPLICIT: "synthetic-override" };
    if (method === "start") {
      await h.bot.start({ envVars });
    } else {
      await h.bot.startAndWaitForPorts({ startOptions: { envVars } });
    }
    assert.deepEqual(h.starts[0]?.env, envVars);
    assert.deepEqual(h.bot.envVars, { ...EXPECTED_ENV, ...PUBLICATION_ENV },
      "per-start override must not mutate defaults");
  });
}

for (const path of ["/metrics", "/metrics?token=synthetic", "/metrics/", "/metrics/extra", "/METRICS", "/%6detrics", "//metrics", "/metrics//extra"]) {
  for (const method of ["GET", "HEAD", "POST"]) {
    test(`${method} ${path} is never publicly routed or sent to the container`, async (t) => {
      const h = await harness(t);
      const request = new Request(`https://worker.invalid${path}`, { method });
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
  const response = await h.bot.fetch(new Request("https://worker.invalid/debug"));
  assert.equal(response.status, 404);
  assert.equal(h.starts.length, 0);
});
