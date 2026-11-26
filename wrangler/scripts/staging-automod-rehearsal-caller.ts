import { boundStagingAutomodAuthority } from "../src/staging-automod-bounded-authority.ts";
import { readStagingAutomodJobInputs } from "../src/staging-automod-inputs.ts";
import { STAGING_AUTOMOD_TARGET } from "../src/staging-automod-recovery.ts";
import { bindStagingAutomodDeploymentTransport } from "./staging-automod-deployment-transport.ts";
import { runStagingAutomodPrivateJob } from "./staging-automod-private-job.ts";

type AuthorityContext = NonNullable<Parameters<typeof readStagingAutomodJobInputs>[0]>;
type Context = AuthorityContext & Readonly<{
  workflowRef: string;
  eventName: string;
  refProtected: boolean;
}>;
type TransportInputs = Parameters<typeof bindStagingAutomodDeploymentTransport>;
type Approval = Parameters<ReturnType<typeof bindStagingAutomodDeploymentTransport>>[1];
type BoundAuthority = typeof boundStagingAutomodAuthority<AuthorityContext, Approval["issuedLease"], Approval["action"], Approval>;
type Providers = Readonly<{
  authority: Parameters<BoundAuthority>[0];
  // Only an authenticated caller can prepare. Durable confirmation uses the
  // separate, read-only authority deadline below.
  prepareLease?: (context: AuthorityContext) => Promise<void>;
  readWorker: () => TransportInputs[0];
  readInventory: TransportInputs[1];
  invoke: TransportInputs[2];
}>;

const WORKFLOW_REF = "TogetherWeOwn/two-bot-next/.github/workflows/staging-automod-preview.yml@refs/heads/main";
const DEPLOYMENT_ID = /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/;

function admitted(context: Context | undefined): context is Context {
  return context?.environment === "staging" && context.workflowRef === WORKFLOW_REF
    && context.eventName === "workflow_dispatch" && context.refProtected === true && context.runAttempt === 1
    && context.target?.applicationId === STAGING_AUTOMOD_TARGET.applicationId
    && context.target.guildId === STAGING_AUTOMOD_TARGET.guildId
    && typeof context.deploymentId === "string" && DEPLOYMENT_ID.test(context.deploymentId)
    && typeof context.sha === "string" && /^[0-9a-f]{40}$/.test(context.sha)
    && typeof context.runId === "string" && /^[1-9][0-9]{0,19}$/.test(context.runId);
}

/** Dormant entry for the authenticated protected staging job. Supply options
 * from trusted job state and use readWorker only to obtain local bindings.
 * Authenticate the inventory against its producer's workflow SHA and run ID
 * in readInventory. Invoke the exact private instance identified by the transport.
 */
export async function runStagingAutomodRehearsal(
  options: Readonly<{ enabled?: boolean; context?: Context }>, providers: Providers,
) {
  if (options.enabled !== true) return { verdict: "disabled" } as const;
  const { context } = options;
  if (!admitted(context)) return { verdict: "failed", code: "context_unconfirmed" } as const;

  const expected = { ...context, target: STAGING_AUTOMOD_TARGET };
  if (providers.prepareLease) {
    try { await providers.prepareLease(expected); }
    catch {
      // An interrupted acquisition can already be durable. Read the exact
      // issued lease independently before approving either action.
    }
  }
  const inputs = await readStagingAutomodJobInputs(expected, boundStagingAutomodAuthority(providers.authority));
  if (!inputs) return { verdict: "failed", code: "lease_unconfirmed" } as const;

  let transport: ReturnType<typeof bindStagingAutomodDeploymentTransport> | undefined;
  return runStagingAutomodPrivateJob(inputs.context, inputs.issuedLease, inputs.approvals, (call, approval, signal) => {
    transport ??= bindStagingAutomodDeploymentTransport(providers.readWorker(), providers.readInventory, providers.invoke);
    return transport(call, approval, signal);
  });
}
