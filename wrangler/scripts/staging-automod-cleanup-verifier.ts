import { sameLease, STAGING_AUTOMOD_TARGET, type CommandRevision } from "../src/staging-automod-recovery.ts";
import { automodRecoverAllowed, STAGING_AUTOMOD_PLAN, type StagingAutomodPlan } from "../src/staging-automod-plan.ts";
import { AUTOMOD_STAGING_PURPOSE } from "../src/staging-use-window.ts";

type Lease = Parameters<typeof automodRecoverAllowed>[2];
type CleanupVerdict = Readonly<{ cleaned: true; runId: string; sha: string; epoch: number; verifiedAt: number }>
  | Readonly<{ cleaned: false; reasons: readonly string[] }>;

const MAX_OBSERVATION_AGE_MS = 60_000;
const LEASE_KEYS = ["environment", "phase", "deploymentId", "sha", "epoch", "runId", "runAttempt",
  "acquiredAt", "expiresAt", "purpose"] as const;

// Snapshot only the required own data fields. Getters, symbol keys and extra
// non-enumerable fields cannot supply or hide receipt evidence.
function readRecord<K extends string>(value: unknown, keys: readonly K[]): Record<K, unknown> | undefined {
  if (value === null || typeof value !== "object" || Array.isArray(value)) return;
  const prototype: unknown = Object.getPrototypeOf(value);
  if (prototype !== Object.prototype && prototype !== null) return;
  if (Reflect.ownKeys(value).length !== keys.length) return;

  const snapshot = Object.create(null) as Record<K, unknown>;
  for (const key of keys) {
    const field = Object.getOwnPropertyDescriptor(value, key);
    if (!field || !Object.hasOwn(field, "value")) return;
    snapshot[key] = field.value;
  }
  return snapshot;
}

function leaseFields(value: unknown) {
  const lease = readRecord(value, LEASE_KEYS);
  return lease?.purpose === AUTOMOD_STAGING_PURPOSE ? lease : undefined;
}

function fixedTarget(value: unknown): boolean {
  const target = readRecord(value, ["applicationId", "guildId", "name"]);
  return target?.applicationId === STAGING_AUTOMOD_TARGET.applicationId
    && target.guildId === STAGING_AUTOMOD_TARGET.guildId && target.name === STAGING_AUTOMOD_TARGET.name;
}

function snowflake(value: unknown): value is string {
  return typeof value === "string" && /^[1-9][0-9]{16,19}$/.test(value) && BigInt(value) < 2n ** 64n;
}

function revision(value: unknown): CommandRevision | undefined {
  const command = readRecord(value, ["id", "version"]);
  if (command && snowflake(command.id) && snowflake(command.version)) {
    return { id: command.id, version: command.version };
  }
}

function validTime(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

function fresh(value: unknown, lease: Lease, now: number): value is number {
  return validTime(value) && value >= lease.acquiredAt && value <= now && now - value <= MAX_OBSERVATION_AGE_MS;
}

function refused(reason: string): CleanupVerdict {
  return Object.freeze({ cleaned: false, reasons: Object.freeze([reason]) });
}

/** Verify decoded evidence from an authenticated collector and a trusted clock.
 * `ownedCommand: null` is a positive never-created pin. `registry: "absent"`
 * attests a complete guild read after confirming the stop. The collector
 * confirms the stop after fencing and draining remote startup and publication.
 * The collector must authenticate the run, original lease, ownership pin and
 * observations, including the exact revision removed when a command existed.
 * Times are UTC epoch milliseconds. Recovery may use the original expired lease.
 */
export function verifyStagingAutomodCleanup(input: unknown, now: number): CleanupVerdict {
  try {
    const evidence = readRecord(input, ["promotionDisabled", "run", "expectedLease", "ownedCommand",
      "runtimeStop", "publicationFence", "discordRegistry"]);
    if (!evidence || !validTime(now)) return refused("invalid_evidence");
    if (evidence.promotionDisabled !== true) return refused("promotion_not_disabled");

    const run = readRecord(evidence.run, ["runId", "sha"]);
    const lease = leaseFields(evidence.expectedLease) as Lease | undefined;
    const plan: StagingAutomodPlan = STAGING_AUTOMOD_PLAN;
    if (!lease || !sameLease(lease, lease) || typeof run?.runId !== "string" || typeof run.sha !== "string"
      || !automodRecoverAllowed(plan, { runId: run.runId, sha: run.sha }, lease)) {
      return refused("recovery_not_authorized");
    }

    const fence = readRecord(evidence.publicationFence, ["lease", "target", "observedAt", "publicationDrained", "startupDrained"]);
    if (!fence || fence.publicationDrained !== true || fence.startupDrained !== true || !fixedTarget(fence.target)
      || !sameLease(lease, leaseFields(fence.lease)) || !fresh(fence.observedAt, lease, now)) {
      return refused("publication_drain_unverified");
    }

    const stop = readRecord(evidence.runtimeStop, ["lease", "observedAt", "running"]);
    if (!stop || stop.running !== false || !sameLease(lease, leaseFields(stop.lease))
      || !fresh(stop.observedAt, lease, now) || stop.observedAt < fence.observedAt) return refused("runtime_stop_unverified");

    const registry = readRecord(evidence.discordRegistry, ["lease", "target", "observedAt", "registry", "deletedCommand"]);
    if (!registry || registry.registry !== "absent" || !fixedTarget(registry.target)
      || !sameLease(lease, leaseFields(registry.lease)) || !fresh(registry.observedAt, lease, now)
      || registry.observedAt < stop.observedAt) return refused("registry_absence_unverified");

    if (evidence.ownedCommand !== null || registry.deletedCommand !== null) {
      const owned = revision(evidence.ownedCommand);
      const deleted = revision(registry.deletedCommand);
      if (!owned || !deleted || owned.id !== deleted.id || owned.version !== deleted.version) {
        return refused("command_deletion_unverified");
      }
    }

    return Object.freeze({ cleaned: true, runId: lease.runId, sha: lease.sha, epoch: lease.epoch, verifiedAt: now });
  } catch {
    return refused("invalid_evidence");
  }
}
