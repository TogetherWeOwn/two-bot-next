/**
 * Admin control endpoints (TOG-22862): read-only stored config plus
 * default-dark mutating controls, behind a least-privilege control-scope
 * token that is distinct from the ownership and metrics tokens.
 *
 * Routes (all exact path, no query):
 * - GET  /ops/config      stored TWO_* overrides (env baseline plus pending
 *                          overrides; nothing here is applied to the running
 *                          bot yet, see `enforcement` below)
 * - PUT  /ops/config      replace overrides (gated, epoch-checked, audited)
 * - GET  /ops/kill-switch current fences (same auth as config)
 * - POST /ops/kill-switch replace fences (gated, epoch-checked, audited)
 * - GET  /ops/audit       recent audit rows, newest first (same auth)
 *
 * Fences: automation, automod, publish, gateway_park. `true` means fenced
 * (held/disabled/parked); `false` (the default) means active. Enforcement in
 * the Rust runtime is a follow-up after the CISO review (AC4); this module
 * owns the control-plane state machine, its epoch semantics and its audit.
 * Until that follow-up lands, every config and kill-switch response carries
 * `enforcement: "pending"`: stored overrides are not in effect on the
 * running bot, and no response field may claim otherwise.
 *
 * Epoch semantics mirror wrangler/src/ownership.ts: every mutating call posts
 * the epoch it saw, the DO checks it optimistically, persists the new record
 * at old+1, then appends exactly one audit row. A stale epoch is a 409 with
 * reason epoch_conflict; the stored record is unchanged and no audit row is
 * written. Audit keys extend the two-bot:ownership-audit:v1 pattern with the
 * two-bot:ops-audit:v1 prefix; keys are unique per mutation and never
 * overwritten or deleted.
 *
 * Official Durable Object storage patterns (get/put/list with prefix):
 * https://developers.cloudflare.com/durable-objects/api/storage-api/
 * Concurrency gates (all state transitions run inside blockConcurrencyWhile):
 * https://developers.cloudflare.com/durable-objects/api/state/#blockconcurrencywhile
 */

import { FORWARDED_FLAGS, forwardedFlagVars } from "./container-env.ts";
import { authenticated } from "./ownership.ts";
import { TokenBuckets } from "./redirect.ts";
import { withSecurityHeaders } from "./security-headers.ts";

export const OPS_CONFIG_PATH = "/ops/config";
export const OPS_KILLSWITCH_PATH = "/ops/kill-switch";
export const OPS_AUDIT_PATH = "/ops/audit";

export const OPS_CONFIG_KEY = "two-bot:ops-config:v1";
export const OPS_KILLSWITCH_KEY = "two-bot:ops-killswitch:v1";
/** Monotonic audit sequence; zero-padded keys keep prefix listings chronological. */
export const OPS_AUDIT_SEQ_KEY = "two-bot:ops-audit-seq:v1";
/** Append-only audit rows; extends the two-bot:ownership-audit:v1 pattern. */
export const OPS_AUDIT_PREFIX = "two-bot:ops-audit:v1:";
/**
 * Enforcement marker on every config and kill-switch response. Stored
 * overrides are control-plane state only until the Rust runtime reads them
 * (CISO review AC4 follow-up); the marker keeps operators from mistaking a
 * stored override for an applied one.
 */
export const OPS_ENFORCEMENT_PENDING = "pending" as const;

/** Same floor as the ownership control and metrics scrape tokens. */
export const MIN_OPS_TOKEN_LENGTH = 32;
/** Mutating bodies are small JSON; bound independently of Content-Length. */
export const MAX_OPS_BODY_BYTES = 16 * 1024;
/** GET /ops/audit returns at most this many rows, newest first. */
export const MAX_AUDIT_ROWS = 100;

const ACTOR_PATTERN = /^[a-zA-Z0-9_.:@/-]{1,128}$/;
const ALLOWED_FLAGS: ReadonlySet<string> = new Set(FORWARDED_FLAGS);
const FENCE_KEYS = ["automation", "automod", "publish", "gateway_park"] as const;
export type FenceKey = (typeof FENCE_KEYS)[number];
export type Fences = Record<FenceKey, boolean>;

const DEFAULT_FENCES: Fences = {
  automation: false,
  automod: false,
  publish: false,
  gateway_park: false,
};

export interface OpsConfigRecord {
  epoch: number;
  overrides: Record<string, string>;
  actor: string;
  timestamp: string;
  oldEpoch: number;
}

export interface OpsKillswitchRecord {
  epoch: number;
  fences: Fences;
  actor: string;
  timestamp: string;
  oldEpoch: number;
}

export interface OpsAuditRow {
  actor: string;
  action: "config_put" | "kill_switch";
  epoch: number;
  timestamp: string;
  detail: Record<string, unknown>;
}

export interface OpsEnv {
  OPS_CONTROL_TOKEN?: string;
  OPS_CONFIG_WRITES_ENABLED?: string;
  OPS_KILLSWITCH_ENABLED?: string;
}

export class OpsRefused extends Error {
  readonly reason: string;
  readonly status: number;

  constructor(reason: string, status: number) {
    super(`ops control refused: ${reason}`);
    this.reason = reason;
    this.status = status;
  }
}

function validActor(actor: unknown): actor is string {
  return typeof actor === "string" && ACTOR_PATTERN.test(actor);
}

function validEpoch(epoch: unknown): epoch is number {
  return Number.isSafeInteger(epoch) && (epoch as number) >= 0;
}

function validTimestamp(timestamp: unknown): timestamp is string {
  return typeof timestamp === "string" && Number.isFinite(Date.parse(timestamp));
}

function validOverrides(overrides: unknown): overrides is Record<string, string> {
  if (!overrides || typeof overrides !== "object" || Array.isArray(overrides)) return false;
  const entries = Object.entries(overrides as Record<string, unknown>);
  if (entries.length > FORWARDED_FLAGS.length) return false;
  for (const [key, value] of entries) {
    if (!ALLOWED_FLAGS.has(key)) return false;
    if (typeof value !== "string" || value.length > 4096) return false;
    // Printable ASCII plus tab; never a control blob. Empty string is allowed:
    // set-but-empty is not unset to the Rust readers (see container-env.ts).
    if (!/^[\t\x20-\x7e]*$/.test(value)) return false;
  }
  return true;
}

function validFences(fences: unknown): fences is Fences {
  if (!fences || typeof fences !== "object" || Array.isArray(fences)) return false;
  const record = fences as Record<string, unknown>;
  if (Object.keys(record).length !== FENCE_KEYS.length) return false;
  for (const key of FENCE_KEYS) {
    if (typeof record[key] !== "boolean") return false;
  }
  return true;
}

function validConfigRecord(value: unknown): value is OpsConfigRecord {
  if (!value || typeof value !== "object") return false;
  const v = value as Record<string, unknown>;
  return validEpoch(v.epoch) && (v.epoch as number) > 0 &&
    validEpoch(v.oldEpoch) && (v.oldEpoch as number) === (v.epoch as number) - 1 &&
    validOverrides(v.overrides) && validActor(v.actor) && validTimestamp(v.timestamp);
}

function validKillswitchRecord(value: unknown): value is OpsKillswitchRecord {
  if (!value || typeof value !== "object") return false;
  const v = value as Record<string, unknown>;
  return validEpoch(v.epoch) && (v.epoch as number) > 0 &&
    validEpoch(v.oldEpoch) && (v.oldEpoch as number) === (v.epoch as number) - 1 &&
    validFences(v.fences) && validActor(v.actor) && validTimestamp(v.timestamp);
}

export interface ConfigPut {
  overrides: Record<string, string>;
  expectedEpoch: number;
  actor: string;
}

export interface KillswitchPost {
  fences: Fences;
  expectedEpoch: number;
  actor: string;
}

export function parseConfigPut(value: unknown): ConfigPut | null {
  if (!value || typeof value !== "object") return null;
  const v = value as Record<string, unknown>;
  if (!validOverrides(v.overrides) || !validEpoch(v.expectedEpoch) || !validActor(v.actor)) {
    return null;
  }
  return {
    overrides: v.overrides as Record<string, string>,
    expectedEpoch: v.expectedEpoch as number,
    actor: v.actor as string,
  };
}

export function parseKillswitchPost(value: unknown): KillswitchPost | null {
  if (!value || typeof value !== "object") return null;
  const v = value as Record<string, unknown>;
  if (!validFences(v.fences) || !validEpoch(v.expectedEpoch) || !validActor(v.actor)) {
    return null;
  }
  return {
    fences: v.fences as Fences,
    expectedEpoch: v.expectedEpoch as number,
    actor: v.actor as string,
  };
}

/** Whole-body deadline mirroring the receiver convention (internal-actions BODY_TIMEOUT_MS). */
export const OPS_BODY_TIMEOUT_MS = 5_000;

/** Bound control input independently of the untrusted Content-Length header. */
export async function readOpsBody(request: Request): Promise<unknown> {
  const reader = request.body?.getReader();
  if (!reader) return null;
  const chunks: Uint8Array[] = [];
  let size = 0;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<"timeout">((resolve) => {
    timer = setTimeout(() => resolve("timeout"), OPS_BODY_TIMEOUT_MS);
  });
  try {
    for (;;) {
      const step = await Promise.race([reader.read(), timeout]);
      if (step === "timeout") {
        void reader.cancel().catch(() => {});
        return null;
      }
      if (step.done) break;
      size += step.value.byteLength;
      if (size > MAX_OPS_BODY_BYTES) {
        void reader.cancel().catch(() => {});
        return null;
      }
      chunks.push(step.value);
    }
    const bytes = new Uint8Array(size);
    let offset = 0;
    for (const chunk of chunks) {
      bytes.set(chunk, offset);
      offset += chunk.length;
    }
    return JSON.parse(new TextDecoder().decode(bytes));
  } catch {
    return null;
  } finally {
    clearTimeout(timer);
  }
}

export function configWritesEnabled(env: OpsEnv): boolean {
  return env.OPS_CONFIG_WRITES_ENABLED === "1";
}

export function killswitchEnabled(env: OpsEnv): boolean {
  return env.OPS_KILLSWITCH_ENABLED === "1";
}

export function opsTokenConfigured(env: OpsEnv): boolean {
  return !!env.OPS_CONTROL_TOKEN && env.OPS_CONTROL_TOKEN.length >= MIN_OPS_TOKEN_LENGTH;
}

// Per-isolate attempt cap for the ops control token, mirroring the metrics
// scrape buckets: every attempt takes one token synchronously before the
// digest comparison, so concurrent guesses cannot share one token.
const opsAuthBuckets = new TokenBuckets({ capacity: 10, refillPerSecond: 1 });
let opsShortTokenLogged = false;

/** Exact path, no query: anything else is not this surface. */
export function isOpsPath(request: Request, path: string): boolean {
  if (request.url.includes("?")) return false;
  const url = new URL(request.url);
  return url.pathname === path && url.search === "";
}

function json(status: number, value: unknown, allow?: string): Response {
  const headers = withSecurityHeaders({ "cache-control": "no-store" }) as Record<string, string>;
  if (allow) headers["allow"] = allow;
  return Response.json(value, { status, headers });
}

function text(status: number, body: string, allow?: string): Response {
  const headers = withSecurityHeaders({
    "content-type": "text/plain",
    "cache-control": "no-store",
    ...(allow ? { allow } : {}),
  }) as Record<string, string>;
  return new Response(body, { status, headers });
}

/**
 * Worker half of the ops control gate. Returns a response when the request is
 * refused or is dark; returns null when the caller is authenticated and the
 * request must be forwarded to the fenced DO (with the deployment header
 * stamped by the caller, as with the ownership and metrics paths).
 */
export async function gateOpsRequest(request: Request, env: OpsEnv): Promise<Response | null> {
  const url = new URL(request.url);
  const path = url.pathname;
  const isConfig = path === OPS_CONFIG_PATH;
  const isKillswitch = path === OPS_KILLSWITCH_PATH;
  const isAudit = path === OPS_AUDIT_PATH;
  if (!isConfig && !isKillswitch && !isAudit) return null;
  if (request.url.includes("?") || url.search !== "") return text(404, "not found\n");
  if (!opsTokenConfigured(env)) {
    if (env.OPS_CONTROL_TOKEN && env.OPS_CONTROL_TOKEN.length < MIN_OPS_TOKEN_LENGTH && !opsShortTokenLogged) {
      opsShortTokenLogged = true;
      console.error(JSON.stringify({
        event: "ops_control_token_misconfigured",
        reason: "token_below_minimum_length",
        minimum_length: MIN_OPS_TOKEN_LENGTH,
      }));
    }
    return text(404, "not found\n");
  }
  const allowedMethods =
    isConfig ? "GET, PUT" : isKillswitch ? "GET, POST" : "GET";
  if (!allowedMethods.split(", ").includes(request.method)) {
    return text(405, "method not allowed\n", allowedMethods);
  }
  if (isConfig && request.method === "PUT" && !configWritesEnabled(env)) {
    return text(404, "not found\n");
  }
  if (isKillswitch && request.method === "POST" && !killswitchEnabled(env)) {
    return text(404, "not found\n");
  }
  const caller = request.headers.get("cf-connecting-ip") ?? "unknown";
  const verdict = opsAuthBuckets.take(caller);
  if (!verdict.allowed && !opsAuthBuckets.peek(caller).allowed) {
    const headers = withSecurityHeaders({
      "content-type": "text/plain",
      "cache-control": "no-store",
      "retry-after": String(verdict.retryAfter),
    }) as Record<string, string>;
    return new Response("slow down\n", { status: 429, headers });
  }
  if (!await authenticated(request, env.OPS_CONTROL_TOKEN)) {
    return text(401, "unauthorized\n");
  }
  return null;
}

/** Test seam: forget per-isolate ops admission state. */
export function resetOpsLimits(): void {
  opsShortTokenLogged = false;
}

/**
 * Durable-Object half: epoch-checked state plus append-only audit, called
 * inside the DO's blockConcurrencyWhile gate by the container class.
 */
export class OpsControl {
  private readonly storage: DurableObjectStorage;
  private readonly env: Record<string, unknown>;

  constructor(storage: DurableObjectStorage, env: Record<string, unknown>) {
    this.storage = storage;
    this.env = env;
  }

  async readConfig(): Promise<{
    epoch: number;
    overrides: Record<string, string>;
    baseline: Record<string, string>;
    enforcement: typeof OPS_ENFORCEMENT_PENDING;
  }> {
    const stored = await this.getConfig();
    const overrides = stored?.overrides ?? {};
    return {
      epoch: stored?.epoch ?? 0,
      overrides,
      baseline: this.baseline(),
      enforcement: OPS_ENFORCEMENT_PENDING,
    };
  }

  async putConfig(put: ConfigPut): Promise<OpsConfigRecord> {
    const old = await this.getConfig();
    const oldEpoch = old?.epoch ?? 0;
    if (oldEpoch !== put.expectedEpoch) throw new OpsRefused("epoch_conflict", 409);
    if (oldEpoch === Number.MAX_SAFE_INTEGER) throw new OpsRefused("epoch_exhausted", 503);
    const record: OpsConfigRecord = {
      epoch: oldEpoch + 1,
      overrides: { ...put.overrides },
      actor: put.actor,
      timestamp: new Date().toISOString(),
      oldEpoch,
    };
    await this.putAtomic(OPS_CONFIG_KEY, record, {
      actor: put.actor,
      action: "config_put",
      epoch: record.epoch,
      timestamp: record.timestamp,
      detail: { overrides: record.overrides },
    });
    return record;
  }

  async readKillswitch(): Promise<OpsKillswitchRecord & { epoch: number }> {
    const stored = await this.getKillswitch();
    if (stored) return stored;
    return {
      epoch: 0,
      fences: { ...DEFAULT_FENCES },
      actor: "",
      timestamp: new Date(0).toISOString(),
      oldEpoch: 0,
    };
  }

  async postKillswitch(post: KillswitchPost): Promise<OpsKillswitchRecord> {
    const old = await this.getKillswitch();
    const oldEpoch = old?.epoch ?? 0;
    if (oldEpoch !== post.expectedEpoch) throw new OpsRefused("epoch_conflict", 409);
    if (oldEpoch === Number.MAX_SAFE_INTEGER) throw new OpsRefused("epoch_exhausted", 503);
    const record: OpsKillswitchRecord = {
      epoch: oldEpoch + 1,
      fences: { ...post.fences },
      actor: post.actor,
      timestamp: new Date().toISOString(),
      oldEpoch,
    };
    await this.putAtomic(OPS_KILLSWITCH_KEY, record, {
      actor: post.actor,
      action: "kill_switch",
      epoch: record.epoch,
      timestamp: record.timestamp,
      detail: { fences: record.fences },
    });
    return record;
  }

  async listAudit(limit = MAX_AUDIT_ROWS): Promise<OpsAuditRow[]> {
    const bound = Number.isSafeInteger(limit) && limit > 0 ? Math.min(limit, MAX_AUDIT_ROWS) : MAX_AUDIT_ROWS;
    const rows = await this.storage.list<OpsAuditRow>({ prefix: OPS_AUDIT_PREFIX, limit: bound, reverse: true });
    return [...rows.values()];
  }

  private baseline(): Record<string, string> {
    return { ...forwardedFlagVars(this.env) };
  }

  private async getConfig(): Promise<OpsConfigRecord | undefined> {
    let value: OpsConfigRecord | undefined;
    try {
      value = await this.storage.get<OpsConfigRecord>(OPS_CONFIG_KEY);
    } catch {
      throw new OpsRefused("storage_unavailable", 503);
    }
    if (value !== undefined && !validConfigRecord(value)) {
      throw new OpsRefused("storage_invalid", 503);
    }
    return value;
  }

  private async getKillswitch(): Promise<OpsKillswitchRecord | undefined> {
    let value: OpsKillswitchRecord | undefined;
    try {
      value = await this.storage.get<OpsKillswitchRecord>(OPS_KILLSWITCH_KEY);
    } catch {
      throw new OpsRefused("storage_unavailable", 503);
    }
    if (value !== undefined && !validKillswitchRecord(value)) {
      throw new OpsRefused("storage_invalid", 503);
    }
    return value;
  }

  private async putAtomic(key: string, record: OpsConfigRecord | OpsKillswitchRecord, audit: OpsAuditRow): Promise<void> {
    // The sequence counter makes audit keys unique per mutation and keeps
    // prefix listings chronological even when two mutations share a
    // millisecond. Record, counter and audit row commit atomically; audit keys
    // are never reused, overwritten or deleted: the trail is append-only.
    let seq: number;
    try {
      const stored = await this.storage.get<number>(OPS_AUDIT_SEQ_KEY);
      if (stored !== undefined && (!Number.isSafeInteger(stored) || stored < 0)) {
        throw new OpsRefused("storage_invalid", 503);
      }
      seq = (stored ?? 0) + 1;
      if (seq > Number.MAX_SAFE_INTEGER) throw new OpsRefused("epoch_exhausted", 503);
      await this.storage.put({
        [key]: record,
        [OPS_AUDIT_SEQ_KEY]: seq,
        [`${OPS_AUDIT_PREFIX}${String(seq).padStart(16, "0")}`]: audit,
      });
    } catch (error) {
      if (error instanceof OpsRefused) throw error;
      throw new OpsRefused("storage_unavailable", 503);
    }
  }
}

export function opsRefused(error: unknown): Response {
  if (error instanceof OpsRefused) {
    if (error.reason === "epoch_conflict") {
      console.warn(JSON.stringify({ event: "ops_control_refused", reason: error.reason }));
      return json(409, { error: "ops_conflict", reason: error.reason });
    }
    console.warn(JSON.stringify({ event: "ops_control_refused", reason: error.reason }));
    return json(error.status, { error: "ops_unavailable", reason: error.reason });
  }
  return json(503, { error: "ops_unavailable", reason: "operation_failed" });
}

export { json as opsJson };
