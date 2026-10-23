// Closed transport prove-out. No verb is exposed until gateway audit exists.
import { TokenBuckets } from "./redirect.ts";
import { ensureRequestId } from "./request-id.ts";
import { withSecurityHeaders } from "./security-headers.ts";
import { ACTIONS_PATH, envelope, relayReceiverResponse } from "./internal-actions.ts";
import type { McpControlScope } from "./mcp-control-binding.ts";

export interface AdminGatewayEnv {
  ADMIN_GATEWAY_SERVICE_KEY_ID?: string;
  ADMIN_GATEWAY_SERVICE_KEY?: string;
}

const MAX_BYTES = 64 * 1024;
const TIMEOUT_MS = 6000;
const callers = new TokenBuckets({ capacity: 20, refillPerSecond: 1 });
let inFlight = 0;
let unauthorizedCount = 0;
let throttledCount = 0;
const encoder = new TextEncoder();

export function adminGatewayEnabled(env: AdminGatewayEnv): boolean {
  return /^[A-Za-z0-9_-]{1,114}-admin-service$/.test(env.ADMIN_GATEWAY_SERVICE_KEY_ID ?? "")
    && typeof env.ADMIN_GATEWAY_SERVICE_KEY === "string"
    && encoder.encode(env.ADMIN_GATEWAY_SERVICE_KEY).byteLength >= 32
    && env.ADMIN_GATEWAY_SERVICE_KEY.trim() === env.ADMIN_GATEWAY_SERVICE_KEY
    && !env.ADMIN_GATEWAY_SERVICE_KEY.includes(",");
}

/** Best-effort per-isolate counters, no credential/caller/path label or log. */
export function adminAuthMetrics(): string {
  return "# TYPE two_bot_admin_gateway_unauthorized_total counter\n"
    + `two_bot_admin_gateway_unauthorized_total ${unauthorizedCount}\n`
    + "# TYPE two_bot_admin_gateway_throttled_total counter\n"
    + `two_bot_admin_gateway_throttled_total ${throttledCount}\n`;
}

function unauthorized(): Response {
  unauthorizedCount++;
  return new Response("unauthorized", {
    status: 401,
    headers: withSecurityHeaders({ "www-authenticate": "Bearer", "cache-control": "no-store" }),
  });
}

function throttled(seconds: number): Response {
  throttledCount++;
  return new Response("slow down\n", {
    status: 429,
    headers: withSecurityHeaders({ "retry-after": String(seconds), "cache-control": "no-store" }),
  });
}

/** Mirrors the code-owned container scope map; never a caller scope override. */
export function adminScopeForAction(action: string): McpControlScope {
  switch (action) {
    case "moderation.warn": case "moderation.timeout": case "moderation.kick":
    case "moderation.purge": case "moderation.slowmode": case "guild.add_member":
    case "event.upsert": case "event.cancel": return "moderate";
    case "moderation.ban": case "moderation.tempban": case "moderation.lockdown":
    case "moderation.unlock": case "role.assign": case "announcement.post":
    case "settings.set": case "automations.import": return "admin";
    // Unknown actions reach only the container's closed allowlist refusal.
    default: return "read";
  }
}

function hex(bytes: ArrayBuffer | Uint8Array): string {
  return Array.from(new Uint8Array(bytes), (b) => b.toString(16).padStart(2, "0")).join("");
}

export async function signAdminRequest(
  secret: string,
  timestamp: string,
  nonce: string,
  bytes: Uint8Array,
  scope: string,
  guild: string,
): Promise<string> {
  const digest = hex(await crypto.subtle.digest("SHA-256", bytes));
  const canonical = ["two-internal-action/scope-claim/v1", "POST", ACTIONS_PATH,
    timestamp, nonce, digest, scope, guild].join("\n");
  const key = await crypto.subtle.importKey("raw", encoder.encode(secret),
    { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
  return "sha256=" + hex(await crypto.subtle.sign("HMAC", key, encoder.encode(canonical)));
}

async function bounded<T>(work: Promise<T>, signal: AbortSignal): Promise<T> {
  if (signal.aborted) throw new Error("admin deadline");
  let abort = () => {};
  const timeout = new Promise<never>((_, reject) => {
    abort = () => reject(new Error("admin deadline"));
    signal.addEventListener("abort", abort, { once: true });
  });
  try { return await Promise.race([work, timeout]); }
  finally { signal.removeEventListener("abort", abort); }
}

async function readParams(request: Request, signal: AbortSignal): Promise<Record<string, unknown>> {
  const declared = request.headers.get("content-length");
  if (request.headers.has("content-encoding")
    || !/^application\/json(?:; charset=utf-8)?$/i.test(request.headers.get("content-type") ?? "")
    || (declared !== null && (!/^\d{1,10}$/.test(declared) || Number(declared) > MAX_BYTES))) {
    throw new Error("admin body refused");
  }
  const reader = request.body?.getReader();
  if (!reader) throw new Error("admin body refused");
  const chunks: Uint8Array[] = [];
  let size = 0;
  try {
    for (;;) {
      const step = await bounded(reader.read(), signal);
      if (step.done) break;
      size += step.value.byteLength;
      if (size > MAX_BYTES) throw new Error("admin body refused");
      chunks.push(step.value);
    }
  } finally {
    void reader.cancel().catch(() => {});
    reader.releaseLock();
  }
  const bytes = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.byteLength; }
  const value: unknown = JSON.parse(new TextDecoder("utf-8", { fatal: true, ignoreBOM: false }).decode(bytes));
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("admin body refused");
  return value as Record<string, unknown>;
}

export async function handleAdminGateway(
  request: Request,
  env: AdminGatewayEnv,
  forward: (request: Request) => Promise<Response>,
): Promise<Response> {
  const url = new URL(request.url);
  const requestId = ensureRequestId(request.headers.get("x-request-id"));
  if (url.search !== "" || request.url.includes("?")) {
    return new Response("not found", { status: 404, headers: withSecurityHeaders() });
  }
  if (request.method !== "GET" && request.method !== "POST") {
    return new Response("method not allowed", {
      status: 405, headers: withSecurityHeaders({ allow: "GET, POST" }),
    });
  }
  const verdict = callers.take(request.headers.get("cf-connecting-ip") ?? "unknown");
  if (!verdict.allowed) return throttled(verdict.retryAfter);
  const auth = /^Bearer (adm_[0-9a-f]{16}\.[A-Za-z0-9_-]{32,})$/.exec(request.headers.get("authorization") ?? "");
  if (!auth || auth[1]!.length > 4096) return unauthorized();
  if (inFlight >= 8) return throttled(1);
  inFlight++;
  const signal = AbortSignal.timeout(TIMEOUT_MS);
  let pending: Promise<Response> | undefined;
  let fetched = false;
  let forwarding = false;
  try {
    const path = /^\/admin\/v1\/([a-zA-Z0-9_.-]{1,96})(?:\/(\d{1,20}))?$/.exec(url.pathname);
    if (!path) return envelope(404, "not_found", false, {}, requestId);
    const input = request.method === "POST" ? await readParams(request, signal) : {};
    // Project only guild/params. Caller token/holder/scope fields and signing
    // headers are never copied; bearer proof comes solely from Authorization.
    if (request.method === "POST" && path[2] !== undefined) {
      return envelope(400, "malformed", false, {}, requestId);
    }
    const guild = request.method === "GET" ? path[2] : input["guild_id"];
    if (typeof guild !== "string" || !/^[1-9]\d{0,18}$/.test(guild)
      || BigInt(guild) > 9223372036854775807n) return envelope(400, "malformed", false, {}, requestId);
    const action = path[1]!;
    const scope = adminScopeForAction(action);
    const bytes = encoder.encode(JSON.stringify({ action, guild_id: guild, params: input["params"] ?? {}, admin_bearer: auth[1] }));
    if (bytes.byteLength > MAX_BYTES) return envelope(413, "malformed", false, {}, requestId);
    const timestamp = String(Math.floor(Date.now() / 1000));
    const nonce = hex(crypto.getRandomValues(new Uint8Array(16)));
    forwarding = true;
    const signature = await bounded(signAdminRequest(env.ADMIN_GATEWAY_SERVICE_KEY!, timestamp, nonce, bytes, scope, guild), signal);
    const headers = new Headers({ "content-type": "application/json", "x-request-id": requestId,
      "x-two-key-id": env.ADMIN_GATEWAY_SERVICE_KEY_ID!, "x-two-timestamp": timestamp,
      "x-two-nonce": nonce, "x-two-signature": signature, "x-two-scope": scope, "x-two-guild": guild });
    const idempotency = request.headers.get("idempotency-key");
    if (idempotency !== null) {
      if (!/^[\x21-\x2b\x2d-\x7e]{1,200}$/.test(idempotency)) return envelope(400, "malformed", false, {}, requestId);
      headers.set("idempotency-key", idempotency);
    }
    pending = forward(new Request(`https://worker.invalid${ACTIONS_PATH}`, { method: "POST", headers, body: bytes, signal }));
    const upstream = await bounded(pending, signal);
    fetched = true;
    const response = await relayReceiverResponse(upstream, requestId, signal);
    if (response.status === 401) return unauthorized();
    // Closed prove-out cannot accidentally expose a future receiver success.
    if (response.ok) return envelope(403, "action_not_allowed", false, {}, requestId);
    return response;
  } catch {
    if (signal.aborted) return envelope(504, "upstream_timeout", true, {}, requestId);
    return forwarding ? envelope(503, "unavailable", true, {}, requestId)
      : envelope(400, "malformed", false, {}, requestId);
  } finally {
    if (pending && !fetched) {
      // Retain admission until an uncooperative forward settles, release its
      // late body, and never retry. Observe rejection even after timeout.
      void pending.then((response) => {
        inFlight--;
        try { void response.body?.cancel().catch(() => {}); } catch { /* best effort */ }
      }, () => { inFlight--; });
    } else inFlight--;
  }
}
