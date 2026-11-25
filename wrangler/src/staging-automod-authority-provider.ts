import type { Env } from "./index.ts";
import { authenticated } from "./ownership.ts";
import type { readStagingAutomodJobInputs } from "./staging-automod-inputs.ts";
import { sameLease, STAGING_AUTOMOD_TARGET } from "./staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE } from "./staging-use-window.ts";

type Inputs = Parameters<typeof readStagingAutomodJobInputs>;
type Context = NonNullable<Inputs[0]>;
type Lease = NonNullable<Awaited<ReturnType<Inputs[1]["readIssuedLease"]>>>;
type Approval = NonNullable<Awaited<ReturnType<Inputs[1]["readApproval"]>>>;
type Action = Approval["action"];
type AuthorityEnv = Pick<Env, "TWO_DEPLOY_ENV" | "STAGING_RUNTIME_POLICY" | "OWNERSHIP_CONTROL_TOKEN"
  | "DISCORD_APPLICATION_ID" | "GUILD_ID" | "CF_VERSION_METADATA" | "CONTAINER_RELEASE">;
export type ReadProtectedAutomodApproval = (action: Action, issuedLease: Lease, signal: AbortSignal) => Promise<Approval | undefined>;

function fixedTarget(target: Context["target"]): boolean {
  return target?.applicationId === STAGING_AUTOMOD_TARGET.applicationId && target.guildId === STAGING_AUTOMOD_TARGET.guildId;
}

/** In-process only. The protected caller supplies an independently authenticated
 * approval reader and resolves lifecycle reads under the Container ownership gate.
 * Wrap the returned callbacks with boundStagingAutomodAuthority at the job. */
export function bindStagingAutomodAuthorityProvider(request: Request, env: AuthorityEnv,
  readLifecycleLease: (context: Context, signal: AbortSignal) => Promise<Lease | undefined>,
  readProtectedApproval: ReadProtectedAutomodApproval,
) {
  async function readIssuedLease(context: Context, signal: AbortSignal) {
    signal.throwIfAborted();
    if (env.TWO_DEPLOY_ENV !== "staging" || env.STAGING_RUNTIME_POLICY !== "bounded" || !fixedTarget(context.target)
      || env.DISCORD_APPLICATION_ID !== STAGING_AUTOMOD_TARGET.applicationId || env.GUILD_ID !== STAGING_AUTOMOD_TARGET.guildId
      || context.environment !== "staging" || context.runAttempt !== 1 || context.deploymentId !== env.CF_VERSION_METADATA?.id
      || context.sha !== env.CONTAINER_RELEASE || !await authenticated(request, env.OWNERSHIP_CONTROL_TOKEN)) return;
    signal.throwIfAborted();
    const issued = await readLifecycleLease(context, signal);
    signal.throwIfAborted();
    if (issued?.purpose === AUTOMOD_STAGING_PURPOSE && sameLease(issued, issued)
      && issued.deploymentId === context.deploymentId && issued.sha === context.sha
      && issued.runId === context.runId && issued.runAttempt === context.runAttempt && issued.acquiredAt <= Date.now()) return issued;
  }

  async function readApproval(action: Action, issuedLease: Lease, signal: AbortSignal) {
    if ((action !== "start" && action !== "recover") || issuedLease?.purpose !== AUTOMOD_STAGING_PURPOSE) return;
    const { environment, deploymentId, sha, runId, runAttempt } = issuedLease;
    const context = { environment, deploymentId, sha, runId, runAttempt, target: STAGING_AUTOMOD_TARGET };
    const issued = await readIssuedLease(context, signal);
    if (!issued || !sameLease(issued, issuedLease) || (action === "start" && Date.now() >= issued.expiresAt)) return;
    const approval = await readProtectedApproval(action, issued, signal);
    if (approval?.action !== action || !fixedTarget(approval.target) || approval.issuedLease?.purpose !== AUTOMOD_STAGING_PURPOSE
      || !sameLease(issued, approval.issuedLease)) return;
    // The external approval lookup does not hold the ownership gate.
    const current = await readIssuedLease(context, signal);
    if (current && sameLease(current, issued) && (action === "recover" || Date.now() < current.expiresAt)) return approval;
  }

  return { readIssuedLease, readApproval };
}
