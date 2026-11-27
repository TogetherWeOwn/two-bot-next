import { STAGING_AUTOMOD_PLAN, automodReservationAllowed, type StagingAutomodPlan } from "../src/staging-automod-plan.ts";
import { sameLease, STAGING_AUTOMOD_TARGET } from "../src/staging-automod-recovery.ts";
import { parseStagingUseChange, type StagingUse, type StagingUseChange } from "../src/staging-lifecycle.ts";
import { AUTOMOD_STAGING_PURPOSE, MAX_STAGING_USE_MS } from "../src/staging-use-window.ts";

type Target = typeof STAGING_AUTOMOD_TARGET;
type Lease = Readonly<StagingUse>;
type ServingPin = Readonly<{
  // Staging lifecycle uses the Worker version ID as its deploymentId.
  deploymentId: string;
  sha: string;
  epoch: number;
  workerDeploymentId: string;
  containerApplicationId: string;
  namespaceId: string;
  rolloutId: string;
  imageDigest: string;
}>;

type OwnershipReadback = Readonly<{
  observedAt: number;
  owner: Pick<ServingPin, "deploymentId" | "epoch"> & Readonly<{ phase: "active" | "fenced" }>;
  sha: string;
  use: Lease | null;
  runtimeState: "stopped" | "starting" | "running" | "unknown";
  stopVerified: boolean;
  cleanupPending: boolean;
  requestedAttempt: Readonly<{
    runId: string;
    runAttempt: number;
    /** The lifecycle's per-attempt marker, or null for verified absence. */
    usedUntil: number | null;
  }>;
}>;

type InventoryReadback = ServingPin & Readonly<{
  observedAt: number;
  ownerPhase: "active" | "fenced";
  trafficPercent: number;
  runtimePolicy: string;
  rolloutStatus: string;
}>;

export type StagingAutomodNonoverlapPreflightInput = Readonly<{
  now: number;
  target: Target;
  // The trusted caller obtains the run and pin independently of the readbacks.
  run: Readonly<{ runId: string; sha: string }>;
  pin: ServingPin;
  plan?: StagingAutomodPlan;
  reservation: unknown;
  expectedLease: Lease | null;
  promotion?: Readonly<{ observedAt: number; PRODUCTION_AUTO_PROMOTE_DISABLED?: boolean }>;
  ownership?: OwnershipReadback;
  inventory?: InventoryReadback;
}>;

type Verdict = Readonly<{ admitted: false; reasons: readonly string[] }> | Readonly<{
  admitted: true;
  target: Target;
  purpose: typeof AUTOMOD_STAGING_PURPOSE;
  reservation: Readonly<StagingUseChange>;
}>;

const MAX_AGE_MS = 60_000;
const UUID = /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/;

function record(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function fresh(observedAt: number, now: number): boolean {
  return Number.isSafeInteger(observedAt) && observedAt >= 0 && observedAt <= now && now - observedAt <= MAX_AGE_MS;
}

function fixedTarget(target: Target | undefined): boolean {
  return target?.applicationId === STAGING_AUTOMOD_TARGET.applicationId
    && target.guildId === STAGING_AUTOMOD_TARGET.guildId && target.name === STAGING_AUTOMOD_TARGET.name;
}

function matches(value: unknown, format: RegExp): boolean {
  return typeof value === "string" && format.test(value);
}

function inventoryMatches(inventory: InventoryReadback, pin: ServingPin): boolean {
  return inventory.deploymentId === pin.deploymentId && matches(pin.deploymentId, UUID)
    && inventory.sha === pin.sha && inventory.epoch === pin.epoch && inventory.ownerPhase === "active"
    && inventory.workerDeploymentId === pin.workerDeploymentId && matches(pin.workerDeploymentId, UUID)
    && inventory.containerApplicationId === pin.containerApplicationId && matches(pin.containerApplicationId, UUID)
    && inventory.namespaceId === pin.namespaceId && matches(pin.namespaceId, /^[0-9a-f]{32}$/)
    && inventory.rolloutId === pin.rolloutId && matches(pin.rolloutId, UUID)
    && inventory.imageDigest === pin.imageDigest && matches(pin.imageDigest, /^sha256:[0-9a-f]{64}$/)
    && inventory.trafficPercent === 100 && inventory.runtimePolicy === "bounded" && inventory.rolloutStatus === "completed";
}

function nonoverlapping(ownership: OwnershipReadback, expected: Lease | null, request: StagingUseChange): boolean {
  if (ownership.runtimeState !== "stopped" || ownership.stopVerified !== true || ownership.cleanupPending !== false
    || !["runtimeState", "stopVerified", "cleanupPending", "use", "requestedAttempt"].every(key => Object.hasOwn(ownership, key))) return false;
  const { use, requestedAttempt } = ownership;
  if (!record(requestedAttempt) || requestedAttempt.runId !== request.runId || requestedAttempt.runAttempt !== request.runAttempt
    || !["runId", "runAttempt", "usedUntil"].every(key => Object.hasOwn(requestedAttempt, key))) return false;
  const unused = expected === null && requestedAttempt.usedUntil === null;
  if (use === null) return unused;
  if (!record(use) || use.acquiredAt > ownership.observedAt || "voiceMove" in use) return false;

  if (use.phase === "released") {
    // A release tombstone alone does not prove stop. The native stop and
    // cleanup readbacks above must confirm that the old use is over.
    const original = { ...use, phase: "active" as const };
    return unused && (!("purpose" in use) || use.purpose === AUTOMOD_STAGING_PURPOSE) && sameLease(original, original)
      && (use.runId !== request.runId || use.runAttempt !== request.runAttempt);
  }
  return Object.hasOwn(use, "purpose") && use.purpose === AUTOMOD_STAGING_PURPOSE
    && expected?.purpose === AUTOMOD_STAGING_PURPOSE && sameLease(expected, expected) && sameLease(expected, use)
    && use.deploymentId === request.deploymentId && use.sha === request.sha && use.epoch === request.expectedEpoch
    && use.runId === request.runId && use.runAttempt === request.runAttempt && use.expiresAt === request.expiresAt
    && requestedAttempt.usedUntil === request.expiresAt;
}

function denied(reason: string): Verdict {
  return { admitted: false, reasons: [reason] };
}

/** Evaluate authenticated, plain-data snapshots supplied by a trusted caller.
 * Ownership, staging use, the requested attempt marker, native stop and cleanup
 * must come from one coherent observation. Issuance remains in the lifecycle gate.
 */
export function evaluateStagingAutomodNonoverlapPreflight(input: StagingAutomodNonoverlapPreflightInput): Verdict {
  if (!input || !Number.isSafeInteger(input.now) || input.now < 0) return denied("invalid_clock");
  const { now, promotion, ownership, inventory, target, run, pin, plan = STAGING_AUTOMOD_PLAN } = input;
  if (promotion?.PRODUCTION_AUTO_PROMOTE_DISABLED !== true || !Object.hasOwn(promotion, "PRODUCTION_AUTO_PROMOTE_DISABLED")
    || !fresh(promotion.observedAt, now)) return denied("production_auto_promotion_unconfirmed");
  if (!ownership || !fresh(ownership.observedAt, now)) return denied("staging_ownership_readback_unconfirmed");
  if (!inventory || !fresh(inventory.observedAt, now)) return denied("staging_inventory_readback_unconfirmed");

  const request = record(input.reservation) ? parseStagingUseChange(input.reservation) : null;
  if (!request || !run || !plan || !fixedTarget(target) || !fixedTarget(plan.target)
    || !Number.isSafeInteger(plan.leaseMs) || plan.leaseMs <= 0
    || request.expiresAt <= now || request.expiresAt - now > MAX_STAGING_USE_MS
    || !automodReservationAllowed(plan, run, request, now)) return denied("staging_automod_reservation_refused");

  if (!pin || pin.sha !== request.sha || pin.deploymentId !== request.deploymentId || pin.epoch !== request.expectedEpoch
    || ownership.owner?.phase !== "active" || ownership.owner.deploymentId !== pin.deploymentId
    || ownership.owner.epoch !== pin.epoch || ownership.sha !== pin.sha || !inventoryMatches(inventory, pin)) {
    return denied("staging_serving_pin_mismatch");
  }
  if (!nonoverlapping(ownership, input.expectedLease, request)) return denied("staging_nonoverlap_unconfirmed");

  return { admitted: true, target: STAGING_AUTOMOD_TARGET, purpose: AUTOMOD_STAGING_PURPOSE, reservation: Object.freeze(request) };
}
