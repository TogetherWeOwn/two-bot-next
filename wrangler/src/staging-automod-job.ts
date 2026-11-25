import type { ProtectedAutomodCall } from "./staging-automod-caller.ts";
import { sameLease, STAGING_AUTOMOD_TARGET } from "./staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE } from "./staging-lifecycle.ts";

type ApprovedJob = ProtectedAutomodCall & {
  readonly target: Readonly<{ applicationId: string; guildId: string }>;
};

function hasKeys(value: unknown, keys: readonly string[]): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    && Object.keys(value).length === keys.length && keys.every(key => Object.hasOwn(value, key));
}

function admit(request: unknown, approved: ApprovedJob | undefined): ProtectedAutomodCall | undefined {
  if (!approved || !hasKeys(request, ["action", "issuedLease"])) return;
  const { action, issuedLease: issued, target } = approved;
  if ((action !== "start" && action !== "recover") || request.action !== action
    || target?.applicationId !== STAGING_AUTOMOD_TARGET.applicationId || target.guildId !== STAGING_AUTOMOD_TARGET.guildId
    || issued?.purpose !== AUTOMOD_STAGING_PURPOSE) return;

  // Project the approved tuple explicitly. It is also the only scope that
  // can appear in a job receipt.
  const { runId, runAttempt, deploymentId, sha, epoch, acquiredAt, expiresAt } = issued;
  const lease = Object.freeze({ environment: "staging", purpose: AUTOMOD_STAGING_PURPOSE, phase: "active",
    runId, runAttempt, deploymentId, sha, epoch, acquiredAt, expiresAt } as const);
  const requested = request.issuedLease;
  if (!sameLease(lease, issued) || !hasKeys(requested, Object.keys(lease))
    || requested.purpose !== lease.purpose || !sameLease(lease, requested)) return;

  const now = Date.now();
  if (acquiredAt > now || (action === "start" && now >= expiresAt)) return;
  return Object.freeze({ action, issuedLease: lease });
}

/** Dormant in-process adapter. The transport must supply bounded decoded JSON
 * and independently authenticated approval for the exact action and issued
 * lease. Invoke must bind the private Container caller to that deployment.
 */
export async function runProtectedAutomodJob(request: unknown, approved: ApprovedJob | undefined,
  invoke: (call: ProtectedAutomodCall) => Promise<void>) {
  const receipt = () => ({ schema: "two-bot-staging-automod-job.v1", environment: "staging",
    observedAt: new Date().toISOString() } as const);
  const call = admit(request, approved);
  if (!call) return { ...receipt(), verdict: "failed", code: "request_refused" } as const;

  const scope = { action: call.action, lease: call.issuedLease };
  try {
    // Recovery may use the original expired lease. The caller requires its
    // fresh cleanup grant or a durable completion for the same lease.
    await invoke(call);
    return { ...receipt(), ...scope, verdict: "passed" } as const;
  } catch {
    return { ...receipt(), ...scope, verdict: "failed", code: "operation_unconfirmed" } as const;
  }
}
