import { boundRead } from "../src/internal-actions.ts";
import type { bindStagingAutomodDeploymentTransport } from "./staging-automod-deployment-transport.ts";

type Inventory = NonNullable<Awaited<ReturnType<Parameters<typeof bindStagingAutomodDeploymentTransport>[1]>>>;
type InventorySource = Readonly<{ workflowSha: string; runId: number }>;
type InventoryReadback = Inventory & Readonly<{ workflow_sha: string; workflow_run_id: number }>;
type RunVerifier = (workflowSha: string, runId: string, signal: AbortSignal) => Promise<string>;

const SHA = /^[0-9a-f]{40}$/;
const UUID = /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/;
const UTC = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,6})?(?:\+00:00|Z)$/;
const MAX_OUTPUT_LENGTH = 4_096;

function hasKeys<K extends string>(value: unknown, keys: readonly K[]): value is Record<K, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    && Object.keys(value).length === keys.length && keys.every(key => Object.hasOwn(value, key));
}

function matches(value: unknown, shape: RegExp): value is string {
  return typeof value === "string" && shape.test(value);
}

function validObservation(value: unknown): boolean {
  if (!matches(value, UTC)) return false;
  const observed = Date.parse(value);
  return Number.isFinite(observed) && new Date(observed).toISOString().slice(0, 19) === value.slice(0, 19);
}

function readback(value: unknown, workflowSha: string, runId: number): value is InventoryReadback {
  if (!hasKeys(value, ["schema_version", "workflow_sha", "workflow_run_id", "observed_at", "worker", "container", "ownership"])
    || value.schema_version !== 1 || value.workflow_sha !== workflowSha || value.workflow_run_id !== runId
    || !validObservation(value.observed_at)) return false;

  const { worker, container, ownership } = value;
  return hasKeys(worker, ["deployment_id", "version_id", "source_sha", "traffic_percent", "runtime_policy"])
    && matches(worker.deployment_id, UUID) && matches(worker.version_id, UUID) && matches(worker.source_sha, SHA)
    && worker.traffic_percent === 100 && worker.runtime_policy === "bounded"
    && hasKeys(container, ["application_id", "namespace_id", "rollout_id", "rollout_status", "image_digest"])
    && matches(container.application_id, UUID) && matches(container.namespace_id, /^[0-9a-f]{32}$/)
    && matches(container.rollout_id, UUID) && container.rollout_status === "completed"
    && matches(container.image_digest, /^sha256:[0-9a-f]{64}$/)
    && hasKeys(ownership, ["phase", "epoch"]) && ownership.phase === "active"
    && typeof ownership.epoch === "number" && Number.isSafeInteger(ownership.epoch) && ownership.epoch > 0;
}

/** Supply a bounded runner for scripts/staging_automod_inventory_reader.py.
 * Resolve stdout only on successful exit and cancel the verifier on abort.
 * The calling transport checks freshness and binds the observation to its lease.
 */
export function bindStagingAutomodInventoryReader(runVerifier: RunVerifier) {
  return async ({ workflowSha, runId }: InventorySource, signal: AbortSignal): Promise<InventoryReadback | undefined> => {
    if (signal.aborted || !matches(workflowSha, SHA) || !Number.isSafeInteger(runId) || runId <= 0) return;
    try {
      const output = await boundRead(runVerifier(workflowSha, String(runId), signal), signal);
      if (signal.aborted || typeof output !== "string" || output.length > MAX_OUTPUT_LENGTH) return;
      const value: unknown = JSON.parse(output);
      if (readback(value, workflowSha, runId)) return value;
    } catch {
      return;
    }
  };
}
