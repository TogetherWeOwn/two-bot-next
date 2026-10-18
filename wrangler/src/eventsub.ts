/**
 * Staging-only, default-dark ingress to the Container's private Twitch
 * EventSub receiver (TOG-23364; core TOG-23359 / PR #945, CISO gate pending
 * for this new public ingress).
 *
 * Dark unless BOTH hold: the staging-only Worker var EVENTSUB_INGRESS is
 * exactly "1" (wrangler.toml `[env.staging.vars]` only, enforced by
 * scripts/check-env-bindings.py) and the Operator has set TWO_EVENTSUB to
 * exactly "1". In every other state the route does not exist: the caller sees
 * today's behavior and the Container is never contacted.
 *
 * Authentication is NOT done here. The receiver verifies the Twitch HMAC
 * (`twitch_eventsub::verify_delivery`: `sha256=` HMAC-SHA256 over
 * `message-id + timestamp + raw body`, then a 10-minute freshness window)
 * before parsing anything; this module only bounds what may reach it (POST,
 * exact path, no query, content type, 64 KiB body, timeouts, header
 * allowlist, rate caps) and never relays Container error text. Nothing here
 * is ever logged except scalar event/status/error_class/request_id plus the
 * container `release` (deploy-stamped SHA, `"unknown"` when the edge has no
 * SHA yet) and the stable `log_url`: no header values, challenge, body bytes
 * or query. The request id is the validated `x-request-id` shape (or a fresh
 * one), so it joins the DO and container logs without carrying secrets.
 *
 * The receiver listener itself (port below) lands in the follow-up slice with
 * the Operator secret provisioning. Until it does, an enabled route forwards
 * to a port nothing listens on and answers the fixed 503 envelope, exactly
 * like a cold container. The Operator simply leaves TWO_EVENTSUB unset until
 * the receiver exists.
 */

import { TokenBuckets } from "./redirect.ts";
import { LOG_QUERY_URL } from "./alert-rules.ts";
import { UNKNOWN_RELEASE, normalizeRelease } from "./error-events.ts";
import { REQUEST_ID_HEADER, ensureRequestId, newRequestId } from "./request-id.ts";

export const EVENTSUB_PATH = "/hooks/twitch";
/**
 * Fixed receiver port, distinct from BOT_PORT (8080) and the internal-actions
 * RECEIVER_PORT (8091). The DO `requiredPorts` entry and the container bind
 * are wired in `index.ts` once the Operator provisions the secret; until then
 * forwarding here fails closed.
 */
export const EVENTSUB_RECEIVER_PORT = 8092;
/**
 * The Worker, not the Operator, picks the bind (same rule as
 * `RECEIVER_BIND` in internal-actions.ts: the Containers port check and
 * `containerFetch` cannot reach a loopback-only socket, so the receiver
 * listens on all interfaces inside the private container network, gated by
 * the Worker-set `TWO_INTERNAL_CONTAINER` marker).
 */
export const EVENTSUB_RECEIVER_BIND = `0.0.0.0:${EVENTSUB_RECEIVER_PORT}`;
/** Mirrors the core (`twitch_eventsub::MAX_BODY_BYTES`): cap before hashing. */
export const MAX_BODY_BYTES = 64 * 1024;
/** A subscription challenge is one short token; anything larger is not one. */
export const MAX_CHALLENGE_BYTES = 4 * 1024;
export const BODY_TIMEOUT_MS = 5_000;
export const REQUEST_TIMEOUT_MS = 20_000;
/** Concurrent ingress requests per isolate; the receiver itself admits 32. */
export const MAX_IN_FLIGHT = 8;
export const MAX_RESPONSE_BYTES = 64 * 1024;

export interface EventsubIngressEnv {
  /** Staging-only var; absent or anything but "1" keeps the route dark. */
  EVENTSUB_INGRESS?: string;
  /** Operator-set Worker secret; "1" is the last switch in the enable order. */
  TWO_EVENTSUB?: string;
}

export function eventsubReceiverEnabled(env: EventsubIngressEnv): boolean {
  return env.TWO_EVENTSUB === "1";
}

export function eventsubIngressEnabled(env: EventsubIngressEnv): boolean {
  return env.EVENTSUB_INGRESS === "1" && eventsubReceiverEnabled(env);
}

// [header, byte cap, alphabet]. Every value must be one printable-ASCII token
// without commas, so a duplicated header (which Headers joins with ", ") or an
// injected list never reaches the receiver's singleton check. Caps mirror the
// core (`twitch_eventsub.rs`: id alphabet and MESSAGE_ID_MAX verbatim); the
// timestamp/signature/type bounds only keep edge memory flat, and the receiver
// re-validates everything during verification.
const DELIVERY_HEADERS = [
  ["twitch-eventsub-message-id", 128, /^[A-Za-z0-9_-]+$/],
  ["twitch-eventsub-message-timestamp", 64, /^[\x21-\x7e]+$/],
  ["twitch-eventsub-message-type", 64, /^[\x21-\x7e]+$/],
] as const;
const SIGNATURE_HEADER = "twitch-eventsub-message-signature";
const SIGNATURE_PREFIX = "sha256=";
const SIGNATURE_MAX = 128;
const HEADER_VALUE = /^[\x21-\x7e]+$/;
const CONTENT_TYPES = ["application/json", "application/json; charset=utf-8"];

// Per-isolate caps, the same best-effort shape as healthBuckets/clickBuckets.
// Twitch delivers from many edge IPs and retries with backoff, so a per-IP
// burst budget only blunts a single-source flood; signature verification in
// the receiver stays the real gate. Unauthenticated floods must still be cut
// here before the Container is touched.
const eventsubBuckets = new TokenBuckets({ capacity: 20, refillPerSecond: 1 });
let inFlight = 0;

/** Test seam: forget per-isolate admission state. */
export function resetEventsubIngressLimits(): void {
  inFlight = 0;
}

export function notFound(): Response {
  return new Response("not found", { status: 404 });
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
  return Response.json(
    {
      ok: false,
      request_id: requestId,
      error: { code, message: "request refused", retryable },
    },
    { status, headers: { "cache-control": "no-store", ...headers } },
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
    event: "eventsub_ingress",
    status,
    error_class: code,
    request_id: requestId,
    release: normalizeRelease(release),
    log_url: LOG_QUERY_URL,
  }));
  return envelope(status, code, retryable, headers, requestId);
}

/** True when the receiver would answer 404 for this request shape. */
export function isEventsubRequest(request: Request): boolean {
  if (request.method !== "POST") return false;
  const url = new URL(request.url);
  return url.pathname === EVENTSUB_PATH && url.search === "" && !request.url.includes("?");
}

/**
 * The allowlisted headers, or the refusal that explains the first bad one.
 * `authorization`, `cookie`, `cf-connecting-ip`, the retry/subscription
 * informational headers and any caller-supplied deployment header are never
 * copied. `x-request-id` is the one caller header that flows on: only its
 * validated generated shape (a non-secret 26-char token; anything else is
 * replaced with a fresh id), so the DO and container can join this delivery
 * in their own logs. The signature keeps its `sha256=` prefix verbatim: the
 * receiver strips and hex-decodes it during verification.
 */
export function allowlistedHeaders(
  source: Headers,
  requestId: string = ensureRequestId(source.get(REQUEST_ID_HEADER)),
  release: unknown = UNKNOWN_RELEASE,
): { headers: Headers } | { refusal: Response } {
  const media = source.get("content-type");
  if (
    media === null || media.length > 128 || !CONTENT_TYPES.includes(media.toLowerCase()) ||
    source.has("content-encoding")
  ) {
    return { refusal: refuse(415, "unsupported_media_type", false, undefined, requestId, release) };
  }
  const headers = new Headers({ "content-type": media });
  for (const [name, max, alphabet] of DELIVERY_HEADERS) {
    const value = source.get(name);
    if (value === null || value === "") return { refusal: refuse(401, "unauthorized", false, undefined, requestId, release) };
    if (value.length > max || !alphabet.test(value)) {
      return { refusal: refuse(400, "malformed", false, undefined, requestId, release) };
    }
    headers.set(name, value);
  }
  const signature = source.get(SIGNATURE_HEADER);
  if (signature === null || signature === "") return { refusal: refuse(401, "unauthorized", false, undefined, requestId, release) };
  if (
    signature.length > SIGNATURE_MAX || !HEADER_VALUE.test(signature) ||
    !signature.startsWith(SIGNATURE_PREFIX)
  ) {
    return { refusal: refuse(400, "malformed", false, undefined, requestId, release) };
  }
  headers.set(SIGNATURE_HEADER, signature);
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
 * fenced DO through `forward`. Called only when `eventsubIngressEnabled(env)`.
 */
export async function handleEventsubIngress(
  request: Request,
  clientKey: string,
  forward: (sanitized: Request) => Promise<Response>,
  release: unknown = UNKNOWN_RELEASE,
): Promise<Response> {
  if (!isEventsubRequest(request)) return notFound();
  const requestId = ensureRequestId(request.headers.get(REQUEST_ID_HEADER));
  const verdict = eventsubBuckets.take(clientKey);
  if (!verdict.allowed) {
    return refuse(429, "rate_limited", true, { "retry-after": String(verdict.retryAfter) }, requestId, release);
  }
  const checked = allowlistedHeaders(request.headers, requestId, release);
  if ("refusal" in checked) return checked.refusal;
  if (inFlight >= MAX_IN_FLIGHT) return refuse(429, "rate_limited", true, { "retry-after": "1" }, requestId, release);
  inFlight++;
  try {
    const body = await readBody(request, requestId, release);
    if ("refusal" in body) return body.refusal;
    const sanitized = forwardRequest(
      `https://worker.invalid${EVENTSUB_PATH}`,
      checked.headers,
      body.bytes,
    );
    let timer: ReturnType<typeof setTimeout> | undefined;
    const deadline = new Promise<"timeout">((resolve) => {
      timer = setTimeout(() => resolve("timeout"), REQUEST_TIMEOUT_MS);
    });
    try {
      const upstream = await Promise.race([forward(sanitized), deadline]);
      if (upstream === "timeout") return refuse(504, "upstream_timeout", true, undefined, requestId, release);
      return upstream;
    } catch {
      // DO construction/binding failures: never serialize the exception.
      return refuse(503, "unavailable", true, undefined, requestId, release);
    } finally {
      clearTimeout(timer);
    }
  } finally {
    inFlight--;
  }
}

const RELAYED_RESPONSE_HEADERS = ["content-type", "retry-after"];

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
 * Only the receiver's own answers may leave. A `200 text/plain` challenge
 * echo relays byte for byte (bounded: the challenge is one short token).
 * JSON envelopes relay with their status. SDK 0.3.7 synthesizes text
 * 429/500/503 bodies from startup failures (raw `e.message`), and a wrong
 * listener could answer anything else; both become the fixed unavailable
 * envelope. Allowlisted headers only.
 *
 * Caps are enforced incrementally while reading: the reader is cancelled the
 * moment the cap is crossed, so no tail past the cap is collected. Rejected
 * shapes are cancelled without reading a byte. Read errors, oversize and
 * cancel failures all resolve to the same fixed 503 envelope; no exception,
 * wire body or header value ever leaves.
 *
 * When `signal` (the existing receiver request deadline) is given, every read
 * races it: a stalled tail settles the caller at the deadline instead of
 * holding the wrapper critical section. Cancellation is best-effort and never
 * awaited, so a never-settling cancel hook cannot hold the caller either.
 *
 * `release` stamps the container SHA on the span/refusal log line so it joins
 * the flow's container logs; hostile values bucket to `unknown` and never echo.
 */
export async function relayEventsubResponse(
  upstream: Response,
  requestId: string = newRequestId(),
  signal?: AbortSignal,
  release: unknown = UNKNOWN_RELEASE,
): Promise<Response> {
  const media = upstream.headers.get("content-type")?.split(";")[0]?.trim().toLowerCase();
  const bodyless = upstream.status === 204 || upstream.status === 205 || upstream.status === 304;
  // Only a 200 challenge may be text; any other text body is not ours.
  const isChallenge = media === "text/plain" && upstream.status === 200;
  const isEnvelope = media === "application/json";
  if (bodyless || upstream.status < 200 || upstream.status > 599 || (!isChallenge && !isEnvelope)) {
    // Cancel without reading: a huge or never-ending rejected tail must not
    // be drained here. The cancel is best-effort and never awaited: a
    // never-settling hook still yields the fixed refusal promptly.
    releaseUpstream(upstream);
    return refuse(503, "unavailable", true, undefined, requestId, release);
  }
  const limit = isChallenge ? MAX_CHALLENGE_BYTES : MAX_RESPONSE_BYTES;
  const reader = upstream.body?.getReader();
  if (!reader) {
    const headers = relayHeaders(upstream.headers, media!);
    return new Response(new Uint8Array(), { status: upstream.status, headers });
  }
  const chunks: Uint8Array[] = [];
  let total = 0;
  try {
    for (;;) {
      if (signal?.aborted) {
        releaseReader(reader);
        return refuse(503, "unavailable", true, undefined, requestId);
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
      if (total > limit) {
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
  const headers = relayHeaders(upstream.headers, media!);
  return new Response(body, { status: upstream.status, headers });
}

function relayHeaders(source: Headers, media: string): Headers {
  const headers = new Headers({ "content-type": media });
  const retry = source.get("retry-after");
  if (retry !== null && /^\d{1,6}$/.test(retry)) headers.set("retry-after", retry);
  headers.set("cache-control", "no-store");
  return headers;
}
