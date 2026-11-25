import assert from "node:assert/strict";
import { test } from "node:test";
import { runStagingAutomodDryRun } from "../scripts/staging-automod-workflow.ts";
import { runProtectedAutomodJob } from "../src/staging-automod-job.ts";
import { STAGING_AUTOMOD_TARGET as target } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE } from "../src/staging-lifecycle.ts";
import { lease, now } from "./fixtures/staging-automod-recovery.ts";

const issuedLease = { ...lease, purpose: AUTOMOD_STAGING_PURPOSE, runAttempt: 1 } as const;
const { runId, runAttempt, sha } = issuedLease;
const job = { runId, runAttempt, sha };
const passed = { verdict: "passed", start: true, recover: true };
const startupUnconfirmed = { verdict: "failed", start: false, recover: true };
type Invoke = Parameters<typeof runStagingAutomodDryRun>[2];
type Receipt = Awaited<ReturnType<typeof runProtectedAutomodJob>>;

function syntheticBoundary(effect: (action: "start" | "recover") => void = () => {},
  reply: (receipt: Receipt) => Response | Promise<Response> = receipt => Response.json(receipt)): Invoke {
  const approvals = {
    start: { action: "start", issuedLease, target },
    recover: { action: "recover", issuedLease, target },
  } as const;
  return async request => {
    const receipt = await runProtectedAutomodJob(request, approvals[request.action], call => {
      assert.deepEqual(call, { action: request.action, issuedLease });
      effect(call.action);
      return Promise.resolve();
    });
    return reply(receipt);
  };
}

test("the exact protected staging job uses the sole AutoMod lease for startup and recovery", async t => {
  t.mock.timers.enable({ apis: ["Date"], now: now() });
  const calls: string[] = [];
  const invoke = t.mock.fn(syntheticBoundary(action => calls.push(action)));
  for (const wrongLease of [{ ...issuedLease, purpose: undefined }, { ...issuedLease, voiceMove: true },
    { ...issuedLease, environment: "production" }, { ...issuedLease, sha: "b".repeat(40) },
    { ...issuedLease, runId: "999" }, { ...issuedLease, runAttempt: 2 }]) {
    assert.deepEqual(await runStagingAutomodDryRun(job, wrongLease as typeof issuedLease, invoke),
      { verdict: "failed", code: "lease_unconfirmed" });
  }
  assert.deepEqual(await runStagingAutomodDryRun({ ...job, runAttempt: 2 },
    { ...issuedLease, runAttempt: 2 }, invoke), { verdict: "failed", code: "lease_unconfirmed" });
  assert.equal(invoke.mock.callCount(), 0);
  assert.deepEqual(await runStagingAutomodDryRun(job, issuedLease, invoke), passed);
  assert.deepEqual(calls, ["start", "recover"]);
});

test("a failed startup still recovers the original lease after expiry and keeps the job red", async t => {
  t.mock.timers.enable({ apis: ["Date"], now: now() });
  let released = false;
  const invoke = syntheticBoundary(action => {
    if (action === "start") {
      t.mock.timers.setTime(issuedLease.expiresAt + 1);
      throw new Error("synthetic startup failure after taking effect");
    }
    released = true;
  });
  assert.deepEqual(await runStagingAutomodDryRun(job, issuedLease, invoke), startupUnconfirmed);
  assert.equal(released, true);
});

test("the startup deadline reaches recovery when the invoker loses the reply", async t => {
  t.mock.timers.enable({ apis: ["Date", "setTimeout"], now: now() });
  let started!: () => void;
  const attempted = new Promise<void>(resolve => { started = resolve; });
  const calls: string[] = [];
  const invoke = syntheticBoundary(action => calls.push(action), receipt => {
    if ("action" in receipt && receipt.action === "start") {
      started();
      return new Promise(() => {});
    }
    return Response.json(receipt);
  });
  const run = runStagingAutomodDryRun(job, issuedLease, invoke);
  await attempted;
  t.mock.timers.tick(15_000);
  assert.deepEqual(await run, startupUnconfirmed);
  assert.deepEqual(calls, ["start", "recover"]);
});

test("only bounded, exact receipt evidence can confirm either action", async t => {
  t.mock.timers.enable({ apis: ["Date"], now: now() });
  for (const mutate of [
    (receipt: Receipt) => new Response(JSON.stringify(receipt) + " ".repeat(1024)),
    () => new Response(Uint8Array.of(0xff)),
    () => new Response("{"),
    (receipt: Receipt) => Response.json({ ...receipt, detail: "synthetic debug" }),
    (receipt: Receipt) => Response.json({ ...receipt, lease: { ...issuedLease, epoch: issuedLease.epoch + 1 } }),
    (receipt: Receipt) => Response.json({ ...receipt, lease: { ...issuedLease, purpose: null } }),
    (receipt: Receipt) => Response.json({ ...receipt,
      action: "action" in receipt && receipt.action === "recover" ? "start" : "recover" }),
    (receipt: Receipt) => Response.json({ ...receipt, observedAt: "yesterday" }),
    (receipt: Receipt) => Response.json(receipt, { status: 503 }),
  ]) {
    for (const action of ["start", "recover"] as const) {
      const invoke = syntheticBoundary(undefined, receipt =>
        "action" in receipt && receipt.action === action ? mutate(receipt) : Response.json(receipt));
      assert.deepEqual(await runStagingAutomodDryRun(job, issuedLease, invoke),
        { ...passed, verdict: "failed", [action]: false });
    }
  }
  const refusedCleanup = syntheticBoundary(action => {
    if (action === "recover") throw new Error("synthetic cleanup refusal");
  });
  assert.deepEqual(await runStagingAutomodDryRun(job, issuedLease, refusedCleanup),
    { verdict: "failed", start: true, recover: false });
});
