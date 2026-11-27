import { STAGING_AUTOMOD_TARGET } from "./staging-automod-recovery.ts";
import type { StagingUse, StagingUseChange } from "./staging-lifecycle.ts";

/** Reviewed authorization policy. Serving identity is resolved after deploy. */
export type StagingAutomodPlan = Readonly<{
  enabled: boolean;
  target: typeof STAGING_AUTOMOD_TARGET;
  /** Reservation and start also need this. Recover never depends on enablement. */
  allowStart: boolean;
  /** Epoch milliseconds. No reservation or start at or after this instant. */
  notAfter: number;
  /** Longest reservation the plan admits. */
  leaseMs: number;
  /** Cleanup authority after the original lease expires. */
  cleanupMs: number;
}>;

export const STAGING_AUTOMOD_PLAN: StagingAutomodPlan = Object.freeze({
  enabled: false,
  target: STAGING_AUTOMOD_TARGET,
  allowStart: false,
  notAfter: 0,
  leaseMs: 120_000,
  cleanupMs: 600_000,
});

type RunScope = Readonly<{ runId: string; sha: string }>;

function fixedTarget(plan: StagingAutomodPlan): boolean {
  return plan.target.applicationId === STAGING_AUTOMOD_TARGET.applicationId
    && plan.target.guildId === STAGING_AUTOMOD_TARGET.guildId;
}

/** A reservation needs the enabled plan, its window and the verified run. */
export function automodReservationAllowed(plan: StagingAutomodPlan, run: RunScope,
  change: Readonly<StagingUseChange>, now: number): boolean {
  return plan.enabled === true && fixedTarget(plan) && Number.isSafeInteger(plan.notAfter) && now < plan.notAfter
    && change.action === "acquire" && change.voiceMove !== true && change.runAttempt === 1
    && change.runId === run.runId && change.sha === run.sha
    && change.expiresAt <= plan.notAfter && change.expiresAt - now <= plan.leaseMs;
}

/** Start needs the enabled plan, its separate start permission, its window
 * and the unexpired lease issued to the verified run. */
export function automodStartAllowed(plan: StagingAutomodPlan, run: RunScope,
  lease: Readonly<StagingUse>, now: number): boolean {
  return plan.enabled === true && plan.allowStart === true && fixedTarget(plan)
    && Number.isSafeInteger(plan.notAfter) && now < plan.notAfter && now < lease.expiresAt
    && lease.runAttempt === 1 && lease.runId === run.runId && lease.sha === run.sha;
}

/** Recover is allowed on the original lease of the verified run only. */
export function automodRecoverAllowed(plan: StagingAutomodPlan, run: RunScope, lease: Readonly<StagingUse>): boolean {
  return fixedTarget(plan) && lease.runAttempt === 1 && lease.runId === run.runId && lease.sha === run.sha;
}
