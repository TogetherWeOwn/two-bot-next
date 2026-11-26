import assert from "node:assert/strict";
import { test } from "node:test";
import { bindStagingAutomodDeploymentTransport } from "../scripts/staging-automod-deployment-transport.ts";
import { bindStagingAutomodInventoryReader } from "../scripts/staging-automod-inventory-reader.ts";
import { bindStagingAutomodInventoryVerifier } from "../scripts/staging-automod-inventory-verifier.ts";
import { runStagingAutomodPrivateJob } from "../scripts/staging-automod-private-job.ts";
import { STAGING_AUTOMOD_TARGET as target } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE } from "../src/staging-use-window.ts";
import { lease } from "./fixtures/staging-automod-recovery.ts";
import { syntheticVerifier, verifierWithDescendant } from "./fixtures/staging-automod-verifier.ts";

type Inputs = Parameters<typeof bindStagingAutomodDeploymentTransport>;
type Read = Inputs[1];
type Invoke = Inputs[2];
type Inventory = Awaited<ReturnType<Read>>;
const now = Date.parse("2026-01-01T00:00:30Z");
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
  schema_version: 1, observed_at: "2026-01-01T00:00:29.123456+00:00",
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
  assert.equal(read.mock.callCount(), 1);
  assert.equal(invoke.mock.calls[0]?.arguments[2], read.mock.calls[0]?.arguments[0]);
  assert.notEqual(invoke.mock.calls[0]?.arguments[2], invoke.mock.calls[1]?.arguments[2]);
});

test("recovery keeps the startup pin after the observation ages out and the rollout changes", async t => {
  t.mock.timers.enable({ apis: ["Date"], now });
  let observed: NonNullable<Inventory> = { ...inventory, observed_at: new Date(now - 50_000).toISOString() };
  const read = t.mock.fn<Read>(async () => observed);
  const invoke = t.mock.fn<Invoke>(async (_pin, call) => {
    if (call.action === "start") {
      t.mock.timers.setTime(now + 12_000);
      observed = { ...inventory, container: { ...inventory.container,
        rollout_id: "55555555-5555-5555-5555-555555555555" } };
      throw new Error("synthetic startup response lost");
    }
  });
  const transport = bindStagingAutomodDeploymentTransport(worker, read, invoke);

  assert.deepEqual(await runStagingAutomodPrivateJob(job, issuedLease, approvals, transport),
    { verdict: "failed", start: false, recover: true });
  assert.equal(read.mock.callCount(), 1);
  assert.deepEqual(invoke.mock.calls.map(({ arguments: [, call] }) => call),
    [{ action: "start", issuedLease }, { action: "recover", issuedLease }]);
  assert.equal(invoke.mock.calls[1]?.arguments[0], invoke.mock.calls[0]?.arguments[0]);
  assert.equal(invoke.mock.calls[1]?.arguments[0].rolloutId, inventory.container.rollout_id);

  const otherLease = { ...issuedLease, runId: "99999" };
  const otherCall = { action: "recover", issuedLease: otherLease } as const;
  const refused = await transport(otherCall, { ...otherCall, target }, new AbortController().signal);
  assert.equal((await refused.json() as { code: string }).code, "operation_unconfirmed");
  assert.equal(invoke.mock.callCount(), 2);
});

test("a second startup cannot use an aged out cached pin while its lease is still active", async t => {
  t.mock.timers.enable({ apis: ["Date"], now });
  const call = { action: "start", issuedLease: { ...issuedLease, expiresAt: now + 30_000 } } as const;
  const approval = { ...call, target };
  const read = t.mock.fn<Read>(async () => ({ ...inventory, observed_at: new Date(now - 50_000).toISOString() }));
  const invoke = t.mock.fn<Invoke>(async () => {});
  const transport = bindStagingAutomodDeploymentTransport(worker, read, invoke);

  const first = await transport(call, approval, new AbortController().signal);
  assert.equal((await first.json() as { verdict: string }).verdict, "passed");
  t.mock.timers.setTime(now + 12_000);
  const second = await transport(call, approval, new AbortController().signal);
  assert.equal((await second.json() as { code: string }).code, "operation_unconfirmed");
  assert.equal(read.mock.callCount(), 1);
  assert.equal(invoke.mock.callCount(), 1);
});

test("missing stale and mismatched serving inventory never reaches the private invoker", async t => {
  t.mock.timers.enable({ apis: ["Date"], now });
  const invoke = t.mock.fn<Invoke>(async () => {});
  const cases = {
    missing: undefined,
    stale: { ...inventory, observed_at: new Date(now - 60_001).toISOString() },
    future: { ...inventory, observed_at: new Date(now + 1).toISOString() },
    "wrong version": { ...inventory, worker: { ...inventory.worker, version_id: inventory.worker.deployment_id } },
    "wrong build": { ...inventory, worker: { ...inventory.worker, source_sha: "b".repeat(40) } },
    "wrong epoch": { ...inventory, ownership: { ...inventory.ownership, epoch: issuedLease.epoch + 1 } },
    "wrong schema": { ...inventory, schema_version: 2 },
    "split serving traffic": { ...inventory, worker: { ...inventory.worker, traffic_percent: 95 } },
    "wrong runtime policy": { ...inventory, worker: { ...inventory.worker, runtime_policy: "continuous" } },
    "inactive owner": { ...inventory, ownership: { ...inventory.ownership, phase: "draining" } },
    "uncompleted rollout": { ...inventory, container: { ...inventory.container, rollout_status: "pending" } },
    "mutable digest": { ...inventory, container: { ...inventory.container, image_digest: "latest" } },
    "invalid deployment ID": { ...inventory, worker: { ...inventory.worker, deployment_id: "active" } },
    "invalid application ID": { ...inventory, container: { ...inventory.container, application_id: "current" } },
    "invalid namespace ID": { ...inventory, container: { ...inventory.container, namespace_id: [inventory.container.namespace_id] } },
    "invalid rollout ID": { ...inventory, container: { ...inventory.container, rollout_id: "current" } },
    "non-UTC observation": { ...inventory, observed_at: "2026-01-01T01:00:29+01:00" },
    "normalized date": { ...inventory, observed_at: "2025-12-31T24:00:00Z" },
  };
  for (const [label, observed] of Object.entries(cases)) {
    const read = t.mock.fn<Read>(async () => observed as Inventory);
    const transport = bindStagingAutomodDeploymentTransport(worker, read, invoke);
    const result = await runStagingAutomodPrivateJob(job, issuedLease, approvals, transport);
    assert.deepEqual(result, { verdict: "failed", start: false, recover: false }, label);
    assert.equal(read.mock.callCount(), 2, label);
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

test("a timed out inventory verifier drains before recovery can read or invoke", { timeout: 10_000 }, async t => {
  t.mock.timers.enable({ apis: ["Date", "setTimeout"], now });
  const network = t.mock.method(globalThis, "fetch", async () => assert.fail("unexpected network access"));
  const source = { workflowSha: "c".repeat(40), runId: 1234 };
  const receipt = JSON.stringify({ ...inventory, workflow_sha: source.workflowSha, workflow_run_id: source.runId });
  let descendant = 0;
  let closed = false;
  let calls = 0;
  const checkDrained = () => {
    assert.equal(closed, true);
    assert.ok(descendant > 0);
    assert.throws(() => process.kill(descendant, 0), { code: "ESRCH" });
  };
  const run = bindStagingAutomodInventoryVerifier((file, args, options) => {
    calls += 1;
    if (calls === 1) {
      return syntheticVerifier(verifierWithDescendant(receipt), child => {
        child.stderr.once("data", chunk => { descendant = Number(chunk); t.mock.timers.tick(15_000); });
        child.once("close", () => { closed = true; });
      })(file, args, options);
    }
    checkDrained();
    return syntheticVerifier(`process.stdout.write(${JSON.stringify(receipt)});`)(file, args, options);
  });
  const read = bindStagingAutomodInventoryReader(run);
  const invoke = t.mock.fn<Invoke>(async (_, call) => {
    checkDrained();
    assert.equal(call.action, "recover");
  });
  const transport = bindStagingAutomodDeploymentTransport(worker, signal => read(source, signal), invoke);
  assert.deepEqual(await runStagingAutomodPrivateJob(job, issuedLease, approvals, transport),
    { verdict: "failed", start: false, recover: true });
  checkDrained();
  assert.equal(calls, 2);
  assert.equal(invoke.mock.callCount(), 1);
  assert.equal(network.mock.callCount(), 0);
});
