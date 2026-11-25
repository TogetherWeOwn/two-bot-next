import assert from "node:assert/strict";
import { test } from "node:test";
import { bindStagingAutomodDeploymentTransport } from "../scripts/staging-automod-deployment-transport.ts";
import { runStagingAutomodPrivateJob } from "../scripts/staging-automod-private-job.ts";
import { STAGING_AUTOMOD_TARGET as target } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE } from "../src/staging-use-window.ts";
import { lease } from "./fixtures/staging-automod-recovery.ts";

type Inputs = Parameters<typeof bindStagingAutomodDeploymentTransport>;
type Read = Inputs[1];
type Invoke = Inputs[2];
const now = Date.parse("2026-01-01T00:02:00Z");
const issuedLease = { ...lease, deploymentId: "11111111-1111-1111-1111-111111111111",
  purpose: AUTOMOD_STAGING_PURPOSE, runAttempt: 1, acquiredAt: now - 1_000, expiresAt: now + 1_000 } as const;
const { runId, runAttempt, sha } = issuedLease;
const job = { runId, runAttempt, sha };
const approvals = {
  start: { action: "start", target, issuedLease },
  recover: { action: "recover", target, issuedLease },
} as const;
const controlBearer = "synthetic-automod-deployment-transport-token";
const worker = { controlBearer, env: { TWO_DEPLOY_ENV: "staging", STAGING_RUNTIME_POLICY: "bounded",
  DISCORD_APPLICATION_ID: target.applicationId, GUILD_ID: target.guildId, CONTAINER_RELEASE: sha,
  CF_VERSION_METADATA: { id: issuedLease.deploymentId }, OWNERSHIP_CONTROL_TOKEN: controlBearer } };
const inventory = {
  schema_version: 1, observed_at: "2026-01-01T00:01:59.123456+00:00",
  worker: { deployment_id: "22222222-2222-2222-2222-222222222222", version_id: issuedLease.deploymentId,
    source_sha: sha, traffic_percent: 100, runtime_policy: "bounded" },
  container: { application_id: "33333333-3333-3333-3333-333333333333", namespace_id: "a".repeat(32),
    rollout_id: "44444444-4444-4444-4444-444444444444", rollout_status: "completed", image_digest: `sha256:${"b".repeat(64)}` },
  ownership: { phase: "active", epoch: issuedLease.epoch },
} as const;

test("the attested serving deployment reaches the lazy invoker for each approved action", async t => {
  t.mock.timers.enable({ apis: ["Date"], now });
  const invoke = t.mock.fn<Invoke>(async (pin, call) => {
    assert.ok(Object.isFrozen(pin));
    if (call.action === "start") t.mock.timers.setTime(issuedLease.expiresAt + 1);
  });
  const read = t.mock.fn<Read>(async () => inventory);
  const transport = bindStagingAutomodDeploymentTransport(worker, read, invoke);
  assert.equal(read.mock.callCount(), 0);
  assert.equal(invoke.mock.callCount(), 0);

  assert.deepEqual(await runStagingAutomodPrivateJob(job, issuedLease, approvals, transport),
    { verdict: "passed", start: true, recover: true });
  assert.deepEqual(invoke.mock.calls.map(({ arguments: [, call] }) => call),
    [{ action: "start", issuedLease }, { action: "recover", issuedLease }]);
  const pin = { workerDeploymentId: inventory.worker.deployment_id, workerVersionId: issuedLease.deploymentId,
    sourceSha: sha, applicationId: inventory.container.application_id, namespaceId: inventory.container.namespace_id,
    rolloutId: inventory.container.rollout_id, imageDigest: inventory.container.image_digest };
  assert.deepEqual(invoke.mock.calls.map(({ arguments: [binding] }) => binding), [pin, pin]);
  assert.deepEqual(invoke.mock.calls.map(({ arguments: [, , signal] }) => signal),
    read.mock.calls.map(({ arguments: [signal] }) => signal));
  assert.notEqual(read.mock.calls[0]?.arguments[0], read.mock.calls[1]?.arguments[0]);
});

test("missing stale and mismatched serving inventory never reaches the private invoker", async t => {
  t.mock.timers.enable({ apis: ["Date"], now });
  const invoke = t.mock.fn<Invoke>(async () => {});
  const stale = { ...inventory, observed_at: new Date(now - 60_001).toISOString() };
  const future = { ...inventory, observed_at: new Date(now + 1).toISOString() };
  const wrongVersion = { ...inventory, worker: { ...inventory.worker, version_id: inventory.worker.deployment_id } };
  const wrongBuild = { ...inventory, worker: { ...inventory.worker, source_sha: "b".repeat(40) } };
  const wrongOwner = { ...inventory, ownership: { ...inventory.ownership, epoch: issuedLease.epoch + 1 } };
  const malformedRollout = { ...inventory, container: { ...inventory.container, image_digest: "latest" } };
  for (const observed of [undefined, stale, future, wrongVersion, wrongBuild, wrongOwner, malformedRollout]) {
    const read = t.mock.fn<Read>(async () => observed);
    const transport = bindStagingAutomodDeploymentTransport(worker, read, invoke);
    const result = await runStagingAutomodPrivateJob(job, issuedLease, approvals, transport);
    assert.deepEqual(result, { verdict: "failed", start: false, recover: false });
    assert.equal(read.mock.callCount(), 2);
  }
  assert.equal(invoke.mock.callCount(), 0);
});

test("each action still needs its own approval and Worker admission before inventory is read", async t => {
  t.mock.timers.enable({ apis: ["Date"], now });
  const invoke = t.mock.fn<Invoke>(async () => {});
  const read = t.mock.fn<Read>(async () => inventory);
  const transport = bindStagingAutomodDeploymentTransport(worker, read, invoke);

  assert.deepEqual(await runStagingAutomodPrivateJob({ ...job, runAttempt: 2 }, issuedLease, approvals, transport),
    { verdict: "failed", code: "lease_unconfirmed" });
  for (const unboundWorker of [{ ...worker, controlBearer: "wrong-synthetic-ownership-control-token" },
    { ...worker, env: { ...worker.env, CF_VERSION_METADATA: { id: inventory.worker.deployment_id } } }]) {
    assert.deepEqual(await runStagingAutomodPrivateJob(job, issuedLease, approvals,
      bindStagingAutomodDeploymentTransport(unboundWorker, read, invoke)),
    { verdict: "failed", start: false, recover: false });
  }
  assert.equal(read.mock.callCount(), 0);
  assert.equal(invoke.mock.callCount(), 0);

  assert.deepEqual(await runStagingAutomodPrivateJob(job, issuedLease, { start: approvals.recover, recover: approvals.recover }, transport),
    { verdict: "failed", start: false, recover: true });
  assert.equal(read.mock.callCount(), 1);
  assert.deepEqual(invoke.mock.calls.map(({ arguments: [, call] }) => call), [{ action: "recover", issuedLease }]);
});

test("caller cancellation keeps a late serving observation from resolving the private invoker", async t => {
  t.mock.timers.enable({ apis: ["Date"], now });
  const controller = new AbortController();
  const entered = Promise.withResolvers<AbortSignal>();
  const pending = Promise.withResolvers<typeof inventory>();
  const read = t.mock.fn<Read>(signal => { entered.resolve(signal); return pending.promise; });
  const invoke = t.mock.fn<Invoke>(async () => {});
  const transport = bindStagingAutomodDeploymentTransport(worker, read, invoke);
  const call = { action: "start", issuedLease } as const;
  const run = transport(call, approvals.start, controller.signal);
  try {
    assert.equal(await Promise.race([entered.promise, run.then(() => undefined)]), controller.signal);
    controller.abort();
    assert.equal((await (await run).json() as { code: string }).code, "operation_unconfirmed");
    await assert.rejects(transport(call, approvals.start, controller.signal), { name: "AbortError" });
  } finally {
    pending.resolve(inventory);
    await pending.promise;
  }
  assert.equal(read.mock.callCount(), 1);
  assert.equal(invoke.mock.callCount(), 0);
});
