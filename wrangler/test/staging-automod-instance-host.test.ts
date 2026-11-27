import assert from "node:assert/strict";
import { test, type TestContext } from "node:test";
import { runStagingAutomodInstanceHost } from "../scripts/staging-automod-instance-host.ts";
import { STAGING_AUTOMOD_TARGET as target } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE } from "../src/staging-lifecycle.ts";
import { lease } from "./fixtures/staging-automod-recovery.ts";

type Wiring = NonNullable<Parameters<typeof runStagingAutomodInstanceHost>[1]>;
type Inventory = NonNullable<Awaited<ReturnType<Wiring["readAuthenticatedInventory"]>>>;
type Worker = ReturnType<Wiring["readWorker"]>;

const now = Date.parse("2026-01-01T00:00:30Z");
const pin = {
  workerVersionId: "11111111-1111-1111-1111-111111111111",
  workerDeploymentId: "22222222-2222-2222-2222-222222222222",
  sourceSha: lease.sha,
  applicationId: "33333333-3333-3333-3333-333333333333",
  namespaceId: "d".repeat(32),
  rolloutId: "44444444-4444-4444-4444-444444444444",
  imageDigest: `sha256:${"b".repeat(64)}`,
};
const serving = { deploymentId: pin.workerVersionId, sha: pin.sourceSha };
const issuedLease = { ...lease, ...serving, runAttempt: 1, purpose: AUTOMOD_STAGING_PURPOSE,
  acquiredAt: now - 1_000, expiresAt: now + 10_000 } as const;
const change = { ...serving, action: "acquire", environment: "staging", runId: issuedLease.runId,
  runAttempt: 1, expectedEpoch: issuedLease.epoch, expiresAt: issuedLease.expiresAt } as const;
const path = ".github/workflows/staging-automod-preview.yml";
const head = "e".repeat(40);
const repository = { id: 1396224242 };
const runner = { GITHUB_REPOSITORY_ID: String(repository.id), GITHUB_WORKFLOW_REF: `TogetherWeOwn/two-bot-next/${path}@refs/heads/main`,
  GITHUB_REF: "refs/heads/main", GITHUB_REF_PROTECTED: "true", GITHUB_EVENT_NAME: "workflow_dispatch",
  GITHUB_RUN_ID: change.runId, GITHUB_RUN_ATTEMPT: "1", GITHUB_SHA: head, GITHUB_WORKFLOW_SHA: head };
const run = { id: Number(change.runId), event: "workflow_dispatch", run_attempt: 1, status: "in_progress",
  path, head_branch: "main", head_sha: head, repository, head_repository: repository };
const inventorySource = { workflowSha: "c".repeat(40), runId: run.id + 1 };
const options = { enabled: true, runner, serving, inventorySource };
const unconfirmed = { verdict: "failed", code: "context_unconfirmed" };
const inventory: Inventory = {
  schema_version: 1, workflow_sha: inventorySource.workflowSha, workflow_run_id: inventorySource.runId,
  observed_at: new Date(now).toISOString(),
  worker: { deployment_id: pin.workerDeploymentId, version_id: pin.workerVersionId, source_sha: pin.sourceSha,
    traffic_percent: 100, runtime_policy: "bounded" },
  container: { application_id: pin.applicationId, namespace_id: pin.namespaceId, rollout_id: pin.rolloutId,
    image_digest: pin.imageDigest, rollout_status: "completed" },
  ownership: { phase: "active", epoch: change.expectedEpoch },
};
const controlBearer = "synthetic-automod-host-control-value";
const worker: Worker = { controlBearer, env: { TWO_DEPLOY_ENV: "staging", STAGING_RUNTIME_POLICY: "bounded",
  DISCORD_APPLICATION_ID: target.applicationId, GUILD_ID: target.guildId, CONTAINER_RELEASE: serving.sha,
  CF_VERSION_METADATA: { id: serving.deploymentId }, OWNERSHIP_CONTROL_TOKEN: controlBearer } };

function fixture(t: TestContext, alreadyIssued = false) {
  t.mock.timers.enable({ apis: ["Date"], now });
  const access: string[] = [];
  let issued = alreadyIssued;
  const acquireLease = t.mock.fn<Wiring["instance"]["acquireLease"]>(async (observedPin, observedChange, signal) => {
    assert.deepEqual(observedPin, pin);
    assert.deepEqual(observedChange, change);
    assert.equal(signal.aborted, false);
    access.push("acquire");
    issued = true;
    return issuedLease;
  });
  const invokePrivate = t.mock.fn<Wiring["instance"]["invokePrivate"]>(async (observedPin, call) => {
    assert.deepEqual(observedPin, pin);
    assert.deepEqual(call.issuedLease, issuedLease);
    access.push(call.action);
  });
  const wiring: Wiring = {
    reservation: { pin, change },
    readRun: async () => { access.push("run"); return run; },
    readWorker: () => { access.push("worker"); return worker; },
    readAuthenticatedInventory: async (source, signal) => {
      assert.deepEqual(source, inventorySource);
      assert.equal(signal.aborted, false);
      access.push("inventory");
      return inventory;
    },
    instance: {
      acquireLease, invokePrivate,
      authority: {
        readIssuedLease: async () => { access.push("lease"); return issued ? issuedLease : undefined; },
        readApproval: async action => ({ action, target, issuedLease }),
      },
    },
  };
  return { access, wiring, acquireLease, invokePrivate };
}

test("the instance host stays disabled without reading its providers", async t => {
  const { access, wiring, acquireLease } = fixture(t);
  assert.deepEqual(await runStagingAutomodInstanceHost({ ...options, enabled: false }, wiring), { verdict: "disabled" });
  assert.deepEqual(access, []);
  assert.equal(acquireLease.mock.callCount(), 0);
});

test("the instance host validates both serving readbacks before acquiring the exact lease", async t => {
  const { access, wiring, acquireLease } = fixture(t);
  assert.deepEqual(await runStagingAutomodInstanceHost(options, wiring), { verdict: "passed", start: true, recover: true });
  const acquired = access.indexOf("acquire");
  for (const read of ["inventory", "worker"]) {
    assert.ok(access.indexOf(read) > access.indexOf("run"));
    assert.ok(access.indexOf(read) < acquired);
  }
  assert.deepEqual(access.slice(acquired + 1), ["lease", "worker", "inventory", "start", "recover"]);
  assert.equal(acquireLease.mock.callCount(), 1);
});

test("the instance host refuses missing, stale, or drifted inventory before acquiring a lease", async t => {
  const { wiring, acquireLease, invokePrivate } = fixture(t);
  const otherId = "55555555-5555-5555-5555-555555555555";
  const cases: Record<string, Inventory | undefined> = {
    missing: undefined,
    stale: { ...inventory, observed_at: new Date(now - 60_001).toISOString() },
    future: { ...inventory, observed_at: new Date(now + 1).toISOString() },
    "wrong inventory run": { ...inventory, workflow_run_id: inventory.workflow_run_id + 1 },
    "wrong inventory workflow": { ...inventory, workflow_sha: head },
    "Worker version": { ...inventory, worker: { ...inventory.worker, version_id: otherId } },
    "Worker deployment": { ...inventory, worker: { ...inventory.worker, deployment_id: otherId } },
    "source SHA": { ...inventory, worker: { ...inventory.worker, source_sha: head } },
    "split traffic": { ...inventory, worker: { ...inventory.worker, traffic_percent: 50 } } as unknown as Inventory,
    application: { ...inventory, container: { ...inventory.container, application_id: otherId } },
    namespace: { ...inventory, container: { ...inventory.container, namespace_id: "e".repeat(32) } },
    rollout: { ...inventory, container: { ...inventory.container, rollout_id: otherId } },
    image: { ...inventory, container: { ...inventory.container, image_digest: `sha256:${"c".repeat(64)}` } },
    "missing ownership": { ...inventory, ownership: undefined } as unknown as Inventory,
    "owner epoch": { ...inventory, ownership: { phase: "active", epoch: change.expectedEpoch + 1 } },
    "inactive owner": { ...inventory, ownership: { ...inventory.ownership, phase: "released" } } as unknown as Inventory,
  };
  for (const [label, readback] of Object.entries(cases)) {
    assert.deepEqual(await runStagingAutomodInstanceHost(options, {
      ...wiring, readAuthenticatedInventory: async () => readback,
    }), unconfirmed, label);
    assert.equal(acquireLease.mock.callCount(), 0, label);
    assert.equal(invokePrivate.mock.callCount(), 0, label);
  }
});

test("the instance host refuses inventory that ages out during the local Worker read", async t => {
  const { wiring, acquireLease } = fixture(t);
  assert.deepEqual(await runStagingAutomodInstanceHost(options, {
    ...wiring,
    readWorker: () => {
      t.mock.timers.setTime(now + 2);
      return worker;
    },
    readAuthenticatedInventory: async () => ({ ...inventory, observed_at: new Date(now - 59_999).toISOString() }),
  }), unconfirmed);
  assert.equal(acquireLease.mock.callCount(), 0);
});

test("the instance host refuses missing or drifted local Worker bindings before acquiring a lease", async t => {
  const { wiring, acquireLease, invokePrivate } = fixture(t);
  const cases: Record<string, Worker> = {
    missing: undefined as unknown as Worker,
    "missing version": { ...worker, env: { ...worker.env, CF_VERSION_METADATA: undefined } },
    "wrong version": { ...worker, env: { ...worker.env, CF_VERSION_METADATA: { id: pin.workerDeploymentId } } },
    "wrong release": { ...worker, env: { ...worker.env, CONTAINER_RELEASE: head } },
    "wrong target": { ...worker, env: { ...worker.env, GUILD_ID: "123" } },
    "wrong bearer": { ...worker, controlBearer: "synthetic-automod-host-other-control" },
  };
  for (const [label, readback] of Object.entries(cases)) {
    assert.deepEqual(await runStagingAutomodInstanceHost(options, { ...wiring, readWorker: () => readback }), unconfirmed, label);
    assert.equal(acquireLease.mock.callCount(), 0, label);
    assert.equal(invokePrivate.mock.callCount(), 0, label);
  }
});

test("the instance host refuses a late inventory response when the preparation deadline aborts", async t => {
  const { wiring, acquireLease, invokePrivate } = fixture(t);
  const deadlines = new Map<AbortSignal, AbortController>();
  t.mock.method(AbortSignal, "timeout", (ms: number) => {
    assert.equal(ms, 2_000);
    const controller = new AbortController();
    deadlines.set(controller.signal, controller);
    return controller.signal;
  });
  const backgroundRead = Promise.withResolvers<Inventory>();
  const started = Promise.withResolvers<AbortSignal>();
  const result = runStagingAutomodInstanceHost(options, {
    ...wiring, readAuthenticatedInventory: (_source, signal) => {
      started.resolve(signal);
      return backgroundRead.promise;
    },
  });
  const deadline = deadlines.get(await started.promise);
  try {
    assert.ok(deadline);
    deadline.abort();
    assert.deepEqual(await result, unconfirmed);
  } finally {
    backgroundRead.resolve(inventory);
    await result;
  }
  assert.equal(acquireLease.mock.callCount(), 0);
  assert.equal(invokePrivate.mock.callCount(), 0);
});

test("the instance host can recover the original lease without another acquisition", async t => {
  const { access, wiring, acquireLease } = fixture(t, true);
  const { instance } = wiring;
  const authority: typeof instance.authority = { ...instance.authority,
    readApproval: async action => action === "recover" ? { action, target, issuedLease } : undefined };
  assert.deepEqual(await runStagingAutomodInstanceHost(options, { ...wiring, instance: { ...instance, authority } }),
    { verdict: "failed", start: false, recover: true });
  assert.deepEqual(access, ["run", "lease", "lease", "worker", "inventory", "recover"]);
  assert.equal(acquireLease.mock.callCount(), 0);
});

test("the instance host confirms a durable lease after an interrupted acquisition reply", async t => {
  const { access, wiring, acquireLease } = fixture(t);
  const { instance } = wiring;
  assert.deepEqual(await runStagingAutomodInstanceHost(options, {
    ...wiring,
    instance: {
      ...instance,
      acquireLease: async (...args) => {
        await acquireLease(...args);
        throw new Error("synthetic lost acquisition reply");
      },
    },
  }), { verdict: "passed", start: true, recover: true });
  assert.deepEqual(access, ["run", "lease", "inventory", "worker", "acquire", "lease", "worker", "inventory", "start", "recover"]);
  assert.equal(acquireLease.mock.callCount(), 1);
});

test("the instance host refuses a conflicting readback returned after acquisition", async t => {
  const { wiring, acquireLease, invokePrivate } = fixture(t);
  const readAuthenticatedInventory = t.mock.fn<Wiring["readAuthenticatedInventory"]>(async () => {
    if (!acquireLease.mock.callCount()) return inventory;
    return { ...inventory, container: { ...inventory.container, rollout_id: pin.applicationId } };
  });
  assert.deepEqual(await runStagingAutomodInstanceHost(options, { ...wiring, readAuthenticatedInventory }),
    { verdict: "failed", start: false, recover: false });
  assert.equal(readAuthenticatedInventory.mock.callCount(), 2);
  assert.equal(acquireLease.mock.callCount(), 1);
  assert.equal(invokePrivate.mock.callCount(), 0);
});
