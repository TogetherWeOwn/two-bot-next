import assert from "node:assert/strict";
import test from "node:test";
import { createStagingAutomodDryRunAdapter } from "../src/staging-automod-dry-run-adapter.ts";
import { AUTOMOD_STAGING_PURPOSE } from "../src/staging-use-window.ts";

const lease = Object.freeze({
  environment: "staging", phase: "active", purpose: AUTOMOD_STAGING_PURPOSE,
  deploymentId: "build-1", sha: "a".repeat(40), epoch: 2, runId: "3", runAttempt: 4,
  acquiredAt: 1_000, expiresAt: 3_000,
} as const);
const policy = Object.freeze({
  memberIntake: false, enforcement: false, resume: false, registrySync: false,
  interactionDispatch: false, dedicatedIntents: true,
});
const running = { lease, running: true, policy };
const stopped = { lease, running: false };
const drained = { lease, publicationDrained: true, startupDrained: true };
const startupBefore = 2_500;
const cleanupBefore = 5_000;

function fixture() {
  let at = 2_000;
  const history: string[] = [];
  const transport: Parameters<typeof createStagingAutomodDryRunAdapter>[0] = {
    plannedPolicy: { ...policy },
    launch: async (issued, planned, before, signal) => {
      assert.deepEqual(issued, lease);
      assert.deepEqual(planned, policy);
      assert.equal(before, startupBefore);
      assert.equal(signal.aborted, false);
      history.push("launch");
      return true;
    },
    readRuntime: async (issued, signal) => {
      assert.deepEqual(issued, lease);
      assert.equal(signal.aborted, false);
      history.push("readback");
      return running;
    },
    fenceAndDrainRemote: async (issued, before, signal) => {
      assert.deepEqual(issued, lease);
      assert.equal(before, cleanupBefore);
      assert.equal(signal.aborted, false);
      history.push("drain");
      return drained;
    },
    stop: async (issued, before, signal) => {
      assert.deepEqual(issued, lease);
      assert.equal(before, cleanupBefore);
      assert.equal(signal.aborted, false);
      history.push("stop");
      return true;
    },
  };
  const create = () => createStagingAutomodDryRunAdapter(transport, () => at);
  return { history, transport, create, adapter: create(), time: (next: number) => { at = next; } };
}

test("launch requires a separate same-lease running policy readback", async () => {
  const { adapter, history } = fixture();
  assert.deepEqual(adapter.dryRun?.policy, policy);
  assert.ok(adapter.dryRun);
  await adapter.dryRun.start(lease, policy, startupBefore);
  assert.deepEqual(history, ["launch", "readback"]);
});

test("missing, unsafe, or drifted policy never reaches launch", async () => {
  const { adapter, create, transport, history } = fixture();
  delete transport.plannedPolicy;
  assert.equal(create().dryRun, undefined);
  assert.ok(adapter.dryRun);
  await assert.rejects(adapter.dryRun.start(lease, policy, startupBefore));
  for (const key of Object.keys(policy) as (keyof typeof policy)[]) {
    const unsafe = { ...policy, [key]: !policy[key] };
    transport.plannedPolicy = unsafe;
    assert.equal(create().dryRun, undefined);
    await assert.rejects(adapter.dryRun.start(lease, policy, startupBefore));
    transport.plannedPolicy = policy;
    await assert.rejects(adapter.dryRun.start(lease, unsafe as typeof policy, startupBefore));
  }
  for (const malformed of [{ ...policy, extra: false }, Object.create(policy), undefined]) {
    await assert.rejects(adapter.dryRun.start(lease, malformed, startupBefore));
  }
  assert.deepEqual(history, []);
});

test("invalid lease or cutoff refuses before transport", async () => {
  const { adapter, history, time } = fixture();
  assert.ok(adapter.dryRun);
  const invalid = [
    { ...lease, purpose: undefined }, { ...lease, voiceMove: true },
    { ...lease, phase: "preparing" }, { ...lease, environment: "production" },
    { ...lease, expiresAt: Infinity }, { ...lease, runId: "0" },
  ];
  for (const bad of invalid) {
    const input = bad as typeof lease;
    await assert.rejects(adapter.dryRun.start(input, policy, startupBefore));
    assert.equal(await adapter.readRuntime(input), undefined);
    assert.equal(await adapter.fenceAndDrain(input, cleanupBefore), undefined);
    assert.equal(await adapter.stop(input, cleanupBefore), false);
  }
  for (const before of [1_999, 2_000, 2_500.5, NaN, Infinity, Number.MAX_SAFE_INTEGER + 1]) {
    await assert.rejects(adapter.dryRun.start(lease, policy, before));
    assert.equal(await adapter.fenceAndDrain(lease, before), undefined);
    assert.equal(await adapter.stop(lease, before), false);
  }
  await assert.rejects(adapter.dryRun.start(lease, policy, lease.expiresAt));
  time(lease.expiresAt);
  await assert.rejects(adapter.dryRun.start(lease, policy, cleanupBefore));
  assert.deepEqual(history, []);
});

test("lost launch reply and unverified runtime keep startup failed", async () => {
  const { adapter, transport, history, time } = fixture();
  assert.ok(adapter.dryRun);
  const { launch } = transport;
  transport.launch = async () => undefined;
  await assert.rejects(adapter.dryRun.start(lease, policy, startupBefore));
  transport.launch = async () => { throw new Error("lost reply"); };
  await assert.rejects(adapter.dryRun.start(lease, policy, startupBefore));
  assert.deepEqual(history, []);
  transport.launch = launch;
  for (const observed of [
    undefined, stopped, { lease, running: true }, { ...running, running: "true" },
    { ...running, lease: { ...lease, runAttempt: lease.runAttempt + 1 } },
    { ...running, lease: { ...lease, purpose: undefined } },
    { ...running, policy: { ...policy, resume: true } },
    { ...running, policy: { ...policy, extra: false } },
  ]) {
    transport.readRuntime = async () => observed as typeof running | undefined;
    await assert.rejects(adapter.dryRun.start(lease, policy, startupBefore));
  }
  transport.readRuntime = async () => { time(startupBefore); return running; };
  await assert.rejects(adapter.dryRun.start(lease, policy, startupBefore));
  time(2_000);
  transport.launch = async () => { time(startupBefore); return true; };
  transport.readRuntime = async () => { history.push("late readback"); return running; };
  await assert.rejects(adapter.dryRun.start(lease, policy, startupBefore));
  assert.equal(history.includes("late readback"), false);
});

test("cleanup accepts authoritative drain and stopped readback after lease expiry", async () => {
  const { adapter, transport, history, time } = fixture();
  time(lease.expiresAt + 1);
  assert.deepEqual(await adapter.fenceAndDrain(lease, cleanupBefore), drained);
  transport.readRuntime = async (...args) => {
    assert.deepEqual(args[0], lease);
    history.push("readback");
    return stopped;
  };
  assert.equal(await adapter.stop(lease, cleanupBefore), true);
  assert.deepEqual(await adapter.readRuntime(lease), stopped);
  assert.deepEqual(history, ["drain", "stop", "readback", "readback"]);
});

test("lost launch times out without a readback or retry", async () => {
  const { adapter, transport, history } = fixture();
  assert.ok(adapter.dryRun);
  let requestSignal: AbortSignal | undefined;
  transport.launch = async (_issued, _policy, _before, signal) => {
    requestSignal = signal;
    history.push("launch");
    return new Promise(() => {});
  };
  // Node's AbortSignal timer is unref'ed. Keep the test alive until it fires.
  const timer = setTimeout(() => {}, 1_000);
  try {
    await assert.rejects(adapter.dryRun.start(lease, policy, 2_005));
    assert.equal(requestSignal?.aborted, true);
    assert.deepEqual(history, ["launch"]);
  } finally { clearTimeout(timer); }
});

test("partial, unbound, or late remote drain never clears cleanup", async () => {
  const { adapter, transport, time } = fixture();
  for (const proof of [
    undefined, { lease }, { ...drained, publicationDrained: false }, { ...drained, startupDrained: false },
    { ...drained, startupDrained: 1 },
    ...Object.entries({
      purpose: undefined, phase: "released", voiceMove: true,
      deploymentId: "build-2", sha: "b".repeat(40), epoch: 9, runId: "4", runAttempt: 5,
      acquiredAt: 1_001, expiresAt: 3_001,
    }).map(([key, value]) => ({ ...drained, lease: { ...lease, [key]: value } })),
  ]) {
    transport.fenceAndDrainRemote = async () => proof as typeof drained | undefined;
    assert.equal(await adapter.fenceAndDrain(lease, cleanupBefore), undefined);
  }
  transport.fenceAndDrainRemote = async () => { throw new DOMException("client aborted", "AbortError"); };
  assert.equal(await adapter.fenceAndDrain(lease, cleanupBefore), undefined);
  transport.fenceAndDrainRemote = async () => { time(cleanupBefore); return drained; };
  assert.equal(await adapter.fenceAndDrain(lease, cleanupBefore), undefined);
});

test("the drain deadline bounds a transport that ignores client cancellation", async () => {
  const { adapter, transport } = fixture();
  let requestSignal: AbortSignal | undefined;
  let finish: (proof: typeof drained) => void = () => {};
  transport.fenceAndDrainRemote = async (_issued, _before, signal) => {
    requestSignal = signal;
    return new Promise(resolve => { finish = resolve; });
  };
  // Node's AbortSignal timer is unref'ed. Keep the test alive until it fires.
  const timer = setTimeout(() => {}, 1_000);
  try {
    assert.equal(await adapter.fenceAndDrain(lease, 2_005), undefined);
    assert.equal(requestSignal?.aborted, true);
    finish(drained);
  } finally { clearTimeout(timer); }
});

test("stop acknowledgement alone cannot prove a stopped runtime", async () => {
  const { adapter, transport, history, time } = fixture();
  const { stop } = transport;
  transport.stop = async () => false;
  transport.readRuntime = async () => { history.push("unexpected readback"); return stopped; };
  assert.equal(await adapter.stop(lease, cleanupBefore), false);
  assert.deepEqual(history, []);
  transport.stop = stop;
  for (const observed of [
    undefined, running, { ...stopped, running: 0 },
    { ...stopped, lease: { ...lease, epoch: lease.epoch + 1 } },
    { ...stopped, lease: { ...lease, purpose: undefined } },
  ]) {
    transport.readRuntime = async () => observed as typeof stopped | undefined;
    assert.equal(await adapter.stop(lease, cleanupBefore), false);
  }
  transport.readRuntime = async () => { throw new Error("readback unavailable"); };
  assert.equal(await adapter.stop(lease, cleanupBefore), false);
  assert.equal(await adapter.readRuntime(lease), undefined);
  transport.readRuntime = async () => { time(cleanupBefore); return stopped; };
  assert.equal(await adapter.stop(lease, cleanupBefore), false);
});
