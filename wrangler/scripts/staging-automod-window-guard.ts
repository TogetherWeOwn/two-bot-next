import {
  automodRecoverAllowed, automodReservationAllowed, automodStartAllowed, type StagingAutomodPlan,
} from "../src/staging-automod-plan.ts";
import { sameLease, STAGING_AUTOMOD_TARGET, type AutomodIntent } from "../src/staging-automod-recovery.ts";
import type { StagingUse, StagingUseChange } from "../src/staging-lifecycle.ts";
import { AUTOMOD_STAGING_PURPOSE, MAX_STAGING_USE_MS } from "../src/staging-use-window.ts";

type WindowBudget = Readonly<
  | { admitted: true; runId: string; sha: string; epoch: number; remainingPublishMs: number; remainingCleanupMs: number }
  | { admitted: false; reasons: readonly string[] }
>;

const leaseKeys = ["environment", "phase", "deploymentId", "sha", "epoch", "runId", "runAttempt", "acquiredAt", "expiresAt"];

// Snapshot own data properties so policy checks use exactly the values validated here.
function record<Key extends string, Optional extends string = never>(
  value: unknown, keys: readonly Key[], optional?: Optional,
): (Record<Key, unknown> & Partial<Record<Optional, unknown>>) | undefined {
  if (value === null || typeof value !== "object") return;
  try {
    if (Object.getPrototypeOf(value) !== Object.prototype) return;
    const ownKeys = Reflect.ownKeys(value);
    const expected = optional !== undefined && ownKeys.includes(optional) ? [...keys, optional] : keys;
    if (ownKeys.length !== expected.length) return;
    const result: Record<string, unknown> = Object.create(null);
    for (const key of expected) {
      if (!ownKeys.includes(key)) return;
      const descriptor = Object.getOwnPropertyDescriptor(value, key);
      if (!descriptor?.enumerable || !Object.hasOwn(descriptor, "value")) return;
      result[key] = descriptor.value;
    }
    return result as Record<Key, unknown> & Partial<Record<Optional, unknown>>;
  } catch {
    return;
  }
}

function timestamp(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

function fixedTarget(value: unknown): boolean {
  const target = record(value, ["applicationId", "guildId", "name"]);
  return target?.applicationId === STAGING_AUTOMOD_TARGET.applicationId
    && target.guildId === STAGING_AUTOMOD_TARGET.guildId && target.name === STAGING_AUTOMOD_TARGET.name;
}

function readLease(value: unknown): Readonly<StagingUse> | undefined {
  const lease = record(value, leaseKeys, "purpose");
  if (!lease || lease.environment !== "staging" || lease.phase !== "active"
    || (Object.hasOwn(lease, "purpose") && lease.purpose !== AUTOMOD_STAGING_PURPOSE)
    || typeof lease.deploymentId !== "string" || typeof lease.sha !== "string" || typeof lease.runId !== "string"
    || !timestamp(lease.epoch) || !timestamp(lease.runAttempt) || !timestamp(lease.acquiredAt) || !timestamp(lease.expiresAt)) return;
  // sameLease checks the issued lease's identifier formats and positive epoch.
  return lease as Readonly<StagingUse>;
}

function deny(reason: string): WindowBudget {
  return Object.freeze({ admitted: false, reasons: Object.freeze([reason]) });
}

/** Evaluate a supplied plan, run, reservation change, issued lease, publication
 * intent and cleanup deadline at one instant. The caller supplies the protected
 * readbacks. This budget grants no authority to perform a live operation. */
export function evaluateStagingAutomodWindowBudget(input: unknown, now: number): WindowBudget {
  const window = record(input, ["promotionDisabled", "plan", "run", "change", "lease", "intent", "removeBefore"]);
  if (!window || !timestamp(now) || !timestamp(window.removeBefore)) return deny("invalid_window");
  if (window.promotionDisabled !== true) return deny("promotion_not_disabled");

  const plan = record(window.plan, ["enabled", "target", "allowStart", "notAfter", "leaseMs", "cleanupMs"]);
  if (!plan || typeof plan.enabled !== "boolean" || typeof plan.allowStart !== "boolean" || !fixedTarget(plan.target)
    || !timestamp(plan.notAfter) || !timestamp(plan.leaseMs) || !timestamp(plan.cleanupMs)) return deny("invalid_plan");
  const policy: StagingAutomodPlan = { enabled: plan.enabled, allowStart: plan.allowStart, target: STAGING_AUTOMOD_TARGET,
    notAfter: plan.notAfter, leaseMs: plan.leaseMs, cleanupMs: plan.cleanupMs };

  const run = record(window.run, ["runId", "sha"]);
  const change = record(window.change, ["environment", "action", "runId", "runAttempt", "expiresAt", "expectedEpoch", "deploymentId", "sha"]);
  const lease = readLease(window.lease);
  const intent = record(window.intent, ["lease", "publishBefore", "target", "baseline"]);
  if (!run || typeof run.runId !== "string" || typeof run.sha !== "string" || !change || !lease || !intent
    || !timestamp(intent.publishBefore) || !fixedTarget(intent.target)
    || typeof intent.baseline !== "string" || !/^[0-9a-f]{64}$/.test(intent.baseline)) return deny("invalid_evidence");
  const intentLease: AutomodIntent["lease"] | undefined = readLease(intent.lease);
  if (!intentLease || lease.purpose !== AUTOMOD_STAGING_PURPOSE || lease.runAttempt !== 1
    || !sameLease(intentLease, lease)) return deny("lease_mismatch");
  if (change.environment !== "staging" || change.action !== "acquire" || change.runId !== lease.runId
    || change.runAttempt !== lease.runAttempt || change.expiresAt !== lease.expiresAt || change.expectedEpoch !== lease.epoch
    || change.deploymentId !== lease.deploymentId || change.sha !== lease.sha) return deny("reservation_mismatch");

  const scope = { runId: run.runId, sha: run.sha };
  if (!automodReservationAllowed(policy, scope, change as Readonly<StagingUseChange>, now)
    || !automodStartAllowed(policy, scope, lease, now) || !automodRecoverAllowed(policy, scope, lease)) {
    return deny("plan_refused");
  }

  const { publishBefore } = intent;
  const { removeBefore } = window;
  const { acquiredAt, expiresAt } = lease;
  // Compare durations so adding cleanupMs cannot overflow the safe timestamp range.
  if (now < acquiredAt || publishBefore <= now || publishBefore > expiresAt || expiresAt > policy.notAfter
    || expiresAt - acquiredAt > MAX_STAGING_USE_MS || removeBefore <= publishBefore
    || removeBefore - expiresAt > policy.cleanupMs) return deny("deadline_outside_window");

  return Object.freeze({ admitted: true, runId: lease.runId, sha: lease.sha, epoch: lease.epoch,
    remainingPublishMs: publishBefore - now, remainingCleanupMs: removeBefore - now });
}
