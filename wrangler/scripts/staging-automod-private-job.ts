import { invokeProtectedAutomodJob } from "../src/staging-automod-entry.ts";
import { sameLease, STAGING_AUTOMOD_TARGET } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE } from "../src/staging-lifecycle.ts";
import { runStagingAutomodDryRun } from "./staging-automod-workflow.ts";

type WorkerEntry = Parameters<typeof invokeProtectedAutomodJob>;
type Approval = NonNullable<WorkerEntry[2]>;
type Runner = Parameters<typeof runStagingAutomodDryRun>;
type Call = Parameters<Runner[2]>[0];
type Transport = (call: Call, approval: Approval, signal: AbortSignal) => Promise<Response>;
type Approvals = Readonly<Partial<Record<Call["action"], Approval>>>;
type ProtectedContext = Runner[0] & Readonly<{
  environment: string;
  deploymentId: string;
  target: Approval["target"];
}>;
type PrivateWorker = Readonly<{
  env: WorkerEntry[1];
  controlBearer: string;
  invoke: WorkerEntry[3];
}>;

function fixedTarget(target: Approval["target"]): boolean {
  return target?.applicationId === STAGING_AUTOMOD_TARGET.applicationId
    && target.guildId === STAGING_AUTOMOD_TARGET.guildId;
}

/** Job-side adapter for an injected private handoff. */
export function runStagingAutomodPrivateJob(job: Runner[0], issuedLease: Runner[1],
  { start, recover }: Approvals, transport: Transport,
) {
  return runStagingAutomodDryRun(job, issuedLease, async (request, signal) => {
    const approval = request.action === "start" ? start : recover;
    if (approval?.action !== request.action || !fixedTarget(approval.target)
      || approval.issuedLease?.purpose !== AUTOMOD_STAGING_PURPOSE
      || !sameLease(request.issuedLease, approval.issuedLease)) {
      throw new Error("AutoMod job approval unconfirmed");
    }
    return transport(request, approval, signal);
  });
}

/** Dormant binding. Supply context from the already authenticated protected
 * job, the original issued lease, and separate approvals from protected
 * authority. These scope checks do not authenticate GitHub context or grant
 * authority. The Worker still authenticates the separately supplied ownership
 * control bearer and calls invoke only after admission.
 */
export async function runStagingAutomodBoundJob(context: ProtectedContext | undefined,
  issuedLease: Runner[1] | undefined, approvals: Approvals | undefined, worker: PrivateWorker,
) {
  if (context?.environment !== "staging" || !fixedTarget(context.target) || !issuedLease
    || context.deploymentId !== issuedLease.deploymentId) {
    return { verdict: "failed", code: "lease_unconfirmed" } as const;
  }

  return runStagingAutomodPrivateJob(context, issuedLease, approvals ?? {}, async (call, approval, signal) => {
    const request = new Request("https://worker.invalid", {
      method: "POST", headers: { authorization: `Bearer ${worker.controlBearer}` }, body: JSON.stringify(call), signal,
    });
    return Response.json(await invokeProtectedAutomodJob(request, worker.env, approval, call => worker.invoke(call)));
  });
}
