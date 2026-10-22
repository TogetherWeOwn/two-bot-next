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
import { readFileSync } from "node:fs";
import { setImmediate } from "node:timers/promises";
import worker, { TwoBotContainer, gatewayComponentReady, type Env } from "../src/index.ts";
import { OWNER_KEY, AUDIT_PREFIX, DEPLOYMENT_HEADER, CONTROL_PATH } from "../src/ownership.ts";
import { EMPTY_STATE, LOG_QUERY_URL, type MetricsAlertState } from "../src/alert-rules.ts";
import { metricsBody, metricsLines } from "./fixtures/metrics.ts";

let scrapeSerial = 0;
/**
 * Full fixture exposition with a fresh ACK latency per serve. The harnesses
 * below serve a static exposition across many keepalive ticks; without a
 * moving latency those ticks would (correctly) trip gateway_latency_dead on
 * the third consecutive serve, since a healthy gateway ACKs every heartbeat
 * interval. Dead-gateway behavior itself is pinned in
 * gateway-transport-alert.test.ts against static expositions.
 */
function liveMetricsBody(rankFailures = 0): string {
  scrapeSerial += 1;
  return metricsBody(rankFailures).replace(
    "two_bot_gateway_latency_seconds 0",
    `two_bot_gateway_latency_seconds 0.0${10 + (scrapeSerial % 50)}`,
  );
}

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
  TWO_AUTOMATIONS: "1",
  TWO_TEXT_COMMANDS: "1",
  KEEPALIVE_SECONDS: "60",
  REDIRECT_FALLBACK_CODE: "not-a-container-var",
  REDIRECT_MAPPINGS_JSON: "[]",
  OPS_ALERT_WEBHOOK_URL: "https://discord.com/api/webhooks/123456789012345678/synthetic-token-not-a-secret",
  UNREADY_ALERT_FAILURES: "10",
};
const EXPECTED_ENV = {
  DISCORD_TOKEN: WORKER_ENV.DISCORD_TOKEN,
  DATABASE_URL: WORKER_ENV.DATABASE_URL,
  GUILD_ID: WORKER_ENV.GUILD_ID,
  TWO_AUTOMATIONS: WORKER_ENV.TWO_AUTOMATIONS,
  TWO_TEXT_COMMANDS: WORKER_ENV.TWO_TEXT_COMMANDS,
  LISTEN_ADDR: "0.0.0.0:8080",
};

const PUBLICATION_ENV = {
  DISCORD_APPLICATION_ID: "2222",
  TWO_COMMANDS_PUBLISH_ON_BOOT: "1",
  TWO_COMMANDS_ALLOW_LIVE_GUILD: "1",
  TWO_AUTOMATIONS: "1",
  TWO_TEXT_COMMANDS: "0",
  TWO_ANNOUNCEMENTS: "1",
  TWO_FEED_POLL_SECONDS: "300",
  TWO_MODERATION: "1",
  TWO_OWEN_USER_ID: "333444555666777888",
  TWO_MODERATION_PROTECTED_ROLE_IDS: "444555666777888999,555666777888999000",
  TWO_COMMUNITY_SCORECARD: "1",
};
const PUBLICATION_WORKER_ENV = {
  ...WORKER_ENV,
  ...PUBLICATION_ENV,
  DISCORD_API_BASE: "https://not-forwarded.invalid",
  UNRELATED_SETTING: "not-a-container-var",
  OPS_ALERT_FORWARDING: "on",
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
      assert.ok(h.logs.includes(JSON.stringify({
        event: "container_probe_failed",
        error_class: "container_unavailable",
        release: "unknown",
        worker_version: ID,
        log_url: LOG_QUERY_URL,
      })));
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
  assert.ok(h.logs.includes(JSON.stringify({
    event: "container_error",
    error_class: "container_lifecycle_failed",
    release: "unknown",
    worker_version: ID,
    log_url: LOG_QUERY_URL,
  })));
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
          assert.ok(h.logs.includes(JSON.stringify({
            event: "container_keepalive_arm_failed",
            release: "unknown",
            worker_version: ID,
            log_url: LOG_QUERY_URL,
          })));
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
  assert.ok(h.logs.includes(JSON.stringify({
    event: "container_readyz_unhealthy",
    status: 503,
    release: "unknown",
    worker_version: ID,
    log_url: LOG_QUERY_URL,
  })));
});

// Each probe-failure branch stamps the same attribution line: release stays
// "unknown" (no probe body was parsed this tick) and worker_version names the
// serving deployment, so Workers Logs can join the line to the deploy receipt.
const PROBE_FAILED_LINE = JSON.stringify({
  event: "container_keepalive_probe_failed",
  release: "unknown",
  worker_version: ID,
  log_url: LOG_QUERY_URL,
});

test("keepalive probe failure (transport throws) stamps release and worker version", async (t) => {
  const h = await harness(t);
  t.mock.method(h.bot, "containerFetch", async () => {
    throw new Error("synthetic probe transport failure");
  });
  await tickKeepalive(h.bot);
  assert.ok(h.logs.includes(PROBE_FAILED_LINE));
});

test("keepalive probe failure (oversized body) stamps release and worker version", async (t) => {
  const h = await harness(t);
  t.mock.method(h.bot, "containerFetch", async () => new Response("x".repeat(70 * 1024), {
    status: 200, headers: { "content-type": "application/json" },
  }));
  await tickKeepalive(h.bot);
  assert.ok(h.logs.includes(PROBE_FAILED_LINE));
});

test("keepalive probe failure (completes past deadline) stamps release and worker version", async (t) => {
  const h = await harness(t);
  const ac = new AbortController();
  const realTimeout = AbortSignal.timeout;
  t.mock.method(AbortSignal, "timeout", (ms: number) => (ms === 6000 ? ac.signal : realTimeout(ms)));
  let pulls = 0;
  t.mock.method(h.bot, "containerFetch", async () => new Response(new ReadableStream<Uint8Array>({
    pull(controller) {
      pulls += 1;
      if (pulls === 1) {
        controller.enqueue(new TextEncoder().encode(JSON.stringify({ ready: true })));
        return;
      }
      // Complete the body and trip the deadline in the same pull: the read
      // finishes, but the probe signal is aborted before the tick trusts it.
      controller.close();
      ac.abort();
    },
  }, { highWaterMark: 0 }), { status: 200, headers: { "content-type": "application/json" } }));
  await tickKeepalive(h.bot);
  assert.ok(h.logs.includes(PROBE_FAILED_LINE));
});

// TOG-13044: the bot reports why its gateway task failed on /readyz; the
// Worker passes it through and logs the fixed phase/class once per keepalive.
function readyzWith(gatewayFailure: unknown, extra: Record<string, unknown> = {}): string {
  return JSON.stringify({
    components: [["process", "ready"], ["gateway", "starting"]],
    ...(gatewayFailure === undefined ? {} : { gateway_failure: gatewayFailure }),
    ...extra,
  });
}

async function keepaliveSeeing(t: TestContext, body: string) {
  const h = await harness(t);
  h.setProbeResponse((status) => new Response(body, {
    status, headers: { "content-type": "application/json" },
  }));
  await tickKeepalive(h.bot);
  return h;
}

const gatewayFailureLogs = (logs: string[]) =>
  logs.filter((line) => line.includes("container_gateway_failure"));
const containerErrorLogs = (logs: string[]) =>
  logs.filter((line) => line.includes('"container_error"'));

test("keepalive logs the gateway failure phase and class once, as strict tokens", async (t) => {
  const failure = { phase: "durable_gateway", class: "checkpoint_load_failed" };
  const h = await keepaliveSeeing(t, readyzWith(failure));
  assert.deepEqual(gatewayFailureLogs(h.logs), [JSON.stringify({
    event: "container_gateway_failure",
    phase: "durable_gateway",
    class: "checkpoint_load_failed",
    failed_step: "unknown",
    error_code: "none",
    build_revision: "unknown",
    deploy_id: "unknown",
    release: "unknown",
    worker_version: ID,
    log_url: "https://developers.cloudflare.com/workers/observability/logs/workers-logs/",
  })]);
});

test("keepalive logs build revision plus failed step and SQLSTATE when served", async (t) => {
  const h = await keepaliveSeeing(t, readyzWith(
    {
      phase: "durable_gateway",
      class: "gateway_runtime_failed",
      failed_step: "gateway_run",
      error_code: "42501",
    },
    { build_revision: "abc1234", build_id: "build-1", deploy_id: "2026.10.21.1+abc1234" },
  ));
  assert.deepEqual(gatewayFailureLogs(h.logs), [JSON.stringify({
    event: "container_gateway_failure",
    phase: "durable_gateway",
    class: "gateway_runtime_failed",
    failed_step: "gateway_run",
    error_code: "42501",
    build_revision: "abc1234",
    deploy_id: "2026.10.21.1+abc1234",
    release: "abc1234",
    worker_version: ID,
    log_url: "https://developers.cloudflare.com/workers/observability/logs/workers-logs/",
  })]);
});

test("public /readyz forwards the gateway failure unchanged", async (t) => {
  const body = readyzWith({ phase: "durable_gateway", class: "automod_config_invalid" });
  const h = await harness(t);
  h.setProbeResponse((status) => new Response(body, {
    status, headers: { "content-type": "application/json" },
  }));
  const response = await worker.fetch(new Request("https://worker.invalid/readyz"), {
    CF_VERSION_METADATA: { id: ID },
    TWO_BOT: { getByName: () => h.bot },
  } as unknown as Env, {} as ExecutionContext);
  assert.equal(response.status, 503);
  assert.equal(await response.text(), body);
  assert.deepEqual(gatewayFailureLogs(h.logs), [], "only the keepalive path logs the class");
});

test("keepalive without a gateway failure logs no failure event", async (t) => {
  const h = await keepaliveSeeing(t, readyzWith(undefined));
  assert.deepEqual(gatewayFailureLogs(h.logs), []);
});

const HOSTILE_FAILURES: [string, unknown][] = [
  ["url as class", { phase: "durable_gateway", class: "postgres://user:hunter2@db.internal/app" }],
  ["uppercase class", { phase: "durable_gateway", class: "Checkpoint_Load_Failed" }],
  ["over-long class", { phase: "durable_gateway", class: "a".repeat(33) }],
  ["empty phase", { phase: "", class: "checkpoint_load_failed" }],
  ["spaced phase", { phase: "durable gateway", class: "checkpoint_load_failed" }],
  ["numeric class", { phase: "durable_gateway", class: 7 }],
  ["missing class", { phase: "durable_gateway" }],
  ["string instead of object", "durable_gateway:checkpoint_load_failed"],
  ["null", null],
  ["array", ["durable_gateway", "checkpoint_load_failed"]],
];
for (const [name, hostile] of HOSTILE_FAILURES) {
  test(`keepalive drops a hostile gateway failure (${name})`, async (t) => {
    const h = await keepaliveSeeing(t, readyzWith(hostile));
    assert.deepEqual(gatewayFailureLogs(h.logs), []);
    assert.ok(h.logs.every((line) => !line.includes("hunter2") && !line.includes("db.internal")));
  });
}

test("keepalive logs only phase and class even when the failure object carries more", async (t) => {
  const h = await keepaliveSeeing(t, readyzWith({
    phase: "durable_gateway", class: "gateway_runtime_failed", error: "postgres://user:hunter2@db.internal/app",
  }));
  const [line] = gatewayFailureLogs(h.logs);
  assert.equal(line, JSON.stringify({
    event: "container_gateway_failure",
    phase: "durable_gateway",
    class: "gateway_runtime_failed",
    failed_step: "unknown",
    error_code: "none",
    build_revision: "unknown",
    deploy_id: "unknown",
    release: "unknown",
    worker_version: ID,
    log_url: "https://developers.cloudflare.com/workers/observability/logs/workers-logs/",
  }));
  assert.ok(h.logs.every((entry) => !entry.includes("hunter2")));
});

// Release-tagged error tracking groups gateway failures by `class`: the keepalive
// re-emits the container failure with the /readyz git SHA as `release`
// (Workers Logs filters by release and groups by class).
const RELEASE_SHA = "9f3a1c7e2b4d6a8c0e1f3a5b7c9d2e4f6a8b0c1d";

test("keepalive tags the gateway failure with the container release and worker version", async (t) => {
  const h = await keepaliveSeeing(t, readyzWith(
    { phase: "durable_gateway", class: "checkpoint_load_failed" },
    { build_revision: RELEASE_SHA, build_id: "123-1" },
  ));
  assert.deepEqual(gatewayFailureLogs(h.logs), [JSON.stringify({
    event: "container_gateway_failure",
    phase: "durable_gateway",
    class: "checkpoint_load_failed",
    failed_step: "unknown",
    error_code: "none",
    build_revision: RELEASE_SHA,
    deploy_id: "unknown",
    release: RELEASE_SHA,
    worker_version: ID,
    log_url: "https://developers.cloudflare.com/workers/observability/logs/workers-logs/",
  })]);
});

test("container_error stamps the parsed release once a probe has seen it", async (t) => {
  const h = await keepaliveSeeing(t, readyzWith(
    { phase: "durable_gateway", class: "checkpoint_load_failed" },
    { build_revision: RELEASE_SHA, build_id: "123-1" },
  ));
  assert.throws(() => h.bot.onError(new Error("synthetic lifecycle failure")),
    { message: "container_lifecycle_failed" });
  assert.deepEqual(containerErrorLogs(h.logs), [JSON.stringify({
    event: "container_error",
    error_class: "container_lifecycle_failed",
    release: RELEASE_SHA,
    worker_version: ID,
    log_url: LOG_QUERY_URL,
  })]);
});

test("container_error after a container stop stamps unknown, not the previous release", async (t) => {
  const h = await keepaliveSeeing(t, readyzWith(
    { phase: "durable_gateway", class: "checkpoint_load_failed" },
    { build_revision: RELEASE_SHA, build_id: "123-1" },
  ));
  // The old image stops; the replacement is unprobed when its startup fails.
  h.bot.onStop();
  assert.throws(() => h.bot.onError(new Error("synthetic restart failure")),
    { message: "container_lifecycle_failed" });
  assert.deepEqual(containerErrorLogs(h.logs), [JSON.stringify({
    event: "container_error",
    error_class: "container_lifecycle_failed",
    release: "unknown",
    worker_version: ID,
    log_url: LOG_QUERY_URL,
  })]);
});

test("container_error during a probe fetch stamps unknown, not the previous release", async (t) => {
  const h = await keepaliveSeeing(t, readyzWith(
    { phase: "durable_gateway", class: "checkpoint_load_failed" },
    { build_revision: RELEASE_SHA, build_id: "123-1" },
  ));
  t.mock.method(h.bot, "containerFetch", async (input: string | Request) => {
    const path = new URL(typeof input === "string" ? input : input.url).pathname;
    if (path === "/metrics") return new Response(liveMetricsBody(0));
    // The SDK awaits onError from startup/port checks inside containerFetch
    // (SDK 0.3.7 waitForPort), before the fetch settles; handler errors are
    // ignored and the original failure propagates.
    try { h.bot.onError(new Error("synthetic startup failure")); }
    catch { /* SDK ignores onError handler errors */ }
    throw new Error("synthetic probe fetch failure");
  });
  await tickKeepalive(h.bot);
  assert.deepEqual(containerErrorLogs(h.logs), [JSON.stringify({
    event: "container_error",
    error_class: "container_lifecycle_failed",
    release: "unknown",
    worker_version: ID,
    log_url: LOG_QUERY_URL,
  })]);
});

test("keepalive tags an unparseable build revision as unknown without echoing it", async (t) => {
  const h = await keepaliveSeeing(t, readyzWith(
    { phase: "durable_gateway", class: "checkpoint_load_failed" },
    { build_revision: "postgres://user:hunter2@db.internal/app" },
  ));
  const [line] = gatewayFailureLogs(h.logs);
  assert.equal(JSON.parse(line!).release, "unknown");
  assert.equal(JSON.parse(line!).worker_version, ID);
  assert.ok(h.logs.every((entry) => !entry.includes("hunter2")));
});

test("keepalive drops a hostile deploy_id to unknown, never logging it", async (t) => {
  // Shape-only: the deploy gates own canonical validation (leading-zero N
  // parses here and refuses there), this only gates log echo.
  for (const deployId of [
    "2026.10.21.1+ABC1234",
    "2026-10-21.1+abc1234",
    "2026.10.21.1+abc1234\ninjected",
    7,
    null,
  ]) {
    const h = await keepaliveSeeing(t, readyzWith(
      { phase: "durable_gateway", class: "gateway_runtime_failed" },
      { deploy_id: deployId },
    ));
    const [line] = gatewayFailureLogs(h.logs);
    assert.ok(line.includes('"deploy_id":"unknown"'), `deploy_id leaked: ${line}`);
  }
});

test("metrics alert lines carry the keepalive release and worker version", async (t) => {
  const h = await harness(t);
  const rankClass = `two_bot_job_last_error_class{job="rank",class="database"} 1`;
  const failing = () => liveMetricsBody(3).replace(
    'two_bot_job_last_error_class{job="rank",class="database"} 0', rankClass);
  t.mock.method(h.bot, "containerFetch", async (input: string | Request) => {
    const path = new URL(typeof input === "string" ? input : input.url).pathname;
    if (path === "/metrics") return new Response(failing());
    return new Response(JSON.stringify({
      components: [["process", "ready"]],
      build_revision: RELEASE_SHA, build_id: "123-1",
    }), { status: 200, headers: { "content-type": "application/json" } });
  });
  await tickKeepalive(h.bot);
  const alerts = h.logs.filter((line) => line.startsWith("{")).map((line) => JSON.parse(line))
    .filter((event) => event.event === "metrics_alert");
  assert.ok(alerts.length > 0, "expected a metrics alert line");
  for (const alert of alerts) {
    assert.equal(alert.release, RELEASE_SHA);
    assert.equal(alert.worker_version, ID);
  }
  assert.match(alerts[0].content, /last error class: database/);
});

test("gateway component parsing reads only the ready/starting/down vocabulary", () => {
  assert.equal(gatewayComponentReady([["process", "ready"], ["gateway", "ready"]]), true);
  assert.equal(gatewayComponentReady([["process", "ready"], ["gateway", "starting"]]), false);
  assert.equal(gatewayComponentReady([["process", "ready"], ["gateway", "down"]]), false);
  assert.equal(gatewayComponentReady([["process", "ready"], ["gateway", "down"], ["token_invalid", "down"]]), false);
  // Anything else holds the stuck-gateway rule instead of guessing.
  assert.equal(gatewayComponentReady([["process", "ready"]]), undefined);
  assert.equal(gatewayComponentReady([["process", "ready"], ["gateway", "restarting"]]), undefined);
  assert.equal(gatewayComponentReady([["process", "ready"], ["gateway", 7]]), undefined);
  assert.equal(gatewayComponentReady([["process", "ready"], "gateway"]), undefined);
  assert.equal(gatewayComponentReady({ gateway: "down" }), undefined);
  assert.equal(gatewayComponentReady(undefined), undefined);
});

test("keepalive pages gateway_not_ready on the third tick with the gateway component down", async (t) => {
  const h = await harness(t);
  let gatewayStatus = "down";
  t.mock.method(h.bot, "containerFetch", async (input: string | Request) => {
    const path = new URL(typeof input === "string" ? input : input.url).pathname;
    if (path === "/metrics") return new Response(liveMetricsBody(0));
    return new Response(JSON.stringify({
      components: [["process", "ready"], ["gateway", gatewayStatus]],
      build_revision: RELEASE_SHA, build_id: "123-1",
    }), { status: gatewayStatus === "ready" ? 200 : 503, headers: { "content-type": "application/json" } });
  });
  const metricsAlerts = () => h.logs.filter((line) => line.startsWith("{")).map((line) => JSON.parse(line))
    .filter((event) => event.event === "metrics_alert").map((event) => event.content as string);
  await tickKeepalive(h.bot);
  await tickKeepalive(h.bot);
  assert.ok(metricsAlerts().every((content) => !content.includes("gateway_not_ready")), "no page before the third sample");
  await tickKeepalive(h.bot);
  assert.ok(metricsAlerts().some((content) => content.includes("two-bot-next ALERT gateway_not_ready")),
    `expected a gateway_not_ready page, got: ${JSON.stringify(metricsAlerts())}`);
  // Recovery clears on the first ready sample.
  gatewayStatus = "ready";
  await tickKeepalive(h.bot);
  assert.ok(metricsAlerts().some((content) => content.includes("two-bot-next RESOLVED gateway_not_ready")),
    `expected a gateway_not_ready resolution, got: ${JSON.stringify(metricsAlerts())}`);
});

test("keepalive ignores a gateway failure in a non-bot (text) probe answer", async (t) => {
  const h = await harness(t);
  h.setProbeResponse((status) => new Response(
    readyzWith({ phase: "durable_gateway", class: "checkpoint_load_failed" }),
    { status, headers: { "content-type": "text/plain" } },
  ));
  await tickKeepalive(h.bot);
  assert.deepEqual(gatewayFailureLogs(h.logs), []);
});

test("keepalive treats oversize /readyz as a failed probe without logging the body", async (t) => {
  const h = await harness(t);
  const oversizeMarker = `synthetic-readyz-oversize-${"m".repeat(64)}`;
  const oversizeBody = `${JSON.stringify({ ready: true, marker: oversizeMarker })}\n${"m 1\n".repeat(20000)}`;
  assert.ok(new TextEncoder().encode(oversizeBody).byteLength > 64 * 1024);
  // A 200 with a huge body would be ready if the cap were removed (buffered
  // parse), so this tick only passes when the bounded reader rejects it.
  // Mock containerFetch to bypass the SDK proxy pipe: cancelling the pipe's
  // destination readable rejects its internal pipeTo (unhandled in the test
  // runtime), while the mocked Response exercises the same bounded reader.
  t.mock.method(h.bot, "containerFetch", async (input: string | Request) => {
    const path = new URL(typeof input === "string" ? input : input.url).pathname;
    if (path === "/metrics") return new Response(null, { status: 503 });
    return new Response(oversizeBody, {
      status: 200, headers: { "content-type": "application/json" },
    });
  });
  await tickKeepalive(h.bot);
  assert.equal(h.values.get("two-bot:readiness")?.lastStatus, null);
  assert.equal(h.values.get("two-bot:readiness")?.failures, 1);
  assert.deepEqual(gatewayFailureLogs(h.logs), []);
  assert.ok(h.logs.every((line) => !line.includes(oversizeMarker)), "no raw probe body in logs");
  assert.ok(h.logs.every((line) => !line.includes("m 1\nm 1")), "no probe bytes in logs");
});

test("public probe discards oversize non-bot body without buffering it", async (t) => {
  const h = await harness(t);
  const marker = `synthetic-probe-discard-${"d".repeat(64)}`;
  const encoder = new TextEncoder();
  const firstChunk = encoder.encode(marker + "x".repeat(8192));
  const chunk = encoder.encode("x".repeat(8192));
  const totalChunks = 32; // ~256 KiB streamed, well over the 64 KiB cap
  let pulls = 0;
  let cancelled = 0;
  // Mock containerFetch to bypass the SDK proxy pipe: cancelling the pipe's
  // destination readable rejects its internal pipeTo (unhandled in the test
  // runtime), while the mocked Response exercises the same bounded reader.
  t.mock.method(h.bot, "containerFetch", async () => new Response(new ReadableStream({
    pull(controller) {
      pulls++;
      if (pulls <= totalChunks) controller.enqueue(pulls === 1 ? firstChunk : chunk);
      else controller.close();
    },
    cancel() { cancelled++; },
  }, { highWaterMark: 0 }), {
    status: 500, headers: { "content-type": "text/plain" },
  }));
  const response = await h.bot.fetch(probeRequest("https://worker.invalid/readyz"));
  assert.equal(response.status, 500);
  assert.deepEqual(await response.json(), { ready: false, error_class: "container_unavailable" });
  // Bounded: the reader cancels after ~64 KiB instead of draining ~256 KiB.
  // The old unbounded arrayBuffer() drain pulls every chunk and never cancels.
  assert.equal(cancelled, 1, "oversize non-bot probe body must be cancelled");
  assert.ok(pulls < totalChunks, `expected early cancel, pulled ${pulls}/${totalChunks} chunks`);
  assert.ok(pulls * 8192 <= 96 * 1024, `expected ~64 KiB discard, pulled ~${pulls * 8} KiB`);
  assert.ok(h.logs.every((line) => !line.includes(marker)), "no raw probe body in logs");
});

for (const path of ["/health", "/readyz", "keepalive", "start", "startAndWaitForPorts"]) {
  test(`${path} startup forwards only allowlisted publication settings`, async (t) => {
    const h = await harness(t, PUBLICATION_WORKER_ENV);
    if (path === "keepalive") {
      await tickKeepalive(h.bot);
    } else if (path === "start") {
      await h.bot.start();
    } else if (path === "startAndWaitForPorts") {
      await h.bot.startAndWaitForPorts();
    } else {
      await h.bot.fetch(probeRequest(`https://worker.invalid${path}`));
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
  await h.bot.fetch(probeRequest("https://worker.invalid/health"));
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
    await h.bot.fetch(probeRequest("https://worker.invalid/health"));
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
        await tickKeepalive(h.bot);
        assert.ok(h.logs.includes(JSON.stringify({
    event: "container_readyz_unhealthy",
    status: 503,
    release: "unknown",
    worker_version: ID,
    log_url: LOG_QUERY_URL,
  })));
        assert.ok(!h.logs.some((line) => line.includes("probe_failed")));
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
      ...WORKER_ENV,
      TWO_AUTOMATIONS: automations,
      TWO_TEXT_COMMANDS: textCommands,
    });
    await h.bot.fetch(probeRequest("https://worker.invalid/health"));
    const expected: Record<string, string> = { ...EXPECTED_ENV };
    delete expected.TWO_AUTOMATIONS;
    delete expected.TWO_TEXT_COMMANDS;
    if (automations !== undefined) expected.TWO_AUTOMATIONS = automations;
    if (textCommands !== undefined) expected.TWO_TEXT_COMMANDS = textCommands;
    assert.equal(h.starts.length, 1);
    assert.deepEqual(h.starts[0]?.env, expected);
  });
}

for (const automod of [undefined, "0", "1", "", "false", " 0", "00"] as const) {
  for (const path of ["/health", "keepalive"]) {
    test(`moderation availability passes through unchanged (${JSON.stringify(automod)}, ${path})`, async (t) => {
      const h = await harness(t, { ...WORKER_ENV, TWO_AUTOMOD: automod });
      if (path === "keepalive") {
        await tickKeepalive(h.bot);
      } else {
        await h.bot.fetch(probeRequest(`https://worker.invalid${path}`));
      }
      const expected: Record<string, string> = { ...EXPECTED_ENV };
      if (automod !== undefined) expected.TWO_AUTOMOD = automod;
      assert.equal(h.starts.length, 1);
      assert.deepEqual(h.starts[0]?.env, expected);
    });
  }
}

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
  // A same-owner exact-epoch repeat is a no-op, so the commit path is
  // exercised by a fresh deployment at the fresh epoch.
  const h = await harness(t, { ...WORKER_ENV, CF_VERSION_METADATA: { id: "deployment-B" } }, { running: true });
  // Reconstruction already tore down the previous owner's live process
  // (destroys is 1); the takeover below must not start a replacement.
  const asB = (body?: object) => {
    const request = controlRequest(body);
    request.headers.set(DEPLOYMENT_HEADER, "deployment-B");
    return request;
  };
  const result = await h.bot.fetch(asB({ action: "takeover", expectedEpoch: 1, actor: "fixture-operator" }));
  assert.equal(result.status, 200);
  const state = await result.json();
  assert.equal(state.owner.epoch, 2);
  assert.equal(state.owner.oldEpoch, 1);
  assert.equal(state.owner.actor, "fixture-operator");
  assert.ok(Number.isFinite(Date.parse(state.owner.timestamp)));
  assert.equal(state.running, false);
  assert.equal(h.runtime.destroys, 1);
  assert.equal(h.starts.length, 0, "control must not start replacement");
  assert.deepEqual(h.values.get(`${AUDIT_PREFIX}2:active`), state.owner);
  assert.equal((await h.bot.fetch(asB({ action: "takeover", expectedEpoch: 1, actor: "fixture-operator" }))).status, 409);
  await h.bot.keepalive(KEEPALIVE, { taskId: "stale-task" });
  assert.equal(h.starts.length, 0, "old epoch is harmless even on the same deployment");
  const probe = probeRequest("https://worker.invalid/health");
  probe.headers.set(DEPLOYMENT_HEADER, "deployment-B");
  await h.bot.fetch(probe);
  assert.equal(h.starts.length, 1);
});

test("same-owner exact-epoch takeover repeat is a read-only no-op", async (t) => {
  const h = await harness(t);
  await h.bot.fetch(probeRequest("https://worker.invalid/health"));
  const destroys = h.runtime.destroys;
  const logMark = h.logs.length;
  // The harness presets deployment-A active at epoch 1, so this is a repeat.
  const repeat = await h.bot.fetch(controlRequest({ action: "takeover", expectedEpoch: 1, actor: "fixture-operator" }));
  assert.equal(repeat.status, 200);
  const state = await repeat.json();
  assert.equal(state.owner.epoch, 1);
  assert.equal(state.owner.deploymentId, "deployment-A");
  assert.equal(state.owner.phase, "active");
  assert.equal(h.runtime.destroys, destroys, "no-op must not tear down");
  assert.ok([...h.values.keys()].every((key) => !String(key).startsWith(AUDIT_PREFIX)), "no-op writes no audit row");
  const fresh = h.logs.slice(logMark).join("\n");
  assert.ok(fresh.includes("two-bot ownership takeover already applied at epoch 1"));
  assert.ok(!fresh.includes("two-bot ownership change:"), "no-op must not log a change that never happened");
  assert.equal((await h.bot.fetch(probeRequest("https://worker.invalid/health"))).status, 200);
});

test("same-owner exact-epoch takeover while recoveryFailed still destroys and clears", async (t) => {
  const epoch2 = { ...ACTIVE_OWNER, epoch: 2, oldEpoch: 1 };
  const h = await harness(t, WORKER_ENV, { owner: epoch2 });
  await h.bot.fetch(probeRequest("https://worker.invalid/health"));
  // Simulate an unconfirmed teardown: the repeat must re-run the full
  // sequence, never silently clear the flag without tearing down.
  (h.bot as unknown as { recoveryFailed: boolean }).recoveryFailed = true;
  const destroys = h.runtime.destroys;
  const logMark = h.logs.length;
  const repeat = await h.bot.fetch(controlRequest({ action: "takeover", expectedEpoch: 2, actor: "fixture-operator" }));
  assert.equal(repeat.status, 200);
  const state = await repeat.json();
  assert.equal(state.owner.epoch, 3);
  assert.equal(h.runtime.destroys, destroys + 1, "recovery must actually tear down");
  assert.deepEqual(h.values.get(`${AUDIT_PREFIX}3:active`), state.owner);
  assert.equal((h.bot as unknown as { recoveryFailed: boolean }).recoveryFailed, false);
  assert.ok(h.logs.slice(logMark).join("\n").includes("two-bot ownership change:"));
  const again = await h.bot.fetch(controlRequest({ action: "takeover", expectedEpoch: 3, actor: "fixture-operator" }));
  assert.equal(again.status, 200);
  assert.equal((await again.json()).owner.epoch, 3, "flag clear restores the no-op");
  assert.equal(h.runtime.destroys, destroys + 1);
});

test("same-owner exact-epoch takeover while recoveryFailed refuses an unconfirmed teardown", async (t) => {
  const epoch2 = { ...ACTIVE_OWNER, epoch: 2, oldEpoch: 1 };
  const h = await harness(t, WORKER_ENV, { owner: epoch2, stopError: true });
  await h.bot.fetch(probeRequest("https://worker.invalid/health"));
  (h.bot as unknown as { recoveryFailed: boolean }).recoveryFailed = true;
  const starts = h.starts.length;
  const repeat = await h.bot.fetch(controlRequest({ action: "takeover", expectedEpoch: 2, actor: "fixture-operator" }));
  assert.equal(repeat.status, 503, "a repeat must not report success over an unconfirmed teardown");
  assert.equal((h.bot as unknown as { recoveryFailed: boolean }).recoveryFailed, true);
  assert.equal((h.values.get(OWNER_KEY) as { phase: string }).phase, "fenced");
  assert.equal((await h.bot.fetch(probeRequest("https://worker.invalid/health"))).status, 503);
  assert.equal(h.starts.length, starts);
});

test("different-deployment takeover at the fresh epoch still commits", async (t) => {
  const h = await harness(t, { ...WORKER_ENV, CF_VERSION_METADATA: { id: "deployment-B" } });
  const asB = (body?: object) => {
    const request = controlRequest(body);
    request.headers.set(DEPLOYMENT_HEADER, "deployment-B");
    return request;
  };
  assert.equal((await h.bot.fetch(asB())).status, 200);
  const result = await h.bot.fetch(asB({ action: "takeover", expectedEpoch: 1, actor: "fixture-operator" }));
  assert.equal(result.status, 200);
  const state = await result.json();
  assert.equal(state.owner.epoch, 2);
  assert.equal(state.owner.deploymentId, "deployment-B");
  assert.equal(state.owner.phase, "active");
  assert.equal(h.runtime.destroys, 0);
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

test("DO /ops/metrics: hung container fetch aborts within the bound (504, generic body)", async (t) => {
  const h = await harness(t);
  // Prove the 6 s bound without sleeping through it: the runner cancels
  // live waits near ~5 s, so shrink only the timer and assert the production
  // code requested the full bound. The SDK resolves aborts as a 500
  // Response (never a rejection), so the fixture mirrors that shape.
  const realTimeout = AbortSignal.timeout;
  const timeout = t.mock.method(AbortSignal, "timeout", (ms: number) =>
    realTimeout(ms === 6000 ? 50 : ms));
  t.mock.method(h.bot, "containerFetch", (...args: Parameters<TwoBotContainer["containerFetch"]>) => {
    const init = args[1] as RequestInit | undefined;
    return new Promise<Response>((resolve) => {
      const sdkAbortBody = (message: string) =>
        resolve(new Response(`Error proxying request to container: ${message}`, { status: 500 }));
      if (init?.signal?.aborted) sdkAbortBody("fixture already aborted");
      else init?.signal?.addEventListener("abort", () => sdkAbortBody("The operation was aborted due to timeout"));
    });
  });
  const started = Date.now();
  const response = await h.bot.fetch(probeRequest("https://worker.invalid/ops/metrics"));
  const elapsed = Date.now() - started;
  assert.equal(response.status, 504);
  assert.equal(await response.text(), "metrics unavailable\n");
  assert.ok(
    timeout.mock.calls.some((call) => call.arguments[0] === 6000),
    "container fetch must request the 6 s abort bound",
  );
  assert.ok(elapsed < 5000, `hung fetch must return well within the bound, took ${elapsed}ms`);
});

test("DO /ops/metrics: rejected container fetch still returns 504 with a generic body", async (t) => {
  const h = await harness(t);
  t.mock.method(h.bot, "containerFetch", async () => { throw new Error("fixture transport failure"); });
  const response = await h.bot.fetch(probeRequest("https://worker.invalid/ops/metrics"));
  assert.equal(response.status, 504);
  assert.equal(await response.text(), "metrics unavailable\n");
});

test("DO /ops/metrics: oversized body returns 502 with a generic body", async (t) => {
  const h = await harness(t);
  t.mock.method(h.bot, "containerFetch", async () => new Response("m 1\n".repeat(20000)));
  const response = await h.bot.fetch(probeRequest("https://worker.invalid/ops/metrics"));
  assert.equal(response.status, 502);
  assert.equal(await response.text(), "metrics unavailable\n");
});

test("DO /ops/metrics: small body proxies status with the exposition content type", async (t) => {
  const h = await harness(t);
  t.mock.method(h.bot, "containerFetch", async () => new Response("# HELP x\n", { status: 200 }));
  const response = await h.bot.fetch(probeRequest("https://worker.invalid/ops/metrics"));
  assert.equal(response.status, 200);
  assert.equal(response.headers.get("content-type"), "text/plain; version=0.0.4; charset=utf-8");
  assert.equal(response.headers.get("cache-control"), "no-store");
  assert.equal(await response.text(), "# HELP x\n");
});

test("Worker and DO private scrape preserve the bounded REST guard fixture", async (t) => {
  const body = readFileSync(new URL("../../crates/core/tests/fixtures/rest_guard_metrics.prom", import.meta.url), "utf8");
  const samples = body.split("\n").filter(line => line.startsWith("two_bot_rest_guard_"));
  assert.equal(samples.length, 11);
  assert.equal(new Set(samples.map(line => line.split(" ")[0])).size, 11);
  assert.ok(samples.every(line => !line.includes("{") && Number.isFinite(Number(line.split(" ")[1]))));
  assert.ok(Buffer.byteLength(body) < 65_536);
  const token = "synthetic-rest-guard-scrape-token-0123456789";
  const h = await harness(t, { METRICS_SCRAPE_TOKEN: token });
  const upstream = t.mock.method(h.bot, "containerFetch", async () => new Response(body));
  const env = {
    ...WORKER_ENV,
    METRICS_SCRAPE_TOKEN: token,
    CF_VERSION_METADATA: { id: ID },
    TWO_BOT: { getByName: () => ({ fetch: (request: Request) => h.bot.fetch(request) }) },
  } as unknown as Env;
  const ctx = { waitUntil: () => {} } as ExecutionContext;
  for (const [path, auth, expected] of [
    ["/metrics", `Bearer ${token}`, 404],
    ["/ops/metrics", "", 401],
    ["/ops/metrics", `Bearer ${token}`, 200],
  ] as const) {
    const response = await worker.fetch(new Request(`https://worker.invalid${path}`, {
      headers: auth ? { authorization: auth } : {},
    }), env, ctx);
    assert.equal(response.status, expected);
    if (expected === 200) {
      assert.equal(await response.text(), body);
      assert.equal(response.headers.get("content-type"), "text/plain; version=0.0.4; charset=utf-8");
      assert.equal(response.headers.get("cache-control"), "no-store");
    } else {
      assert.equal(upstream.mock.callCount(), 0, "public/unauthenticated calls never reach the container");
    }
  }
  assert.equal(upstream.mock.callCount(), 1);
});

for (const path of ["/ops/summary", "/ops/gateway", "/ops/backfill", "/ops/backfill/progress"]) {
  test(`DO ${path}: small JSON body proxies with the JSON content type`, async (t) => {
    const h = await harness(t);
    t.mock.method(h.bot, "containerFetch", async () => new Response('{"ready":true}\n', { status: 200 }));
    const response = await h.bot.fetch(probeRequest(`https://worker.invalid${path}`));
    assert.equal(response.status, 200);
    assert.equal(response.headers.get("content-type"), "application/json");
    assert.equal(response.headers.get("cache-control"), "no-store");
    assert.equal(await response.text(), '{"ready":true}\n');
  });

  test(`DO ${path}: unknown paths are not served by the container`, async (t) => {
    const h = await harness(t);
    const response = await h.bot.fetch(probeRequest(`https://worker.invalid${path}/extra`));
    assert.equal(response.status, 404);
  });
}

test("DO /ops/backfill/progress: forwards x-two-* signing headers to the container", async (t) => {
  // Regression for the 504-every-call review finding: the SDK builds a bare
  // request from a string URL, so the DO must attach the signing headers
  // explicitly — the container 401s without them and the proxy maps that to
  // 504. The mock asserts on the request the DO hands to containerFetch.
  const h = await harness(t);
  const seen: unknown[] = [];
  t.mock.method(h.bot, "containerFetch", async (input: unknown) => {
    seen.push(input);
    return new Response('{"ok":true}\n', { status: 200 });
  });
  const response = await h.bot.fetch(new Request("https://worker.invalid/ops/backfill/progress", {
    headers: {
      "x-two-key-id": "synthetic-progress-key",
      "x-two-timestamp": "1700000000",
      "x-two-nonce": "synthetic-nonce-0123456789abcdef",
      "x-two-signature": "synthetic-progress-signature",
      [DEPLOYMENT_HEADER]: ID,
    },
  }));
  assert.equal(response.status, 200);
  assert.equal(seen.length, 1);
  const forwarded = seen[0] as Request;
  assert.ok(forwarded instanceof Request);
  assert.equal(forwarded.headers.get("x-two-key-id"), "synthetic-progress-key");
  assert.equal(forwarded.headers.get("x-two-timestamp"), "1700000000");
  assert.equal(forwarded.headers.get("x-two-nonce"), "synthetic-nonce-0123456789abcdef");
  assert.equal(forwarded.headers.get("x-two-signature"), "synthetic-progress-signature");
});

test("DO /ops/backfill/progress: container 401 stays 401 with a generic body", async (t) => {
  // A bad signature, skew, replay or unknown key is the route's own 401, not
  // an outage: the DO must not map it to 504 "ops unavailable".
  const h = await harness(t);
  t.mock.method(h.bot, "containerFetch", async () => new Response('{"error":"unauthorized"}\n', { status: 401 }));
  const response = await h.bot.fetch(new Request("https://worker.invalid/ops/backfill/progress", {
    headers: {
      "x-two-key-id": "synthetic-progress-key",
      "x-two-timestamp": "1700000000",
      "x-two-nonce": "synthetic-nonce-0123456789abcdef",
      "x-two-signature": "synthetic-progress-signature",
      [DEPLOYMENT_HEADER]: ID,
    },
  }));
  assert.equal(response.status, 401);
  assert.equal(await response.text(), "unauthorized");
});

test("DO /ops/gateway: oversized body returns 502 with a generic body", async (t) => {
  const h = await harness(t);
  t.mock.method(h.bot, "containerFetch", async () => new Response('{"k":"v"}\n'.repeat(20000)));
  const response = await h.bot.fetch(probeRequest("https://worker.invalid/ops/gateway"));
  assert.equal(response.status, 502);
  assert.equal(await response.text(), "ops unavailable\n");
});

test("DO /ops/backfill: rejected container fetch returns 504 with a generic body", async (t) => {
  const h = await harness(t);
  t.mock.method(h.bot, "containerFetch", async () => { throw new Error("fixture transport failure"); });
  const response = await h.bot.fetch(probeRequest("https://worker.invalid/ops/backfill"));
  assert.equal(response.status, 504);
  assert.equal(await response.text(), "ops unavailable\n");
});

test("Worker gates the ops JSON roll-ups behind the scrape bearer, before the redirect lookup", async (t) => {
  const token = "synthetic-ops-json-scrape-token-0123456789";
  const body = '{"ready":true,"tables":[]}\n';
  // A campaign row can never shadow the reserved roll-ups: the exact-path
  // gate runs before the invite-redirect lookup.
  const env = {
    ...WORKER_ENV,
    METRICS_SCRAPE_TOKEN: token,
    CF_VERSION_METADATA: { id: ID },
    REDIRECT_FALLBACK_CODE: "synthetic0",
    REDIRECT_MAPPINGS_JSON: "[]",
    TWO_BOT: { getByName: () => { throw new Error("unauthenticated calls must not reach the DO"); } },
  } as unknown as Env;
  const ctx = { waitUntil: () => {} } as ExecutionContext;
  // Attempt buckets are per caller and module-level, so every sub-request
  // below carries its own caller IP: earlier tests in this isolate share the
  // default "unknown" budget and must not throttle these assertions.
  let caller = 0;
  const gated = (path: string, init?: RequestInit) => {
    caller += 1;
    return worker.fetch(new Request(`https://worker.invalid${path}`, {
      ...init,
      headers: { "cf-connecting-ip": `10.99.0.${caller}`, ...(init?.headers ?? {}) },
    }), env, ctx);
  };
  for (const path of ["/ops/summary", "/ops/gateway", "/ops/backfill"]) {
    assert.equal((await gated(path)).status, 401);
    const wrong = await gated(path, {
      headers: { authorization: "Bearer wrong-bearer-not-a-secret-0123456789" },
    });
    assert.equal(wrong.status, 401);
    assert.equal(wrong.headers.get("www-authenticate"), "Bearer");
    const post = await gated(path, {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
    });
    assert.equal(post.status, 404);
  }
  // No token configured: the routes do not exist.
  const unconfigured = { ...env, METRICS_SCRAPE_TOKEN: undefined } as unknown as Env;
  for (const path of ["/ops/summary", "/ops/gateway", "/ops/backfill"]) {
    caller += 1;
    const response = await worker.fetch(new Request(`https://worker.invalid${path}`, {
      headers: { authorization: `Bearer ${token}`, "cf-connecting-ip": `10.99.0.${caller}` },
    }), unconfigured, ctx);
    assert.equal(response.status, 404);
  }
  // Authorized reads proxy the container JSON untouched.
  const h = await harness(t, { METRICS_SCRAPE_TOKEN: token });
  const upstream = t.mock.method(h.bot, "containerFetch", async () => new Response(body));
  const authed = {
    ...WORKER_ENV,
    METRICS_SCRAPE_TOKEN: token,
    CF_VERSION_METADATA: { id: ID },
    TWO_BOT: { getByName: () => ({ fetch: (request: Request) => h.bot.fetch(request) }) },
  } as unknown as Env;
  for (const path of ["/ops/summary", "/ops/gateway", "/ops/backfill"]) {
    caller += 1;
    const response = await worker.fetch(new Request(`https://worker.invalid${path}`, {
      headers: { authorization: `Bearer ${token}`, "cf-connecting-ip": `10.99.0.${caller}` },
    }), authed, ctx);
    assert.equal(response.status, 200);
    assert.equal(response.headers.get("content-type"), "application/json");
    assert.equal(response.headers.get("cache-control"), "no-store");
    assert.equal(await response.text(), body);
  }
  assert.equal(upstream.mock.callCount(), 3);
  // Unknown /ops subpaths never reach the container: the slug allowlist
  // rejects them in the redirect lookup, with no Location header.
  const before = upstream.mock.callCount();
  caller += 1;
  const unknown = await worker.fetch(new Request("https://worker.invalid/ops/unknown", {
    headers: { authorization: `Bearer ${token}`, "cf-connecting-ip": `10.99.0.${caller}` },
  }), authed, ctx);
  assert.equal(unknown.status, 404);
  assert.equal(unknown.headers.get("location"), null);
  assert.equal(upstream.mock.callCount(), before);
});

test("short scrape token stamps release and worker version on the misconfigured line", async (t) => {
  // The misconfiguration line fires once per isolate; this is the only test
  // in this file that trips it.
  const logs: string[] = [];
  t.mock.method(console, "error", (...args: unknown[]) => {
    logs.push(args.map(String).join(" "));
  });
  const short = "short-token";
  const env = {
    REDIRECT_MAPPINGS_JSON: "[]",
    REDIRECT_FALLBACK_CODE: "fallbackCode",
    CF_VERSION_METADATA: { id: ID },
    METRICS_SCRAPE_TOKEN: short,
    TWO_BOT: { getByName: () => ({ fetch: async () => new Response("must not forward") }) },
  } as unknown as Env;
  const response = await worker.fetch(
    new Request("https://worker.invalid/ops/metrics", { headers: { authorization: `Bearer ${short}` } }),
    env, { waitUntil: () => {} } as ExecutionContext,
  );
  assert.equal(response.status, 404);
  await response.text();
  assert.ok(logs.includes(JSON.stringify({
    event: "metrics_scrape_token_misconfigured",
    reason: "token_below_minimum_length",
    minimum_length: 32,
    release: "unknown",
    worker_version: ID,
    log_url: LOG_QUERY_URL,
  })));
  assert.ok(logs.every((line) => !line.includes(short)));
});

test("Worker forwards /ops/backfill/progress with x-two-* headers and no scrape bearer", async () => {
  // The signed progress read is container-authenticated (x-two-* HMAC), so
  // unlike the scrape-gated roll-ups it forwards with no bearer and no
  // configured token — and the signing headers must arrive unchanged.
  const seen: Request[] = [];
  const env = {
    ...WORKER_ENV,
    CF_VERSION_METADATA: { id: ID },
    REDIRECT_FALLBACK_CODE: "synthetic0",
    REDIRECT_MAPPINGS_JSON: "[]",
    TWO_BOT: {
      getByName: () => ({
        fetch: async (r: Request) => {
          seen.push(r);
          return new Response('{"ok":true}\n', {
            status: 200,
            headers: { "content-type": "application/json" },
          });
        },
      }),
    },
  } as unknown as Env;
  const ctx = { waitUntil: () => {} } as ExecutionContext;
  const response = await worker.fetch(new Request("https://worker.invalid/ops/backfill/progress", {
    headers: {
      "cf-connecting-ip": "10.99.7.1",
      "x-two-key-id": "synthetic-progress-key",
      "x-two-timestamp": "1700000000",
      "x-two-nonce": "synthetic-nonce-0123456789abcdef",
      "x-two-signature": "sha256=synthetic-progress-signature",
    },
  }), env, ctx);
  assert.equal(response.status, 200);
  assert.equal(seen.length, 1);
  const forwarded = seen[0]!;
  assert.equal(new URL(forwarded.url).pathname, "/ops/backfill/progress");
  assert.equal(forwarded.headers.get("x-two-key-id"), "synthetic-progress-key");
  assert.equal(forwarded.headers.get("x-two-timestamp"), "1700000000");
  assert.equal(forwarded.headers.get("x-two-nonce"), "synthetic-nonce-0123456789abcdef");
  assert.equal(forwarded.headers.get("x-two-signature"), "sha256=synthetic-progress-signature");
  assert.equal(forwarded.headers.get(DEPLOYMENT_HEADER), ID);
});

test("Worker /ops/backfill/progress: non-GET is 405 without reaching the DO", async () => {
  const env = {
    ...WORKER_ENV,
    CF_VERSION_METADATA: { id: ID },
    REDIRECT_FALLBACK_CODE: "synthetic0",
    REDIRECT_MAPPINGS_JSON: "[]",
    TWO_BOT: { getByName: () => { throw new Error("method-gated calls must not reach the DO"); } },
  } as unknown as Env;
  const ctx = { waitUntil: () => {} } as ExecutionContext;
  const response = await worker.fetch(new Request("https://worker.invalid/ops/backfill/progress", {
    method: "POST",
    headers: {
      "cf-connecting-ip": "10.99.7.2",
      "x-two-key-id": "synthetic-progress-key",
      "x-two-timestamp": "1700000000",
      "x-two-nonce": "synthetic-nonce-0123456789abcdef",
      "x-two-signature": "synthetic-progress-signature",
    },
  }), env, ctx);
  assert.equal(response.status, 405);
  assert.equal(response.headers.get("allow"), "GET");
});

test("Worker /ops/backfill/progress: missing or overlong x-two-* headers are 401 without reaching the DO", async () => {
  const env = {
    ...WORKER_ENV,
    CF_VERSION_METADATA: { id: ID },
    REDIRECT_FALLBACK_CODE: "synthetic0",
    REDIRECT_MAPPINGS_JSON: "[]",
    TWO_BOT: { getByName: () => { throw new Error("unsigned calls must not reach the DO"); } },
  } as unknown as Env;
  const ctx = { waitUntil: () => {} } as ExecutionContext;
  const unsigned = await worker.fetch(new Request("https://worker.invalid/ops/backfill/progress", {
    headers: { "cf-connecting-ip": "10.99.7.3" },
  }), env, ctx);
  assert.equal(unsigned.status, 401);
  const overlong = await worker.fetch(new Request("https://worker.invalid/ops/backfill/progress", {
    headers: {
      "cf-connecting-ip": "10.99.7.4",
      "x-two-key-id": "k".repeat(129),
      "x-two-timestamp": "1700000000",
      "x-two-nonce": "synthetic-nonce-0123456789abcdef",
      "x-two-signature": "synthetic-progress-signature",
    },
  }), env, ctx);
  assert.equal(overlong.status, 401);
});

test("Worker /ops/backfill/progress: over-budget caller is 429 without reaching the DO", async () => {
  let reached = 0;
  const env = {
    ...WORKER_ENV,
    CF_VERSION_METADATA: { id: ID },
    REDIRECT_FALLBACK_CODE: "synthetic0",
    REDIRECT_MAPPINGS_JSON: "[]",
    TWO_BOT: {
      getByName: () => ({
        fetch: async () => {
          reached += 1;
          return new Response('{"ok":true}\n', {
            status: 200,
            headers: { "content-type": "application/json" },
          });
        },
      }),
    },
  } as unknown as Env;
  const ctx = { waitUntil: () => {} } as ExecutionContext;
  // Attempt buckets are module-level: one caller IP burns one 10-token budget.
  const headers = {
    "cf-connecting-ip": "10.99.7.5",
    "x-two-key-id": "synthetic-progress-key",
    "x-two-timestamp": "1700000000",
    "x-two-nonce": "synthetic-nonce-0123456789abcdef",
    "x-two-signature": "synthetic-progress-signature",
  };
  let last: Response | undefined;
  for (let i = 0; i < 11; i++) {
    last = await worker.fetch(new Request("https://worker.invalid/ops/backfill/progress", { headers }), env, ctx);
  }
  assert.equal(last!.status, 429);
  assert.ok(last!.headers.get("retry-after"));
  assert.equal(reached, 10);
});

// Readiness monitoring uses synthetic responses only. Global fetch is stubbed
// in every alert test: no configured webhook, Worker or database is contacted.
async function alertHarness(t: TestContext, env: Partial<Env> = {}, values?: Map<string, unknown>) {
  const h = await harness(t, { KEEPALIVE_SECONDS: "60", ...env }, values);
  let status: number | Error = 503;
  t.mock.method(h.bot, "containerFetch", async (input: string | Request) => {
    if (status instanceof Error) throw status;
    // Readiness is the subject here: keep the metrics scrape healthy and
    // quiet so sustained scrape-loss tickets never ride along. Tests that
    // exercise scrape failure override this mock with their own /metrics.
    const path = new URL(typeof input === "string" ? input : input.url).pathname;
    if (path === "/metrics") return new Response(liveMetricsBody());
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
    // Transition/webhook sequence without the per-tick readiness
    // observations: container_readyz_unhealthy and
    // container_keepalive_probe_failed ride every failing tick and are pinned
    // by the alert-line release battery, not by the alert-sequence tests.
    events: () => h.logs.filter((line) => line.startsWith("{")).map((line) => JSON.parse(line))
      .filter((event) => event.event !== "container_readyz_unhealthy"
        && event.event !== "container_keepalive_probe_failed"),
  };
}

const ALERT_ENV = {
  UNREADY_ALERT_FAILURES: "3",
  OPS_ALERT_WEBHOOK_URL: WORKER_ENV.OPS_ALERT_WEBHOOK_URL,
  OPS_ALERT_FORWARDING: "on",
};

for (const failure of ["http", "fetch", "body", "missing", "invalid"] as const) {
  test(`DO dispatch tickets survive ${failure} scrape failure without a false clear`, async (t) => {
    const h = await alertHarness(t, ALERT_ENV);
    let count = 20;
    let failed = false;
    // Complete scrapes: the background validator rejects partial bodies, so the
    // streak grows from full exposition with only the reactions lane rising.
    const reactions = (value: string) => liveMetricsBody().replace(
      'two_bot_dispatch_drops_total{lane="reactions"} 0',
      `two_bot_dispatch_drops_total{lane="reactions"} ${value}`,
    );
    t.mock.method(h.bot, "containerFetch", async (input: string | Request) => {
      const path = new URL(typeof input === "string" ? input : input.url).pathname;
      if (path !== "/metrics") return new Response(null, { status: 200 });
      if (failed) {
        if (failure === "fetch") throw new Error("synthetic metrics transport failure");
        if (failure === "http") return new Response(null, { status: 503 });
        if (failure === "missing") return new Response("");
        if (failure === "invalid") return new Response(reactions("NaN"));
        return new Response(new ReadableStream({ start(controller) {
          controller.error(new Error("synthetic metrics body failure"));
        } }));
      }
      return new Response(reactions(String(count)));
    });
    for (const n of [20, 21, 22]) {
      count = n;
      await h.tick();
      assert.equal(h.posts.length, 0);
    }
    failed = true;
    await h.tick();
    failed = false;
    count = 23;
    await h.tick();
    assert.equal(h.posts.length, 0, "failure interrupted the two-rise streak; return is only a baseline");
    for (const n of [24, 25, 26]) {
      count = n;
      await h.tick();
    }
    assert.equal(h.posts.length, 1);
    const content = JSON.parse(h.posts[0]!.init.body as string).content;
    assert.match(content, /ALERT dispatch_drops:reactions \(ticket\):/);
    assert.deepEqual(h.values.get("two-bot:metrics-alerts")?.firing, ["dispatch_drops:reactions"]);
    failed = true;
    await h.tick();
    assert.equal(h.posts.length, 1, "missing evidence must not send RESOLVED");
    assert.deepEqual(h.values.get("two-bot:metrics-alerts")?.firing, ["dispatch_drops:reactions"]);
    failed = false;
    count = 27;
    await h.tick();
    await h.tick();
    assert.equal(h.posts.length, 2, "only the second valid (flat) sample proves recovery");
    assert.match(JSON.parse(h.posts[1]!.init.body as string).content, /RESOLVED dispatch_drops:reactions\. Build: unknown\. Logs: /);
    assert.deepEqual(h.values.get("two-bot:metrics-alerts")?.firing, []);
  });
}

for (const failure of ["missing", "invalid", "fetch"] as const) {
  for (const forwarding of ["on", "off"] as const) {
    test(`DO breaker ticket interrupts ${failure} samples with forwarding ${forwarding}`, async (t) => {
      const h = await alertHarness(t, { ...ALERT_ENV, OPS_ALERT_FORWARDING: forwarding });
      const gauge = "two_bot_rest_guard_breaker_open";
      const rule = "rest_admission_breaker_open";
      let open = 1;
      let failed = false;
      t.mock.method(h.bot, "containerFetch", async (input: string | Request) => {
        const path = new URL(typeof input === "string" ? input : input.url).pathname;
        if (path !== "/metrics") return new Response(null, { status: 200 });
        if (failed && failure === "fetch") throw new Error("synthetic metrics transport failure");
        const row = failed ? (failure === "missing" ? "" : `${gauge} 2`) : `${gauge} ${open}`;
        return new Response(liveMetricsBody().replace(`${gauge} 0`, row));
      });
      const state = () => h.values.get("two-bot:metrics-alerts") as MetricsAlertState;
      const transitions = () => h.events().filter((event) => event.event === "metrics_alert");
      await h.tick();
      await h.tick();
      assert.equal(state().breakerOpenStreak, 2);
      assert.equal(transitions().length, 0);
      failed = true;
      await h.tick();
      assert.equal(state().breakerOpenStreak, 0, "a gap invalidates the persisted two-open streak");
      assert.deepEqual(state().firing, []);
      failed = false;
      for (const streak of [1, 2]) {
        await h.tick();
        assert.equal(state().breakerOpenStreak, streak);
        assert.equal(transitions().length, 0, "a fresh streak needs three valid open samples");
      }
      await h.tick();
      await h.tick();
      assert.equal(state().breakerOpenStreak, 3);
      assert.deepEqual(state().firing, [rule]);
      assert.equal(transitions().length, 1, "sustained opening notifies once");
      assert.match(transitions()[0].content, /ALERT rest_admission_breaker_open \(ticket\):/);
      failed = true;
      await h.tick();
      assert.equal(state().breakerOpenStreak, 0);
      assert.deepEqual(state().firing, [rule], "missing evidence cannot resolve an existing ticket");
      failed = false;
      await h.tick();
      assert.equal(transitions().length, 1, "open after a gap neither resolves nor re-alerts");
      open = 0;
      await h.tick();
      await h.tick();
      assert.deepEqual(state().firing, []);
      assert.equal(transitions().length, 2, "only a valid closed sample recovers once");
      assert.match(transitions()[1].content, /RESOLVED rest_admission_breaker_open\./);
      assert.equal(h.posts.length, forwarding === "on" ? 2 : 0);
    });
  }
}

for (const forwarding of ["on", "off"] as const) {
  for (const gap of ["http", "fetch", "body", "missing", "invalid", "unreadable"] as const) {
    test(`DO audit halt (${forwarding} forwarding) holds across ${gap} and reconstructs without duplicate tickets`, async (t) => {
      const env = { ...ALERT_ENV, OPS_ALERT_FORWARDING: forwarding };
      const first = await alertHarness(t, env);
      let value = 1;
      let failed = false;
      const mockMetrics = (h: typeof first) => t.mock.method(h.bot, "containerFetch", async (input: string | Request) => {
        const path = new URL(typeof input === "string" ? input : input.url).pathname;
        if (path !== "/metrics") return new Response(null, { status: 200 });
        if (failed && gap === "fetch") throw new Error("synthetic scrape failure");
        if (failed && gap === "http") return new Response(null, { status: 503 });
        if (failed && gap === "body") return new Response(new ReadableStream({ start(controller) {
          controller.error(new Error("synthetic body failure"));
        } }));
        let body = liveMetricsBody().replace("two_bot_audit_delivery_halt 0", `two_bot_audit_delivery_halt ${value}`).replace(
          'two_bot_job_last_success_timestamp_seconds{job="audit_retry"} 0',
          `two_bot_job_last_success_timestamp_seconds{job="audit_retry"} ${Math.floor(Date.now() / 1000)}`,
        );
        if (failed && gap === "missing") body = body.split("\n").filter((line) => !line.startsWith("two_bot_audit_delivery_halt")).join("\n");
        if (failed && gap === "invalid") body = body.replace(`two_bot_audit_delivery_halt ${value}`, "two_bot_audit_delivery_halt 2");
        if (failed && gap === "unreadable") body = body.replace(
          'two_bot_job_consecutive_failures{job="audit_retry"} 0',
          'two_bot_job_consecutive_failures{job="audit_retry"} 1',
        );
        return new Response(body);
      });
      const stored = (h: typeof first) => h.values.get("two-bot:metrics-alerts");
      const messages = (h: typeof first) => h.events().filter((e) => e.event === "metrics_alert").map((e) => e.content);
      mockMetrics(first);
      await first.tick();
      await first.tick();
      failed = true;
      await first.tick();
      assert.deepEqual(stored(first)?.firing, []);
      assert.equal(stored(first)?.auditHaltStreak, 0, "gap interrupts pending engagement");
      failed = false;
      await first.tick();
      await first.tick();
      assert.deepEqual(messages(first), []);
      // Simulated eviction: the third valid sample continues only persisted state.
      const rebuilt = await alertHarness(t, env, first.values);
      mockMetrics(rebuilt);
      await rebuilt.tick();
      assert.deepEqual(stored(rebuilt)?.firing, ["audit_delivery_halt"]);
      assert.equal(messages(rebuilt).length, 1);
      assert.match(messages(rebuilt)[0], /ALERT audit_delivery_halt \(ticket\):/);
      assert.equal(rebuilt.posts.length, forwarding === "on" ? 1 : 0);
      failed = true;
      value = 0;
      await rebuilt.tick();
      assert.deepEqual(stored(rebuilt)?.firing, ["audit_delivery_halt"], "failure is not a recovery");
      assert.equal(messages(rebuilt).length, 1);
      // Reconstruct an active ticket too: steady engagement must not notify again.
      const active = await alertHarness(t, env, rebuilt.values);
      mockMetrics(active);
      failed = false;
      value = 1;
      await active.tick();
      assert.deepEqual(messages(active), []);
      value = 0;
      await active.tick();
      await active.tick();
      assert.deepEqual(stored(active)?.firing, []);
      assert.equal(messages(active).length, 1);
      assert.match(messages(active)[0], /RESOLVED audit_delivery_halt\./);
      assert.equal(active.posts.length, forwarding === "on" ? 1 : 0);
    });
  }
}

test("DO failed EventSub reconcile passes ticket once, survive a failed scrape, and recover on a healthy pass", async (t) => {
  const h = await alertHarness(t, ALERT_ENV);
  let healthy = 10;
  let pending = 2;
  let failedPasses = 4;
  let scrapeFails = false;
  // Complete scrapes from the shared fixture with only the pass counters moving.
  const bodyWithPasses = () => liveMetricsBody()
    .replace('two_bot_eventsub_reconcile_passes_total{outcome="healthy"} 0', `two_bot_eventsub_reconcile_passes_total{outcome="healthy"} ${healthy}`)
    .replace('two_bot_eventsub_reconcile_passes_total{outcome="pending"} 0', `two_bot_eventsub_reconcile_passes_total{outcome="pending"} ${pending}`)
    .replace('two_bot_eventsub_reconcile_passes_total{outcome="failed"} 0', `two_bot_eventsub_reconcile_passes_total{outcome="failed"} ${failedPasses}`);
  t.mock.method(h.bot, "containerFetch", async (input: string | Request) => {
    const path = new URL(typeof input === "string" ? input : input.url).pathname;
    if (path !== "/metrics") return new Response(null, { status: 200 });
    if (scrapeFails) return new Response(null, { status: 503 });
    return new Response(bodyWithPasses());
  });
  await h.tick();
  assert.equal(h.posts.length, 0, "the first valid sample is only a baseline");
  assert.deepEqual(h.values.get("two-bot:metrics-alerts")?.firing, []);

  failedPasses = 5;
  await h.tick();
  assert.equal(h.posts.length, 1, "one failed-pass increase raises one ticket");
  assert.match(JSON.parse(h.posts[0]!.init.body as string).content, /ALERT eventsub_reconcile_failed \(ticket\):/);
  assert.deepEqual(h.values.get("two-bot:metrics-alerts")?.firing, ["eventsub_reconcile_failed"]);

  failedPasses = 6;
  await h.tick();
  assert.equal(h.posts.length, 1, "repeated failed passes do not re-alert");

  scrapeFails = true;
  await h.tick();
  assert.equal(h.posts.length, 1, "a failed scrape must not send RESOLVED");
  assert.deepEqual(h.values.get("two-bot:metrics-alerts")?.firing, ["eventsub_reconcile_failed"]);

  scrapeFails = false;
  await h.tick();
  assert.equal(h.posts.length, 1, "a flat failed counter after the gap is not recovery");
  pending = 3;
  await h.tick();
  assert.equal(h.posts.length, 1, "a verification-pending pass is not recovery");

  healthy = 11;
  await h.tick();
  assert.equal(h.posts.length, 2, "a healthy-pass increase with no failed increase recovers");
  assert.match(JSON.parse(h.posts[1]!.init.body as string).content, /RESOLVED eventsub_reconcile_failed\. Build: unknown\. Logs: /);
  assert.deepEqual(h.values.get("two-bot:metrics-alerts")?.firing, []);
});

type AlertSender = "readiness" | "metrics";
async function senderHarness(t: TestContext, sender: AlertSender, env: Partial<Env>, values?: Map<string, unknown>) {
  const h = await alertHarness(t, { UNREADY_ALERT_FAILURES: "1", ...env }, values);
  let failing = true;
  t.mock.method(h.bot, "containerFetch", async (input: string | Request) => {
    const path = new URL(typeof input === "string" ? input : input.url).pathname;
    if (path === "/metrics") {
      // The readiness sender keeps a healthy quiet scrape: its subject is
      // readiness forwarding, and a permanently failed scrape would now
      // (by design) ticket its own sustained-loss incident mid-test.
      if (sender !== "metrics") return new Response(liveMetricsBody());
      const rankClass = `two_bot_job_last_error_class{job="rank",class="database"} ${failing ? 1 : 0}`;
      return new Response(liveMetricsBody(failing ? 3 : 0).replace('two_bot_job_last_error_class{job="rank",class="database"} 0', rankClass));
    }
    return new Response(null, { status: sender === "readiness" && failing ? 503 : 200 });
  });
  return { ...h, recover: () => { failing = false; } };
}

function assertAlertRecorded(h: Awaited<ReturnType<typeof senderHarness>>, sender: AlertSender) {
  if (sender === "readiness") {
    assert.equal(h.values.get("two-bot:readiness")?.alerted, true);
    assert.equal(h.events()[0].event, "container_unready_alert");
  } else {
    assert.deepEqual(h.values.get("two-bot:metrics-alerts")?.firing, ["job_consecutive_failures:rank"]);
    assert.equal(h.events()[0].event, "metrics_alert");
    assert.match(h.events()[0].content, /ALERT job_consecutive_failures:rank/);
    assert.match(h.events()[0].content, /last error class: database/);
  }
}

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
  // Chain continuity is the subject here: serve a healthy quiet scrape so the
  // ticks under test never trip the sustained scrape-loss ticket, while
  // readiness probes still travel the recorded TCP path below.
  const containerFetch = h.bot.containerFetch.bind(h.bot);
  t.mock.method(h.bot, "containerFetch", (...args: Parameters<typeof containerFetch>) => {
    const input = args[0];
    const url = typeof input === "string" ? input : input.url;
    if (new URL(url).pathname === "/metrics") return Promise.resolve(new Response(liveMetricsBody()));
    return containerFetch(...args);
  });
  await h.bot.onStart();
  assert.deepEqual(h.pending(), [initialTask], "eviction must reuse the persisted task");
  t.mock.timers.tick(60_000);
  for (let failures = 1; failures <= 3; failures++) {
    await fireSdkAlarm(t, h.bot);
    assert.equal(h.values.get("two-bot:readiness")?.failures, failures);
    assert.equal(h.pending().length, 1, "exactly one successor per firing");
    assert.equal(h.requests.filter((r) => r.pathname === "/readyz").length, failures);
    const events = h.logs.filter((line) => line.startsWith("{")).map((line) => JSON.parse(line))
      .filter((event) => event.event !== "container_readyz_unhealthy"
        && event.event !== "container_keepalive_probe_failed");
    assert.equal(events.length, failures === 2 ? 1 : failures > 2 ? 1 : 0, "traffic must not accelerate the threshold");
  }
  h.setProbeStatus(200);
  await fireSdkAlarm(t, h.bot);
  await fireSdkAlarm(t, h.bot);
  assert.deepEqual(h.logs.filter((line) => line.startsWith("{")).map((line) => JSON.parse(line))
    .filter((event) => event.event !== "container_readyz_unhealthy"
      && event.event !== "container_keepalive_probe_failed")
    .map((event) => event.event),
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
        .map((line) => JSON.parse(line))
        .filter((event) => event.event !== "container_readyz_unhealthy"
          && event.event !== "container_keepalive_probe_failed")
        .map((event) => event.event);
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
    assert.ok(!h.logs.some((line) => line.includes("probe_failed")));
    assert.equal(h.pending().length, 1, "onStart during a cold tick must not create another chain");
  });
}

test("default threshold is two failed minute ticks; absent binding is log-only", async (t) => {
  const h = await alertHarness(t);
  await h.tick();
  assert.equal(h.events().length, 0);
  await h.tick();
  assert.equal(h.events().length, 1);
  assert.equal(h.events()[0].event, "container_unready_alert");
  assert.equal(h.events()[0].consecutive_failures, 2);
  assert.equal(h.events()[0].threshold, 2);
  assert.equal(h.events()[0].status, 503);
  assert.equal(typeof h.events()[0].first_failure_at, "number");
  await h.tick();
  h.setStatus(200);
  await h.tick();
  await h.tick();
  assert.deepEqual(h.events().map((e) => e.event), ["container_unready_alert", "container_unready_recovery"]);
  assert.equal(h.posts.length, 0);
  assert.equal(h.schedules.mock.callCount(), 5);
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
    assert.equal(h.events()[1].worker_version, ID);
    h.setStatus(200);
    await h.tick();
    await h.tick();
    assert.equal(webhook.mock.callCount(), 2);
    assert.equal(h.schedules.mock.callCount(), 4);
    assert.ok(h.logs.every((line) => !line.includes(ALERT_ENV.OPS_ALERT_WEBHOOK_URL)));
  });
}

test("metrics webhook 500 stamps the serving worker version on the failure line", async (t) => {
  const h = await senderHarness(t, "metrics", {
    OPS_ALERT_FORWARDING: "on", OPS_ALERT_WEBHOOK_URL: ALERT_ENV.OPS_ALERT_WEBHOOK_URL,
  });
  const webhook = t.mock.method(globalThis, "fetch", async () =>
    new Response("synthetic error", { status: 500 }));
  await h.tick();
  await h.tick();
  assert.equal(webhook.mock.callCount(), 1);
  assertAlertRecorded(h, "metrics");
  assert.equal(h.events()[1].event, "metrics_alert_webhook_failed");
  assert.equal(h.events()[1].status, 500);
  assert.equal(h.events()[1].worker_version, ID);
});

const WEBHOOK_PATH = new URL(ALERT_ENV.OPS_ALERT_WEBHOOK_URL).pathname;
const INVALID_WEBHOOKS = {
  "non-Discord host": `https://ops.invalid${WEBHOOK_PATH}`,
  "subdomain": `https://canary.discord.com${WEBHOOK_PATH}`,
  "lookalike host": `https://discord.com.invalid${WEBHOOK_PATH}`,
  "non-default port": `https://discord.com:8443${WEBHOOK_PATH}`,
  "non-webhook path": "https://discord.com/api/channels/123/messages",
  "query": `${ALERT_ENV.OPS_ALERT_WEBHOOK_URL}?wait=true`,
  "empty query": `${ALERT_ENV.OPS_ALERT_WEBHOOK_URL}?`,
  "fragment": `${ALERT_ENV.OPS_ALERT_WEBHOOK_URL}#fragment`,
  "empty fragment": `${ALERT_ENV.OPS_ALERT_WEBHOOK_URL}#`,
  "userinfo": `https://user:synthetic@discord.com${WEBHOOK_PATH}`,
  "empty userinfo": `https://@discord.com${WEBHOOK_PATH}`,
  "HTTP": `http://discord.com${WEBHOOK_PATH}`,
  "HTTPS loopback": `https://127.0.0.1${WEBHOOK_PATH}`,
  "invalid URL": "not a URL",
  "missing id": "https://discord.com/api/webhooks//synthetic-token",
  "non-numeric id": "https://discord.com/api/webhooks/not-an-id/synthetic-token",
  "missing token": "https://discord.com/api/webhooks/123/",
  "extra path segment": `${ALERT_ENV.OPS_ALERT_WEBHOOK_URL}/messages/123`,
  "encoded token separator": `${ALERT_ENV.OPS_ALERT_WEBHOOK_URL}%2Fextra`,
  "encoded path": `https://discord.com/%61pi${WEBHOOK_PATH.slice(4)}`,
  "dot-segment alias": `https://discord.com/ignored/..${WEBHOOK_PATH}`,
  "backslash alias": `https://discord.com\\${WEBHOOK_PATH.slice(1)}`,
  "leading whitespace": ` ${ALERT_ENV.OPS_ALERT_WEBHOOK_URL}`,
  "trailing newline": `${ALERT_ENV.OPS_ALERT_WEBHOOK_URL}\n`,
};

for (const sender of ["readiness", "metrics"] as const) {
  for (const [name, binding] of Object.entries(INVALID_WEBHOOKS)) {
    test(`${sender} rejects webhook ${name} without fetching or logging the binding`, async (t) => {
      const h = await senderHarness(t, sender, { OPS_ALERT_FORWARDING: "on", OPS_ALERT_WEBHOOK_URL: binding });
      await h.tick();
      await h.tick();
      assert.equal(h.posts.length, 0);
      assertAlertRecorded(h, sender);
      assert.equal(h.events()[1].event, sender === "readiness"
        ? "container_unready_webhook_failed" : "metrics_alert_webhook_failed");
      assert.equal(h.events()[1].status, null);
      assert.equal(h.events()[1].worker_version, ID);
      assert.ok(h.logs.every((line) => !line.includes(binding)));
      assert.equal(h.schedules.mock.callCount(), 2);
    });
  }

  for (const port of ["", ":443"]) {
    test(`${sender} sends only the canonical Discord webhook on default port ${port || "implicit"}`, async (t) => {
      const h = await senderHarness(t, sender, {
        OPS_ALERT_FORWARDING: "on", OPS_ALERT_WEBHOOK_URL: `https://discord.com${port}${WEBHOOK_PATH}`,
      });
      const timeout = t.mock.method(AbortSignal, "timeout");
      await h.tick();
      await h.tick();
      assert.equal(h.posts.length, 1, "at most one attempt per persisted transition");
      assertAlertRecorded(h, sender);
      const post = h.posts[0]!;
      assert.equal(post.url, ALERT_ENV.OPS_ALERT_WEBHOOK_URL);
      assert.equal(post.init.method, "POST");
      assert.equal(post.init.redirect, "error");
      assert.ok(post.init.signal instanceof AbortSignal);
      const bound = timeout.mock.calls.find((call) => call.result === post.init.signal);
      assert.deepEqual(bound?.arguments, [6000]);
      assert.deepEqual(JSON.parse(String(post.init.body)).allowed_mentions, { parse: [], replied_user: false });
    });
  }

  for (const flag of [undefined, "off", "", "ON", "true", "1", " on", "on "]) {
    test(`${sender} forwarding ${JSON.stringify(flag)} keeps monitoring durable and log-only`, async (t) => {
      const h = await senderHarness(t, sender, { OPS_ALERT_FORWARDING: flag, OPS_ALERT_WEBHOOK_URL: ALERT_ENV.OPS_ALERT_WEBHOOK_URL });
      await h.tick();
      await h.tick();
      assert.equal(h.posts.length, 0, "installing the secret alone must never page");
      assertAlertRecorded(h, sender);
      h.recover();
      await h.tick();
      await h.tick();
      assert.equal(h.posts.length, 0);
      assert.deepEqual(h.events().map((event) => event.event), sender === "readiness"
        ? ["container_unready_alert", "container_unready_recovery"] : ["metrics_alert", "metrics_alert"]);
      assert.equal(h.values.get("two-bot:readiness")?.alerted, false);
      assert.deepEqual(h.values.get("two-bot:metrics-alerts")?.firing, []);
      if (sender === "metrics") {
        assert.match(h.events()[1].content, /RESOLVED job_consecutive_failures:rank/);
        assert.match(h.events()[1].content, /last error class: database/);
      }
      assert.equal(h.schedules.mock.callCount(), 4);
      assert.ok(h.logs.every((line) => !line.includes(ALERT_ENV.OPS_ALERT_WEBHOOK_URL)));
    });
  }

  test(`${sender} can disable forwarding without deleting the secret or replaying transitions`, async (t) => {
    const enabled = { OPS_ALERT_FORWARDING: "on", OPS_ALERT_WEBHOOK_URL: ALERT_ENV.OPS_ALERT_WEBHOOK_URL };
    const first = await senderHarness(t, sender, enabled);
    await first.tick();
    assert.equal(first.posts.length, 1);
    const disabled = await senderHarness(t, sender, { ...enabled, OPS_ALERT_FORWARDING: "off" }, first.values);
    await disabled.tick();
    disabled.recover();
    await disabled.tick();
    assert.equal(disabled.posts.length, 0);
    assert.equal(disabled.values.get("two-bot:readiness")?.alerted, false);
    assert.deepEqual(disabled.values.get("two-bot:metrics-alerts")?.firing, []);
    assert.equal(disabled.events().length, 1, "the muted recovery is still logged");
    const resumed = await senderHarness(t, sender, enabled, disabled.values);
    resumed.recover();
    await resumed.tick();
    assert.equal(resumed.posts.length, 0, "enabling must not replay a muted recovery");
  });

  test(`${sender} fetch error details never escape to logs`, async (t) => {
    const h = await senderHarness(t, sender, { OPS_ALERT_FORWARDING: "on", OPS_ALERT_WEBHOOK_URL: ALERT_ENV.OPS_ALERT_WEBHOOK_URL });
    const detail = `synthetic-fetch-error ${ALERT_ENV.OPS_ALERT_WEBHOOK_URL}`;
    const webhook = t.mock.method(globalThis, "fetch", async () => { throw new Error(detail); });
    await h.tick();
    await h.tick();
    assert.equal(webhook.mock.callCount(), 1);
    assert.equal(h.events()[1].status, null);
    assert.ok(h.logs.every((line) => !line.includes("synthetic-fetch-error") && !line.includes(ALERT_ENV.OPS_ALERT_WEBHOOK_URL)));
  });
}

for (const count of [undefined, "", "0", "-1", "1.5", "NaN", "Infinity", "1e1", " 2 ", "9007199254740992"]) {
  test(`invalid or absent threshold ${JSON.stringify(count)} defaults to a two-minute tick count`, async (t) => {
    const h = await alertHarness(t, { KEEPALIVE_SECONDS: "120", UNREADY_ALERT_FAILURES: count });
    await h.tick();
    assert.equal(h.events().length, 1);
    assert.equal(h.events()[0].threshold, 1);
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
  const h = await alertHarness(t, { ...ALERT_ENV, UNREADY_ALERT_FAILURES: "1000" });
  let failures = 3;
  t.mock.method(h.bot, "containerFetch", async (input: string | Request) => {
    const path = new URL(typeof input === "string" ? input : input.url).pathname;
    if (path === "/metrics") return new Response(liveMetricsBody(failures));
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

const METRICS_KEY = "two-bot:metrics-alerts";
const SCRAPE_FAILURE = "two-bot metrics scrape failed";
const PRIOR_METRICS: MetricsAlertState = {
  ...EMPTY_STATE, firing: ["job_consecutive_failures:rank"],
  rest429: 12, restTotal: 100, poolStreak: 2, dbErrors: 7,
  sendBlocked: 8, sendBlockedStreak: 2, breakerOpenStreak: 2, voiceOps: 40, voiceFailures: 2,
  voiceDeadLetters: 1, voiceOrphans: 1, gatewayMissed: 4, gatewayMissedSeen: true,
  receiverRefusals: { membership: 8 }, receiverRefusalsSeen: true,
  receiverRefusalStreaks: { membership: 2 },
  dispatchDrops: { reactions: 5 }, dispatchDropStreaks: { reactions: 2 },
  auditHaltStreak: 2, restPendingStuckStreak: 2,
};

async function backgroundMetricsHarness(t: TestContext) {
  const h = await alertHarness(t, ALERT_ENV);
  const previous = structuredClone(PRIOR_METRICS);
  h.values.set(METRICS_KEY, previous);
  const puts = t.mock.method(h.ctx.storage, "put");
  let response = (_signal: AbortSignal): Promise<Response> => Promise.resolve(new Response(liveMetricsBody()));
  t.mock.method(h.bot, "containerFetch", async (input: string | Request, init?: RequestInit) => {
    const path = new URL(typeof input === "string" ? input : input.url).pathname;
    return path === "/metrics" ? response(init!.signal!) : new Response(null, { status: 200 });
  });
  return {
    ...h, previous, puts,
    setMetricsResponse: (next: typeof response) => { response = next; },
    assertInterrupted: () => {
      // A failed scrape resets dispatch-drop, breaker, pending-global and
      // audit-halt evidence, preserving firing keys and unrelated baselines
      // without a recovery.
      const writes = puts.mock.calls.filter((call) => call.arguments[0] === METRICS_KEY);
      assert.equal(writes.length, 1, "one scrape-interrupt write");
      assert.deepEqual(writes[0]!.arguments[1], { ...previous, dispatchDrops: {}, dispatchDropStreaks: {}, breakerOpenStreak: 0, restPendingStuckStreak: 0, auditHaltStreak: 0 });
      assert.deepEqual(previous, PRIOR_METRICS, "no in-place baseline/streak/firing mutation");
      assert.deepEqual(h.values.get(METRICS_KEY), { ...PRIOR_METRICS, dispatchDrops: {}, dispatchDropStreaks: {}, breakerOpenStreak: 0, restPendingStuckStreak: 0, auditHaltStreak: 0 });
      assert.equal(h.posts.length, 0, "no false RESOLVED notification");
      assert.equal(h.events().length, 0, "no false transition logs");
      assert.deepEqual(h.logs.filter((line) => line !== "two-bot container started"), [SCRAPE_FAILURE]);
      assert.equal(h.schedules.mock.callCount(), 1, "failure still rearms keepalive");
    },
  };
}

function paddedMetrics(bytes: number): Uint8Array {
  const body = metricsBody();
  return new TextEncoder().encode(body + "#" + "x".repeat(bytes - body.length - 2) + "\n");
}

for (const bytes of [64 * 1024, 64 * 1024 + 1]) {
  test(`background metrics: streaming ${bytes} bytes respects the cap and cancels oversize`, async (t) => {
    const h = await backgroundMetricsHarness(t);
    let cancelled = 0;
    let pulls = 0;
    const chunks = [paddedMetrics(64 * 1024), ...(bytes > 64 * 1024 ? [new Uint8Array([32])] : [])];
    h.setMetricsResponse(async () => new Response(new ReadableStream({
      pull(controller) {
        pulls++;
        if (chunks.length) controller.enqueue(chunks.shift()!);
        else if (bytes === 64 * 1024) controller.close();
        else throw new Error("must cancel before a third read");
      },
      cancel() { cancelled++; return new Promise<void>(() => {}); },
    }, { highWaterMark: 0 })));
    await h.tick();
    if (bytes > 64 * 1024) {
      h.assertInterrupted();
      assert.equal(cancelled, 1, "cancel hook does not need to finish");
      assert.equal(pulls, 2);
    } else {
      assert.equal(cancelled, 0);
      assert.equal(pulls, 2);
      assert.deepEqual(h.values.get(METRICS_KEY)?.firing, []);
      assert.equal(h.posts.length, 1, "valid exact-cap scrape resolves");
    }
  });
}

for (const cancel of ["normal", "reject", "stall"] as const) {
  test(`background metrics: huge non-OK body is cancelled without reading (${cancel})`, async (t) => {
    const h = await backgroundMetricsHarness(t);
    let cancelled = 0;
    let pulls = 0;
    h.setMetricsResponse(async () => new Response(new ReadableStream({
      pull(controller) { pulls++; controller.enqueue(new Uint8Array(1024 * 1024)); },
      cancel() {
        cancelled++;
        if (cancel === "reject") return Promise.reject(new Error("synthetic-secret-body-marker"));
        if (cancel === "stall") return new Promise<void>(() => {});
      },
    }, { highWaterMark: 0 }), { status: 500 }));
    await h.tick();
    h.assertInterrupted();
    assert.equal(pulls, 0, "no error-body buffering or draining");
    assert.equal(cancelled, 1);
  });
}

for (const [name, body] of Object.entries({
  empty: "", nonExposition: "<html>synthetic-secret-body-marker</html>",
  commentsOnly: "# HELP x synthetic-secret-body-marker\n",
  partial: 'two_bot_job_consecutive_failures{job="rank"} 0\n',
  malformed: `${metricsBody()}synthetic-secret-body-marker\n`,
  malformedLabel: metricsBody().replace('{job="rank"}', '{job=rank}'),
  missingSeries: metricsLines().slice(1).join("\n"),
  nonfinite: metricsBody().replace("two_bot_gateway_missed_events_total 0", "two_bot_gateway_missed_events_total NaN"),
  negative: metricsBody().replace("two_bot_db_pool_connections 0", "two_bot_db_pool_connections -1"),
  duplicate: `${metricsBody()}two_bot_gateway_missed_events_total 0\n`,
  ...Object.fromEntries(["NaN", "2", "-1", "0.5", "+Inf", "-Inf"].map((value) => [
    `breaker_${value}`, metricsBody().replace("two_bot_rest_guard_breaker_open 0", `two_bot_rest_guard_breaker_open ${value}`),
  ])),
  breakerLabeled: metricsBody().replace("two_bot_rest_guard_breaker_open 0", 'two_bot_rest_guard_breaker_open{route="other"} 1'),
  breakerDuplicate: `${metricsBody()}two_bot_rest_guard_breaker_open 1\n`,
})) {
  test(`background metrics: invalid 200 preserves all previous alert state (${name})`, async (t) => {
    const h = await backgroundMetricsHarness(t);
    h.setMetricsResponse(async () => new Response(body));
    await h.tick();
    h.assertInterrupted();
  });
}

test("background metrics: interrupted stream after a valid prefix never commits or leaks details", async (t) => {
  const h = await backgroundMetricsHarness(t);
  let pulls = 0;
  h.setMetricsResponse(async () => new Response(new ReadableStream({
    pull(controller) {
      if (pulls++ === 0) controller.enqueue(new TextEncoder().encode(metricsBody()));
      else controller.error(new Error("synthetic-secret-stream-marker"));
    },
  }, { highWaterMark: 0 })));
  await h.tick();
  h.assertInterrupted();
  assert.equal(pulls, 2);
});

for (const failure of ["fetch stall", "late response", "body stall", "abort response"] as const) {
  test(`background metrics: one six-second request/body deadline bounds ${failure}`, async (t) => {
    const h = await backgroundMetricsHarness(t);
    let cancelled = 0;
    let release: (() => void) | undefined;
    let signal: AbortSignal | undefined;
    const realTimeout = AbortSignal.timeout;
    const timeout = t.mock.method(AbortSignal, "timeout", (ms: number) => realTimeout(ms === 6000 ? 30 : ms));
    const stream = () => new ReadableStream<Uint8Array>({
      pull(controller) {
        controller.enqueue(new TextEncoder().encode(metricsBody()));
        return new Promise<void>(() => {});
      },
      cancel() { cancelled++; return new Promise<void>(() => {}); },
    }, { highWaterMark: 0 });
    h.setMetricsResponse((nextSignal) => {
      signal = nextSignal;
      if (failure === "body stall") return Promise.resolve(new Response(stream()));
      if (failure === "abort response") return new Promise((resolve) => {
        nextSignal.addEventListener("abort", () => resolve(new Response(stream())), { once: true });
      });
      return new Promise((resolve) => { release = () => resolve(new Response(stream())); });
    });
    // Keep node alive: AbortSignal.timeout is an unref'ed timer. The guard
    // also fails a regression rather than leaving the test runner hanging.
    const guard = setTimeout(() => {}, 1000);
    try {
      await h.tick();
      h.assertInterrupted();
      assert.ok(signal?.aborted);
      assert.ok(timeout.mock.calls.some((call) => call.arguments[0] === 6000));
      if (failure === "late response") { release!(); await setImmediate(); }
      assert.equal(cancelled, failure === "fetch stall" ? 0 : 1);
    } finally { clearTimeout(guard); }
  });
}

test("background metrics: transport rejection is bounded and never throws", async (t) => {
  const h = await backgroundMetricsHarness(t);
  h.setMetricsResponse(async () => { throw new Error("synthetic-secret-fetch-marker"); });
  await h.tick();
  h.assertInterrupted();
});

test("background metrics: valid scrape persists before resolving, notifies once and supports extra series", async (t) => {
  const h = await backgroundMetricsHarness(t);
  h.setMetricsResponse(async () => new Response(liveMetricsBody()
    + 'new_metric{version="future"} 1\nnew_histogram_bucket{le="+Inf"} 0\n'));
  const webhook = t.mock.method(globalThis, "fetch", async () => {
    assert.deepEqual(h.values.get(METRICS_KEY)?.firing, [], "persisted before notifying");
    return new Response(null, { status: 204 });
  });
  await h.tick();
  await h.tick();
  assert.equal(webhook.mock.callCount(), 1);
  assert.equal(h.events().length, 1);
  assert.match(h.events()[0].content, /RESOLVED job_consecutive_failures:rank/);
  assert.ok(!h.logs.includes(SCRAPE_FAILURE));
});

test("background metrics: metrics storage failure cannot notify and retains the old state", async (t) => {
  const h = await backgroundMetricsHarness(t);
  const put = h.ctx.storage.put;
  t.mock.method(h.ctx.storage, "put", async (...args: Parameters<typeof put>) => {
    if (args[0] === METRICS_KEY) throw new Error("synthetic-secret-storage-marker");
    return put(...args);
  });
  await h.tick();
  assert.equal(h.values.get(METRICS_KEY), h.previous);
  assert.deepEqual(h.previous, PRIOR_METRICS);
  assert.equal(h.posts.length, 0);
  assert.equal(h.events().length, 0);
  assert.ok(h.logs.includes(SCRAPE_FAILURE));
  assert.equal(h.schedules.mock.callCount(), 1);
});
