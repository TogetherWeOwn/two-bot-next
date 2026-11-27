import assert from "node:assert/strict";
import test from "node:test";
import { verifyStagingAutomodFreshness as verify } from "../scripts/staging-automod-freshness-verifier.ts";
import { composeStagingAutomodQualificationBundle } from "../scripts/staging-automod-qualification-bundle.ts";
import type { StagingAutomodQualificationPin } from "../scripts/staging-automod-receipt-verifier.ts";
import { STAGING_AUTOMOD_TARGET } from "../src/staging-automod-recovery.ts";

const observedAt = 1_800_000_000_000;
const schema = "two-bot-staging-automod-qualification.v1";
const lease = {
  environment: "staging", purpose: "automod-preview", phase: "active", runId: "12345", runAttempt: 1,
  deploymentId: "22222222-2222-4222-8222-222222222222", sha: "a".repeat(40), epoch: 17,
  acquiredAt: observedAt - 500, expiresAt: observedAt + 120_000,
} as const;

function composeBundle() {
  const pin = {
    runId: lease.runId,
    lease,
    serving: {
      workerDeploymentId: "11111111-1111-4111-8111-111111111111", workerVersionId: lease.deploymentId,
      sourceSha: lease.sha, ownerEpoch: lease.epoch, containerApplicationId: "33333333-3333-4333-8333-333333333333",
      containerNamespaceId: "b".repeat(32), containerRolloutId: "44444444-4444-4444-8444-444444444444",
      containerImageDigest: `sha256:${"c".repeat(64)}`,
    },
    command: { id: "1212345678901234567", version: "1234567890123456789" },
  } satisfies StagingAutomodQualificationPin;
  const scope = { runId: lease.runId, runAttempt: lease.runAttempt, lease };
  const identity = { ...pin.serving, ownerPhase: "active", trafficPercent: 100,
    runtimePolicy: "bounded", rolloutStatus: "completed" };
  const stopped = { lease: "absent", container: "stopped" };
  const receipt = {
    schema,
    before: { scope, identity, observedAt: observedAt - 400 },
    registry: {
      scope, startedAt: observedAt - 300, completedAt: observedAt - 200,
      command: { ...pin.command, ...STAGING_AUTOMOD_TARGET, type: 1, default_member_permissions: "32", options: [
        { name: "text", type: 3, required: true, autocomplete: false, choiceCount: 0, min_length: 1, max_length: 2000 },
        { name: "mentions", type: 4, required: false, autocomplete: false, choiceCount: 0, min_value: 0, max_value: 100 },
        { name: "attachments", type: 3, required: false, autocomplete: false, choiceCount: 0, max_length: 1000 },
      ] },
    },
    after: { scope, identity, observedAt: observedAt - 100 },
    router: { sourceSha: lease.sha, name: STAGING_AUTOMOD_TARGET.name, requiredPermission: 32,
      withManageGuild: "admitted", withoutManageGuild: "denied" },
    coldStartRefusal: {
      scope, action: "start", startedAt: observedAt - 700, completedAt: observedAt - 600,
      response: { schema: "two-bot-staging-automod-job.v1", environment: "staging",
        observedAt: new Date(observedAt - 650).toISOString(), verdict: "failed", code: "request_refused" },
      before: stopped, after: stopped,
      effects: { leaseWrites: 0, containerAcquisitions: 0, containerInvocations: 0,
        containerStarts: 0, containerStops: 0, containerWrites: 0 },
    },
  };
  const rehearsal = { verdict: "passed", start: true, recover: true, runId: lease.runId, sha: lease.sha,
    deploymentId: lease.deploymentId };
  const bundle = composeStagingAutomodQualificationBundle({ enabled: true, pin, receipt, rehearsal }, observedAt);
  assert.equal(bundle.verdict, "passed");
  return bundle;
}

const bundle = composeBundle();
const input = { enabled: true, bundle, lease, now: observedAt };
const disabled = { verdict: "disabled" };
const failed = { verdict: "failed" };
const passed = { verdict: "passed" };

test("defaults off without inspecting timestamps or running accessors", () => {
  let reads = 0;
  const unreadable = () => { reads++; throw Error("unexpected getter"); };
  const evidence = Object.defineProperties({}, {
    bundle: { get: unreadable }, lease: { get: unreadable }, now: { get: unreadable },
    maxLeaseWindowMs: { get: unreadable },
  });
  for (const enabled of [undefined, false, "true", 1, null]) {
    const supplied = Object.defineProperty(evidence, "enabled", { value: enabled, configurable: true });
    assert.deepEqual(verify(supplied), disabled);
  }
  assert.deepEqual(verify(Object.defineProperty(evidence, "enabled", { get: unreadable })), disabled);
  assert.deepEqual(verify(Object.create(input)), disabled);
  assert.deepEqual(verify(), disabled);
  assert.ok(Object.isFrozen(verify()));
  assert.equal(reads, 0);
});

test("passes a real composed staging qualification bundle with its pinned lease", () => {
  const result = verify(input);
  assert.deepEqual(result, passed);
  assert.ok(Object.isFrozen(result));
});

test("enforces the inclusive age and skew bounds", () => {
  assert.deepEqual(verify({ ...input, now: observedAt + 60_000 }), passed);
  assert.deepEqual(verify({ ...input, now: observedAt + 60_001 }), failed);
  assert.deepEqual(verify({ ...input, now: observedAt - 5_000 }), passed);
  assert.deepEqual(verify({ ...input, now: observedAt - 5_001 }), failed);
});

test("enforces acquisition and exclusive expiry for observation and clock", () => {
  assert.deepEqual(verify({ ...input, lease: { acquiredAt: observedAt, expiresAt: observedAt + 1 } }), passed);
  assert.deepEqual(verify({ ...input, lease: { ...lease, acquiredAt: observedAt + 1 } }), failed);
  assert.deepEqual(verify({ ...input, lease: { ...lease, expiresAt: observedAt }, now: observedAt - 1 }), failed);

  const shortLease = { ...lease, expiresAt: observedAt + 100 };
  assert.deepEqual(verify({ ...input, lease: shortLease, now: shortLease.expiresAt - 1 }), passed);
  assert.deepEqual(verify({ ...input, lease: shortLease, now: shortLease.expiresAt }), failed);
  assert.deepEqual(verify({ ...input, lease: shortLease, now: shortLease.expiresAt + 1 }), failed);
  assert.deepEqual(verify({ ...input, lease: { acquiredAt: observedAt, expiresAt: observedAt } }), failed);
});

test("enforces the default and configured positive lease windows", () => {
  const exact = { acquiredAt: observedAt, expiresAt: observedAt + 300_000 };
  const tooLong = { ...exact, expiresAt: exact.expiresAt + 1 };
  assert.deepEqual(verify({ ...input, lease: exact }), passed);
  assert.deepEqual(verify({ ...input, lease: tooLong }), failed);
  assert.deepEqual(verify({ ...input, lease: tooLong, maxLeaseWindowMs: 300_001 }), passed);
  assert.deepEqual(verify({ ...input, lease: exact, maxLeaseWindowMs: 299_999 }), failed);
  for (const maxLeaseWindowMs of [NaN, Infinity, 0, -1, 1.5, Number.MAX_SAFE_INTEGER + 1]) {
    assert.deepEqual(verify({ ...input, maxLeaseWindowMs }), failed);
  }
});

test("requires the exact passed staging bundle schema", () => {
  for (const patch of [{ schema: "two-bot.staging-automod-qualification.v1" }, { schema: undefined },
    { verdict: "failed" }, { verdict: "disabled" }, { environment: "production" }]) {
    const result = verify({ ...input, bundle: { ...bundle, ...patch } });
    assert.deepEqual(result, failed);
    assert.ok(Object.isFrozen(result));
  }
});

test("requires a canonical UTC observation and safe epoch milliseconds", () => {
  for (const invalid of [undefined, null, NaN, observedAt, "2027-02-30T00:00:00.000Z",
    "2027-01-15T08:00:00Z", "2027-01-15T08:00:00.000+00:00", "2027-01-15T08:00:00.000z",
    "2027-01-15T08:00:00.000Z\n", "2027-01-15T08:00:00.0000Z"]) {
    assert.deepEqual(verify({ ...input, bundle: { ...bundle, observedAt: invalid } }), failed);
  }
  const maxDate = 8_640_000_000_000_000;
  for (const now of [NaN, Infinity, -1, observedAt + 0.5, maxDate + 1]) {
    assert.deepEqual(verify({ ...input, now }), failed);
  }
  assert.deepEqual(verify({ enabled: true, bundle, lease }), failed);
  for (const invalid of [undefined, null, NaN, Infinity, -1, observedAt + 0.5, `${observedAt}`,
    Number.MAX_SAFE_INTEGER + 1]) {
    assert.deepEqual(verify({ ...input, lease: { ...lease, acquiredAt: invalid } }), failed);
    assert.deepEqual(verify({ ...input, lease: { ...lease, expiresAt: invalid } }), failed);
  }
  for (const now of [0, maxDate]) {
    assert.deepEqual(verify({ ...input, now, bundle: { ...bundle, observedAt: new Date(now).toISOString() },
      lease: { acquiredAt: now, expiresAt: now + 1 } }), passed);
  }
});

test("rejects accessors, non-plain records and proxies without invoking them", () => {
  let reads = 0;
  const unreadable = () => { reads++; throw Error("unexpected read"); };
  const traps = { getPrototypeOf: unreadable, getOwnPropertyDescriptor: unreadable, get: unreadable };
  const revoked = Proxy.revocable(bundle, {});
  revoked.revoke();
  for (const invalid of [null, Object.create(bundle), Object.assign([], bundle),
    Object.defineProperty({ ...bundle }, "observedAt", { get: unreadable }),
    Object.defineProperty({ ...bundle }, Symbol("accessor"), { get: unreadable }), new Proxy(bundle, traps), revoked.proxy]) {
    assert.deepEqual(verify({ ...input, bundle: invalid }), failed);
  }
  for (const invalid of [Object.create(lease), Object.assign(new Date(), lease),
    Object.defineProperty({ ...lease }, "expiresAt", { get: unreadable }), new Proxy(lease, traps)]) {
    assert.deepEqual(verify({ ...input, lease: invalid }), failed);
  }
  assert.deepEqual(verify(Object.assign(Object.create({ now: observedAt }), input)), failed);
  assert.deepEqual(verify(Object.defineProperty({ ...input }, "now", { get: unreadable })), failed);
  assert.deepEqual(verify(new Proxy(input, traps)), disabled);
  assert.equal(reads, 0);
});
