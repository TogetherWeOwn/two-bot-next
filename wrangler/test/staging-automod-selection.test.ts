import assert from "node:assert/strict";
import { test, type TestContext } from "node:test";
import { resolveStagingAutomodSelection } from "../scripts/staging-automod-selection.ts";
import type { bindStagingAutomodDispatch } from "../scripts/staging-automod-dispatch.ts";
import { STAGING_AUTOMOD_PLAN as dormant } from "../src/staging-automod-plan.ts";
import { STAGING_AUTOMOD_TARGET as target } from "../src/staging-automod-recovery.ts";

type Resolver = Parameters<typeof resolveStagingAutomodSelection>;
type Readbacks = Resolver[1];
type Run = NonNullable<Awaited<ReturnType<Readbacks["readPostDeployRuns"]>>>[number];
type Inventory = NonNullable<Awaited<ReturnType<Readbacks["readAuthenticatedInventory"]>>>;
const now = Date.parse("2026-11-27T12:00:00Z");
const plan = { ...dormant, enabled: true, allowStart: true, notAfter: now + dormant.leaseMs };
const dispatch = { sha: "a".repeat(40), runId: "11223344", runAttempt: 1 };
const source = { workflowSha: dispatch.sha, runId: 12345678 };
const pin = {
  workerDeploymentId: "11111111-1111-1111-1111-111111111111",
  workerVersionId: "22222222-2222-2222-2222-222222222222",
  sourceSha: dispatch.sha,
  applicationId: "33333333-3333-3333-3333-333333333333",
  namespaceId: "a".repeat(32),
  rolloutId: "44444444-4444-4444-4444-444444444444",
  imageDigest: `sha256:${"b".repeat(64)}`,
};
const run: Run = { dispatch, inventoryRun: { ...source, runAttempt: 1 }, pin, target, expiresAt: plan.notAfter };
const inventory: Inventory = {
  schema_version: 1, workflow_sha: source.workflowSha, workflow_run_id: source.runId,
  observed_at: new Date(now).toISOString(),
  worker: { deployment_id: pin.workerDeploymentId, version_id: pin.workerVersionId, source_sha: pin.sourceSha,
    traffic_percent: 100, runtime_policy: "bounded" },
  container: { application_id: pin.applicationId, namespace_id: pin.namespaceId, rollout_id: pin.rolloutId,
    rollout_status: "completed", image_digest: pin.imageDigest },
  ownership: { phase: "active", epoch: 7 },
};
const readbacks: Readbacks = {
  readPostDeployRuns: async () => [run],
  readAuthenticatedInventory: async () => inventory,
};

function setup(t: TestContext) {
  t.mock.timers.enable({ apis: ["Date"], now });
  return new AbortController();
}

test("selects the exact serving tuple, inventory producer and attempt-one reservation", async t => {
  const caller = setup(t);
  const signals: AbortSignal[] = [];
  const result: Parameters<typeof bindStagingAutomodDispatch>[1] = await resolveStagingAutomodSelection(dispatch, {
    readPostDeployRuns: async (scope, signal) => {
      assert.deepEqual(scope, dispatch);
      assert.equal(signal.aborted, false);
      signals.push(signal);
      return [{ ...run, voiceMove: false }];
    },
    readAuthenticatedInventory: async (selection, signal) => {
      assert.deepEqual(selection, source);
      signals.push(signal);
      return inventory;
    },
  }, caller.signal, plan);
  assert.equal(signals.length, 2);
  assert.equal(signals[0], signals[1]);
  assert.deepEqual(result, {
    serving: { deploymentId: pin.workerVersionId, sha: dispatch.sha }, inventorySource: source,
    reservation: { pin, change: { environment: "staging", action: "acquire", runId: dispatch.runId, runAttempt: 1,
      deploymentId: pin.workerVersionId, sha: dispatch.sha, expectedEpoch: 7, expiresAt: plan.notAfter } },
  });
});

test("refuses dormant policy, invalid dispatch or cancellation before reading", async t => {
  const { signal } = setup(t);
  const readRuns = t.mock.fn(readbacks.readPostDeployRuns);
  const readers = { ...readbacks, readPostDeployRuns: readRuns };
  assert.equal(await resolveStagingAutomodSelection(dispatch, readers, signal), undefined);
  for (const change of [{ sha: "main" }, { runId: "01" }, { runId: "9007199254740992" }, { runAttempt: 2 }]) {
    assert.equal(await resolveStagingAutomodSelection({ ...dispatch, ...change }, readers, signal, plan), undefined);
  }
  assert.equal(await resolveStagingAutomodSelection(dispatch, readers, AbortSignal.abort(), plan), undefined);
  assert.equal(readRuns.mock.callCount(), 0);
});

test("refuses absent, ambiguous or out-of-scope run evidence before inventory access", async t => {
  const { signal } = setup(t);
  const readInventory = t.mock.fn(readbacks.readAuthenticatedInventory);
  const wrongSha = "b".repeat(40);
  const changes: [string, Partial<Run>][] = [
    ["different dispatch SHA", { dispatch: { ...dispatch, sha: wrongSha } }],
    ["different dispatch run", { dispatch: { ...dispatch, runId: "9" } }],
    ["retried dispatch", { dispatch: { ...dispatch, runAttempt: 2 } }],
    ["different workflow SHA", { inventoryRun: { ...run.inventoryRun, workflowSha: wrongSha } }],
    ["retried inventory", { inventoryRun: { ...run.inventoryRun, runAttempt: 2 } }],
    ["nonpositive inventory run", { inventoryRun: { ...run.inventoryRun, runId: 0 } }],
    ["fractional inventory run", { inventoryRun: { ...run.inventoryRun, runId: 1.5 } }],
    ["aliased inventory run", { inventoryRun: { ...run.inventoryRun, runId: Number(dispatch.runId) } }],
    ["wrong Discord app", { target: { ...target, applicationId: "1234567890123456789" } }],
    ["different serving build", { pin: { ...pin, sourceSha: wrongSha } }],
    ["invalid pin", { pin: { ...pin, namespaceId: " " } }],
    ["voice move", { voiceMove: true }],
    ["expired lease", { expiresAt: now }],
    ["past plan cutoff", { expiresAt: plan.notAfter + 1 }],
    ["fractional expiry", { expiresAt: now + 1.5 }],
  ];
  const cases: [string, Run[] | undefined][] = [
    ["missing", undefined], ["empty", []], ["duplicate", [run, run]],
    ["conflicting", [run, { ...run, inventoryRun: { ...run.inventoryRun, runId: source.runId + 1 } }]],
    ...changes.map(([label, change]): [string, Run[]] => [label, [{ ...run, ...change }]]),
  ];
  for (const [label, evidence] of cases) {
    assert.equal(await resolveStagingAutomodSelection(dispatch, {
      readPostDeployRuns: async () => evidence, readAuthenticatedInventory: readInventory,
    }, signal, plan), undefined, label);
  }
  assert.equal(await resolveStagingAutomodSelection(dispatch, {
    readPostDeployRuns: async () => { throw new Error("unauthenticated readback"); },
    readAuthenticatedInventory: readInventory,
  }, signal, plan), undefined);
  assert.equal(await resolveStagingAutomodSelection(dispatch, { ...readbacks, readAuthenticatedInventory: readInventory },
    signal, { ...plan, notAfter: plan.notAfter + 1_000, leaseMs: dormant.leaseMs - 1 }), undefined, "overlong lease");
  assert.equal(readInventory.mock.callCount(), 0);
});

test("refuses conflicting, stale or malformed authenticated inventory", async t => {
  const { signal } = setup(t);
  const changes: [string, Partial<Inventory>][] = [
    ["workflow SHA", { workflow_sha: "b".repeat(40) }],
    ["producer run", { workflow_run_id: source.runId + 1 }],
    ["Worker deployment", { worker: { ...inventory.worker, deployment_id: pin.workerVersionId } }],
    ["Worker version", { worker: { ...inventory.worker, version_id: pin.workerDeploymentId } }],
    ["Worker build", { worker: { ...inventory.worker, source_sha: "b".repeat(40) } }],
    ["Container app", { container: { ...inventory.container, application_id: pin.workerVersionId } }],
    ["namespace", { container: { ...inventory.container, namespace_id: "f".repeat(32) } }],
    ["rollout", { container: { ...inventory.container, rollout_id: pin.workerVersionId } }],
    ["image", { container: { ...inventory.container, image_digest: `sha256:${"f".repeat(64)}` } }],
    ["fractional owner epoch", { ownership: { phase: "active", epoch: 1.5 } }],
    ["nonpositive owner epoch", { ownership: { phase: "active", epoch: 0 } }],
    ["unsafe owner epoch", { ownership: { phase: "active", epoch: Number.MAX_SAFE_INTEGER + 1 } }],
    ["stale readback", { observed_at: new Date(now - 60_001).toISOString() }],
    ["future readback", { observed_at: new Date(now + 1).toISOString() }],
    ["malformed date", { observed_at: "2026-11-26T23:59:60Z" }],
  ];
  for (const [label, change] of changes) {
    assert.equal(await resolveStagingAutomodSelection(dispatch, {
      ...readbacks, readAuthenticatedInventory: async () => ({ ...inventory, ...change }),
    }, signal, plan), undefined, label);
  }
  for (const readAuthenticatedInventory of [async () => undefined, async () => { throw new Error("unavailable"); }]) {
    assert.equal(await resolveStagingAutomodSelection(dispatch, { ...readbacks, readAuthenticatedInventory }, signal, plan),
      undefined);
  }
});

test("refuses a lease expiring during inventory readback", async t => {
  const { signal } = setup(t);
  assert.equal(await resolveStagingAutomodSelection(dispatch, {
    ...readbacks, readAuthenticatedInventory: async () => {
      t.mock.timers.setTime(run.expiresAt);
      return { ...inventory, observed_at: new Date().toISOString() };
    },
  }, signal, plan), undefined);
});

test("settles on caller abort during either read without waiting for the reader", { timeout: 500 }, async t => {
  setup(t);
  for (const step of ["readPostDeployRuns", "readAuthenticatedInventory"] as const) {
    const caller = new AbortController();
    const { promise: entered, resolve: signalSeen } = Promise.withResolvers<AbortSignal>();
    const stalled = t.mock.fn((_scope: unknown, signal: AbortSignal) => {
      signalSeen(signal);
      return new Promise<never>(() => {});
    });
    const readers = { ...readbacks, readAuthenticatedInventory: t.mock.fn(readbacks.readAuthenticatedInventory),
      [step]: stalled };
    const pending = resolveStagingAutomodSelection(dispatch, readers, caller.signal, plan);
    const signal = await entered;
    caller.abort();
    assert.equal(await pending, undefined);
    assert.equal(signal.aborted, true);
    assert.equal(stalled.mock.callCount(), 1);
    if (step === "readPostDeployRuns") assert.equal(readers.readAuthenticatedInventory.mock.callCount(), 0);
  }
});

test("the shared deadline refuses a readback that completes before the abort event runs", async t => {
  const { signal } = setup(t);
  const clock = t.mock.method(performance, "now", () => 0);
  assert.equal(await resolveStagingAutomodSelection(dispatch, {
    ...readbacks, readAuthenticatedInventory: async () => {
      clock.mock.mockImplementation(() => 2_000);
      return inventory;
    },
  }, signal, plan), undefined);
});

test("the internal timeout cancels a stalled reader", { timeout: 500 }, async t => {
  const caller = setup(t);
  const timer = new AbortController();
  const timeout = t.mock.method(AbortSignal, "timeout", () => timer.signal);
  const readPostDeployRuns = t.mock.fn((_scope: unknown, _signal: AbortSignal) => {
    timer.abort();
    return new Promise<never>(() => {});
  });
  assert.equal(await resolveStagingAutomodSelection(dispatch, { ...readbacks, readPostDeployRuns }, caller.signal, plan),
    undefined);
  assert.equal(caller.signal.aborted, false);
  assert.equal(timeout.mock.callCount(), 1);
  assert.deepEqual(timeout.mock.calls[0]?.arguments, [2_000]);
  assert.equal(readPostDeployRuns.mock.callCount(), 1);
  assert.equal(readPostDeployRuns.mock.calls[0]?.arguments[1].aborted, true);
});
