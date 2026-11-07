import { OwnershipRefused, type OwnerRecord } from "./ownership.ts";

export const STAGING_USE_PATH = "/internal/staging-use";
export const STAGING_USE_KEY = "two-bot:staging-use:v1";
const USED_PREFIX = "two-bot:staging-use-ended:v1:";
// Allows an explicitly authorized four-ACTIVE-hour soak plus startup/cleanup;
// routine synthetics and deployment callers must request much shorter windows.
export const MAX_STAGING_USE_MS = 6 * 60 * 60 * 1000;

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
}

export interface StagingIdentity {
  deploymentId: string;
  sha: string;
  owner: OwnerRecord;
}

export interface StagingUseChange extends StagingWindow {
  action: "acquire" | "release";
  expectedEpoch: number;
  deploymentId: string;
  sha: string;
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
  if ((v.action !== "acquire" && v.action !== "release")
    || !validIdentity({ ...v, epoch: v.expectedEpoch })) return null;
  return {
    environment: "staging", action: v.action, runId: v.runId, runAttempt: v.runAttempt,
    expiresAt: v.expiresAt, expectedEpoch: v.expectedEpoch, deploymentId: v.deploymentId, sha: v.sha,
  };
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
      || !["preparing", "active", "released"].includes(value.phase))) {
      throw new OwnershipRefused("staging_storage_invalid");
    }
    return value;
  }

  async require(identity: StagingIdentity, now = Date.now()): Promise<StagingUse> {
    const use = await this.read();
    if (!use || use.phase !== "active" || now < use.acquiredAt || now >= use.expiresAt
      || use.deploymentId !== identity.deploymentId || use.sha !== identity.sha
      || use.epoch !== identity.owner.epoch || identity.owner.phase !== "active"
      || identity.owner.deploymentId !== identity.deploymentId) {
      throw new OwnershipRefused("staging_use_required");
    }
    return use;
  }

  async acquire(change: StagingUseChange, identity: StagingIdentity,
    stop: () => Promise<void>, scheduleExpiry: (expiresAt: number) => Promise<void>, now = Date.now()): Promise<StagingUse> {
    this.checkIdentity(change, identity);
    if (change.expiresAt <= now || change.expiresAt - now > MAX_STAGING_USE_MS) {
      throw new OwnershipRefused("staging_expiry_invalid");
    }
    const old = await this.read();
    if (old && old.phase !== "released" && old.expiresAt > now) {
      // A single workflow may rebind after a deployment or rollback, but may
      // never extend its original finite window or steal another consumer's.
      if (!sameWindow(old, change)) throw new OwnershipRefused("staging_use_conflict");
      if (old.phase === "active" && old.deploymentId === identity.deploymentId && old.epoch === identity.owner.epoch
        && old.sha === identity.sha) {
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
    };
    // Durable denial precedes both teardown and scheduling. A crash cannot
    // publish an active grant lacking its expiry alarm. End markers prevent a
    // delayed acquire retry from resurrecting a released workflow attempt.
    await this.write(pending, true);
    await stop();
    await scheduleExpiry(pending.expiresAt);
    if (Date.now() >= pending.expiresAt) throw new OwnershipRefused("staging_expiry_invalid");
    const active = { ...pending, phase: "active" as const };
    await this.write(active);
    return active;
  }

  async release(change: StagingUseChange, identity: StagingIdentity, stop: () => Promise<void>): Promise<StagingUse> {
    this.checkIdentity(change, identity);
    const use = await this.read();
    if (!use || !sameWindow(use, change)) throw new OwnershipRefused("staging_use_conflict");
    const released = { ...use, phase: "released" as const };
    // Keep the tombstone even after a confirmed stop. Repeat cleanup is safe;
    // cleanup from an old run can never stop a newer run's container.
    await this.write(released);
    await stop();
    return released;
  }

  private checkIdentity(change: StagingUseChange, identity: StagingIdentity): void {
    if (change.expectedEpoch !== identity.owner.epoch) throw new OwnershipRefused("epoch_conflict");
    if (change.deploymentId !== identity.deploymentId || change.sha !== identity.sha
      || identity.owner.phase !== "active" || identity.owner.deploymentId !== identity.deploymentId) {
      throw new OwnershipRefused("deployment_mismatch");
    }
  }

  private async write(use: StagingUse, markUsed = false): Promise<void> {
    try {
      await this.storage.put({
        [STAGING_USE_KEY]: use,
        ...(markUsed ? { [`${USED_PREFIX}${use.runId}:${use.runAttempt}`]: use.expiresAt } : {}),
      });
    } catch { throw new OwnershipRefused("staging_storage_unavailable"); }
  }
}
