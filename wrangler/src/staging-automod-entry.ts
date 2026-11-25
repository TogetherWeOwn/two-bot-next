import type { Env } from "./index.ts";
import { authenticated, readControlBody } from "./ownership.ts";
import { runProtectedAutomodJob } from "./staging-automod-job.ts";
import { STAGING_AUTOMOD_TARGET } from "./staging-automod-recovery.ts";
import { parseUniqueJson } from "./unique-json.ts";

type EntryEnv = Pick<Env, "TWO_DEPLOY_ENV" | "STAGING_RUNTIME_POLICY" | "OWNERSHIP_CONTROL_TOKEN"
  | "DISCORD_APPLICATION_ID" | "GUILD_ID" | "CF_VERSION_METADATA" | "CONTAINER_RELEASE">;
type JobAuthority = Parameters<typeof runProtectedAutomodJob>[1];
type Invoke = Parameters<typeof runProtectedAutomodJob>[2];

/** Private in-process entry. Supply approval from authenticated protected job
 * context independent of the request, including its action, target and exact
 * original issued lease. Resolve the private caller lazily inside invoke so
 * refusals never acquire the Container or touch its state.
 */
export async function invokeProtectedAutomodJob(request: Request, env: EntryEnv,
  approval: JobAuthority, invoke: Invoke,
) {
  let body: unknown;
  if (request.method === "POST" && approval
    && env.TWO_DEPLOY_ENV === "staging" && env.STAGING_RUNTIME_POLICY === "bounded"
    && env.DISCORD_APPLICATION_ID === STAGING_AUTOMOD_TARGET.applicationId
    && env.GUILD_ID === STAGING_AUTOMOD_TARGET.guildId
    && await authenticated(request, env.OWNERSHIP_CONTROL_TOKEN).catch(() => false)) {
    const decoder = new TextDecoder("utf-8", { fatal: true, ignoreBOM: false });
    body = await readControlBody(request, bytes => parseUniqueJson(decoder.decode(bytes))).catch(() => null);
  }

  // Use the Worker binding for the deployment and build. The caller checks
  // ownership and the entire stored lease under the Container gate.
  const lease = approval?.issuedLease;
  const localApproval = lease && lease.deploymentId === env.CF_VERSION_METADATA?.id
    && lease.sha === env.CONTAINER_RELEASE ? approval : undefined;
  return runProtectedAutomodJob(body, localApproval, invoke);
}
