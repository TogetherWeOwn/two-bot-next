import assert from "node:assert/strict";
import { test, type TestContext } from "node:test";
import { readStagingAutomodJobInputs } from "../src/staging-automod-inputs.ts";
import { runProtectedAutomodJob } from "../src/staging-automod-job.ts";
import { STAGING_AUTOMOD_TARGET as target } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE, StagingLifecycle } from "../src/staging-lifecycle.ts";
import { lease, now } from "./fixtures/staging-automod-recovery.ts";

type Authority = Parameters<typeof readStagingAutomodJobInputs>[1];
const { environment, deploymentId, sha, runId, expiresAt, epoch } = lease;
const context = { environment, deploymentId, sha, runId, runAttempt: 1, target };

async function fixture(t: TestContext) {
  t.mock.timers.enable({ apis: ["Date"], now: now() });
  const values = new Map<string, unknown>();
  const storage = {
    get: async (key: string) => structuredClone(values.get(key)),
    put: async (entries: Record<string, unknown>) => {
      for (const [key, value] of Object.entries(entries)) values.set(key, structuredClone(value));
    },
    sync: async () => {},
  } as unknown as DurableObjectStorage;
  const owner = { deploymentId, epoch, phase: "active" as const, actor: "fixture",
    timestamp: new Date().toISOString(), oldDeploymentId: null, oldEpoch: 0 };
  const staging = new StagingLifecycle(storage);
  const issuedLease = await staging.acquire({
    environment, deploymentId, sha, runId, runAttempt: context.runAttempt, expiresAt, expectedEpoch: epoch, action: "acquire",
  }, { deploymentId, sha, owner }, async () => {}, async () => {}, now(), AUTOMOD_STAGING_PURPOSE);
  const start = { action: "start", target, issuedLease } as const;
  const recover = { action: "recover", target, issuedLease } as const;
  const readIssuedLease = t.mock.fn<Authority["readIssuedLease"]>(() => staging.read());
  const readApproval = t.mock.fn<Authority["readApproval"]>(async action => ({ start, recover })[action]);
  return { issuedLease, start, recover, authority: { readIssuedLease, readApproval } };
}

test("protected readbacks supply the issued lease and both independent approvals", async t => {
  const { issuedLease, start, recover, authority } = await fixture(t);
  const inputs = await readStagingAutomodJobInputs(context, authority);
  assert.deepEqual(inputs, { context, issuedLease, approvals: { start, recover } });
  assert.ok(inputs);
  assert.deepEqual(authority.readIssuedLease.mock.calls.map(call => call.arguments), [[context]]);
  assert.deepEqual(authority.readApproval.mock.calls.map(call => call.arguments), [
    ["start", issuedLease], ["recover", issuedLease],
  ]);

  const invoke = t.mock.fn(async () => {});
  for (const action of ["start", "recover"] as const) {
    const receipt = await runProtectedAutomodJob({ action, issuedLease }, inputs.approvals[action], invoke);
    assert.equal(receipt.verdict, "passed");
  }
  assert.deepEqual(invoke.mock.calls.map(call => call.arguments), [
    [{ action: "start", issuedLease }], [{ action: "recover", issuedLease }],
  ]);
});

test("a lease readback must match the protected job before any approval lookup", async t => {
  const { issuedLease, authority } = await fixture(t);
  for (const expected of [undefined, { ...context, runAttempt: 2 },
    { ...context, target: { ...target, guildId: "other" } }]) {
    assert.equal(await readStagingAutomodJobInputs(expected, authority), undefined);
  }
  assert.equal(authority.readIssuedLease.mock.callCount(), 0);
  for (const patch of [{ deploymentId: "other" }, { sha: "b".repeat(40) }, { runId: "12346" },
    { runAttempt: 2 }, { phase: "released" } as const, { purpose: undefined }, { voiceMove: true } as const]) {
    assert.equal(await readStagingAutomodJobInputs(context, {
      ...authority, readIssuedLease: async () => ({ ...issuedLease, ...patch }),
    }), undefined);
  }
  assert.equal(authority.readApproval.mock.callCount(), 0);
});

test("each approval must match its action and the exact issued lease", async t => {
  const { issuedLease, start, recover, authority } = await fixture(t);
  for (const invalid of [recover, { ...start, target: { ...target, guildId: "other" } },
    ...[{ runId: "12346" }, { runAttempt: 2 }, { deploymentId: "other" }, { sha: "b".repeat(40) },
      { epoch: epoch + 1 }, { acquiredAt: issuedLease.acquiredAt + 1 }, { expiresAt: expiresAt + 1 },
    ].map(patch => ({ ...start, issuedLease: { ...issuedLease, ...patch } }))]) {
    assert.deepEqual((await readStagingAutomodJobInputs(context, {
      ...authority, readApproval: async action => action === "start" ? invalid : recover,
    }))?.approvals, { recover });
  }
  assert.deepEqual((await readStagingAutomodJobInputs(context, {
    ...authority, readApproval: async action => {
      if (action === "start") throw new Error("synthetic read failure");
      return recover;
    },
  }))?.approvals, { recover });
  assert.deepEqual((await readStagingAutomodJobInputs(context, {
    ...authority, readApproval: async () => start,
  }))?.approvals, { start });
});

test("request-supplied authority cannot replace a protected readback", async t => {
  const { issuedLease, start, authority } = await fixture(t);
  const request = { action: "start", issuedLease } as const;
  const assertedAuthority = { issuedLease, approvals: { start } };
  const untrustedFields = { ...context, ...assertedAuthority };
  assert.equal(await readStagingAutomodJobInputs(untrustedFields, {
    ...authority, readIssuedLease: async () => undefined,
  }), undefined);
  const inputs = await readStagingAutomodJobInputs(untrustedFields, {
    ...authority, readApproval: async () => undefined,
  });
  assert.ok(inputs);
  assert.deepEqual(inputs.approvals, {});
  const invoke = t.mock.fn(async () => {});
  for (const body of [request, { ...request, ...assertedAuthority }]) {
    const receipt = await runProtectedAutomodJob(body, inputs.approvals.start, invoke);
    assert.equal(receipt.verdict, "failed");
    assert.equal(receipt.code, "request_refused");
  }
  assert.equal(invoke.mock.callCount(), 0);
});
