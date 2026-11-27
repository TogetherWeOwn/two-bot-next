import { sameLease, STAGING_AUTOMOD_TARGET, type AutomodIntent, type CommandRevision } from "../src/staging-automod-recovery.ts";

type Lease = AutomodIntent["lease"] & Readonly<{ purpose: "automod-preview"; runAttempt: 1 }>;

export type StagingAutomodQualificationPin = Readonly<{
  runId: string;
  lease: Lease;
  serving: Readonly<{
    workerDeploymentId: string;
    workerVersionId: string;
    sourceSha: string;
    containerApplicationId: string;
    containerNamespaceId: string;
    containerRolloutId: string;
    containerImageDigest: string;
    ownerEpoch: number;
  }>;
  command: CommandRevision;
}>;

const MAX_OBSERVATION_AGE_MS = 60_000;
const UUID = /^[0-9a-f]{8}(-[0-9a-f]{4}){3}-[0-9a-f]{12}$/;
const OPTIONS = [
  { name: "text", type: 3, required: true, autocomplete: false, min_length: 1, max_length: 2000 },
  { name: "mentions", type: 4, required: false, autocomplete: false, min_value: 0, max_value: 100 },
  { name: "attachments", type: 3, required: false, autocomplete: false, max_length: 1000 },
] as const;

function field(value: unknown, key: PropertyKey): unknown {
  if (value === null || typeof value !== "object") return;
  const own = Object.getOwnPropertyDescriptor(value, key);
  return own && Object.hasOwn(own, "value") ? own.value : undefined;
}

// Walk only the fixed receipt schema. Reject extra keys at every level,
// including array properties and fields that JSON serialization would omit.
function exact(actual: unknown, expected: unknown): boolean {
  if (expected === null || typeof expected !== "object") return actual === expected;
  if (actual === null || typeof actual !== "object" || Array.isArray(actual) !== Array.isArray(expected)) return false;
  const prototype: unknown = Object.getPrototypeOf(actual);
  if (prototype !== null && prototype !== Object.getPrototypeOf(expected)) return false;
  const keys = Reflect.ownKeys(expected);
  return Reflect.ownKeys(actual).length === keys.length
    && keys.every(key => exact(field(actual, key), field(expected, key)));
}

function matches(value: unknown, shape: RegExp): boolean {
  return typeof value === "string" && value.length <= 71 && shape.test(value);
}

function snowflake(value: unknown): boolean {
  return typeof value === "string" && /^[1-9][0-9]{16,19}$/.test(value) && BigInt(value) < 2n ** 64n;
}

function time(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

/** Verify a bounded, decoded staging qualification receipt. The caller supplies
 * independently authenticated pins, observations and a trusted UTC clock. The
 * registry projection comes from the complete, unique guild command read and
 * normalizes omitted required/autocomplete flags to false. Its interval spans
 * the entire read. Router outcomes come from synthetic dispatch on the pinned
 * source with Manage Guild alone and with moderation permissions but without
 * Manage Guild or Administrator. Cold refusal counters cover the entire isolated pre-lease probe,
 * including transient lease/Container activity between the two state samples.
 * Times are UTC epoch milliseconds. No input field is returned or logged.
 */
export function verifyStagingAutomodReceipt(receipt: unknown, pin: StagingAutomodQualificationPin,
  now = Date.now()): boolean {
  try {
    const { lease, serving, command, runId } = pin;
    const { deploymentId, sha, epoch, acquiredAt, expiresAt } = lease;
    const { workerDeploymentId, workerVersionId, sourceSha, ownerEpoch, containerApplicationId,
      containerNamespaceId, containerRolloutId, containerImageDigest } = serving;
    if (!time(now) || !sameLease(lease, lease) || lease.purpose !== "automod-preview" || lease.runAttempt !== 1
      || runId !== lease.runId || workerVersionId !== deploymentId || sourceSha !== sha || ownerEpoch !== epoch
      || ![workerDeploymentId, workerVersionId, containerApplicationId, containerRolloutId].every(id => matches(id, UUID))
      || !matches(containerNamespaceId, /^[0-9a-f]{32}$/) || !matches(containerImageDigest, /^sha256:[0-9a-f]{64}$/)
      || !snowflake(command.id) || !snowflake(command.version)) return false;

    const beforeAt = field(field(receipt, "before"), "observedAt");
    const afterAt = field(field(receipt, "after"), "observedAt");
    const registry = field(receipt, "registry");
    const readStartedAt = field(registry, "startedAt");
    const readCompletedAt = field(registry, "completedAt");
    const cold = field(receipt, "coldStartRefusal");
    const coldStartedAt = field(cold, "startedAt");
    const coldCompletedAt = field(cold, "completedAt");
    const refusedAt = field(field(cold, "response"), "observedAt");
    if (!time(beforeAt) || !time(afterAt) || !time(readStartedAt) || !time(readCompletedAt)
      || !time(coldStartedAt) || !time(coldCompletedAt) || !time(refusedAt)
      || coldStartedAt < now - MAX_OBSERVATION_AGE_MS || coldStartedAt >= coldCompletedAt
      || refusedAt < coldStartedAt || refusedAt > coldCompletedAt
      || coldCompletedAt > acquiredAt || acquiredAt > beforeAt || beforeAt > readStartedAt
      || readStartedAt >= readCompletedAt || readCompletedAt > afterAt || afterAt > now || now >= expiresAt) return false;

    const scope = { runId, runAttempt: 1, lease: { environment: "staging", purpose: "automod-preview", phase: "active",
      runId, runAttempt: 1, deploymentId, sha, epoch, acquiredAt, expiresAt } };
    const identity = { workerDeploymentId, workerVersionId, sourceSha, containerApplicationId, containerNamespaceId,
      containerRolloutId, containerImageDigest, ownerEpoch, ownerPhase: "active", trafficPercent: 100,
      runtimePolicy: "bounded", rolloutStatus: "completed" };
    const commandIdentity = { ...STAGING_AUTOMOD_TARGET, id: command.id, version: command.version, type: 1 };
    const stopped = { lease: "absent", container: "stopped" };
    return exact(receipt, {
      schema: "two-bot-staging-automod-qualification.v1",
      before: { scope, observedAt: beforeAt, identity },
      registry: { scope, startedAt: readStartedAt, completedAt: readCompletedAt,
        command: { ...commandIdentity, default_member_permissions: "32", options: OPTIONS } },
      after: { scope, observedAt: afterAt, identity },
      router: { sourceSha, name: STAGING_AUTOMOD_TARGET.name, requiredPermission: 32,
        withManageGuild: "admitted", withoutManageGuild: "denied" },
      coldStartRefusal: { scope, action: "start", startedAt: coldStartedAt, completedAt: coldCompletedAt,
        response: { schema: "two-bot-staging-automod-job.v1", environment: "staging", observedAt: refusedAt,
          verdict: "failed", code: "request_refused" }, before: stopped, after: stopped,
        effects: { leaseWrites: 0, containerAcquisitions: 0, containerInvocations: 0,
          containerStarts: 0, containerStops: 0, containerWrites: 0 } },
    });
  } catch {
    return false;
  }
}
