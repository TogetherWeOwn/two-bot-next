import assert from "node:assert/strict";
import { test } from "node:test";
import { runStagingAutomodPrivateJob } from "../scripts/staging-automod-private-job.ts";
import { runProtectedAutomodJob } from "../src/staging-automod-job.ts";
import { STAGING_AUTOMOD_TARGET as target } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE } from "../src/staging-lifecycle.ts";
import { lease, now } from "./fixtures/staging-automod-recovery.ts";

type Transport = Parameters<typeof runStagingAutomodPrivateJob>[3];
type Invoke = Parameters<typeof runProtectedAutomodJob>[2];
type Receipt = Awaited<ReturnType<typeof runProtectedAutomodJob>>;
const issuedLease = { ...lease, purpose: AUTOMOD_STAGING_PURPOSE, runAttempt: 1 } as const;
const { runId, runAttempt, sha } = issuedLease;
const job = { runId, runAttempt, sha };
const approvals = {
  start: { action: "start", target, issuedLease },
  recover: { action: "recover", target, issuedLease },
} as const;
const passed = { verdict: "passed", start: true, recover: true };

function syntheticTransport(invoke: Invoke,
  reply: (receipt: Receipt) => Response = receipt => Response.json(receipt),
): Transport {
  return async (call, approval, signal) => {
    signal.throwIfAborted();
    return reply(await runProtectedAutomodJob(call, approval, invoke));
  };
}

test("private job forwards the issued scope and each independent approval", async t => {
  t.mock.timers.enable({ apis: ["Date"], now: now() });
  const invoke = t.mock.fn<Invoke>(async () => {});
  const transport = t.mock.fn(syntheticTransport(invoke));
  for (const wrongJob of [{ ...job, runId: "98765" }, { ...job, runAttempt: 2 }, { ...job, sha: "b".repeat(40) }]) {
    assert.deepEqual(await runStagingAutomodPrivateJob(wrongJob, issuedLease, approvals, transport),
      { verdict: "failed", code: "lease_unconfirmed" });
  }
  assert.equal(transport.mock.callCount(), 0);

  assert.deepEqual(await runStagingAutomodPrivateJob(job, issuedLease, approvals, transport), passed);
  const calls = [{ action: "start", issuedLease }, { action: "recover", issuedLease }];
  assert.deepEqual(invoke.mock.calls.map(call => call.arguments[0]), calls);
  assert.deepEqual(transport.mock.calls.map(call => call.arguments[0]), calls);
  assert.equal(transport.mock.calls[0]?.arguments[1], approvals.start);
  assert.equal(transport.mock.calls[1]?.arguments[1], approvals.recover);
});

test("private job cannot pass with missing or mismatched approval", async t => {
  t.mock.timers.enable({ apis: ["Date"], now: now() });
  for (const action of ["start", "recover"] as const) {
    const other = action === "start" ? "recover" : "start";
    const approval = approvals[action];
    for (const wrong of [approvals[other], { ...approval, target: { ...target, guildId: "other" } },
      ...[{ runId: "98765" }, { runAttempt: 2 }, { deploymentId: "other" }, { sha: "b".repeat(40) },
        { epoch: lease.epoch + 1 }].map(scope => ({ ...approval, issuedLease: { ...issuedLease, ...scope } }))]) {
      const invoke = t.mock.fn<Invoke>(async () => {});
      assert.deepEqual(await runStagingAutomodPrivateJob(job, issuedLease,
        { ...approvals, [action]: wrong }, syntheticTransport(invoke)), { ...passed, verdict: "failed", [action]: false });
      assert.deepEqual(invoke.mock.calls.map(call => call.arguments[0]), [{ action: other, issuedLease }]);
    }

    const transport = t.mock.fn(syntheticTransport(async () => {}));
    assert.deepEqual(await runStagingAutomodPrivateJob(job, issuedLease,
      { [other]: approvals[other] }, transport), { ...passed, verdict: "failed", [action]: false });
    assert.deepEqual(transport.mock.calls.map(call => call.arguments[0]), [{ action: other, issuedLease }]);
  }
});

test("private job recovers the original lease after failed startup and expiry", async t => {
  t.mock.timers.enable({ apis: ["Date"], now: now() });
  const invoke = t.mock.fn<Invoke>(async call => {
    if (call.action === "start") {
      t.mock.timers.setTime(issuedLease.expiresAt + 1);
      throw new Error("synthetic startup failure after taking effect");
    }
  });
  assert.deepEqual(await runStagingAutomodPrivateJob(job, issuedLease, approvals, syntheticTransport(invoke)),
    { verdict: "failed", start: false, recover: true });
  assert.deepEqual(invoke.mock.calls.map(call => call.arguments[0]),
    [{ action: "start", issuedLease }, { action: "recover", issuedLease }]);
});

test("private job requires bounded same-scope receipts for both actions", async t => {
  t.mock.timers.enable({ apis: ["Date"], now: now() });
  for (const action of ["start", "recover"] as const) {
    for (const corrupt of [
      (receipt: Receipt) => new Response(JSON.stringify(receipt) + " ".repeat(1024)),
      (receipt: Receipt) => Response.json({ ...receipt, lease: { ...issuedLease, epoch: lease.epoch + 1 } }),
    ]) {
      const transport = syntheticTransport(async () => {}, receipt =>
        "action" in receipt && receipt.action === action ? corrupt(receipt) : Response.json(receipt));
      assert.deepEqual(await runStagingAutomodPrivateJob(job, issuedLease, approvals, transport),
        { ...passed, verdict: "failed", [action]: false });
    }
  }
});
