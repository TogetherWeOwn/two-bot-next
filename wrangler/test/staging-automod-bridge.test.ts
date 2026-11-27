import assert from "node:assert/strict";
import { generateKeyPairSync, sign } from "node:crypto";
import { test } from "node:test";
import { readControlBody } from "../src/ownership.ts";
import { STAGING_AUTOMOD_BODY_BYTES, operationRun, parseStagingAutomodOperation,
  verifyStagingAutomodOidc } from "../src/staging-automod-bridge.ts";
import { STAGING_AUTOMOD_PLAN, automodRecoverAllowed, automodReservationAllowed,
  automodStartAllowed } from "../src/staging-automod-plan.ts";
import { STAGING_AUTOMOD_TARGET } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE } from "../src/staging-use-window.ts";

const now = 1_800_000_000_000;
const scope = { runId: "123456", runAttempt: 1, sha: "a".repeat(40) } as const;
const context = { ...scope, environment: "staging", deploymentId: "version" } as const;
const issuedLease = { ...context, phase: "active", purpose: AUTOMOD_STAGING_PURPOSE,
  epoch: 1, acquiredAt: now, expiresAt: now + 60_000 } as const;
const change = { ...context, action: "acquire", expiresAt: issuedLease.expiresAt, expectedEpoch: 1 } as const;
const pin = { workerDeploymentId: "deployment", workerVersionId: "version", sourceSha: scope.sha,
  applicationId: "application", namespaceId: "namespace", rolloutId: "rollout", imageDigest: "sha256:fixture" };

test("AutoMod protocol verifies the signed run, release and required GitHub claims", async t => {
  const key = generateKeyPairSync("rsa", { modulusLength: 2048 });
  const otherKey = generateKeyPairSync("rsa", { modulusLength: 2048 }).privateKey;
  const issuer = "https://token.actions.githubusercontent.com";
  const seconds = now / 1000;
  const claims = {
    iss: issuer, aud: "two-bot-staging-automod", repository_id: "1396224242", ref: "refs/heads/main",
    ref_protected: "true", event_name: "workflow_dispatch", run_id: scope.runId, run_attempt: "1",
    environment: "staging", sha: scope.sha, exp: seconds + 60, nbf: seconds, iat: seconds,
    workflow_ref: "TogetherWeOwn/two-bot-next/.github/workflows/staging-automod-preview.yml@refs/heads/main",
  };
  const encode = (value: unknown) => Buffer.from(JSON.stringify(value)).toString("base64url");
  const token = (overrides = {}, signingKey = key.privateKey, headers = {}) => {
    const input = `${encode({ alg: "RS256", kid: "fixture", ...headers })}.${encode({ ...claims, ...overrides })}`;
    return `${input}.${sign("sha256", Buffer.from(input), signingKey).toString("base64url")}`;
  };
  const jwk = { ...key.publicKey.export({ format: "jwk" }), kid: "fixture", alg: "RS256", use: "sig" };
  t.mock.method(globalThis, "fetch", async (url: string, init?: RequestInit) => {
    assert.equal(url, `${issuer}/.well-known/jwks`);
    assert.equal(init?.redirect, "error");
    return Response.json({ keys: [jwk] });
  });
  const verify = (value: string | null) => verifyStagingAutomodOidc(value, scope.sha, new AbortController().signal, now);
  assert.deepEqual(await verify(token()), scope);
  for (const claim of ["iss", "aud", "repository_id", "ref", "ref_protected", "event_name",
    "run_attempt", "environment", "workflow_ref", "sha", "run_id"]) {
    assert.equal(await verify(token({ [claim]: "wrong" })), undefined, claim);
  }
  for (const invalid of [null, "not-a-token", token({}, otherKey), token({}, key.privateKey, { alg: "HS256" }),
    token({}, key.privateKey, { kid: "unknown" }), token({ exp: seconds - 61, iat: seconds - 120 }),
    token({ nbf: seconds + 61 }), token({ iat: seconds + 61, exp: seconds + 120 })]) {
    assert.equal(await verify(invalid), undefined);
  }
});

test("AutoMod protocol accepts only its closed run-scoped operation shapes", () => {
  const operations = [
    { op: "acquire", pin, change },
    { op: "read-lease", context: { ...context, target: STAGING_AUTOMOD_TARGET } },
    { op: "read-approval", action: "recover", issuedLease },
    { op: "invoke", pin, call: { action: "start", issuedLease } },
  ];
  for (const original of operations) {
    const parsed = parseStagingAutomodOperation(original);
    assert.deepEqual(parsed, original);
    assert.ok(parsed && parsed !== "unknown");
    const { runId, runAttempt, sha } = operationRun(parsed);
    assert.deepEqual({ runId, runAttempt, sha }, scope);
    assert.equal(parseStagingAutomodOperation({ ...original, extra: true }), undefined);
  }
  assert.equal(parseStagingAutomodOperation({ op: "publish" }), "unknown");
  assert.equal(parseStagingAutomodOperation({ op: "constructor" }), "unknown");
  assert.equal(parseStagingAutomodOperation({ ...operations[0], change: { ...change, voiceMove: true } }), undefined);
  assert.equal(parseStagingAutomodOperation({ ...operations[2], issuedLease: { ...issuedLease, purpose: "other" } }),
    undefined);
});

test("AutoMod plan denies reservation and start by default and preserves same-run recovery", () => {
  const active = { ...STAGING_AUTOMOD_PLAN, enabled: true, allowStart: true, notAfter: issuedLease.expiresAt };
  assert.equal(automodReservationAllowed(active, scope, change, now), true);
  assert.equal(automodStartAllowed(active, scope, issuedLease, now), true);
  assert.equal(automodReservationAllowed(STAGING_AUTOMOD_PLAN, scope, change, now), false);
  assert.equal(automodStartAllowed(STAGING_AUTOMOD_PLAN, scope, issuedLease, now), false);
  assert.equal(automodStartAllowed({ ...active, allowStart: false }, scope, issuedLease, now), false);
  assert.equal(automodStartAllowed(active, scope, issuedLease, active.notAfter), false);
  assert.equal(automodRecoverAllowed(STAGING_AUTOMOD_PLAN, scope, issuedLease), true);
  assert.equal(automodRecoverAllowed(STAGING_AUTOMOD_PLAN, { ...scope, runId: "123457" }, issuedLease), false);
});

test("AutoMod control body limit is explicit and keeps the existing default limit", async () => {
  const request = (size: number) => new Request("https://worker.invalid", {
    method: "POST", body: JSON.stringify("x".repeat(size)), headers: { "content-length": "1" },
  });
  assert.equal(await readControlBody(request(2000)), null);
  const decode = (bytes: Uint8Array) => JSON.parse(new TextDecoder().decode(bytes));
  assert.equal(await readControlBody(request(2000), decode, STAGING_AUTOMOD_BODY_BYTES), "x".repeat(2000));
  assert.equal(await readControlBody(request(4096), decode, STAGING_AUTOMOD_BODY_BYTES), null);
});
