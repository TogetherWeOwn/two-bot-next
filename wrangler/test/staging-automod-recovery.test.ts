import assert from "node:assert/strict";
import { test } from "node:test";
import {
  type AutomodIntent, StagingAutomodRecovery, STAGING_AUTOMOD_TARGET as target,
} from "../src/staging-automod-recovery.ts";
import { admission, created, intent, lease, now, observe, other, Registry, Storage } from "./fixtures/staging-automod-recovery.ts";

test("AutoMod recovery reloads the exact intent and receipt after restart", async () => {
  const store = new Storage();
  let recovery = new StagingAutomodRecovery(store);
  assert.equal(await recovery.pending(), undefined);
  await recovery.persistIntent(intent, observe, now);
  assert.deepEqual(await recovery.pending(), intent);
  await recovery.recordCreated({ intent, ...created });
  await recovery.recordCreated({ intent, ...created });

  recovery = new StagingAutomodRecovery(store);
  const registry = new Registry();
  registry.target = created;
  registry.mutateReturnedRevision = true;
  const pending = await recovery.pending();
  const nextLease = { ...lease, runAttempt: 3 };
  const startNext = () => recovery.persistIntent({ ...intent, lease: nextLease }, async () => ({ ...admission, lease: nextLease }), now);
  assert.deepEqual(pending, intent);
  await assert.rejects(startNext(), /intent_held/);
  await recovery.recover(pending!.lease, registry);
  assert.deepEqual(registry.calls, [created.id, "delete", created.id]);
  assert.deepEqual([...registry.sameGuild], [["help", other]]);
  assert.deepEqual([...registry.otherGuild], [[target.name, other]]);
  assert.equal(await recovery.pending(), undefined);
  await recovery.recover(lease, registry);
  assert.equal(registry.calls.length, 3, "completion replay does not contact the registry");
  await assert.rejects(recovery.persistIntent(intent, observe, now), /intent_held/);

  await startNext();
  await assert.rejects(recovery.recover(lease, registry), /recovery_mismatch/);
  assert.equal(registry.calls.length, 3);
});

test("AutoMod recovery fences publication on expiry or cancellation without renewing authority", async () => {
  for (const current of [undefined, { ...admission, lease: { ...lease, phase: "released" as const } },
    { ...admission, publishBefore: intent.publishBefore - 1 }]) {
    const store = new Storage();
    const recovery = new StagingAutomodRecovery(store);
    let at = now();
    let observed: { lease: AutomodIntent["lease"]; baselineAbsent: true; publishBefore: number } = admission;
    store.afterCommit = () => {
      at = current ? now() : lease.expiresAt;
      observed = current ?? admission;
    };
    await assert.rejects(recovery.persistIntent(intent, async () => observed, () => at),
      /admission_mismatch/);
    assert.deepEqual((store.value as { intent: AutomodIntent }).intent, intent);
    const registry = new Registry();
    registry.authorized = false;
    await assert.rejects(recovery.recover(lease, registry), /cleanup_unverified/);
    await assert.rejects(recovery.recordCreated({ intent, ...created }), /receipt_mismatch/);
    await assert.rejects(recovery.persistIntent(intent, observe, now), /intent_held/);
    registry.authorized = true;
    await new StagingAutomodRecovery(store).recover(lease, registry);
    assert.deepEqual(registry.calls, ["unknown", "unknown"]);
    assert.equal(await recovery.pending(), undefined);
  }
});

test("AutoMod recovery holds a lost create response until ownership or absence is proven", async () => {
  const store = new Storage();
  const recovery = new StagingAutomodRecovery(store);
  await recovery.persistIntent(intent, observe, now);
  const registry = new Registry();
  registry.drained = false;
  await assert.rejects(recovery.recover(lease, registry), /cleanup_unverified/);
  assert.deepEqual(await recovery.pending(), intent, "an in-flight POST might still land");
  registry.drained = true;
  registry.target = created;
  registry.attributable = false;
  await assert.rejects(recovery.recover(lease, registry), /cleanup_unverified/);
  assert.deepEqual(registry.calls, ["unknown", "unknown"]);
  assert.deepEqual(await recovery.pending(), intent, "a matching command may belong to another writer");

  registry.attributable = true;
  registry.loseDeleteResponse = true;
  registry.beforeDelete = command => assert.deepEqual((store.value as { created: unknown }).created, command);
  await assert.rejects(recovery.recover(lease, registry), /removal_unverified/);
  assert.deepEqual(registry.calls, ["unknown", "unknown", "unknown", "delete"]);
  const restarted = new StagingAutomodRecovery(store);
  assert.deepEqual(await restarted.pending(), intent);
  await restarted.recover(lease, registry);
  assert.deepEqual(registry.calls, ["unknown", "unknown", "unknown", "delete", created.id]);
  assert.equal(await restarted.pending(), undefined);
});

test("AutoMod recovery refuses a wrong scope, lease or command revision", async () => {
  const recovery = new StagingAutomodRecovery(new Storage());
  for (const changed of [
    { ...target, applicationId: other.id }, { ...target, guildId: other.id }, { ...target, name: "help" }, { ...target, type: 2 },
  ]) await assert.rejects(recovery.persistIntent({ ...intent, target: changed as typeof target }, observe, now), /intent_invalid/);
  const registry = new Registry();
  const wrongLease = { ...lease, epoch: lease.epoch + 1 };
  await assert.rejects(recovery.persistIntent(intent, async () => ({ ...admission, lease: wrongLease }), now), /admission_mismatch/);
  assert.equal(await recovery.pending(), undefined);
  await recovery.persistIntent(intent, observe, now);
  await assert.rejects(recovery.recordCreated({ ...created, intent: { ...intent, lease: wrongLease } }), /receipt_mismatch/);
  await assert.rejects(recovery.recordCreated({ ...created, intent: { ...intent, baseline: "c".repeat(64) } }), /receipt_mismatch/);
  await assert.rejects(recovery.recordCreated({ ...created, intent: { ...intent, publishBefore: intent.publishBefore - 1 } }), /receipt_mismatch/);
  await assert.rejects(recovery.recordCreated({ intent, ...created, id: "18446744073709551616" }), /receipt_mismatch/);
  await recovery.recordCreated({ intent, ...created });
  await assert.rejects(recovery.recordCreated({ intent, ...other }), /receipt_mismatch/);
  await assert.rejects(recovery.recover(wrongLease, registry), /recovery_mismatch/);
  assert.deepEqual(registry.calls, []);
  registry.proofIntent = { ...intent, target: { ...target, guildId: other.id } as typeof target };
  await assert.rejects(recovery.recover(lease, registry), /cleanup_unverified/);
  registry.proofIntent = intent;
  for (const command of [other, { ...created, version: other.version }]) {
    registry.target = command;
    await assert.rejects(recovery.recover(lease, registry), /cleanup_unverified/);
    assert.deepEqual(registry.target, command);
  }
  registry.target = created;
  registry.proofIntent = { ...intent, publishBefore: intent.publishBefore - 1 };
  await assert.rejects(recovery.recover(lease, registry), /cleanup_unverified/);
  assert.deepEqual(registry.calls, [created.id, created.id, created.id, created.id]);
  assert.deepEqual(await recovery.pending(), intent);
});

test("AutoMod recovery needs a durable hold and a stopped, affirmative absence read", async () => {
  const store = new Storage();
  const recovery = new StagingAutomodRecovery(store);
  const registry = new Registry();
  store.afterWrite = () => { store.beforeSync = () => { throw new Error("storage unavailable"); }; };
  await assert.rejects(recovery.persistIntent(intent, observe, now), /storage_unavailable/);
  await assert.rejects(recovery.recover(lease, registry), /storage_unavailable/);
  assert.deepEqual(registry.calls, []);
  delete store.afterWrite;
  delete store.beforeSync;
  await assert.rejects(recovery.persistIntent(intent, observe, now), /intent_held/);
  await recovery.recordCreated({ intent, ...created });
  registry.stopped = false;
  await assert.rejects(recovery.recover(lease, registry), /cleanup_unverified/);
  registry.stopped = true;
  registry.target = created;
  registry.keepAfterDelete = true;
  await assert.rejects(recovery.recover(lease, registry), /absence_unverified/);
  assert.deepEqual(registry.calls, [created.id, created.id, "delete", created.id]);
  assert.deepEqual(await recovery.pending(), intent);
  store.value = { ...store.value as object, phase: "unknown" };
  await assert.rejects(recovery.pending(), /storage_invalid/);
});
