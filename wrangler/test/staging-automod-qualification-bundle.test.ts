import assert from "node:assert/strict";
import test from "node:test";
import { composeStagingAutomodQualificationBundle as compose } from "../scripts/staging-automod-qualification-bundle.ts";
import type { StagingAutomodQualificationPin } from "../scripts/staging-automod-receipt-verifier.ts";
import { STAGING_AUTOMOD_TARGET } from "../src/staging-automod-recovery.ts";

const now = 1_800_000_000_000;
const schema = "two-bot-staging-automod-qualification.v1";
const common = { schema, environment: "staging", observedAt: new Date(now).toISOString() };
const unverified = { ...common, verdict: "failed", code: "receipt_unverified" };
const unconfirmed = { ...common, verdict: "failed", code: "rehearsal_unconfirmed" };
const scope = {
  runId: "12345",
  runAttempt: 1,
  lease: {
    environment: "staging", purpose: "automod-preview", phase: "active", runId: "12345", runAttempt: 1,
    deploymentId: "22222222-2222-4222-8222-222222222222", sha: "a".repeat(40), epoch: 17,
    acquiredAt: now - 500, expiresAt: now + 120_000,
  },
} as const;
const pin = {
  runId: scope.runId,
  lease: scope.lease,
  serving: {
    workerDeploymentId: "11111111-1111-4111-8111-111111111111", workerVersionId: scope.lease.deploymentId,
    sourceSha: scope.lease.sha, containerApplicationId: "33333333-3333-4333-8333-333333333333",
    containerNamespaceId: "b".repeat(32), containerRolloutId: "44444444-4444-4444-8444-444444444444",
    containerImageDigest: `sha256:${"c".repeat(64)}`, ownerEpoch: scope.lease.epoch,
  },
  command: { id: "1212345678901234567", version: "1234567890123456789" },
} satisfies StagingAutomodQualificationPin;
const rehearsal = {
  verdict: "passed", start: true, recover: true, runId: scope.runId, sha: scope.lease.sha,
  deploymentId: scope.lease.deploymentId,
};
const passed = {
  ...common, verdict: "passed", runId: pin.runId, sha: pin.lease.sha,
  workerDeploymentId: pin.serving.workerDeploymentId, workerVersionId: pin.serving.workerVersionId,
  containerApplicationId: pin.serving.containerApplicationId, containerNamespaceId: pin.serving.containerNamespaceId,
  containerRolloutId: pin.serving.containerRolloutId, containerImageDigest: pin.serving.containerImageDigest,
  ownerEpoch: pin.serving.ownerEpoch, command: pin.command,
};

function receipt() {
  const identity = { ...pin.serving, ownerPhase: "active", trafficPercent: 100,
    runtimePolicy: "bounded", rolloutStatus: "completed" };
  return {
    schema,
    before: { scope, identity: { ...identity }, observedAt: now - 400 },
    registry: {
      scope, startedAt: now - 300, completedAt: now - 200,
      command: { ...pin.command, ...STAGING_AUTOMOD_TARGET, type: 1, default_member_permissions: "32", options: [
        { name: "text", type: 3, required: true, autocomplete: false, choiceCount: 0, min_length: 1, max_length: 2000 },
        { name: "mentions", type: 4, required: false, autocomplete: false, choiceCount: 0, min_value: 0, max_value: 100 },
        { name: "attachments", type: 3, required: false, autocomplete: false, choiceCount: 0, max_length: 1000 },
      ] },
    },
    after: { scope, identity: { ...identity }, observedAt: now - 100 },
    router: { sourceSha: scope.lease.sha, name: "automod-preview", requiredPermission: 32,
      withManageGuild: "admitted", withoutManageGuild: "denied" },
    coldStartRefusal: {
      scope, action: "start", startedAt: now - 700, completedAt: now - 600,
      response: { schema: "two-bot-staging-automod-job.v1", environment: "staging",
        observedAt: new Date(now - 650).toISOString(), verdict: "failed", code: "request_refused" },
      before: { lease: "absent", container: "stopped" }, after: { lease: "absent", container: "stopped" },
      effects: { leaseWrites: 0, containerAcquisitions: 0, containerInvocations: 0,
        containerStarts: 0, containerStops: 0, containerWrites: 0 },
    },
  };
}

function input() {
  return { enabled: true, pin, receipt: receipt(), rehearsal };
}

test("emits a frozen v1 bundle with the pinned staging identifiers", t => {
  t.mock.timers.enable({ apis: ["Date"], now });
  const result = compose(input());
  assert.deepEqual(result, passed);
  assert.ok(Object.isFrozen(result));
  assert.equal(result.verdict, "passed");
  assert.ok(Object.isFrozen(result.command));
  assert.notEqual(result.command, pin.command);
});

test("requires explicit true before inspecting the evidence", () => {
  let evidenceReads = 0;
  const disabled = { ...input(), enabled: false, get pin() { evidenceReads++; return pin; } };
  const expected = { ...common, verdict: "disabled" };
  const result = compose(disabled, now);
  assert.deepEqual(result, expected);
  assert.ok(Object.isFrozen(result));
  const { enabled, ...missing } = input();
  assert.equal(enabled, true);
  assert.deepEqual(compose(missing, now), expected);
  for (const value of ["true", 1]) {
    assert.deepEqual(compose(Object.assign(disabled, { enabled: value }), now), expected);
  }
  assert.equal(evidenceReads, 0);
});

test("fails closed on a tampered receipt or misbound pin", () => {
  const tampered = input();
  Object.assign(tampered.receipt.after.identity, { workerVersionId: pin.serving.workerDeploymentId });
  const result = compose(tampered, now);
  assert.deepEqual(result, unverified);
  assert.ok(Object.isFrozen(result));
  assert.deepEqual(compose({ ...input(), pin: { ...pin, runId: "98765" } }, now), unverified);
  assert.deepEqual(compose({ ...input(), pin: { ...pin, lease: { ...pin.lease, voiceMove: true } } }, now), unverified);
  assert.deepEqual(compose({ ...input(), pin: { ...pin, lease: Object.assign([], pin.lease) } }, now), unverified);
  assert.deepEqual(compose({ ...input(), receipt: null, rehearsal: null }, now), unverified);
});

test("fails closed on stale evidence while the lease is still active", () => {
  const boundary = now - 700 + 60_000;
  assert.deepEqual(compose(input(), boundary), { ...passed, observedAt: new Date(boundary).toISOString() });
  const stale = boundary + 1;
  assert.deepEqual(compose(input(), stale), { ...unverified, observedAt: new Date(stale).toISOString() });
});

test("fails closed on invalid clocks without echoing them", () => {
  for (const invalid of [NaN, Infinity, -1, now + 0.5, 8_640_000_000_000_001]) {
    assert.deepEqual(compose(input(), invalid), { ...unverified, observedAt: null });
  }
});

test("requires a passed rehearsal with literal start and recover confirmations", () => {
  const { recover, ...missingRecovery } = rehearsal;
  assert.equal(recover, true);
  for (const outcome of [null, missingRecovery, { ...rehearsal, verdict: "failed" }, { ...rehearsal, start: false },
    { ...rehearsal, start: 1 }, { ...rehearsal, recover: false }, { ...rehearsal, recover: "true" }]) {
    const result = compose({ ...input(), rehearsal: outcome }, now);
    assert.deepEqual(result, unconfirmed);
    assert.ok(Object.isFrozen(result));
  }
});

test("refuses a rehearsal from another run, SHA or Worker version", () => {
  for (const patch of [{ runId: "98765" }, { sha: "d".repeat(40) }, { deploymentId: pin.serving.workerDeploymentId }]) {
    assert.deepEqual(compose({ ...input(), rehearsal: { ...rehearsal, ...patch } }, now), unconfirmed);
  }
});

test("refuses inherited, accessor and unreadable rehearsal fields", () => {
  let getterReads = 0;
  const accessor = { ...rehearsal, get recover() { getterReads++; return true; } };
  const unreadable = new Proxy(rehearsal, { getOwnPropertyDescriptor() { throw Error("synthetic failure"); } });
  for (const outcome of [Object.create(rehearsal), accessor, unreadable]) {
    assert.deepEqual(compose({ ...input(), rehearsal: outcome }, now), unconfirmed);
  }
  assert.equal(getterReads, 0);
});

test("strips extra fields on success and keeps them out of failures", () => {
  const sentinel = "synthetic private material";
  const extras = { token: sentinel, credentials: { value: sentinel }, headers: { authorization: sentinel } };
  const supplied = { ...input(), ...extras, pin: { ...pin, ...extras, lease: { ...pin.lease, ...extras },
    serving: { ...pin.serving, ...extras }, command: { ...pin.command, ...extras } }, rehearsal: { ...rehearsal, ...extras } };
  const outcomes = [compose(supplied, now), compose({ ...supplied, enabled: false }, now),
    compose({ ...supplied, receipt: { ...receipt(), ...extras } }, now),
    compose({ ...supplied, rehearsal: { ...supplied.rehearsal, runId: sentinel } }, now)];
  assert.deepEqual(outcomes, [passed, { ...common, verdict: "disabled" }, unverified, unconfirmed]);
  assert.ok(!JSON.stringify(outcomes).includes(sentinel));
});

test("publishes the verified snapshot if a receipt read changes the original pin", () => {
  const mutablePin = structuredClone(pin);
  const changingReceipt = new Proxy(receipt(), {
    getOwnPropertyDescriptor(target, key) {
      mutablePin.command.id = "synthetic private material";
      return Reflect.getOwnPropertyDescriptor(target, key);
    },
  });
  assert.deepEqual(compose({ ...input(), pin: mutablePin, receipt: changingReceipt }, now), passed);
  assert.equal(mutablePin.command.id, "synthetic private material");
});
