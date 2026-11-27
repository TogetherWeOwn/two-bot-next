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
import { execFileSync } from "node:child_process";
import { createHash, generateKeyPairSync, sign } from "node:crypto";
import { fileURLToPath } from "node:url";
import { setImmediate } from "node:timers/promises";
import worker, { TwoBotContainer, gatewayComponentReady, type Env } from "../src/index.ts";
import { Container } from "@cloudflare/containers";
import { bindStagingAutomodDeploymentTransport } from "../scripts/staging-automod-deployment-transport.ts";
import { runStagingAutomodInstanceHost } from "../scripts/staging-automod-instance-host.ts";
import { bindStagingAutomodInventoryReader } from "../scripts/staging-automod-inventory-reader.ts";
import detail from "./fixtures/staging-automod-preview.json" with { type: "json" };
import databaseTargets from "./fixtures/staging-database-fingerprint.json" with { type: "json" };
import automodPublication from "./fixtures/staging-automod-publication.json" with { type: "json" };
import type { ProtectedAutomodEffects } from "../src/staging-automod-caller.ts";
import { StagingAutomodLifecycle } from "../src/staging-automod-lifecycle.ts";
import { STAGING_AUTOMOD_TARGET, StagingAutomodRecovery } from "../src/staging-automod-recovery.ts";
import { STAGING_AUTOMOD_OIDC_HEADER, STAGING_AUTOMOD_PATH } from "../src/staging-automod-bridge.ts";
import { STAGING_AUTOMOD_PLAN, type StagingAutomodPlan } from "../src/staging-automod-plan.ts";
import { OWNER_KEY, AUDIT_PREFIX, DEPLOYMENT_HEADER, CONTROL_PATH } from "../src/ownership.ts";
import { PREVIEW_HEADER } from "../src/staging-preview.ts";
import {
  AUTOMOD_STAGING_PURPOSE, StagingLifecycle, STAGING_USE_PATH, STAGING_USE_KEY, VOICE_MOVE_LEASE_HEADER,
  voiceMoveAllowed, type StagingUse, type StagingWindow,
} from "../src/staging-lifecycle.ts";
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
  owner?: unknown; readError?: boolean; stopError?: boolean; unreadableKey?: string; running?: boolean;
  values?: Map<string, unknown>;
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
  const alarms: (number | null)[] = [];
  const values = options instanceof Map ? options
    : flags.values ?? new Map<string, unknown>([[OWNER_KEY, "owner" in flags ? flags.owner : ACTIVE_OWNER]]);
  const gates: Promise<unknown>[] = [];
  let listenerPort = 8080;
  let probeStatus = 503;
  let probeResponse: ((status: number, pathname: string, redirect?: RequestRedirect) => Response) | undefined;
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
      async fetch(input: string | Request, init?: RequestInit) {
        const pathname = new URL(typeof input === "string" ? input : input.url).pathname;
        requests.push({ pathname, port });
        if (pathname === "/readyz") {
          const headers = new Headers((input instanceof Request ? input : init)?.headers);
          assert.equal(headers.has("authorization") || headers.has(PREVIEW_HEADER), false);
        }
        if (port !== listenerPort) throw new Error("connection refused: wrong container port");
        // Liveness is not readiness, even with a configured token. Preserve
        // gateway-down; a synthetic env fixture is NOT proof of a READY event.
        const status = pathname === "/readyz" ? probeStatus : 200;
        const response = probeResponse?.(status, pathname, input instanceof Request ? input.redirect : init?.redirect)
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
        if ((key === OWNER_KEY && flags.readError) || key === flags.unreadableKey) {
          throw new Error("synthetic storage error");
        }
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
      setAlarm: async (time: number) => { alarms.push(time); },
      deleteAlarm: async () => { alarms.push(null); },
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
    bot, runtime, starts, requests, logs, values, ctx, database, alarms,
    setProbeStatus: (status: number) => { probeStatus = status; },
    setProbeResponse: (factory: NonNullable<typeof probeResponse>) => { probeResponse = factory; },
    pending: () => database.prepare("SELECT * FROM container_schedules WHERE callback = 'keepalive'").all(),
  };
}

async function tickKeepalive(bot: TwoBotContainer) {
  await bot.onStart();
  const [pending] = await bot.listSchedules<{ startedAt: number }>("keepalive");
  assert.ok(pending);
  await bot.keepalive(pending.payload!, pending);
}

const BOUNDED_STAGING_ENV = {
  ...WORKER_ENV, TWO_DEPLOY_ENV: "staging", STAGING_RUNTIME_POLICY: "bounded", CONTAINER_RELEASE: "a".repeat(40),
};

test("authenticated bounded staging ownership GET attests the forwarded target without container I/O", async (t) => {
  const host = "fixture.pg.psdb.cloud";
  const databaseUrl = `postgres://synthetic_role.fixture_branch:synthetic_password@${host}/postgres`;
  const fingerprint = createHash("sha256")
    .update(JSON.stringify({ branch_id: "fixture_branch", database: "postgres", host })).digest("hex");
  const url = `https://worker.invalid${CONTROL_PATH}?runtime_db_fingerprint=${fingerprint}`;
  const stage = await harness(t, { ...BOUNDED_STAGING_ENV, DATABASE_URL: databaseUrl,
    STAGING_RECOVERY_DB_FINGERPRINT: fingerprint });
  const values = structuredClone(stage.values);
  const alarms = [...stage.alarms];
  const destroys = stage.runtime.destroys;
  const request = probeRequest(url, { headers: { authorization: `Bearer ${CONTROL_TOKEN}` } });
  const response = await stage.bot.fetch(request);
  assert.equal(response.status, 200);
  assert.equal(response.headers.get("cache-control"), "no-store");
  const state = await response.json();
  assert.deepEqual(state, {
    deploymentId: ID, owner: ACTIVE_OWNER, running: false, runtimeDatabaseFingerprint: fingerprint,
    runtimeDatabaseStartupGuard: true,
  });
  assert.equal(stage.bot.envVars.DATABASE_URL, databaseUrl);
  assert.deepEqual(stage.values, values);
  assert.deepEqual(stage.alarms, alarms);
  assert.equal(stage.runtime.destroys, destroys);
  assert.deepEqual(stage.starts, []);
  assert.deepEqual(stage.requests, []);
  assert.equal((await stage.bot.fetch(probeRequest(url))).status, 401);
});

test("ordinary and non-staging ownership control omit runtime DB evidence", async (t) => {
  const host = "fixture.pg.psdb.cloud";
  const databaseUrl = `postgres://synthetic_role.fixture_branch:synthetic_password@${host}/postgres`;
  const fingerprint = createHash("sha256")
    .update(JSON.stringify({ branch_id: "fixture_branch", database: "postgres", host })).digest("hex");
  const normalPath = `https://worker.invalid${CONTROL_PATH}`;
  const stage = await harness(t, { ...BOUNDED_STAGING_ENV, DATABASE_URL: databaseUrl });
  const prod = await harness(t, { ...WORKER_ENV, TWO_DEPLOY_ENV: "production", DATABASE_URL: databaseUrl });
  for (const [bot, url] of [[stage.bot, normalPath],
    [prod.bot, normalPath + `?runtime_db_fingerprint=${fingerprint}`]] as const) {
    const response = await bot.fetch(probeRequest(url, { headers: { authorization: `Bearer ${CONTROL_TOKEN}` } }));
    assert.equal(response.status, 200);
    assert.deepEqual(await response.json(), { deploymentId: ID, owner: ACTIVE_OWNER, running: false });
  }
});

function stagingUseRequest(action: "acquire" | "release" | "stop", window: StagingWindow): Request {
  return probeRequest(`https://worker.invalid${STAGING_USE_PATH}`, {
    method: "POST", headers: { authorization: `Bearer ${CONTROL_TOKEN}` },
    body: JSON.stringify({ ...window, action, expectedEpoch: 1, deploymentId: ID, sha: BOUNDED_STAGING_ENV.CONTAINER_RELEASE }),
  });
}

function stagingWindow(): StagingWindow {
  return { environment: "staging", runId: "123456", runAttempt: 1, expiresAt: Date.now() + 120_000 };
}

test("protected staging refuses missing or different database bindings before grant or SDK start", async (t) => {
  for (const database of [undefined, "postgres://fixture.other_branch:synthetic@fixture.pg.psdb.cloud/postgres"]) {
    const stage = await harness(t, { ...BOUNDED_STAGING_ENV, DATABASE_URL: database,
      STAGING_RECOVERY_DB_FINGERPRINT: databaseTargets.fingerprint });
    const response = await stage.bot.fetch(stagingUseRequest("acquire", stagingWindow()));
    assert.equal(response.status, 503);
    assert.equal((await response.json() as { reason: string }).reason, "staging_database_unverified");
    assert.equal(stage.values.has(STAGING_USE_KEY), false);
    await assert.rejects(stage.bot.start(), /staging_database_unverified/);
    await assert.rejects(stage.bot.startAndWaitForPorts(), /staging_database_unverified/);
    await assert.rejects(stage.bot.containerFetch("http://c/health"), /staging_database_unverified/);
    assert.deepEqual(stage.starts, []);
    assert.deepEqual(stage.requests, []);
  }
});

test("protected staging admits only the checked startup environment", async (t) => {
  const database = "postgres://fixture.fixture_branch:synthetic@fixture.pg.psdb.cloud/postgres";
  const stage = await harness(t, { ...BOUNDED_STAGING_ENV, DATABASE_URL: database,
    STAGING_RECOVERY_DB_FINGERPRINT: databaseTargets.fingerprint });
  assert.equal((await stage.bot.fetch(stagingUseRequest("acquire", stagingWindow()))).status, 200);
  const override = { envVars: { DATABASE_URL: "different" } };
  await assert.rejects(stage.bot.start(override), /staging_start_options_refused/);
  assert.deepEqual(stage.starts, []);
  await assert.rejects(stage.bot.startAndWaitForPorts(8080, undefined, override), /staging_start_options_refused/);
  assert.deepEqual(stage.starts, []);
  await assert.rejects(stage.bot.startAndWaitForPorts({ ports: 8080, startOptions: { entrypoint: ["other"] } }),
    /staging_start_options_refused/);
  assert.deepEqual(stage.starts, []);
  await stage.bot.startAndWaitForPorts(8080);
  assert.equal(stage.starts.length, 1);
  assert.equal(stage.starts[0]?.env?.DATABASE_URL, database);
  assert.equal(stage.starts[0]?.env?.STAGING_RECOVERY_DB_FINGERPRINT, undefined);
});

const AUTOMOD_JOURNAL_KEY = "two-bot:staging-automod-recovery:v1";
const AUTOMOD_RESERVATION_KEY = "two-bot:staging-automod-reservation:v1";

test("protected AutoMod purpose excludes ordinary and voice staging callers", async (t) => {
  t.mock.timers.enable({ apis: ["Date"], now: 1_800_000_000_000 });
  const window = stagingWindow(), sha = BOUNDED_STAGING_ENV.CONTAINER_RELEASE;
  const identity = { deploymentId: ID, sha, owner: { ...ACTIVE_OWNER, phase: "active" as const } };
  const ordinary: StagingUse = { ...window, deploymentId: ID, sha, epoch: ACTIVE_OWNER.epoch,
    acquiredAt: Date.now(), phase: "active" };
  const issued = { ...ordinary, purpose: AUTOMOD_STAGING_PURPOSE };
  const values = new Map<string, unknown>([[OWNER_KEY, ACTIVE_OWNER]]);
  const h = await harness(t, BOUNDED_STAGING_ENV, values);
  const staging = new StagingLifecycle(h.ctx.storage as unknown as DurableObjectStorage);
  for (const use of [ordinary, { ...ordinary, voiceMove: true as const }]) {
    values.set(STAGING_USE_KEY, use);
    assert.deepEqual(await staging.require(identity), use);
    assert.equal(voiceMoveAllowed(use, identity), use.voiceMove === true);
    await assert.rejects(staging.automodBinding(issued, identity, Date.now()), /staging_use_conflict/);
  }

  // Exercise purpose persistence solely in the isolated mock store.
  values.delete(STAGING_USE_KEY);
  const change = { ...window, action: "acquire" as const, deploymentId: ID, sha, expectedEpoch: issued.epoch };
  const publicSelection = probeRequest(`https://worker.invalid${STAGING_USE_PATH}`, { method: "POST",
    headers: { authorization: `Bearer ${CONTROL_TOKEN}` }, body: JSON.stringify({ ...change, purpose: AUTOMOD_STAGING_PURPOSE }) });
  assert.equal((await h.bot.fetch(publicSelection)).status, 400);
  assert.equal(values.has(STAGING_USE_KEY), false);
  const stopInterrupted = async () => { throw new Error("preparation interrupted"); };
  await assert.rejects(staging.acquire(change, identity, stopInterrupted, async () => {}, Date.now(), AUTOMOD_STAGING_PURPOSE),
    /preparation interrupted/);
  assert.deepEqual(values.get(STAGING_USE_KEY), { ...issued, phase: "preparing" });
  assert.equal(values.has(AUTOMOD_RESERVATION_KEY), false);
  const put = h.ctx.storage.put;
  const writes = t.mock.method(h.ctx.storage, "put", async (key: Parameters<typeof put>[0], value?: unknown) => {
    if (typeof key !== "string" && (key[STAGING_USE_KEY] as StagingUse | undefined)?.phase === "active") {
      assert.deepEqual(key[AUTOMOD_RESERVATION_KEY], { lease: issued, phase: "reserved" });
    }
    await put(key, value);
  });
  assert.deepEqual(await staging.acquire(change, identity, async () => {
    assert.deepEqual(values.get(STAGING_USE_KEY), { ...issued, phase: "preparing" });
  }, async () => {}, Date.now(), AUTOMOD_STAGING_PURPOSE), issued);
  assert.equal(writes.mock.calls.filter(({ arguments: [key] }) => typeof key !== "string"
    && (key[STAGING_USE_KEY] as StagingUse | undefined)?.phase === "active").length, 1);
  writes.mock.restore();
  assert.deepEqual(await staging.automodBinding(issued, identity, Date.now()), { lease: issued, active: true });
  await assert.rejects(h.bot.start(), /staging_use_conflict/);
  await assert.rejects(h.bot.containerFetch("http://c/readyz"), /staging_use_conflict/);
  await assert.rejects(h.bot.destroy(), /staging_use_conflict/);
  for (const action of ["acquire", "release"] as const) {
    assert.equal((await h.bot.fetch(stagingUseRequest(action, window))).status, 503);
  }
  const previewBinding = [ID, sha, issued.runId, issued.runAttempt, issued.expiresAt].join(":");
  assert.equal((await h.bot.fetch(probeRequest("https://worker.invalid/readyz", { headers: {
    authorization: `Bearer ${CONTROL_TOKEN}`, [PREVIEW_HEADER]: previewBinding,
  } }))).status, 503);
  assert.equal(voiceMoveAllowed({ ...issued, voiceMove: true }, identity), false);
  assert.deepEqual([h.starts.length, h.requests.length, h.runtime.destroys], [0, 0, 0]);

  t.mock.timers.tick(issued.expiresAt - Date.now());
  assert.deepEqual(await staging.automodBinding(issued, identity, Date.now()), { lease: issued, active: false });
  await assert.rejects(staging.automodBinding({ ...issued, expiresAt: issued.expiresAt + 1 }, identity, Date.now()),
    /staging_use_conflict/);
});

async function unusedAutomodReservation(t: TestContext) {
  t.mock.timers.enable({ apis: ["Date"], now: 1_800_000_000_000 });
  const h = await harness(t, BOUNDED_STAGING_ENV);
  const storage = h.ctx.storage as unknown as DurableObjectStorage;
  const staging = new StagingLifecycle(storage);
  const sha = BOUNDED_STAGING_ENV.CONTAINER_RELEASE;
  const identity = { deploymentId: ID, sha, owner: { ...ACTIVE_OWNER, phase: "active" as const } };
  const change = { ...stagingWindow(), action: "acquire" as const, deploymentId: ID, sha, expectedEpoch: ACTIVE_OWNER.epoch };
  const reserve = (stop = async () => {}) =>
    h.ctx.blockConcurrencyWhile(() => staging.acquire(change, identity, stop, async () => {}, Date.now(), AUTOMOD_STAGING_PURPOSE));
  const issued = await reserve();
  const calls: string[] = [];
  const effects: ProtectedAutomodEffects = {
    readRuntime: async lease => ({ lease, running: h.runtime.running }),
    readCleanupGrant: async lease => ({ lease, before: issued.expiresAt + 5_000 }),
    stop: async (lease, before) => {
      assert.deepEqual(lease, issued);
      assert.equal(before, issued.expiresAt + 5_000);
      calls.push("stop");
      await h.runtime.destroy();
      return true;
    },
    fenceAndDrain: async () => assert.fail("unused reservation has no work to drain"),
    request: async () => assert.fail("unused reservation does not access the registry"),
  };
  const recover = (lease = issued, adapter = effects, bot = h.bot) =>
    TwoBotContainer.runProtectedAutomod(bot, { action: "recover", issuedLease: lease }, adapter);
  return { ...h, staging, storage, identity, change, issued, effects, calls, recover };
}

test("the private AutoMod host acquires the exact approved lease and recovers with its own action record", async t => {
  const now = 1_800_000_000_000;
  t.mock.timers.enable({ apis: ["Date"], now });
  const version = "11111111-1111-1111-1111-111111111111";
  const env = { ...BOUNDED_STAGING_ENV, ...automodPublication.env, CF_VERSION_METADATA: { id: version } };
  const h = await harness(t, env, { owner: { ...ACTIVE_OWNER, deploymentId: version } });
  const request = new Request("https://worker.invalid", { headers: { authorization: `Bearer ${CONTROL_TOKEN}` } });
  const pin = { workerVersionId: version, sourceSha: env.CONTAINER_RELEASE, namespaceId: "a".repeat(32),
    workerDeploymentId: "22222222-2222-2222-2222-222222222222", applicationId: "33333333-3333-3333-3333-333333333333",
    rolloutId: "44444444-4444-4444-4444-444444444444", imageDigest: `sha256:${"b".repeat(64)}` };
  const change = { ...stagingWindow(), action: "acquire" as const, deploymentId: version, sha: pin.sourceSha,
    expectedEpoch: ACTIVE_OWNER.epoch };
  const target = STAGING_AUTOMOD_TARGET;
  const context = { environment: change.environment, deploymentId: version, sha: change.sha,
    runId: change.runId, runAttempt: change.runAttempt, target };
  const signal = new AbortController().signal;
  type Reservation = Awaited<ReturnType<Parameters<typeof TwoBotContainer.bindProtectedAutomodInstance>[2]["readReservation"]>>;
  let reservation: Reservation = { change, pin, target };
  let serving = pin;
  const effects: ProtectedAutomodEffects = {
    enabled: false,
    readRuntime: async lease => ({ lease, running: h.runtime.running }),
    readCleanupGrant: async lease => ({ lease, before: lease.expiresAt + 5_000 }),
    stop: async () => { assert.equal(h.runtime.running, false); return true; },
    fenceAndDrain: async () => assert.fail("unused reservation has no work to drain"),
    request: async () => assert.fail("unused reservation does not access the registry"),
  };
  const readReservation = t.mock.fn(async () => reservation);
  const adapters: Parameters<typeof TwoBotContainer.bindProtectedAutomodInstance>[2] = {
    readReservation,
    readServing: async () => serving,
    readApproval: async (_action, issuedLease) => ({ action: "recover", issuedLease, target }),
    effects,
  };
  const bind = (control = request) => TwoBotContainer.bindProtectedAutomodInstance(h.bot, control, adapters);
  const host = bind();
  const refuse = /staging_use_conflict/;
  assert.equal("bindProtectedAutomodInstance" in h.bot, false);
  assert.equal("isProtectedAutomodStage" in h.bot, false);
  await assert.rejects(host.acquireLease(pin, change, signal), refuse);
  effects.enabled = true;
  await assert.rejects(bind(new Request(request.url)).acquireLease(pin, change, signal), refuse);
  await assert.rejects(host.acquireLease(pin, { ...change, action: "stop" }, signal), refuse);
  await assert.rejects(host.acquireLease(pin, { ...change, voiceMove: true }, signal), refuse);
  await assert.rejects(host.acquireLease(pin, { ...change, runAttempt: 2 }, signal), refuse);
  const ordinary = await h.bot.fetch(new Request(`https://worker.invalid${STAGING_USE_PATH}`, {
    method: "POST", headers: { authorization: `Bearer ${CONTROL_TOKEN}`, [DEPLOYMENT_HEADER]: version },
    body: JSON.stringify({ ...change, purpose: AUTOMOD_STAGING_PURPOSE }),
  }));
  assert.equal(ordinary.status, 400);
  assert.equal(readReservation.mock.callCount(), 0);
  reservation = undefined;
  await assert.rejects(host.acquireLease(pin, change, signal), refuse);
  reservation = { change, pin, target: { ...target, guildId: "111111111111111111" } };
  await assert.rejects(host.acquireLease(pin, change, signal), refuse);
  reservation = { change: { ...change, expiresAt: change.expiresAt + 1 }, pin, target };
  await assert.rejects(host.acquireLease(pin, change, signal), refuse);
  reservation = { change, pin: { ...pin, rolloutId: "different-rollout" }, target };
  await assert.rejects(host.acquireLease(pin, change, signal), refuse);
  reservation = { change, pin, target };
  serving = { ...pin, rolloutId: "different-rollout" };
  await assert.rejects(host.acquireLease(pin, change, signal), refuse);
  const cancelled = new AbortController();
  const interrupted = TwoBotContainer.bindProtectedAutomodInstance(h.bot, request, {
    ...adapters, readServing: async () => { cancelled.abort(); return pin; },
  });
  await assert.rejects(interrupted.acquireLease(pin, change, cancelled.signal), { name: "AbortError" });
  serving = pin;
  const wrongEpoch = { ...change, expectedEpoch: change.expectedEpoch + 1 };
  reservation = { change: wrongEpoch, pin, target };
  await assert.rejects(host.acquireLease(pin, wrongEpoch, signal), /epoch_conflict/);
  reservation = { change, pin, target };
  h.runtime.running = true;
  await assert.rejects(host.acquireLease(pin, change, signal), /shutdown_unconfirmed/);
  h.runtime.running = false;
  assert.equal(h.values.has(STAGING_USE_KEY), false);
  assert.equal(h.runtime.destroys, 0);

  const issued = await host.acquireLease(pin, change, signal);
  assert.equal(issued.purpose, AUTOMOD_STAGING_PURPOSE);
  assert.deepEqual(await host.authority.readIssuedLease(context, signal), issued);
  assert.equal(await host.authority.readApproval("start", issued, signal), undefined);
  assert.deepEqual(await new StagingAutomodRecovery(h.ctx.storage as unknown as DurableObjectStorage).pending(),
    { lease: issued, phase: "reserved" });

  t.mock.timers.setTime(issued.expiresAt + 1);
  effects.enabled = false;
  const approval = await host.authority.readApproval("recover", issued, signal);
  assert.ok(approval);
  const transport = bindStagingAutomodDeploymentTransport({ env, controlBearer: CONTROL_TOKEN }, async () => ({
    schema_version: 1, observed_at: new Date(Date.now()).toISOString(),
    worker: { deployment_id: pin.workerDeploymentId, version_id: pin.workerVersionId, source_sha: pin.sourceSha,
      traffic_percent: 100, runtime_policy: "bounded" },
    container: { application_id: pin.applicationId, namespace_id: pin.namespaceId, rollout_id: pin.rolloutId,
      rollout_status: "completed", image_digest: pin.imageDigest },
    ownership: { phase: "active", epoch: issued.epoch },
  }), host.invokePrivate);
  const receipt = await transport({ action: "recover", issuedLease: issued }, approval, signal);
  assert.deepEqual(await receipt.json(), { schema: "two-bot-staging-automod-job.v1", environment: "staging",
    observedAt: new Date(Date.now()).toISOString(), action: "recover", lease: issued, verdict: "passed" });
  assert.deepEqual(h.values.get(STAGING_USE_KEY), { ...issued, phase: "released" });
  assert.equal(await host.authority.readIssuedLease(context, signal), undefined);
  assert.deepEqual(h.starts, []);
  assert.deepEqual(h.requests, []);

  // Expire after the preparing record is persisted but before issuance.
  effects.enabled = true;
  const next = { ...change, runId: "123457", expiresAt: Date.now() + 2_000 };
  reservation = { change: next, pin, target };
  const setAlarm = h.ctx.storage.setAlarm;
  const expire = t.mock.method(h.ctx.storage, "setAlarm", async (when: number) => {
    await setAlarm(when);
    t.mock.timers.setTime(next.expiresAt);
  });
  await assert.rejects(host.acquireLease(pin, next, signal), /staging_expiry_invalid/);
  expire.mock.restore();
  const preparing = h.values.get(STAGING_USE_KEY) as StagingUse;
  assert.equal(preparing.phase, "preparing");
  assert.equal(await host.authority.readIssuedLease({ ...context, runId: next.runId }, signal), undefined);
  assert.equal(await new StagingAutomodRecovery(h.ctx.storage as unknown as DurableObjectStorage).pending(), undefined);
  await h.bot.stagingExpiry();
  assert.deepEqual(h.values.get(STAGING_USE_KEY), { ...preparing, phase: "released" });
  assert.deepEqual(await h.bot.listSchedules("stagingExpiry"), []);

  const retry = { ...next, expiresAt: Date.now() + 2_000 };
  reservation = { change: retry, pin, target };
  await assert.rejects(host.acquireLease(pin, retry, signal), /staging_use_ended/);
  const following = { ...retry, runId: "123458" };
  reservation = { change: following, pin, target };
  assert.equal((await host.acquireLease(pin, following, signal)).runId, following.runId);
});

async function exerciseAutomodInstanceHost(t: TestContext, loseRecovery: boolean) {
  const now = 1_800_000_000_000;
  t.mock.timers.enable({ apis: ["Date"], now });
  const nativeTimeout = AbortSignal.timeout.bind(AbortSignal);
  const deadlines = new WeakMap<AbortSignal, AbortController>();
  t.mock.method(AbortSignal, "timeout", (ms: number) => {
    if (ms !== 2_000) return nativeTimeout(ms);
    const controller = new AbortController();
    deadlines.set(controller.signal, controller);
    return controller.signal;
  });
  const network = t.mock.method(globalThis, "fetch", async () => assert.fail("unexpected network access"));
  const { env, replies: { help, preview } } = automodPublication;
  const version = "11111111-1111-1111-1111-111111111111";
  const stage = { ...BOUNDED_STAGING_ENV, ...env, CF_VERSION_METADATA: { id: version } };
  const h = await harness(t, stage, { owner: { ...ACTIVE_OWNER, deploymentId: version } });
  const journal = new StagingAutomodRecovery(h.ctx.storage as unknown as DurableObjectStorage);
  const control = new Request("https://worker.invalid", { headers: { authorization: `Bearer ${CONTROL_TOKEN}` } });
  const target = STAGING_AUTOMOD_TARGET;
  const window = stagingWindow();
  const serving = { deploymentId: version, sha: stage.CONTAINER_RELEASE };
  const change = { ...window, ...serving, action: "acquire" as const, expectedEpoch: ACTIVE_OWNER.epoch };
  const issued: StagingUse = { ...window, ...serving, phase: "active", purpose: AUTOMOD_STAGING_PURPOSE,
    acquiredAt: now, epoch: ACTIVE_OWNER.epoch };
  const pin = { workerDeploymentId: "22222222-2222-2222-2222-222222222222", workerVersionId: version,
    sourceSha: serving.sha, applicationId: "33333333-3333-3333-3333-333333333333", namespaceId: "a".repeat(32),
    rolloutId: "44444444-4444-4444-4444-444444444444", imageDigest: `sha256:${"b".repeat(64)}` };
  const path = ".github/workflows/staging-automod-preview.yml";
  const workflowSha = "c".repeat(40);
  const repo = { id: 1396224242 };
  const runner = { GITHUB_REPOSITORY_ID: String(repo.id),
    GITHUB_WORKFLOW_REF: `TogetherWeOwn/two-bot-next/${path}@refs/heads/main`, GITHUB_REF: "refs/heads/main",
    GITHUB_REF_PROTECTED: "true", GITHUB_EVENT_NAME: "workflow_dispatch", GITHUB_RUN_ID: window.runId,
    GITHUB_RUN_ATTEMPT: "1", GITHUB_SHA: workflowSha, GITHUB_WORKFLOW_SHA: workflowSha };
  const inventorySource = { workflowSha: "d".repeat(40), runId: Number(window.runId) + 1 };
  const run = { id: Number(window.runId), event: "workflow_dispatch", run_attempt: 1, status: "in_progress",
    path, head_branch: "main", head_sha: workflowSha, repository: repo, head_repository: repo };
  let runEvidence = { ...run, head_sha: serving.sha };
  const approvals = { start: { action: "start", target, issuedLease: issued },
    recover: { action: "recover", target, issuedLease: issued } } as const;
  const policy = { memberIntake: false, enforcement: false, resume: false, registrySync: false,
    interactionDispatch: false, dedicatedIntents: true };
  const commands = `https://discord.com/api/v10/applications/${target.applicationId}/guilds/${target.guildId}/commands`;
  const detail = `${commands}/${preview.id}`;
  const steps: string[] = [];
  let command: typeof preview | undefined;
  let inventoryRollout = "55555555-5555-5555-5555-555555555555";
  let recoveryReachable = !loseRecovery;
  let acquisitionSignal: AbortSignal | undefined;
  const instance = TwoBotContainer.bindProtectedAutomodInstance(h.bot, control, {
    readReservation: async () => { steps.push("acquire"); return { change, pin, target }; },
    readServing: async () => pin,
    readApproval: async (action, lease) => {
      assert.deepEqual(lease, issued);
      steps.push(`approve:${action}`);
      return approvals[action];
    },
    effects: {
      enabled: true,
      readRuntime: async lease => ({ lease, running: h.runtime.running, policy }),
      dryRun: { policy, start: async () => { steps.push("start"); h.runtime.running = true; } },
      readCleanupGrant: async lease => ({ lease, before: issued.expiresAt + 5_000 }),
      fenceAndDrain: async lease => {
        steps.push("drain");
        return { lease, publicationDrained: true, startupDrained: true };
      },
      stop: async () => { steps.push("stop"); await h.runtime.destroy(); return true; },
      request: async (url, init) => {
        if (init.method === "DELETE") {
          assert.equal(url, detail);
          assert.equal(h.runtime.running, false);
          assert.deepEqual(command, preview);
          steps.push("delete");
          command = undefined;
          return new Response(null, { status: 204 });
        }
        assert.equal(init.method, "GET");
        if (url === "https://discord.com/api/v10/users/@me") return Response.json(automodPublication.identity);
        if (url === `${commands}?with_localizations=true`) return Response.json(command ? [help, command] : [help]);
        assert.equal(url, detail);
        return command ? Response.json(command) : Response.json({ code: 10063 }, { status: 404 });
      },
    },
  });
  const options = { enabled: true, runner, serving, inventorySource };
  const readWorker = t.mock.fn(() => ({ env: stage, controlBearer: CONTROL_TOKEN }));
  const readIssuedLease = t.mock.fn(instance.authority.readIssuedLease);
  const wiring: NonNullable<Parameters<typeof runStagingAutomodInstanceHost>[1]> = {
    reservation: { pin, change },
    readRun: async id => { assert.equal(id, window.runId); steps.push("run"); return runEvidence; },
    readWorker,
    readAuthenticatedInventory: bindStagingAutomodInventoryReader(async (sha, id, signal) => {
      assert.equal(sha, inventorySource.workflowSha);
      assert.equal(id, String(inventorySource.runId));
      assert.equal(signal.aborted, false);
      steps.push("inventory");
      return JSON.stringify({ schema_version: 1, workflow_sha: sha, workflow_run_id: Number(id),
        observed_at: new Date(Date.now()).toISOString(),
        worker: { deployment_id: pin.workerDeploymentId, version_id: version, source_sha: serving.sha,
          traffic_percent: 100, runtime_policy: "bounded" },
        container: { application_id: pin.applicationId, namespace_id: pin.namespaceId, rollout_id: inventoryRollout,
          rollout_status: "completed", image_digest: pin.imageDigest },
        ownership: { phase: "active", epoch: issued.epoch } });
    }),
    instance: {
      ...instance,
      authority: { ...instance.authority, readIssuedLease },
      acquireLease: async (...scope) => {
        acquisitionSignal = scope[2];
        const deadline = deadlines.get(acquisitionSignal);
        assert.ok(deadline);
        assert.deepEqual(await instance.acquireLease(...scope), issued);
        assert.deepEqual(h.values.get(STAGING_USE_KEY), issued);
        deadline.abort(new DOMException("synthetic acquisition deadline expired after issuance", "TimeoutError"));
        steps.push("lost-acquire-reply");
        acquisitionSignal.throwIfAborted();
        return assert.fail("the acquisition reply must be unavailable");
      },
      invokePrivate: async (attested, call, signal) => {
        steps.push(`invoke:${call.action}`);
        assert.deepEqual(attested, pin);
        assert.deepEqual(call.issuedLease, issued);
        if (call.action === "recover" && !recoveryReachable) throw new Error("synthetic recovery unavailable");
        if (call.action === "start") {
          // Seed the independently confirmed publication receipt locally. The
          // private startup caller consumes it and performs no publication.
          const publishBefore = issued.expiresAt - 5_000;
          const intent = { lease: issued, target, publishBefore, baseline: "b".repeat(64) };
          await journal.persistIntent(intent, async () => ({ lease: issued, baselineAbsent: true, publishBefore }));
          const receipt = { intent, id: preview.id, version: preview.version };
          await journal.recordCreated(receipt);
          await journal.confirmPublication(receipt);
          command = preview;
        }
        await instance.invokePrivate(attested, call, signal);
        if (call.action === "start") t.mock.timers.setTime(issued.expiresAt + 1);
      },
    },
  };

  const disabled = { ...options, enabled: false };
  const unavailable = { verdict: "failed", code: "host_unavailable" };
  const unconfirmed = { verdict: "failed", code: "context_unconfirmed" };
  const { readAuthenticatedInventory: _, ...incomplete } = wiring;
  assert.deepEqual(await runStagingAutomodInstanceHost(disabled, wiring), { verdict: "disabled" });
  assert.deepEqual(await runStagingAutomodInstanceHost(options), unavailable);
  assert.deepEqual(await runStagingAutomodInstanceHost(options, incomplete as typeof wiring), unavailable);
  assert.deepEqual(await runStagingAutomodInstanceHost(options, { ...wiring,
    reservation: { pin, change: { ...change, deploymentId: "different-worker" } } }), unconfirmed);
  assert.deepEqual(steps, []);
  assert.deepEqual(await runStagingAutomodInstanceHost(options, wiring), unconfirmed);
  assert.deepEqual(steps, ["run"]);
  assert.equal(readIssuedLease.mock.callCount(), 0);
  assert.equal(readWorker.mock.callCount(), 0);
  assert.equal(h.values.has(STAGING_USE_KEY), false);
  runEvidence = run;
  assert.deepEqual(await runStagingAutomodInstanceHost(options, wiring), { verdict: "failed", start: false, recover: false });
  assert.equal(acquisitionSignal?.aborted, true);
  assert.equal(readIssuedLease.mock.callCount(), 2);
  const confirmingSignal = readIssuedLease.mock.calls[1]?.arguments[1];
  assert.ok(confirmingSignal);
  assert.notEqual(confirmingSignal, acquisitionSignal);
  assert.equal(confirmingSignal.aborted, false);
  assert.deepEqual(h.values.get(STAGING_USE_KEY), issued);
  assert.deepEqual(await journal.pending(), { lease: issued, phase: "reserved" });
  assert.deepEqual(steps.slice(1, 4), ["run", "acquire", "lost-acquire-reply"]);
  assert.deepEqual(steps.slice(4, 6).sort(), ["approve:recover", "approve:start"]);
  assert.deepEqual(steps.slice(6), ["inventory"]);

  // Continue the same synthetic run with the original rollout. The host must
  // use the held tuple without acquiring a replacement or repeating acquisition.
  inventoryRollout = pin.rolloutId;
  steps.length = 0;
  assert.deepEqual(await runStagingAutomodInstanceHost(options, wiring), {
    verdict: loseRecovery ? "failed" : "passed", start: true, recover: !loseRecovery,
  });
  assert.equal(steps[0], "run");
  assert.deepEqual(steps.slice(1, 3).sort(), ["approve:recover", "approve:start"]);
  assert.deepEqual(steps.slice(3), ["inventory", "invoke:start", "start", "invoke:recover",
    ...(loseRecovery ? [] : ["drain", "stop", "delete"])]);
  assert.equal(Date.now(), issued.expiresAt + 1);
  if (loseRecovery) {
    assert.deepEqual(h.values.get(STAGING_USE_KEY), issued);
    assert.ok(await journal.pending());
    recoveryReachable = true;
    steps.length = 0;
    assert.deepEqual(await runStagingAutomodInstanceHost(options, wiring), { verdict: "failed", start: false, recover: true });
    assert.deepEqual(steps, ["run", "approve:recover", "inventory", "invoke:recover", "drain", "stop", "delete"]);
  }
  assert.equal(h.runtime.running, false);
  assert.equal(command, undefined);
  assert.equal(await journal.completedFor(issued), true);
  assert.deepEqual(h.values.get(STAGING_USE_KEY), { ...issued, phase: "released" });
  assert.equal(network.mock.callCount(), 0);
}

test("the AutoMod instance host acquires exactly once and runs both approved actions on the bound Container", async t => {
  await t.test("startup and recovery complete in one invocation", t => exerciseAutomodInstanceHost(t, false));
  await t.test("recovery reentry retires the original expired lease", t => exerciseAutomodInstanceHost(t, true));
});

test("protected AutoMod retires an unused reservation and retries an interrupted release", async t => {
  const h = await unusedAutomodReservation(t);
  const { issued, values, ctx, staging, calls, recover } = h;
  const journal = new StagingAutomodRecovery(h.storage);
  assert.deepEqual(await journal.pending(), { lease: issued, phase: "reserved" });
  await assert.rejects(journal.claimStartup(issued, async () => assert.fail("unused lease cannot start")),
    /staging_automod_startup_refused/);
  h.runtime.running = true;
  t.mock.timers.setTime(issued.expiresAt + 1);

  const loseStopReply = { ...h.effects, stop: async (lease: StagingUse, before: number) => {
    await h.effects.stop(lease, before);
    throw new Error("lost stop acknowledgement");
  } };
  await assert.rejects(recover(issued, loseStopReply), /staging_automod_lifecycle_refused/);
  assert.deepEqual(await staging.read(), issued);
  assert.equal(await journal.completedFor(issued), false);

  const put = ctx.storage.put;
  const write = t.mock.method(ctx.storage, "put", async (key: Parameters<typeof put>[0], value?: unknown) => {
    if (typeof key !== "string" && (key[STAGING_USE_KEY] as StagingUse | undefined)?.phase === "released") {
      throw new Error("release write interrupted");
    }
    await put(key, value);
  });
  await assert.rejects(recover(), /staging_storage_unavailable/);
  assert.deepEqual(calls, ["stop", "stop"]);
  assert.deepEqual(await staging.read(), issued);
  assert.equal(await journal.completedFor(issued), true);
  write.mock.restore();

  const restarted = await harness(t, BOUNDED_STAGING_ENV, values);
  t.mock.timers.setTime(issued.expiresAt + 5_000);
  const noEffects = { ...h.effects, readCleanupGrant: async () => assert.fail("completion receipt already durable") };
  await recover(issued, noEffects, restarted.bot);
  await recover(issued, noEffects, restarted.bot);
  assert.deepEqual(await staging.read(), { ...issued, phase: "released" });
  assert.equal(values.has(AUTOMOD_JOURNAL_KEY), false, "retirement leaves the older publication journal readable");
  assert.deepEqual(calls, ["stop", "stop"]);

  await assert.rejects(staging.acquire({ ...h.change, expiresAt: Date.now() + 10_000 }, h.identity,
    async () => assert.fail("ended attempt cannot reserve again"), async () => {}, Date.now(), AUTOMOD_STAGING_PURPOSE),
  /staging_use_ended/);
  const next = await staging.acquire({ ...h.change, runAttempt: issued.runAttempt + 1, expiresAt: Date.now() + 10_000 },
    h.identity, async () => {}, async () => {}, Date.now(), AUTOMOD_STAGING_PURPOSE);
  await assert.rejects(recover(issued, noEffects), /staging_use_conflict/);
  assert.deepEqual(await staging.read(), next);
});

test("protected AutoMod refuses unused retirement without exact durable evidence and confirmed stop", async t => {
  const { issued, values, ctx, staging, change, identity, calls, effects, recover, runtime } = await unusedAutomodReservation(t);
  const other = { ...issued, runAttempt: issued.runAttempt + 1 };
  const reserved = values.get(AUTOMOD_RESERVATION_KEY);
  const get = ctx.storage.get;
  const unreadable = t.mock.method(ctx.storage, "get", async (key: string) => {
    if (key === AUTOMOD_RESERVATION_KEY) throw new Error("reservation unavailable");
    return get(key);
  });
  await assert.rejects(recover(), /staging_automod_storage_unavailable/);
  unreadable.mock.restore();
  values.delete(AUTOMOD_RESERVATION_KEY);
  assert.deepEqual(await staging.acquire(change, identity, async () => assert.fail("already active"),
    async () => {}, Date.now(), AUTOMOD_STAGING_PURPOSE), issued);
  assert.equal(values.has(AUTOMOD_RESERVATION_KEY), false, "acquire retry cannot recreate missing evidence");
  await assert.rejects(recover(), /staging_use_conflict/);
  for (const journal of [{ phase: "reserved", lease: issued, created: null }, { phase: "reserved", lease: other },
    { phase: "retired", lease: other }]) {
    values.set(AUTOMOD_RESERVATION_KEY, journal);
    await assert.rejects(recover(), /staging_(automod_|use_conflict)/);
  }
  values.set(AUTOMOD_RESERVATION_KEY, reserved);
  await assert.rejects(recover(other), /staging_use_conflict/);
  await assert.rejects(recover(issued, { ...effects, readCleanupGrant: async () => ({ lease: other, before: issued.expiresAt }) }),
    /staging_automod_lifecycle_refused/);
  assert.deepEqual(calls, []);

  runtime.running = true;
  for (const acknowledged of [false, true]) {
    const stop = t.mock.fn(async () => acknowledged);
    await assert.rejects(recover(issued, { ...effects, stop }), /staging_automod_lifecycle_refused/);
    assert.equal(stop.mock.callCount(), 1);
    assert.deepEqual(await staging.read(), issued);
    assert.deepEqual(values.get(AUTOMOD_RESERVATION_KEY), reserved);
  }
  await recover();
  assert.deepEqual(calls, ["stop"]);
  assert.equal(runtime.running, false);
  assert.deepEqual(await staging.read(), { ...issued, phase: "released" });
});

test("protected AutoMod publication intent spends unused reservation evidence before recovery", async t => {
  const { issued, storage, staging, values, ctx, calls, effects, recover } = await unusedAutomodReservation(t);
  const journal = new StagingAutomodRecovery(storage);
  const { env, identity, replies: { help, preview } } = automodPublication;
  const { purpose: _purpose, ...lease } = issued;
  const owner = "https://discord.com/api/v10/users/@me";
  const put = ctx.storage.put;
  t.mock.method(ctx.storage, "put", async (key: Parameters<typeof put>[0], value?: unknown) => {
    const journal = typeof key === "string" ? value : key[AUTOMOD_JOURNAL_KEY];
    if ((journal as { phase?: string } | undefined)?.phase === "publishing") {
      assert.notEqual(typeof key, "string");
      assert.equal((key as Record<string, unknown>)[AUTOMOD_RESERVATION_KEY], null);
    }
    await put(key, value);
  });
  const lifecycle = new StagingAutomodLifecycle({
    ...effects, enabled: true, storage, exclusive: operation => ctx.blockConcurrencyWhile(operation),
    env: { ...env, STAGING_RUNTIME_POLICY: "bounded", TWO_MODERATION: "1", TWO_OWEN_USER_ID: preview.id },
    observe: async () => ({ lease: issued, purpose: AUTOMOD_STAGING_PURPOSE, active: true, running: false }),
    get: async url => url === owner ? identity : [help],
    request: async (url, init) => {
      if (init.method === "GET") return Response.json(url === owner ? identity : [help]);
      assert.equal(init.method, "POST");
      assert.equal(values.get(AUTOMOD_RESERVATION_KEY), null);
      calls.push("post");
      throw new Error("creation response lost");
    },
  });
  await assert.rejects(lifecycle.publish({
    lease, origin: "https://two-bot-next-staging.example.workers.dev", purposes: [AUTOMOD_STAGING_PURPOSE],
    verifiedRecoveryPair: true, verifiedRuntimeDatabase: true, verifiedGatewayIntents: true, cleanupReserveMs: 5_000,
  }), /staging_automod_publication_uncertain/);
  await assert.rejects(recover(issued, { ...effects, fenceAndDrain: async lease => {
    calls.push("drain");
    return { lease, publicationDrained: true, startupDrained: false };
  } }), /staging_automod_cleanup_unverified/);
  assert.deepEqual(calls, ["post", "drain"]);
  assert.deepEqual(await staging.read(), issued);
  assert.equal(await journal.completedFor(issued), false);
  assert.equal((await journal.pending())?.lease.runAttempt, issued.runAttempt);
  values.delete(AUTOMOD_JOURNAL_KEY);
  await assert.rejects(recover(), /staging_use_conflict/);
  assert.deepEqual(calls, ["post", "drain"]);
  assert.deepEqual(await staging.read(), issued);
});

test("protected AutoMod scopes startup and cleanup to the exact exclusive staging purpose", async (t) => {
  t.mock.timers.enable({ apis: ["Date"], now: 1_800_000_000_000 });
  const { env, replies: { preview } } = automodPublication;
  const version = "11111111-1111-1111-1111-111111111111";
  const stageEnv = { ...BOUNDED_STAGING_ENV, ...env, CF_VERSION_METADATA: { id: version } };
  const owner = { ...ACTIVE_OWNER, deploymentId: version };
  const window = stagingWindow();
  const ordinary: StagingUse = {
    ...window, deploymentId: version, sha: stageEnv.CONTAINER_RELEASE, epoch: owner.epoch,
    acquiredAt: Date.now(), phase: "active",
  };
  const issued: Readonly<StagingUse> = { ...ordinary, purpose: AUTOMOD_STAGING_PURPOSE };
  const values = new Map<string, unknown>([[OWNER_KEY, owner], [STAGING_USE_KEY, issued]]);
  const h = await harness(t, stageEnv, { values, owner });
  const storage = h.ctx.storage as unknown as DurableObjectStorage;
  const options = { memberIntake: false, enforcement: false, resume: false, registrySync: false,
    interactionDispatch: false, dedicatedIntents: true };
  const calls: string[] = [];
  const cutoff = issued.expiresAt - 5_000, cleanupBefore = issued.expiresAt + 5_000;
  const guild = `https://discord.com/api/v10/applications/${env.DISCORD_APPLICATION_ID}/guilds/${env.GUILD_ID}/commands`;
  let commandPresent = true;
  let cleanupLease = issued;
  let reportedRunning: boolean | undefined;
  const effects: ProtectedAutomodEffects = {
    readRuntime: async lease => {
      calls.push("runtime");
      assert.deepEqual(lease, issued);
      return { lease, running: reportedRunning ?? h.runtime.running, policy: options };
    },
    readCleanupGrant: async lease => {
      calls.push("grant");
      assert.deepEqual(lease, issued);
      return { lease: cleanupLease, before: cleanupBefore };
    },
    dryRun: { policy: options, start: async (lease, policy, before) => {
      calls.push("start");
      assert.deepEqual(lease, issued);
      assert.deepEqual(policy, options);
      assert.equal(before, cutoff);
      h.runtime.running = true;
    } },
    fenceAndDrain: async (lease, before) => {
      assert.deepEqual(lease, issued);
      assert.equal(before, cleanupBefore);
      calls.push("drain");
      return { lease, publicationDrained: true, startupDrained: true };
    },
    stop: async (lease, before) => {
      assert.deepEqual(lease, issued);
      assert.equal(before, cleanupBefore);
      calls.push("stop");
      h.runtime.running = false;
      return true;
    },
    request: async (url, init) => {
      if (init.method === "DELETE") {
        assert.equal(url, `${guild}/${preview.id}`);
        assert.equal(h.runtime.running, false);
        calls.push("delete");
        commandPresent = false;
        return new Response(null, { status: 204 });
      }
      assert.equal(init.method, "GET");
      if (url === "https://discord.com/api/v10/users/@me") {
        calls.push("identity");
        return Response.json(automodPublication.identity);
      }
      calls.push("registry-get");
      if (url === `${guild}?with_localizations=true`) return Response.json(commandPresent ? [preview] : []);
      assert.equal(url, `${guild}/${preview.id}`);
      return commandPresent ? Response.json(preview) : Response.json({ code: 10063 }, { status: 404 });
    },
  };
  const pin = { workerDeploymentId: "22222222-2222-2222-2222-222222222222", workerVersionId: version, sourceSha: issued.sha,
    applicationId: "33333333-3333-3333-3333-333333333333", namespaceId: "a".repeat(32),
    rolloutId: "44444444-4444-4444-4444-444444444444",
    imageDigest: `sha256:${"b".repeat(64)}` };
  let observed: typeof pin | undefined = pin;
  const readServing = t.mock.fn(async () => observed);
  const run = (action: "start" | "recover", issuedLease = issued, enabled = true) => {
    // A loosely typed adapter may carry extra properties. Only the Worker clock is authoritative.
    const adapter = { ...effects, enabled, now: () => 0 };
    const invoke = TwoBotContainer.bindProtectedAutomodInvoker(h.bot, readServing, adapter);
    return invoke(pin, { action, issuedLease }, new AbortController().signal);
  };
  const refused = /staging_(automod_|use_conflict)/;
  await assert.rejects(run("start", { ...issued, deploymentId: "other-worker" }), refused);
  await assert.rejects(run("start", { ...issued, sha: "c".repeat(40) }), refused);
  const wrongVersion = "other-worker";
  let invoke = TwoBotContainer.bindProtectedAutomodInvoker(h.bot, readServing, { ...effects, enabled: true });
  const readInventory = t.mock.fn<Parameters<typeof bindStagingAutomodDeploymentTransport>[1]>(async () => ({
    schema_version: 1, observed_at: new Date(Date.now()).toISOString(),
    worker: { deployment_id: pin.workerDeploymentId, version_id: version, source_sha: issued.sha,
      traffic_percent: 100, runtime_policy: "bounded" },
    container: { application_id: pin.applicationId, namespace_id: pin.namespaceId, rollout_id: pin.rolloutId,
      rollout_status: "completed", image_digest: pin.imageDigest },
    ownership: { phase: "active", epoch: issued.epoch },
  }));
  const transport = bindStagingAutomodDeploymentTransport({ env: stageEnv, controlBearer: CONTROL_TOKEN }, readInventory,
    (pin, call, signal) => invoke(pin, call, signal));
  const request = async (action: "start" | "recover") => {
    const call = { action, issuedLease: issued };
    const response = await transport(call, { ...call, target: STAGING_AUTOMOD_TARGET }, new AbortController().signal);
    assert.equal(response.status, 200);
    return await response.json();
  };
  const scopedReceipt = (action: "start" | "recover") => ({ schema: "two-bot-staging-automod-job.v1",
    environment: "staging", observedAt: new Date(Date.now()).toISOString(), action, lease: issued });
  assert.equal("bindProtectedAutomodInvoker" in h.bot, false);
  await assert.rejects(invoke({ ...pin, workerVersionId: wrongVersion }, {
    action: "start", issuedLease: { ...issued, deploymentId: wrongVersion },
  }, new AbortController().signal), refused);
  assert.equal(readServing.mock.callCount(), 0);
  for (const key of Object.keys(pin)) {
    observed = { ...pin, [key]: "different-pin" };
    await assert.rejects(run("start"), refused, key);
  }
  observed = undefined;
  await assert.rejects(run("recover"), refused);
  assert.equal(readServing.mock.callCount(), Object.keys(pin).length + 1);

  const deadline = new AbortController();
  const delayed = Promise.withResolvers<typeof pin>();
  const lateInvoker = TwoBotContainer.bindProtectedAutomodInvoker(h.bot, signal => {
    assert.equal(signal, deadline.signal);
    return delayed.promise;
  }, { ...effects, enabled: true });
  const result = lateInvoker(pin, { action: "start", issuedLease: issued }, deadline.signal);
  deadline.abort();
  await assert.rejects(result, /deadline exceeded/);
  delayed.resolve(pin);
  await setImmediate();
  assert.deepEqual(calls, []);

  observed = pin;
  await assert.rejects(run("start", issued, false), refused);
  await assert.rejects(run("recover"), refused);
  assert.deepEqual(calls, []);
  assert.deepEqual(values.get(STAGING_USE_KEY), issued);

  // Seed the journal through its offline storage API. Launch runs through
  // the real DO entry and cleanup never calls a live provider.
  const journal = new StagingAutomodRecovery(storage);
  const intent = { lease: issued, target: STAGING_AUTOMOD_TARGET, publishBefore: cutoff, baseline: "b".repeat(64) };
  await journal.persistIntent(intent, async () => ({ lease: issued, baselineAbsent: true, publishBefore: cutoff }));
  const receipt = { intent, id: preview.id, version: preview.version };
  await journal.recordCreated(receipt);
  await journal.confirmPublication(receipt);
  assert.deepEqual(await request("start"), { ...scopedReceipt("start"), verdict: "passed" });
  assert.equal(h.runtime.running, true);
  assert.equal(calls.filter(call => call === "start").length, 1);
  await assert.rejects(run("start"), refused);

  // Disable startup while keeping the transport's original serving pin.
  invoke = TwoBotContainer.bindProtectedAutomodInvoker(h.bot, readServing, { ...effects, enabled: false });
  t.mock.timers.tick(issued.expiresAt - Date.now());
  values.set(STAGING_USE_KEY, { ...issued, phase: "released" });
  await assert.rejects(h.bot.start(), /staging_use_conflict/);
  await h.bot.stagingExpiry();
  assert.equal(h.runtime.running, true);
  observed = { ...pin, rolloutId: "different-rollout" };
  const beforeDrift = [...calls];
  assert.deepEqual(await request("recover"), { ...scopedReceipt("recover"), verdict: "failed", code: "operation_unconfirmed" });
  assert.deepEqual(calls, beforeDrift);
  assert.equal(h.runtime.running, true);
  observed = pin;
  await assert.rejects(run("recover", { ...issued, expiresAt: issued.expiresAt + 1 }), refused);
  cleanupLease = { ...issued, epoch: issued.epoch + 1 };
  await assert.rejects(run("recover", issued, false), refused);

  cleanupLease = issued;
  reportedRunning = false;
  await assert.rejects(run("recover", issued, false), refused);
  assert.equal(h.runtime.running, true);
  assert.equal(calls.includes("drain"), false);
  assert.equal(calls.includes("registry-get"), false);
  assert.ok(await journal.pending());

  reportedRunning = undefined;
  // The journal can finish before the lease retirement write succeeds.
  values.set(STAGING_USE_KEY, issued);
  const put = h.ctx.storage.put;
  const failRelease = t.mock.method(h.ctx.storage, "put", async (...args: Parameters<typeof put>) => {
    const [key] = args;
    if (typeof key !== "string" && Object.hasOwn(key, STAGING_USE_KEY)) throw new Error("synthetic lease write failure");
    await put(...args);
  });
  assert.deepEqual(await request("recover"), { ...scopedReceipt("recover"), verdict: "failed", code: "operation_unconfirmed" });
  failRelease.mock.restore();
  assert.deepEqual(calls.filter(call => ["start", "drain", "stop", "delete"].includes(call)),
    ["start", "drain", "stop", "delete"]);
  assert.equal(calls.filter(call => call === "registry-get").length, 8);
  assert.equal(await journal.completedFor(issued), true);
  assert.deepEqual(values.get(STAGING_USE_KEY), issued);
  await assert.rejects(h.bot.start(), /staging_use_conflict/);

  const afterCleanup = calls.length;
  assert.deepEqual(await request("recover"), { ...scopedReceipt("recover"), verdict: "passed" });
  assert.equal(readInventory.mock.callCount(), 1);
  assert.deepEqual(values.get(STAGING_USE_KEY), { ...issued, phase: "released" });
  await run("recover", issued, false);
  assert.deepEqual(calls.slice(afterCleanup), [], "completed-journal retries must perform no external calls");
  assert.equal(h.runtime.running, false);
});

test("staging preview extends readiness without extra cold lifecycle activity", async (t) => {
  t.mock.timers.enable({ apis: ["Date"] });
  const { application_id: app, guild_id: guild, name, id } = detail;
  const env = { ...BOUNDED_STAGING_ENV, GUILD_ID: guild, DISCORD_APPLICATION_ID: app };
  const window = stagingWindow(), sha = env.CONTAINER_RELEASE;
  const grant = { ...window, deploymentId: ID, sha, epoch: 1, acquiredAt: Date.now(), phase: "active" };
  const report = { components: ["process", "gateway", "database", "token_invalid"].map((name) => [name, "ready"]),
    jobs: {}, build_revision: sha, build_id: `${window.runId}-${window.runAttempt}` };
  const list = `https://discord.com/api/v10/applications/${app}/guilds/${guild}/commands`;
  const urls = ["https://discord.com/api/v10/users/@me", list, `${list}/${id}`];
  const replies = (end: unknown = detail) => [Response.json({ id: app, bot: true }), Response.json([detail]), Response.json(end)];
  let queue = replies(), onDiscordRead: (() => void) | undefined;
  const discord = t.mock.method(globalThis, "fetch", async (url: string, init: RequestInit) => {
    assert.equal(url, urls[discord.mock.callCount() % 3]);
    assert.equal(init.method, "GET");
    assert.equal(init.redirect, "error");
    assert.ok(init.signal);
    assert.equal(new Headers(init.headers).get("authorization"), `Bot ${env.DISCORD_TOKEN}`);
    onDiscordRead?.();
    return queue.shift() ?? Response.error();
  });
  const binding = [ID, sha, window.runId, window.runAttempt, window.expiresAt].join(":");
  const request = (headers = {}, method = "GET", query = "") =>
    new Request(`https://worker.invalid/readyz${query}`, { method, headers: { ...headers, "cf-connecting-ip": "preview-fixture" } });
  const opted = (patch = {}, method = "GET", query = "") =>
    request({ authorization: `Bearer ${CONTROL_TOKEN}`, [PREVIEW_HEADER]: binding, ...patch }, method, query);
  const renew = t.mock.method(Container.prototype, "renewActivityTimeout");
  const cold = (use: unknown, running: boolean) =>
    harness(t, env, { values: new Map<string, unknown>([[OWNER_KEY, ACTIVE_OWNER], [STAGING_USE_KEY, use]]), running });
  const edge = (h: Awaited<ReturnType<typeof cold>>) => ({ ...env,
    TWO_BOT: { getByName: () => h.bot } }) as unknown as Env;
  const effects = (h: Awaited<ReturnType<typeof cold>>) => ({
    alarms: h.alarms, starts: h.starts.length, stops: h.runtime.destroys, storage: h.values,
    renewals: renew.mock.calls.filter((call) => call.this === h.bot).length,
    schedules: h.database.prepare("SELECT callback, time, payload FROM container_schedules ORDER BY callback").all(),
  });
  for (const [use, running] of [[grant, false], [undefined, true]] as const) {
    const ordinary = await cold(use, running), preview = await cold(use, running);
    ordinary.setProbeResponse(() => Response.json(report));
    preview.setProbeResponse(() => Response.json(report));
    const normal = await worker.fetch(request(), edge(ordinary));
    const withPreview = await worker.fetch(opted(), edge(preview));
    const bodies = await Promise.all([withPreview.text(), normal.text()]);
    await setImmediate();
    assert.equal(withPreview.status, normal.status);
    assert.ok(ordinary.alarms.some((time) => typeof time === "number"), "pinned SDK writes on cold construction");
    assert.deepEqual(effects(preview), effects(ordinary));
    if (use) {
      assert.equal(ordinary.starts.length, 1);
      assert.equal(discord.mock.callCount(), 3);
      assert.equal(bodies[0], bodies[1]);
      assert.deepEqual(JSON.parse(withPreview.headers.get(PREVIEW_HEADER)!), { registry: "pass", command: name,
        deploymentId: ID, sha, runId: window.runId, runAttempt: window.runAttempt, expiresAt: window.expiresAt });
    } else assert.equal(withPreview.headers.has(PREVIEW_HEADER), false);
  }
  const h = await cold(grant, true);
  const send = (req = opted()) => worker.fetch(req, edge(h));
  discord.mock.resetCalls();
  for (const [req, status] of [[opted({ authorization: "Bearer incorrect" }), 401], [opted({}, "POST"), 405],
    [opted({}, "GET", "?guild=other"), 400], [opted({ [PREVIEW_HEADER]: "wrong-attempt" }), 503]] as const) {
    assert.equal((await send(req)).status, status);
  }
  assert.equal((await worker.fetch(opted(), { ...edge(h), CF_VERSION_METADATA: { id: "stale-edge" } } as Env)).status, 503);
  h.values.set(STAGING_USE_KEY, { ...grant, acquiredAt: Date.now() - 2000, expiresAt: Date.now() - 1000 });
  assert.equal((await send()).status, 503);
  assert.equal(discord.mock.callCount(), 0);
  const unready = { ...report, components: report.components.map(([key, state]) => [key, key === "gateway" ? "down" : state]),
    gateway_failure: { phase: "durable_gateway", class: "checkpoint_load_failed" } };
  for (const [change, read, status, calls] of [["redirect", 1, 500, 0], ["unready", 1, 503, 0], ["unready", 2, 503, 3],
    ["drift", 1, 503, 0], ["drift", 2, 503, 3], ["release", 0, 503, 1], ["release", 1, 503, 0],
    ["release", 2, 503, 3], ["expiry", 2, 503, 3]] as const) {
    discord.mock.resetCalls();
    queue = replies();
    h.values.set(STAGING_USE_KEY, grant);
    onDiscordRead = read === 0 ? () => h.values.set(STAGING_USE_KEY, { ...grant, phase: "released" }) : undefined;
    let probes = 0;
    h.setProbeResponse((_, path, redirect) => {
      if (path !== "/readyz" || ++probes !== read) return Response.json(report);
      if (change === "redirect" && redirect === "manual") return Response.redirect("https://redirect.invalid");
      if (change === "unready") return Response.json(unready, { status: 503 });
      if (change === "release") h.values.set(STAGING_USE_KEY, { ...grant, phase: "released" });
      if (change === "expiry") t.mock.timers.setTime(grant.expiresAt);
      return Response.json(change === "drift" ? { ...report, build_id: "9-9" } : report);
    });
    const result = await send();
    assert.deepEqual([result.status, result.headers.has(PREVIEW_HEADER)],
      [status, false], `${change} during readiness read ${read}`);
    assert.equal(discord.mock.callCount(), calls);
    if (change === "unready") assert.deepEqual(await result.json(), unready);
  }
  t.mock.timers.setTime(grant.acquiredAt);
  h.setProbeResponse(() => Response.json(report));
  for (const bad of [replies({ ...detail, default_member_permissions: "0" }), replies({ ...detail, options: [] }),
    replies({ ...detail, version: "323456789012345678" }), [replies()[0]!, Response.json([])],
    [replies()[0]!, new Response("x".repeat(256 * 1024 + 1))]]) {
    discord.mock.resetCalls();
    queue = [...bad];
    const result = await send();
    assert.deepEqual([result.status, result.headers.has(PREVIEW_HEADER)], [200, false]);
    assert.deepEqual(await result.json(), report);
    assert.equal(discord.mock.callCount(), bad.length);
  }
});

test("staging registry absence is a stopped, authenticated GET under the exact lease", async (t) => {
  const { application_id: app, guild_id: guild } = detail;
  const env = { ...BOUNDED_STAGING_ENV, GUILD_ID: guild, DISCORD_APPLICATION_ID: app };
  const window = stagingWindow(), sha = env.CONTAINER_RELEASE;
  const grant = { ...window, deploymentId: ID, sha, epoch: 1, acquiredAt: Date.now(), phase: "active" };
  const h = await harness(t, env, { values: new Map<string, unknown>([[OWNER_KEY, ACTIVE_OWNER], [STAGING_USE_KEY, grant]]) });
  const edge = { ...env, TWO_BOT: { getByName: () => h.bot } } as unknown as Env;
  const binding = [ID, sha, window.runId, window.runAttempt, window.expiresAt].join(":");
  const request = (headers = {}, method = "GET", query = "") => probeRequest(
    `https://worker.invalid${STAGING_USE_PATH}${query}`, { method,
      headers: { authorization: `Bearer ${CONTROL_TOKEN}`, [PREVIEW_HEADER]: binding, ...headers } });
  const send = (req = request()) => worker.fetch(req, edge);
  const sibling = { ...detail, name: "help" };
  const defaultSlash = { id: "18446744073709551615", version: "18446744073709551615",
    application_id: app, guild_id: guild, name: "rank" };
  let list = Response.json([sibling, defaultSlash]);
  let botId = app;
  let afterList = () => {};
  const calls: string[] = [];
  const mock = t.mock.method(globalThis, "fetch", async (url: string, init: RequestInit) => {
    calls.push(url);
    assert.equal(init.method, "GET");
    assert.equal(init.redirect, "error");
    assert.ok(init.signal);
    assert.equal(new Headers(init.headers).get("authorization"), `Bot ${env.DISCORD_TOKEN}`);
    if (url === "https://discord.com/api/v10/users/@me") return Response.json({ id: botId, bot: true });
    assert.equal(url, `https://discord.com/api/v10/applications/${app}/guilds/${guild}/commands`);
    afterList();
    return list;
  });
  for (const [req, status] of [[request({ authorization: "Bearer wrong" }), 401],
    [request({}, "POST"), 405], [request({}, "GET", "?ignored"), 400],
    [request({ [PREVIEW_HEADER]: "other-window" }), 503]] as const) {
    assert.equal((await send(req)).status, status);
  }
  h.runtime.running = true;
  assert.equal((await send()).status, 503);
  h.runtime.running = false;
  assert.equal(mock.mock.callCount(), 0);
  const ok = await send();
  assert.equal(ok.status, 200);
  assert.match(ok.headers.get("cache-control") ?? "", /no-store/);
  assert.deepEqual(await ok.json(), { deploymentId: ID, sha, owner: ACTIVE_OWNER,
    use: grant, running: false, registry: "absent" });
  assert.equal(calls.length, 2);
  botId = "wrong-app";
  assert.equal((await send()).status, 503);
  assert.equal(calls.length, 3);
  botId = app;
  for (const bad of [Response.json([detail]), Response.json([sibling, sibling]),
    Response.json([{ ...defaultSlash, id: "18446744073709551616" }]),
    Response.json([{ ...defaultSlash, version: "18446744073709551616" }]),
    Response.json([defaultSlash, { ...sibling, name: defaultSlash.name }]),
    Response.json([null]), Response.json({ commands: [] }), Response.json([{ ...sibling, guild_id: "wrong" }]),
    new Response(`[{${JSON.stringify(sibling).slice(1, -1)},"na\\u006de":"other"}]`),
    new Response(new Uint8Array([91, 34, 255, 34, 93])),
    new Response("[]", { status: 403 }), Response.redirect("https://redirect.invalid/"),
    new Response(" ".repeat(256 * 1024 + 1))]) {
    list = bad;
    const rejected = await send();
    assert.equal(rejected.status, 503);
    assert.deepEqual(await rejected.json(), { error: "ownership_fenced", reason: "operation_failed" });
  }
  list = Response.json([]);
  afterList = () => { h.runtime.running = true; };
  const resumed = await send();
  assert.equal(resumed.status, 503);
  assert.deepEqual(await resumed.json(), { error: "ownership_fenced", reason: "operation_failed" });
  assert.deepEqual(h.values.get(STAGING_USE_KEY), grant);
  h.runtime.running = false;
  list = Response.json([]);
  afterList = () => { h.values.set(STAGING_USE_KEY, { ...grant, phase: "released" }); };
  assert.equal((await send()).status, 503);
  assert.deepEqual(h.starts, []);
  assert.deepEqual(h.requests, []);
});

for (const path of ["/health", "/readyz", "/version"]) {
  test(`bounded staging: idle ${path} cannot start or arm keepalive`, async (t) => {
    const h = await harness(t, BOUNDED_STAGING_ENV);
    const response = await h.bot.fetch(probeRequest(`https://worker.invalid${path}`));
    assert.equal(response.status, 503);
    assert.equal(h.starts.length, 0);
    assert.equal(h.requests.length, 0);
    assert.equal(h.pending().length, 0);
    assert.equal((await h.bot.listSchedules("stagingExpiry")).length, 0);
    await assert.rejects(h.bot.start(), /staging_use_required/);
    await assert.rejects(h.bot.startAndWaitForPorts(8080), /staging_use_required/);
    await assert.rejects(h.bot.containerFetch("http://c/health"), /staging_use_required/);
    assert.equal(h.starts.length, 0, "all native SDK start paths are fenced");
  });
}

test("bounded staging: stop tears down the live process without spending its grant", async (t) => {
  const h = await harness(t, BOUNDED_STAGING_ENV);
  const window = stagingWindow();
  const action = () => h.bot.fetch(stagingUseRequest("stop", window));
  const acquired = await h.bot.fetch(stagingUseRequest("acquire", window));
  assert.equal(acquired.status, 200);
  const grant = await acquired.json() as { running: boolean; use: unknown };
  await h.bot.fetch(probeRequest("https://worker.invalid/health")).then((r) => r.arrayBuffer());
  assert.equal(h.runtime.running, true);
  const failure = t.mock.method(h.runtime, "destroy", async () => { throw new Error("stop failed"); });
  assert.equal((await action()).status, 503);
  assert.equal(h.runtime.running, true);
  assert.deepEqual(h.values.get(STAGING_USE_KEY), grant.use);
  failure.mock.restore();
  const stopped = await action();
  assert.equal(stopped.status, 200);
  assert.deepEqual(await stopped.json(), { ...grant, running: false });
  assert.equal(h.runtime.running, false);
  assert.equal(h.pending().length, 0);
  assert.equal((await h.bot.listSchedules("stagingExpiry"))[0]?.time, Math.ceil(window.expiresAt / 1000));
  await h.bot.fetch(probeRequest("https://worker.invalid/health")).then((r) => r.arrayBuffer());
  assert.equal(h.starts.length, 2);
  assert.equal((await h.bot.fetch(stagingUseRequest("release", window))).status, 200);
  assert.equal(h.runtime.running, false);
});

test("bounded staging: authenticated acquire authorizes exact identity without starting; release is idempotent", async (t) => {
  const h = await harness(t, BOUNDED_STAGING_ENV);
  const window = stagingWindow();
  const acquired = await h.bot.fetch(stagingUseRequest("acquire", window));
  assert.equal(acquired.status, 200);
  const grant = await acquired.json() as { running: boolean; use: { phase: string; expiresAt: number } };
  assert.equal(grant.running, false);
  assert.equal(grant.use.phase, "active");
  assert.equal(grant.use.expiresAt, window.expiresAt);
  assert.equal(h.starts.length, 0);
  const [expiry] = await h.bot.listSchedules("stagingExpiry");
  assert.equal(expiry?.time, Math.ceil(window.expiresAt / 1000));
  assert.equal(expiry?.payload, null, "SDK SQL bindings must be serializable, not undefined");
  const ready = await h.bot.fetch(probeRequest("https://worker.invalid/health"));
  assert.equal(ready.status, 200);
  await ready.arrayBuffer();
  assert.equal(h.runtime.running, true);
  assert.equal(h.pending().length, 1);
  const first = await h.bot.fetch(stagingUseRequest("release", window));
  assert.equal(first.status, 200);
  assert.equal(h.runtime.running, false);
  assert.equal(h.pending().length, 0);
  assert.equal((await h.bot.listSchedules("stagingExpiry")).length, 0);
  assert.equal((await h.bot.fetch(stagingUseRequest("release", window))).status, 200);
  assert.equal((await h.bot.fetch(stagingUseRequest("acquire", window))).status, 503);
  assert.equal((await h.bot.fetch(probeRequest("https://worker.invalid/readyz"))).status, 503);
  assert.equal(h.starts.length, 1);
  const receipts = h.logs.filter((line) => line.startsWith("{"))
    .map((line) => JSON.parse(line)).filter((event) => event.event === "staging_use");
  assert.deepEqual(receipts.map((event) => [event.action, event.outcome, event.running]),
    [["acquire", "authorized", false], ["release", "stopped", false], ["release", "stopped", false]]);
  for (const receipt of receipts) {
    assert.equal(receipt.environment, "staging");
    assert.equal(receipt.run_id, window.runId);
    assert.equal(receipt.run_attempt, window.runAttempt);
    assert.equal(receipt.expires_at_ms, window.expiresAt);
    assert.equal(receipt.deployment_id, ID);
    assert.equal(receipt.sha, BOUNDED_STAGING_ENV.CONTAINER_RELEASE);
    assert.equal(receipt.epoch, 1);
    assert.ok(Number.isFinite(Date.parse(receipt.observed_at)));
    assert.equal(JSON.stringify(receipt).includes(CONTROL_TOKEN), false);
  }
});

test("bounded staging: control authentication, version stamping and bounded body parsing precede acquisition", async (t) => {
  const h = await harness(t, BOUNDED_STAGING_ENV);
  const window = stagingWindow();
  const unauthorized = stagingUseRequest("acquire", window);
  unauthorized.headers.set("authorization", "Bearer incorrect-synthetic-token");
  assert.equal((await h.bot.fetch(unauthorized)).status, 401);
  assert.equal(h.values.has(STAGING_USE_KEY), false);
  const noVersion = stagingUseRequest("acquire", window);
  noVersion.headers.delete(DEPLOYMENT_HEADER);
  assert.equal((await h.bot.fetch(noVersion)).status, 503);
  assert.equal(h.values.has(STAGING_USE_KEY), false);
  for (const body of ["{", "x".repeat(1025), JSON.stringify({ environment: "production" })]) {
    const malformed = probeRequest(`https://worker.invalid${STAGING_USE_PATH}`, {
      method: "POST", headers: { authorization: `Bearer ${CONTROL_TOKEN}` }, body,
    });
    assert.equal((await h.bot.fetch(malformed)).status, 400);
    assert.equal(h.values.has(STAGING_USE_KEY), false);
  }
  let stamped = "";
  const forged = stagingUseRequest("acquire", window);
  forged.headers.set(DEPLOYMENT_HEADER, "deployment-forged");
  const result = await worker.fetch(forged, {
    ...BOUNDED_STAGING_ENV,
    TWO_BOT: { getByName: () => ({ fetch: async (request: Request) => {
      stamped = request.headers.get(DEPLOYMENT_HEADER) ?? "";
      return h.bot.fetch(request);
    } }) },
  } as unknown as Env, {} as ExecutionContext);
  assert.equal(stamped, ID);
  assert.equal(result.status, 200);
  assert.equal(h.starts.length, 0);
});

test("bounded staging: queued status returns unknown without asserting cleanup", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  let calls = 0;
  let markCalled!: () => void;
  const called = new Promise<void>((resolve) => { markCalled = resolve; });
  const env = { ...BOUNDED_STAGING_ENV, TWO_BOT: { getByName: () => ({ fetch: () => {
    calls++;
    markCalled();
    return new Promise<Response>(() => {});
  } }) } } as unknown as Env;
  const request = new Request(`https://worker.invalid${STAGING_USE_PATH}`, {
    headers: { authorization: `Bearer ${CONTROL_TOKEN}` },
  });
  const result = worker.fetch(request, env, {} as ExecutionContext);
  await called;
  assert.equal(calls, 1);
  t.mock.timers.tick(10_000);
  const response = await result;
  assert.equal(response.status, 503);
  assert.equal(await response.text(), "staging_status_unknown");
  assert.equal(response.headers.get("content-type")?.includes("application/json"), false);
});

test("bounded staging: deployment identity is CF_VERSION_METADATA's Worker version UUID", async (t) => {
  const versionId = "11111111-1111-4111-8111-111111111111";
  const deploymentResourceId = "22222222-2222-4222-8222-222222222222";
  const env = { ...BOUNDED_STAGING_ENV, CF_VERSION_METADATA: { id: versionId } };
  const h = await harness(t, env, { owner: { ...ACTIVE_OWNER, deploymentId: versionId } });
  const ingress = { ...env, TWO_BOT: { getByName: () => h.bot } } as unknown as Env;
  const window = stagingWindow();
  const acquire = (deploymentId: string) => new Request(`https://worker.invalid${STAGING_USE_PATH}`, {
    method: "POST", headers: { authorization: `Bearer ${CONTROL_TOKEN}`, [DEPLOYMENT_HEADER]: deploymentResourceId },
    body: JSON.stringify({ ...window, action: "acquire", expectedEpoch: 1,
      deploymentId, sha: env.CONTAINER_RELEASE }),
  });
  const refused = await worker.fetch(acquire(deploymentResourceId), ingress, {} as ExecutionContext);
  assert.equal(refused.status, 503);
  assert.equal(h.values.has(STAGING_USE_KEY), false);
  assert.equal(h.starts.length, 0);
  const accepted = await worker.fetch(acquire(versionId), ingress, {} as ExecutionContext);
  assert.equal(accepted.status, 200, "Worker overwrites the caller header with its own version UUID");
  const state = await accepted.json();
  assert.equal(state.deploymentId, versionId);
  assert.equal(state.owner.deploymentId, versionId);
  assert.equal(state.use.deploymentId, versionId);
  assert.equal((h.values.get(STAGING_USE_KEY) as { deploymentId: string }).deploymentId, versionId);
  assert.equal(h.starts.length, 0, "acquisition is not startup");
});

test("bounded staging: offline pipeline ownership and standalone behavior", () => {
  const fixture = fileURLToPath(new URL("../../scripts/ci/staging_admission_behavior.mjs", import.meta.url));
  execFileSync(process.execPath, [fixture], { timeout: 10_000 });
});

async function assertNativeExpiryStopped(h: {
  runtime: { running: boolean }; pending: () => unknown[];
  bot: { listSchedules: (name: string) => Promise<unknown[]> };
}) {
  assert.equal(h.runtime.running, false, "expiry stopped runtime");
  assert.equal(h.pending().length, 0, "expiry drained keepalive");
  assert.equal((await h.bot.listSchedules("stagingExpiry")).length, 0, "expiry drained alarm");
}

test("bounded staging: native expiry stops a lost runner's container, consumes schedules, and never resurrects", async (t) => {
  t.mock.timers.enable({ apis: ["Date", "setTimeout"], now: 1_800_000_000_000 });
  const h = await harness(t, BOUNDED_STAGING_ENV);
  const window = stagingWindow();
  assert.equal((await h.bot.fetch(stagingUseRequest("acquire", window))).status, 200);
  const response = await h.bot.fetch(probeRequest("https://worker.invalid/health"));
  await response.arrayBuffer();
  const [oldTick] = await h.bot.listSchedules("keepalive");
  assert.ok(oldTick);
  t.mock.timers.tick(120_000);
  await h.bot.alarm();
  await assertNativeExpiryStopped(h);
  const receipt = h.logs.filter((line) => line.startsWith("{"))
    .map((line) => JSON.parse(line)).find((event) => event.event === "staging_use" && event.action === "expiry");
  assert.ok(receipt, "a lost/cancelled runner still gets a native stop receipt");
  assert.equal(receipt.outcome, "stopped");
  assert.equal(receipt.running, false);
  assert.equal(receipt.run_id, window.runId);
  assert.equal(receipt.run_attempt, window.runAttempt);
  assert.equal(receipt.expires_at_ms, window.expiresAt);
  const before = h.requests.length;
  await h.bot.keepalive(oldTick.payload as typeof KEEPALIVE, oldTick);
  await h.bot.stagingExpiry();
  assert.equal(h.requests.length, before);
  assert.equal(h.starts.length, 1);
  assert.equal((await h.bot.fetch(probeRequest("https://worker.invalid/health"))).status, 503);
  assert.equal(h.starts.length, 1);
});

test("bounded staging: expiry assertions detect running, pending, and armed corruptions", async () => {
  const state = { runtime: { running: false }, pending: (): unknown[] => [],
    bot: { listSchedules: async (): Promise<unknown[]> => [] } };
  await assertNativeExpiryStopped(state);
  state.runtime.running = true;
  await assert.rejects(assertNativeExpiryStopped(state), /expiry stopped runtime/);
  state.runtime.running = false;
  state.pending = () => [1];
  await assert.rejects(assertNativeExpiryStopped(state), /expiry drained keepalive/);
  state.pending = () => [];
  state.bot.listSchedules = async () => [1];
  await assert.rejects(assertNativeExpiryStopped(state), /expiry drained alarm/);
});

test("bounded staging: DO reconstruction after expiry stops a reattached process before ingress", async (t) => {
  t.mock.timers.enable({ apis: ["Date", "setTimeout"], now: 1_800_000_000_000 });
  const h = await harness(t, BOUNDED_STAGING_ENV);
  assert.equal((await h.bot.fetch(stagingUseRequest("acquire", stagingWindow()))).status, 200);
  t.mock.timers.tick(120_001);
  const reconstructed = await harness(t, BOUNDED_STAGING_ENV, { values: h.values, running: true }, h.database);
  assert.equal(reconstructed.runtime.running, false);
  assert.equal(reconstructed.runtime.destroys, 1);
  assert.equal((await reconstructed.bot.fetch(probeRequest("https://worker.invalid/health"))).status, 503);
  assert.equal(reconstructed.starts.length, 0);
  await reconstructed.bot.stagingExpiry();
  assert.equal((await reconstructed.bot.listSchedules("stagingExpiry")).length, 0);
});

test("bounded staging: first activation drains a running predecessor and its ungranted legacy keepalive", async (t) => {
  t.mock.timers.enable({ apis: ["Date", "setTimeout"], now: 1_800_000_000_000 });
  const predecessor = await harness(t, WORKER_ENV, { running: true });
  await predecessor.bot.onStart();
  const [oldTick] = await predecessor.bot.listSchedules<typeof KEEPALIVE>("keepalive");
  assert.ok(oldTick);
  for (const key of ["runId", "runAttempt", "expiresAt"]) {
    assert.equal(key in oldTick.payload!, false, "legacy rows carry no finite-use identity");
  }
  assert.equal(predecessor.runtime.running, true);
  assert.equal(predecessor.values.has(STAGING_USE_KEY), false, "no grant has ever been stored");

  const activated = await harness(t, BOUNDED_STAGING_ENV,
    { values: predecessor.values, running: true }, predecessor.database);
  assert.equal(activated.runtime.running, false, "construction drains before ingress or alarm delivery");
  assert.equal(activated.runtime.destroys, 1);
  assert.equal(activated.pending().length, 0, "construction deletes persisted legacy rows");
  assert.equal((await activated.bot.listSchedules("stagingExpiry")).length, 0);
  assert.equal(activated.starts.length, 0);
  assert.equal(activated.requests.length, 0);

  await activated.bot.keepalive(oldTick.payload!, oldTick);
  t.mock.timers.tick(60_000);
  await activated.bot.alarm();
  assert.equal(activated.runtime.running, false);
  assert.equal(activated.pending().length, 0, "a queued legacy callback cannot rearm");
  assert.equal((await activated.bot.listSchedules("stagingExpiry")).length, 0);
  assert.equal(activated.values.has(STAGING_USE_KEY), false);
  assert.equal(activated.starts.length, 0, "a queued legacy callback cannot restart");
  assert.equal(activated.requests.length, 0, "a queued legacy callback cannot probe");
});

test("bounded staging: cleanup from an older run cannot stop the new consumer", async (t) => {
  const h = await harness(t, BOUNDED_STAGING_ENV);
  const first = stagingWindow();
  assert.equal((await h.bot.fetch(stagingUseRequest("acquire", first))).status, 200);
  assert.equal((await h.bot.fetch(stagingUseRequest("release", first))).status, 200);
  const next = { ...first, runId: "999999" };
  assert.equal((await h.bot.fetch(stagingUseRequest("acquire", next))).status, 200);
  const response = await h.bot.fetch(probeRequest("https://worker.invalid/health"));
  await response.arrayBuffer();
  assert.equal((await h.bot.fetch(stagingUseRequest("release", first))).status, 503);
  assert.equal(h.runtime.running, true);
  await h.bot.stagingExpiry();
  assert.equal(h.runtime.running, true, "an early/stale callback is not stop authority");
});

test("bounded staging: startup that crosses expiry cannot leave an unauthorized process running", async (t) => {
  t.mock.timers.enable({ apis: ["Date", "setTimeout"], now: 1_800_000_000_000 });
  const h = await harness(t, BOUNDED_STAGING_ENV);
  assert.equal((await h.bot.fetch(stagingUseRequest("acquire", stagingWindow()))).status, 200);
  const start = h.runtime.start.bind(h.runtime);
  t.mock.method(h.runtime, "start", (config: StartConfig) => {
    start(config);
    t.mock.timers.tick(120_000);
  });
  await assert.rejects(h.bot.start(), /staging_use_required/);
  assert.equal(h.runtime.running, false);
  assert.equal(h.pending().length, 0);
});

test("bounded staging: unreadable authority in expiry stops rather than sustaining an idle alarm loop", async (t) => {
  const h = await harness(t, BOUNDED_STAGING_ENV);
  assert.equal((await h.bot.fetch(stagingUseRequest("acquire", stagingWindow()))).status, 200);
  const response = await h.bot.fetch(probeRequest("https://worker.invalid/health"));
  await response.arrayBuffer();
  const get = h.ctx.storage.get;
  t.mock.method(h.ctx.storage, "get", async (key: string) => {
    if (key === STAGING_USE_KEY) throw new Error("synthetic storage failure");
    return get(key);
  });
  await h.bot.stagingExpiry();
  assert.equal(h.runtime.running, false);
  assert.equal(h.pending().length, 0);
  assert.equal((await h.bot.listSchedules("stagingExpiry")).length, 0);
});

test("bounded staging: unreadable AutoMod journal preserves native stop and retry", async (t) => {
  t.mock.timers.enable({ apis: ["Date"], now: 1_800_000_000_000 });
  const journalKey = "two-bot:staging-automod-recovery:v1";
  const reconstructed = await harness(t, BOUNDED_STAGING_ENV, { running: true, unreadableKey: journalKey });
  assert.equal(reconstructed.runtime.running, false, "reconstruction must stop an unconfirmed process");

  const h = await harness(t, BOUNDED_STAGING_ENV);
  const window = stagingWindow();
  assert.equal((await h.bot.fetch(stagingUseRequest("acquire", window))).status, 200);
  const get = h.ctx.storage.get;
  t.mock.method(h.ctx.storage, "get", async (key: string) => {
    if (key === journalKey) throw new Error("synthetic journal read failure");
    return get(key);
  });
  h.runtime.running = true;
  await h.bot.onActivityExpired();
  assert.equal(h.runtime.running, false, "native inactivity still stops without storage");

  h.runtime.running = true;
  await assert.rejects(h.bot.onStart(), /staging_automod_storage_unavailable/);
  assert.equal(h.runtime.running, false, "failed startup still stops without storage");

  const destroy = h.runtime.destroy.bind(h.runtime);
  let failStop = true;
  t.mock.method(h.runtime, "destroy", async () => {
    if (failStop) throw new Error("synthetic native stop failure");
    await destroy();
  });
  h.runtime.running = true;
  t.mock.timers.tick(window.expiresAt - Date.now());
  await h.bot.stagingExpiry();
  const [retry] = await h.bot.listSchedules("stagingExpiry");
  assert.equal(retry?.time, Math.ceil((Date.now() + 15_000) / 1000));
  assert.equal(h.runtime.running, true);
  failStop = false;
  await h.bot.stagingExpiry();
  assert.equal(h.runtime.running, false);
  assert.deepEqual(await h.bot.listSchedules("stagingExpiry"), []);
});

test("bounded staging: preparing AutoMod reservation retries native stop", async (t) => {
  t.mock.timers.enable({ apis: ["Date"], now: 1_800_000_000_000 });
  const use: StagingUse = { ...stagingWindow(), deploymentId: ID, sha: BOUNDED_STAGING_ENV.CONTAINER_RELEASE,
    epoch: ACTIVE_OWNER.epoch, acquiredAt: Date.now(), phase: "preparing", purpose: AUTOMOD_STAGING_PURPOSE };
  const values = new Map<string, unknown>([[OWNER_KEY, ACTIVE_OWNER], [STAGING_USE_KEY, use]]);
  // A reconstruction can find the durable reservation after the old process's stop failed.
  const h = await harness(t, BOUNDED_STAGING_ENV, { running: true, stopError: true, values });
  assert.equal(h.runtime.destroys, 1);
  assert.equal(h.runtime.running, true);
  const [retry] = await h.bot.listSchedules("stagingExpiry");
  assert.equal(retry?.time, Math.ceil((Date.now() + 15_000) / 1000));
  await assert.rejects(h.bot.start(), /shutdown_unconfirmed/);

  t.mock.method(h.runtime, "destroy", async () => { h.runtime.running = false; });
  await h.bot.stagingExpiry();
  assert.equal(h.runtime.running, false);
  assert.deepEqual(values.get(STAGING_USE_KEY), use);
  await assert.rejects(h.bot.start(), /staging_use_conflict/);
  h.runtime.running = true;
  await h.bot.onActivityExpired();
  assert.equal(h.runtime.running, false);

  t.mock.timers.setTime(use.expiresAt);
  await h.bot.stagingExpiry();
  assert.deepEqual(values.get(STAGING_USE_KEY), { ...use, phase: "released" });
});

test("bounded staging: uncertain reservation permits only scoped emergency stop", async (t) => {
  t.mock.timers.enable({ apis: ["Date"], now: 1_800_000_000_000 });
  const h = await harness(t, BOUNDED_STAGING_ENV);
  const window = stagingWindow();
  assert.equal((await h.bot.fetch(stagingUseRequest("acquire", window))).status, 200);
  const lease = h.values.get(STAGING_USE_KEY), get = h.ctx.storage.get;
  const journalKey = "two-bot:staging-automod-recovery:v1";
  let unreadable = journalKey;
  t.mock.method(h.ctx.storage, "get", async (key: string) => {
    if (key === unreadable) throw new Error("synthetic authority read failure");
    return get(key);
  });
  const fence = { action: "fence", expectedEpoch: ACTIVE_OWNER.epoch, actor: "fixture-operator" };
  h.runtime.running = true;
  assert.equal((await h.bot.fetch(stagingUseRequest("release", { ...window, runAttempt: window.runAttempt + 1 }))).status, 503);
  assert.equal((await h.bot.fetch(controlRequest({ ...fence, expectedEpoch: fence.expectedEpoch + 1 }))).status, 409);
  assert.equal(h.runtime.running, true);
  for (const request of [stagingUseRequest("stop", window), stagingUseRequest("release", window), controlRequest(fence)]) {
    h.runtime.running = true;
    assert.equal((await h.bot.fetch(request)).status, 503);
    assert.equal(h.runtime.running, false);
  }
  assert.deepEqual(h.values.get(STAGING_USE_KEY), lease);
  assert.deepEqual(h.values.get(OWNER_KEY), ACTIVE_OWNER);
  for (unreadable of [journalKey, STAGING_USE_KEY]) {
    h.runtime.running = true;
    await assert.rejects(h.bot.start(), /storage_unavailable/);
    assert.equal(h.runtime.running, false);
  }

  unreadable = journalKey;
  h.runtime.running = true;
  t.mock.method(h.runtime, "destroy", async () => { throw new Error("synthetic stop failure"); });
  assert.equal((await h.bot.fetch(stagingUseRequest("release", window))).status, 503);
  const [retry] = await h.bot.listSchedules("stagingExpiry");
  assert.equal(retry?.time, Math.ceil((Date.now() + 15_000) / 1000));
  const rebuilt = await harness(t, BOUNDED_STAGING_ENV, { values: h.values, running: true }, h.database);
  const [retained] = await rebuilt.bot.listSchedules("stagingExpiry");
  assert.equal(retained?.time, retry?.time, "reconstruction must preserve the prompt stop retry");
  await assert.rejects(rebuilt.bot.start(), /shutdown_unconfirmed/);
  assert.equal((await rebuilt.bot.fetch(stagingUseRequest("acquire", window))).status, 503);
  t.mock.timers.tick(15_000);
  await rebuilt.bot.stagingExpiry();
  assert.equal(rebuilt.runtime.running, false);
});

test("bounded staging: failed control teardown persists a prompt stop-only retry", async (t) => {
  t.mock.timers.enable({ apis: ["Date", "setTimeout"], now: 1_800_000_000_000 });
  for (const action of ["acquire", "release"] as const) {
    const h = await harness(t, BOUNDED_STAGING_ENV);
    const window = { ...stagingWindow(), expiresAt: Date.now() + 6 * 60 * 60 * 1000 };
    if (action === "release") {
      assert.equal((await h.bot.fetch(stagingUseRequest("acquire", window))).status, 200);
    }
    h.runtime.running = true;
    const destroy = h.runtime.destroy.bind(h.runtime);
    let failing = true;
    t.mock.method(h.runtime, "destroy", async () => {
      if (failing) throw new Error("synthetic provider stop failure");
      await destroy();
    });
    assert.equal((await h.bot.fetch(stagingUseRequest(action, window))).status, 503);
    assert.equal((h.values.get(STAGING_USE_KEY) as { phase: string }).phase,
      action === "acquire" ? "preparing" : "released");
    assert.equal(h.runtime.running, true, "unconfirmed shutdown is not idle");
    const [retry] = await h.bot.listSchedules("stagingExpiry");
    assert.ok(retry, `${action} failure must persist a stop retry before returning`);
    assert.equal(retry.time, Math.ceil((Date.now() + 15_000) / 1000));
    await assert.rejects(h.bot.start(), /shutdown_unconfirmed/);
    t.mock.timers.tick(15_000);
    await fireSdkAlarm(t, h.bot, 15_000);
    assert.equal((await h.bot.listSchedules("stagingExpiry")).length, 1,
      "a failed retry must persist its successor, not wait for the six-hour grant");
    failing = false;
    t.mock.timers.tick(15_000);
    await h.bot.alarm();
    assert.equal(h.runtime.running, false);
    assert.equal((await h.bot.listSchedules("stagingExpiry")).length, 0);
    assert.equal(h.starts.length, 0, "stop-only retries never start a process");
    assert.equal(h.requests.length, 0);
  }
});

test("bounded staging: unconfirmed expiry teardown persists a stop-only successor until confirmed", async (t) => {
  t.mock.timers.enable({ apis: ["Date", "setTimeout"], now: 1_800_000_000_000 });
  const h = await harness(t, BOUNDED_STAGING_ENV);
  assert.equal((await h.bot.fetch(stagingUseRequest("acquire", stagingWindow()))).status, 200);
  const response = await h.bot.fetch(probeRequest("https://worker.invalid/health"));
  await response.arrayBuffer();
  const destroy = h.runtime.destroy.bind(h.runtime);
  let failing = true;
  t.mock.method(h.runtime, "destroy", async () => {
    if (failing) throw new Error("synthetic provider stop failure");
    await destroy();
  });
  t.mock.timers.tick(120_000);
  await fireSdkAlarm(t, h.bot, 15_000);
  assert.equal(h.pending().length, 0);
  assert.equal((await h.bot.listSchedules("stagingExpiry")).length, 1, "SDK consuming the failed row must not lose stop retry");
  const failed = h.logs.filter((line) => line.startsWith("{"))
    .map((line) => JSON.parse(line)).find((event) => event.event === "staging_use" && event.outcome === "stop_unconfirmed");
  assert.ok(failed, "provider stop failure is not a successful cleanup receipt");
  assert.equal(failed.action, "expiry");
  assert.equal(failed.running, true);
  assert.equal(failed.run_id, "123456");
  await assert.rejects(h.bot.start(), /shutdown_unconfirmed/);
  failing = false;
  t.mock.timers.tick(15_000);
  await h.bot.alarm();
  assert.equal(h.runtime.running, false);
  assert.equal((await h.bot.listSchedules("stagingExpiry")).length, 0);
  assert.equal(h.starts.length, 1);
});

test("staging self-move stamps only its selected lease and reads OFF after release or interruption", async (t) => {
  t.mock.timers.enable({ apis: ["Date", "setTimeout"], now: 1_800_000_000_000 });
  const env = { ...BOUNDED_STAGING_ENV, INTERNAL_ACTIONS_INGRESS: "1",
    TWO_INTERNAL_ACTIONS: "1", TWO_INTERNAL_ALLOW_VOICE_MOVE: "1" };
  const inert = await harness(t, { ...env, TWO_INTERNAL_ALLOW_VOICE_MOVE: "0" });
  const h = await harness(t, env);
  const window = stagingWindow();
  const off = () => h.bot.fetch(probeRequest(`https://worker.invalid${STAGING_USE_PATH}`,
    { headers: { authorization: `Bearer ${CONTROL_TOKEN}` } })).then((r) => r.json());
  const selected = { ...window, voiceMove: true as const };
  const body = '{"action":"voice.move_member","discord_id":"111","channel_id":"222"}';
  const delivered: (string | null)[] = [];
  t.mock.method(h.bot, "containerFetch", async (request: Request) => {
    assert.equal(await request.text(), body);
    delivered.push(request.headers.get(VOICE_MOVE_LEASE_HEADER));
    return Response.json({ ok: true, request_id: request.headers.get("x-request-id"), result: {} });
  });
  const send = () => h.bot.fetch(probeRequest("https://worker.invalid/internal/actions", { method: "POST", body,
    headers: { "content-type": "application/json", "x-two-key-id": "fixture",
      "x-two-timestamp": "1800000000", "x-two-nonce": "fixture",
      "x-two-signature": "fixture", [VOICE_MOVE_LEASE_HEADER]: String(window.expiresAt) } }));
  const refusal = await inert.bot.fetch(stagingUseRequest("acquire", selected));
  assert.deepEqual([refusal.status, (await refusal.json()).reason, inert.values.has(STAGING_USE_KEY)], [503, "staging_use_required", false]);
  assert.equal((await h.bot.fetch(stagingUseRequest("acquire", window))).status, 200);
  assert.equal((await off()).voiceMoveEnabled, false);
  const conflict = await h.bot.fetch(stagingUseRequest("acquire", selected));
  assert.equal((await conflict.json()).reason, "staging_use_conflict");
  h.runtime.running = true;
  assert.equal((await send()).status, 200);
  assert.equal((await h.bot.fetch(stagingUseRequest("release", window))).status, 200);
  const opted = { ...selected, runId: "123457" };
  assert.equal((await h.bot.fetch(stagingUseRequest("acquire", opted))).status, 200);
  assert.equal((await off()).voiceMoveEnabled, true);
  h.runtime.running = true;
  t.mock.timers.setTime(window.expiresAt - 1_100);
  assert.equal((await send()).status, 200);
  t.mock.timers.setTime(window.expiresAt - 999);
  assert.equal((await send()).status, 200);
  assert.deepEqual(delivered, [null, String(window.expiresAt - 1_000), null]);
  const failure = t.mock.method(h.runtime, "destroy", async () => { throw new Error("stop failed"); });
  assert.equal((await h.bot.fetch(stagingUseRequest("release", { ...window, runId: opted.runId }))).status, 503);
  const afterFailure = await off();
  assert.deepEqual([afterFailure.use.phase, afterFailure.voiceMoveEnabled, afterFailure.running], ["released", false, true]);
  assert.equal((await send()).status, 503);
  failure.mock.restore();
  await h.bot.stagingExpiry();
  assert.equal((await off()).running, false);

  const interrupted = { ...stagingWindow(), voiceMove: true as const, runId: "123458" };
  assert.equal((await h.bot.fetch(stagingUseRequest("acquire", interrupted))).status, 200);
  h.runtime.running = true;
  t.mock.timers.setTime(interrupted.expiresAt);
  assert.partialDeepStrictEqual(await off(), { voiceMoveEnabled: false, running: true });
  await h.bot.stagingExpiry();
  assert.partialDeepStrictEqual(await off(), { voiceMoveEnabled: false, running: false });
  assert.ok(h.logs.some((line) => line.includes('"action":"expiry","outcome":"stopped"')
    && line.includes('"voice_move_enabled":false') && line.includes('"run_id":"123458"')));
});

for (const env of [WORKER_ENV, { ...WORKER_ENV, TWO_DEPLOY_ENV: "production", STAGING_RUNTIME_POLICY: "bounded" }]) {
  test(`staging-use route is dark outside bounded staging (${env.TWO_DEPLOY_ENV ?? "unmarked"})`, async (t) => {
    const h = await harness(t, env);
    assert.equal((await h.bot.fetch(stagingUseRequest("acquire", stagingWindow()))).status, 404);
    const response = await h.bot.fetch(probeRequest("https://worker.invalid/health"));
    assert.equal(response.status, 200, "production auto-start behavior is unchanged");
    await response.arrayBuffer();
    assert.equal(h.starts.length, 1);
  });
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

test("exact takeover refuses a routed version mismatch before changing ownership", async (t) => {
  const h = await harness(t);
  const status = await h.bot.fetch(controlRequest());
  assert.equal(status.headers.get("x-two-bot-exact-takeover"), "1");
  const fence = await h.bot.fetch(controlRequest({ action: "fence", expectedEpoch: 1, actor: "fixture-operator" }));
  assert.equal(fence.status, 200);
  const before = h.values.get(OWNER_KEY);
  const change = { action: "takeover-exact", expectedEpoch: 2, actor: "fixture-recovery" };
  const wrong = await h.bot.fetch(controlRequest({ ...change, expectedDeploymentId: "deployment-other" }));
  assert.equal(wrong.status, 503);
  assert.equal((await wrong.json()).reason, "deployment_mismatch");
  assert.deepEqual(h.values.get(OWNER_KEY), before);
  const matched = await h.bot.fetch(controlRequest({ ...change, expectedDeploymentId: ID }));
  assert.equal(matched.status, 200);
  const state = await matched.json();
  assert.equal(state.owner.deploymentId, ID);
  assert.equal(state.owner.epoch, 3);
  assert.equal(state.running, false);
  assert.equal(h.starts.length, 0);
});

test("exact takeover refuses a pending fence without changing ownership", async (t) => {
  const pending = { ...ACTIVE_OWNER, phase: "fenced", epoch: 2, oldEpoch: 1 };
  const h = await harness(t, WORKER_ENV, { owner: pending });
  const response = await h.bot.fetch(controlRequest({
    action: "takeover-exact", expectedDeploymentId: ID, expectedEpoch: pending.epoch, actor: "fixture-recovery",
  }));
  assert.equal(response.status, 409);
  assert.equal((await response.json()).reason, "epoch_conflict");
  assert.deepEqual(h.values.get(OWNER_KEY), pending);
  assert.equal(h.values.has(`${AUDIT_PREFIX}3:fenced`), false);
  assert.equal(h.values.has(`${AUDIT_PREFIX}3:active`), false);
  assert.equal(h.starts.length, 0);
});

test("exact schema fence refuses a routed version mismatch and confirms a matching stop", async (t) => {
  const h = await harness(t);
  const before = h.values.get(OWNER_KEY);
  const change = { action: "fence-exact", expectedEpoch: 1, actor: "fixture-release" };
  const wrong = await h.bot.fetch(controlRequest({ ...change, expectedDeploymentId: "deployment-other" }));
  assert.equal(wrong.status, 503);
  assert.equal((await wrong.json()).reason, "deployment_mismatch");
  assert.deepEqual(h.values.get(OWNER_KEY), before);
  const matched = await h.bot.fetch(controlRequest({ ...change, expectedDeploymentId: ID }));
  assert.equal(matched.status, 200);
  const state = await matched.json();
  assert.partialDeepStrictEqual(state, { owner: { deploymentId: null, epoch: 2 }, running: false });
});

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

test("PRE predecessor preserves readiness build fields through ordinary ownership and boot", async (t) => {
  const outbound = t.mock.method(globalThis, "fetch", () => {
    throw new Error("unexpected outbound fetch during the PRE boot fixture");
  });
  const manifest = {
    workerVersion: "fixture-predecessor-version",
    rustBuild: { build_revision: "a".repeat(40), build_id: "123-1" },
  };
  const env = { ...WORKER_ENV, CF_VERSION_METADATA: { id: manifest.workerVersion } };
  const h = await harness(t, env);
  const send = (request: Request) => worker.fetch(request, {
    ...env, TWO_BOT: { getByName: () => h.bot },
  } as unknown as Env, {} as ExecutionContext);
  const readyz = () => send(new Request("https://worker.invalid/readyz"));

  const capability = await send(controlRequest());
  assert.equal(capability.status, 200);
  assert.equal(capability.headers.get("x-two-bot-exact-takeover"), "1");
  assert.deepEqual(await capability.json(), {
    deploymentId: manifest.workerVersion, owner: ACTIVE_OWNER, running: false,
  });
  const parked = await readyz();
  assert.equal(parked.status, 503);
  assert.equal((await parked.json()).reason, "not_owner");
  assert.equal(h.starts.length, 0);
  assert.deepEqual(h.values.get(OWNER_KEY), ACTIVE_OWNER);

  const takeover = await send(controlRequest({ action: "takeover", expectedEpoch: ACTIVE_OWNER.epoch,
    actor: "fixture-ordinary-rollout" }));
  assert.equal(takeover.status, 200);
  const state = await takeover.json();
  assert.equal(state.running, false);
  assert.equal(state.owner.deploymentId, manifest.workerVersion);
  assert.equal(state.owner.phase, "active");
  assert.equal(state.owner.epoch, ACTIVE_OWNER.epoch + 1);
  assert.equal(h.starts.length, 0);

  const report = { ready: true, components: [["process", "ready"], ["gateway", "ready"], ["database", "ready"]],
    jobs: {}, ...manifest.rustBuild };
  h.setProbeStatus(200);
  h.setProbeResponse((status) =>
    Response.json(h.requests.at(-1)?.pathname === "/readyz" ? report : { status: "ok" }, { status }));
  for (let probe = 0; probe < 2; probe++) {
    const response = await readyz();
    assert.equal(response.status, 200);
    assert.equal(response.headers.get("x-two-worker-version"), manifest.workerVersion);
    assert.deepEqual(await response.json(), report);
  }
  assert.equal(h.requests.filter(({ pathname }) => pathname === "/readyz").length, 2);
  assert.equal(h.starts.length, 1);
  assert.deepEqual(h.starts[0]?.env, EXPECTED_ENV);
  assert.equal(h.runtime.destroys, 0);
  assert.deepEqual(h.values.get(OWNER_KEY), state.owner);
  assert.deepEqual([...h.values.keys()].filter((key) => key.startsWith(AUDIT_PREFIX)).sort(), [
    `${AUDIT_PREFIX}2:active`, `${AUDIT_PREFIX}2:fenced`,
  ]);
  assert.equal(outbound.mock.callCount(), 0);
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

const BRIDGE_VERSION = "11111111-1111-1111-1111-111111111111";
const BRIDGE_ENV = { ...BOUNDED_STAGING_ENV, ...automodPublication.env, CF_VERSION_METADATA: { id: BRIDGE_VERSION } };
const BRIDGE_JWKS_URL = "https://token.actions.githubusercontent.com/.well-known/jwks";
const BRIDGE_KID = "synthetic-actions-key";
const BRIDGE_KEY = generateKeyPairSync("rsa", { modulusLength: 2048 });
const OTHER_KEY = generateKeyPairSync("rsa", { modulusLength: 2048 });
const BRIDGE_CLAIMS = {
  iss: "https://token.actions.githubusercontent.com", aud: "two-bot-staging-automod",
  repository: "TogetherWeOwn/two-bot-next", repository_id: "1396224242", ref: "refs/heads/main", ref_protected: "true",
  event_name: "workflow_dispatch", run_id: "123456", run_attempt: "1", environment: "staging",
  workflow_ref: "TogetherWeOwn/two-bot-next/.github/workflows/staging-automod-preview.yml@refs/heads/main",
  sha: BOUNDED_STAGING_ENV.CONTAINER_RELEASE,
};

function actionsToken(overrides: Record<string, unknown> = {}, key = BRIDGE_KEY.privateKey): string {
  const now = Math.floor(Date.now() / 1_000);
  const encode = (value: unknown) => Buffer.from(JSON.stringify(value)).toString("base64url");
  const signed = `${encode({ alg: "RS256", typ: "JWT", kid: BRIDGE_KID })}.${encode({
    ...BRIDGE_CLAIMS, iat: now, nbf: now, exp: now + 300, ...overrides })}`;
  return `${signed}.${sign("sha256", Buffer.from(signed), key).toString("base64url")}`;
}

async function bridgeHarness(t: TestContext, env: Partial<Env> = BRIDGE_ENV, version = BRIDGE_VERSION) {
  t.mock.timers.enable({ apis: ["Date"], now: 1_800_000_000_000 });
  const h = await harness(t, env, { owner: { ...ACTIVE_OWNER, deploymentId: version } });
  const jwk = { ...BRIDGE_KEY.publicKey.export({ format: "jwk" }), kid: BRIDGE_KID, alg: "RS256", use: "sig" };
  const jwks = t.mock.method(globalThis, "fetch", async (input: string | URL | Request, init?: RequestInit) => {
    assert.equal(String(input instanceof Request ? input.url : input), BRIDGE_JWKS_URL);
    assert.equal(init?.redirect, "error");
    return Response.json({ keys: [jwk] });
  });
  const reads = { get: t.mock.method(h.ctx.storage, "get"), put: t.mock.method(h.ctx.storage, "put"),
    delete: t.mock.method(h.ctx.storage, "delete") };
  const storageCalls = () => reads.get.mock.callCount() + reads.put.mock.callCount() + reads.delete.mock.callCount();
  const planReads: string[] = [];
  const setPlan = (plan: StagingAutomodPlan) => {
    (h.bot as unknown as { automodPlan: StagingAutomodPlan }).automodPlan = new Proxy(plan, {
      get: (target, key, receiver) => { planReads.push(String(key)); return Reflect.get(target, key, receiver); },
    });
  };
  const enabledPlan: StagingAutomodPlan = { ...STAGING_AUTOMOD_PLAN, enabled: true, allowStart: true,
    notAfter: Date.now() + 600_000 };
  const pin = { workerVersionId: BRIDGE_VERSION, sourceSha: BRIDGE_ENV.CONTAINER_RELEASE, namespaceId: "a".repeat(32),
    workerDeploymentId: "22222222-2222-2222-2222-222222222222", applicationId: "33333333-3333-3333-3333-333333333333",
    rolloutId: "44444444-4444-4444-4444-444444444444", imageDigest: `sha256:${"b".repeat(64)}` };
  const change = { ...stagingWindow(), action: "acquire" as const, deploymentId: BRIDGE_VERSION, sha: pin.sourceSha,
    expectedEpoch: ACTIVE_OWNER.epoch };
  const context = { environment: "staging", deploymentId: BRIDGE_VERSION, sha: pin.sourceSha, runId: change.runId,
    runAttempt: 1, target: STAGING_AUTOMOD_TARGET };
  const request = (body: unknown, options: { token?: string | null; bearer?: string; deployment?: string;
    signal?: AbortSignal; method?: string } = {}) => {
    const headers = new Headers({ "content-type": "application/json", [DEPLOYMENT_HEADER]: options.deployment ?? version });
    if (options.bearer !== "") headers.set("authorization", `Bearer ${options.bearer ?? CONTROL_TOKEN}`);
    const token = options.token === undefined ? actionsToken() : options.token;
    if (token !== null) headers.set(STAGING_AUTOMOD_OIDC_HEADER, token);
    const method = options.method ?? "POST";
    return new Request(`https://worker.invalid${STAGING_AUTOMOD_PATH}`, { method, headers,
      ...(method === "POST" ? { body: typeof body === "string" ? body : JSON.stringify(body) } : {}),
      ...(options.signal ? { signal: options.signal } : {}) });
  };
  const send = async (body: unknown, options: Parameters<typeof request>[1] = {}) => {
    const response = await h.bot.fetch(request(body, options));
    return { status: response.status, body: await response.json().catch(() => null) as { value?: unknown } | null };
  };
  return { ...h, jwks, storageCalls, planReads, setPlan, enabledPlan, pin, change, context, request, send };
}

const BRIDGE_REFUSED = { status: 403, body: { error: "staging_automod_refused" } };

test("AutoMod bridge: the Worker route needs bounded staging, POST and the control bearer", async t => {
  const h = await bridgeHarness(t);
  h.setPlan(h.enabledPlan);
  let routed = 0;
  const edge = (env: Partial<Env> = BRIDGE_ENV) => ({ ...env, TWO_BOT: { getByName: () => {
    routed++;
    return h.bot;
  } } }) as unknown as Env;
  const body = { op: "read-lease", context: h.context };
  for (const env of [{ ...BRIDGE_ENV, TWO_DEPLOY_ENV: "production" }, { ...BRIDGE_ENV, STAGING_RUNTIME_POLICY: "standard" }]) {
    assert.equal((await worker.fetch(h.request(body), edge(env))).status, 404);
  }
  assert.equal((await worker.fetch(h.request(body, { method: "GET" }), edge())).status, 404);
  assert.equal((await worker.fetch(h.request(body, { bearer: "" }), edge())).status, 401);
  assert.equal((await worker.fetch(h.request(body, { bearer: "x".repeat(43) }), edge())).status, 401);
  assert.equal(routed, 0);
  assert.equal(h.jwks.mock.callCount(), 0);

  const forged = h.request(body, { deployment: "forged-deployment" });
  const routedResponse = await worker.fetch(forged, edge());
  assert.equal(routedResponse.status, 200);
  assert.deepEqual(await routedResponse.json(), { value: null });
  assert.deepEqual(await h.send(body, { deployment: "forged-deployment" }), BRIDGE_REFUSED);
  assert.equal(routed, 1);
  assert.equal((await h.send(body, { bearer: "" })).status, 401);
  assert.equal((await h.send(body, { bearer: "x".repeat(43) })).status, 401);
  assert.equal((await h.send(body, { method: "GET" })).status, 404);
  assert.equal(h.jwks.mock.callCount(), 1);
});

test("AutoMod bridge: every unverified Actions token is refused before storage, plan or Container", async t => {
  const h = await bridgeHarness(t);
  h.setPlan(h.enabledPlan);
  const body = { op: "acquire", pin: h.pin, change: h.change };
  const now = Math.floor(Date.now() / 1_000);
  const cases: [string, string | null][] = [
    ["missing", null],
    ["malformed", "not-a-jwt"],
    ["bad signature", actionsToken({}, OTHER_KEY.privateKey)],
    ["wrong audience", actionsToken({ aud: "sts.amazonaws.com" })],
    ["second attempt", actionsToken({ run_attempt: "2" })],
    ["non-main ref", actionsToken({ ref: "refs/heads/feature" })],
    ["unprotected ref", actionsToken({ ref_protected: "false" })],
    ["other workflow", actionsToken({ workflow_ref: BRIDGE_CLAIMS.workflow_ref.replace("staging-automod-preview", "other") })],
    ["other environment", actionsToken({ environment: "production" })],
    ["other repository", actionsToken({ repository_id: "1" })],
    ["other event", actionsToken({ event_name: "push" })],
    ["other issuer", actionsToken({ iss: "https://issuer.invalid" })],
    ["expired", actionsToken({ iat: now - 600, nbf: now - 600, exp: now - 61 })],
    ["not yet valid", actionsToken({ nbf: now + 120 })],
    ["other build", actionsToken({ sha: "c".repeat(40) })],
  ];
  for (const [label, token] of cases) assert.deepEqual(await h.send(body, { token }), BRIDGE_REFUSED, label);
  assert.equal(h.storageCalls(), 0);
  assert.deepEqual(h.planReads, []);
  assert.deepEqual(h.starts, []);
  assert.equal(h.runtime.destroys, 0);
  assert.equal(h.values.has(STAGING_USE_KEY), false);

  const skewed = actionsToken({ iat: now - 400, nbf: now - 400, exp: now - 30 });
  const accepted = await h.send(body, { token: skewed });
  assert.equal(accepted.status, 200);
  assert.equal((accepted.body?.value as StagingUse).purpose, AUTOMOD_STAGING_PURPOSE);
  assert.ok(h.storageCalls() > 0);
});

test("AutoMod bridge: a token for another run is refused before the reservation is read", async t => {
  const h = await bridgeHarness(t);
  h.setPlan(h.enabledPlan);
  const token = actionsToken({ run_id: "654321" });
  for (const body of [{ op: "acquire", pin: h.pin, change: h.change }, { op: "read-lease", context: h.context }]) {
    assert.deepEqual(await h.send(body, { token }), BRIDGE_REFUSED);
  }
  assert.deepEqual(h.planReads, []);
  assert.equal(h.storageCalls(), 0);
  assert.equal(h.values.has(STAGING_USE_KEY), false);
});

test("AutoMod bridge: another version's instance refuses the pinned reservation", async t => {
  const other = "66666666-6666-6666-6666-666666666666";
  const h = await bridgeHarness(t, { ...BRIDGE_ENV, CF_VERSION_METADATA: { id: other } }, other);
  h.setPlan(h.enabledPlan);
  assert.deepEqual(await h.send({ op: "acquire", pin: h.pin, change: h.change }), BRIDGE_REFUSED);
  assert.equal(h.planReads.includes("notAfter"), false, "the reservation gate must not be read");
  assert.equal(h.values.has(STAGING_USE_KEY), false);
  assert.deepEqual(h.starts, []);
  assert.equal(h.runtime.destroys, 0);
});

test("AutoMod bridge: the default plan refuses start, and recover retires the original lease", async t => {
  const h = await bridgeHarness(t);
  assert.equal(STAGING_AUTOMOD_PLAN.enabled, false);
  assert.equal(STAGING_AUTOMOD_PLAN.allowStart, false);
  const disabled = [STAGING_AUTOMOD_PLAN, { ...h.enabledPlan, enabled: false }, { ...h.enabledPlan, allowStart: false },
    { ...h.enabledPlan, notAfter: Date.now() }, { ...h.enabledPlan, notAfter: h.change.expiresAt - 1 },
    { ...h.enabledPlan, leaseMs: 1_000 }];
  for (const plan of disabled) {
    h.setPlan(plan);
    assert.deepEqual(await h.send({ op: "acquire", pin: h.pin, change: h.change }), BRIDGE_REFUSED);
  }
  assert.equal(h.values.has(STAGING_USE_KEY), false);

  h.setPlan(h.enabledPlan);
  const acquired = await h.send({ op: "acquire", pin: h.pin, change: h.change });
  assert.equal(acquired.status, 200);
  const issued = acquired.body?.value as StagingUse;
  assert.deepEqual(await h.send({ op: "read-lease", context: h.context }), { status: 200, body: { value: issued } });
  assert.deepEqual(await h.send({ op: "read-approval", action: "start", issuedLease: issued }),
    { status: 200, body: { value: { action: "start", issuedLease: issued, target: STAGING_AUTOMOD_TARGET } } });

  for (const plan of disabled.slice(0, 4)) {
    h.setPlan(plan);
    assert.deepEqual(await h.send({ op: "read-approval", action: "start", issuedLease: issued }),
      { status: 200, body: { value: null } });
    assert.deepEqual(await h.send({ op: "invoke", pin: h.pin, call: { action: "start", issuedLease: issued } }),
      BRIDGE_REFUSED);
  }
  h.setPlan(STAGING_AUTOMOD_PLAN);
  assert.deepEqual(h.starts, []);
  assert.deepEqual(await new StagingAutomodRecovery(h.ctx.storage as unknown as DurableObjectStorage).pending(),
    { lease: issued, phase: "reserved" });

  const recover = { action: "recover", issuedLease: issued, target: STAGING_AUTOMOD_TARGET };
  assert.deepEqual(await h.send({ op: "read-approval", action: "recover", issuedLease: issued }),
    { status: 200, body: { value: recover } });
  assert.deepEqual(await h.send({ op: "invoke", pin: h.pin, call: { action: "recover", issuedLease: issued } }),
    { status: 200, body: { value: null } });
  assert.deepEqual(h.values.get(STAGING_USE_KEY), { ...issued, phase: "released" });
  assert.deepEqual(await h.send({ op: "read-lease", context: h.context }), { status: 200, body: { value: null } });
  assert.deepEqual(h.starts, []);
});

test("AutoMod bridge: the plan gates refuse even when the in-DO effects would permit start", async t => {
  const h = await bridgeHarness(t);
  let policyReads = 0;
  const policy = { memberIntake: false, enforcement: false, resume: false, registrySync: false,
    interactionDispatch: false, dedicatedIntents: true };
  const effects: ProtectedAutomodEffects = {
    enabled: true,
    dryRun: { get policy() { policyReads++; return policy; }, start: async () => assert.fail("start is not approved") },
    readRuntime: async lease => ({ lease, running: h.runtime.running, policy }),
    readCleanupGrant: async lease => ({ lease, before: lease.expiresAt + 5_000 }),
    fenceAndDrain: async () => assert.fail("no work to drain"),
    stop: async () => true,
    request: async () => assert.fail("no registry access"),
  };
  (h.bot as unknown as { automodEffects: ProtectedAutomodEffects }).automodEffects = effects;
  const disabled = [STAGING_AUTOMOD_PLAN, { ...h.enabledPlan, enabled: false }, { ...h.enabledPlan, notAfter: Date.now() }];
  for (const plan of disabled) {
    h.setPlan(plan);
    assert.deepEqual(await h.send({ op: "acquire", pin: h.pin, change: h.change }), BRIDGE_REFUSED);
  }
  assert.equal(h.values.has(STAGING_USE_KEY), false);

  h.setPlan(h.enabledPlan);
  const issued = (await h.send({ op: "acquire", pin: h.pin, change: h.change })).body?.value as StagingUse;
  assert.equal(issued.purpose, AUTOMOD_STAGING_PURPOSE);
  for (const plan of [...disabled, { ...h.enabledPlan, allowStart: false }]) {
    h.setPlan(plan);
    assert.deepEqual(await h.send({ op: "invoke", pin: h.pin, call: { action: "start", issuedLease: issued } }),
      BRIDGE_REFUSED);
  }
  assert.equal(policyReads, 0, "a refused approval must not reach the private invoker");
  assert.deepEqual(h.starts, []);
});

test("AutoMod bridge: only the closed operation set with a strict bounded body is admitted", async t => {
  const h = await bridgeHarness(t);
  h.setPlan(h.enabledPlan);
  const invalid = { status: 400, body: { error: "staging_automod_invalid" } };
  for (const op of ["release", "publish", "constructor", "__proto__"]) {
    assert.equal((await h.send({ op, context: h.context })).status, 404, op);
  }
  assert.equal((await h.send({ context: h.context })).status, 404);
  const context = JSON.stringify(h.context);
  assert.deepEqual(await h.send(`{"op":"read-lease","context":${context},"context":${context}}`), invalid);
  assert.deepEqual(await h.send(`{"op":"read-lease","op":"read-lease","context":${context}}`), invalid);
  assert.deepEqual(await h.send({ op: "read-lease", context: h.context, padding: "x".repeat(5_000) }), invalid);
  assert.deepEqual(await h.send({ op: "read-lease", context: h.context, extra: true }), invalid);
  assert.deepEqual(await h.send({ op: "read-lease", context: { ...h.context, extra: true } }), invalid);
  for (const extra of [{ voiceMove: true }, { purpose: AUTOMOD_STAGING_PURPOSE }]) {
    assert.deepEqual(await h.send({ op: "acquire", pin: h.pin, change: { ...h.change, ...extra } }), invalid);
  }
  assert.deepEqual(await h.send({ op: "read-approval", action: "publish", issuedLease: {} }), invalid);
  assert.deepEqual(await h.send("[]"), invalid);
  assert.deepEqual(await h.send("not json"), invalid);
  assert.equal(h.values.has(STAGING_USE_KEY), false);
  assert.deepEqual(h.starts, []);
});
