import assert from "node:assert/strict";
import { test } from "node:test";
import {
  evaluateStagingAutomodNonoverlapPreflight as evaluate,
  type StagingAutomodNonoverlapPreflightInput as Input,
} from "../scripts/staging-automod-nonoverlap-preflight.ts";
import { STAGING_AUTOMOD_PLAN } from "../src/staging-automod-plan.ts";
import { STAGING_AUTOMOD_TARGET as target } from "../src/staging-automod-recovery.ts";

const now = 100_000;
const pin = {
  sha: "a".repeat(40), deploymentId: "11111111-1111-1111-1111-111111111111", epoch: 3,
  workerDeploymentId: "22222222-2222-2222-2222-222222222222",
  containerApplicationId: "33333333-3333-3333-3333-333333333333",
  namespaceId: "b".repeat(32), rolloutId: "44444444-4444-4444-4444-444444444444", imageDigest: `sha256:${"c".repeat(64)}`,
};
const run = { runId: "12345", sha: pin.sha };
const reservation = Object.freeze({
  ...run, action: "acquire", environment: "staging", runAttempt: 1, expiresAt: now + 10_000,
  deploymentId: pin.deploymentId, expectedEpoch: pin.epoch,
} as const);
const lease = Object.freeze({
  ...run, environment: "staging", phase: "active", purpose: "automod-preview", runAttempt: 1,
  deploymentId: pin.deploymentId, epoch: pin.epoch, acquiredAt: now - 10, expiresAt: reservation.expiresAt,
} as const);
const plan = { ...STAGING_AUTOMOD_PLAN, enabled: true, notAfter: now + 20_000 };
const promotion = { observedAt: now, PRODUCTION_AUTO_PROMOTE_DISABLED: true };
const ownership = {
  observedAt: now, owner: { deploymentId: pin.deploymentId, epoch: pin.epoch, phase: "active" },
  sha: pin.sha, use: null, runtimeState: "stopped", stopVerified: true, cleanupPending: false,
} as const;
const inventory = {
  ...pin, observedAt: now, ownerPhase: "active", trafficPercent: 100, runtimePolicy: "bounded", rolloutStatus: "completed",
} as const;
const input = Object.freeze({ now, target, run, pin, plan, reservation, expectedLease: null, promotion, ownership, inventory } satisfies Input);

function refuses(candidate: Input, reason: string): void {
  assert.deepEqual(evaluate(candidate), { admitted: false, reasons: [reason] });
}

test("admits an idle staging singleton with fresh pinned readbacks", () => {
  const verdict = evaluate(input);
  assert.deepEqual(verdict, { admitted: true, target, purpose: "automod-preview", reservation });
  assert.ok(verdict.admitted && Object.isFrozen(verdict.reservation));
  assert.notEqual(verdict.reservation, reservation);
});

test("requires an explicit fresh automatic production promotion disable", () => {
  const { promotion: _promotion, ...missing } = input;
  refuses(missing, "production_auto_promotion_unconfirmed");
  refuses({ ...input, promotion: { observedAt: now } }, "production_auto_promotion_unconfirmed");
  refuses({ ...input, promotion: { ...promotion, PRODUCTION_AUTO_PROMOTE_DISABLED: false } }, "production_auto_promotion_unconfirmed");
  refuses({ ...input, promotion: Object.create(promotion) }, "production_auto_promotion_unconfirmed");
  refuses({ ...input, promotion: { ...promotion, PRODUCTION_AUTO_PROMOTE_DISABLED: "true" } } as unknown as Input,
    "production_auto_promotion_unconfirmed");
});

test("bounds every observation to the preceding sixty seconds", () => {
  const observedAt = now - 60_000;
  assert.equal(evaluate({ ...input, promotion: { ...promotion, observedAt }, ownership: { ...ownership, observedAt },
    inventory: { ...inventory, observedAt } }).admitted, true);
  for (const observedAt of [NaN, Infinity, -Infinity, -1, now - 60_001, now + 1]) {
    refuses({ ...input, promotion: { ...promotion, observedAt } }, "production_auto_promotion_unconfirmed");
    refuses({ ...input, ownership: { ...ownership, observedAt } }, "staging_ownership_readback_unconfirmed");
    refuses({ ...input, inventory: { ...inventory, observedAt } }, "staging_inventory_readback_unconfirmed");
  }
  for (const now of [NaN, Infinity, -Infinity, -1, 0.5]) refuses({ ...input, now }, "invalid_clock");
  const { ownership: _ownership, ...withoutOwnership } = input;
  const { inventory: _inventory, ...withoutInventory } = input;
  refuses(withoutOwnership, "staging_ownership_readback_unconfirmed");
  refuses(withoutInventory, "staging_inventory_readback_unconfirmed");
});

test("refuses competing uses and unresolved stop or expiry", () => {
  const { purpose: _purpose, ...generic } = lease;
  const snapshots = [
    { use: { ...generic, voiceMove: true } },
    { use: { ...generic, phase: "preparing" } },
    { use: { ...generic, expiresAt: now - 1 } },
    { use: { ...generic, phase: "released", runId: "67890" } },
    { use: { ...lease, phase: "preparing" } },
    { use: { ...lease, purpose: "pre-rollback" } },
    { use: { ...lease, voiceMove: true } },
    { use: undefined },
    { runtimeState: "starting" },
    { runtimeState: "running" },
    { runtimeState: "unknown" },
    { stopVerified: false },
    { cleanupPending: true },
    { cleanupPending: undefined },
  ];
  for (const snapshot of snapshots) {
    refuses({ ...input, ownership: { ...ownership, ...snapshot } } as Input, "staging_nonoverlap_unconfirmed");
  }
  const { stopVerified: _stopVerified, ...missingStop } = ownership;
  refuses({ ...input, ownership: Object.assign(Object.create(ownership), missingStop) }, "staging_nonoverlap_unconfirmed");
  const expired = { ...lease, acquiredAt: now - 100, expiresAt: now };
  refuses({ ...input, expectedLease: expired, ownership: { ...ownership, use: expired } }, "staging_nonoverlap_unconfirmed");
});

test("requires independent exact evidence for an active AutoMod reservation", () => {
  const snapshot = { ...ownership, use: lease };
  assert.equal(evaluate({ ...input, ownership: snapshot, expectedLease: lease }).admitted, true);
  refuses({ ...input, ownership: snapshot }, "staging_nonoverlap_unconfirmed");
  refuses({ ...input, expectedLease: lease }, "staging_nonoverlap_unconfirmed");
  for (const wrong of [{ acquiredAt: lease.acquiredAt - 1 }, { expiresAt: lease.expiresAt + 1 }, { runId: "12346" }]) {
    const other = { ...lease, ...wrong };
    refuses({ ...input, ownership: snapshot, expectedLease: other }, "staging_nonoverlap_unconfirmed");
    if (wrong.acquiredAt === undefined) {
      refuses({ ...input, ownership: { ...ownership, use: other }, expectedLease: other }, "staging_nonoverlap_unconfirmed");
    }
  }
  refuses({ ...input, ownership: { ...snapshot, observedAt: lease.acquiredAt - 1 }, expectedLease: lease },
    "staging_nonoverlap_unconfirmed");
});

test("requires confirmed stop and cleanup after a release and denies replay", () => {
  const released = { ...lease, phase: "released", runId: "67890" } as const;
  const snapshot = { ...ownership, use: released };
  assert.equal(evaluate({ ...input, ownership: snapshot }).admitted, true);
  refuses({ ...input, ownership: { ...snapshot, stopVerified: false } }, "staging_nonoverlap_unconfirmed");
  refuses({ ...input, ownership: { ...snapshot, cleanupPending: true } }, "staging_nonoverlap_unconfirmed");
  refuses({ ...input, ownership: { ...snapshot, use: { ...released, runId: run.runId } } }, "staging_nonoverlap_unconfirmed");
  refuses({ ...input, ownership: { ...snapshot, use: { ...released, acquiredAt: NaN } } }, "staging_nonoverlap_unconfirmed");
});

test("requires an enabled matching plan and a valid acquisition for the pinned run", () => {
  const { plan: _plan, ...disabledByDefault } = input;
  refuses(disabledByDefault, "staging_automod_reservation_refused");
  for (const patch of [{ enabled: false }, { notAfter: now }, { leaseMs: 1 }, { leaseMs: Infinity }]) {
    refuses({ ...input, plan: { ...plan, ...patch } }, "staging_automod_reservation_refused");
  }
  for (const patch of [{ applicationId: "other" }, { guildId: "other" }, { name: "other" }]) {
    const other = { ...target, ...patch } as typeof target;
    refuses({ ...input, target: other }, "staging_automod_reservation_refused");
    refuses({ ...input, plan: { ...plan, target: other } }, "staging_automod_reservation_refused");
  }
  for (const patch of [{ runId: "67890" }, { sha: "d".repeat(40) }, { sha: "short" }, { action: "stop" },
    { environment: "production" }, { voiceMove: true }, { purpose: "automod-preview" }, { runAttempt: 2 }, { expiresAt: now }]) {
    refuses({ ...input, reservation: { ...reservation, ...patch } }, "staging_automod_reservation_refused");
  }
});

test("requires exact serving inventory pins and active ownership parity", () => {
  for (const patch of [
    { deploymentId: pin.workerDeploymentId }, { sha: "d".repeat(40) }, { epoch: pin.epoch + 1 },
    { workerDeploymentId: pin.deploymentId }, { containerApplicationId: pin.deploymentId },
    { namespaceId: "d".repeat(32) }, { rolloutId: pin.deploymentId }, { imageDigest: `sha256:${"d".repeat(64)}` },
    { ownerPhase: "fenced" as const }, { trafficPercent: 99 }, { rolloutStatus: "pending" }, { runtimePolicy: "unbounded" },
  ]) {
    refuses({ ...input, inventory: { ...inventory, ...patch } }, "staging_serving_pin_mismatch");
  }
  refuses({ ...input, pin: { ...pin, imageDigest: "bad" }, inventory: { ...inventory, imageDigest: "bad" } },
    "staging_serving_pin_mismatch");
  for (const patch of [{ phase: "fenced" as const }, { deploymentId: pin.workerDeploymentId }, { epoch: pin.epoch + 1 }]) {
    refuses({ ...input, ownership: { ...ownership, owner: { ...ownership.owner, ...patch } } }, "staging_serving_pin_mismatch");
  }
  refuses({ ...input, ownership: { ...ownership, sha: "e".repeat(40) } }, "staging_serving_pin_mismatch");
  refuses({ ...input, reservation: { ...reservation, expectedEpoch: 4 } }, "staging_serving_pin_mismatch");
});
