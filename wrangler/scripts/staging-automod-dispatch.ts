import { isDirectInvocation } from "./is-direct-invocation.mjs";
import { runStagingAutomodProtectedHost } from "./staging-automod-protected-host.ts";

type ProtectedHost = Parameters<typeof runStagingAutomodProtectedHost>;
type HostOptions = NonNullable<ProtectedHost[0]>;
type DispatchEnv = NonNullable<HostOptions["runner"]> & Readonly<{ STAGING_AUTOMOD_REHEARSAL?: string }>;
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

if (isDirectInvocation(process.argv[1], import.meta.url)) {
  // The checked-in CLI has no approved host binding. A future protected change
  // must assemble one in source. No dispatch input selects a provider or plan.
  const result = await runStagingAutomodDispatch(process.env).catch(() => ({ verdict: "failed" } as const));
  if (result.verdict === "passed") console.log("staging_automod_rehearsal_passed");
  if (result.verdict === "failed") {
    console.error("staging_automod_rehearsal_failed");
    process.exitCode = 1;
  }
}
