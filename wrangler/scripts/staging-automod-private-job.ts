import type { invokeProtectedAutomodJob } from "../src/staging-automod-entry.ts";
import { runStagingAutomodDryRun } from "./staging-automod-workflow.ts";

type Approval = NonNullable<Parameters<typeof invokeProtectedAutomodJob>[2]>;
type Runner = Parameters<typeof runStagingAutomodDryRun>;
type Call = Parameters<Runner[2]>[0];
type Transport = (call: Call, approval: Approval, signal: AbortSignal) => Promise<Response>;
type Approvals = Readonly<Partial<Record<Call["action"], Approval>>>;

/** Job-side adapter for an injected private handoff. Live use requires an
 * authenticated binding that carries protected approval independently of the
 * request body and resolves the caller for the issued deployment.
 */
export function runStagingAutomodPrivateJob(job: Runner[0], issuedLease: Runner[1],
  { start, recover }: Approvals, transport: Transport,
) {
  return runStagingAutomodDryRun(job, issuedLease, async (request, signal) => {
    const approval = request.action === "start" ? start : recover;
    if (!approval) throw new Error("AutoMod job approval missing");
    return transport(request, approval, signal);
  });
}
