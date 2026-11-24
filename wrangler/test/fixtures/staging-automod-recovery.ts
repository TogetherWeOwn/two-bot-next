import assert from "node:assert/strict";
import {
  type AutomodCleanup, type AutomodIntent, type AutomodOwnedCommand, type CommandRevision, STAGING_AUTOMOD_TARGET as target,
} from "../../src/staging-automod-recovery.ts";

export const lease = {
  environment: "staging", phase: "active", deploymentId: "fixture", sha: "a".repeat(40),
  epoch: 3, runId: "12345", runAttempt: 2, acquiredAt: 1_000, expiresAt: 60_000,
} as const;
export const admission = { lease, baselineAbsent: true, publishBefore: 55_000 } as const;
export const intent = { lease, target, baseline: "b".repeat(64), publishBefore: admission.publishBefore };
export const created = { id: "1000000000000000000", version: "1000000000000000001" };
export const other = { id: "1000000000000000002", version: "1000000000000000003" };
export const now = () => 2_000;
export const observe = async () => admission;

export class Storage {
  value: unknown;
  staged: unknown;
  beforeSync?: () => void;
  afterWrite?: () => void;

  async get<T>(_key: string): Promise<T | undefined> {
    return structuredClone(this.value) as T | undefined;
  }

  async put<T>(_key: string, value: T): Promise<void> {
    this.staged = structuredClone(value);
    this.afterWrite?.();
  }

  async sync(): Promise<void> {
    this.beforeSync?.();
    if (this.staged !== undefined) {
      this.value = this.staged;
      this.staged = undefined;
    }
  }
}

// Only the fake knows which command this publication created and whether its
// remote outcome is settled. It never attributes ownership by name alone.
export class Registry implements AutomodCleanup {
  calls: string[] = [];
  target: CommandRevision | undefined;
  otherGuild = new Map([[target.name, other]]);
  sameGuild = new Map([["help", other]]);
  attributable = true;
  authorized = true;
  stopped = true;
  drained = true;
  keepAfterDelete = false;
  loseDeleteResponse = false;
  proofIntent = intent as AutomodIntent;
  beforeDelete?: (command: CommandRevision) => void;

  async inspect(received: AutomodIntent, known: CommandRevision | null) {
    assert.deepEqual(received, intent);
    this.calls.push(known?.id ?? "unknown");
    if (this.target && !known && !this.attributable) throw new Error("ambiguous create");
    const proof = {
      intent: this.proofIntent,
      authorized: this.authorized as true,
      stopped: this.stopped as true,
      publicationDrained: this.drained as true,
    };
    return this.target
      ? { ...proof, registry: "owned" as const, command: this.target }
      : { ...proof, registry: "absent" as const };
  }

  async removeOwned({ intent: received, id, version }: AutomodOwnedCommand) {
    assert.deepEqual(received, intent);
    const command = { id, version };
    this.beforeDelete?.(command);
    assert.deepEqual(command, this.target);
    this.calls.push("delete");
    if (!this.keepAfterDelete) this.target = undefined;
    if (this.loseDeleteResponse) throw new Error("lost delete response");
  }
}
