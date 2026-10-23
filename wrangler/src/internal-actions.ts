/**
 * Staging-only, default-dark ingress to the Container's private
 * `POST /internal/actions` receiver (TOG-12980; design TOG-12973, CISO
 * conditions C1-C10 in TOG-12979).
 *
 * Dark unless BOTH hold: the staging-only Worker var INTERNAL_ACTIONS_INGRESS
 * is exactly "1" (wrangler.toml `[env.staging.vars]` only, enforced by
 * scripts/check-env-bindings.py) and the Operator has set TWO_INTERNAL_ACTIONS
 * to exactly "1". In every other state the route does not exist: the caller
 * sees today's behavior and the Container is never contacted.
 *
 * Authentication is NOT done here. The receiver keeps legacy v1 HMAC + durable
 * nonce burn; this module only bounds what may reach it (method, exact path,
 * no query, content type, 2 MiB body, timeouts, header allowlist, rate caps)
 * and never relays Container error text. Nothing here is ever logged except
 * scalar event/status/error_class/request_id plus the container `release`
 * (deploy-stamped SHA, `"unknown"` when the edge has no SHA yet) and the
 * stable `log_url`: no header values, body bytes or query. The request id is
 * the validated `x-request-id` shape (or a fresh one), so it joins the DO
 * and container logs without carrying secrets.
 */

import { TokenBuckets } from "./redirect.ts";
import { LOG_QUERY_URL } from "./alert-rules.ts";
import { UNKNOWN_RELEASE, normalizeRelease } from "./error-events.ts";
import { REQUEST_ID_HEADER, ensureRequestId, isRequestId, newRequestId } from "./request-id.ts";
import { applySecurityHeaders, withSecurityHeaders } from "./security-headers.ts";

export const ACTIONS_PATH = "/internal/actions";
/** Fixed receiver port, distinct from BOT_PORT (8080). */
export const RECEIVER_PORT = 8091;
/**
 * The Worker, not the Operator, picks the bind. TOG-16851: the Containers port
 * check and `containerFetch` cannot reach a loopback-only socket inside the
 * Container, so the receiver listens on all interfaces. The bot accepts that
 * wildcard only with the Worker-set `TWO_INTERNAL_CONTAINER` marker
 * (crates/core/src/internal_action_config.rs); the container network is
 * private and the receiver keeps its HMAC/caller checks. An Operator-supplied
 * bind value is still ignored.
 */
export const RECEIVER_BIND = `0.0.0.0:${RECEIVER_PORT}`;
/**
 * Worker-set marker proving the process runs inside the private Cloudflare
 * Container network, where the wildcard `RECEIVER_BIND` is reachable only via
 * `containerFetch` and the startup port check. Never an Operator value or a
 * `wrangler.toml` var (see scripts/check-env-bindings.py).
 */
export const CONTAINER_MARKER = "TWO_INTERNAL_CONTAINER";
export const CONTAINER_MARKER_VALUE = "1";

/** Mirrors the receiver (crates/bot/src/internal_action_http.rs:41-42, MAX_BODY_BYTES). */
export const MAX_BODY_BYTES = 2 * 1024 * 1024;
export const BODY_TIMEOUT_MS = 5_000;
export const REQUEST_TIMEOUT_MS = 20_000;
/** Concurrent ingress requests per isolate; the receiver itself admits 32. */
export const MAX_IN_FLIGHT = 8;
export const MAX_RESPONSE_BYTES = 64 * 1024;

export interface IngressEnv {
  /** Staging-only var; absent or anything but "1" keeps the route dark. */
  INTERNAL_ACTIONS_INGRESS?: string;
  /** Operator-set Worker secret; "1" is the last switch in the enable order. */
  TWO_INTERNAL_ACTIONS?: string;
}

export function receiverEnabled(env: IngressEnv): boolean {
  return env.TWO_INTERNAL_ACTIONS === "1";
}

export function ingressEnabled(env: IngressEnv): boolean {
  return env.INTERNAL_ACTIONS_INGRESS === "1" && receiverEnabled(env);
}

// [header, receiver's byte cap]. Every value must be one printable-ASCII token
// without commas, so a duplicated header (which Headers joins with ", ") or an
// injected list never reaches the receiver's singleton check. Exported so the
// signed backfill-progress edge gate reuses the same caps (TOG-22953).
export const SIGNING_HEADERS = [
  ["x-two-key-id", 128],
  ["x-two-timestamp", 15],
  ["x-two-nonce", 32],
  ["x-two-signature", 71],
] as const;
export const IDEMPOTENCY_HEADER = ["idempotency-key", 200] as const;
/** Inbound `content-type` header byte cap, enforced before the media-type allowlist. */
export const CONTENT_TYPE_MAX_BYTES = 128;
const HEADER_VALUE = /^[\x21-\x2b\x2d-\x7e]+$/;
const CONTENT_TYPES = ["application/json", "application/json; charset=utf-8"];

// Per-isolate caps, the same best-effort shape as healthBuckets/clickBuckets.
// The receiver burns a permit and a DB nonce per request before any per-key
// bucket exists, so unauthenticated floods must be cut here (CISO C5).
const actionsBuckets = new TokenBuckets({ capacity: 20, refillPerSecond: 1 });
let inFlight = 0;

/** Test seam: forget per-isolate admission state. */
export function resetIngressLimits(): void {
  inFlight = 0;
}

export function notFound(): Response {
  return new Response("not found", { status: 404, headers: withSecurityHeaders() });
}

/**
 * The receiver's envelope, with a fixed message: never Container text. The
 * id is the flow's `x-request-id` (honored inbound or minted at the edge),
 * so one value joins the Worker, DO and container logs with this response.
 */
export function envelope(
  status: number,
  code: string,
  retryable: boolean,
  headers: Record<string, string> = {},
  requestId: string = newRequestId(),
): Response {
  const hardened = new Headers({ "cache-control": "no-store", ...headers });
  applySecurityHeaders(hardened);
  return Response.json(
    {
      ok: false,
      request_id: requestId,
      error: { code, message: "request refused", retryable },
    },
    { status, headers: hardened },
  );
}

function refuse(
  status: number,
  code: string,
  retryable: boolean,
  headers?: Record<string, string>,
  requestId: string = newRequestId(),
  release: unknown = UNKNOWN_RELEASE,
): Response {
  console.warn(JSON.stringify({
    event: "internal_actions_ingress",
    status,
    error_class: code,
    request_id: requestId,
    release: normalizeRelease(release),
    log_url: LOG_QUERY_URL,
  }));
  return envelope(status, code, retryable, headers, requestId);
}

/** True when the receiver would answer 404 for this request shape. */
export function isActionsRequest(request: Request): boolean {
  if (request.method !== "POST") return false;
  const url = new URL(request.url);
  return url.pathname === ACTIONS_PATH && url.search === "" && !request.url.includes("?");
}

/**
 * The allowlisted headers, or the refusal that explains the first bad one.
 * `authorization`, `cookie`, `cf-connecting-ip` and any caller-supplied
 * deployment header are never copied (CISO C3). `x-request-id` is the one
 * caller header that flows on: only its validated generated shape (a
 * non-secret 26-char token; anything else is replaced with a fresh id), so
 * the DO and container can join this request in their own logs.
 */
export function allowlistedHeaders(
  source: Headers,
  requestId: string = ensureRequestId(source.get(REQUEST_ID_HEADER)),
  release: unknown = UNKNOWN_RELEASE,
): { headers: Headers } | { refusal: Response } {
  const media = source.get("content-type");
  if (
    media === null || media.length > CONTENT_TYPE_MAX_BYTES || !CONTENT_TYPES.includes(media.toLowerCase()) ||
    source.has("content-encoding")
  ) {
    return { refusal: refuse(415, "unsupported_media_type", false, undefined, requestId, release) };
  }
  const headers = new Headers({ "content-type": media });
  for (const [name, max] of SIGNING_HEADERS) {
    const value = source.get(name);
    if (value === null || value === "") return { refusal: refuse(401, "unauthorized", false, undefined, requestId, release) };
    if (value.length > max || !HEADER_VALUE.test(value)) {
      return { refusal: refuse(400, "malformed", false, undefined, requestId, release) };
    }
    headers.set(name, value);
  }
  if (source.has("x-two-scope") || source.has("x-two-guild")) {
    const scope = source.get("x-two-scope") ?? "";
    const guild = source.get("x-two-guild") ?? "";
    if (!/^(read|moderate|admin)$/.test(scope) || !/^\d{1,20}$/.test(guild)) {
      return { refusal: envelope(401, "unauthorized", false, {}, requestId) };
    }
    headers.set("x-two-scope", scope);
    headers.set("x-two-guild", guild);
  }
  const [name, max] = IDEMPOTENCY_HEADER;
  const key = source.get(name);
  if (key !== null) {
    if (key.length > max || !HEADER_VALUE.test(key)) return { refusal: refuse(400, "malformed", false, undefined, requestId, release) };
    headers.set(name, key);
  }
  headers.set(REQUEST_ID_HEADER, requestId);
  return { headers };
}

export type BodyRead = { bytes: Uint8Array } | { refusal: Response };

/** Read at most MAX_BODY_BYTES within BODY_TIMEOUT_MS, byte for byte. */
export async function readBody(
  request: Request,
  requestId: string = ensureRequestId(request.headers.get(REQUEST_ID_HEADER)),
  release: unknown = UNKNOWN_RELEASE,
): Promise<BodyRead> {
  const declared = request.headers.get("content-length");
  if (declared !== null) {
    if (!/^\d{1,10}$/.test(declared)) return { refusal: refuse(400, "malformed", false, undefined, requestId, release) };
    if (Number(declared) > MAX_BODY_BYTES) return { refusal: refuse(413, "payload_too_large", false, undefined, requestId, release) };
  }
  if (request.body === null) return { refusal: refuse(400, "malformed", false, undefined, requestId, release) };
  const reader = request.body.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<"timeout">((resolve) => {
    timer = setTimeout(() => resolve("timeout"), BODY_TIMEOUT_MS);
  });
  try {
    for (;;) {
      const step = await Promise.race([reader.read(), timeout]);
      if (step === "timeout") {
        void reader.cancel().catch(() => {});
        return { refusal: refuse(408, "request_timeout", true, undefined, requestId, release) };
      }
      if (step.done) break;
      total += step.value.byteLength;
      if (total > MAX_BODY_BYTES) {
        void reader.cancel().catch(() => {});
        return { refusal: refuse(413, "payload_too_large", false, undefined, requestId, release) };
      }
      chunks.push(step.value);
    }
  } catch {
    return { refusal: refuse(400, "malformed", false, undefined, requestId, release) };
  } finally {
    clearTimeout(timer);
  }
  if (total === 0) return { refusal: refuse(400, "malformed", false, undefined, requestId, release) };
  const bytes = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return { bytes };
}

/** The sanitized request the DO receives; the DO adds the fence header. */
export function forwardRequest(
  url: string,
  headers: Headers,
  bytes: Uint8Array,
  signal?: AbortSignal,
): Request {
  return new Request(url, { method: "POST", headers, body: bytes, ...(signal ? { signal } : {}) });
}

/**
 * Worker half: gate, rate-cap, validate, then hand byte-exact bytes to the
 * fenced DO through `forward`. Called only when `ingressEnabled(env)`.
 */
export async function handleActionsIngress(
  request: Request,
  clientKey: string,
  forward: (sanitized: Request) => Promise<Response>,
  release: unknown = UNKNOWN_RELEASE,
): Promise<Response> {
  if (!isActionsRequest(request)) return notFound();
  const requestId = ensureRequestId(request.headers.get(REQUEST_ID_HEADER));
  const verdict = actionsBuckets.take(clientKey);
  if (!verdict.allowed) {
    return refuse(429, "rate_limited", true, { "retry-after": String(verdict.retryAfter) }, requestId, release);
  }
  const checked = allowlistedHeaders(request.headers, requestId, release);
  if ("refusal" in checked) return checked.refusal;
  if (inFlight >= MAX_IN_FLIGHT) return refuse(429, "rate_limited", true, { "retry-after": "1" }, requestId, release);
  inFlight++;
  let forwardPromise: Promise<Response> | undefined;
  try {
    const body = await readBody(request, requestId, release);
    if ("refusal" in body) return body.refusal;
    const timeoutSignal = AbortSignal.timeout(REQUEST_TIMEOUT_MS);
    const abortController = new AbortController();
    const abortOrphan = (): void => {
      try {
        abortController.abort();
      } catch {
        // Ignore: the fixed refusal below stands.
      }
    };
    if (timeoutSignal.aborted) abortOrphan();
    else timeoutSignal.addEventListener("abort", abortOrphan, { once: true });
    const signal = AbortSignal.any([timeoutSignal, abortController.signal]);
    const sanitized = forwardRequest(
      `https://worker.invalid${ACTIONS_PATH}`,
      checked.headers,
      body.bytes,
      signal,
    );
    try {
      forwardPromise = forward(sanitized);
    } catch {
      // DO construction/binding failures: never serialize the exception.
      return refuse(503, "unavailable", true, undefined, requestId, release);
    }
    // Hold the slot until the forward settles; observe now so a late
    // rejection after the deadline never surfaces as unhandled.
    void forwardPromise.then(
      () => {
        inFlight--;
      },
      () => {
        inFlight--;
      },
    );
    let timer: ReturnType<typeof setTimeout> | undefined;
    const deadline = new Promise<"timeout">((resolve) => {
      timer = setTimeout(() => resolve("timeout"), REQUEST_TIMEOUT_MS);
    });
    try {
      const upstream = await Promise.race([forwardPromise, deadline]);
      if (upstream === "timeout") {
        abortOrphan();
        return refuse(504, "upstream_timeout", true, undefined, requestId, release);
      }
      return upstream;
    } catch {
      // An abort-driven rejection after the deadline is still a timeout.
      if (signal.aborted) return refuse(504, "upstream_timeout", true, undefined, requestId, release);
      // DO construction/binding failures: never serialize the exception.
      return refuse(503, "unavailable", true, undefined, requestId, release);
    } finally {
      clearTimeout(timer);
      timeoutSignal.removeEventListener("abort", abortOrphan);
    }
  } finally {
    if (forwardPromise === undefined) inFlight--;
  }
}

const RELAYED_RESPONSE_HEADERS = ["content-type", "idempotent-replay", "retry-after"];

/**
 * Race a body read against the existing receiver deadline (`signal`). No new
 * budget is created: an already-fired signal settles promptly and a stalled
 * read cannot hold the caller past the deadline. Cleanup stays with the
 * caller (best-effort release, never awaited).
 */
function boundRead<T>(work: Promise<T>, signal?: AbortSignal): Promise<T> {
  if (!signal) return work;
  if (signal.aborted) return Promise.reject(new Error("receiver deadline exceeded"));
  let onAbort = () => {};
  const aborted = new Promise<never>((_, reject) => {
    onAbort = () => reject(new Error("receiver deadline exceeded"));
    signal.addEventListener("abort", onAbort, { once: true });
  });
  return Promise.race([work, aborted]).finally(() => {
    signal.removeEventListener("abort", onAbort);
  });
}

/** Best-effort body release: a stalling cancel hook must not hold the caller. */
function releaseUpstream(upstream: Response): void {
  if (!upstream.body) return;
  try {
    void upstream.body.cancel().catch(() => {});
  } catch {
    // Ignore a synchronously-throwing hook: the fixed refusal below stands.
  }
}

/**
 * Best-effort reader release: cancel is attempted but never awaited, so a
 * never-settling cancel cannot hold the wrapper critical section. The lock is
 * dropped where possible; a still-pending read settles on its own.
 */
function releaseReader(reader: ReadableStreamDefaultReader<Uint8Array>): void {
  try {
    void reader.cancel().catch(() => {});
  } catch {
    // Ignore a synchronously-throwing hook: the fixed refusal stands.
  }
  try {
    reader.releaseLock();
  } catch {
    // Ignore: a pending read may still hold the lock; it settles on its own.
  }
}

/**
 * The receiver's refusal vocabulary, mirrored from its constructors and call
 * sites (`crates/core/src/internal_actions.rs`: `ErrorCode::as_str/status/
 * retryable`; `crates/bot/src/internal_action_http.rs`: `Failure::response`,
 * boundary `not_found`/`handle`/`receive`, terminal `needs_reconciliation`/
 * `no_effect`): each code is emitted with exactly one status and one retry
 * policy. `payload_too_large` and `unsupported_media_type` are EventSub-only
 * wire codes, and `unavailable` is Worker-only, so none of the three is
 * relayable here. A `Map` (not a plain object) keeps inherited names like
 * `toString` from matching as codes.
 */
const RECEIVER_REFUSALS: ReadonlyMap<string, { status: number; retryable: boolean }> = new Map([
  ["malformed", { status: 400, retryable: false }],
  ["unauthorized", { status: 401, retryable: false }],
  ["stale_request", { status: 401, retryable: false }],
  ["action_not_allowed", { status: 403, retryable: false }],
  ["not_found", { status: 404, retryable: false }],
  ["method_not_allowed", { status: 405, retryable: false }],
  ["request_timeout", { status: 408, retryable: true }],
  ["replayed", { status: 409, retryable: false }],
  ["version_conflict", { status: 409, retryable: false }],
  ["in_progress", { status: 409, retryable: true }],
  ["needs_reconciliation", { status: 409, retryable: false }],
  ["discord_rejected", { status: 422, retryable: false }],
  ["rate_limited", { status: 429, retryable: true }],
  ["internal", { status: 500, retryable: true }],
  ["discord_unavailable", { status: 502, retryable: true }],
  ["no_effect", { status: 502, retryable: false }],
  ["busy", { status: 503, retryable: true }],
  ["upstream_timeout", { status: 504, retryable: true }],
]);

/** True for a JSON object: objects only, never null or an array. */
function isJsonObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * Project one collected JSON body to the receiver's supported envelope, or
 * `null` when it is not one. Successes keep only `{ok: true, request_id,
 * result}` at status 200 with the action-specific result verbatim plus the
 * optional top-level settings `version` beside it (never inside the result);
 * refusals keep only `{ok: false, request_id, error: {code, retryable}}` with
 * their table status and the fixed refusal message. Extra top-level or nested
 * fields and the upstream message text never leave. The id must be the
 * generated shape AND the id actually forwarded, so a foreign or mismatched id
 * fails closed. This is schema/outflow enforcement, not proof of upstream
 * authenticity.
 */
function projectReceiverJson(
  status: number,
  body: Uint8Array,
  requestId: string,
): { ok: true; request_id: string; result: Record<string, unknown>; version?: number } | {
  ok: false;
  request_id: string;
  error: { code: string; message: string; retryable: boolean };
} | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(new TextDecoder("utf-8", { fatal: true, ignoreBOM: false }).decode(body));
  } catch {
    return null;
  }
  if (!isJsonObject(parsed)) return null;
  if (typeof parsed["request_id"] !== "string") return null;
  const upstreamId = parsed["request_id"];
  if (!isRequestId(upstreamId) || upstreamId !== requestId) return null;
  if (parsed["ok"] === true) {
    if (status !== 200 || "error" in parsed) return null;
    const result = parsed["result"];
    if (!isJsonObject(result)) return null;
    const projected: { ok: true; request_id: string; result: Record<string, unknown>; version?: number } = {
      ok: true, request_id: requestId, result,
    };
    if ("version" in parsed) {
      const version = parsed["version"];
      if (typeof version !== "number" || !Number.isInteger(version)) return null;
      projected.version = version;
    }
    return projected;
  }
  if (parsed["ok"] !== false) return null;
  const error = parsed["error"];
  if (!isJsonObject(error)) return null;
  const { code, retryable } = error;
  if (typeof code !== "string" || typeof retryable !== "boolean") return null;
  const verdict = RECEIVER_REFUSALS.get(code);
  if (verdict === undefined || verdict.status !== status || verdict.retryable !== retryable) return null;
  return {
    ok: false,
    request_id: requestId,
    error: { code, message: "request refused", retryable },
  };
}

/**
 * Only the receiver's own JSON envelope may leave, validated against the
 * receiver's constructors and projected to the supported contract: successes
 * keep their action-specific result verbatim beside the correlated id (plus
 * the optional top-level settings version), refusals keep their table
 * status/code/retryability with the fixed message, and anything else becomes
 * the fixed unavailable envelope. SDK 0.3.7 synthesizes text 429/500/503
 * bodies from startup failures (raw `e.message`), and a wrong listener could
 * answer anything else; both become the fixed unavailable envelope.
 * Allowlisted headers only.
 *
 * The 64 KiB cap is enforced incrementally while reading: the reader is
 * cancelled the moment the cap is crossed, so no tail past the cap is
 * collected. Rejected shapes (wrong media, out-of-range status, bodyless) are
 * cancelled without reading a byte. Read errors, oversize and cancel failures
 * all resolve to the same fixed 503 envelope; no exception, wire body or
 * header value ever leaves.
 *
 * When `signal` (the existing receiver request deadline) is given, every read
 * races it: a stalled tail settles the caller at the deadline instead of
 * holding the wrapper critical section. Cancellation is best-effort and never
 * awaited, so a never-settling cancel hook cannot hold the caller either.
 *
 * `release` stamps the container SHA on the span/refusal log line so it joins
 * the flow's container logs; hostile values bucket to `unknown` and never echo.
 */
export async function relayReceiverResponse(
  upstream: Response,
  requestId: string = newRequestId(),
  signal?: AbortSignal,
  release: unknown = UNKNOWN_RELEASE,
): Promise<Response> {
  const media = upstream.headers.get("content-type")?.split(";")[0]?.trim().toLowerCase();
  const bodyless = upstream.status === 204 || upstream.status === 205 || upstream.status === 304;
  if (media !== "application/json" || upstream.status < 200 || upstream.status > 599 || bodyless) {
    // Cancel without reading: a huge or never-ending rejected tail must not
    // be drained here. The cancel is best-effort and never awaited: a
    // never-settling hook still yields the fixed refusal promptly.
    releaseUpstream(upstream);
    return refuse(503, "unavailable", true, undefined, requestId, release);
  }
  const reader = upstream.body?.getReader();
  if (!reader) {
    // The receiver always answers with a body: a bodiless JSON response is
    // not an envelope, so it fails closed.
    releaseUpstream(upstream);
    return refuse(503, "unavailable", true, undefined, requestId, release);
  }
  const chunks: Uint8Array[] = [];
  let total = 0;
  try {
    for (;;) {
      if (signal?.aborted) {
        releaseReader(reader);
        return refuse(503, "unavailable", true, undefined, requestId, release);
      }
      let step: ReadableStreamReadResult<Uint8Array>;
      try {
        step = await boundRead(reader.read(), signal);
      } catch {
        releaseReader(reader);
        return refuse(503, "unavailable", true, undefined, requestId, release);
      }
      if (step.done) break;
      total += step.value.byteLength;
      if (total > MAX_RESPONSE_BYTES) {
        releaseReader(reader);
        return refuse(503, "unavailable", true, undefined, requestId, release);
      }
      chunks.push(step.value);
    }
  } catch {
    return refuse(503, "unavailable", true, undefined, requestId, release);
  }
  const body = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    body.set(chunk, offset);
    offset += chunk.byteLength;
  }
  const projected = projectReceiverJson(upstream.status, body, requestId);
  if (projected === null) {
    return refuse(503, "unavailable", true, undefined, requestId, release);
  }
  const headers = relayHeaders(upstream.headers);
  return Response.json(projected, { status: upstream.status, headers });
}

function relayHeaders(source: Headers): Headers {
  const headers = new Headers();
  for (const name of RELAYED_RESPONSE_HEADERS) {
    const value = source.get(name);
    if (value !== null) headers.set(name, value);
  }
  const retry = headers.get("retry-after");
  if (retry !== null && !/^\d{1,6}$/.test(retry)) headers.delete("retry-after");
  headers.set("cache-control", "no-store");
  applySecurityHeaders(headers);
  return headers;
}
