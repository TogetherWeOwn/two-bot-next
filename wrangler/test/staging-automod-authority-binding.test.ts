import assert from "node:assert/strict";
import { test, type TestContext } from "node:test";
import { setImmediate } from "node:timers/promises";
import { runStagingAutomodBoundJob } from "../scripts/staging-automod-private-job.ts";
import { boundStagingAutomodAuthority } from "../src/staging-automod-bounded-authority.ts";
import { readStagingAutomodJobInputs } from "../src/staging-automod-inputs.ts";
import { STAGING_AUTOMOD_TARGET as target } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE } from "../src/staging-use-window.ts";
import { lease, now } from "./fixtures/staging-automod-recovery.ts";

type Authority = Parameters<typeof readStagingAutomodJobInputs>[1];
type Approval = NonNullable<Awaited<ReturnType<Authority["readApproval"]>>>;
type Invoke = Parameters<typeof runStagingAutomodBoundJob>[3]["invoke"];

const issuedLease = { ...lease, purpose: AUTOMOD_STAGING_PURPOSE, runAttempt: 1 } as const;
const { environment, deploymentId, sha, runId, runAttempt } = issuedLease;
const context = { environment, deploymentId, sha, runId, runAttempt, target };
const approvals = {
  start: { action: "start", target, issuedLease },
  recover: { action: "recover", target, issuedLease },
} as const;
const controlBearer = "a".repeat(32);
const env = {
  TWO_DEPLOY_ENV: "staging", STAGING_RUNTIME_POLICY: "bounded",
  DISCORD_APPLICATION_ID: target.applicationId, GUILD_ID: target.guildId,
  OWNERSHIP_CONTROL_TOKEN: controlBearer, CF_VERSION_METADATA: { id: deploymentId }, CONTAINER_RELEASE: sha,
};
const failed = { verdict: "failed", start: false, recover: false };

async function fixture(t: TestContext) {
  t.mock.timers.enable({ apis: ["Date"], now: now() });
  const fetch = t.mock.method(globalThis, "fetch", async () => { throw new Error("unexpected network request"); });
  const readControlBearer = t.mock.fn(() => controlBearer);
  const invoke = t.mock.fn<Invoke>(async () => {});
  const worker = { env, get controlBearer() { return readControlBearer(); }, invoke };

  const caller = new AbortController();
  const reads = { start: Promise.withResolvers<Approval>(), recover: Promise.withResolvers<Approval>() };
  const entered = Promise.withResolvers<boolean>();
  const readIssuedLease = t.mock.fn<Authority["readIssuedLease"]>(async () => issuedLease);
  const readApproval = t.mock.fn((action: Approval["action"], _lease: Approval["issuedLease"], _signal: AbortSignal) => {
    if (action === "recover") entered.resolve(true);
    return reads[action].promise;
  });
  const authority = boundStagingAutomodAuthority({ readIssuedLease, readApproval }, caller.signal);

  const run = readStagingAutomodJobInputs(context, authority).then(inputs =>
    runStagingAutomodBoundJob(inputs?.context, inputs?.issuedLease, inputs?.approvals, worker));

  assert.equal(await Promise.race([entered.promise, run.then(() => false)]), true);
  assert.deepEqual(readIssuedLease.mock.calls.map(call => call.arguments[0]), [context]);
  assert.deepEqual(readApproval.mock.calls.map(({ arguments: [action, lease, signal] }) =>
    [action, lease, signal.aborted]), [["start", issuedLease, false], ["recover", issuedLease, false]]);

  function assertTransport(actions: Approval["action"][]) {
    assert.equal(readControlBearer.mock.callCount(), actions.length);
    assert.deepEqual(invoke.mock.calls.map(call => call.arguments[0]), actions.map(action => ({ action, issuedLease })));
    assert.equal(fetch.mock.callCount(), 0);
  }

  assertTransport([]);
  return { caller, reads, run, assertTransport };
}

test("caller cancellation while approvals are pending blocks the bound Worker transport", async t => {
  const { caller, reads, run, assertTransport } = await fixture(t);

  caller.abort();
  assert.deepEqual(await run, failed);
  reads.start.resolve(approvals.start);
  reads.recover.resolve(approvals.recover);
  await setImmediate();
  assertTransport([]);
});

test("caller cancellation racing a valid approval blocks the bound Worker transport", async t => {
  const { caller, reads, run, assertTransport } = await fixture(t);

  // Fulfillment queues first, before the caller aborts in the same turn.
  reads.start.resolve(approvals.start);
  caller.abort();
  reads.recover.resolve(approvals.recover);
  assert.deepEqual(await run, failed);
  assertTransport([]);
});

test("the same protected scope reaches the bound Worker with both approvals", async t => {
  const { reads, run, assertTransport } = await fixture(t);

  reads.start.resolve(approvals.start);
  reads.recover.resolve(approvals.recover);
  assert.deepEqual(await run, { verdict: "passed", start: true, recover: true });
  assertTransport(["start", "recover"]);
});
