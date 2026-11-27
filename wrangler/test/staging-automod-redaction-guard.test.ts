import assert from "node:assert/strict";
import test from "node:test";
import { composeStagingAutomodQualificationBundle } from "../scripts/staging-automod-qualification-bundle.ts";
import { verifyStagingAutomodRedaction as verify,
  type StagingAutomodRedactionFailureCode } from "../scripts/staging-automod-redaction-guard.ts";
import { STAGING_AUTOMOD_TARGET } from "../src/staging-automod-recovery.ts";

const now = 1_800_000_000_000;
const scope = {
  runId: "12345", runAttempt: 1,
  lease: { environment: "staging", purpose: "automod-preview", phase: "active", runId: "12345", runAttempt: 1,
    deploymentId: "22222222-2222-4222-8222-222222222222", sha: "a".repeat(40), epoch: 17,
    acquiredAt: now - 500, expiresAt: now + 120_000 },
} as const;
const serving = {
  workerDeploymentId: "11111111-1111-4111-8111-111111111111", workerVersionId: scope.lease.deploymentId,
  sourceSha: scope.lease.sha, containerApplicationId: "33333333-3333-4333-8333-333333333333",
  containerNamespaceId: "b".repeat(32), containerRolloutId: "44444444-4444-4444-8444-444444444444",
  containerImageDigest: `sha256:${"c".repeat(64)}`, ownerEpoch: scope.lease.epoch,
};
const identity = { ...serving, ownerPhase: "active", trafficPercent: 100, runtimePolicy: "bounded",
  rolloutStatus: "completed" };
const command = { id: "1212345678901234567", version: "1234567890123456789" };
const stopped = { lease: "absent", container: "stopped" };
const receipt = {
  schema: "two-bot-staging-automod-qualification.v1",
  before: { scope, identity, observedAt: now - 400 },
  registry: {
    scope, startedAt: now - 300, completedAt: now - 200,
    command: { ...command, ...STAGING_AUTOMOD_TARGET, type: 1, default_member_permissions: "32", options: [
      { name: "text", type: 3, required: true, autocomplete: false, choiceCount: 0, min_length: 1, max_length: 2000 },
      { name: "mentions", type: 4, required: false, autocomplete: false, choiceCount: 0, min_value: 0, max_value: 100 },
      { name: "attachments", type: 3, required: false, autocomplete: false, choiceCount: 0, max_length: 1000 },
    ] },
  },
  after: { scope, identity, observedAt: now - 100 },
  router: { sourceSha: scope.lease.sha, name: "automod-preview", requiredPermission: 32,
    withManageGuild: "admitted", withoutManageGuild: "denied" },
  coldStartRefusal: { scope, action: "start", startedAt: now - 700, completedAt: now - 600,
    response: { schema: "two-bot-staging-automod-job.v1", environment: "staging",
      observedAt: new Date(now - 650).toISOString(), verdict: "failed", code: "request_refused" },
    before: stopped, after: stopped,
    effects: { leaseWrites: 0, containerAcquisitions: 0, containerInvocations: 0, containerStarts: 0,
      containerStops: 0, containerWrites: 0 } },
};
const rehearsal = { verdict: "passed", start: true, recover: true, runId: scope.runId, sha: scope.lease.sha,
  deploymentId: scope.lease.deploymentId };
const sentinel = "synthetic private material";

function rejects(evidence: unknown, code: StagingAutomodRedactionFailureCode, secret = sentinel): void {
  const result = verify({ enabled: true, evidence });
  assert.deepEqual(result, { verdict: "failed", code });
  assert.ok(Object.isFrozen(result));
  assert.ok(!JSON.stringify(result).includes(secret), "the verdict must not expose input");
}

test("defaults to disabled without touching evidence or invoking accessors", () => {
  const expected = { verdict: "disabled" };
  assert.deepEqual(verify(), expected);
  let reads = 0;
  for (const enabled of [undefined, false, null, 1, "true"]) {
    const result = verify({ enabled, get evidence() { reads++; throw Error(sentinel); } });
    assert.deepEqual(result, expected);
    assert.ok(Object.isFrozen(result));
  }
  assert.deepEqual(verify({ get enabled() { reads++; return true; } }), expected);
  assert.deepEqual(verify(Object.create({ enabled: true })), expected);
  assert.equal(reads, 0);
});

test("accepts the existing qualification receipt and composed evidence bundle", () => {
  const input = {
    enabled: true, pin: { runId: scope.runId, lease: scope.lease, serving, command }, receipt, rehearsal,
  };
  const bundle = composeStagingAutomodQualificationBundle(input, now);
  const diagnostic = composeStagingAutomodQualificationBundle(input, NaN);
  assert.equal(bundle.verdict, "passed");
  assert.equal(diagnostic.verdict, "failed");
  assert.equal(diagnostic.observedAt, null);
  for (const evidence of [receipt, bundle, diagnostic, { receipt, bundle, rehearsal },
    JSON.parse(JSON.stringify({ receipt, bundle, rehearsal }))]) {
    const result = verify({ enabled: true, evidence });
    assert.deepEqual(result, { verdict: "passed" });
    assert.ok(Object.isFrozen(result));
  }
});

test("rejects credential patterns in strings and keys without echoing them", () => {
  const opaque = "synthetic-credential";
  const credentials = [
    `bEaReR ${opaque}`, `Authorization: Basic ${opaque}`, `Proxy-Authorization: Digest ${opaque}`,
    `Bot ${opaque}`, ["A".repeat(24), "B".repeat(6), "C".repeat(30)].join("."), `mfa.${"d".repeat(60)}`,
    ...["ghp", "ghs", "gho", "ghu", "github_pat"].map(prefix => `${prefix}_${"e".repeat(40)}`),
    "Cloudflare API Token=" + opaque, "X-Auth-Key: " + opaque, "f".repeat(37), "z".repeat(40),
    ["v1.0", "k".repeat(24), "m".repeat(146)].join("-"),
    ["-----BEGIN", "OPENSSH PRIVATE KEY-----", sentinel].join(" "),
    "postgresql://" + opaque + ":db-value@db.invalid/bot",
    "postgres://" + opaque + "@db.invalid/bot",
    "postgresql://db.invalid/bot?%70assword=" + opaque,
  ];
  for (const credential of credentials) {
    rejects({ ...receipt, router: { ...receipt.router, name: credential } }, "secret_value", credential);
    rejects({ [credential]: true }, "secret_value", credential);
  }
  rejects({ sha: "z".repeat(40) }, "secret_value", "z".repeat(40));
});

test("rejects sensitive key names and credential-bearing connection fields", () => {
  for (const key of ["token", "secret", "password", "Authorization", "cookie", "apiKey", "private_key",
    "DISCORD_BOT_TOKEN", "CLOUDFLARE_API_TOKEN", "CLOUDFLARE_API_KEY", "X-Auth-Key"]) {
    rejects({ receipt: { ...receipt, [key]: sentinel } }, "sensitive_key");
  }
  rejects({ databaseUrl: "postgresql://operator:" + sentinel.replaceAll(" ", "-") + "@db.invalid" }, "sensitive_key");
  rejects({ "D-S-N": "host=db.invalid password='" + sentinel + "'" }, "sensitive_key");
  rejects({ dsn: "postgresql://db.invalid/bot" }, "unexpected_field");
});

test("rejects raw member content and metadata in the wrong place", () => {
  for (const key of ["content", "message", "text", "attachments", "mentions", "member", "payload", "description",
    "options", "name", "toJSON", "__proto__"]) {
    rejects({ ...receipt, [key]: sentinel }, "unexpected_field");
  }
  rejects({ receipt: { ...receipt, before: { ...receipt.before, content: sentinel } } }, "unexpected_field");
  rejects({ router: { ...receipt.router, name: sentinel } }, "invalid_metadata");
  rejects({ command: sentinel }, "invalid_metadata");
});

test("rejects non-JSON values without running their hooks", () => {
  let reads = 0;
  const circular: { receipt?: unknown } = {};
  circular.receipt = circular;
  const accessor = { get sha() { reads++; throw Error(sentinel); } };
  const proxy = new Proxy({}, { ownKeys() { reads++; throw Error(sentinel); } });
  const hidden = Object.defineProperty({}, "sha", { value: sentinel });
  const symbol = { [Symbol(sentinel)]: true };
  const sparse = Array(1);
  const extraArrayKey = Object.assign([1], { extra: sentinel });
  for (const value of [undefined, () => sentinel, 1n, NaN, Infinity, new Date(), circular, accessor, proxy,
    hidden, symbol, sparse, extraArrayKey, Object.create({ sha: scope.lease.sha })]) {
    rejects({ receipt: value }, "non_json_value");
  }
  assert.equal(reads, 0);
  rejects(undefined, "non_json_value");
  rejects({}, "unexpected_field");
});

test("bounds nesting, cumulative keys and string length", () => {
  let nested: unknown = sentinel;
  for (let depth = 0; depth < 14; depth++) nested = { receipt: nested };
  rejects(nested, "depth_limit");
  rejects(Object.fromEntries(Array.from({ length: 513 }, (_, index) => [String(index), true])), "key_limit");
  rejects({ receipt, bundle: Array.from({ length: 450 }, () => receipt.router) }, "key_limit");
  rejects({ bundle: Array(513) }, "key_limit");
  rejects({ sha: "x".repeat(4097) + sentinel }, "string_limit");
  rejects({ ["x".repeat(4097) + sentinel]: true }, "string_limit");
});
