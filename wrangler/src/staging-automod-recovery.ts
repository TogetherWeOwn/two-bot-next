import { isSnowflake } from "./container-env.ts";
import type { admitStagingAutomod } from "./staging-automod.ts";
import { MAX_STAGING_USE_MS, type StagingUse } from "./staging-lifecycle.ts";

const KEY = "two-bot:staging-automod-recovery:v1";
// Same fixed guild slash identity as the protected staging preview reader.
export const STAGING_AUTOMOD_TARGET = Object.freeze({
  applicationId: "1469137636663758888", guildId: "1545644954272137297",
  name: "automod-preview",
} as const);

type Lease = Readonly<StagingUse>;
type Admission = Readonly<Awaited<ReturnType<typeof admitStagingAutomod>>>;
export type CommandRevision = Readonly<{ id: string; version: string }>;
export type AutomodIntent = Readonly<{
  lease: Lease;
  publishBefore: number;
  target: typeof STAGING_AUTOMOD_TARGET;
  baseline: string;
}>;
export type AutomodOwnedCommand = CommandRevision & Readonly<{ intent: AutomodIntent }>;

type Journal = Readonly<{
  intent: AutomodIntent;
  phase: "publishing" | "cleanup" | "complete";
  created: CommandRevision | null;
}>;

interface JournalStorage {
  get(key: string): Promise<unknown>;
  put(key: string, journal: Journal): Promise<void>;
  sync(): Promise<void>;
}

type CleanupProof = Readonly<{
  intent: AutomodIntent;
  authorized: true;
  stopped: true;
  publicationDrained: true;
}> & (
  | Readonly<{ registry: "absent" }>
  | Readonly<{ registry: "owned"; command: CommandRevision }>
);

/** The protected caller supplies the existing staging authority and writer fence.
 * Inspect must prove the original publication cannot still land remotely, and
 * affirm absence with a complete scoped read. "Owned" additionally requires
 * attributable creation and matching list/detail. A matching name/definition,
 * a timeout, or an aborted client request is insufficient. Unknown outcomes throw.
 */
export interface AutomodCleanup {
  inspect(intent: AutomodIntent, created: CommandRevision | null): Promise<CleanupProof>;
  /** Recheck cleanup authority and exact scoped ID/version at the DELETE. No
   * registry replacement, name-based removal, or mutation of a changed command. */
  removeOwned(owned: AutomodOwnedCommand): Promise<void>;
}

function record(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function validLease(lease: unknown): lease is Lease {
  if (!record(lease)) return false;
  const v = lease as Lease;
  return v.environment === "staging" && v.phase === "active" && !("voiceMove" in v)
    && typeof v.deploymentId === "string" && /^[a-zA-Z0-9_-]{1,128}$/.test(v.deploymentId)
    && typeof v.sha === "string" && /^[0-9a-f]{40}$/.test(v.sha)
    && typeof v.runId === "string" && /^[1-9][0-9]{0,19}$/.test(v.runId)
    && Number.isSafeInteger(v.epoch) && v.epoch > 0
    && Number.isSafeInteger(v.runAttempt) && v.runAttempt > 0
    && Number.isSafeInteger(v.acquiredAt) && v.acquiredAt >= 0
    && Number.isSafeInteger(v.expiresAt) && v.expiresAt > v.acquiredAt
    && v.expiresAt - v.acquiredAt <= MAX_STAGING_USE_MS;
}

function sameLease(expected: Lease, other: unknown): boolean {
  return validLease(other) && expected.deploymentId === other.deploymentId && expected.sha === other.sha
    && expected.epoch === other.epoch && expected.runId === other.runId && expected.runAttempt === other.runAttempt
    && expected.acquiredAt === other.acquiredAt && expected.expiresAt === other.expiresAt;
}

function validIntent(value: unknown): value is AutomodIntent {
  if (!record(value) || !record(value.target) || !validLease(value.lease)) return false;
  const { target, publishBefore, baseline, lease } = value;
  const expected = STAGING_AUTOMOD_TARGET;
  return Object.keys(target).length === 3 && target.applicationId === expected.applicationId && target.guildId === expected.guildId
    && target.name === expected.name && typeof publishBefore === "number" && Number.isSafeInteger(publishBefore)
    && publishBefore > lease.acquiredAt && publishBefore < lease.expiresAt
    && typeof baseline === "string" && /^[0-9a-f]{64}$/.test(baseline);
}

function copyIntent(intent: AutomodIntent): AutomodIntent {
  const { deploymentId, sha, epoch, runId, runAttempt, acquiredAt, expiresAt } = intent.lease;
  return Object.freeze({
    lease: Object.freeze({
      environment: "staging", phase: "active", deploymentId, sha, epoch, runId, runAttempt, acquiredAt, expiresAt,
    }),
    target: STAGING_AUTOMOD_TARGET,
    publishBefore: intent.publishBefore,
    baseline: intent.baseline,
  });
}

function sameIntent(expected: AutomodIntent, other: unknown): boolean {
  return validIntent(other) && expected.baseline === other.baseline
    && expected.publishBefore === other.publishBefore && sameLease(expected.lease, other.lease);
}

function snowflake(value: unknown): value is string {
  return typeof value === "string" && /^[1-9][0-9]{16,19}$/.test(value) && isSnowflake(value);
}

function validRevision(value: unknown): value is CommandRevision {
  return record(value) && snowflake(value.id) && snowflake(value.version);
}

function sameRevision(expected: CommandRevision, actual: CommandRevision): boolean {
  return expected.id === actual.id && expected.version === actual.version;
}

/** Dormant DO journal. The existing ownership gate must serialize each whole
 * operation with publication, cleanup, lifecycle and other registry writers.
 * The active hold must bar competing uses across restart. This class supplies
 * no authentication, transport, alarms or activation on its own.
 */
export class StagingAutomodRecovery {
  private readonly storage: JournalStorage;

  constructor(storage: JournalStorage) {
    this.storage = storage;
  }

  /** Resume using the original intent, even if the live lease expired or was released. */
  async pending(): Promise<AutomodIntent | undefined> {
    const journal = await this.read();
    return journal?.phase === "complete" ? undefined : journal?.intent;
  }

  /** The publication adapter supplies a digest from its fresh absent registry
   * read under current admission. Success permits a single POST, subject to
   * the adapter's own send-boundary checks. A repeat must go to recovery. */
  async persistIntent(intent: AutomodIntent, currentAdmission: () => Promise<Admission | undefined>,
    now = Date.now): Promise<true> {
    if (!validIntent(intent)) throw new Error("staging_automod_intent_invalid");
    const saved = copyIntent(intent);
    const { lease, publishBefore } = saved;
    const old = await this.read();
    if (old && (old.phase !== "complete" || sameLease(old.intent.lease, lease))) {
      throw new Error("staging_automod_intent_held");
    }
    const requireActive = async () => {
      let current: Admission | undefined;
      try { current = await currentAdmission(); }
      catch { throw new Error("staging_automod_admission_mismatch"); }
      const at = now();
      if (current?.baselineAbsent !== true || current.publishBefore !== publishBefore || !sameLease(lease, current.lease)
        || !Number.isSafeInteger(at) || at < lease.acquiredAt || at >= publishBefore) {
        throw new Error("staging_automod_admission_mismatch");
      }
    };
    await requireActive();
    await this.write({ intent: saved, phase: "publishing", created: null });
    await requireActive();
    return true;
  }

  /** Record only a validated fresh-create receipt. The adapter still owes
   * the positive detail and full-registry delta before it reports success. */
  async recordCreated(command: AutomodOwnedCommand): Promise<true> {
    const journal = await this.read();
    if (!journal || journal.phase !== "publishing" || !validRevision(command) || !sameIntent(journal.intent, command.intent)
      || (journal.created !== null && !sameRevision(journal.created, command))) {
      throw new Error("staging_automod_receipt_mismatch");
    }
    if (journal.created === null) {
      await this.write({ ...journal, created: { id: command.id, version: command.version } });
    }
    return true;
  }

  /** Fence new publication before inspection. Finish only on affirmative
   * absence and confirmed stop under the existing cleanup authority. */
  async recover(lease: Lease, adapter: AutomodCleanup): Promise<void> {
    let journal = await this.read();
    if (!journal || !sameLease(journal.intent.lease, lease)) {
      throw new Error("staging_automod_recovery_mismatch");
    }
    if (journal.phase === "complete") return;
    if (journal.phase === "publishing") {
      journal = { ...journal, phase: "cleanup" };
      await this.write(journal);
    }
    const { intent, created } = journal;
    const proof = await this.inspect(adapter, intent, created);
    if (proof.registry === "owned") {
      const command = Object.freeze({ id: proof.command.id, version: proof.command.version });
      if (created === null) {
        // A separately proven lost receipt must become durable before DELETE.
        journal = { ...journal, created: command };
        await this.write(journal);
      }
      try { await adapter.removeOwned(Object.freeze({ intent, ...command })); }
      catch { throw new Error("staging_automod_removal_unverified"); }
      if ((await this.inspect(adapter, intent, command)).registry !== "absent") {
        throw new Error("staging_automod_absence_unverified");
      }
    }
    // Retain a singleton replay tombstone. The staging lifecycle prevents an
    // ended run/attempt from reacquiring its old lease after it is replaced.
    await this.write({ ...journal, phase: "complete" });
  }

  private async inspect(adapter: AutomodCleanup, intent: AutomodIntent, created: CommandRevision | null):
    Promise<CleanupProof> {
    let proof: CleanupProof;
    try { proof = await adapter.inspect(intent, created); }
    catch { throw new Error("staging_automod_cleanup_unverified"); }
    if (!proof || !sameIntent(intent, proof.intent) || proof.authorized !== true
      || proof.stopped !== true || proof.publicationDrained !== true
      || (proof.registry !== "absent" && (proof.registry !== "owned" || !validRevision(proof.command)
        || (created !== null && !sameRevision(created, proof.command))))) {
      throw new Error("staging_automod_cleanup_unverified");
    }
    return proof;
  }

  private async read(): Promise<Journal | undefined> {
    let value: unknown;
    try {
      // An earlier failed flush can leave a speculative record in the DO cache.
      await this.storage.sync();
      value = await this.storage.get(KEY);
    }
    catch { throw new Error("staging_automod_storage_unavailable"); }
    if (value === undefined) return undefined;
    if (!record(value) || !validIntent(value.intent)
      || (value.phase !== "publishing" && value.phase !== "cleanup" && value.phase !== "complete")
      || (value.created !== null && !validRevision(value.created))) {
      throw new Error("staging_automod_storage_invalid");
    }
    const created = value.created === null ? null : Object.freeze({ id: value.created.id, version: value.created.version });
    return { intent: copyIntent(value.intent), phase: value.phase, created };
  }

  private async write(journal: Journal): Promise<void> {
    try {
      await this.storage.put(KEY, journal);
      await this.storage.sync();
    } catch { throw new Error("staging_automod_storage_unavailable"); }
  }
}
