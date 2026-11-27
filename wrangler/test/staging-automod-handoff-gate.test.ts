import assert from "node:assert/strict";
import { registerHooks } from "node:module";
import test from "node:test";
import {
  evaluateStagingAutomodHandoffGate as evaluate,
  type StagingAutomodHandoffFailureCode,
  type StagingAutomodHandoffGateInput,
} from "../scripts/staging-automod-handoff-gate.ts";
import { composeStagingAutomodQualificationBundle } from "../scripts/staging-automod-qualification-bundle.ts";
import { STAGING_AUTOMOD_TARGET } from "../src/staging-automod-recovery.ts";

const now = 1_800_000_000_000;
const observedAt = now - 100;
const baselineFingerprint = "d".repeat(64);
const pin = {
  runId: "12345",
  sha: "a".repeat(40),
  workerDeploymentId: "11111111-1111-4111-8111-111111111111",
  workerVersionId: "22222222-2222-4222-8222-222222222222",
  containerApplicationId: "33333333-3333-4333-8333-333333333333",
  containerNamespaceId: "b".repeat(32),
  containerRolloutId: "44444444-4444-4444-8444-444444444444",
  containerImageDigest: `sha256:${"c".repeat(64)}`,
  ownerEpoch: 17,
  command: { id: "1212345678901234567", version: "1234567890123456789" },
};
const bundle = {
  ...pin,
  schema: "two-bot-staging-automod-qualification.v1",
  environment: "staging",
  verdict: "passed",
  observedAt: new Date(observedAt).toISOString(),
};
const restoration = {
  observedAt: now - 1,
  command: null,
  baselineFingerprint,
  activeLease: null,
  stagingUseReleased: true,
};
const input = {
  enabled: true,
  now,
  bundle,
  pin,
  expectedBaselineFingerprint: baselineFingerprint,
  restoration,
  productionAutoPromoteDisabled: true,
} satisfies StagingAutomodHandoffGateInput;

function fails(value: unknown, code: StagingAutomodHandoffFailureCode) {
  const result = evaluate(value as StagingAutomodHandoffGateInput);
  assert.deepEqual(result, { verdict: "failed", code });
  assert.ok(Object.isFrozen(result));
}

test("defaults to disabled and reads only explicit enablement", () => {
  const result = evaluate();
  assert.deepEqual(result, { verdict: "disabled" });
  assert.ok(Object.isFrozen(result));
  for (const enabled of [undefined, false, "true", 1]) {
    const fieldsRead: PropertyKey[] = [];
    const evidence = new Proxy({ ...input, enabled }, {
      getOwnPropertyDescriptor(target, key) {
        fieldsRead.push(key);
        return Reflect.getOwnPropertyDescriptor(target, key);
      },
    });
    assert.deepEqual(evaluate(evidence as StagingAutomodHandoffGateInput), result);
    assert.deepEqual(fieldsRead, ["enabled"]);
  }
  assert.deepEqual(evaluate(Object.create(input)), result);
});

test("passes the composed qualification bundle and confirmed restoration", () => {
  const { runId, sha, command, ...deployment } = pin;
  const deploymentId = pin.workerVersionId;
  const lease = { environment: "staging", purpose: "automod-preview", phase: "active", runId, runAttempt: 1,
    deploymentId, sha, epoch: pin.ownerEpoch, acquiredAt: observedAt - 500, expiresAt: observedAt + 120_000 } as const;
  const scope = { runId, runAttempt: 1, lease };
  const serving = { ...deployment, sourceSha: sha };
  const identity = { ...serving, ownerPhase: "active", trafficPercent: 100, runtimePolicy: "bounded",
    rolloutStatus: "completed" };
  const stopped = { lease: "absent", container: "stopped" };
  const receipt = {
    schema: bundle.schema,
    before: { scope, identity, observedAt: observedAt - 400 },
    registry: {
      scope, startedAt: observedAt - 300, completedAt: observedAt - 200,
      command: { ...command, ...STAGING_AUTOMOD_TARGET, type: 1, default_member_permissions: "32", options: [
        { name: "text", type: 3, required: true, autocomplete: false, choiceCount: 0, min_length: 1, max_length: 2000 },
        { name: "mentions", type: 4, required: false, autocomplete: false, choiceCount: 0, min_value: 0, max_value: 100 },
        { name: "attachments", type: 3, required: false, autocomplete: false, choiceCount: 0, max_length: 1000 },
      ] },
    },
    after: { scope, identity, observedAt: observedAt - 100 },
    router: { sourceSha: sha, name: STAGING_AUTOMOD_TARGET.name, requiredPermission: 32,
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
  const rehearsal = { verdict: "passed", start: true, recover: true, runId, sha, deploymentId };
  const composed = composeStagingAutomodQualificationBundle({
    enabled: true, pin: { runId, lease, serving, command }, receipt, rehearsal,
  }, observedAt);
  assert.equal(composed.verdict, "passed");
  const result = evaluate({ ...input, bundle: composed });
  assert.deepEqual(result, { verdict: "passed" });
  assert.ok(Object.isFrozen(result));
});

test("requires the pinned schema, staging environment and passed verdict", () => {
  for (const patch of [{ schema: "two-bot.staging-automod-qualification.v1" }, { environment: "production" },
    { verdict: "failed" }, { verdict: "disabled" }]) {
    fails({ ...input, bundle: { ...bundle, ...patch } }, "qualification_unconfirmed");
  }
  fails({ ...input, bundle: null }, "qualification_unconfirmed");
  fails({ ...input, bundle: Object.create(bundle) }, "qualification_unconfirmed");
});

test("refuses drift in every pinned identity field", () => {
  const drift = {
    runId: "98765",
    sha: "f".repeat(40),
    workerDeploymentId: pin.workerVersionId,
    workerVersionId: pin.workerDeploymentId,
    containerApplicationId: pin.containerRolloutId,
    containerNamespaceId: "e".repeat(32),
    containerRolloutId: pin.containerApplicationId,
    containerImageDigest: `sha256:${"e".repeat(64)}`,
    ownerEpoch: pin.ownerEpoch + 1,
    command: { ...pin.command, version: "1234567890123456790" },
  } satisfies typeof pin;
  for (const [key, value] of Object.entries(drift)) {
    fails({ ...input, bundle: { ...bundle, [key]: value } }, "qualification_pin_mismatch");
  }
  fails({ ...input, bundle: { ...bundle, command: { ...pin.command, id: pin.command.version } } },
    "qualification_pin_mismatch");
  for (const patch of [{ sha: undefined }, { runId: "" }, { ownerEpoch: 0 },
    { ownerEpoch: "17" }, { command: {} }]) {
    fails({ ...input, pin: { ...pin, ...patch }, bundle: { ...bundle, ...patch } }, "qualification_pin_mismatch");
  }
});

test("bounds qualification freshness and refuses invalid clocks and timestamps", () => {
  const at = (time: number) => ({ ...input, bundle: { ...bundle, observedAt: new Date(time).toISOString() } });
  assert.deepEqual(evaluate(at(now - 60_000)), { verdict: "passed" });
  fails(at(now - 60_001), "qualification_time_invalid");
  fails(at(now + 1), "qualification_time_invalid");
  for (const value of [undefined, null, observedAt, "2030-02-30T00:00:00.000Z", "2027-01-15T07:59:59.900z"]) {
    fails({ ...input, bundle: { ...bundle, observedAt: value } }, "qualification_time_invalid");
  }
  for (const value of [undefined, NaN, Infinity, -1, now + 0.5, 8_640_000_000_000_001]) {
    fails({ ...input, now: value }, "qualification_time_invalid");
  }
});

test("requires a restoration readback from the qualification window", () => {
  assert.deepEqual(evaluate({ ...input, restoration: { ...restoration, observedAt } }), { verdict: "passed" });
  for (const time of [undefined, observedAt - 1, now + 1, NaN]) {
    fails({ ...input, restoration: { ...restoration, observedAt: time } }, "restoration_time_invalid");
  }
  fails({ ...input, restoration: undefined }, "restoration_time_invalid");
});

test("requires explicit command absence and a matching baseline fingerprint", () => {
  for (const command of [undefined, pin.command]) {
    fails({ ...input, restoration: { ...restoration, command } }, "command_absence_unconfirmed");
  }
  for (const value of [undefined, "e".repeat(64)]) {
    fails({ ...input, restoration: { ...restoration, baselineFingerprint: value } }, "baseline_unconfirmed");
  }
  fails({ ...input, expectedBaselineFingerprint: "", restoration: { ...restoration, baselineFingerprint: "" } },
    "baseline_unconfirmed");
});

test("requires lease clearance and explicit staging use release", () => {
  for (const activeLease of [undefined, { runId: pin.runId }]) {
    fails({ ...input, restoration: { ...restoration, activeLease } }, "lease_clearance_unconfirmed");
  }
  for (const stagingUseReleased of [undefined, false, "true"]) {
    fails({ ...input, restoration: { ...restoration, stagingUseReleased } }, "staging_use_release_unconfirmed");
  }
});

test("requires literal confirmation that production auto-promotion is disabled", () => {
  for (const productionAutoPromoteDisabled of [undefined, false, "true"]) {
    fails({ ...input, productionAutoPromoteDisabled }, "production_auto_promote_not_disabled");
  }
});

test("refuses handoff if the repository staging plan is enabled", async () => {
  const gateUrl = new URL("../scripts/staging-automod-handoff-gate.ts?enabled-plan", import.meta.url).href;
  const hooks = registerHooks({
    resolve(specifier, context, nextResolve) {
      if (context.parentURL === gateUrl && specifier === "../src/staging-automod-plan.ts") {
        const source = "export const STAGING_AUTOMOD_PLAN = Object.freeze({ enabled: true });";
        return { url: `data:text/javascript,${encodeURIComponent(source)}`, shortCircuit: true };
      }
      return nextResolve(specifier, context);
    },
  });
  try {
    const gate: typeof import("../scripts/staging-automod-handoff-gate.ts") = await import(gateUrl);
    const result = gate.evaluateStagingAutomodHandoffGate(input);
    assert.deepEqual(result, { verdict: "failed", code: "staging_automod_plan_enabled" });
    assert.ok(Object.isFrozen(result));
  } finally {
    hooks.deregister();
  }
});

test("rejects accessor and unreadable evidence with bounded failures", () => {
  let getterReads = 0;
  const accessor = { ...bundle, get workerVersionId() { getterReads++; return pin.workerVersionId; } };
  fails({ ...input, bundle: accessor }, "qualification_pin_mismatch");
  assert.equal(getterReads, 0);
  const unreadable = new Proxy(restoration, { getOwnPropertyDescriptor() { throw Error("synthetic private detail"); } });
  fails({ ...input, restoration: unreadable }, "restoration_time_invalid");
});
