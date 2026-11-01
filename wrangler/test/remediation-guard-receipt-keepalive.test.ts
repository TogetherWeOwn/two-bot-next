/**
 * Keepalive integration for staging-only persisted remediation guard receipts.
 *
 * Synthetic DO/storage/TCP only; no Discord, database, Worker or secret is
 * contacted. Proves the receipt reflects the actual stored guard (read-back),
 * never claims success on storage failure, carries exact correlation, stays
 * public-safe, emits at most one per tick, and stays dark in production.
 */
import { test, type TestContext } from "node:test";
import assert from "node:assert/strict";
import { DatabaseSync } from "node:sqlite";
import { TwoBotContainer, type Env } from "../src/index.ts";
import { OWNER_KEY, DEPLOYMENT_HEADER } from "../src/ownership.ts";
import { metricsBody } from "./fixtures/metrics.ts";

const ID = "deployment-A";
const CONTROL_TOKEN = "synthetic-control-token-not-a-secret-12345";
const ACTIVE_OWNER = {
  deploymentId: ID, epoch: 1, phase: "active", actor: "fixture-operator",
  timestamp: "2026-10-01T00:00:00.000Z", oldEpoch: 0, oldDeploymentId: null,
};
const RELEASE_SHA = "9f3a1c7e2b4d6a8c0e1f3a5b7c9d2e4f6a8b0c1d";

const BASE_ENV = {
  CF_VERSION_METADATA: { id: ID },
  OWNERSHIP_CONTROL_TOKEN: CONTROL_TOKEN,
  DISCORD_TOKEN: "synthetic-discord-token-not-a-secret",
  DATABASE_URL: "synthetic-database-value-not-a-secret",
  GUILD_ID: "111222333444555666",
  KEEPALIVE_SECONDS: "60",
  REDIRECT_FALLBACK_CODE: "not-a-container-var",
  REDIRECT_MAPPINGS_JSON: "[]",
  OPS_ALERT_WEBHOOK_URL: "https://discord.com/api/webhooks/123456789012345678/synthetic-token-not-a-secret",
  OPS_ALERT_FORWARDING: "off",
  UNREADY_ALERT_FAILURES: "10",
};

function staleMetrics(): string {
  return metricsBody()
    .replace("two_bot_voice_creator_map_generation 0", "two_bot_voice_creator_map_generation 1")
    .replace("two_bot_voice_creator_map_age_seconds 0", "two_bot_voice_creator_map_age_seconds 660");
}

async function receiptHarness(
  t: TestContext,
  env: Record<string, unknown>,
  metrics: () => string,
  storage?: { getThrows?: boolean; getMismatch?: boolean; putThrows?: boolean; syncThrows?: boolean },
) {
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
  let remediationPuts = 0;
  // The container base also calls storage.sync() for its own schedule
  // bookkeeping, so a receipt-flush failure must not break setup or the
  // tick's reschedule: fail only the first sync after setup (the receipt
  // flush, which runs before the tick's reschedule).
  let setupComplete = false;
  let receiptSyncFailed = false;
  const ctx = {
    exports: { ContainerProxy: () => ({ fetch: () => { throw new Error("unexpected outbound"); } }) },
    id: { toString: () => "synthetic-do-id" },
    container: {
      running: false,
      async destroy() {},
      start() { (this as { running: boolean }).running = true; },
      monitor: () => new Promise<void>(() => {}),
      getTcpPort: () => ({
        async fetch() { throw new Error("must use containerFetch mock"); },
      }),
    },
    storage: {
      get: async (key: string) => {
        if (storage?.getThrows && key === "two-bot:remediation") throw new Error("synthetic storage read failure");
        if (storage?.getMismatch && key === "two-bot:remediation") {
          return { lastRecommendedAtSeconds: 1, scopes: values.get("two-bot:remediation") };
        }
        return values.get(key);
      },
      put: async (key: string | Record<string, unknown>, value?: unknown) => {
        if (storage?.putThrows && (key === "two-bot:remediation" || (typeof key !== "string" && "two-bot:remediation" in (key as object)))) {
          throw new Error("synthetic storage write failure");
        }
        if (typeof key === "string") {
          if (key === "two-bot:remediation") remediationPuts += 1;
          values.set(key, value);
        } else {
          for (const [k, v] of Object.entries(key)) {
            if (k === "two-bot:remediation") remediationPuts += 1;
            values.set(k, v);
          }
        }
      },
      delete: async (key: string) => { values.delete(key); },
      kv: {
        get: (key: string) => values.get(key),
        put: (key: string, value: unknown) => { values.set(key, value); },
        delete: (key: string) => { values.delete(key); },
      },
      sql: {
        exec(query: string, ...bindings: (string | number | null)[]) {
          return database.prepare(query).all(...bindings);
        },
      },
      setAlarm: async () => {},
      deleteAlarm: async () => {},
      sync: async () => {
        if (storage?.syncThrows && setupComplete && !receiptSyncFailed) {
          receiptSyncFailed = true;
          throw new Error("synthetic storage sync failure");
        }
      },
    },
    blockConcurrencyWhile<T>(callback: () => Promise<T>) {
      const promise = callback();
      gates.push(promise);
      return promise;
    },
  };
  const bot = new TwoBotContainer(
    ctx as unknown as DurableObjectState<{}>,
    { ...BASE_ENV, ...env } as unknown as Env,
  );
  await Promise.all(gates);
  t.mock.method(bot, "containerFetch", async (input: string | Request) => {
    const path = new URL(typeof input === "string" ? input : input.url).pathname;
    if (path === "/metrics") return new Response(metrics());
    return new Response(JSON.stringify({
      components: [["process", "ready"], ["gateway", "ready"]],
      build_revision: RELEASE_SHA, build_id: "123-1",
    }), { status: 200, headers: { "content-type": "application/json" } });
  });
  t.mock.method(globalThis, "fetch", async () => new Response(null, { status: 204 }));
  await bot.onStart();
  setupComplete = true;
  const tick = async () => {
    const [pending] = await bot.listSchedules<{ startedAt: number }>("keepalive");
    assert.ok(pending);
    await bot.keepalive(pending.payload!, pending);
  };
  const receipts = () => logs
    .filter((line) => line.startsWith("{"))
    .map((line) => JSON.parse(line))
    .filter((event) => event.event === "remediation_guard_receipt");
  return { bot, logs, values, tick, receipts, remediationPuts: () => remediationPuts };
}

test("staging emits one baseline receipt per tick reflecting the stored guard", async (t) => {
  const h = await receiptHarness(t, { TWO_DEPLOY_ENV: "staging" }, () => metricsBody());
  await h.tick();
  await h.tick();
  const receipts = h.receipts();
  assert.equal(receipts.length, 2, `expected one receipt per tick, got ${receipts.length}`);
  for (const receipt of receipts) {
    assert.equal(receipt.ok, true);
    assert.equal(receipt.deploy_env, "staging");
    assert.equal(receipt.decision, "idle");
    assert.deepEqual(receipt.firing, []);
    assert.equal(receipt.build_revision, RELEASE_SHA);
    assert.equal(receipt.release, RELEASE_SHA);
    assert.equal(receipt.worker_version, ID);
    assert.ok(Number.isSafeInteger(receipt.evaluated_at_seconds));
    // The receipt guard is the read-back stored guard, not a fresh object.
    assert.deepEqual(receipt.guard, h.values.get("two-bot:remediation"));
  }
});

test("staging firing receipt recommends R1 with trigger correlation", async (t) => {
  const h = await receiptHarness(t, { TWO_DEPLOY_ENV: "staging" }, staleMetrics);
  await h.tick();
  const receipts = h.receipts();
  assert.equal(receipts.length, 1);
  assert.equal(receipts[0].ok, true);
  assert.equal(receipts[0].decision, "recommend");
  assert.equal(receipts[0].action, "creator-map-reload");
  assert.deepEqual(receipts[0].firing, ["voice_creator_map_stale"]);
  assert.deepEqual(receipts[0].guard, h.values.get("two-bot:remediation"));
});

// A deaf voice actor: the live view's not-ready age past the 120 s bound.
function deafMetrics(age: number | undefined): () => string {
  return () => metricsBody()
    .split("\n")
    .filter((row) => age !== undefined || !row.startsWith("two_bot_voice_live_not_ready_seconds "))
    .map((row) => age !== undefined && row.startsWith("two_bot_voice_live_not_ready_seconds ")
      ? `two_bot_voice_live_not_ready_seconds ${age}` : row)
    .join("\n");
}

test("staging deaf voice actor: takeover, frozen across a missing age, resumed on a fresh one", async (t) => {
  let scrape = deafMetrics(121);
  let clock = 1_760_000_000_000;
  t.mock.method(Date, "now", () => clock);
  const h = await receiptHarness(t, { TWO_DEPLOY_ENV: "staging" }, () => scrape());
  const alerting = () => (h.values.get("two-bot:metrics-alerts") as { firing: string[] } | undefined)?.firing ?? [];

  await h.tick();
  const [first] = h.receipts();
  assert.equal(first.ok, true);
  assert.equal(first.decision, "recommend");
  assert.equal(first.action, "same-sha-takeover");
  assert.deepEqual(first.firing, ["voice_live_not_ready"]);
  assert.equal(first.guard.scopes["voice-live"].phase, "takeover-recommended");
  assert.ok(h.logs.some((row) => row.includes("REMEDIATION voice-live: same-sha-takeover")));
  const guardAfterTakeover = structuredClone(h.values.get("two-bot:remediation"));

  // Past the cooldown, the scrape omits the optional age gauge: the alert is
  // held, but the guard must not move and the receipt names the evidence hold.
  scrape = deafMetrics(undefined);
  clock += 1_801_000;
  await h.tick();
  const gap = h.receipts().at(-1);
  assert.deepEqual(alerting(), ["voice_live_not_ready"]);
  assert.equal(gap.ok, true);
  assert.equal(gap.decision, "held-evidence");
  assert.deepEqual(h.values.get("two-bot:remediation"), guardAfterTakeover);

  // A fresh age sample resumes progression from the untouched guard.
  scrape = deafMetrics(121);
  clock += 60_000;
  await h.tick();
  const resumed = h.receipts().at(-1);
  assert.equal(resumed.decision, "recommend");
  assert.equal(resumed.action, "same-sha-takeover");
  assert.equal(resumed.guard.scopes["voice-live"].consecutiveFailures, 1);
});

test("a rejected scrape gap is not recovery: the stopped deaf scope needs a fresh observed window", async (t) => {
  let scrape = deafMetrics(0);
  let clock = 1_760_000_000_000;
  t.mock.method(Date, "now", () => clock);
  const h = await receiptHarness(t, { TWO_DEPLOY_ENV: "staging" }, () => scrape());
  const t0 = Math.floor(clock / 1000);
  const stopped = {
    phase: "takeover-recommended", consecutiveFailures: 2, suppressed: true, clearSinceSeconds: 0,
  };
  const idle = { phase: "idle", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 };
  h.values.set("two-bot:remediation", {
    lastRecommendedAtSeconds: t0 - 60,
    scopes: { "voice-stall": { ...idle }, "voice-live": { ...stopped }, "crash-loop": { ...idle } },
  });
  const live = () => (h.values.get("two-bot:remediation") as { scopes: Record<string, typeof stopped> }).scopes["voice-live"];

  // One observed quiet sample starts the clear clock.
  await h.tick();
  assert.equal(live().clearSinceSeconds, t0);
  // A full window of rejected scrapes (a malformed age fails the whole scrape).
  scrape = () => deafMetrics(0)().replace("two_bot_voice_live_not_ready_seconds 0", "two_bot_voice_live_not_ready_seconds NaN");
  clock += 1_860_000;
  await h.tick();
  assert.ok(h.logs.some((row) => row.includes("two-bot metrics scrape failed")));
  assert.deepEqual(live(), stopped);
  // The next observed quiet sample starts a fresh window instead of resetting.
  scrape = deafMetrics(0);
  clock += 60_000;
  await h.tick();
  assert.equal(live().suppressed, true);
  assert.equal(live().clearSinceSeconds, Math.floor(clock / 1000));
  // And a refire right after is still stopped: no fresh takeover.
  scrape = deafMetrics(121);
  clock += 60_000;
  await h.tick();
  assert.equal(live().suppressed, true);
  assert.ok(!h.logs.some((row) => row.includes("REMEDIATION voice-live: same-sha-takeover")));
});

test("production emits no guard receipt", async (t) => {
  for (const env of [{}, { TWO_DEPLOY_ENV: "production" }, { TWO_DEPLOY_ENV: "" }]) {
    const h = await receiptHarness(t, env, () => metricsBody());
    await h.tick();
    assert.deepEqual(h.receipts(), [], `production env ${JSON.stringify(env)} must stay dark`);
  }
});

test("storage write failure emits no success receipt", async (t) => {
  const h = await receiptHarness(t, { TWO_DEPLOY_ENV: "staging" }, () => metricsBody(), { putThrows: true });
  await h.tick();
  assert.ok(h.receipts().every((receipt) => receipt.ok !== true), "no false success on storage failure");
});

test("storage read failure emits no success receipt", async (t) => {
  const h = await receiptHarness(t, { TWO_DEPLOY_ENV: "staging" }, () => metricsBody(), { getThrows: true });
  await h.tick();
  assert.ok(h.receipts().every((receipt) => receipt.ok !== true), "no false success on read-back failure");
});

test("storage sync failure emits a readback_failed receipt, never a success", async (t) => {
  const h = await receiptHarness(t, { TWO_DEPLOY_ENV: "staging" }, () => metricsBody(), { syncThrows: true });
  await h.tick();
  const receipts = h.receipts();
  assert.equal(receipts.length, 1);
  assert.equal(receipts[0].ok, false);
  assert.equal(receipts[0].reason, "readback_failed");
});

test("mismatched read-back is a failure receipt, never a success", async (t) => {
  const h = await receiptHarness(t, { TWO_DEPLOY_ENV: "staging" }, () => metricsBody(), { getMismatch: true });
  await h.tick();
  const receipts = h.receipts();
  assert.equal(receipts.length, 1);
  assert.equal(receipts[0].ok, false);
});

test("receipts never carry secrets or webhook bindings", async (t) => {
  const h = await receiptHarness(t, { TWO_DEPLOY_ENV: "staging" }, staleMetrics);
  await h.tick();
  const text = h.receipts().map((receipt) => JSON.stringify(receipt)).join("\n");
  assert.ok(text.length > 0);
  for (const secret of [
    BASE_ENV.DISCORD_TOKEN,
    BASE_ENV.DATABASE_URL,
    BASE_ENV.OPS_ALERT_WEBHOOK_URL,
    CONTROL_TOKEN,
  ]) {
    assert.ok(!text.includes(secret), "receipt must not carry secret material");
  }
  assert.doesNotMatch(text, /bearer|token|secret|webhook/i);
});
