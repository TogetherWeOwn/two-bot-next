import { STAGING_AUTOMOD_PLAN, type StagingAutomodPlan } from "../src/staging-automod-plan.ts";
import { isDirectInvocation } from "./is-direct-invocation.mjs";
import { bindStagingAutomodInstanceClient } from "./staging-automod-instance-client.ts";
import { runStagingAutomodProtectedHost } from "./staging-automod-protected-host.ts";

type ProtectedHost = Parameters<typeof runStagingAutomodProtectedHost>;
type HostOptions = NonNullable<ProtectedHost[0]>;
type DispatchEnv = NonNullable<HostOptions["runner"]> & Parameters<typeof bindStagingAutomodInstanceClient>[0]
  & Readonly<{ STAGING_AUTOMOD_REHEARSAL?: string }>;
type Binding = Required<Pick<HostOptions, "serving" | "inventorySource">> & Readonly<{
  host: NonNullable<ProtectedHost[1]>;
}>;

/** The protected job supplies GitHub-owned metadata and an in-process binding
 * prepared from its approved plan and verified staging fixture. Dispatch input
 * selects the job only. The protected host authenticates the run before any
 * lease or action approval reads and preserves the original lease for recovery.
 */
export async function runStagingAutomodDispatch(env: DispatchEnv = {}, binding?: Binding) {
  if (env.STAGING_AUTOMOD_REHEARSAL !== "true") return { verdict: "disabled" } as const;
  if (!binding) return { verdict: "failed", code: "host_unavailable" } as const;

  return runStagingAutomodProtectedHost({ enabled: true, runner: env,
    serving: binding.serving, inventorySource: binding.inventorySource }, binding.host);
}

/** Assemble the bridge binding from the compiled plan. Only an explicit opt
 * in, the Actions OIDC request variables and the plan's serving and inventory
 * selection produce one. The default plan selects neither, so the checked-in
 * command keeps its fixed host_unavailable failure. No input selects a plan.
 */
export function bindStagingAutomodDispatch(env: DispatchEnv,
  plan: StagingAutomodPlan = STAGING_AUTOMOD_PLAN): Binding | undefined {
  const { serving, inventorySource } = plan;
  if (env.STAGING_AUTOMOD_REHEARSAL !== "true" || !serving || !inventorySource) return;
  const client = bindStagingAutomodInstanceClient(env, serving);
  return client && { serving, inventorySource, host: client.host };
}

if (isDirectInvocation(process.argv[1], import.meta.url)) {
  const result = await runStagingAutomodDispatch(process.env, bindStagingAutomodDispatch(process.env))
    .catch(() => ({ verdict: "failed" } as const));
  if (result.verdict === "passed") console.log("staging_automod_rehearsal_passed");
  if (result.verdict === "failed") {
    console.error("staging_automod_rehearsal_failed");
    process.exitCode = 1;
  }
}
