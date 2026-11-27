import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { test } from "node:test";
import { runProtectedAutomod } from "../src/staging-automod-caller.ts";
import { createProtectedAutomodEffects } from "../src/staging-automod-effects-composer.ts";
import { STAGING_AUTOMOD_PLAN, type StagingAutomodPlan } from "../src/staging-automod-plan.ts";
import { STAGING_AUTOMOD_TARGET, StagingAutomodRecovery } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE, type StagingUse } from "../src/staging-lifecycle.ts";
import { admission, intent, lease as fixtureLease, Storage } from "./fixtures/staging-automod-recovery.ts";
import publication from "./fixtures/staging-automod-publication.json" with { type: "json" };

type Dependencies = Parameters<typeof createProtectedAutomodEffects>[0];
type Grant = NonNullable<Awaited<ReturnType<Dependencies["readAuthenticatedCleanupGrant"]>>>;
type Authority = Parameters<typeof runProtectedAutomod>[1];

const lease = Object.freeze({ ...fixtureLease, purpose: AUTOMOD_STAGING_PURPOSE });
const policy = Object.freeze({
  memberIntake: false, enforcement: false, resume: false, registrySync: false,
  interactionDispatch: false, dedicatedIntents: true,
});
const plan = {
  ...STAGING_AUTOMOD_PLAN, enabled: true, allowStart: true, notAfter: 70_000, cleanupMs: 10_000,
} satisfies StagingAutomodPlan;
const cleanupBefore = 70_000;
const noEffect = async () => assert.fail("unexpected effect");

function fixture() {
  let at = 2_000;
  let grant: Grant | undefined = { lease: { ...lease }, before: cleanupBefore };
  const grantsRead: Readonly<StagingUse>[] = [];
  const deps: Dependencies = {
    now: () => at,
    transport: {
      plannedPolicy: policy, launch: noEffect, readRuntime: noEffect, fenceAndDrainRemote: noEffect, stop: noEffect,
    },
    request: noEffect,
    readAuthenticatedCleanupGrant: async issued => {
      grantsRead.push(issued);
      return grant;
    },
  };
  return {
    deps, grantsRead,
    setTime: (value: number) => { at = value; },
    setGrant: (value: Grant | undefined) => { grant = value; },
  };
}

test("the frozen default effects omit startup", async () => {
  const { deps, grantsRead, setTime } = fixture();
  const effects = createProtectedAutomodEffects(deps);
  assert.equal(STAGING_AUTOMOD_PLAN.enabled, false);
  assert.ok(Object.isFrozen(effects));
  assert.equal(effects.enabled, false);
  assert.equal(Object.hasOwn(effects, "dryRun"), false);
  setTime(lease.expiresAt + 1);
  assert.deepEqual(await effects.readCleanupGrant(lease), { lease, before: cleanupBefore });
  assert.deepEqual(grantsRead, [lease]);
});

test("startup requires the exact configured target and an open plan window", async () => {
  const { deps, grantsRead } = fixture();
  const effects = createProtectedAutomodEffects({ ...deps, plan });
  assert.equal(effects.enabled, true);
  assert.deepEqual(effects.dryRun?.policy, policy);
  assert.ok(Object.isFrozen(effects.dryRun));

  for (const overrides of [{ enabled: false }, { allowStart: false }, { notAfter: 2_000 }, { notAfter: Infinity }]) {
    const effects = createProtectedAutomodEffects({ ...deps, plan: { ...plan, ...overrides } });
    assert.equal(effects.enabled, false);
    assert.equal(Object.hasOwn(effects, "dryRun"), false);
  }
  for (const field of Object.keys(STAGING_AUTOMOD_TARGET)) {
    const target = { ...STAGING_AUTOMOD_TARGET, [field]: "wrong" };
    const effects = createProtectedAutomodEffects({ ...deps, plan: { ...plan, target } });
    assert.equal(effects.enabled, false);
    assert.equal(Object.hasOwn(effects, "dryRun"), false);
    assert.equal(await effects.readCleanupGrant(lease), undefined);
  }
  assert.deepEqual(grantsRead, []);
});

test("startup uses the shorter plan deadline after composition", async () => {
  const { deps, setTime } = fixture();
  const cutoffs: number[] = [];
  deps.transport.launch = async (_lease, _policy, before) => {
    cutoffs.push(before);
    return true;
  };
  deps.transport.readRuntime = async issued => ({ lease: issued, running: true, policy });
  const effects = createProtectedAutomodEffects({ ...deps, plan: { ...plan, notAfter: 3_000 } });
  assert.ok(effects.dryRun);
  await effects.dryRun.start(lease, policy, intent.publishBefore);
  setTime(3_000);
  await assert.rejects(effects.dryRun.start(lease, policy, intent.publishBefore), /unverified/);
  assert.deepEqual(cutoffs, [3_000]);
});

test("cleanup rejects invalid plan durations and leases before reading a grant", async () => {
  const { deps, grantsRead } = fixture();
  for (const cleanupMs of [0, -1, 1.5, NaN, Infinity, Number.MAX_SAFE_INTEGER, Number.MAX_SAFE_INTEGER + 1]) {
    const effects = createProtectedAutomodEffects({ ...deps, plan: { ...plan, cleanupMs } });
    assert.equal(await effects.readCleanupGrant(lease), undefined);
  }
  const effects = createProtectedAutomodEffects({ ...deps, plan });
  for (const bad of [{ ...lease, purpose: undefined }, { ...lease, runId: "0" }, { ...lease, voiceMove: true }]) {
    assert.equal(await effects.readCleanupGrant(bad as StagingUse), undefined);
  }
  assert.deepEqual(grantsRead, []);
});

test("cleanup accepts only a fresh authenticated grant bounded by both deadlines", async () => {
  const { deps, setTime, setGrant } = fixture();
  setTime(lease.expiresAt + 1);
  const effects = createProtectedAutomodEffects({ ...deps, plan });
  assert.deepEqual(await effects.readCleanupGrant(lease), { lease, before: cleanupBefore });
  for (const grant of [
    undefined, { lease: { ...lease, epoch: lease.epoch + 1 }, before: cleanupBefore },
    { lease: fixtureLease, before: cleanupBefore },
    ...[lease.expiresAt, lease.expiresAt + 1, cleanupBefore + 1, 69_000.5, NaN, Infinity].map(before => ({ lease, before })),
  ]) {
    setGrant(grant);
    assert.equal(await effects.readCleanupGrant(lease), undefined);
  }

  const shortPlan = createProtectedAutomodEffects({ ...deps, plan: { ...plan, notAfter: 58_000 } });
  assert.equal(shortPlan.enabled, false);
  setGrant({ lease, before: 68_001 });
  assert.equal(await shortPlan.readCleanupGrant(lease), undefined);
  setGrant({ lease, before: 68_000 });
  assert.deepEqual(await shortPlan.readCleanupGrant(lease), { lease, before: 68_000 });
});

test("cleanup fails closed on provider failure or a grant that expires during the read", async () => {
  const { deps, setTime } = fixture();
  const unavailable = createProtectedAutomodEffects({ ...deps, plan, readAuthenticatedCleanupGrant: noEffect });
  assert.equal(await unavailable.readCleanupGrant(lease), undefined);
  const late = createProtectedAutomodEffects({ ...deps, plan, readAuthenticatedCleanupGrant: async () => {
    setTime(cleanupBefore);
    return { lease, before: cleanupBefore };
  } });
  assert.equal(await late.readCleanupGrant(lease), undefined);
});

test("the protected caller starts once and recovers the expired lease with startup disabled", async t => {
  t.mock.timers.enable({ apis: ["Date"], now: 2_000 });
  const storage = new Storage();
  const journal = new StagingAutomodRecovery(storage);
  const pending = { ...intent, lease, baseline: createHash("sha256").update("[]").digest("hex") };
  const { env, identity, replies: { preview } } = publication;
  const command = { id: preview.id, version: preview.version, intent: pending };
  await journal.persistIntent(pending, async () => ({ ...admission, lease }));
  await journal.recordCreated(command);
  await journal.confirmPublication(command);

  const events: string[] = [];
  let running = false;
  let published = true;
  const { deploymentId, sha, epoch } = lease;
  const binding = {
    deploymentId, sha, owner: {
      deploymentId, epoch, phase: "active", actor: "fixture", timestamp: new Date().toISOString(),
      oldEpoch: epoch - 1, oldDeploymentId: null,
    },
  } as const;
  const staging = {
    automodBinding: async (expected, observed, at) => {
      assert.deepEqual(expected, lease);
      assert.deepEqual(observed, binding);
      return { lease, active: at < lease.expiresAt };
    },
    releaseAutomod: async (expected, observed) => {
      assert.deepEqual(expected, lease);
      assert.deepEqual(observed, binding);
      assert.equal(await journal.completedFor(lease), true);
      events.push("release");
    },
  } satisfies Pick<Authority["staging"], "automodBinding" | "releaseAutomod">;
  const authority: Authority = {
    storage, env: { ...env, STAGING_RUNTIME_POLICY: "bounded" },
    staging: staging as Authority["staging"], identity: async () => binding,
    exclusive: operation => operation(),
    confirmStoppedBeforeRelease: async () => assert.equal(running, false),
  };
  const guild = `https://discord.com/api/v10/applications/${env.DISCORD_APPLICATION_ID}/guilds/${env.GUILD_ID}/commands`;
  const deps: Dependencies = {
    plan,
    transport: {
      plannedPolicy: policy,
      launch: async (issued, actualPolicy, before) => {
        assert.deepEqual(issued, lease);
        assert.deepEqual(actualPolicy, policy);
        assert.equal(before, pending.publishBefore);
        events.push("launch");
        running = true;
        return true;
      },
      readRuntime: async issued => ({ lease: issued, running, policy }),
      fenceAndDrainRemote: async (issued, before) => {
        assert.deepEqual(issued, lease);
        assert.equal(before, cleanupBefore);
        events.push("drain");
        return { lease: issued, publicationDrained: true, startupDrained: true };
      },
      stop: async (issued, before) => {
        assert.deepEqual(issued, lease);
        assert.equal(before, cleanupBefore);
        events.push("stop");
        running = false;
        return true;
      },
    },
    readAuthenticatedCleanupGrant: async issued => ({ lease: issued, before: cleanupBefore }),
    request: async (url, init) => {
      assert.equal(running, false);
      assert.equal(init.signal?.aborted, false);
      if (init.method === "DELETE") {
        assert.equal(url, `${guild}/${preview.id}`);
        assert.equal(published, true);
        published = false;
        events.push("delete");
        return new Response(null, { status: 204 });
      }
      assert.equal(init.method, "GET");
      if (url === "https://discord.com/api/v10/users/@me") return Response.json(identity);
      if (url === `${guild}?with_localizations=true`) return Response.json(published ? [preview] : []);
      assert.equal(url, `${guild}/${preview.id}`);
      return published ? Response.json(preview) : Response.json({ code: 10063 }, { status: 404 });
    },
  };
  const start = { action: "start", issuedLease: lease } as const;
  const recover = { action: "recover", issuedLease: lease } as const;
  const allowed = createProtectedAutomodEffects(deps);
  const dormant = () => createProtectedAutomodEffects({ ...deps, plan: { ...plan, allowStart: false } });
  await assert.rejects(runProtectedAutomod(start, authority, dormant()));
  assert.deepEqual(events, []);
  await runProtectedAutomod(start, authority, allowed);
  await assert.rejects(runProtectedAutomod(start, authority, allowed));
  assert.deepEqual(events, ["launch"]);

  t.mock.timers.setTime(lease.expiresAt + 1);
  const effects = dormant();
  assert.equal(effects.enabled, false);
  assert.equal(Object.hasOwn(effects, "dryRun"), false);
  await runProtectedAutomod(recover, authority, effects);
  assert.deepEqual(events, ["launch", "drain", "stop", "delete", "release"]);
  assert.equal(await journal.completedFor(lease), true);
});
