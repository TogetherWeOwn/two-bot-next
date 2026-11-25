import type { TwoBotContainer } from "../src/index.ts";
import { boundRead } from "../src/internal-actions.ts";
import { parseStagingUseChange } from "../src/staging-lifecycle.ts";
import { runStagingAutomodProtectedHost } from "./staging-automod-protected-host.ts";

type ProtectedHost = Parameters<typeof runStagingAutomodProtectedHost>;
type Host = NonNullable<ProtectedHost[1]>;
type Instance = ReturnType<typeof TwoBotContainer.bindProtectedAutomodInstance>;
type Acquire = Parameters<Instance["acquireLease"]>;
type LeaseContext = Parameters<Instance["authority"]["readIssuedLease"]>[0];
type Wiring = Pick<Host, "readRun" | "readWorker" | "readAuthenticatedInventory"> & Readonly<{
  instance: Instance;
  reservation: Readonly<{ pin: Acquire[0]; change: Readonly<Acquire[1]> }>;
}>;

/** Compose the protected job with one already bound in-process instance.
 * A trusted host supplies the approved reservation and all collaborators.
 * This entry has no loader, ingress or live publication adapter.
 */
export async function runStagingAutomodInstanceHost(options: ProtectedHost[0] = {}, wiring?: Wiring) {
  if (options.enabled !== true) return { verdict: "disabled" } as const;
  if (!wiring?.reservation?.pin || typeof wiring.readRun !== "function" || typeof wiring.readWorker !== "function"
    || typeof wiring.readAuthenticatedInventory !== "function" || typeof wiring.instance?.acquireLease !== "function"
    || typeof wiring.instance.authority?.readIssuedLease !== "function"
    || typeof wiring.instance.authority.readApproval !== "function" || typeof wiring.instance.invokePrivate !== "function") {
    return { verdict: "failed", code: "host_unavailable" } as const;
  }

  const { instance, reservation } = wiring;
  const change = parseStagingUseChange(reservation.change);
  const pin = Object.freeze({ ...reservation.pin });
  if (change?.action !== "acquire" || change.voiceMove || change.runAttempt !== 1
    || change.deploymentId !== pin.workerVersionId || change.sha !== pin.sourceSha
    || change.deploymentId !== options.serving?.deploymentId || change.sha !== options.serving?.sha) {
    return { verdict: "failed", code: "context_unconfirmed" } as const;
  }

  const matchesRun = (context: LeaseContext) => context.environment === change.environment
    && context.runId === change.runId && context.runAttempt === change.runAttempt
    && context.deploymentId === change.deploymentId && context.sha === change.sha;

  return runStagingAutomodProtectedHost(options, {
    readRun: (id, signal) => wiring.readRun(id, signal),
    prepareLease: async context => {
      if (!matchesRun(context)) return;
      const signal = AbortSignal.timeout(2_000);
      const existing = await boundRead(instance.authority.readIssuedLease(context, signal), signal);
      signal.throwIfAborted();
      if (!existing) await instance.acquireLease(pin, change, AbortSignal.timeout(2_000));
    },
    readWorker: () => wiring.readWorker(),
    readAuthenticatedInventory: (source, signal) => wiring.readAuthenticatedInventory(source, signal),
    invokePrivate: (serving, call, signal) => {
      if (serving.workerDeploymentId !== pin.workerDeploymentId || serving.workerVersionId !== pin.workerVersionId
        || serving.sourceSha !== pin.sourceSha || serving.applicationId !== pin.applicationId
        || serving.namespaceId !== pin.namespaceId || serving.rolloutId !== pin.rolloutId || serving.imageDigest !== pin.imageDigest) {
        throw new Error("AutoMod reservation serving pin mismatch");
      }
      return instance.invokePrivate(serving, call, signal);
    },
    authority: {
      readApproval: (action, lease, signal) => instance.authority.readApproval(action, lease, signal),
      readIssuedLease: async (context, signal) => {
        if (!matchesRun(context)) return;
        const issued = await instance.authority.readIssuedLease(context, signal);
        if (issued?.epoch === change.expectedEpoch && issued.expiresAt === change.expiresAt) return issued;
      },
    },
  });
}
