import assert from "node:assert/strict";
import { createHash, generateKeyPairSync, sign } from "node:crypto";
import { DatabaseSync } from "node:sqlite";
import { test, type TestContext } from "node:test";
import { TwoBotContainer, type Env } from "../src/index.ts";
import { DEPLOYMENT_HEADER, OWNER_KEY } from "../src/ownership.ts";
import { STAGING_AUTOMOD_OIDC_HEADER, STAGING_AUTOMOD_PATH } from "../src/staging-automod-bridge.ts";
import type { ProtectedAutomodEffects } from "../src/staging-automod-caller.ts";
import { STAGING_AUTOMOD_PLAN } from "../src/staging-automod-plan.ts";
import { STAGING_AUTOMOD_TARGET, StagingAutomodRecovery } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE, StagingLifecycle, STAGING_USE_KEY, type StagingUse } from "../src/staging-lifecycle.ts";
import publication from "./fixtures/staging-automod-publication.json" with { type: "json" };

const now = 1_800_000_000_000;
const version = "11111111-1111-1111-1111-111111111111";
const sha = "a".repeat(40);
const bearer = "synthetic-automod-control-token-for-tests";
const owner = { deploymentId: version, epoch: 3, phase: "active" as const, actor: "fixture-operator",
  timestamp: new Date(now).toISOString(), oldEpoch: 2, oldDeploymentId: null };
const change = { action: "acquire" as const, environment: "staging" as const, runId: "12345", runAttempt: 1,
  expiresAt: now + 120_000, deploymentId: version, sha, expectedEpoch: owner.epoch };
const identity = { deploymentId: version, sha, owner };
const reservationKey = "two-bot:staging-automod-reservation:v1";
const intentKey = "two-bot:staging-automod-recovery:v1";

async function fixture(t: TestContext) {
  t.mock.timers.enable({ apis: ["Date"], now });
  const sql = new DatabaseSync(":memory:");
  t.after(() => sql.close());
  const values = new Map<string, unknown>([[OWNER_KEY, owner]]);
  const storage = {
    get: async (key: string) => structuredClone(values.get(key)),
    put: async (key: string | Record<string, unknown>, value?: unknown) => {
      for (const [name, entry] of Object.entries(typeof key === "string" ? { [key]: value } : key)) {
        values.set(name, structuredClone(entry));
      }
    },
    delete: async (key: string) => values.delete(key),
    kv: { get: (key: string) => values.get(key), put: (key: string, value: unknown) => values.set(key, value),
      delete: (key: string) => values.delete(key) },
    sql: { exec: (query: string, ...args: (string | number | null)[]) => sql.prepare(query).all(...args) },
    setAlarm: async () => {}, deleteAlarm: async () => {}, sync: async () => {},
  };
  const runtime = {
    running: false,
    destroy: t.mock.fn(async () => { runtime.running = false; }),
    monitor: () => new Promise<void>(() => {}),
    start: () => assert.fail("unexpected startup"),
    getTcpPort: () => assert.fail("unexpected runtime traffic"),
  };
  const build = async (deploymentId = version, release = sha) => {
    const gates: Promise<unknown>[] = [];
    const bot = new TwoBotContainer({ storage, container: runtime, id: { toString: () => "synthetic-automod-do" },
      exports: { ContainerProxy: () => ({ fetch: () => assert.fail("unexpected outbound traffic") }) },
      blockConcurrencyWhile<T>(operation: () => Promise<T>) {
        const result = operation();
        gates.push(result);
        return result;
      },
    } as unknown as DurableObjectState<{}>, {
      ...publication.env, STAGING_RUNTIME_POLICY: "bounded", CF_VERSION_METADATA: { id: deploymentId },
      CONTAINER_RELEASE: release, OWNERSHIP_CONTROL_TOKEN: bearer,
    } as Env);
    await Promise.all(gates);
    return bot;
  };
  const bot = await build();
  const store = storage as unknown as DurableObjectStorage;
  const staging = new StagingLifecycle(store);
  const issue = (stop = async () => {}, window = change) => staging.acquire(window, identity, stop, async expiresAt => {
    await bot.schedule(new Date(expiresAt), "stagingExpiry", AUTOMOD_STAGING_PURPOSE);
  }, Date.now(), AUTOMOD_STAGING_PURPOSE);
  return { bot, build, runtime, staging, issue, values, storage, journal: new StagingAutomodRecovery(store) };
}

test("expiry retires the original unused lease across rollout drift and refuses bridge takeover", async t => {
  const h = await fixture(t);
  const issued = await h.issue();
  const keys = generateKeyPairSync("rsa", { modulusLength: 2048 });
  const kid = "synthetic-actions-key";
  const jwk = { ...keys.publicKey.export({ format: "jwk" }), kid, alg: "RS256", use: "sig" };
  t.mock.method(globalThis, "fetch", async (input: string | URL | Request) => {
    assert.equal(String(input), "https://token.actions.githubusercontent.com/.well-known/jwks");
    return Response.json({ keys: [jwk] });
  });
  const bridge = (bot: TwoBotContainer, expected: StagingUse, build: { deploymentId: string; sha: string }) => {
    const iat = Math.floor(Date.now() / 1000);
    const encode = (value: unknown) => Buffer.from(JSON.stringify(value)).toString("base64url");
    const claims = { iss: "https://token.actions.githubusercontent.com", aud: "two-bot-staging-automod",
      repository: "TogetherWeOwn/two-bot-next", repository_id: "1396224242", ref: "refs/heads/main", ref_protected: "true",
      event_name: "workflow_dispatch", run_id: expected.runId, run_attempt: String(expected.runAttempt), environment: "staging",
      workflow_ref: "TogetherWeOwn/two-bot-next/.github/workflows/staging-automod-preview.yml@refs/heads/main",
      sha: build.sha, iat, nbf: iat, exp: iat + 300 };
    const signed = `${encode({ alg: "RS256", typ: "JWT", kid })}.${encode(claims)}`;
    const jwt = `${signed}.${sign("sha256", Buffer.from(signed), keys.privateKey).toString("base64url")}`;
    const call = { action: "recover", issuedLease: expected };
    const pin = { workerVersionId: build.deploymentId, sourceSha: build.sha, namespaceId: "a".repeat(32),
      workerDeploymentId: "22222222-2222-2222-2222-222222222222", applicationId: "33333333-3333-3333-3333-333333333333",
      rolloutId: "44444444-4444-4444-4444-444444444444", imageDigest: `sha256:${"b".repeat(64)}` };
    return bot.fetch(new Request(`https://worker.invalid${STAGING_AUTOMOD_PATH}`, {
      method: "POST", headers: { authorization: `Bearer ${bearer}`, [DEPLOYMENT_HEADER]: build.deploymentId,
        [STAGING_AUTOMOD_OIDC_HEADER]: jwt },
      body: JSON.stringify({ op: "invoke", pin, call }),
    }));
  };

  h.runtime.running = true;
  await h.bot.stagingExpiry();
  assert.equal(h.runtime.destroy.mock.callCount(), 0, "a stale wake preserves the live lease");

  t.mock.timers.setTime(issued.expiresAt + STAGING_AUTOMOD_PLAN.cleanupMs + 1);
  const next = { deploymentId: "55555555-5555-5555-5555-555555555555", sha: "b".repeat(40) };
  h.bot.deleteSchedules("stagingExpiry");
  const reattached = await h.build(next.deploymentId, next.sha);
  assert.equal((await bridge(h.bot, { ...issued, runAttempt: 2 }, issued)).status, 403);
  assert.equal((await bridge(reattached, issued, next)).status, 403);
  assert.equal((await bridge(reattached, { ...issued, ...next }, next)).status, 403);
  assert.equal(h.runtime.running, true);
  assert.deepEqual(await h.staging.read(), issued);

  await reattached.alarm();
  assert.deepEqual(await h.staging.read(), { ...issued, phase: "released" });
  assert.deepEqual(h.values.get(reservationKey), { lease: issued, phase: "retired" });
  assert.deepEqual(h.values.get(OWNER_KEY), owner);
  assert.equal(h.runtime.running, false);

  const nextLease = await h.issue(undefined, { ...change, runId: "12346", expiresAt: Date.now() + 120_000 });
  assert.equal((await h.bot.listSchedules("stagingExpiry")).length, 1);
  assert.equal((await bridge(h.bot, nextLease, nextLease)).status, 200);
  assert.deepEqual(await h.bot.listSchedules("stagingExpiry"), []);
});

test("a preparation interrupted during teardown retains its expiry alarm", async t => {
  const h = await fixture(t);
  await assert.rejects(h.issue(async () => {
    assert.equal((await h.bot.listSchedules("stagingExpiry")).length, 1);
    throw new Error("interrupted teardown");
  }), /interrupted teardown/);
  const preparing = await h.staging.read();
  assert.equal(preparing?.phase, "preparing");

  t.mock.timers.setTime(change.expiresAt);
  const get = h.storage.get;
  const failRead = t.mock.method(h.storage, "get", async (key: string) => {
    if (key === STAGING_USE_KEY) throw new Error("interrupted lease read");
    return get(key);
  });
  await h.bot.alarm();
  assert.equal((await h.bot.listSchedules("stagingExpiry"))[0]?.payload, true);
  failRead.mock.restore();
  t.mock.timers.tick(15_000);
  await h.bot.alarm();
  assert.deepEqual(await h.staging.read(), { ...preparing, phase: "released" });
  await assert.rejects(h.staging.acquire({ ...change, expiresAt: Date.now() + 10_000 }, identity,
    async () => assert.fail("ended attempt cannot acquire"), async () => {}, Date.now(), AUTOMOD_STAGING_PURPOSE), /staging_use_ended/);
});

test("interrupted stop and release preserve the lease and retry durable retirement", async t => {
  const h = await fixture(t);
  const issued = await h.issue();
  h.runtime.running = true;
  const destroy = t.mock.method(h.runtime, "destroy", async () => { throw new Error("lost stop acknowledgement"); });
  h.bot.deleteSchedules("stagingExpiry");
  await h.bot.schedule(new Date(Date.now() + 15_000), "stagingExpiry", true);
  const interrupted = await h.build();
  t.mock.timers.tick(15_000);
  await interrupted.stagingExpiry();
  assert.deepEqual(await h.journal.pending(), { lease: issued, phase: "reserved" });
  assert.deepEqual(await h.staging.read(), issued);
  const retry = await h.bot.listSchedules("stagingExpiry");
  assert.partialDeepStrictEqual(retry[0], { payload: true, time: (Date.now() + 15_000) / 1_000 });

  const rebuilt = await h.build();
  assert.deepEqual(await rebuilt.listSchedules("stagingExpiry"), retry);
  destroy.mock.restore();
  const put = h.storage.put;
  const release = t.mock.method(h.storage, "put", async (key: Parameters<typeof put>[0], value?: unknown) => {
    if (typeof key !== "string" && (key[STAGING_USE_KEY] as StagingUse | undefined)?.phase === "released") {
      assert.equal(await h.journal.completedFor(issued), true, "journal retirement precedes the release write");
      throw new Error("interrupted release");
    }
    await put(key, value);
  });
  t.mock.timers.tick(15_000);
  await rebuilt.alarm();
  assert.deepEqual(await h.staging.read(), issued);
  assert.equal(h.runtime.running, false);
  assert.equal((await rebuilt.listSchedules("stagingExpiry"))[0]?.payload, true);

  release.mock.restore();
  h.runtime.running = true;
  const reattached = await h.build();
  const failReplayStop = t.mock.method(h.runtime, "destroy", async () => { throw new Error("reattached stop failed"); });
  t.mock.timers.tick(15_000);
  await reattached.stagingExpiry();
  assert.deepEqual(await h.staging.read(), issued, "terminal journal cannot release a running container");
  assert.equal((await h.bot.listSchedules("stagingExpiry"))[0]?.payload, true);
  failReplayStop.mock.restore();

  t.mock.timers.tick(15_000);
  await (await h.build()).alarm();
  assert.deepEqual(await h.staging.read(), { ...issued, phase: "released" });
  assert.ok(Date.now() < issued.expiresAt, "a stop retry can retire before the original deadline");
  assert.equal(h.runtime.running, false);
  assert.equal(h.runtime.destroy.mock.callCount(), 2, "terminal recovery stops a reattached process before releasing");
  assert.deepEqual(await rebuilt.listSchedules("stagingExpiry"), []);
});

test("expiry keeps the hold when owner or journal evidence is missing or mismatched", async t => {
  for (const scenario of ["wrong owner epoch", "missing journal", "different reservation", "malformed journal"]) {
    await t.test(scenario, async t => {
      const h = await fixture(t);
      const issued = await h.issue();
      if (scenario === "wrong owner epoch") h.values.set(OWNER_KEY, { ...owner, epoch: owner.epoch + 1 });
      if (scenario === "missing journal") h.values.delete(reservationKey);
      if (scenario === "different reservation") {
        h.values.set(reservationKey, { lease: { ...issued, runId: "55555" }, phase: "reserved" });
      }
      if (scenario === "malformed journal") h.values.set(reservationKey, { phase: "unknown" });
      const before = structuredClone(h.values);
      h.runtime.running = true;
      t.mock.timers.setTime(issued.expiresAt);
      await h.bot.stagingExpiry();
      assert.deepEqual(h.values, before);
      assert.equal(h.runtime.destroy.mock.callCount(), 0);
      assert.equal((await h.bot.listSchedules("stagingExpiry"))[0]?.payload, true);
    });
  }
});

test("an expired startup claim holds until exact drain, stop and independent registry proof", async t => {
  const h = await fixture(t);
  const issued = await h.issue();
  const intent = { lease: issued, target: STAGING_AUTOMOD_TARGET, publishBefore: issued.expiresAt - 5_000,
    baseline: createHash("sha256").update("[]").digest("hex") };
  const { id, version } = publication.replies.preview;
  await h.journal.persistIntent(intent, async () => ({ lease: issued, baselineAbsent: true, publishBefore: intent.publishBefore }));
  await h.journal.recordCreated({ intent, id, version });
  await h.journal.confirmPublication({ intent, id, version });
  await h.journal.claimStartup(issued, async () => {});
  h.runtime.running = true;
  t.mock.timers.setTime(issued.expiresAt);
  await h.bot.stagingExpiry();
  assert.equal(h.runtime.running, true, "native stop cannot substitute for remote drain");
  assert.deepEqual(await h.staging.read(), issued);
  assert.equal((h.values.get(intentKey) as { phase: string }).phase, "cleanup");
  assert.equal((await h.bot.listSchedules("stagingExpiry"))[0]?.payload, true);

  const rebuilt = await h.build();
  const calls: string[] = [];
  const effects: ProtectedAutomodEffects = {
    readRuntime: async lease => ({ lease, running: h.runtime.running }),
    readCleanupGrant: async lease => ({ lease, before: issued.expiresAt + 60_000 }),
    fenceAndDrain: async lease => { assert.deepEqual(lease, issued); calls.push("drain");
      return { lease, publicationDrained: true, startupDrained: true }; },
    stop: async () => { calls.push("stop"); await h.runtime.destroy(); return true; },
    request: async (url, init) => {
      assert.equal(init.method, "GET");
      assert.equal(h.runtime.running, false);
      if (url.endsWith("/users/@me")) { calls.push("bot"); return Response.json(publication.identity); }
      if (url.endsWith("/commands?with_localizations=true")) { calls.push("list"); return Response.json([]); }
      assert.ok(url.endsWith(`/commands/${id}`));
      calls.push("detail");
      return Response.json({ code: 10063 }, { status: 404 });
    },
  };
  (rebuilt as unknown as { automodEffects: ProtectedAutomodEffects }).automodEffects = effects;
  t.mock.timers.tick(15_000);
  await rebuilt.alarm();
  assert.deepEqual(calls, ["drain", "stop", "bot", "list", "detail"]);
  assert.equal(await h.journal.completedFor(issued), true);
  assert.deepEqual(await h.staging.read(), { ...issued, phase: "released" });
});
