/** Persisted singleton ownership. Missing/corrupt/unreadable state never grants ownership. */
export const OWNER_KEY = "two-bot:owner:v1";
export const AUDIT_PREFIX = "two-bot:ownership-audit:v1:";
export const DEPLOYMENT_HEADER = "x-two-bot-deployment-id";
export const CONTROL_PATH = "/internal/ownership";

export interface OwnerRecord {
  deploymentId: string | null;
  epoch: number;
  phase: "active" | "fenced";
  actor: string;
  timestamp: string;
  oldEpoch: number;
  oldDeploymentId: string | null;
}

export interface OwnershipChange {
  action: "takeover" | "fence";
  expectedEpoch: number;
  actor: string;
}

export class OwnershipRefused extends Error {
  readonly reason: string;

  constructor(reason: string) {
    super(`ownership refused: ${reason}`);
    this.reason = reason;
  }
}

function validId(id: unknown): id is string {
  return typeof id === "string" && /^[a-zA-Z0-9_-]{1,128}$/.test(id);
}

export function deploymentId(id: unknown): string {
  if (!validId(id)) throw new OwnershipRefused("deployment_id_missing");
  return id;
}

export function parseChange(value: unknown): OwnershipChange | null {
  if (!value || typeof value !== "object") return null;
  const v = value as Record<string, unknown>;
  if (v.action !== "takeover" && v.action !== "fence") return null;
  if (!Number.isSafeInteger(v.expectedEpoch) || (v.expectedEpoch as number) < 0) return null;
  // Actor is the caller's audit label; authentication is the control token, not this label.
  if (typeof v.actor !== "string" || !/^[a-zA-Z0-9_.:@/-]{1,128}$/.test(v.actor)) return null;
  return { action: v.action, expectedEpoch: v.expectedEpoch as number, actor: v.actor };
}

function validRecord(v: OwnerRecord): boolean {
  return Number.isSafeInteger(v.epoch) && v.epoch > 0 &&
    Number.isSafeInteger(v.oldEpoch) && v.oldEpoch === v.epoch - 1 &&
    (v.phase === "active" || v.phase === "fenced") &&
    (v.deploymentId === null || validId(v.deploymentId)) &&
    (v.phase !== "active" || v.deploymentId !== null) &&
    (v.oldDeploymentId === null || validId(v.oldDeploymentId)) &&
    typeof v.actor === "string" && /^[a-zA-Z0-9_.:@/-]{1,128}$/.test(v.actor) &&
    typeof v.timestamp === "string" && Number.isFinite(Date.parse(v.timestamp));
}

/**
 * Call inside a DO concurrency gate, including the entire admitted operation.
 * Storage input gates alone don't serialize container I/O with a takeover.
 * https://developers.cloudflare.com/durable-objects/api/state/#blockconcurrencywhile
 */
export class OwnershipFence {
  private readonly storage: DurableObjectStorage;

  constructor(storage: DurableObjectStorage) { this.storage = storage; }

  async read(): Promise<OwnerRecord | undefined> {
    let value: OwnerRecord | undefined;
    try {
      value = await this.storage.get<OwnerRecord>(OWNER_KEY);
    } catch {
      throw new OwnershipRefused("storage_unavailable");
    }
    if (value !== undefined && (!value || !validRecord(value))) {
      throw new OwnershipRefused("storage_invalid");
    }
    return value;
  }

  async require(id: string, callerId = id): Promise<OwnerRecord> {
    deploymentId(id);
    const owner = await this.read();
    if (!owner || owner.phase !== "active" || owner.deploymentId !== id || callerId !== id) {
      throw new OwnershipRefused("not_owner");
    }
    return owner;
  }

  /**
   * Persist revocation BEFORE destroying the old process. Only a confirmed
   * teardown permits release. A crash/write/stop failure leaves durable denial.
   * Record and audit are written atomically, with an optimistic epoch check.
   */
  async change(id: string, change: OwnershipChange, destroy: () => Promise<void>): Promise<OwnerRecord> {
    deploymentId(id);
    const old = await this.read();
    const oldEpoch = old?.epoch ?? 0;
    if (oldEpoch !== change.expectedEpoch) throw new OwnershipRefused("epoch_conflict");
    if (oldEpoch === Number.MAX_SAFE_INTEGER) throw new OwnershipRefused("epoch_exhausted");
    const pending: OwnerRecord = {
      deploymentId: change.action === "takeover" ? id : null,
      epoch: oldEpoch + 1,
      phase: "fenced",
      actor: change.actor,
      timestamp: new Date().toISOString(),
      oldEpoch,
      oldDeploymentId: old?.deploymentId ?? null,
    };
    await this.write(pending);
    await destroy();
    if (change.action === "fence") return pending;
    const active: OwnerRecord = { ...pending, phase: "active" };
    await this.write(active);
    return active;
  }

  private async write(record: OwnerRecord): Promise<void> {
    try {
      // KV multi-put is atomic; both the current record and its epoch receipt
      // share the DO's storage output gate. Never clear SDK alarms/storage.
      await this.storage.put({
        [OWNER_KEY]: record,
        [`${AUDIT_PREFIX}${record.epoch}:${record.phase}`]: record,
      });
    } catch {
      throw new OwnershipRefused("storage_unavailable");
    }
  }
}

/** Bound control input independently of the untrusted Content-Length header. */
export async function readChange(request: Request): Promise<OwnershipChange | null> {
  const reader = request.body?.getReader();
  if (!reader) return null;
  const chunks: Uint8Array[] = [];
  let size = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      size += value.byteLength;
      if (size > 1024) {
        await reader.cancel();
        return null;
      }
      chunks.push(value);
    }
    const bytes = new Uint8Array(size);
    let offset = 0;
    for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.length; }
    return parseChange(JSON.parse(new TextDecoder().decode(bytes)));
  } catch {
    return null;
  }
}

/** Fixed-size digest comparison; never log supplied or configured tokens. */
export async function authenticated(request: Request, token: string | undefined): Promise<boolean> {
  if (!token || token.length < 32) return false;
  const header = request.headers.get("authorization");
  if (!header?.startsWith("Bearer ") || header.length > 4096) return false;
  const encode = (value: string) => new TextEncoder().encode(value);
  const [expected, supplied] = await Promise.all([
    crypto.subtle.digest("SHA-256", encode(token)),
    crypto.subtle.digest("SHA-256", encode(header.slice(7))),
  ]);
  const a = new Uint8Array(expected);
  const b = new Uint8Array(supplied);
  let difference = 0;
  for (let i = 0; i < a.length; i++) difference |= a[i]! ^ b[i]!;
  return difference === 0;
}

export function refused(error: unknown): Response {
  const reason = error instanceof OwnershipRefused ? error.reason : "operation_failed";
  console.warn(`two-bot ownership refused: ${reason}`);
  return Response.json({ error: "ownership_fenced", reason }, {
    status: reason === "epoch_conflict" ? 409 : 503,
    headers: { "cache-control": "no-store" },
  });
}
