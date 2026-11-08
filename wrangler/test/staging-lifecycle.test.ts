import { test } from "node:test";
import assert from "node:assert/strict";
import {
  StagingLifecycle, STAGING_USE_KEY, MAX_STAGING_USE_MS, parseStagingUseChange,
  type StagingUseChange, type StagingIdentity,
} from "../src/staging-lifecycle.ts";

const SHA = "a".repeat(40);
const identity: StagingIdentity = {
  deploymentId: "deployment-A", sha: SHA,
  owner: { deploymentId: "deployment-A", epoch: 1, phase: "active", actor: "fixture",
    timestamp: "2026-11-07T00:00:00Z", oldEpoch: 0, oldDeploymentId: null },
};

function fixture() {
  const values = new Map<string, unknown>();
  const storage = {
    get: async (key: string) => values.get(key),
    put: async (items: Record<string, unknown>) => {
      for (const [key, value] of Object.entries(items)) values.set(key, structuredClone(value));
    },
  } as unknown as DurableObjectStorage;
  const lifecycle = new StagingLifecycle(storage);
  const expiresAt = Date.now() + 60_000;
  const change: StagingUseChange = { action: "acquire", environment: "staging", runId: "123456", runAttempt: 1,
    expiresAt, deploymentId: identity.deploymentId, sha: SHA, expectedEpoch: 1 };
  let stops = 0;
  const deadlines: number[] = [];
  const stop = async () => { stops++; };
  const schedule = async (at: number) => { deadlines.push(at); };
  return { lifecycle, values, storage, change, stop, schedule, deadlines, stops: () => stops };
}

test("staging use: strict environment, identity, attempt, epoch and expiry input shapes", () => {
  const { change } = fixture();
  assert.deepEqual(parseStagingUseChange(change), change);
  for (const value of [null, [], {}, "acquire"]) assert.equal(parseStagingUseChange(value), null);
  for (const patch of [
    { environment: "production" }, { environment: "STAGING" }, { action: "renew" },
    { runId: 123 }, { runId: "01" }, { runId: "1/2" }, { runId: "1".repeat(21) },
    { runAttempt: 0 }, { runAttempt: "1" }, { runAttempt: 1.5 }, { runAttempt: Infinity },
    { expectedEpoch: 0 }, { expectedEpoch: -1 }, { expectedEpoch: "1" },
    { deploymentId: "../deployment-A" }, { sha: "a".repeat(39) }, { sha: "A".repeat(40) },
    { expiresAt: "2026-11-07T01:00:00Z" }, { expiresAt: NaN }, { expiresAt: -1 },
  ]) assert.equal(parseStagingUseChange({ ...change, ...patch }), null, JSON.stringify(patch));
});

test("staging use: cold state denies; acquisition persists stop and expiry before active", async () => {
  const h = fixture();
  await assert.rejects(h.lifecycle.require(identity), /staging_use_required/);
  const order: string[] = [];
  const grant = await h.lifecycle.acquire(h.change, identity, async () => {
    assert.equal((await h.lifecycle.read())?.phase, "preparing");
    await assert.rejects(h.lifecycle.require(identity), /staging_use_required/);
    order.push("stop");
  }, async (at) => {
    assert.equal((await h.lifecycle.read())?.phase, "preparing");
    assert.equal(at, h.change.expiresAt);
    order.push("expiry");
  });
  assert.deepEqual(order, ["stop", "expiry"]);
  assert.equal(grant.phase, "active");
  assert.deepEqual(await h.lifecycle.require(identity), grant);
  assert.equal(grant.environment, "staging");
  assert.equal(grant.epoch, identity.owner.epoch);
  assert.equal(grant.sha, SHA);
});

test("staging use: idempotent acquire cannot extend a window or steal another attempt", async () => {
  const h = fixture();
  const first = await h.lifecycle.acquire(h.change, identity, h.stop, h.schedule);
  assert.deepEqual(await h.lifecycle.acquire(h.change, identity, h.stop, h.schedule), first);
  assert.equal(h.stops(), 1);
  for (const patch of [{ expiresAt: h.change.expiresAt + 1 }, { runAttempt: 2 }, { runId: "999" }]) {
    await assert.rejects(h.lifecycle.acquire({ ...h.change, ...patch }, identity, h.stop, h.schedule), /staging_use_conflict/);
  }
  assert.deepEqual(await h.lifecycle.read(), first);
  assert.equal(h.stops(), 1);
});

test("staging use: finite deadline, observed expiry and future-acquired state fail closed", async () => {
  const h = fixture();
  for (const expiresAt of [Date.now() - 1, Date.now(), Date.now() + MAX_STAGING_USE_MS + 60_000]) {
    await assert.rejects(h.lifecycle.acquire({ ...h.change, expiresAt }, identity, h.stop, h.schedule), /staging_expiry_invalid/);
  }
  const grant = await h.lifecycle.acquire(h.change, identity, h.stop, h.schedule);
  await assert.rejects(h.lifecycle.require(identity, grant.expiresAt), /staging_use_required/);
  await assert.rejects(h.lifecycle.require(identity, grant.acquiredAt - 1), /staging_use_required/);
  assert.equal(h.stops(), 1);
});

test("staging use: rebinding through takeover/rollback preserves the original run and deadline", async () => {
  const h = fixture();
  const first = await h.lifecycle.acquire(h.change, identity, h.stop, h.schedule);
  const replacement = { ...identity, deploymentId: "deployment-B", sha: "b".repeat(40),
    owner: { ...identity.owner, deploymentId: "deployment-B", epoch: 2, oldEpoch: 1 } };
  await assert.rejects(h.lifecycle.require(replacement), /staging_use_required/);
  const rebound = await h.lifecycle.acquire({ ...h.change, deploymentId: replacement.deploymentId,
    sha: replacement.sha, expectedEpoch: 2 }, replacement, h.stop, h.schedule);
  assert.equal(rebound.expiresAt, first.expiresAt);
  assert.equal(rebound.acquiredAt, first.acquiredAt);
  assert.equal(h.stops(), 2);
  assert.deepEqual(await h.lifecycle.require(replacement), rebound);
  await assert.rejects(h.lifecycle.require(identity), /staging_use_required/);
});

test("staging use: lost runner state reconstructs from durable storage, with no renewal", async () => {
  const h = fixture();
  const original = await h.lifecycle.acquire(h.change, identity, h.stop, h.schedule);
  const reloaded = new StagingLifecycle(h.storage);
  assert.deepEqual(await reloaded.require(identity), original);
  await assert.rejects(reloaded.require(identity, original.expiresAt + 1), /staging_use_required/);
  assert.deepEqual(await reloaded.read(), original);
});

test("staging use: idempotent release persists denial before stop and prevents delayed reacquire", async () => {
  const h = fixture();
  await h.lifecycle.acquire(h.change, identity, h.stop, h.schedule);
  const release = { ...h.change, action: "release" as const };
  const stopped = async () => {
    assert.equal((await h.lifecycle.read())?.phase, "released");
    await assert.rejects(h.lifecycle.require(identity), /staging_use_required/);
  };
  const first = await h.lifecycle.release(release, identity, stopped);
  assert.deepEqual(await h.lifecycle.release(release, identity, stopped), first);
  await assert.rejects(h.lifecycle.acquire(h.change, identity, h.stop, h.schedule), /staging_use_ended/);
  await h.lifecycle.acquire({ ...h.change, runId: "999" }, identity, h.stop, h.schedule);
  await assert.rejects(h.lifecycle.release(release, identity, h.stop), /staging_use_conflict/);
  await assert.rejects(h.lifecycle.acquire(h.change, identity, h.stop, h.schedule), /staging_use_conflict/);
  await h.lifecycle.release({ ...release, runId: "999" }, identity, stopped);
  await assert.rejects(h.lifecycle.acquire(h.change, identity, h.stop, h.schedule), /staging_use_ended/);
});

for (const failure of ["stop", "schedule", "commit"] as const) {
  test(`staging use: ${failure} failure cannot publish an active grant; exact retry recovers`, async () => {
    const h = fixture();
    const put = h.storage.put.bind(h.storage);
    let failCommit = failure === "commit";
    h.storage.put = (async (items: Record<string, unknown>) => {
      if (failCommit && (items[STAGING_USE_KEY] as { phase: string }).phase === "active") throw new Error("synthetic failure");
      return put(items);
    }) as DurableObjectStorage["put"];
    await assert.rejects(h.lifecycle.acquire(h.change, identity,
      async () => { if (failure === "stop") throw new Error("stop failed"); },
      async () => { if (failure === "schedule") throw new Error("schedule failed"); }));
    assert.equal((await h.lifecycle.read())?.phase, "preparing");
    await assert.rejects(h.lifecycle.require(identity), /staging_use_required/);
    failCommit = false;
    assert.equal((await h.lifecycle.acquire(h.change, identity, h.stop, h.schedule)).phase, "active");
  });
}

test("staging use: failed release teardown remains denied after reconstruction", async () => {
  const h = fixture();
  await h.lifecycle.acquire(h.change, identity, h.stop, h.schedule);
  const release = { ...h.change, action: "release" as const };
  await assert.rejects(h.lifecycle.release(release, identity, async () => { throw new Error("stop failed"); }));
  const reloaded = new StagingLifecycle(h.storage);
  assert.equal((await reloaded.read())?.phase, "released");
  await assert.rejects(reloaded.require(identity), /staging_use_required/);
  await reloaded.release(release, identity, h.stop);
});

test("staging use: ownership/SHA mismatch cannot create or release authorization", async () => {
  const h = fixture();
  for (const patch of [{ expectedEpoch: 2 }, { deploymentId: "deployment-B" }, { sha: "b".repeat(40) }]) {
    await assert.rejects(h.lifecycle.acquire({ ...h.change, ...patch }, identity, h.stop, h.schedule));
  }
  assert.equal(await h.lifecycle.read(), undefined);
  assert.equal(h.stops(), 0);
});

test("staging use: corrupt or unavailable storage never grants authority", async () => {
  const h = fixture();
  const grant = await h.lifecycle.acquire(h.change, identity, h.stop, h.schedule);
  for (const value of [null, {}, { ...grant, environment: "production" }, { ...grant, expiresAt: grant.acquiredAt },
    { ...grant, expiresAt: grant.acquiredAt + MAX_STAGING_USE_MS + 1 }, { ...grant, phase: "unknown" }]) {
    h.values.set(STAGING_USE_KEY, value);
    await assert.rejects(h.lifecycle.require(identity), /staging_storage_invalid/);
  }
  h.storage.get = async () => { throw new Error("synthetic storage error"); };
  await assert.rejects(h.lifecycle.require(identity), /staging_storage_unavailable/);
});
