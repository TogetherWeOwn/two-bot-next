const STAGING_ORIGIN = "https://next.togetherweown.com";
const RECEIPT_URL = `${STAGING_ORIGIN}/__probe/voice-move/session`;
const MAX_RECEIPT_BYTES = 1024;
const WORKER_VERSION = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const COMMIT = /^[0-9a-f]{40}$/;
const OBSERVED_AT = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/;
const WEB_REQUEST_ID = /^(?:[a-fA-F0-9]{16}(?:-[A-Z]{3})?|[0-7][0-9A-HJKMNP-TV-Z]{25})$/;

const matches = (value, pattern) => typeof value === "string" && pattern.test(value);
const snowflake = (value) => matches(value, /^[1-9][0-9]{16,19}$/) && BigInt(value) < 2n ** 64n;
const unconfirmed = () => new Error("staging_voice_session_unconfirmed");

const exactObject = (value, keys) => value !== null && typeof value === "object" && !Array.isArray(value)
  && Object.keys(value).length === keys.length && keys.every((key) => Object.hasOwn(value, key));

function acceptedResponse(response, status) {
  if (response?.status !== status || response.redirected !== false || response.url !== RECEIPT_URL
    || response.headers.has("location") || response.headers.has("set-cookie")) return false;
  const mediaType = response.headers.get("content-type")?.split(";", 1)[0].trim().toLowerCase();
  const cache = response.headers.get("cache-control")?.toLowerCase().split(",").map((part) => part.trim());
  const length = response.headers.get("content-length");
  return mediaType === "application/json" && cache?.length === 2 && cache.includes("private") && cache.includes("no-store")
    && (length === null || (/^[0-9]+$/.test(length) && Number(length) <= MAX_RECEIPT_BYTES));
}

async function readReceipt(reader, signal) {
  const bodyBytes = new Uint8Array(MAX_RECEIPT_BYTES);
  let bytes = 0;
  for (;;) {
    const { done, value } = await reader.read();
    signal.throwIfAborted();
    if (done) break;
    if (!(value instanceof Uint8Array)) throw unconfirmed();
    if (bytes + value.byteLength > MAX_RECEIPT_BYTES) throw unconfirmed();
    bodyBytes.set(value, bytes);
    bytes += value.byteLength;
  }

  const text = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(bodyBytes.subarray(0, bytes));
  const body = JSON.parse(text);
  // JSON.parse collapses duplicate keys. Count wire keys so the validators can reject duplicates.
  const keys = Array.from(text.matchAll(/"(?:[^"\\]|\\[\s\S])*"(\s*:)?/g)).filter((token) => token[1]);
  return { body, keyCount: keys.length };
}

function currentObservation({ body, keyCount }, versionId, commit) {
  if (keyCount !== 5 || !exactObject(body, ["matched", "observed_at", "revision"]) || body.matched !== true
    || !exactObject(body.revision, ["version_id", "commit"])
    || body.revision.version_id !== versionId || body.revision.commit !== commit) throw unconfirmed();

  const observedAt = body.observed_at;
  const time = matches(observedAt, OBSERVED_AT) ? Date.parse(observedAt) : NaN;
  if (!(time > 0) || new Date(time).toISOString() !== observedAt) throw unconfirmed();
  return observedAt;
}

function requireDisabledRefusal({ body, keyCount }) {
  const validShape = keyCount === 1 && exactObject(body, ["error"])
    || keyCount === 2 && exactObject(body, ["error", "request_id"]) && matches(body.request_id, WEB_REQUEST_ID);
  if (!validShape || body.error !== "not_found") throw unconfirmed();
}

export function stagingVoiceSessionScopeMatches({ session, fixture, expectedRevision }) {
  const { version_id, commit } = expectedRevision ?? {};
  const { discordId, channelIds } = fixture ?? {};
  return fixture?.environment === "staging" && snowflake(discordId) && Array.isArray(channelIds)
    && channelIds.length === 2 && snowflake(channelIds[0]) && snowflake(channelIds[1])
    && new Set([discordId, ...channelIds]).size === 3
    && session?.environment === "staging" && session.origin === STAGING_ORIGIN && session.discordId === discordId
    && typeof session.request === "function" && typeof session.move === "function"
    && matches(version_id, WORKER_VERSION) && matches(commit, COMMIT);
}

/**
 * Build the protected runner's scoped session from the approved staging fixture and Web
 * revision pins. The injected scope owns both request and move and must use
 * the same installed Web session for both. Request has Fetch semantics,
 * including the final URL, redirect flag and a response byte stream.
 * Lease-relative freshness belongs to the runner's preflight.
 */
export function bindStagingVoiceSession(inputs = {}) {
  if (!stagingVoiceSessionScopeMatches(inputs)) throw unconfirmed();
  const { session, fixture: { discordId, channelIds }, expectedRevision: { version_id: versionId, commit } } = inputs;

  const orderedChannels = [channelIds[0], channelIds[1]];
  const requestBody = JSON.stringify({ discord_id: discordId, channel_ids: orderedChannels });
  const request = session.request.bind(session);
  async function read(status, signal, parse) {
    let response;
    let reader;
    let onAbort;
    const cancelBody = () => {
      if (!response) return;
      try {
        void (reader ? reader.cancel() : response.body?.cancel())?.catch(() => {});
      } catch {
        // Cleanup failures must not escape the refusal or hold up the runner.
      } finally {
        response = undefined;
      }
    };
    try {
      signal.throwIfAborted();
      const aborted = new Promise((_, reject) => {
        onAbort = () => {
          reject(unconfirmed());
          cancelBody();
        };
        signal.addEventListener("abort", onAbort, { once: true });
      });
      const observe = async () => {
        response = await request(RECEIPT_URL, {
          method: "POST",
          headers: { origin: STAGING_ORIGIN, accept: "application/json", "content-type": "application/json" },
          body: requestBody,
          redirect: "error",
          cache: "no-store",
          signal,
        });
        try {
          signal.throwIfAborted();
          if (!acceptedResponse(response, status)) throw unconfirmed();
          reader = response.body?.getReader();
          if (!reader) throw unconfirmed();
          const receipt = await readReceipt(reader, signal);
          signal.throwIfAborted();
          return parse(receipt);
        } finally {
          cancelBody();
          reader?.releaseLock();
        }
      };
      return await Promise.race([aborted, observe()]);
    } catch {
      // Transport, parsing and cancellation errors can include private input.
      throw unconfirmed();
    } finally {
      if (onAbort) signal.removeEventListener("abort", onAbort);
    }
  }
  return Object.freeze({
    environment: "staging",
    discordId,
    move: session.move.bind(session),
    async readCurrent({ signal } = {}) {
      const observedAt = await read(200, signal, (receipt) => currentObservation(receipt, versionId, commit));
      return {
        environment: "staging", authenticated: true, action: "voice.move_member",
        discordId, channelIds: [...orderedChannels], observedAt,
      };
    },
    async readDisabled({ signal } = {}) {
      await read(404, signal, requireDisabledRefusal);
    },
  });
}
