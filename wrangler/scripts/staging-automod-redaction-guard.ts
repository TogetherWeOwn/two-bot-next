import { types } from "node:util";

export type StagingAutomodRedactionFailureCode =
  | "secret_value" | "sensitive_key" | "unexpected_field" | "invalid_metadata"
  | "non_json_value" | "depth_limit" | "key_limit" | "string_limit";

type Result = Readonly<{ verdict: "disabled" }> | Readonly<{ verdict: "passed" }>
  | Readonly<{ verdict: "failed"; code: StagingAutomodRedactionFailureCode }>;
type Input = Readonly<{ enabled?: unknown; evidence?: unknown }>;

type Metadata = "integer" | "boolean" | RegExp | Fields | readonly [Fields];
type Fields = { readonly [key: string]: Metadata };

const MAX_DEPTH = 12;
const MAX_KEYS = 512;
const MAX_STRING_LENGTH = 4096;
const sha = /^[0-9a-f]{40}$/;
const uuid = /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/;
const snowflake = /^[1-9][0-9]{16,19}$/;
const isoTime = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/;
const runId = /^[1-9][0-9]{0,19}$/;
const environment = /^staging$/;
const schema = /^two-bot-staging-automod-qualification\.v1$/;
const commandName = /^automod-preview$/;
const command = { id: snowflake, version: snowflake } as const;
const serving = {
  workerDeploymentId: uuid, workerVersionId: uuid, containerApplicationId: uuid, containerRolloutId: uuid,
  containerNamespaceId: /^[0-9a-f]{32}$/, containerImageDigest: /^sha256:[0-9a-f]{64}$/, ownerEpoch: "integer",
} as const;
const scope = {
  runId, runAttempt: "integer",
  lease: { environment, purpose: commandName, phase: /^active$/, runId, runAttempt: "integer",
    deploymentId: uuid, sha, epoch: "integer", acquiredAt: "integer", expiresAt: "integer" },
} as const;
const observation = {
  scope, observedAt: "integer",
  identity: { ...serving, sourceSha: sha, ownerPhase: /^active$/, trafficPercent: "integer",
    runtimePolicy: /^bounded$/, rolloutStatus: /^completed$/ },
} as const;
const stopped = { lease: /^absent$/, container: /^stopped$/ } as const;
const receipt = {
  schema, before: observation, after: observation,
  registry: {
    scope, startedAt: "integer", completedAt: "integer",
    command: { ...command, name: commandName, applicationId: snowflake, guildId: snowflake,
      type: "integer", default_member_permissions: /^32$/, options: [{
        name: /^(?:text|mentions|attachments)$/, type: "integer", required: "boolean", autocomplete: "boolean",
        choiceCount: "integer", min_length: "integer", max_length: "integer", min_value: "integer", max_value: "integer",
      }] },
  },
  router: { sourceSha: sha, name: commandName, requiredPermission: "integer",
    withManageGuild: /^admitted$/, withoutManageGuild: /^denied$/ },
  coldStartRefusal: {
    scope, action: /^start$/, startedAt: "integer", completedAt: "integer", before: stopped, after: stopped,
    response: { schema: /^two-bot-staging-automod-job\.v1$/, environment, observedAt: isoTime,
      verdict: /^failed$/, code: /^request_refused$/ },
    effects: { leaseWrites: "integer", containerAcquisitions: "integer", containerInvocations: "integer",
      containerStarts: "integer", containerStops: "integer", containerWrites: "integer" },
  },
} as const satisfies Fields;
const bundle = {
  schema, environment, observedAt: isoTime, verdict: /^(?:disabled|passed|failed)$/,
  code: /^(?:receipt_unverified|rehearsal_unconfirmed)$/, runId, sha, ...serving, command,
} as const satisfies Fields;
const evidenceFields = {
  ...receipt, ...bundle, receipt, bundle,
  rehearsal: { verdict: /^passed$/, start: "boolean", recover: "boolean", runId, sha, deploymentId: uuid },
} as const satisfies Fields;

const sensitiveKeys = new Set([
  "token", "secret", "password", "authorization", "proxyauthorization", "cookie", "setcookie", "apikey",
  "privatekey", "discordtoken", "discordbottoken", "githubtoken", "cloudflareapitoken", "cloudflareapikey",
  "cloudflareglobalapikey", "cfapitoken", "cfapikey", "xauthkey", "xauthemail", "xapitoken", "xapikey",
]);
const labeledSecrets = [
  /\b(?:authorization|cookie|set-cookie)\s*:/i,
  /\b(?:bearer|bot)\s+[A-Za-z0-9._~+/=-]+/i,
  /\b(?:gh[psou]_|github_pat_)[A-Za-z0-9_]+/i,
  /(?:^|[^\w-])[\w-]{23,28}\.[\w-]{6,7}\.[\w-]{25,50}(?![\w-])|\bmfa\.[\w-]{20,}/,
  /\b(?:cloudflare[_ -]+(?:global[_ -]+)?api[_ -]+(?:token|key)|cf[_ -]+api[_ -]+(?:token|key)|x-auth-key|x-auth-email)\s*[:=]/i,
  /\bv1\.0-[\w-]{24}-[\w-]{146}(?![\w-])/,
  /-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY-----/,
  /\bpostgres(?:ql)?:\/\/[^\s/?#]*@/i,
  /(?:^|[?&\s])(?:user|password|passfile)\s*=/i,
];
const cloudflareCredential = /(?:^|[^\w-])(?:[a-f0-9]{37}|[\w-]{40})(?![\w-])/i;

function hasSecret(value: string, rule?: Metadata): boolean {
  if (labeledSecrets.some(pattern => pattern.test(value))) return true;
  for (const [uri] of value.matchAll(/\bpostgres(?:ql)?:\/\/[^\s"'<>]+/gi)) {
    const url = URL.parse(uri);
    if (url && (url.username || url.password
      || [...url.searchParams.keys()].some(key => /^(?:user|password|passfile)$/i.test(key)))) return true;
  }
  // A bare API token can have the same alphabet and length as a commit SHA.
  return !(rule === sha && sha.test(value)) && cloudflareCredential.test(value);
}

function childRule(fields: Fields | undefined, key: string): Metadata | undefined {
  return fields && Object.hasOwn(fields, key) ? fields[key] : undefined;
}

function recordRule(rule: Metadata | undefined): rule is Fields {
  return typeof rule === "object" && !(rule instanceof RegExp) && !Array.isArray(rule);
}

type Scan = { remaining: number; ancestors: Set<object> };

function inspect(value: unknown, rule: Metadata | undefined, scan: Scan, depth: number):
  StagingAutomodRedactionFailureCode | undefined {
  if (depth > MAX_DEPTH) return "depth_limit";
  switch (typeof value) {
    case "string":
      if (value.length > MAX_STRING_LENGTH) return "string_limit";
      if (hasSecret(value, rule)) return "secret_value";
      return rule instanceof RegExp && rule.test(value) ? undefined : "invalid_metadata";
    case "number":
      if (!Number.isFinite(value)) return "non_json_value";
      return rule === "integer" && Number.isSafeInteger(value) && value >= 0 ? undefined : "invalid_metadata";
    case "boolean":
      return rule === "boolean" ? undefined : "invalid_metadata";
    case "object":
      if (value === null) return "invalid_metadata";
      break;
    default:
      return "non_json_value";
  }
  if (types.isProxy(value) || scan.ancestors.has(value)) return "non_json_value";
  const array = Array.isArray(value);
  const prototype: unknown = Object.getPrototypeOf(value);
  if (array ? prototype !== Array.prototype : prototype !== null && prototype !== Object.prototype) return "non_json_value";
  if (array && value.length > scan.remaining) return "key_limit";
  const keys = Reflect.ownKeys(value);
  // Arrays include their built-in length property and must be dense JSON lists.
  scan.remaining -= keys.length - Number(array);
  if (scan.remaining < 0) return "key_limit";
  if (array && keys.length !== value.length + 1) return "non_json_value";
  const fields = recordRule(rule) ? rule : undefined;
  const element: Metadata | undefined = Array.isArray(rule) ? rule[0] : undefined;
  let invalidShape = array ? !element : !fields;
  let invalidMetadata = false;
  scan.ancestors.add(value);
  for (const [index, key] of keys.entries()) {
    if (array && key === "length") continue;
    if (typeof key !== "string" || (array && key !== String(index))) return "non_json_value";
    if (key.length > MAX_STRING_LENGTH) return "string_limit";
    if (hasSecret(key)) return "secret_value";
    const normalized = key.toLowerCase().replace(/[-_\s]/g, "");
    if (sensitiveKeys.has(normalized)) return "sensitive_key";
    const descriptor = Object.getOwnPropertyDescriptor(value, key);
    if (!descriptor?.enumerable || !Object.hasOwn(descriptor, "value")) return "non_json_value";
    const child: unknown = descriptor.value;
    if ((normalized === "databaseurl" || normalized === "dsn") && typeof child === "string"
      && child.length <= MAX_STRING_LENGTH
      && (hasSecret(child) || /:\/\/[^\s/?#]*@/.test(child))) return "sensitive_key";
    const childMetadata = array ? element : childRule(fields, key);
    const failure = inspect(child, childMetadata, scan, depth + 1);
    if (failure && failure !== "invalid_metadata" && failure !== "unexpected_field") return failure;
    if (!childMetadata || failure === "unexpected_field") invalidShape = true;
    else if (failure === "invalid_metadata") invalidMetadata = true;
  }
  scan.ancestors.delete(value);
  if (invalidShape || (depth === 0 && keys.length === 0)) return "unexpected_field";
  return invalidMetadata ? "invalid_metadata" : undefined;
}

function ownValue(value: Input, key: keyof Input): unknown {
  const descriptor = Object.getOwnPropertyDescriptor(value, key);
  return descriptor && Object.hasOwn(descriptor, "value") ? descriptor.value : undefined;
}

/** Scan a decoded receipt, bundle or evidence envelope for publication hygiene.
 * The caller bounds serialized input before decoding and verifies qualification
 * independently. This guard returns only fixed verdicts and never publishes data.
 */
export function verifyStagingAutomodRedaction(input: Input = {}): Result {
  if (!input || types.isProxy(input) || ownValue(input, "enabled") !== true) {
    return Object.freeze({ verdict: "disabled" });
  }
  const code = inspect(ownValue(input, "evidence"), evidenceFields, { remaining: MAX_KEYS, ancestors: new Set() }, 0);
  return code ? Object.freeze({ verdict: "failed", code }) : Object.freeze({ verdict: "passed" });
}
