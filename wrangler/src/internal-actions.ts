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
 * scalar event/status/error_class: no header values, body bytes or query.
 */

import { TokenBuckets } from "./redirect.ts";

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
// injected list never reaches the receiver's singleton check.
const SIGNING_HEADERS = [
  ["x-two-key-id", 128],
  ["x-two-timestamp", 15],
  ["x-two-nonce", 32],
  ["x-two-signature", 71],
] as const;
const IDEMPOTENCY_HEADER = ["idempotency-key", 200] as const;
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
  return new Response("not found", { status: 404 });
}

/** The receiver's envelope, with a fixed message: never Container text. */
export function envelope(
  status: number,
  code: string,
  retryable: boolean,
  headers: Record<string, string> = {},
): Response {
  return Response.json(
    {
      ok: false,
      request_id: crypto.randomUUID(),
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
): Response {
  console.warn(JSON.stringify({ event: "internal_actions_ingress", status, error_class: code }));
  return envelope(status, code, retryable, headers);
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
 * deployment header are never copied (CISO C3).
 */
export function allowlistedHeaders(source: Headers): { headers: Headers } | { refusal: Response } {
  const media = source.get("content-type");
  if (
    media === null || media.length > 128 || !CONTENT_TYPES.includes(media.toLowerCase()) ||
    source.has("content-encoding")
  ) {
    return { refusal: refuse(415, "unsupported_media_type", false) };
  }
  const headers = new Headers({ "content-type": media });
  for (const [name, max] of SIGNING_HEADERS) {
    const value = source.get(name);
    if (value === null || value === "") return { refusal: refuse(401, "unauthorized", false) };
    if (value.length > max || !HEADER_VALUE.test(value)) {
      return { refusal: refuse(400, "malformed", false) };
    }
    headers.set(name, value);
  }
  const [name, max] = IDEMPOTENCY_HEADER;
  const key = source.get(name);
  if (key !== null) {
    if (key.length > max || !HEADER_VALUE.test(key)) return { refusal: refuse(400, "malformed", false) };
    headers.set(name, key);
  }
  return { headers };
}

export type BodyRead = { bytes: Uint8Array } | { refusal: Response };

/** Read at most MAX_BODY_BYTES within BODY_TIMEOUT_MS, byte for byte. */
export async function readBody(request: Request): Promise<BodyRead> {
  const declared = request.headers.get("content-length");
  if (declared !== null) {
    if (!/^\d{1,10}$/.test(declared)) return { refusal: refuse(400, "malformed", false) };
    if (Number(declared) > MAX_BODY_BYTES) return { refusal: refuse(413, "payload_too_large", false) };
  }
  if (request.body === null) return { refusal: refuse(400, "malformed", false) };
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
        return { refusal: refuse(408, "request_timeout", true) };
      }
      if (step.done) break;
      total += step.value.byteLength;
      if (total > MAX_BODY_BYTES) {
        void reader.cancel().catch(() => {});
        return { refusal: refuse(413, "payload_too_large", false) };
      }
      chunks.push(step.value);
    }
  } catch {
    return { refusal: refuse(400, "malformed", false) };
  } finally {
    clearTimeout(timer);
  }
  if (total === 0) return { refusal: refuse(400, "malformed", false) };
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
): Promise<Response> {
  if (!isActionsRequest(request)) return notFound();
  const verdict = actionsBuckets.take(clientKey);
  if (!verdict.allowed) {
    return refuse(429, "rate_limited", true, { "retry-after": String(verdict.retryAfter) });
  }
  const checked = allowlistedHeaders(request.headers);
  if ("refusal" in checked) return checked.refusal;
  if (inFlight >= MAX_IN_FLIGHT) return refuse(429, "rate_limited", true, { "retry-after": "1" });
  inFlight++;
  try {
    const body = await readBody(request);
    if ("refusal" in body) return body.refusal;
    const sanitized = forwardRequest(
      `https://worker.invalid${ACTIONS_PATH}`,
      checked.headers,
      body.bytes,
    );
    let timer: ReturnType<typeof setTimeout> | undefined;
    const deadline = new Promise<"timeout">((resolve) => {
      timer = setTimeout(() => resolve("timeout"), REQUEST_TIMEOUT_MS);
    });
    try {
      const upstream = await Promise.race([forward(sanitized), deadline]);
      if (upstream === "timeout") return refuse(504, "upstream_timeout", true);
      return upstream;
    } catch {
      // DO construction/binding failures: never serialize the exception.
      return refuse(503, "unavailable", true);
    } finally {
      clearTimeout(timer);
    }
  } finally {
    inFlight--;
  }
}

const RELAYED_RESPONSE_HEADERS = ["content-type", "idempotent-replay", "retry-after"];

/**
 * Only the receiver's own JSON envelope may leave. SDK 0.3.7 synthesizes text
 * 429/500/503 bodies from startup failures (raw `e.message`), and a wrong
 * listener could answer anything else; both become the fixed unavailable
 * envelope. Allowlisted headers only.
 *
 * The 64 KiB cap is enforced incrementally while reading: the reader is
 * cancelled the moment the cap is crossed, so no tail past the cap is
 * collected. Rejected shapes (wrong media, out-of-range status, bodyless) are
 * cancelled without reading a byte. Read errors, oversize and cancel failures
 * all resolve to the same fixed 503 envelope; no exception, wire body or
 * header value ever leaves.
 */
export async function relayReceiverResponse(upstream: Response): Promise<Response> {
  const media = upstream.headers.get("content-type")?.split(";")[0]?.trim().toLowerCase();
  const bodyless = upstream.status === 204 || upstream.status === 205 || upstream.status === 304;
  if (media !== "application/json" || upstream.status < 200 || upstream.status > 599 || bodyless) {
    // Cancel without reading: a huge or never-ending rejected tail must not
    // be drained here. A failed cancel still yields the fixed refusal.
    if (upstream.body) {
      try {
        await upstream.body.cancel();
      } catch {
        // Ignore: the fixed refusal below stands.
      }
    }
    return refuse(503, "unavailable", true);
  }
  const reader = upstream.body?.getReader();
  if (!reader) {
    const headers = relayHeaders(upstream.headers);
    return new Response(new Uint8Array(), { status: upstream.status, headers });
  }
  const chunks: Uint8Array[] = [];
  let total = 0;
  try {
    for (;;) {
      let step: ReadableStreamReadResult<Uint8Array>;
      try {
        step = await reader.read();
      } catch {
        try {
          await reader.cancel();
        } catch {
          // Ignore: the fixed refusal below stands.
        }
        return refuse(503, "unavailable", true);
      }
      if (step.done) break;
      total += step.value.byteLength;
      if (total > MAX_RESPONSE_BYTES) {
        try {
          await reader.cancel();
        } catch {
          // Ignore: the fixed refusal below stands.
        }
        return refuse(503, "unavailable", true);
      }
      chunks.push(step.value);
    }
  } catch {
    return refuse(503, "unavailable", true);
  }
  const body = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    body.set(chunk, offset);
    offset += chunk.byteLength;
  }
  const headers = relayHeaders(upstream.headers);
  return new Response(body, { status: upstream.status, headers });
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
  return headers;
}
