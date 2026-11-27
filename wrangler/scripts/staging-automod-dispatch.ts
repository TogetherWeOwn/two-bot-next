import { isDirectInvocation } from "./is-direct-invocation.mjs";
import type { bindStagingAutomodInstanceClient } from "./staging-automod-instance-client.ts";
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

/** Assemble the bridge binding. Only an explicit opt in, the Actions OIDC
 * request variables and a serving and inventory selection produce one. The
 * selection must come from authenticated post-deploy evidence for the
 * dispatch SHA, never from source or input. The checked-in command has no
 * such resolver yet, so it keeps its fixed host_unavailable failure.
 */
export async function bindStagingAutomodDispatch(env: DispatchEnv,
  selection?: Pick<Binding, "serving" | "inventorySource">): Promise<Binding | undefined> {
  const { serving, inventorySource } = selection ?? {};
  if (env.STAGING_AUTOMOD_REHEARSAL !== "true" || !serving || !inventorySource
    || !env.ACTIONS_ID_TOKEN_REQUEST_URL || !env.ACTIONS_ID_TOKEN_REQUEST_TOKEN) return;
  const { bindStagingAutomodInstanceClient } = await import("./staging-automod-instance-client.ts");
  const client = bindStagingAutomodInstanceClient(env, serving);
  return client && { serving, inventorySource, host: client.host };
}

if (isDirectInvocation(process.argv[1], import.meta.url)) {
  const result = await bindStagingAutomodDispatch(process.env)
    .then(binding => runStagingAutomodDispatch(process.env, binding))
    .catch(() => ({ verdict: "failed" } as const));
  if (result.verdict === "passed") console.log("staging_automod_rehearsal_passed");
  if (result.verdict === "failed") {
    console.error("staging_automod_rehearsal_failed");
    process.exitCode = 1;
  }
}
