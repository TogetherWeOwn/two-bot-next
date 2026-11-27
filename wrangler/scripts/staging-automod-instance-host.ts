import type { TwoBotContainer } from "../src/index.ts";
import { boundRead } from "../src/internal-actions.ts";
import { authenticated } from "../src/ownership.ts";
import { STAGING_AUTOMOD_TARGET } from "../src/staging-automod-recovery.ts";
import { parseStagingUseChange } from "../src/staging-lifecycle.ts";
import { boundStagingAutomodInventoryRead } from "./staging-automod-inventory-verifier.ts";
import { runStagingAutomodProtectedHost } from "./staging-automod-protected-host.ts";

type ProtectedHost = Parameters<typeof runStagingAutomodProtectedHost>;
type Host = NonNullable<ProtectedHost[1]>;
type Instance = ReturnType<typeof TwoBotContainer.bindProtectedAutomodInstance>;
type Acquire = Parameters<Instance["acquireLease"]>;
type Pin = Acquire[0];
type LeaseContext = Parameters<Instance["authority"]["readIssuedLease"]>[0];
type Inventory = Awaited<ReturnType<Host["readAuthenticatedInventory"]>>;
type InventorySource = Parameters<Host["readAuthenticatedInventory"]>[0];
type Wiring = Pick<Host, "readRun" | "readWorker" | "readAuthenticatedInventory"> & Readonly<{
  instance: Instance;
  reservation: Readonly<{ pin: Pin; change: Readonly<Acquire[1]> }>;
}>;

const UNCONFIRMED = Object.freeze({ verdict: "failed", code: "context_unconfirmed" } as const);
const UUID = /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/;
const UTC_OBSERVATION = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,6})?(?:\+00:00|Z)$/;
const MAX_OBSERVATION_AGE_MS = 60_000;

function matchesServingPin(serving: Pin, pin: Pin): boolean {
  return serving.workerDeploymentId === pin.workerDeploymentId && serving.workerVersionId === pin.workerVersionId
    && serving.sourceSha === pin.sourceSha && serving.applicationId === pin.applicationId
    && serving.namespaceId === pin.namespaceId && serving.rolloutId === pin.rolloutId && serving.imageDigest === pin.imageDigest;
}

function matchesInventory(inventory: Inventory, source: InventorySource, pin: Pin, expectedEpoch: number): boolean {
  if (inventory?.schema_version !== 1 || inventory.workflow_sha !== source.workflowSha
    || inventory.workflow_run_id !== source.runId) return false;

  // Use the same observation window and readiness checks as the action transport.
  const { observed_at, worker, container, ownership } = inventory;
  const observed = typeof observed_at === "string" && UTC_OBSERVATION.test(observed_at) ? Date.parse(observed_at) : NaN;
  const age = Date.now() - observed;
  return age >= 0 && age <= MAX_OBSERVATION_AGE_MS
    && new Date(observed).toISOString().slice(0, 19) === observed_at.slice(0, 19)
    && worker?.traffic_percent === 100 && worker.runtime_policy === "bounded"
    && container?.rollout_status === "completed" && ownership?.phase === "active" && ownership.epoch === expectedEpoch
    && typeof worker.deployment_id === "string" && UUID.test(worker.deployment_id)
    && typeof container.application_id === "string" && UUID.test(container.application_id)
    && typeof container.rollout_id === "string" && UUID.test(container.rollout_id)
    && typeof container.namespace_id === "string" && /^[0-9a-f]{32}$/.test(container.namespace_id)
    && typeof container.image_digest === "string" && /^sha256:[0-9a-f]{64}$/.test(container.image_digest)
    && matchesServingPin({
      workerDeploymentId: worker.deployment_id, workerVersionId: worker.version_id, sourceSha: worker.source_sha,
      applicationId: container.application_id, namespaceId: container.namespace_id,
      rolloutId: container.rollout_id, imageDigest: container.image_digest,
    }, pin);
}

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
  const inventorySource = options.inventorySource && Object.freeze({ ...options.inventorySource });
  if (change?.action !== "acquire" || change.voiceMove || change.runAttempt !== 1
    || change.deploymentId !== pin.workerVersionId || change.sha !== pin.sourceSha
    || change.deploymentId !== options.serving?.deploymentId || change.sha !== options.serving?.sha || !inventorySource) {
    return UNCONFIRMED;
  }

  const matchesRun = (context: LeaseContext) => context.environment === change.environment
    && context.runId === change.runId && context.runAttempt === change.runAttempt
    && context.deploymentId === change.deploymentId && context.sha === change.sha;

  let preparationUnconfirmed = false;
  const result = await runStagingAutomodProtectedHost({ ...options, inventorySource }, {
    readRun: (id, signal) => wiring.readRun(id, signal),
    prepareLease: async context => {
      if (!matchesRun(context)) return;
      const signal = AbortSignal.timeout(2_000);
      const existing = await boundRead(instance.authority.readIssuedLease(context, signal), signal);
      signal.throwIfAborted();
      if (existing) return;

      preparationUnconfirmed = true;
      const deadline = AbortSignal.timeout(2_000);
      const inventory = await boundStagingAutomodInventoryRead(wiring.readAuthenticatedInventory(inventorySource, deadline), deadline);
      deadline.throwIfAborted();
      const { env, controlBearer } = wiring.readWorker() ?? {};
      if (env?.CF_VERSION_METADATA?.id !== pin.workerVersionId || env.CONTAINER_RELEASE !== pin.sourceSha
        || env.TWO_DEPLOY_ENV !== "staging" || env.STAGING_RUNTIME_POLICY !== "bounded"
        || env.DISCORD_APPLICATION_ID !== STAGING_AUTOMOD_TARGET.applicationId || env.GUILD_ID !== STAGING_AUTOMOD_TARGET.guildId
        || typeof controlBearer !== "string") return;
      const request = new Request("https://worker.invalid", { headers: { authorization: `Bearer ${controlBearer}` } });
      if (!await boundRead(authenticated(request, env.OWNERSHIP_CONTROL_TOKEN), deadline)
        || !matchesInventory(inventory, inventorySource, pin, change.expectedEpoch)) return;
      deadline.throwIfAborted();
      preparationUnconfirmed = false;
      await instance.acquireLease(pin, change, AbortSignal.timeout(2_000));
    },
    readWorker: () => wiring.readWorker(),
    readAuthenticatedInventory: (source, signal) => wiring.readAuthenticatedInventory(source, signal),
    invokePrivate: (serving, call, signal) => {
      if (!matchesServingPin(serving, pin)) {
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
  // Still allow the protected caller to confirm and recover a concurrently issued lease.
  return preparationUnconfirmed && "code" in result && result.code === "lease_unconfirmed" ? UNCONFIRMED : result;
}
