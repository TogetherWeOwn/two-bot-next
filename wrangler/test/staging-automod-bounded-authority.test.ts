import assert from "node:assert/strict";
import { test, type TestContext } from "node:test";
import { boundStagingAutomodAuthority } from "../src/staging-automod-bounded-authority.ts";
import type { runProtectedAutomodJob } from "../src/staging-automod-job.ts";
import { AUTOMOD_STAGING_PURPOSE } from "../src/staging-use-window.ts";
import { lease } from "./fixtures/staging-automod-recovery.ts";

type Approval = NonNullable<Parameters<typeof runProtectedAutomodJob>[1]>;
type Lease = Approval["issuedLease"];
type Action = Approval["action"];
type Context = Readonly<Pick<Lease, "environment" | "deploymentId" | "sha" | "runId" | "runAttempt">
  & { target: Approval["target"] }>;

const target = Object.freeze({ applicationId: "fixture-app", guildId: "fixture-guild" });
const issuedLease = Object.freeze({ ...lease, runAttempt: 1, purpose: AUTOMOD_STAGING_PURPOSE });
const { environment, deploymentId, sha, runId, runAttempt } = issuedLease;
const context: Context = Object.freeze({ environment, deploymentId, sha, runId, runAttempt, target });
const approvals = {
  start: Object.freeze({ action: "start", issuedLease, target }),
  recover: Object.freeze({ action: "recover", issuedLease, target }),
} as const;

const actions = ["start", "recover"] as const;
const readIssuedLease = async (_context: Context, _signal: AbortSignal): Promise<Lease> => issuedLease;

function fakeDeadlines(t: TestContext) {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  // Native AbortSignal.timeout uses a different clock from the mocked timers.
  t.mock.method(AbortSignal, "timeout", (ms: number) => {
    const controller = new AbortController();
    setTimeout(() => controller.abort(), ms);
    return controller.signal;
  });
}

test("both reads preserve the issuer scope and original provider records", async () => {
  const authority = boundStagingAutomodAuthority({
    readIssuedLease: async (scope: Context, signal) => {
      assert.equal(scope, context);
      assert.equal(signal.aborted, false);
      return issuedLease;
    },
    readApproval: async (action: Action, scope: Lease, signal) => {
      assert.equal(scope, issuedLease);
      assert.equal(signal.aborted, false);
      return approvals[action];
    },
  });
  const issued = await authority.readIssuedLease(context);
  assert.equal(issued, issuedLease);
  for (const action of actions) {
    assert.equal(await authority.readApproval(action, issued), approvals[action]);
  }
});

test("either stalled approval expires independently and a late record stays unavailable", async t => {
  fakeDeadlines(t);
  for (const [stalled, available] of [["start", "recover"], ["recover", "start"]] as const) {
    const late = Promise.withResolvers<Approval>();
    const signals = new Map<Action, AbortSignal>();
    const authority = boundStagingAutomodAuthority({
      readIssuedLease,
      readApproval: async (action: Action, _lease: Lease, signal) => {
        signals.set(action, signal);
        return action === stalled ? late.promise : approvals[action];
      },
    });
    const stalledRead = authority.readApproval(stalled, issuedLease);
    const availableRead = authority.readApproval(available, issuedLease);
    // The issuer also waits for both reads, catching each refusal separately.
    const result = Promise.all([stalledRead.catch(() => undefined), availableRead]);
    assert.notEqual(signals.get(stalled), signals.get(available));
    assert.equal(await availableRead, approvals[available]);
    t.mock.timers.tick(1_999);
    assert.equal(signals.get(stalled)?.aborted, false);
    t.mock.timers.tick(1);
    assert.equal(signals.get(stalled)?.aborted, true);
    assert.deepEqual(await result, [undefined, approvals[available]]);
    late.resolve(approvals[stalled]);
    assert.equal(await stalledRead.catch(() => undefined), undefined);
  }
});

test("either approval can reject without replacing the other provider record", async () => {
  const failure = new Error("synthetic provider rejection");
  for (const missing of actions) {
    const authority = boundStagingAutomodAuthority({
      readIssuedLease,
      readApproval: async (action: Action, _lease: Lease) => {
        if (action === missing) throw failure;
        return approvals[action];
      },
    });
    const results = await Promise.allSettled(actions.map(action => authority.readApproval(action, issuedLease)));
    assert.deepEqual(results, actions.map(action => action === missing
      ? { status: "rejected", reason: failure }
      : { status: "fulfilled", value: approvals[action] }));
  }
});

test("a stalled lease lookup has the same deadline", async t => {
  fakeDeadlines(t);
  const read = t.mock.fn((_context: Context, _signal: AbortSignal) => new Promise<Lease>(() => {}));
  const authority = boundStagingAutomodAuthority({ readIssuedLease: read, readApproval: async () => undefined });
  const pending = assert.rejects(authority.readIssuedLease(context));
  t.mock.timers.tick(2_000);
  assert.equal(read.mock.calls[0]?.arguments[1].aborted, true);
  await pending;
});

test("caller abort settles both reads and prevents new lookups", async t => {
  const caller = new AbortController();
  const reads = { start: Promise.withResolvers<Approval>(), recover: Promise.withResolvers<Approval>() };
  const leaseRead = t.mock.fn(readIssuedLease);
  const approvalRead = t.mock.fn((action: Action, _lease: Lease, _signal: AbortSignal) => reads[action].promise);
  const authority = boundStagingAutomodAuthority({ readIssuedLease: leaseRead, readApproval: approvalRead }, caller.signal);
  const pending = actions.map(action => assert.rejects(authority.readApproval(action, issuedLease)));

  caller.abort();
  assert.deepEqual(approvalRead.mock.calls.map(call => call.arguments[2].aborted), [true, true]);
  await Promise.all(pending);
  reads.start.reject(new Error("late provider rejection"));
  reads.recover.resolve(approvals.recover);

  await assert.rejects(authority.readIssuedLease(context));
  await assert.rejects(authority.readApproval("start", issuedLease));
  assert.equal(leaseRead.mock.callCount(), 0);
  assert.equal(approvalRead.mock.callCount(), 2);
});

test("a result fulfilled as the caller aborts is unavailable", async () => {
  const caller = new AbortController();
  const authority = boundStagingAutomodAuthority({
    readIssuedLease,
    readApproval: async () => {
      caller.abort();
      return approvals.start;
    },
  }, caller.signal);
  await assert.rejects(authority.readApproval("start", issuedLease));
});
