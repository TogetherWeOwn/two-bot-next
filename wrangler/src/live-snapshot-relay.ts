import {
  ACTIONS_PATH, IDEMPOTENCY_HEADER, LIVE_SNAPSHOT_DISPATCH_HEADER, LIVE_SNAPSHOT_TIMEOUT_MS,
  MAX_IN_FLIGHT, RECEIVER_PORT, allowlistedHeaders,
  boundRead, envelope, isActionsRequest, isJsonObject, readBody, readReceiverBytes, releaseUpstream,
} from "./internal-actions.ts";
import { type OwnershipFence } from "./ownership.ts";
import { TokenBuckets } from "./redirect.ts";
import { REQUEST_ID_HEADER, ensureRequestId } from "./request-id.ts";
import { withSecurityHeaders } from "./security-headers.ts";
import { isLiveId, isSnapshot } from "./live-snapshot-wire.ts";

export { LIVE_SNAPSHOT_TIMEOUT_MS };
export const LIVE_SNAPSHOT_BYTES = 1024 * 1024;
const buckets = new TokenBuckets({ capacity: 20, refillPerSecond: 1 });
let inFlight = 0;

export type SnapshotScope = { keyId: string; guildId: string };

interface OwnedReceiver {
  // TwoBotContainer.owned also checks staging use.
  whileOwned<T>(operation: () => Promise<T>): Promise<T>;
  fence: Pick<OwnershipFence, "require">;
  deploymentId: string;
  stampedDeploymentId: string; // Trusted dispatch metadata, never taken from the HTTPS request.
  running(): boolean;
  fetch(request: Request, port: number): Promise<Response>;
}

function decode(bytes: Uint8Array): unknown {
  try {
    return JSON.parse(new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(bytes));
  } catch {
    return null;
  }
}

/** The scope and receiver come from server state. The Bot owns HMAC, replay and read authorization. */
export async function handleLiveSnapshotRelay(
  request: Request,
  clientKey: string,
  scope?: SnapshotScope,
  receiver?: OwnedReceiver,
): Promise<Response> {
  if (
    !scope || !receiver || !isLiveId(scope.guildId) || !/^[\w.:-]{1,128}$/.test(scope.keyId) ||
    !request.url.startsWith("https://") || !isActionsRequest(request)
  ) {
    return new Response(null, { status: 404, headers: withSecurityHeaders({ "cache-control": "no-store" }) });
  }
  const requestId = ensureRequestId(request.headers.get(REQUEST_ID_HEADER));
  const failure = (status: number, code: string, retryable = false) => envelope(status, code, retryable, {}, requestId);
  const verdict = buckets.take(clientKey);
  if (!verdict.allowed || inFlight >= MAX_IN_FLIGHT) {
    return envelope(429, "rate_limited", true, { "retry-after": String(verdict.allowed ? 1 : verdict.retryAfter) }, requestId);
  }
  const checked = allowlistedHeaders(request.headers, requestId);
  if ("refusal" in checked) return checked.refusal;
  const { headers } = checked;
  if (headers.get("x-two-key-id") !== scope.keyId) return failure(401, "unauthorized");
  if (headers.has(IDEMPOTENCY_HEADER[0])) return failure(400, "malformed");

  inFlight++;
  let pending: Promise<Response> | undefined;
  try {
    const signal = AbortSignal.any([request.signal, AbortSignal.timeout(LIVE_SNAPSHOT_TIMEOUT_MS)]);
    const body = await readBody(request, requestId, undefined, signal);
    if ("refusal" in body) return body.refusal;
    const submitted = decode(body.bytes);
    if (!isJsonObject(submitted) || Object.keys(submitted).length !== 2 ||
      submitted.action !== "live.snapshot") return failure(400, "malformed");
    if (submitted.guild_id !== scope.guildId) return failure(403, "action_not_allowed");
    signal.throwIfAborted();

    const unavailable = () => failure(503, "unavailable", true);
    pending = receiver.whileOwned(async () => {
      await receiver.fence.require(receiver.deploymentId, receiver.stampedDeploymentId);
      if (signal.aborted || !receiver.running()) return unavailable();
      headers.set(LIVE_SNAPSHOT_DISPATCH_HEADER, "1");
      const forward = new Request(`http://c${ACTIONS_PATH}`, {
        method: "POST", headers, body: body.bytes, signal, redirect: "manual",
      });
      const upstream = await boundRead(receiver.fetch(forward, RECEIVER_PORT).then((response) => {
        if (signal.aborted) releaseUpstream(response);
        return response;
      }), signal);
      const length = upstream.headers.get("content-length");
      if (
        signal.aborted || upstream.redirected || upstream.status !== 200 || upstream.headers.has("content-encoding") ||
        (length !== null && (!/^\d{1,10}$/.test(length) || Number(length) > LIVE_SNAPSHOT_BYTES)) ||
        !/^application\/json(?:; charset=utf-8)?$/i.test(upstream.headers.get("content-type") ?? "")
      ) {
        releaseUpstream(upstream);
        return unavailable();
      }
      const bytes = await readReceiverBytes(upstream, LIVE_SNAPSHOT_BYTES, signal);
      const snapshot = bytes === null ? null : decode(bytes);
      if (!isSnapshot(snapshot, scope.guildId)) return unavailable();

      // Re-encode validated fields so ambiguous raw JSON never crosses the boundary.
      const encoded = new TextEncoder().encode(JSON.stringify(snapshot));
      if (encoded.byteLength > LIVE_SNAPSHOT_BYTES || signal.aborted) return unavailable();
      return new Response(encoded, {
        headers: withSecurityHeaders({ "content-type": "application/json", "cache-control": "no-store" }),
      });
    });
    // Keep admission charged until the owned operation settles, even if the caller times out first.
    void pending.then(() => { inFlight--; }, () => { inFlight--; });
    const result = await boundRead(pending, signal);
    return signal.aborted ? unavailable() : result;
  } catch {
    return failure(503, "unavailable", true);
  } finally {
    if (pending === undefined) inFlight--;
  }
}
