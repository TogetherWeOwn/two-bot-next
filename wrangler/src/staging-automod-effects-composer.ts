import type { ProtectedAutomodEffects } from "./staging-automod-caller.ts";
import { createStagingAutomodDryRunAdapter } from "./staging-automod-dry-run-adapter.ts";
import { STAGING_AUTOMOD_PLAN, type StagingAutomodPlan } from "./staging-automod-plan.ts";
import { sameLease, STAGING_AUTOMOD_TARGET } from "./staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE } from "./staging-use-window.ts";

interface Dependencies {
  transport: Parameters<typeof createStagingAutomodDryRunAdapter>[0];
  plan?: StagingAutomodPlan;
  now?: () => number;
  // Return a grant only after authenticating its source and authority for this lease.
  readAuthenticatedCleanupGrant: ProtectedAutomodEffects["readCleanupGrant"];
  // Authenticated, bounded registry transport.
  request: ProtectedAutomodEffects["request"];
}

export function createProtectedAutomodEffects({
  transport, plan = STAGING_AUTOMOD_PLAN, now = Date.now, readAuthenticatedCleanupGrant, request,
}: Dependencies): ProtectedAutomodEffects {
  const { target, notAfter, cleanupMs } = plan;
  const fixedTarget = target?.applicationId === STAGING_AUTOMOD_TARGET.applicationId
    && target.guildId === STAGING_AUTOMOD_TARGET.guildId && target.name === STAGING_AUTOMOD_TARGET.name;
  const at = now();
  const enabled = plan.enabled === true && plan.allowStart === true && fixedTarget
    && Number.isSafeInteger(at) && Number.isSafeInteger(notAfter) && at < notAfter;
  const { dryRun, readRuntime, fenceAndDrain, stop } = createStagingAutomodDryRunAdapter(transport, now);
  const startup: ProtectedAutomodEffects["dryRun"] = enabled && dryRun ? {
    policy: dryRun.policy,
    // The caller may await authority checks after composing the effects.
    start: (lease, policy, before) => dryRun.start(lease, policy,
      Number.isSafeInteger(before) && before < lease.expiresAt ? Math.min(before, notAfter) : before),
  } : undefined;

  const effects: ProtectedAutomodEffects = {
    enabled, readRuntime, fenceAndDrain, stop, request,
    ...(startup === undefined ? {} : { dryRun: Object.freeze(startup) }),
    async readCleanupGrant(expected) {
      if (!fixedTarget || !Number.isSafeInteger(notAfter)
        || !Number.isSafeInteger(cleanupMs) || cleanupMs <= 0) return;
      try {
        const lease = Object.freeze({ ...expected });
        if (lease.purpose !== AUTOMOD_STAGING_PURPOSE || !sameLease(lease, lease)) return;
        // Recovery can outlive startup enablement, but it still needs the
        // issued plan's cleanup window. The zero-cutoff default is dormant.
        const limit = Math.min(lease.expiresAt + cleanupMs, notAfter + cleanupMs);
        if (!Number.isSafeInteger(limit)) return;
        const { lease: grantedLease, before } = await readAuthenticatedCleanupGrant(lease) ?? {};
        const at = now();
        if (grantedLease?.purpose !== AUTOMOD_STAGING_PURPOSE || !sameLease(lease, grantedLease)
          || before === undefined || !Number.isSafeInteger(before) || !Number.isSafeInteger(at)
          || !(at < before && before <= limit)) return;
        return { lease, before };
      } catch { return undefined; }
    },
  };
  return Object.freeze(effects);
}
