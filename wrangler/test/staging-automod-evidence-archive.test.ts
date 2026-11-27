import assert from "node:assert/strict";
import test from "node:test";
import {
  sealStagingAutomodQualificationArchive as seal,
  verifyStagingAutomodQualificationArchive as verify,
} from "../scripts/staging-automod-evidence-archive.ts";
import type { StagingAutomodQualificationPin } from "../scripts/staging-automod-receipt-verifier.ts";
import { STAGING_AUTOMOD_TARGET } from "../src/staging-automod-recovery.ts";

const now = 1_800_000_000_000;
const lease = {
  environment: "staging", purpose: "automod-preview", phase: "active", runId: "12345", runAttempt: 1,
  deploymentId: "22222222-2222-4222-8222-222222222222", sha: "a".repeat(40), epoch: 17,
  acquiredAt: now - 500, expiresAt: now + 1_000,
} as const;

function fixture() {
  const pin = {
    runId: lease.runId,
    lease: { ...lease },
    serving: {
      workerDeploymentId: "11111111-1111-4111-8111-111111111111", workerVersionId: lease.deploymentId,
      sourceSha: lease.sha, containerApplicationId: "33333333-3333-4333-8333-333333333333",
      containerNamespaceId: "b".repeat(32), containerRolloutId: "44444444-4444-4444-8444-444444444444",
      containerImageDigest: `sha256:${"c".repeat(64)}`, ownerEpoch: lease.epoch,
    },
    command: { id: "1212345678901234567", version: "1234567890123456789" },
  } satisfies StagingAutomodQualificationPin;
  const scope = { runId: lease.runId, runAttempt: 1, lease };
  const identity = { ...pin.serving, ownerPhase: "active", trafficPercent: 100,
    runtimePolicy: "bounded", rolloutStatus: "completed" };
  const receipt = {
    schema: "two-bot-staging-automod-qualification.v1",
    before: { scope, identity: { ...identity }, observedAt: now - 400 },
    registry: { scope, startedAt: now - 300, completedAt: now - 200,
      command: { ...pin.command, ...STAGING_AUTOMOD_TARGET, type: 1, default_member_permissions: "32", options: [
        { name: "text", type: 3, required: true, autocomplete: false, choiceCount: 0, min_length: 1, max_length: 2000 },
        { name: "mentions", type: 4, required: false, autocomplete: false, choiceCount: 0, min_value: 0, max_value: 100 },
        { name: "attachments", type: 3, required: false, autocomplete: false, choiceCount: 0, max_length: 1000 },
      ] } },
    after: { scope, identity: { ...identity }, observedAt: now - 100 },
    router: { sourceSha: lease.sha, name: STAGING_AUTOMOD_TARGET.name, requiredPermission: 32,
      withManageGuild: "admitted", withoutManageGuild: "denied" },
    coldStartRefusal: { scope, action: "start", startedAt: now - 700, completedAt: now - 600,
      response: { schema: "two-bot-staging-automod-job.v1", environment: "staging",
        observedAt: new Date(now - 650).toISOString(), verdict: "failed", code: "request_refused" },
      before: { lease: "absent", container: "stopped" }, after: { lease: "absent", container: "stopped" },
      effects: { leaseWrites: 0, containerAcquisitions: 0, containerInvocations: 0,
        containerStarts: 0, containerStops: 0, containerWrites: 0 },
    },
  };
  const evidenceInput = { expectedLease: { ...lease },
    attempt: { lease: { ...lease }, result: { verdict: "passed", start: true, recover: true } } };
  return { pin, receipt, evidenceInput, promotionDisabled: true };
}

function rejected(input: unknown, reason: string, time = now) {
  const archive = seal(input, time);
  assert.equal(archive.verdict, "rejected");
  assert.deepEqual(archive.reasons, [reason]);
  assert.deepEqual([archive.runId, archive.deploymentId, archive.sha, archive.epoch], [null, null, null, null]);
  assert.ok(Object.isFrozen(archive) && Object.isFrozen(archive.reasons));
  assert.equal(verify(JSON.parse(JSON.stringify(archive))), true);
  return archive;
}

test("seals and verifies a canonical frozen summary of the pinned run", () => {
  const { pin, receipt, evidenceInput, promotionDisabled } = fixture();
  const archive = seal({ pin, receipt, evidenceInput, promotionDisabled }, now);
  assert.deepEqual(archive, {
    verdict: "qualified", reasons: [], runId: lease.runId, deploymentId: lease.deploymentId, sha: lease.sha,
    epoch: lease.epoch, archiveDigest: "db951228128e4425ec959ef45774b377be817a5e4108a0b8138ca54217c59415",
  });
  assert.ok(Object.isFrozen(archive) && Object.isFrozen(archive.reasons));
  assert.equal(verify(JSON.parse(JSON.stringify(archive))), true);
  assert.equal(verify(Object.fromEntries(Object.entries(archive).reverse())), true);
  assert.deepEqual(seal({ promotionDisabled, evidenceInput, receipt, pin }, now), archive);
});

test("rejects a job from another lease even when both evidence leases agree", () => {
  const input = fixture();
  const otherLease = { ...lease, acquiredAt: lease.acquiredAt - 1 };
  rejected({ ...input, evidenceInput: { expectedLease: otherLease, attempt: input.evidenceInput.attempt } }, "lease_mismatch");
  Object.assign(input.evidenceInput.attempt.lease, otherLease);
  rejected(input, "lease_mismatch");
  Object.assign(input.evidenceInput.expectedLease, otherLease);
  rejected(input, "lease_mismatch");
});

test("requires affirmative promotion disable and job confirmations", () => {
  rejected({ ...fixture(), promotionDisabled: false }, "promotion_not_disabled");
  rejected({ ...fixture(), promotionDisabled: "true" }, "promotion_not_disabled");
  for (const result of [
    { verdict: "failed", start: true, recover: false },
    { verdict: "passed", start: false, recover: true },
    { verdict: "failed", code: "lease_unconfirmed" },
  ]) {
    const input = fixture();
    rejected({ ...input, evidenceInput: { ...input.evidenceInput, attempt: { lease, result } } }, "job_evidence_failed");
  }
});

test("requires the exact fresh receipt and a valid clock", () => {
  const input = fixture();
  rejected(input, "receipt_invalid", lease.expiresAt);
  const stale = fixture();
  stale.receipt.coldStartRefusal.startedAt = now - 60_001;
  rejected(stale, "receipt_invalid");
  rejected(input, "invalid_input", NaN);
  rejected(input, "invalid_input", now + 0.5);
  assert.equal(seal(input, undefined as unknown as number).verdict, "rejected");
  input.receipt.router.withoutManageGuild = "admitted";
  rejected(input, "receipt_invalid");
  const malformed = fixture();
  malformed.receipt.before.observedAt = Infinity;
  rejected(malformed, "receipt_invalid");
  malformed.receipt.before.observedAt = now - 400;
  malformed.receipt.coldStartRefusal.response.observedAt = "invalid date";
  rejected(malformed, "receipt_invalid");
});

test("rejects unknown keys, missing fields, non-plain objects and accessors", () => {
  const secret = "synthetic credential";
  const { receipt, pin, evidenceInput } = fixture();
  rejected({ receipt, pin, evidenceInput }, "invalid_input");
  for (const inject of [
    (input: ReturnType<typeof fixture>) => input,
    (input: ReturnType<typeof fixture>) => input.pin,
    (input: ReturnType<typeof fixture>) => input.pin.lease,
    (input: ReturnType<typeof fixture>) => input.pin.serving,
    (input: ReturnType<typeof fixture>) => input.pin.command,
    (input: ReturnType<typeof fixture>) => input.evidenceInput,
    (input: ReturnType<typeof fixture>) => input.evidenceInput.attempt,
    (input: ReturnType<typeof fixture>) => input.evidenceInput.attempt.lease,
    (input: ReturnType<typeof fixture>) => input.evidenceInput.attempt.result,
  ]) {
    const input = fixture();
    Object.assign(inject(input), { extra: secret });
    assert.ok(!JSON.stringify(rejected(input, "invalid_input")).includes(secret));
  }
  const extraReceipt = fixture();
  Object.assign(extraReceipt.receipt.registry.command, { extra: secret });
  assert.ok(!JSON.stringify(rejected(extraReceipt, "receipt_invalid")).includes(secret));
  const inherited = fixture();
  Object.setPrototypeOf(inherited.receipt.before, null);
  rejected(inherited, "invalid_input");
  const accessor = fixture();
  let accessed = false;
  Object.defineProperty(accessor.pin.lease, "epoch", { get() { accessed = true; return lease.epoch; } });
  rejected(accessor, "invalid_input");
  assert.equal(accessed, false);
});

test("rejects changes to any envelope field or its exact schema", () => {
  const archive = seal(fixture(), now);
  for (const patch of [
    { verdict: "rejected" }, { reasons: ["invalid_input"] }, { runId: "12346" },
    { deploymentId: "55555555-5555-4555-8555-555555555555" }, { sha: "d".repeat(40) }, { epoch: lease.epoch + 1 },
    { archiveDigest: "0".repeat(64) }, { extra: "unknown" },
  ]) {
    assert.equal(verify({ ...archive, ...patch }), false, Object.keys(patch).join(", "));
  }
  assert.equal(verify(Object.assign(Object.create(null), archive)), false);
  assert.equal(verify(Object.defineProperty({ ...archive }, Symbol("extra"), { value: true })), false);
  assert.equal(verify({ ...archive, reasons: Object.assign([], { extra: true }) }), false);
  const missing: Partial<typeof archive> = { ...archive };
  Reflect.deleteProperty(missing, "epoch");
  assert.equal(verify(missing), false);
  const rejection = seal({ ...fixture(), promotionDisabled: false }, now);
  assert.equal(verify({ ...rejection, reasons: ["receipt_invalid"] }), false);
});
