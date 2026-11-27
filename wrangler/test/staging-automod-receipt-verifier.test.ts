import assert from "node:assert/strict";
import test from "node:test";
import { verifyStagingAutomodReceipt, type StagingAutomodQualificationPin } from "../scripts/staging-automod-receipt-verifier.ts";
import { runProtectedAutomodJob } from "../src/staging-automod-job.ts";

const now = 1_800_000_000_000;
const scope = {
  runId: "12345",
  runAttempt: 1,
  lease: {
    environment: "staging", purpose: "automod-preview", phase: "active", deploymentId: "22222222-2222-4222-8222-222222222222",
    sha: "a".repeat(40), epoch: 17, runId: "12345", runAttempt: 1, acquiredAt: now - 500, expiresAt: now + 1_000,
  },
} as const;
const pin: StagingAutomodQualificationPin = {
  runId: scope.runId,
  lease: scope.lease,
  serving: {
    workerDeploymentId: "11111111-1111-4111-8111-111111111111", workerVersionId: scope.lease.deploymentId,
    sourceSha: scope.lease.sha, containerApplicationId: "33333333-3333-4333-8333-333333333333",
    containerNamespaceId: "b".repeat(32), containerRolloutId: "44444444-4444-4444-8444-444444444444",
    containerImageDigest: `sha256:${"c".repeat(64)}`, ownerEpoch: scope.lease.epoch,
  },
  command: { id: "1212345678901234567", version: "1234567890123456789" },
};

function receipt() {
  const identity = { ...pin.serving, ownerPhase: "active", trafficPercent: 100,
    runtimePolicy: "bounded", rolloutStatus: "completed" };
  return {
    schema: "two-bot-staging-automod-qualification.v1",
    before: { scope: structuredClone(scope), identity: { ...identity }, observedAt: now - 400 },
    registry: { scope: structuredClone(scope), startedAt: now - 300, completedAt: now - 200,
      command: { ...pin.command, applicationId: "1469137636663758888", guildId: "1545644954272137297",
        name: "automod-preview", type: 1, default_member_permissions: "32", options: [
          { name: "text", type: 3, required: true, autocomplete: false, choiceCount: 0, min_length: 1, max_length: 2000 },
          { name: "mentions", type: 4, required: false, autocomplete: false, choiceCount: 0, min_value: 0, max_value: 100 },
          { name: "attachments", type: 3, required: false, autocomplete: false, choiceCount: 0, max_length: 1000 },
        ] },
    },
    after: { scope: structuredClone(scope), identity: { ...identity }, observedAt: now - 100 },
    router: { sourceSha: scope.lease.sha, name: "automod-preview", requiredPermission: 32,
      withManageGuild: "admitted", withoutManageGuild: "denied" },
    coldStartRefusal: { scope: structuredClone(scope), action: "start", startedAt: now - 700, completedAt: now - 600,
      response: { schema: "two-bot-staging-automod-job.v1", environment: "staging", observedAt: new Date(now - 650).toISOString(),
        verdict: "failed", code: "request_refused" },
      before: { lease: "absent", container: "stopped" }, after: { lease: "absent", container: "stopped" },
      effects: { leaseWrites: 0, containerAcquisitions: 0, containerInvocations: 0,
        containerStarts: 0, containerStops: 0, containerWrites: 0 },
    },
  };
}

const verify = (evidence: unknown) => verifyStagingAutomodReceipt(evidence, pin, now);

test("accepts the exact fresh staging qualification receipt", () => {
  assert.equal(verify(receipt()), true);
});

test("refuses any serving drift on either side of the registry read", () => {
  const changed = {
    workerDeploymentId: "55555555-5555-4555-8555-555555555555", workerVersionId: pin.serving.workerDeploymentId,
    sourceSha: "d".repeat(40), containerApplicationId: pin.serving.containerRolloutId,
    containerNamespaceId: "e".repeat(32), containerRolloutId: pin.serving.containerApplicationId,
    containerImageDigest: `sha256:${"f".repeat(64)}`, ownerEpoch: 18, ownerPhase: "standby",
    trafficPercent: 90, runtimePolicy: "always-on", rolloutStatus: "pending",
  };
  for (const side of ["before", "after"] as const) {
    for (const [key, value] of Object.entries(changed)) {
      const evidence = receipt();
      Object.assign(evidence[side].identity, { [key]: value });
      assert.equal(verify(evidence), false, `${side}.${key}`);
    }
  }
  const evidence = receipt();
  evidence.before.identity.containerRolloutId = changed.containerRolloutId;
  evidence.after.identity.containerRolloutId = changed.containerRolloutId;
  assert.equal(verify(evidence), false, "agreement alone does not establish the expected rollout");
});

test("requires exact command scope, revision and Manage Server enforcement", () => {
  const substitutions = { applicationId: pin.command.id, guildId: pin.command.id, name: "help", type: 2,
    id: pin.command.version, version: pin.command.id, default_member_permissions: "0" };
  for (const [key, value] of Object.entries(substitutions)) {
    const evidence = receipt();
    Object.assign(evidence.registry.command, { [key]: value });
    assert.equal(verify(evidence), false, key);
  }
  for (const [key, value] of Object.entries({ sourceSha: "d".repeat(40), name: "help", requiredPermission: 0,
    withManageGuild: "denied", withoutManageGuild: "admitted" })) {
    const evidence = receipt();
    Object.assign(evidence.router, { [key]: value });
    assert.equal(verify(evidence), false, key);
  }
});

test("requires all three exact preview options and their bounds", () => {
  const changes = [
    { index: 0, fields: { required: false } }, { index: 0, fields: { min_length: 0 } },
    { index: 0, fields: { max_length: 2001 } }, { index: 0, fields: { autocomplete: true } },
    { index: 1, fields: { type: 3 } }, { index: 1, fields: { min_value: -1 } },
    { index: 1, fields: { max_value: 101 } }, { index: 1, fields: { choiceCount: 1 } }, { index: 2, fields: { required: true } },
    { index: 2, fields: { max_length: 1001 } }, { index: 2, fields: { name: "member" } },
  ];
  for (const { index, fields } of changes) {
    const evidence = receipt();
    Object.assign(evidence.registry.command.options[index]!, fields);
    assert.equal(verify(evidence), false, JSON.stringify(fields));
  }
  const missing = receipt();
  missing.registry.command.options.pop();
  assert.equal(verify(missing), false);
  const duplicate = receipt();
  duplicate.registry.command.options[2] = duplicate.registry.command.options[0]!;
  assert.equal(verify(duplicate), false);
});

test("rejects extra receipt fields at every depth without reading accessors", () => {
  for (const inject of [
    (r: ReturnType<typeof receipt>) => Object.assign(r, { token: "synthetic" }),
    (r: ReturnType<typeof receipt>) => Object.assign(r.before.identity, { headers: {} }),
    (r: ReturnType<typeof receipt>) => Object.assign(r.registry.command, { content: "synthetic" }),
    (r: ReturnType<typeof receipt>) => Object.assign(r.registry.command.options[0]!, { value: "synthetic" }),
    (r: ReturnType<typeof receipt>) => Object.defineProperty(r.registry.command.options, "raw", { value: "synthetic" }),
    (r: ReturnType<typeof receipt>) => Object.assign(r.coldStartRefusal.response, { lease: scope.lease }),
  ]) {
    const evidence = receipt();
    inject(evidence);
    assert.equal(verify(evidence), false);
  }
  const evidence = receipt();
  let accessed = false;
  Object.defineProperty(evidence.before, "observedAt", { get() { accessed = true; return now - 400; } });
  assert.equal(verify(evidence), false);
  assert.equal(accessed, false);
});

test("rejects spliced run, later attempt and changed lease observations", () => {
  for (const section of ["before", "registry", "after", "coldStartRefusal"] as const) {
    for (const patch of [{ runId: "98765" }, { runAttempt: 2 }]) {
      const evidence = receipt();
      Object.assign(evidence[section].scope, patch);
      assert.equal(verify(evidence), false, section);
    }
    for (const patch of [{ epoch: 18 }, { phase: "stopped" }, { acquiredAt: now - 501 }, { expiresAt: now + 999 },
      { deploymentId: pin.serving.workerDeploymentId }, { sha: "d".repeat(40) }, { runAttempt: 2 },
      { runId: "98765" }, { purpose: "voice" }, { environment: "production" }]) {
      const evidence = receipt();
      Object.assign(evidence[section].scope.lease, patch);
      assert.equal(verify(evidence), false, `${section}: ${Object.keys(patch)[0]}`);
    }
  }
  assert.equal(verifyStagingAutomodReceipt(receipt(), { ...pin, runId: "98765" }, now), false);
});

test("requires a fresh ordered read fully bracketed by the active lease and serving samples", () => {
  const changes = [
    { section: "before", patch: { observedAt: now - 501 } }, { section: "before", patch: { observedAt: now - 299 } },
    { section: "registry", patch: { completedAt: now - 301 } }, { section: "registry", patch: { startedAt: now - 200 } },
    { section: "after", patch: { observedAt: now - 201 } }, { section: "after", patch: { observedAt: now + 1 } },
    { section: "after", patch: { observedAt: Infinity } }, { section: "after", patch: { observedAt: now - 0.5 } },
    { section: "coldStartRefusal", patch: { startedAt: now - 60_001 } },
    { section: "coldStartRefusal", patch: { completedAt: now - 499 } },
  ] as const;
  for (const { section, patch } of changes) {
    const evidence = receipt();
    Object.assign(evidence[section], patch);
    assert.equal(verify(evidence), false, section);
  }
  assert.equal(verifyStagingAutomodReceipt(receipt(), pin, pin.lease.expiresAt), false);
});

test("requires an unscoped pre-admission refusal and affirmative zero side effects", () => {
  const unrelated = receipt();
  unrelated.coldStartRefusal.action = "recover";
  assert.equal(verify(unrelated), false);
  for (const patch of [{ verdict: "passed" }, { code: "operation_unconfirmed" },
    { observedAt: new Date(now - 701).toISOString() }, { observedAt: new Date(now - 599).toISOString() },
    { observedAt: now - 650 }, { observedAt: new Date(now - 650).toISOString().replace("Z", "+00:00") },
    { environment: "production" }, { schema: "unknown" }]) {
    const evidence = receipt();
    Object.assign(evidence.coldStartRefusal.response, patch);
    assert.equal(verify(evidence), false);
  }
  for (const side of ["before", "after"] as const) {
    for (const patch of [{ lease: "active" }, { container: "running" }]) {
      const evidence = receipt();
      Object.assign(evidence.coldStartRefusal[side], patch);
      assert.equal(verify(evidence), false, side);
    }
  }
  for (const key of Object.keys(receipt().coldStartRefusal.effects)) {
    const evidence = receipt();
    Object.assign(evidence.coldStartRefusal.effects, { [key]: 1 });
    assert.equal(verify(evidence), false, key);
  }
  const missing = receipt();
  Reflect.deleteProperty(missing.coldStartRefusal, "effects");
  assert.equal(verify(missing), false);
});

test("accepts the exact v1 refusal emitted by the merged job adapter", async t => {
  t.mock.timers.enable({ apis: ["Date"], now: now - 650 });
  const response = await runProtectedAutomodJob({ action: "start", issuedLease: scope.lease }, undefined,
    async () => assert.fail("refusal reached the private caller"));
  const evidence = receipt();
  assert.deepEqual(response, evidence.coldStartRefusal.response);
  assert.equal(verify({ ...evidence, coldStartRefusal: { ...evidence.coldStartRefusal, response } }), true);
});
