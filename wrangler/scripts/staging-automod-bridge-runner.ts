import { STAGING_AUTOMOD_PLAN, type StagingAutomodPlan } from "../src/staging-automod-plan.ts";
import { bindStagingAutomodInstanceClient } from "./staging-automod-instance-client.ts";
import { runStagingAutomodInstanceHost } from "./staging-automod-instance-host.ts";
import { resolveStagingAutomodSelection } from "./staging-automod-selection.ts";

type HostOptions = NonNullable<Parameters<typeof runStagingAutomodInstanceHost>[0]>;
type Runner = NonNullable<HostOptions["runner"]>;
type Client = Parameters<typeof bindStagingAutomodInstanceClient>;
type Dependencies = Parameters<typeof resolveStagingAutomodSelection>[1] & Readonly<{
  env: Client[0];
  fetcher: NonNullable<Client[2]>;
}>;
type Options = Readonly<{
  enabled?: boolean;
  runner?: Runner;
  plan?: StagingAutomodPlan;
  signal?: AbortSignal;
}>;

const UNCONFIRMED = Object.freeze({ verdict: "failed", code: "context_unconfirmed" } as const);
const HOST_UNAVAILABLE = Object.freeze({ verdict: "failed", code: "host_unavailable" } as const);

/** Dormant Actions entry. The caller supplies runner metadata, authenticated
 * readbacks and any rehearsal plan override. The selector bounds the reads and
 * the Worker still enforces its compiled plan.
 */
export async function runStagingAutomodBridgeRehearsal(options: Options = {}, deps?: Dependencies) {
  if (options.enabled !== true) return { verdict: "disabled" } as const;
  const runner = { ...options.runner };
  const { GITHUB_SHA: sha, GITHUB_RUN_ID: runId, GITHUB_RUN_ATTEMPT: attempt } = runner;
  if (typeof sha !== "string" || !/^[0-9a-f]{40}$/.test(sha)
    || typeof runId !== "string" || !/^[1-9][0-9]{0,15}$/.test(runId)
    || !Number.isSafeInteger(Number(runId)) || attempt !== "1") return UNCONFIRMED;
  if (!deps) return HOST_UNAVAILABLE;
  const { env, fetcher, readPostDeployRuns, readAuthenticatedInventory } = deps;

  const signal = options.signal ?? new AbortController().signal;
  const selection = await resolveStagingAutomodSelection({ sha, runId, runAttempt: 1 },
    { readPostDeployRuns, readAuthenticatedInventory }, signal, options.plan ?? STAGING_AUTOMOD_PLAN);
  if (!selection || signal.aborted) return UNCONFIRMED;

  if (!env || typeof fetcher !== "function") return HOST_UNAVAILABLE;
  const bound = bindStagingAutomodInstanceClient(env, selection.serving, fetcher, readAuthenticatedInventory);
  if (!bound) return HOST_UNAVAILABLE;

  return runStagingAutomodInstanceHost({
    enabled: true, runner, serving: selection.serving, inventorySource: selection.inventorySource,
  }, { ...bound.host, instance: bound.instance, reservation: selection.reservation });
}
