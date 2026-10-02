/**
 * Regression for TOG-10153: SDK 0.3.7 auto-start bypasses start() overrides.
 * Exercise the installed SDK, not a fake Container implementing our assumptions.
 * Runtime storage/TCP are in-memory; all env values are synthetic. No Discord,
 * database, deployed Worker, Docker daemon or secret bindings are contacted.
 * Run: node --import ./test/cloudflare-loader.mjs --test test/container.test.ts
 */
import { test, type TestContext } from "node:test";
import assert from "node:assert/strict";
import { DatabaseSync } from "node:sqlite";
import { setImmediate } from "node:timers/promises";
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

async function harness(t: TestContext, env: Partial<Env> = WORKER_ENV, options: {
  owner?: unknown; readError?: boolean; stopError?: boolean; running?: boolean;
} | Map<string, unknown> = {}, sqlStore?: DatabaseSync) {
  const database = sqlStore ?? new DatabaseSync(":memory:");
  if (!sqlStore) t.after(() => database.close());
  const flags = options instanceof Map ? {} : options;
  const logs: string[] = [];
  for (const method of ["log", "warn", "error"] as const) {
    t.mock.method(console, method, (...args: unknown[]) => {
      logs.push(args.map(String).join(" "));
    });
  }
  const starts: StartConfig[] = [];
  const requests: { pathname: string; port: number }[] = [];
  const values = options instanceof Map ? options
    : new Map<string, unknown>([[OWNER_KEY, "owner" in flags ? flags.owner : ACTIVE_OWNER]]);
  const gates: Promise<unknown>[] = [];
  let listenerPort = 8080;
  let probeStatus = 503;
  let probeResponse: ((status: number) => Response) | undefined;
  const runtime = {
    running: flags.running ?? false,
    destroys: 0,
    async destroy() {
      this.destroys++;
      if (flags.stopError) throw new Error("synthetic shutdown failure");
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
        const status = pathname === "/readyz" ? probeStatus : 200;
        const response = probeResponse?.(status)
          ?? Response.json({ ready: status === 200, gateway_connected: status === 200 }, { status });
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
        if (key === OWNER_KEY && flags.readError) throw new Error("synthetic storage error");
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
      // Execute the SDK's unmodified schedule SQL in real in-memory SQLite.
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
    { CF_VERSION_METADATA: { id: ID }, OWNERSHIP_CONTROL_TOKEN: CONTROL_TOKEN, ...env } as Env,
  );
  await Promise.all(gates);
  return {
    bot, runtime, starts, requests, logs, values, ctx, database,
    setProbeStatus: (status: number) => { probeStatus = status; },
    setProbeResponse: (factory: (status: number) => Response) => { probeResponse = factory; },
    pending: () => database.prepare("SELECT * FROM container_schedules WHERE callback = 'keepalive'").all(),
  };
}

async function tickKeepalive(bot: TwoBotContainer) {
  await bot.onStart();
  const [pending] = await bot.listSchedules<{ startedAt: number }>("keepalive");
  assert.ok(pending);
  await bot.keepalive(pending.payload!, pending);
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

for (const path of ["/health", "/readyz"]) {
  // The outer Worker stamps the deployment fence from its own version
  // identity, so the serving deployment id must match the fenced DO (the
  // default harness deploys ID). The container still answers with a spoofed
  // version header: only the outer serving Worker counts for provenance.
  for (const metadata of [{ id: ID }]) {
    const provenance = "serving metadata";
    for (const status of path === "/health" ? [200] : [200, 503]) {
      test(`outer Worker ${path} (${status}) replaces/removes SDK spoofed version with ${provenance}`, async (t) => {
        // The container answers with a spoofed version header; only the
        // outer serving Worker counts for provenance.
        const h = await harness(t);
        h.setProbeStatus(status);
        const body = JSON.stringify(path === "/health" ? { status: "ok" } : {
          ready: status === 200,
          components: [["process", "ready"], ["gateway", status === 200 ? "ready" : "unconfigured"]],
          jobs: {},
          build_revision: "synthetic-image-revision",
          build_id: "synthetic-image-build",
        }, null, 2) + "\n";
        const statusText = status === 503 ? "Parked" : "Healthy";
        h.setProbeResponse((status) => new Response(body, {
          status,
          statusText,
          headers: {
            "content-type": "application/json",
            "X-Two-Worker-Version": "spoofed-container-version, second-spoof",
            "x-fixture": "preserved",
          },
        }));
        const response = await worker.fetch(new Request(`https://worker.invalid${path}`), {
          CF_VERSION_METADATA: metadata,
          TWO_BOT: { getByName: () => h.bot },
        } as unknown as Env, {} as ExecutionContext);
        assert.equal(response.headers.get("x-two-worker-version"), metadata?.id ?? null);
        assert.equal(response.status, status, "parked readiness must stay 503");
        assert.equal(response.statusText, statusText);
        assert.equal(response.headers.get("content-type"), "application/json");
        assert.equal(response.headers.get("x-fixture"), "preserved");
        assert.equal(await response.text(), body, "container JSON/build provenance must stay byte-for-byte unchanged");
        assert.equal(h.starts.length, 1);
        assert.deepEqual(h.starts[0]?.env, EXPECTED_ENV, "version metadata must never enter container env");
        assert.ok(h.requests.some((r) => r.pathname === path && r.port === 8080));
      });
    }

    test(`SDK native startup failure returns sanitized outer ${path} 500 with ${provenance}`, async (t) => {
      const h = await harness(t);
      t.mock.method(h.runtime, "start", () => {
        throw new Error(`fixture startup failure ${WORKER_ENV.DISCORD_TOKEN} ${WORKER_ENV.DATABASE_URL}`);
      });
      const response = await worker.fetch(new Request(`https://worker.invalid${path}`), {
        CF_VERSION_METADATA: metadata,
        TWO_BOT: { getByName: () => h.bot },
      } as unknown as Env, {} as ExecutionContext);
      assert.equal(response.status, 500);
      assert.equal(response.headers.get("x-two-worker-version"), metadata?.id ?? null);
      assert.deepEqual(await response.json(), { ready: false, error_class: "container_unavailable" });
      assert.ok(h.logs.every((line) =>
        !line.includes(WORKER_ENV.DISCORD_TOKEN) && !line.includes(WORKER_ENV.DATABASE_URL)));
    });

    // SDK 0.3.7 containerFetch maps these start errors to text 429 (raw
    // e.message) and text 503 instead of rejecting or returning 500.
    for (const [sdkStatus, message] of [
      [429, "You are requesting too many containers per second"],
      [503, "There is no container instance that can be provided to this Durable Object"],
    ] as const) {
      test(`SDK start ${sdkStatus} returns sanitized outer ${path} 500 with ${provenance}`, async (t) => {
        const h = await harness(t);
        t.mock.method(h.runtime, "start", () => {
          throw new Error(`${message} ${WORKER_ENV.DISCORD_TOKEN} ${WORKER_ENV.DATABASE_URL}`);
        });
        const sdkResponse = await h.bot.containerFetch(new Request(`https://worker.invalid${path}`));
        assert.equal(sdkResponse.status, sdkStatus, "fixture must reach the SDK's synthetic branch");
        assert.notEqual(sdkResponse.headers.get("content-type")?.split(";")[0], "application/json");
        await sdkResponse.arrayBuffer();
        const response = await worker.fetch(new Request(`https://worker.invalid${path}`), {
          CF_VERSION_METADATA: metadata,
          TWO_BOT: { getByName: () => h.bot },
        } as unknown as Env, {} as ExecutionContext);
        assert.equal(response.status, 500);
        assert.equal(response.headers.get("x-two-worker-version"), metadata?.id ?? null);
        const body = await response.text();
        assert.deepEqual(JSON.parse(body), { ready: false, error_class: "container_unavailable" });
        for (const secret of [WORKER_ENV.DISCORD_TOKEN, WORKER_ENV.DATABASE_URL]) {
          assert.ok(!body.includes(secret));
          assert.ok(h.logs.every((line) => !line.includes(secret)));
        }
      });
    }

    for (const failure of ["lookup", "fetch"]) {
      test(`Worker DO ${failure} rejection returns sanitized ${path} 500 with ${provenance}`, async (t) => {
        const h = await harness(t);
        const reject = () => { throw new Error(`fixture DO failure ${WORKER_ENV.DATABASE_URL}`); };
        const response = await worker.fetch(new Request(`https://worker.invalid${path}`), {
          CF_VERSION_METADATA: metadata,
          TWO_BOT: { getByName: () => failure === "lookup" ? reject() : { fetch: async () => reject() } },
        } as unknown as Env, {} as ExecutionContext);
        assert.equal(response.status, 500);
        assert.equal(response.headers.get("x-two-worker-version"), metadata?.id ?? null);
        assert.deepEqual(await response.json(), { ready: false, error_class: "container_unavailable" });
        assert.ok(h.logs.every((line) => !line.includes(WORKER_ENV.DATABASE_URL)));
      });
    }
  }
}

// Only 200 and JSON 503 are the bot's own probe answers; anything else that
// reaches the container port is replaced, whatever its status or media type.
for (const [status, contentType, forwarded] of [
  [503, "application/json; charset=utf-8", true],
  [503, "text/plain", false],
  [503, null, false],
  [429, "application/json", false],
  [404, "application/json", false],
  [500, "application/json", false],
] as const) {
  test(`container /readyz ${status} ${contentType ?? "without content-type"} is ${forwarded ? "forwarded" : "sanitized"}`, async (t) => {
    const h = await harness(t);
    h.setProbeStatus(status);
    const leak = `container body ${WORKER_ENV.DATABASE_URL}`;
    h.setProbeResponse((s) => {
      if (s === 200) return Response.json({ status: "ok" });
      const response = new Response(forwarded ? JSON.stringify({ ready: false }) : leak, { status: s });
      if (contentType) response.headers.set("content-type", contentType);
      else response.headers.delete("content-type");
      return response;
    });
    // The fenced DO requires the stamped deployment header even for probes.
    const response = await h.bot.fetch(probeRequest("https://worker.invalid/readyz"));
    const body = await response.text();
    if (forwarded) {
      assert.equal(response.status, status);
      assert.deepEqual(JSON.parse(body), { ready: false });
    } else {
      assert.equal(response.status, 500);
      assert.deepEqual(JSON.parse(body), { ready: false, error_class: "container_unavailable" });
      assert.ok(!body.includes(WORKER_ENV.DATABASE_URL));
    }
    assert.ok(h.logs.every((line) => !line.includes(WORKER_ENV.DATABASE_URL)));
  });
}

test("lifecycle error replaces arbitrary exception without inspecting it", async (t) => {
  const h = await harness(t);
  const unsafe = { toString() { throw new Error(WORKER_ENV.DISCORD_TOKEN); } };
  assert.throws(() => h.bot.onError(unsafe), { message: "container_lifecycle_failed" });
  assert.ok(h.logs.includes(JSON.stringify({ event: "container_error", error_class: "container_lifecycle_failed" })));
  assert.ok(h.logs.every((line) => !line.includes(WORKER_ENV.DISCORD_TOKEN)));
});

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

// Without a version identity the outer Worker cannot stamp the deployment
// fence: the probe is refused, never blamed on the container.
for (const path of ["/health", "/readyz"]) {
  test(`outer Worker ${path} without version identity is fenced`, async (t) => {
    const h = await harness(t);
    const response = await worker.fetch(new Request(`https://worker.invalid${path}`), {
      TWO_BOT: { getByName: () => h.bot },
    } as unknown as Env, {} as ExecutionContext);
    assert.equal(response.status, 503);
    assert.equal(response.headers.get("x-two-worker-version"), null);
    const body = await response.text();
    assert.deepEqual(JSON.parse(body), { error: "ownership_fenced", reason: "deployment_id_missing" });
    assert.equal(h.starts.length, 0, "fenced probe must not start the container");
  });
}

for (const failure of ["lookup", "insert"] as const) {
  for (const warm of [false, true]) {
    for (const path of ["/health", "/readyz"]) {
      for (const status of path === "/health" ? [200] : [200, 503]) {
        test(`real SDK ${warm ? "warm" : "cold"} ${path} (${status}) still forwards on keepalive ${failure} failure`, async (t) => {
          const h = await harness(t);
          h.setProbeStatus(status);
          if (warm) {
            const response = await h.bot.fetch(probeRequest("https://worker.invalid/health"));
            await response.arrayBuffer();
            h.bot.deleteSchedules("keepalive");
          }
          const exec = h.ctx.storage.sql.exec;
          let failing = true;
          t.mock.method(h.ctx.storage.sql, "exec", (query: string, ...bindings: (string | number | null)[]) => {
            const matches = failure === "lookup"
              ? /SELECT \* FROM container_schedules WHERE callback =/.test(query)
              : /INSERT OR REPLACE INTO container_schedules/.test(query);
            if (failing && matches) {
              throw new Error(`synthetic scheduling failure ${WORKER_ENV.DISCORD_TOKEN} ${WORKER_ENV.OPS_ALERT_WEBHOOK_URL}`);
            }
            return exec(query, ...bindings);
          });
          const forwardedBefore = h.requests.filter((r) => r.pathname === path).length;
          const response = await h.bot.fetch(probeRequest(`https://worker.invalid${path}`));
          assert.equal(response.status, status, "monitoring must not replace the Container response");
          assert.deepEqual(await response.json(), { ready: status === 200, gateway_connected: status === 200 });
          assert.ok(h.requests.filter((r) => r.pathname === path).length > forwardedBefore);
          assert.equal(h.starts.length, 1, "monitoring must not cause an extra startup");
          assert.equal(h.pending().length, 0);
          assert.ok(h.logs.includes(JSON.stringify({ event: "container_keepalive_arm_failed" })));
          assert.ok(h.logs.every((line) => !line.includes(WORKER_ENV.DISCORD_TOKEN) && !line.includes(WORKER_ENV.OPS_ALERT_WEBHOOK_URL)));

          failing = false;
          const retry = await h.bot.fetch(probeRequest(`https://worker.invalid${path}`));
          assert.equal(retry.status, status);
          await retry.arrayBuffer();
          await h.bot.onStart();
          assert.equal(h.pending().length, 1, "later requests/startup retry arming without duplicating the chain");
          assert.equal(h.starts.length, 1);
        });
      }
    }
  }
}

test("cold keepalive passes env through the SDK's string-URL fetch path", async (t) => {
  const h = await harness(t);
  await tickKeepalive(h.bot);
  assert.equal(h.starts.length, 1);
  assert.deepEqual(h.starts[0]?.env, EXPECTED_ENV);
  assert.ok(h.logs.includes("two-bot /readyz unhealthy: 503"));
});

for (const port of ["9090", "1", "65535", "09090"]) {
  for (const path of ["/health", "/readyz", "keepalive"]) {
    test(`cold ${path} uses BOT_PORT=${port} for both listener and SDK target`, async (t) => {
      const h = await harness(t, { ...WORKER_ENV, BOT_PORT: port });
      if (path === "keepalive") {
        await tickKeepalive(h.bot);
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
    await h.bot.keepalive(KEEPALIVE, { taskId: "stale-task" });
    assert.equal(h.starts.length, 0);
    assert.equal(h.requests.length, 0);
    assert.equal(h.values.has("two-bot:keepalive:v1"), false);
  });
}

test("stale and baseline keepalive payloads neither start nor rearm", async (t) => {
  const h = await harness(t);
  const schedule = { taskId: "stale-task" };
  await h.bot.keepalive({ startedAt: 0 }, schedule);
  await h.bot.keepalive({ ...KEEPALIVE, epoch: 0 }, schedule);
  await h.bot.keepalive({ ...KEEPALIVE, deploymentId: "deployment-B" }, schedule);
  assert.equal(h.pending().length, 0);
  assert.equal(h.starts.length, 0);
  assert.equal(h.requests.length, 0);
  assert.equal(h.values.has("two-bot:keepalive:v1"), false);
});

test("ownership loss inside a keepalive probe cannot notify or rearm monitoring", async (t) => {
  const h = await harness(t);
  const proxy = h.bot.containerFetch.bind(h.bot);
  t.mock.method(h.bot, "containerFetch", async (...args: Parameters<TwoBotContainer["containerFetch"]>) => {
    h.values.delete(OWNER_KEY);
    return proxy(...args);
  });
  await tickKeepalive(h.bot);
  assert.equal(h.starts.length, 0);
  assert.equal(h.pending().length, 0);
  assert.equal(h.values.has("two-bot:readiness"), false);
  assert.ok(h.logs.includes("two-bot ownership refused: not_owner"));
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
  await h.bot.keepalive(KEEPALIVE, { taskId: "stale-task" });
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

// Readiness monitoring uses synthetic responses only. Global fetch is stubbed
// in every alert test: no configured webhook, Worker or database is contacted.
async function alertHarness(t: TestContext, env: Partial<Env> = {}, values?: Map<string, unknown>) {
  const h = await harness(t, { KEEPALIVE_SECONDS: "60", ...env }, values);
  let status: number | Error = 503;
  t.mock.method(h.bot, "containerFetch", async () => {
    if (status instanceof Error) throw status;
    return new Response(null, { status });
  });
  await h.bot.onStart();
  const schedules = t.mock.method(h.bot, "schedule");
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
    tick: () => tickKeepalive(h.bot),
    events: () => h.logs.filter((line) => line.startsWith("{")).map((line) => JSON.parse(line)),
  };
}

const ALERT_ENV = { UNREADY_ALERT_FAILURES: "3", OPS_ALERT_WEBHOOK_URL: WORKER_ENV.OPS_ALERT_WEBHOOK_URL };

async function fireSdkAlarm(t: TestContext, bot: TwoBotContainer, advance = 60_000) {
  const firing = bot.alarm();
  await setImmediate();
  // The SDK waits until its next task is due before finishing the alarm.
  t.mock.timers.tick(advance);
  await firing;
  await setImmediate();
}

test("real SDK keeps one persistent chain across concurrent health traffic, restart and eviction", async (t) => {
  t.mock.timers.enable({ apis: ["Date", "setTimeout"], now: 1_800_000_000_000 });
  t.mock.method(globalThis, "fetch", async () => { throw new Error("no external fetch allowed"); });
  const env = { KEEPALIVE_SECONDS: "60" };
  const first = await harness(t, env);
  const responses = await Promise.all(Array.from({ length: 10 }, () =>
    first.bot.fetch(probeRequest("https://worker.invalid/health"))));
  await Promise.all(responses.map((response) => response.arrayBuffer()));
  assert.equal(first.starts.length, 1, "cold startup also invokes onStart");
  assert.equal(first.pending().length, 1);
  const initialTask = first.pending()[0];
  await Promise.all(Array.from({ length: 5 }, () => first.bot.onStart()));
  assert.deepEqual(first.pending(), [initialTask], "restart must not postpone the pending tick");

  const h = await harness(t, env, first.values, first.database);
  await h.bot.onStart();
  assert.deepEqual(h.pending(), [initialTask], "eviction must reuse the persisted task");
  t.mock.timers.tick(60_000);
  for (let failures = 1; failures <= 10; failures++) {
    await fireSdkAlarm(t, h.bot);
    assert.equal(h.values.get("two-bot:readiness")?.failures, failures);
    assert.equal(h.pending().length, 1, "exactly one successor per firing");
    assert.equal(h.requests.filter((r) => r.pathname === "/readyz").length, failures);
    const events = h.logs.filter((line) => line.startsWith("{")).map((line) => JSON.parse(line));
    assert.equal(events.length, failures === 10 ? 1 : 0, "traffic must not accelerate the threshold");
  }
  h.setProbeStatus(200);
  await fireSdkAlarm(t, h.bot);
  await fireSdkAlarm(t, h.bot);
  assert.deepEqual(h.logs.filter((line) => line.startsWith("{")).map((line) => JSON.parse(line).event),
    ["container_unready_alert", "container_unready_recovery"]);
});

test("real SDK collapses old duplicate chains and ignores snapshotted stale callbacks", async (t) => {
  t.mock.timers.enable({ apis: ["Date", "setTimeout"], now: 1_800_000_000_000 });
  const h = await harness(t, { KEEPALIVE_SECONDS: "60" });
  for (let i = 0; i < 11; i++) await h.bot.schedule(60, "keepalive", { ...KEEPALIVE, startedAt: i });
  await h.bot.schedule(3600, "onStop", {});
  assert.equal(h.pending().length, 11);
  t.mock.timers.tick(60_000);
  await fireSdkAlarm(t, h.bot);
  assert.equal(h.values.get("two-bot:readiness")?.failures, 1);
  assert.equal(h.requests.filter((r) => r.pathname === "/readyz").length, 1);
  assert.equal(h.pending().length, 1);
  assert.equal((await h.bot.listSchedules("onStop")).length, 1, "unrelated SDK schedules are preserved");
  await fireSdkAlarm(t, h.bot);
  assert.equal(h.values.get("two-bot:readiness")?.failures, 2);
  assert.equal(h.pending().length, 1);
});

for (const warm of [false, true]) {
  for (const status of [200, 503]) {
    test(`real SDK ${warm ? "warm" : "cold"} alarm survives callback-name lookup failure (${status}) and eviction`, async (t) => {
      t.mock.timers.enable({ apis: ["Date", "setTimeout"], now: 1_800_000_000_000 });
      t.mock.method(globalThis, "fetch", async () => new Response(null, { status: 204 }));
      const env = { ...WORKER_ENV, UNREADY_ALERT_FAILURES: "1" };
      const h = await harness(t, env);
      h.setProbeStatus(status);
      if (warm) {
        const response = await h.bot.fetch(probeRequest("https://worker.invalid/health"));
        await response.arrayBuffer();
      } else {
        await h.bot.onStart();
      }
      const exec = h.ctx.storage.sql.exec;
      t.mock.method(h.ctx.storage.sql, "exec", (query: string, ...bindings: (string | number | null)[]) => {
        if (/SELECT \* FROM container_schedules WHERE callback =/.test(query)) {
          throw new Error(`synthetic lookup failure ${WORKER_ENV.OPS_ALERT_WEBHOOK_URL}`);
        }
        return exec(query, ...bindings);
      });
      t.mock.timers.tick(60_000);
      // With the old lost-chain bug, the SDK sleeps for its three-minute
      // lifecycle fallback instead of the next minute tick. Bound either path.
      await fireSdkAlarm(t, h.bot, 180_000);
      assert.equal(h.values.get("two-bot:readiness")?.lastStatus, status);
      assert.equal(h.requests.filter((r) => r.pathname === "/readyz").length, 1);
      assert.equal(h.pending().length, 1, "SDK must not delete the last monitoring task on lookup failure");
      assert.ok(h.logs.every((line) => !line.includes(WORKER_ENV.OPS_ALERT_WEBHOOK_URL)));

      // Resume using only the persisted task: no inbound request or onStart to
      // repair a lost chain, and no in-memory guard survives this eviction.
      const resumed = await harness(t, env, h.values, h.database);
      resumed.setProbeStatus(200);
      await fireSdkAlarm(t, resumed.bot);
      assert.equal(resumed.requests.filter((r) => r.pathname === "/readyz").length, 1);
      assert.equal(resumed.values.get("two-bot:readiness")?.failures, 0);
      assert.equal(resumed.pending().length, 1);
      const events = [...h.logs, ...resumed.logs].filter((line) => line.startsWith("{"))
        .map((line) => JSON.parse(line).event);
      assert.deepEqual(events, status === 503
        ? ["container_unready_alert", "container_unready_recovery"] : []);
    });
  }
}

for (const futureFirst of [true, false]) {
  test(`real SDK samples due keepalive with ${futureFirst ? "future-first" : "due-first"} legacy rows`, async (t) => {
    t.mock.timers.enable({ apis: ["Date", "setTimeout"], now: 1_800_000_000_000 });
    const h = await harness(t, { KEEPALIVE_SECONDS: "60" });
    for (const delay of futureFirst ? [1200, 60] : [60, 1200]) {
      await h.bot.schedule(delay, "keepalive", { ...KEEPALIVE, startedAt: Date.now() });
    }
    await h.bot.schedule(3600, "onStop", {});
    t.mock.timers.tick(60_000);
    await fireSdkAlarm(t, h.bot);
    assert.equal(h.values.get("two-bot:readiness")?.failures, 1, "sample at +60s, not the future legacy deadline");
    assert.equal(h.requests.filter((r) => r.pathname === "/readyz").length, 1);
    assert.equal(h.pending().length, 1, "replace both due and future legacy rows with one successor");
    assert.equal(h.pending()[0].time, Date.now() / 1000);
    assert.equal((await h.bot.listSchedules("onStop")).length, 1);
    await fireSdkAlarm(t, h.bot);
    assert.equal(h.values.get("two-bot:readiness")?.failures, 2);
    assert.equal(h.pending().length, 1);
  });
}

test("health traffic and onStart during an in-flight SDK alarm do not arm another chain", async (t) => {
  t.mock.timers.enable({ apis: ["Date", "setTimeout"], now: 1_800_000_000_000 });
  const h = await harness(t, { KEEPALIVE_SECONDS: "60" });
  await h.bot.onStart();
  const getTcpPort = h.runtime.getTcpPort;
  let release: () => void;
  const gate = new Promise<void>((resolve) => { release = resolve; });
  let probing = false;
  t.mock.method(h.runtime, "getTcpPort", (port: number) => ({
    async fetch(input: string | Request) {
      if (new URL(typeof input === "string" ? input : input.url).pathname === "/readyz") {
        probing = true;
        await gate;
      }
      return getTcpPort(port).fetch(input);
    },
  }));
  t.mock.timers.tick(60_000);
  const firing = h.bot.alarm();
  await setImmediate();
  assert.ok(probing);
  const responses = await Promise.all(Array.from({ length: 10 }, () =>
    h.bot.fetch(probeRequest("https://worker.invalid/health"))));
  await Promise.all(responses.map((response) => response.arrayBuffer()));
  await h.bot.onStart();
  assert.equal(h.pending().length, 1, "only the currently executing task exists");
  release!();
  await setImmediate();
  t.mock.timers.tick(60_000);
  await firing;
  assert.equal(h.pending().length, 1, "only one successor remains");
  assert.equal(h.values.get("two-bot:readiness")?.failures, 1);
});

test("real SDK still has one successor when readiness persistence fails", async (t) => {
  t.mock.timers.enable({ apis: ["Date", "setTimeout"], now: 1_800_000_000_000 });
  const h = await harness(t, { KEEPALIVE_SECONDS: "60", UNREADY_ALERT_FAILURES: "1" });
  const put = h.ctx.storage.put;
  t.mock.method(h.ctx.storage, "put", async (key: string, value: unknown) => {
    if (key === "two-bot:readiness") throw new Error("synthetic readiness storage failure");
    return put(key, value);
  });
  await h.bot.onStart();
  t.mock.timers.tick(60_000);
  await fireSdkAlarm(t, h.bot);
  assert.equal(h.pending().length, 1);
  assert.ok(h.logs.some((line) => line.includes("synthetic readiness storage failure")));
  assert.ok(!h.logs.some((line) => line.includes("container_unready_alert")));
});

for (const status of [200, 503]) {
  test(`real SDK drains nonempty /readyz JSON (${status}) without cancelling the proxy pipe`, async (t) => {
    const h = await harness(t, { KEEPALIVE_SECONDS: "60", UNREADY_ALERT_FAILURES: "1" });
    h.setProbeStatus(status);
    await tickKeepalive(h.bot);
    await setImmediate();
    assert.equal(h.bot["inflightRequests"], 0, "proxy pipe must settle cleanly");
    assert.equal(h.values.get("two-bot:readiness")?.lastStatus, status);
    assert.equal(h.values.get("two-bot:readiness")?.failures, status === 200 ? 0 : 1);
    assert.ok(!h.logs.some((line) => line.includes("probe failed")));
    assert.equal(h.pending().length, 1, "onStart during a cold tick must not create another chain");
  });
}

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

test("keepalive pulls /metrics, alerts once on a failing job and resolves", async (t) => {
  const h = await alertHarness(t, { UNREADY_ALERT_FAILURES: "1000", OPS_ALERT_WEBHOOK_URL: WORKER_ENV.OPS_ALERT_WEBHOOK_URL });
  let failures = 3;
  t.mock.method(h.bot, "containerFetch", async (input: string | Request) => {
    const path = new URL(typeof input === "string" ? input : input.url).pathname;
    if (path === "/metrics") return new Response(`two_bot_job_consecutive_failures{job="rank"} ${failures}\n`);
    return new Response(null, { status: 200 });
  });
  await h.tick();
  await h.tick();
  const alerts = h.posts.filter((p) => String(p.init.body).includes("ALERT job_consecutive_failures:rank"));
  assert.equal(alerts.length, 1);
  assert.match(String(alerts[0]!.init.body), /runbook\.md#alert-job-failures/);
  failures = 0;
  await h.tick();
  assert.equal(h.posts.filter((p) => String(p.init.body).includes("RESOLVED job_consecutive_failures:rank")).length, 1);
});
