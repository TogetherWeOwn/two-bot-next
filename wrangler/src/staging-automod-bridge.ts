import { sameLease, STAGING_AUTOMOD_TARGET } from "./staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE, parseStagingUseChange, type StagingUse, type StagingUseChange } from "./staging-lifecycle.ts";
import { parseUniqueJson } from "./unique-json.ts";

/** Authenticated Worker -> Durable Object path for the protected AutoMod job. */
export const STAGING_AUTOMOD_PATH = "/internal/staging-automod";
/** Carries a fresh GitHub Actions OIDC token, verified inside the DO. */
export const STAGING_AUTOMOD_OIDC_HEADER = "x-two-actions-oidc";
export const STAGING_AUTOMOD_AUDIENCE = "two-bot-staging-automod";
export const STAGING_AUTOMOD_BODY_BYTES = 4_096;

const ISSUER = "https://token.actions.githubusercontent.com";
const JWKS_URL = `${ISSUER}/.well-known/jwks`;
const MAX_TOKEN_LENGTH = 8_192;
const MAX_JWKS_BYTES = 65_536;
const SKEW_SECONDS = 60;
const REQUIRED_CLAIMS = Object.freeze({
  iss: ISSUER,
  repository_id: "1396224242",
  ref: "refs/heads/main",
  ref_protected: "true",
  event_name: "workflow_dispatch",
  run_attempt: "1",
  environment: "staging",
  workflow_ref: "TogetherWeOwn/two-bot-next/.github/workflows/staging-automod-preview.yml@refs/heads/main",
});

/** The run scope bound by a verified token. */
export type StagingAutomodRun = Readonly<{ runId: string; runAttempt: 1; sha: string }>;
type Lease = Readonly<StagingUse>;
type Pin = Readonly<{
  workerDeploymentId: string; workerVersionId: string; sourceSha: string;
  applicationId: string; namespaceId: string; rolloutId: string; imageDigest: string;
}>;
type LeaseContext = Readonly<{
  environment: "staging"; deploymentId: string; sha: string; runId: string; runAttempt: number;
  target: typeof STAGING_AUTOMOD_TARGET;
}>;
type Action = "start" | "recover";

/** Closed operation set. Each names exactly one bindProtectedAutomodInstance member. */
export type StagingAutomodOperation =
  | Readonly<{ op: "acquire"; pin: Pin; change: Readonly<StagingUseChange> }>
  | Readonly<{ op: "read-lease"; context: LeaseContext }>
  | Readonly<{ op: "read-approval"; action: Action; issuedLease: Lease }>
  | Readonly<{ op: "invoke"; pin: Pin; call: Readonly<{ action: Action; issuedLease: Lease }> }>;

const PIN_KEYS = ["workerDeploymentId", "workerVersionId", "sourceSha", "applicationId", "namespaceId", "rolloutId",
  "imageDigest"] as const;
const LEASE_KEYS = ["environment", "runId", "runAttempt", "expiresAt", "deploymentId", "sha", "epoch", "acquiredAt",
  "phase", "purpose"] as const;
const CONTEXT_KEYS = ["environment", "deploymentId", "sha", "runId", "runAttempt", "target"] as const;
const OPERATION_KEYS = {
  acquire: ["op", "pin", "change"], "read-lease": ["op", "context"],
  "read-approval": ["op", "action", "issuedLease"], invoke: ["op", "pin", "call"],
} as const;

function exactKeys(value: unknown, keys: readonly string[]): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    && Object.keys(value).length === keys.length && keys.every(key => Object.hasOwn(value, key));
}

function text(value: unknown, shape: RegExp): value is string {
  return typeof value === "string" && shape.test(value);
}

function pin(value: unknown): Pin | undefined {
  if (!exactKeys(value, PIN_KEYS) || !PIN_KEYS.every(key => text(value[key], /^[A-Za-z0-9:_-]{1,128}$/))) return;
  return Object.freeze({ ...value }) as Pin;
}

function lease(value: unknown): Lease | undefined {
  if (!exactKeys(value, LEASE_KEYS) || value.purpose !== AUTOMOD_STAGING_PURPOSE) return;
  const candidate = value as unknown as Lease;
  if (sameLease(candidate, candidate)) return Object.freeze({ ...candidate });
}

function context(value: unknown): LeaseContext | undefined {
  if (!exactKeys(value, CONTEXT_KEYS) || value.environment !== "staging" || !text(value.deploymentId, /^[A-Za-z0-9_-]{1,128}$/)
    || !text(value.sha, /^[0-9a-f]{40}$/) || !text(value.runId, /^[1-9][0-9]{0,19}$/) || value.runAttempt !== 1
    || !exactKeys(value.target, ["applicationId", "guildId", "name"])
    || value.target.applicationId !== STAGING_AUTOMOD_TARGET.applicationId
    || value.target.guildId !== STAGING_AUTOMOD_TARGET.guildId || value.target.name !== STAGING_AUTOMOD_TARGET.name) return;
  return Object.freeze({ environment: "staging", deploymentId: value.deploymentId, sha: value.sha,
    runId: value.runId, runAttempt: 1, target: STAGING_AUTOMOD_TARGET });
}

function action(value: unknown): Action | undefined {
  return value === "start" || value === "recover" ? value : undefined;
}

/** Parse one strict body. Unknown operations are "unknown"; any other defect is undefined. */
export function parseStagingAutomodOperation(body: unknown): StagingAutomodOperation | "unknown" | undefined {
  if (typeof body !== "object" || body === null || Array.isArray(body)) return;
  const op = (body as { op?: unknown }).op;
  if (typeof op !== "string" || !Object.hasOwn(OPERATION_KEYS, op)) return "unknown";
  if (!exactKeys(body, OPERATION_KEYS[op as keyof typeof OPERATION_KEYS])) return;
  const value = body as Record<string, unknown>;
  if (op === "acquire") {
    const servingPin = pin(value.pin);
    const change = exactKeys(value.change, ["environment", "action", "runId", "runAttempt", "expiresAt", "expectedEpoch",
      "deploymentId", "sha"]) ? parseStagingUseChange(value.change) : null;
    if (servingPin && change?.action === "acquire" && !change.voiceMove) {
      return Object.freeze({ op, pin: servingPin, change: Object.freeze(change) });
    }
    return;
  }
  if (op === "read-lease") {
    const scope = context(value.context);
    return scope ? Object.freeze({ op, context: scope }) : undefined;
  }
  if (op === "read-approval") {
    const selected = action(value.action), issuedLease = lease(value.issuedLease);
    return selected && issuedLease ? Object.freeze({ op, action: selected, issuedLease }) : undefined;
  }
  const servingPin = pin(value.pin);
  const call = exactKeys(value.call, ["action", "issuedLease"]) ? value.call : undefined;
  const selected = action(call?.action), issuedLease = lease(call?.issuedLease);
  if (servingPin && selected && issuedLease) {
    return Object.freeze({ op: "invoke", pin: servingPin, call: Object.freeze({ action: selected, issuedLease }) });
  }
}

/** The run scope named by an operation. The verified token must match it. */
export function operationRun(operation: StagingAutomodOperation): Readonly<{ runId: string; runAttempt: number; sha: string }> {
  if (operation.op === "acquire") return operation.change;
  if (operation.op === "read-lease") return operation.context;
  if (operation.op === "read-approval") return operation.issuedLease;
  return operation.call.issuedLease;
}

function decodeSegment(segment: string): Uint8Array | undefined {
  if (!/^[A-Za-z0-9_-]+$/.test(segment)) return;
  try {
    const binary = atob(segment.replaceAll("-", "+").replaceAll("_", "/").padEnd(Math.ceil(segment.length / 4) * 4, "="));
    return Uint8Array.from(binary, character => character.charCodeAt(0));
  } catch {
    return;
  }
}

function decodeJson(segment: string): Record<string, unknown> | undefined {
  const bytes = decodeSegment(segment);
  if (!bytes) return;
  try {
    const value = parseUniqueJson(new TextDecoder("utf-8", { fatal: true, ignoreBOM: false }).decode(bytes));
    if (typeof value === "object" && value !== null && !Array.isArray(value)) return value as Record<string, unknown>;
  } catch {
    return;
  }
}

async function readJwks(signal: AbortSignal): Promise<unknown> {
  const response = await fetch(JWKS_URL, { method: "GET", redirect: "error", signal });
  if (response.status !== 200 || !response.body) {
    await response.body?.cancel().catch(() => {});
    return;
  }
  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let size = 0;
  for (;;) {
    const step = await reader.read();
    if (step.done) break;
    size += step.value.byteLength;
    if (size > MAX_JWKS_BYTES) {
      await reader.cancel().catch(() => {});
      return;
    }
    chunks.push(step.value);
  }
  const bytes = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.byteLength; }
  return parseUniqueJson(new TextDecoder("utf-8", { fatal: true, ignoreBOM: false }).decode(bytes));
}

function numericDate(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value > 0;
}

/** Verify a GitHub Actions OIDC token for the protected staging AutoMod run.
 * RS256 against GitHub's JWKS, the fixed audience, a bounded clock skew and
 * the exact protected workflow claims. The token's sha must be this build.
 * Returns the bound run scope, or undefined for any failure.
 */
export async function verifyStagingAutomodOidc(token: string | null, release: string | undefined,
  signal: AbortSignal, now = Date.now()): Promise<StagingAutomodRun | undefined> {
  try {
    if (!token || token.length > MAX_TOKEN_LENGTH || !text(release, /^[0-9a-f]{40}$/)) return;
    const segments = token.split(".");
    if (segments.length !== 3) return;
    const [encodedHeader, encodedClaims, encodedSignature] = segments as [string, string, string];
    const header = decodeJson(encodedHeader);
    const claims = decodeJson(encodedClaims);
    const signature = decodeSegment(encodedSignature);
    if (!header || !claims || !signature || header.alg !== "RS256" || !text(header.kid, /^[A-Za-z0-9_-]{1,128}$/)
      || ("typ" in header && header.typ !== "JWT") || "crit" in header || "jku" in header || "jwk" in header
      || "x5u" in header) return;

    const jwks = await readJwks(signal) as { keys?: unknown } | undefined;
    const keys = Array.isArray(jwks?.keys) ? jwks.keys : [];
    const matching = keys.filter((key: { kid?: unknown }) => key?.kid === header.kid);
    if (matching.length !== 1) return;
    const jwk = matching[0] as Record<string, unknown>;
    if (jwk.kty !== "RSA" || ("alg" in jwk && jwk.alg !== "RS256") || ("use" in jwk && jwk.use !== "sig")
      || !text(jwk.n, /^[A-Za-z0-9_-]{342,}$/) || !text(jwk.e, /^[A-Za-z0-9_-]{1,8}$/)) return;
    const key = await crypto.subtle.importKey("jwk", { kty: "RSA", n: jwk.n, e: jwk.e, alg: "RS256", ext: true },
      { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" }, false, ["verify"]);
    signal.throwIfAborted();
    const signed = new TextEncoder().encode(`${encodedHeader}.${encodedClaims}`);
    if (!await crypto.subtle.verify("RSASSA-PKCS1-v1_5", key, signature, signed)) return;

    const seconds = Math.floor(now / 1_000);
    const { aud, exp, nbf, iat, sha, run_id: runId } = claims;
    if (aud !== STAGING_AUTOMOD_AUDIENCE || !numericDate(exp) || !numericDate(iat) || !numericDate(nbf)
      || exp <= iat || seconds >= exp + SKEW_SECONDS || nbf > seconds + SKEW_SECONDS || iat > seconds + SKEW_SECONDS) return;
    for (const [name, expected] of Object.entries(REQUIRED_CLAIMS)) if (claims[name] !== expected) return;
    if (sha !== release || !text(runId, /^[1-9][0-9]{0,19}$/)) return;
    return Object.freeze({ runId, runAttempt: 1, sha });
  } catch {
    return;
  }
}
