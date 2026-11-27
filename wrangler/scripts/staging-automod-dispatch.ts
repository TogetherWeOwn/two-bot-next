import { isDirectInvocation } from "./is-direct-invocation.mjs";
import type { bindStagingAutomodInstanceClient } from "./staging-automod-instance-client.ts";
import { runStagingAutomodProtectedHost } from "./staging-automod-protected-host.ts";

type ProtectedHost = Parameters<typeof runStagingAutomodProtectedHost>;
type HostOptions = NonNullable<ProtectedHost[0]>;
type Client = Parameters<typeof bindStagingAutomodInstanceClient>;
type InstanceHost = typeof import("./staging-automod-instance-host.ts")["runStagingAutomodInstanceHost"];
type Wiring = Pick<NonNullable<Parameters<InstanceHost>[1]>, "instance" | "reservation">;
type Selection = Required<Pick<HostOptions, "serving" | "inventorySource">>;
type DispatchEnv = NonNullable<HostOptions["runner"]> & Client[0]
  & Readonly<{ STAGING_AUTOMOD_REHEARSAL?: string }>;
type Binding = Selection & Readonly<{
  host: NonNullable<ProtectedHost[1]>;
  wiring?: Wiring;
}>;

/** The protected job supplies GitHub-owned metadata and an in-process binding
 * prepared from its approved plan and verified staging fixture. Dispatch input
 * selects the job only. The protected host authenticates the run before any
 * lease or action approval reads and preserves the original lease for recovery.
 */
export async function runStagingAutomodDispatch(env: DispatchEnv = {}, binding?: Binding) {
  if (env.STAGING_AUTOMOD_REHEARSAL !== "true") return { verdict: "disabled" } as const;
  if (!binding) return { verdict: "failed", code: "host_unavailable" } as const;

  const { serving, inventorySource, host, wiring } = binding;
  const options = { enabled: true, runner: env, serving, inventorySource };
  if (!wiring) return runStagingAutomodProtectedHost(options, host);
  const { runStagingAutomodInstanceHost } = await import("./staging-automod-instance-host.ts");
  return runStagingAutomodInstanceHost(options, { ...host, ...wiring });
}

/** Assemble a bridge binding from authenticated post-deploy evidence.
 * Keep the client import lazy because the unbound job uses a sparse checkout. */
export async function bindStagingAutomodDispatch(env: DispatchEnv,
  selection?: Selection & Pick<Wiring, "reservation">,
  ...adapters: [fetcher?: Client[2], readInventory?: Client[3]]): Promise<Binding | undefined> {
  const { serving, inventorySource, reservation } = selection ?? {};
  if (env.STAGING_AUTOMOD_REHEARSAL !== "true" || !serving || !inventorySource || !reservation
    || !env.ACTIONS_ID_TOKEN_REQUEST_URL || !env.ACTIONS_ID_TOKEN_REQUEST_TOKEN) return;
  const { bindStagingAutomodInstanceClient } = await import("./staging-automod-instance-client.ts");
  const client = bindStagingAutomodInstanceClient(env, serving, ...adapters);
  return client && { serving, inventorySource, host: client.host, wiring: { instance: client.instance, reservation } };
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
