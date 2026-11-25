import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { DatabaseSync } from "node:sqlite";
import { test, type TestContext } from "node:test";
import { TwoBotContainer, type Env } from "../src/index.ts";
import { DEPLOYMENT_HEADER, OWNER_KEY, OwnershipRefused } from "../src/ownership.ts";
import type { ProtectedAutomodEffects } from "../src/staging-automod-caller.ts";
import { StagingAutomodRecovery } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE, STAGING_USE_KEY, STAGING_USE_PATH, type StagingUse } from "../src/staging-lifecycle.ts";
import { admission, intent, lease, now } from "./fixtures/staging-automod-recovery.ts";
import publication from "./fixtures/staging-automod-publication.json" with { type: "json" };
import databaseTargets from "./fixtures/staging-database-fingerprint.json" with { type: "json" };

const autoModLease = { ...lease, purpose: AUTOMOD_STAGING_PURPOSE };
const voiceLease = { ...lease, voiceMove: true } as const;
const owner = {
  deploymentId: lease.deploymentId, epoch: lease.epoch, phase: "active", actor: "fixture-operator",
  timestamp: new Date(lease.acquiredAt).toISOString(), oldEpoch: lease.epoch - 1, oldDeploymentId: null,
};
const token = "synthetic-purpose-test-control-token";
const purposeConflict = (error: unknown) =>
  error instanceof OwnershipRefused && error.reason === "staging_use_conflict";

// Run the real DO and Container SDK with in-memory durable state and schedules.
async function fixture(t: TestContext, persistedLease?: StagingUse) {
  t.mock.timers.enable({ apis: ["Date"], now: now() });
  const unexpected = t.mock.method(globalThis, "fetch", () => assert.fail("unexpected network request"));
  t.after(() => assert.equal(unexpected.mock.callCount(), 0));
  const database = new DatabaseSync(":memory:");
  t.after(() => database.close());
  const values = new Map<string, unknown>([[OWNER_KEY, owner]]);
  if (persistedLease) values.set(STAGING_USE_KEY, persistedLease);
  const storage = {
    get: async (key: string) => structuredClone(values.get(key)),
    put: async (key: string | Record<string, unknown>, value?: unknown) => {
      for (const [name, entry] of Object.entries(typeof key === "string" ? { [key]: value } : key)) {
        values.set(name, structuredClone(entry));
      }
    },
    delete: async (key: string) => values.delete(key),
    kv: {
      get: (key: string) => values.get(key),
      put: (key: string, value: unknown) => values.set(key, value),
      delete: (key: string) => values.delete(key),
    },
    sql: { exec: (query: string, ...bindings: (string | number | null)[]) => database.prepare(query).all(...bindings) },
    setAlarm: async () => {},
    deleteAlarm: async () => {},
    sync: async () => {},
  };
  const runtime = {
    running: false,
    start: t.mock.fn(() => assert.fail("unexpected native start")),
    destroy: t.mock.fn(() => assert.fail("unexpected native destroy")),
    getTcpPort: t.mock.fn(() => assert.fail("unexpected container traffic")),
  };
  const initialization: Promise<unknown>[] = [];
  const bot = new TwoBotContainer({
    id: { toString: () => "synthetic-purpose-do" },
    container: runtime,
    storage,
    blockConcurrencyWhile<T>(callback: () => Promise<T>) {
      const result = callback();
      initialization.push(result);
      return result;
    },
  } as unknown as DurableObjectState<{}>, {
    ...publication.env,
    CF_VERSION_METADATA: { id: lease.deploymentId },
    CONTAINER_RELEASE: lease.sha,
    STAGING_RUNTIME_POLICY: "bounded",
    DATABASE_URL: databaseTargets.matching[0],
    STAGING_RECOVERY_DB_FINGERPRINT: databaseTargets.fingerprint,
    OWNERSHIP_CONTROL_TOKEN: token,
    INTERNAL_ACTIONS_INGRESS: "1",
    TWO_INTERNAL_ACTIONS: "1",
    TWO_INTERNAL_ALLOW_VOICE_MOVE: "1",
  } as Env);
  await Promise.all(initialization);

  const effects = {
    enabled: true,
    dryRun: {
      policy: {
        memberIntake: false, enforcement: false, resume: false, registrySync: false,
        interactionDispatch: false, dedicatedIntents: true,
      },
      start: t.mock.fn(async () => assert.fail("unexpected dry-run start")),
    },
    readRuntime: t.mock.fn(async (lease: Readonly<StagingUse>) => ({ lease, running: runtime.running })),
    readCleanupGrant: t.mock.fn(async () => assert.fail("unexpected cleanup grant read")),
    fenceAndDrain: t.mock.fn(async () => assert.fail("unexpected drain")),
    stop: t.mock.fn(async () => assert.fail("unexpected stop")),
    request: t.mock.fn(async () => assert.fail("unexpected registry request")),
  } satisfies ProtectedAutomodEffects;
  return {
    bot,
    values,
    effects,
    journal: new StagingAutomodRecovery(storage as unknown as DurableObjectStorage),
    startAutoMod: (issuedLease: StagingUse) => TwoBotContainer.runProtectedAutomod(bot, { action: "start", issuedLease }, effects),
    control: (action: "acquire" | "release", voiceMove = false) => bot.fetch(new Request(`https://worker.invalid${STAGING_USE_PATH}`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}`, [DEPLOYMENT_HEADER]: lease.deploymentId },
      body: JSON.stringify({
        action, environment: lease.environment, runId: lease.runId, runAttempt: lease.runAttempt, expiresAt: lease.expiresAt,
        deploymentId: lease.deploymentId, sha: lease.sha, expectedEpoch: lease.epoch,
        ...(voiceMove ? { voiceMove: true } : {}),
      }),
    })),
    assertNoEffects() {
      for (const effect of [
        runtime.start, runtime.destroy, runtime.getTcpPort, effects.dryRun.start, effects.readRuntime,
        effects.readCleanupGrant, effects.fenceAndDrain, effects.stop, effects.request,
      ]) assert.equal(effect.mock.callCount(), 0);
      assert.equal(runtime.running, false);
    },
  };
}

test("ordinary and signed-voice leases cannot start AutoMod or lose their cleanup", async (t) => {
  for (const [name, voiceMove] of [["ordinary", false], ["signed voice", true]] as const) {
    await t.test(name, async (t) => {
      const h = await fixture(t);
      const acquired = await h.control("acquire", voiceMove);
      assert.equal(acquired.status, 200);
      const { use, voiceMoveEnabled } = await acquired.json() as { use: StagingUse; voiceMoveEnabled: boolean };
      assert.equal(voiceMoveEnabled, voiceMove);
      assert.deepEqual(use, { ...(voiceMove ? voiceLease : lease), acquiredAt: now() });
      const before = structuredClone(h.values);
      const schedules = await h.bot.listSchedules("stagingExpiry");
      assert.equal(schedules[0]?.time, lease.expiresAt / 1000);

      await assert.rejects(h.startAutoMod(use), purposeConflict);
      // Even an AutoMod-marked request with the same tuple needs stored AutoMod authority.
      await assert.rejects(h.startAutoMod({ ...autoModLease, acquiredAt: use.acquiredAt }), purposeConflict);
      h.assertNoEffects();
      assert.deepEqual(h.values, before);
      assert.deepEqual(await h.bot.listSchedules("stagingExpiry"), schedules);

      const released = await h.control("release");
      assert.equal(released.status, 200);
      assert.deepEqual(h.values.get(STAGING_USE_KEY), { ...lease, acquiredAt: now(), phase: "released" });
      assert.deepEqual(h.values.get(OWNER_KEY), owner);
      assert.deepEqual(await h.bot.listSchedules("stagingExpiry"), []);
    });
  }
});

test("AutoMod lease refuses other startup lanes and retains exact cleanup ownership", async (t) => {
  const h = await fixture(t, autoModLease);
  // AutoMod acquisition persists this deadline before marking the lease active.
  await h.bot.schedule(new Date(autoModLease.expiresAt), "stagingExpiry", null);
  const expiry = await h.bot.listSchedules("stagingExpiry");
  assert.equal(expiry.length, 1);
  assert.equal(expiry[0]?.time, autoModLease.expiresAt / 1000);
  const reserved = structuredClone(h.values);
  // Prove the purpose alone bars both lanes before the journal can supply its own hold.
  await assert.rejects(h.bot.startAndWaitForPorts(8080), purposeConflict);
  const voice = await h.control("acquire", true);
  assert.equal(voice.status, 503);
  assert.equal((await voice.json() as { reason: string }).reason, "staging_use_conflict");
  assert.deepEqual(h.values, reserved);
  assert.deepEqual(await h.bot.listSchedules("stagingExpiry"), expiry);

  // A pending intent with an empty baseline is sufficient for recovery without a registry mutation.
  const pending = { ...intent, lease: autoModLease, baseline: createHash("sha256").update("[]").digest("hex") };
  await h.journal.persistIntent(pending, async () => ({ ...admission, lease: autoModLease }));
  const before = structuredClone(h.values);

  await assert.rejects(h.startAutoMod(lease), purposeConflict);
  await assert.rejects(h.startAutoMod(voiceLease), purposeConflict);
  h.assertNoEffects();
  assert.deepEqual(h.values, before);
  assert.deepEqual(await h.bot.listSchedules("stagingExpiry"), expiry);

  const drained = t.mock.fn(async (lease: Readonly<StagingUse>) => {
    assert.deepEqual(lease, autoModLease);
    return { lease, publicationDrained: true, startupDrained: true };
  });
  const stopped = t.mock.fn(async (lease: Readonly<StagingUse>) => {
    assert.deepEqual(lease, autoModLease);
    return true;
  });
  const { DISCORD_APPLICATION_ID: app, GUILD_ID: guild } = publication.env;
  const reads: string[] = [];
  await TwoBotContainer.runProtectedAutomod(h.bot, { action: "recover", issuedLease: autoModLease }, {
    ...h.effects,
    readCleanupGrant: async lease => ({ lease, before: autoModLease.expiresAt }),
    fenceAndDrain: drained,
    stop: stopped,
    request: async (url, init) => {
      assert.equal(init.method, "GET");
      reads.push(url);
      if (url === "https://discord.com/api/v10/users/@me") return Response.json(publication.identity);
      assert.equal(url, `https://discord.com/api/v10/applications/${app}/guilds/${guild}/commands?with_localizations=true`);
      return Response.json([]);
    },
  });
  assert.equal(drained.mock.callCount(), 1);
  assert.equal(stopped.mock.callCount(), 1);
  assert.equal(reads.length, 2);
  assert.equal(await h.journal.completedFor(autoModLease), true);
  assert.deepEqual(h.values.get(STAGING_USE_KEY), { ...autoModLease, phase: "released" });
  assert.deepEqual(h.values.get(OWNER_KEY), owner);
});
