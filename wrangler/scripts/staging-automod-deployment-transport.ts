import { boundRead } from "../src/internal-actions.ts";
import { invokeProtectedAutomodJob } from "../src/staging-automod-entry.ts";
import { sameLease } from "../src/staging-automod-recovery.ts";
import type { runStagingAutomodBoundJob, runStagingAutomodPrivateJob } from "./staging-automod-private-job.ts";

type Transport = Parameters<typeof runStagingAutomodPrivateJob>[3];
type Worker = Pick<Parameters<typeof runStagingAutomodBoundJob>[3], "env" | "controlBearer">;
type Call = Parameters<Transport>[0];

// Projection of the authenticated staging-ownership-status inventory v1.
type ServingInventory = Readonly<{
  schema_version: 1;
  observed_at: string;
  worker: Readonly<{
    deployment_id: string; version_id: string; source_sha: string; traffic_percent: 100; runtime_policy: "bounded";
  }>;
  container: Readonly<{
    application_id: string; namespace_id: string; rollout_id: string; rollout_status: "completed"; image_digest: string;
  }>;
  ownership: Readonly<{ phase: "active"; epoch: number }>;
}>;

type ServingPin = Readonly<{
  workerDeploymentId: string; workerVersionId: string; sourceSha: string;
  applicationId: string; namespaceId: string; rolloutId: string; imageDigest: string;
}>;
type Binding = Readonly<{ issuedLease: Call["issuedLease"]; observed: number; pin: ServingPin }>;

const MAX_OBSERVATION_AGE_MS = 60_000;
const UTC_OBSERVATION = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,6})?(?:\+00:00|Z)$/;
const UUID = /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/;

function matches(value: unknown, shape: RegExp): boolean {
  return typeof value === "string" && shape.test(value);
}

function fresh(observed: number): boolean {
  const age = Date.now() - observed;
  return age >= 0 && age <= MAX_OBSERVATION_AGE_MS;
}

function servingBinding(inventory: ServingInventory | undefined, lease: Call["issuedLease"]): Binding {
  const { schema_version, observed_at, worker, container, ownership } = inventory ?? {};
  const observed = typeof observed_at === "string" && UTC_OBSERVATION.test(observed_at) ? Date.parse(observed_at) : NaN;
  // The lease's deploymentId is CF_VERSION_METADATA.id, not the provider's deployment record ID.
  if (schema_version !== 1 || !fresh(observed)
    || new Date(observed).toISOString().slice(0, 19) !== observed_at?.slice(0, 19)
    || worker?.version_id !== lease.deploymentId || worker.source_sha !== lease.sha
    || worker.traffic_percent !== 100 || worker.runtime_policy !== "bounded"
    || ownership?.phase !== "active" || ownership.epoch !== lease.epoch
    || !matches(worker.version_id, UUID) || !matches(worker.deployment_id, UUID)
    || container?.rollout_status !== "completed" || !matches(container.rollout_id, UUID)
    || !matches(container.application_id, UUID) || !matches(container.namespace_id, /^[0-9a-f]{32}$/)
    || !matches(container.image_digest, /^sha256:[0-9a-f]{64}$/)) {
    throw new Error("AutoMod serving deployment unconfirmed");
  }
  const pin = Object.freeze({ workerDeploymentId: worker.deployment_id, workerVersionId: worker.version_id,
    sourceSha: worker.source_sha, applicationId: container.application_id, namespaceId: container.namespace_id,
    rolloutId: container.rollout_id, imageDigest: container.image_digest });
  return { issuedLease: lease, observed, pin };
}

/** Create one dormant transport per authenticated protected job after the job
 * scope checks. The reader authenticates the staging inventory artifact and
 * binds its workflow SHA and run ID as required by the protected readback.
 * The observation must be recent and its UTC clock must agree with this job.
 * Resolve the private caller only after Worker admission, on the exact pin.
 * Recovery uses the original route after startup reaches it, including after
 * a lost startup response. Worker ownership and cleanup checks still apply.
 */
export function bindStagingAutomodDeploymentTransport(worker: Worker,
  readInventory: (signal: AbortSignal) => Promise<ServingInventory | undefined>,
  invoke: (pin: ServingPin, call: Call, signal: AbortSignal) => Promise<void>,
): Transport {
  let binding: Binding | undefined;
  return async (call, approval, signal) => {
    signal.throwIfAborted();
    const request = new Request("https://worker.invalid", {
      method: "POST", headers: { authorization: `Bearer ${worker.controlBearer}` }, body: JSON.stringify(call), signal,
    });
    return Response.json(await invokeProtectedAutomodJob(request, worker.env, approval, async admitted => {
      signal.throwIfAborted();
      let current = binding;
      if (!current) {
        const inventory = await boundRead(readInventory(signal), signal);
        signal.throwIfAborted();
        current = binding ?? servingBinding(inventory, admitted.issuedLease);
      }
      if (!sameLease(current.issuedLease, admitted.issuedLease)
        || (admitted.action === "start" && !fresh(current.observed))) {
        throw new Error("AutoMod serving deployment unconfirmed");
      }
      signal.throwIfAborted();
      binding = current;
      await invoke(current.pin, admitted, signal);
    }));
  };
}
