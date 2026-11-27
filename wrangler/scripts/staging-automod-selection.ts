import { boundRead } from "../src/internal-actions.ts";
import type { StagingAutomodOperation } from "../src/staging-automod-bridge.ts";
import { automodReservationAllowed, STAGING_AUTOMOD_PLAN, type StagingAutomodPlan } from "../src/staging-automod-plan.ts";
import { STAGING_AUTOMOD_TARGET } from "../src/staging-automod-recovery.ts";
import { parseStagingUseChange } from "../src/staging-lifecycle.ts";
import type { bindStagingAutomodInventoryReader } from "./staging-automod-inventory-reader.ts";

type Reservation = Omit<Extract<StagingAutomodOperation, { op: "acquire" }>, "op">;
type Pin = Reservation["pin"];
type ReadInventory = ReturnType<typeof bindStagingAutomodInventoryReader>;
type InventorySource = Parameters<ReadInventory>[0];
type Dispatch = Readonly<{ sha: string; runId: string; runAttempt: number }>;
type PostDeployRun = Readonly<{
  dispatch: Dispatch;
  inventoryRun: InventorySource & Readonly<{ runAttempt: number }>;
  pin: Pin;
  target: Readonly<{ applicationId: string; guildId: string; name: string }>;
  expiresAt: number;
  voiceMove?: boolean;
}>;
type Readbacks = Readonly<{
  // Authenticate the protected dispatch, the successful inventory producer and
  // its post-deploy serving and Discord binding evidence. Return the complete
  // candidate set for the dispatch, including conflicts. An incomplete read
  // must fail. The expiry is the approved reservation cutoff.
  readPostDeployRuns: (dispatch: Dispatch, signal: AbortSignal) => Promise<readonly PostDeployRun[] | undefined>;
  readAuthenticatedInventory: ReadInventory;
}>;
type Selection = Readonly<{
  serving: Readonly<{ deploymentId: string; sha: string }>;
  inventorySource: InventorySource;
  reservation: Reservation;
}>;

const READ_BUDGET_MS = 2_000;
const MAX_OBSERVATION_AGE_MS = 60_000;
const SHA = /^[0-9a-f]{40}$/;
const UUID = /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/;
const UTC = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,6})?(?:\+00:00|Z)$/;

function matches(value: unknown, pattern: RegExp): boolean {
  return typeof value === "string" && pattern.test(value);
}

function validPin(pin: Pin, sha: string): boolean {
  return pin.sourceSha === sha && matches(pin.workerDeploymentId, UUID) && matches(pin.workerVersionId, UUID)
    && matches(pin.applicationId, UUID) && matches(pin.namespaceId, /^[0-9a-f]{32}$/)
    && matches(pin.rolloutId, UUID) && matches(pin.imageDigest, /^sha256:[0-9a-f]{64}$/);
}

function observedRecently(value: string, now: number): boolean {
  if (!matches(value, UTC)) return false;
  const observed = Date.parse(value);
  const age = now - observed;
  return age >= 0 && age <= MAX_OBSERVATION_AGE_MS
    && new Date(observed).toISOString().slice(0, 19) === value.slice(0, 19);
}

/** Resolve a dormant dispatch binding from authenticated readbacks. Each reader
 * owns its authentication and must honor the supplied signal. The resolver
 * shares one read budget across both and refuses even if a reader stalls.
 */
export async function resolveStagingAutomodSelection(dispatch: Dispatch, readers: Readbacks, signal: AbortSignal,
  plan: StagingAutomodPlan = STAGING_AUTOMOD_PLAN): Promise<Selection | undefined> {
  const { sha, runId, runAttempt } = dispatch;
  const startedAt = Date.now();
  if (signal.aborted || !matches(sha, SHA) || !matches(runId, /^[1-9][0-9]{0,15}$/)
    || !Number.isSafeInteger(Number(runId)) || runAttempt !== 1 || plan.enabled !== true
    || !Number.isSafeInteger(plan.notAfter) || startedAt >= plan.notAfter
    || !Number.isSafeInteger(plan.leaseMs) || plan.leaseMs <= 0 || plan.leaseMs > STAGING_AUTOMOD_PLAN.leaseMs) return;

  const deadline = performance.now() + READ_BUDGET_MS;
  const readSignal = AbortSignal.any([signal, AbortSignal.timeout(READ_BUDGET_MS)]);
  const outOfTime = () => readSignal.aborted || performance.now() >= deadline;
  try {
    const runs = await boundRead(readers.readPostDeployRuns(Object.freeze({ sha, runId, runAttempt }), readSignal), readSignal);
    if (outOfTime() || !Array.isArray(runs) || runs.length !== 1) return;
    const run: PostDeployRun | undefined = runs[0];
    if (run?.dispatch?.sha !== sha || run.dispatch.runId !== runId || run.dispatch.runAttempt !== 1
      || run.target?.applicationId !== STAGING_AUTOMOD_TARGET.applicationId
      || run.target.guildId !== STAGING_AUTOMOD_TARGET.guildId || run.target.name !== STAGING_AUTOMOD_TARGET.name
      || (run.voiceMove !== undefined && run.voiceMove !== false)) return;

    const { expiresAt, inventoryRun, pin: evidence } = run;
    const inventoryRunId = inventoryRun?.runId;
    if (!Number.isSafeInteger(expiresAt) || expiresAt <= Date.now() || expiresAt > plan.notAfter
      || expiresAt - startedAt > plan.leaseMs || inventoryRun?.workflowSha !== sha || inventoryRun.runAttempt !== 1
      || typeof inventoryRunId !== "number" || !Number.isSafeInteger(inventoryRunId) || inventoryRunId <= 0
      || String(inventoryRunId) === runId
      || !evidence) return;

    // The launch target above is a Discord snowflake. The bridge pin uses the
    // Cloudflare Container application UUID, corroborated by the inventory.
    const pin = Object.freeze({ workerDeploymentId: evidence.workerDeploymentId, workerVersionId: evidence.workerVersionId,
      sourceSha: evidence.sourceSha, applicationId: evidence.applicationId, namespaceId: evidence.namespaceId,
      rolloutId: evidence.rolloutId, imageDigest: evidence.imageDigest });
    if (!validPin(pin, sha) || outOfTime()) return;
    const inventorySource = Object.freeze({ workflowSha: sha, runId: inventoryRunId });
    const inventory = await boundRead(readers.readAuthenticatedInventory(inventorySource, readSignal), readSignal);
    if (outOfTime() || inventory?.schema_version !== 1 || inventory.workflow_sha !== sha
      || inventory.workflow_run_id !== inventorySource.runId) return;

    const { worker, container, ownership } = inventory;
    if (worker?.deployment_id !== pin.workerDeploymentId || worker.version_id !== pin.workerVersionId
      || worker.source_sha !== sha || worker.traffic_percent !== 100 || worker.runtime_policy !== "bounded"
      || container?.application_id !== pin.applicationId || container.namespace_id !== pin.namespaceId
      || container.rollout_id !== pin.rolloutId || container.image_digest !== pin.imageDigest
      || container.rollout_status !== "completed" || ownership?.phase !== "active") return;

    const serving = Object.freeze({ deploymentId: pin.workerVersionId, sha });
    const change = parseStagingUseChange({ ...serving, environment: "staging", action: "acquire", runId,
      runAttempt: 1, expectedEpoch: ownership.epoch, expiresAt });
    const now = Date.now();
    if (!change || now >= expiresAt || !observedRecently(inventory.observed_at, now)
      || !automodReservationAllowed(plan, { runId, sha }, change, startedAt) || outOfTime()) return;
    return Object.freeze({ serving, inventorySource, reservation: Object.freeze({ pin, change: Object.freeze(change) }) });
  } catch {
    return;
  }
}
