import { OwnershipRefused, type OwnerRecord } from "./ownership.ts";
import { StagingAutomodRecovery } from "./staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE, MAX_STAGING_USE_MS } from "./staging-use-window.ts";

export { AUTOMOD_STAGING_PURPOSE, MAX_STAGING_USE_MS } from "./staging-use-window.ts";

export const STAGING_USE_PATH = "/internal/staging-use";
export const STAGING_USE_KEY = "two-bot:staging-use:v1";
export const VOICE_MOVE_LEASE_HEADER = "x-two-voice-move-until";
const USED_PREFIX = "two-bot:staging-use-ended:v1:";

export interface StagingWindow {
  environment: "staging";
  runId: string;
  runAttempt: number;
  expiresAt: number;
}

export interface StagingUse extends StagingWindow {
  deploymentId: string;
  sha: string;
  epoch: number;
  acquiredAt: number;
  phase: "preparing" | "active" | "released";
  voiceMove?: true;
  purpose?: typeof AUTOMOD_STAGING_PURPOSE;
}

export interface StagingIdentity {
  deploymentId: string;
  sha: string;
  owner: OwnerRecord;
}

export interface StagingUseChange extends StagingWindow {
  action: "acquire" | "release" | "stop";
  expectedEpoch: number;
  deploymentId: string;
  sha: string;
  voiceMove?: true;
}

function validWindow(value: unknown): value is StagingWindow {
  if (!value || typeof value !== "object") return false;
  const v = value as StagingWindow;
  return v.environment === "staging" && typeof v.runId === "string" && /^[1-9][0-9]{0,19}$/.test(v.runId)
    && Number.isSafeInteger(v.runAttempt) && v.runAttempt > 0
    && Number.isSafeInteger(v.expiresAt) && v.expiresAt > 0;
}

function sameWindow(a: StagingWindow, b: StagingWindow): boolean {
  return a.runId === b.runId && a.runAttempt === b.runAttempt && a.expiresAt === b.expiresAt;
}

function validIdentity(v: { deploymentId: unknown; sha: unknown; epoch: unknown }): boolean {
  return typeof v.deploymentId === "string" && /^[a-zA-Z0-9_-]{1,128}$/.test(v.deploymentId)
    && typeof v.sha === "string" && /^[0-9a-f]{40}$/.test(v.sha)
    && Number.isSafeInteger(v.epoch) && (v.epoch as number) > 0;
}

export function parseStagingUseChange(value: unknown): StagingUseChange | null {
  if (!validWindow(value)) return null;
  const v = value as StagingUseChange;
  if ((v.action !== "acquire" && v.action !== "release" && v.action !== "stop")
    || !validIdentity({ ...v, epoch: v.expectedEpoch })
    || "purpose" in v
    || ("voiceMove" in v && !(v.action === "acquire" && v.voiceMove === true))) return null;
  return {
    environment: "staging", action: v.action, runId: v.runId, runAttempt: v.runAttempt,
    expiresAt: v.expiresAt, expectedEpoch: v.expectedEpoch, deploymentId: v.deploymentId, sha: v.sha,
    ...(v.voiceMove === true ? { voiceMove: true } : {}),
  };
}

function ownedBy(use: StagingUse, identity: StagingIdentity): boolean {
  return use.deploymentId === identity.deploymentId && use.sha === identity.sha
    && use.epoch === identity.owner.epoch && identity.owner.phase === "active"
    && identity.owner.deploymentId === identity.deploymentId;
}

function activeFor(use: StagingUse, identity: StagingIdentity, now: number): boolean {
  return use.phase === "active" && use.acquiredAt <= now && now < use.expiresAt && ownedBy(use, identity);
}

export function voiceMoveAllowed(use: StagingUse | undefined, identity: StagingIdentity, now = Date.now()): boolean {
  return use?.voiceMove === true && use.purpose === undefined && activeFor(use, identity, now);
}

export function hasAutomodReservation(use: StagingUse | undefined): boolean {
  return use?.purpose === AUTOMOD_STAGING_PURPOSE && use.phase !== "released";
}

/** All calls, including native scheduling and teardown, stay in the DO gate. */
export class StagingLifecycle {
  private readonly storage: DurableObjectStorage;

  constructor(storage: DurableObjectStorage) { this.storage = storage; }

  async read(): Promise<StagingUse | undefined> {
    let value: StagingUse | undefined;
    try { value = await this.storage.get<StagingUse>(STAGING_USE_KEY); }
    catch { throw new OwnershipRefused("staging_storage_unavailable"); }
    if (value !== undefined && (!validWindow(value) || !validIdentity(value)
      || !Number.isSafeInteger(value.acquiredAt) || value.acquiredAt < 0
      || value.expiresAt <= value.acquiredAt || value.expiresAt - value.acquiredAt > MAX_STAGING_USE_MS
      || ("voiceMove" in value && value.voiceMove !== true)
      || ("purpose" in value && (value.purpose !== AUTOMOD_STAGING_PURPOSE || "voiceMove" in value))
      || !["preparing", "active", "released"].includes(value.phase))) {
      throw new OwnershipRefused("staging_storage_invalid");
    }
    return value;
  }

  async require(identity: StagingIdentity, now = Date.now()): Promise<StagingUse> {
    const use = await this.read();
    if (!use || use.purpose !== undefined || !activeFor(use, identity, now)) {
      throw new OwnershipRefused("staging_use_required");
    }
    return use;
  }

  /** Read the issued record under the ownership gate, including after expiry
   * while recovery still holds the lease. Preparation and retirement are not issuance. */
  async readIssuedAutomodLease(scope: Readonly<Pick<StagingWindow, "runId" | "runAttempt">>, identity: StagingIdentity) {
    await this.sync();
    const use = await this.read();
    if (use?.purpose === AUTOMOD_STAGING_PURPOSE && use.phase === "active" && ownedBy(use, identity)
      && use.runId === scope.runId && use.runAttempt === scope.runAttempt) return Object.freeze(use);
  }

  /** Retain the original issued tuple for recovery after release or expiry.
   * Ownership remains fenced to the same deployment, build and epoch. */
  async automodBinding(expected: Readonly<StagingUse>, identity: StagingIdentity, now: number) {
    const use = await this.requireAutomodLease(expected, identity);
    return { lease: Object.freeze({ ...use, phase: "active" as const }), active: activeFor(use, identity, now) };
  }

  /** The protected caller checks completed cleanup under the same gate first. */
  async releaseAutomod(expected: Readonly<StagingUse>, identity: StagingIdentity): Promise<void> {
    const use = await this.requireAutomodLease(expected, identity);
    if (use.phase !== "released") await this.write({ ...use, phase: "released" });
    await this.sync();
  }

  private async requireAutomodLease(expected: Readonly<StagingUse>, identity: StagingIdentity): Promise<StagingUse> {
    const use = await this.read();
    if (use?.purpose !== AUTOMOD_STAGING_PURPOSE || use.phase === "preparing" || !ownedBy(use, identity)
      || expected?.purpose !== AUTOMOD_STAGING_PURPOSE || expected.environment !== use.environment
      || expected.phase !== "active" || "voiceMove" in expected || !sameWindow(use, expected)
      || expected.deploymentId !== use.deploymentId || expected.sha !== use.sha || expected.epoch !== use.epoch
      || expected.acquiredAt !== use.acquiredAt) throw new OwnershipRefused("staging_use_conflict");
    return use;
  }

  async acquire(change: StagingUseChange, identity: StagingIdentity,
    stop: () => Promise<void>, scheduleExpiry: (expiresAt: number) => Promise<void>, now = Date.now(),
    purpose?: typeof AUTOMOD_STAGING_PURPOSE): Promise<StagingUse> {
    this.checkIdentity(change, identity);
    if (purpose !== undefined && (purpose !== AUTOMOD_STAGING_PURPOSE || "voiceMove" in change)) {
      throw new OwnershipRefused("staging_use_conflict");
    }
    if (change.expiresAt <= now || change.expiresAt - now > MAX_STAGING_USE_MS) {
      throw new OwnershipRefused("staging_expiry_invalid");
    }
    if (purpose !== undefined) await this.sync();
    const old = await this.read();
    if (old?.purpose === AUTOMOD_STAGING_PURPOSE && old.phase !== "released"
      && (purpose !== old.purpose || old.expiresAt <= now)) throw new OwnershipRefused("staging_use_conflict");
    if (old && old.phase !== "released" && old.expiresAt > now) {
      // A single workflow may rebind after a deployment or rollback, but may
      // never extend its original finite window or steal another consumer's.
      if (!sameWindow(old, change)) throw new OwnershipRefused("staging_use_conflict");
      const sameBuild = old.deploymentId === identity.deploymentId && old.epoch === identity.owner.epoch
        && old.sha === identity.sha;
      if (old.purpose !== purpose || (purpose !== undefined && !sameBuild)) {
        throw new OwnershipRefused("staging_use_conflict");
      }
      // An exact retry keeps its selection. A rebind may clear the capability
      // for recovery but may never arm it on a different owner/build.
      if (sameBuild ? old.voiceMove !== change.voiceMove : change.voiceMove === true) {
        throw new OwnershipRefused("staging_use_conflict");
      }
      if (old.phase === "active" && sameBuild) {
        await scheduleExpiry(old.expiresAt);
        return old;
      }
    } else {
      let used: unknown;
      try { used = await this.storage.get(`${USED_PREFIX}${change.runId}:${change.runAttempt}`); }
      catch { throw new OwnershipRefused("staging_storage_unavailable"); }
      if (used !== undefined) throw new OwnershipRefused("staging_use_ended");
    }
    const pending: StagingUse = {
      environment: "staging", runId: change.runId, runAttempt: change.runAttempt, expiresAt: change.expiresAt,
      deploymentId: identity.deploymentId, sha: identity.sha, epoch: identity.owner.epoch,
      acquiredAt: old && sameWindow(old, change) ? old.acquiredAt : now, phase: "preparing",
      ...(change.voiceMove === true ? { voiceMove: true } : {}),
      ...(purpose === undefined ? {} : { purpose }),
    };
    const active = { ...pending, phase: "active" as const };
    const reservation = purpose === undefined ? {} : await new StagingAutomodRecovery(this.storage).reservation(active);
    // Durable denial precedes both teardown and scheduling. A crash cannot
    // publish an active grant lacking its expiry alarm. End markers prevent a
    // delayed acquire retry from resurrecting a released workflow attempt.
    await this.write(pending, { [`${USED_PREFIX}${pending.runId}:${pending.runAttempt}`]: pending.expiresAt });
    await stop();
    await scheduleExpiry(pending.expiresAt);
    if (Date.now() >= pending.expiresAt) throw new OwnershipRefused("staging_expiry_invalid");
    // The active grant and its unused evidence must become durable together.
    await this.write(active, reservation);
    if (purpose !== undefined) await this.sync();
    return active;
  }

  /** Stop under the DO gate without consuming the grant needed by rollback. */
  async stop(change: StagingUseChange, identity: StagingIdentity, stop: () => Promise<void>): Promise<StagingUse> {
    const use = await this.requireTeardown(change, identity, true);
    await stop();
    return use;
  }

  async release(change: StagingUseChange, identity: StagingIdentity, stop: () => Promise<void>): Promise<StagingUse> {
    const use = await this.requireTeardown(change, identity, false);
    const { voiceMove: _voiceMove, ...ended } = use;
    const released = { ...ended, phase: "released" as const };
    // Keep the tombstone even after a confirmed stop. Repeat cleanup is safe;
    // cleanup from an old run can never stop a newer run's container.
    await this.write(released);
    await stop();
    return released;
  }

  async requireTeardown(change: StagingUseChange, identity: StagingIdentity, requireActive: boolean): Promise<StagingUse> {
    this.checkIdentity(change, identity);
    const use = requireActive ? await this.require(identity) : await this.read();
    if (!use || use.purpose !== undefined || !sameWindow(use, change)) throw new OwnershipRefused("staging_use_conflict");
    return use;
  }

  private checkIdentity(change: StagingUseChange, identity: StagingIdentity): void {
    if (change.expectedEpoch !== identity.owner.epoch) throw new OwnershipRefused("epoch_conflict");
    if (change.deploymentId !== identity.deploymentId || change.sha !== identity.sha
      || identity.owner.phase !== "active" || identity.owner.deploymentId !== identity.deploymentId) {
      throw new OwnershipRefused("deployment_mismatch");
    }
  }

  private async write(use: StagingUse, entries: Record<string, unknown> = {}): Promise<void> {
    try {
      await this.storage.put({ ...entries, [STAGING_USE_KEY]: use });
    } catch { throw new OwnershipRefused("staging_storage_unavailable"); }
  }

  private async sync(): Promise<void> {
    try { await this.storage.sync(); }
    catch { throw new OwnershipRefused("staging_storage_unavailable"); }
  }
}
