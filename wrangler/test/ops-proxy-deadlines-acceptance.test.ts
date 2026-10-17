/**
 * Acceptance tests (protected): one end-to-end deadline per ops path.
 *
 * Contract under test: the summary proxy, every read-only ops proxy
 * (`proxyOpsBody`: metrics, gateway, backfill, signed backfill-progress) and
 * the keepalive readiness read each enforce ONE six-second deadline across
 * the container fetch AND the bounded body consumption, and body cleanup is
 * best-effort. A container SDK fetch or response stream that ignores abort
 * must not be able to hold the caller (and the ownership gate) past the
 * budget:
 *
 * - a fetch that never resolves settles the caller at the deadline;
 * - a body whose stream stalls mid-read settles the caller at the deadline;
 * - a valid response that only arrives after the deadline is discarded,
 *   never proxied and never recorded as readiness;
 * - a `body.cancel()` whose promise never resolves cannot extend a
 *   refusal/error path (or any path) past the budget.
 *
 * Preserved behavior: generic failure bodies, byte caps (502 on oversize),
 * the summary `ready:false` 200-to-503 mapping, the signed-read 401, the
 * ownership fence, and keepalive scheduling (a failed probe still rearms;
 * a stale epoch drains without probing).
 *
 * These tests were written test-first against a baseline where the three
 * paths above passed a timeout signal to the fetch but not to the body
 * read, awaited the fetch without an abort race, and awaited
 * `body.cancel()` on refusal paths. The deadline tests are expected to
 * FAIL there (each hits the settle guard below) and PASS once the paths
 * use the same abortable pattern as the background metrics scrape. Do not
 * edit this file to make an implementation pass; fix the implementation.
 *
 * Runtime storage/TCP are in-memory; all env values are synthetic. No
 * Discord, database, deployed Worker, Docker daemon or secret bindings
 * are contacted. All transports and clocks are mocked: the production
 * 6000 ms `AbortSignal.timeout` is shrunk to 30 ms and asserted by value.
 * Run: node --import ./test/cloudflare-loader.mjs --test test/ops-proxy-deadlines-acceptance.test.ts
 */
import { test, type TestContext } from "node:test";
import assert from "node:assert/strict";
import { DatabaseSync } from "node:sqlite";
import { setImmediate } from "node:timers/promises";
import { TwoBotContainer, type Env } from "../src/index.ts";
import { OWNER_KEY, DEPLOYMENT_HEADER } from "../src/ownership.ts";

const ID = "deployment-A";
const CONTROL_TOKEN = "synthetic-control-token-not-a-secret-12345";
const ACTIVE_OWNER = {
  deploymentId: ID, epoch: 1, phase: "active", actor: "fixture-operator",
  timestamp: "2026-10-01T00:00:00.000Z", oldEpoch: 0, oldDeploymentId: null,
};
const READINESS_KEY = "two-bot:readiness";
const WORKER_ENV = {
  CF_VERSION_METADATA: { id: ID },
  OWNERSHIP_CONTROL_TOKEN: CONTROL_TOKEN,
  DISCORD_TOKEN: "synthetic-discord-token",
  DATABASE_URL: "synthetic-database-value",
  GUILD_ID: "111222333444555666",
  KEEPALIVE_SECONDS: "60",
  UNREADY_ALERT_FAILURES: "10",
  OPS_ALERT_WEBHOOK_URL: "https://discord.com/api/webhooks/123456789012345678/synthetic-token-not-a-secret",
  OPS_ALERT_FORWARDING: "on",
};
/** Shrunk deadline is 30 ms; a correct caller settles far inside this. */
const SETTLE_BUDGET_MS = 800;

function probeRequest(input: string, init?: RequestInit): Request {
  const req = new Request(input, init);
  req.headers.set(DEPLOYMENT_HEADER, ID);
  return req;
}

function signedProgressRequest(): Request {
  return new Request("https://worker.invalid/ops/backfill/progress", {
    headers: {
      "x-two-key-id": "synthetic-progress-key",
      "x-two-timestamp": "1700000000",
      "x-two-nonce": "synthetic-nonce-0123456789abcdef",
      "x-two-signature": "synthetic-progress-signature",
      [DEPLOYMENT_HEADER]: ID,
    },
  });
}

async function harness(t: TestContext) {
  const database = new DatabaseSync(":memory:");
  t.after(() => database.close());
  const logs: string[] = [];
  for (const method of ["log", "warn", "error"] as const) {
    t.mock.method(console, method, (...args: unknown[]) => {
      logs.push(args.map(String).join(" "));
    });
  }
  const values = new Map<string, unknown>([[OWNER_KEY, ACTIVE_OWNER]]);
  const gates: Promise<unknown>[] = [];
  const runtime = {
    running: false,
    async destroy() { this.running = false; },
    start() { this.running = true; },
    monitor: () => new Promise<void>(() => {}),
    getTcpPort: () => ({
      async fetch() { throw new Error("fixture TCP must not be reached: containerFetch is mocked"); },
    }),
  };
  const ctx = {
    exports: { ContainerProxy: () => ({ fetch: () => { throw new Error("unexpected outbound request"); } }) },
    id: { toString: () => "synthetic-do-id" },
    container: runtime,
    storage: {
      get: async (key: string) => values.get(key),
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
    { CF_VERSION_METADATA: { id: ID }, OWNERSHIP_CONTROL_TOKEN: CONTROL_TOKEN, ...WORKER_ENV } as Env,
  );
  await Promise.all(gates);
  return { bot, logs, values, ctx, database };
}

/**
 * Shrink only the production 6000 ms deadline to 30 ms (the runner cancels
 * live waits near ~5 s) and hand back the mock so a test can assert the
 * production code really requested 6000.
 */
function shrinkDeadline(t: TestContext) {
  const realTimeout = AbortSignal.timeout;
  return t.mock.method(AbortSignal, "timeout", (ms: number) => realTimeout(ms === 6000 ? 30 : ms));
}

/**
 * Fail (do not hang) when the caller does not settle inside the budget.
 * On the unfixed baseline this rejection is the RED evidence: the awaited
 * fetch/body/cancel never settles, so the guard fires first.
 */
async function settleWithin<T>(work: Promise<T>, label: string, ms = SETTLE_BUDGET_MS): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const guard = new Promise<never>((_, reject) => {
    timer = setTimeout(() => reject(new Error(`${label} did not settle within ${ms}ms of a 30ms deadline`)), ms);
  });
  try {
    return await Promise.race([work, guard]);
  } finally {
    clearTimeout(timer);
  }
}

function signalOf(input: unknown, init?: RequestInit): AbortSignal | undefined {
  return init?.signal ?? (input instanceof Request ? input.signal : undefined);
}

function pathOf(input: unknown): string {
  return new URL(typeof input === "string" ? input : (input as Request).url).pathname;
}

/** A stream that yields an optional prefix chunk, then stalls forever. */
function stalledStream(prefix?: string, onCancel?: () => void) {
  let pulled = false;
  return new ReadableStream<Uint8Array>({
    pull(controller) {
      if (prefix !== undefined && !pulled) {
        pulled = true;
        controller.enqueue(new TextEncoder().encode(prefix));
        return;
      }
      return new Promise<void>(() => {});
    },
    cancel() {
      onCancel?.();
      return new Promise<void>(() => {});
    },
  }, { highWaterMark: 0 });
}

/** A stream whose cancel hook runs but whose cancel promise never resolves. */
function cancelStallStream(onCancel: () => void) {
  return new ReadableStream<Uint8Array>({
    pull() {},
    cancel() {
      onCancel();
      return new Promise<void>(() => {});
    },
  }, { highWaterMark: 0 });
}

const NEVER_FETCH = () => new Promise<Response>(() => {});

// ---------------------------------------------------------------------------
// Summary proxy (/ops/summary)
// ---------------------------------------------------------------------------

test("summary: fetch that ignores abort and never resolves settles 504 at the deadline", async (t) => {
  const h = await harness(t);
  const timeout = shrinkDeadline(t);
  let seenSignal: AbortSignal | undefined;
  t.mock.method(h.bot, "containerFetch", (input: unknown, init?: RequestInit) => {
    seenSignal = signalOf(input, init);
    return NEVER_FETCH();
  });
  const response = await settleWithin(
    h.bot.fetch(probeRequest("https://worker.invalid/ops/summary")), "summary never-fetch");
  assert.equal(response.status, 504);
  assert.equal(await response.text(), "summary unavailable\n");
  assert.ok(seenSignal?.aborted, "the deadline signal fired");
  assert.ok(timeout.mock.calls.some((call) => call.arguments[0] === 6000),
    "summary must request the 6 s deadline");
});

test("summary: stalled body stream settles 504 at the deadline, valid prefix never proxied", async (t) => {
  const h = await harness(t);
  shrinkDeadline(t);
  let cancelled = 0;
  t.mock.method(h.bot, "containerFetch", async () => new Response(
    stalledStream('{"ready":true,"synthetic":"prefix', () => { cancelled++; }),
    { status: 200, headers: { "content-type": "application/json" } }));
  const response = await settleWithin(
    h.bot.fetch(probeRequest("https://worker.invalid/ops/summary")), "summary stalled-body");
  assert.equal(response.status, 504);
  assert.equal(await response.text(), "summary unavailable\n");
  assert.ok(cancelled >= 1, "stalled body cleanup is attempted");
});

test("summary: late valid response is discarded; caller already settled 504", async (t) => {
  const h = await harness(t);
  shrinkDeadline(t);
  let cancelled = 0;
  let release: (() => void) | undefined;
  t.mock.method(h.bot, "containerFetch", () => new Promise<Response>((resolve) => {
    release = () => resolve(new Response(
      cancelStallStream(() => { cancelled++; }),
      { status: 200, headers: { "content-type": "application/json" } }));
  }));
  const response = await settleWithin(
    h.bot.fetch(probeRequest("https://worker.invalid/ops/summary")), "summary late-response");
  assert.equal(response.status, 504);
  assert.equal(await response.text(), "summary unavailable\n");
  release!();
  await setImmediate();
  await setImmediate();
  assert.ok(cancelled >= 1, "late response body is released best-effort");
});

test("summary: abort-shaped response whose cancel never resolves still settles 504", async (t) => {
  const h = await harness(t);
  shrinkDeadline(t);
  let cancelled = 0;
  // The SDK resolves the abort as a response; releasing it must not await
  // a cancel hook that never finishes.
  t.mock.method(h.bot, "containerFetch", (input: unknown, init?: RequestInit) =>
    new Promise<Response>((resolve) => {
      signalOf(input, init)?.addEventListener("abort", () => resolve(new Response(
        cancelStallStream(() => { cancelled++; }),
        { status: 500, headers: { "content-type": "text/plain" } })), { once: true });
    }));
  const response = await settleWithin(
    h.bot.fetch(probeRequest("https://worker.invalid/ops/summary")), "summary abort-cancel-stall");
  assert.equal(response.status, 504);
  assert.equal(await response.text(), "summary unavailable\n");
  await setImmediate();
  assert.ok(cancelled >= 1, "cleanup attempted without being awaited");
});

test("summary: non-bot refusal with a never-resolving cancel settles 504 promptly", async (t) => {
  const h = await harness(t);
  let cancelled = 0;
  t.mock.method(h.bot, "containerFetch", async () => new Response(
    cancelStallStream(() => { cancelled++; }),
    { status: 500, headers: { "content-type": "text/plain" } }));
  const response = await settleWithin(
    h.bot.fetch(probeRequest("https://worker.invalid/ops/summary")), "summary refusal-cancel-stall");
  assert.equal(response.status, 504);
  assert.equal(await response.text(), "summary unavailable\n");
  assert.ok(cancelled >= 1, "refusal body cleanup attempted");
});

test("summary (preserved): ready=false maps 200 to 503 and oversize stays 502 generic", async (t) => {
  const h = await harness(t);
  t.mock.method(h.bot, "containerFetch", async () =>
    new Response('{"ready":false}\n', { status: 200, headers: { "content-type": "application/json" } }));
  const mapped = await h.bot.fetch(probeRequest("https://worker.invalid/ops/summary"));
  assert.equal(mapped.status, 503);
  assert.equal(await mapped.text(), '{"ready":false}\n');

  const h2 = await harness(t);
  t.mock.method(h2.bot, "containerFetch", async () =>
    new Response('{"ready":true,"pad":"' + "x".repeat(70 * 1024) + '"}\n',
      { status: 200, headers: { "content-type": "application/json" } }));
  const oversize = await h2.bot.fetch(probeRequest("https://worker.invalid/ops/summary"));
  assert.equal(oversize.status, 502);
  assert.equal(await oversize.text(), "summary unavailable\n");
});

// ---------------------------------------------------------------------------
// proxyOpsBody paths (/ops/metrics, /ops/gateway, /ops/backfill)
// ---------------------------------------------------------------------------

const OPS_PATHS = [
  { path: "/ops/metrics", unavailable: "metrics unavailable\n", validBody: "# HELP x\n" },
  { path: "/ops/gateway", unavailable: "ops unavailable\n", validBody: '{"ready":true}\n' },
  { path: "/ops/backfill", unavailable: "ops unavailable\n", validBody: '{"ready":true}\n' },
] as const;

for (const { path, unavailable, validBody } of OPS_PATHS) {
  test(`${path}: fetch that ignores abort and never resolves settles 504 at the deadline`, async (t) => {
    const h = await harness(t);
    const timeout = shrinkDeadline(t);
    let seenSignal: AbortSignal | undefined;
    t.mock.method(h.bot, "containerFetch", (input: unknown, init?: RequestInit) => {
      seenSignal = signalOf(input, init);
      return NEVER_FETCH();
    });
    const response = await settleWithin(
      h.bot.fetch(probeRequest(`https://worker.invalid${path}`)), `${path} never-fetch`);
    assert.equal(response.status, 504);
    assert.equal(await response.text(), unavailable);
    assert.ok(seenSignal?.aborted, "the deadline signal fired");
    assert.ok(timeout.mock.calls.some((call) => call.arguments[0] === 6000),
      `${path} must request the 6 s deadline`);
  });

  test(`${path}: stalled body stream settles 504 at the deadline, valid prefix never proxied`, async (t) => {
    const h = await harness(t);
    shrinkDeadline(t);
    let cancelled = 0;
    t.mock.method(h.bot, "containerFetch", async () => new Response(
      stalledStream(validBody, () => { cancelled++; }), { status: 200 }));
    const response = await settleWithin(
      h.bot.fetch(probeRequest(`https://worker.invalid${path}`)), `${path} stalled-body`);
    assert.equal(response.status, 504);
    assert.equal(await response.text(), unavailable);
    assert.ok(cancelled >= 1, "stalled body cleanup is attempted");
  });

  test(`${path}: late valid response is discarded; caller already settled 504`, async (t) => {
    const h = await harness(t);
    shrinkDeadline(t);
    let cancelled = 0;
    let release: (() => void) | undefined;
    t.mock.method(h.bot, "containerFetch", () => new Promise<Response>((resolve) => {
      release = () => resolve(new Response(
        cancelStallStream(() => { cancelled++; }), { status: 200 }));
    }));
    const response = await settleWithin(
      h.bot.fetch(probeRequest(`https://worker.invalid${path}`)), `${path} late-response`);
    assert.equal(response.status, 504);
    assert.equal(await response.text(), unavailable);
    release!();
    await setImmediate();
    await setImmediate();
    assert.ok(cancelled >= 1, "late response body is released best-effort");
  });

  test(`${path}: non-OK refusal with a never-resolving cancel settles 504 promptly`, async (t) => {
    const h = await harness(t);
    let cancelled = 0;
    t.mock.method(h.bot, "containerFetch", async () => new Response(
      cancelStallStream(() => { cancelled++; }), { status: 503 }));
    const response = await settleWithin(
      h.bot.fetch(probeRequest(`https://worker.invalid${path}`)), `${path} refusal-cancel-stall`);
    assert.equal(response.status, 504);
    assert.equal(await response.text(), unavailable);
    assert.ok(cancelled >= 1, "refusal body cleanup attempted");
  });
}

// Signed backfill-progress rides the same proxy with headers attached.

test("/ops/backfill/progress: never-resolving signed fetch settles 504 (not 401) at the deadline", async (t) => {
  const h = await harness(t);
  const timeout = shrinkDeadline(t);
  t.mock.method(h.bot, "containerFetch", (input: unknown) => {
    assert.ok(input instanceof Request, "signed read must travel as a Request carrying its headers");
    return NEVER_FETCH();
  });
  const response = await settleWithin(h.bot.fetch(signedProgressRequest()), "progress never-fetch");
  assert.equal(response.status, 504);
  assert.equal(await response.text(), "ops unavailable\n");
  assert.ok(timeout.mock.calls.some((call) => call.arguments[0] === 6000));
});

test("/ops/backfill/progress: stalled signed body settles 504 at the deadline", async (t) => {
  const h = await harness(t);
  shrinkDeadline(t);
  let cancelled = 0;
  t.mock.method(h.bot, "containerFetch", async () => new Response(
    stalledStream('{"ok":true', () => { cancelled++; }), { status: 200 }));
  const response = await settleWithin(h.bot.fetch(signedProgressRequest()), "progress stalled-body");
  assert.equal(response.status, 504);
  assert.equal(await response.text(), "ops unavailable\n");
  assert.ok(cancelled >= 1, "stalled body cleanup is attempted");
});

test("/ops/backfill/progress: container 401 with a never-resolving cancel stays a prompt 401", async (t) => {
  const h = await harness(t);
  let cancelled = 0;
  t.mock.method(h.bot, "containerFetch", async () => new Response(
    cancelStallStream(() => { cancelled++; }), { status: 401 }));
  const response = await settleWithin(h.bot.fetch(signedProgressRequest()), "progress 401-cancel-stall");
  assert.equal(response.status, 401);
  assert.equal(await response.text(), "unauthorized");
  assert.ok(cancelled >= 1, "401 body cleanup attempted without being awaited");
});

test("ops proxies (preserved): small valid bodies proxy and oversize stays 502 generic", async (t) => {
  const h = await harness(t);
  t.mock.method(h.bot, "containerFetch", async (input: unknown) =>
    pathOf(input) === "/metrics"
      ? new Response("# HELP x\n", { status: 200 })
      : new Response('{"ready":true}\n', { status: 200 }));
  const metrics = await h.bot.fetch(probeRequest("https://worker.invalid/ops/metrics"));
  assert.equal(metrics.status, 200);
  assert.equal(metrics.headers.get("content-type"), "text/plain; version=0.0.4; charset=utf-8");
  assert.equal(await metrics.text(), "# HELP x\n");
  const gateway = await h.bot.fetch(probeRequest("https://worker.invalid/ops/gateway"));
  assert.equal(gateway.status, 200);
  assert.equal(gateway.headers.get("content-type"), "application/json");
  assert.equal(await gateway.text(), '{"ready":true}\n');

  const h2 = await harness(t);
  t.mock.method(h2.bot, "containerFetch", async () => new Response('{"k":"v"}\n'.repeat(20000)));
  const oversize = await h2.bot.fetch(probeRequest("https://worker.invalid/ops/gateway"));
  assert.equal(oversize.status, 502);
  assert.equal(await oversize.text(), "ops unavailable\n");
});

test("ops proxies (preserved): wrong deployment fence refuses before any container fetch", async (t) => {
  const h = await harness(t);
  const upstream = t.mock.method(h.bot, "containerFetch", async () => new Response("unreachable"));
  const req = new Request("https://worker.invalid/ops/metrics");
  req.headers.set(DEPLOYMENT_HEADER, "deployment-B");
  const response = await h.bot.fetch(req);
  assert.equal(response.status, 503);
  assert.equal((await response.json() as { error: string }).error, "ownership_fenced");
  assert.equal(upstream.mock.callCount(), 0, "fenced calls never reach the container");
});

// ---------------------------------------------------------------------------
// Keepalive readiness read
// ---------------------------------------------------------------------------

type ReadyzFixture = (signal: AbortSignal | undefined) => Promise<Response>;

async function keepaliveHarness(t: TestContext, readyz: ReadyzFixture) {
  const h = await harness(t);
  const posts: string[] = [];
  t.mock.method(globalThis, "fetch", async (url: URL) => {
    posts.push(String(url));
    return new Response(null, { status: 204 });
  });
  const calls: string[] = [];
  t.mock.method(h.bot, "containerFetch", (input: unknown, init?: RequestInit) => {
    const path = pathOf(input);
    calls.push(path);
    if (path === "/metrics") return Promise.resolve(new Response(null, { status: 503 }));
    return readyz(signalOf(input, init));
  });
  // Alert already firing: a failed/stale probe must never clear it.
  h.values.set(READINESS_KEY, {
    failures: 2, firstFailureAt: 123, alerted: true, lastProbeAt: 123, lastStatus: 503,
  });
  const tick = async (label: string) => {
    await h.bot.onStart();
    const [pending] = await h.bot.listSchedules<{ startedAt: number }>("keepalive");
    assert.ok(pending, "keepalive armed before tick");
    await settleWithin(h.bot.keepalive(pending.payload!, pending), label);
  };
  const readiness = () =>
    h.values.get(READINESS_KEY) as { failures: number; alerted: boolean; lastStatus: number | null };
  const rearmed = async () => (await h.bot.listSchedules("keepalive")).length;
  return { ...h, posts, calls, tick, readiness, rearmed };
}

function assertFailedProbe(h: Awaited<ReturnType<typeof keepaliveHarness>>) {
  assert.equal(h.readiness().lastStatus, null, "timed-out probe is a failed probe, never ready");
  assert.equal(h.readiness().failures, 3, "failure streak grows by one");
  assert.equal(h.readiness().alerted, true, "firing alert is not cleared by missing evidence");
  assert.ok(!h.logs.some((line) => line.includes("container_unready_recovery")),
    "no false recovery event");
  assert.equal(h.posts.length, 0, "no webhook from a failed probe");
}

test("keepalive: readyz fetch that ignores abort and never resolves settles as a failed probe", async (t) => {
  const timeout = shrinkDeadline(t);
  let seenSignal: AbortSignal | undefined;
  const h = await keepaliveHarness(t, (signal) => {
    seenSignal = signal;
    return NEVER_FETCH();
  });
  await h.tick("keepalive never-fetch");
  assertFailedProbe(h);
  assert.ok(seenSignal?.aborted, "the deadline signal fired");
  assert.ok(timeout.mock.calls.some((call) => call.arguments[0] === 6000),
    "keepalive must request the 6 s deadline");
  assert.equal(await h.rearmed(), 1, "failed probe still rearms the chain");
});

test("keepalive: stalled readyz body (cancel also stalls) settles as a failed probe", async (t) => {
  shrinkDeadline(t);
  let cancelled = 0;
  const h = await keepaliveHarness(t, async () => new Response(
    stalledStream('{"ready":true', () => { cancelled++; }),
    { status: 200, headers: { "content-type": "application/json" } }));
  await h.tick("keepalive stalled-body");
  assertFailedProbe(h);
  assert.ok(cancelled >= 1, "stalled body cleanup is attempted");
  assert.equal(await h.rearmed(), 1, "failed probe still rearms the chain");
});

test("keepalive: abort-shaped readyz response settles as a failed probe despite stalling cancel", async (t) => {
  shrinkDeadline(t);
  let cancelled = 0;
  const h = await keepaliveHarness(t, (signal) => new Promise<Response>((resolve) => {
    signal?.addEventListener("abort", () => resolve(new Response(
      cancelStallStream(() => { cancelled++; }),
      { status: 500, headers: { "content-type": "text/plain" } })), { once: true });
  }));
  await h.tick("keepalive abort-response");
  assertFailedProbe(h);
  assert.equal(await h.rearmed(), 1, "failed probe still rearms the chain");
});

test("keepalive: late valid readyz is discarded and never recorded as readiness", async (t) => {
  shrinkDeadline(t);
  let cancelled = 0;
  let release: (() => void) | undefined;
  const h = await keepaliveHarness(t, () => new Promise<Response>((resolve) => {
    release = () => resolve(new Response(
      cancelStallStream(() => { cancelled++; }),
      { status: 200, headers: { "content-type": "application/json" } }));
  }));
  await h.tick("keepalive late-response");
  assertFailedProbe(h);
  release!();
  await setImmediate();
  await setImmediate();
  assertFailedProbe(h);
  assert.ok(cancelled >= 1, "late response body is released best-effort");
  assert.equal(await h.rearmed(), 1, "failed probe still rearms the chain");
});

test("keepalive (preserved): valid ready probe clears the streak and rearms", async (t) => {
  const h = await keepaliveHarness(t, async () => new Response(
    JSON.stringify({ ready: true, components: [["process", "ready"]] }),
    { status: 200, headers: { "content-type": "application/json" } }));
  await h.tick("keepalive valid probe");
  assert.equal(h.readiness().lastStatus, 200);
  assert.equal(h.readiness().failures, 0);
  assert.equal(h.readiness().alerted, false);
  assert.equal(await h.rearmed(), 1);
});

test("keepalive (preserved): stale epoch drains without probing or recording readiness", async (t) => {
  const h = await keepaliveHarness(t, async () => { throw new Error("stale epoch must not probe"); });
  await h.bot.onStart();
  const before = structuredClone(h.values.get(READINESS_KEY));
  await h.bot.keepalive(
    { startedAt: 0, deploymentId: ID, epoch: 999 },
    { taskId: "synthetic-stale" } as never,
  );
  assert.deepEqual(h.calls, [], "stale epoch never reaches the container");
  assert.deepEqual(h.values.get(READINESS_KEY), before, "readiness untouched by a stale tick");
});
