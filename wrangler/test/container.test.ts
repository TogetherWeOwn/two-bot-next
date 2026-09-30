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
  TWO_AUTOMATIONS: "1",
  TWO_TEXT_COMMANDS: "1",
  KEEPALIVE_SECONDS: "60",
  REDIRECT_FALLBACK_CODE: "not-a-container-var",
  REDIRECT_MAPPINGS_JSON: "[]",
};
const EXPECTED_ENV = {
  DISCORD_TOKEN: WORKER_ENV.DISCORD_TOKEN,
  DATABASE_URL: WORKER_ENV.DATABASE_URL,
  GUILD_ID: WORKER_ENV.GUILD_ID,
  TWO_AUTOMATIONS: WORKER_ENV.TWO_AUTOMATIONS,
  TWO_TEXT_COMMANDS: WORKER_ENV.TWO_TEXT_COMMANDS,
  LISTEN_ADDR: "0.0.0.0:8080",
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

for (const [automations, textCommands] of [
  [undefined, undefined],
  ["1", undefined],
  [undefined, "1"],
  ["0", "0"],
  ["1", "0"],
  ["0", "1"],
  ["", ""],
  ["true", "01"],
  [" 1", "1 "],
] as const) {
  test(`custom-command gates pass through unchanged (${JSON.stringify([automations, textCommands])})`, async (t) => {
    const h = await harness(t, {
      TWO_AUTOMATIONS: automations,
      TWO_TEXT_COMMANDS: textCommands,
    });
    await h.bot.fetch(new Request("https://worker.invalid/health"));
    const expected: Record<string, string> = { LISTEN_ADDR: "0.0.0.0:8080" };
    if (automations !== undefined) expected.TWO_AUTOMATIONS = automations;
    if (textCommands !== undefined) expected.TWO_TEXT_COMMANDS = textCommands;
    assert.equal(h.starts.length, 1);
    assert.deepEqual(h.starts[0]?.env, expected);
  });
}

for (const automod of [undefined, "0", "1", "", "false", " 0", "00"] as const) {
  for (const path of ["/health", "keepalive"]) {
    test(`moderation availability passes through unchanged (${JSON.stringify(automod)}, ${path})`, async (t) => {
      const h = await harness(t, { TWO_AUTOMOD: automod });
      if (path === "keepalive") {
        await h.bot.keepalive({ startedAt: 0 });
      } else {
        await h.bot.fetch(new Request(`https://worker.invalid${path}`));
      }
      const expected: Record<string, string> = { LISTEN_ADDR: "0.0.0.0:8080" };
      if (automod !== undefined) expected.TWO_AUTOMOD = automod;
      assert.equal(h.starts.length, 1);
      assert.deepEqual(h.starts[0]?.env, expected);
    });
  }
}

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
