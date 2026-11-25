import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { test } from "node:test";
import { DEPLOYMENT_HEADER } from "../src/ownership.ts";
import { runProtectedAutomod, type ProtectedAutomodCall, type ProtectedAutomodEffects } from "../src/staging-automod-caller.ts";
import { invokeProtectedAutomodJob } from "../src/staging-automod-entry.ts";
import { STAGING_AUTOMOD_TARGET as target, StagingAutomodRecovery } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE, MAX_STAGING_USE_MS, STAGING_USE_KEY, StagingLifecycle } from "../src/staging-lifecycle.ts";
import publication from "./fixtures/staging-automod-publication.json" with { type: "json" };
import { admission, intent, lease, now } from "./fixtures/staging-automod-recovery.ts";

type Inputs = Parameters<typeof invokeProtectedAutomodJob>;
type Approval = NonNullable<Inputs[2]>;
const token = "synthetic-automod-staging-control-token";
const issuedLease: Approval["issuedLease"] = { ...lease, purpose: AUTOMOD_STAGING_PURPOSE };
const env = { ...publication.env, TWO_DEPLOY_ENV: "staging", STAGING_RUNTIME_POLICY: "bounded",
  OWNERSHIP_CONTROL_TOKEN: token, CF_VERSION_METADATA: { id: lease.deploymentId }, CONTAINER_RELEASE: lease.sha };
const request = (body: BodyInit, authorization = `Bearer ${token}`) =>
  new Request("https://worker.invalid", { method: "POST", headers: { authorization }, body });
const receipt = () => ({ schema: "two-bot-staging-automod-job.v1", environment: "staging",
  observedAt: new Date().toISOString() });
const refused = () => ({ ...receipt(), verdict: "failed", code: "request_refused" });

test("private AutoMod entry denies unauthenticated or mismatched work before invoking the caller", async t => {
  t.mock.timers.enable({ apis: ["Date"], now: now() });
  const registry = t.mock.fn(async () => {});
  const invoke = t.mock.fn<Inputs[3]>(async () => { await registry(); });
  const start = { action: "start", issuedLease } as const;
  const approval = { ...start, target };
  const raw = JSON.stringify(start);

  const unopened: [string, Request, Inputs[1], Inputs[2]][] = [
    ["missing bearer", request(raw, ""), env, approval],
    ["wrong bearer", request(raw, "Bearer wrong"), env, approval],
    ["unconfigured bearer", request(raw), { ...env, OWNERSHIP_CONTROL_TOKEN: "" }, approval],
    ["approval only in the body", request(JSON.stringify({ ...start, approval })), env, undefined],
    ["non-POST", new Request("https://worker.invalid"), env, approval],
    ["production Worker", request(raw), { ...env, TWO_DEPLOY_ENV: "production" }, approval],
    ["unbounded Worker", request(raw), { ...env, STAGING_RUNTIME_POLICY: "unbounded" }, approval],
    ["wrong Worker application", request(raw), { ...env, DISCORD_APPLICATION_ID: "other" }, approval],
    ["wrong Worker guild", request(raw), { ...env, GUILD_ID: "other" }, approval],
  ];
  for (const [name, req, worker, approved] of unopened) {
    assert.deepEqual(await invokeProtectedAutomodJob(req, worker, approved, invoke), refused(), name);
    assert.equal(req.bodyUsed, false, name);
  }

  const spoofed = request(raw);
  spoofed.headers.set(DEPLOYMENT_HEADER, lease.deploymentId);
  const denied: [string, Request, Inputs[1], Inputs[2]][] = [
    ["wrong Worker deployment", spoofed, { ...env, CF_VERSION_METADATA: { id: "other" } }, approval],
    ["wrong Worker build", request(raw), { ...env, CONTAINER_RELEASE: "b".repeat(40) }, approval],
    ["wrong approved application", request(raw), env, { ...approval, target: { ...target, applicationId: "other" } }],
    ["wrong approved guild", request(raw), env, { ...approval, target: { ...target, guildId: "other" } }],
    ["different action", request(raw), env, { ...approval, action: "recover" }],
    ["malformed JSON", request("{"), env, approval],
    ["duplicate action", request(raw.replace("{", '{"action":"recover",')), env, approval],
    ["duplicate lease field", request(raw.replace('"runAttempt":', '"runAttempt":999,"runAttempt":')), env, approval],
    ["oversized body", request(raw + " ".repeat(1024)), env, approval],
    ["invalid UTF-8", request(new Uint8Array([0xff, ...new TextEncoder().encode(raw)])), env, approval],
  ];
  for (const [name, req, worker, approved] of denied) {
    assert.deepEqual(await invokeProtectedAutomodJob(req, worker, approved, invoke), refused(), name);
  }
  for (const changed of [{ runId: "12346" }, { runAttempt: lease.runAttempt + 1 }, { epoch: lease.epoch + 1 },
    { deploymentId: "other" }, { sha: "b".repeat(40) }, { acquiredAt: lease.acquiredAt + 1 },
    { expiresAt: lease.expiresAt + 1 }, { purpose: "voice" }, { purpose: undefined }, { voiceMove: true },
    { phase: "released" }, { environment: "production" }]) {
    const req = request(JSON.stringify({ ...start, issuedLease: { ...issuedLease, ...changed } }));
    assert.deepEqual(await invokeProtectedAutomodJob(req, env, approval, invoke), refused(), JSON.stringify(changed));
  }
  const tooLong = { ...issuedLease, expiresAt: lease.acquiredAt + MAX_STAGING_USE_MS + 1 };
  assert.deepEqual(await invokeProtectedAutomodJob(request(JSON.stringify({ ...start, issuedLease: tooLong })),
    env, { ...approval, issuedLease: tooLong }, invoke), refused());
  t.mock.timers.setTime(lease.expiresAt);
  assert.deepEqual(await invokeProtectedAutomodJob(request(raw), env, approval, invoke), refused());
  assert.equal(invoke.mock.callCount(), 0);
  assert.equal(registry.mock.callCount(), 0);

  t.mock.timers.setTime(now());
  assert.deepEqual(await invokeProtectedAutomodJob(request(raw), env, approval, invoke),
    { ...receipt(), action: "start", lease: issuedLease, verdict: "passed" });
  assert.deepEqual(invoke.mock.calls.map(call => call.arguments), [[start]]);
});

test("private AutoMod entry admits original expired lease for recovery and retirement retry", async t => {
  t.mock.timers.enable({ apis: ["Date"], now: now() });
  const values = new Map<string, unknown>([[STAGING_USE_KEY, issuedLease]]);
  let failRetirement = true;
  const storage = {
    get: async (key: string) => structuredClone(values.get(key)),
    put: async (key: string | Record<string, unknown>, value?: unknown) => {
      const entries = typeof key === "string" ? { [key]: value } : key;
      if (failRetirement && Object.hasOwn(entries, STAGING_USE_KEY)) throw new Error("synthetic retirement write failure");
      for (const [name, entry] of Object.entries(entries)) values.set(name, structuredClone(entry));
    },
    sync: async () => {},
  } as unknown as DurableObjectStorage;
  const journal = new StagingAutomodRecovery(storage);
  const staging = new StagingLifecycle(storage);
  await journal.persistIntent({ ...intent, lease: issuedLease, baseline: createHash("sha256").update("[]").digest("hex") },
    async () => ({ ...admission, lease: issuedLease }));
  const owner = { deploymentId: lease.deploymentId, epoch: lease.epoch, phase: "active" as const,
    oldEpoch: lease.epoch - 1, oldDeploymentId: null, actor: "fixture", timestamp: new Date().toISOString() };
  const authority = { storage, staging, env,
    identity: async () => ({ deploymentId: lease.deploymentId, sha: lease.sha, owner }),
    exclusive: async <T>(operation: () => Promise<T>) => operation() };
  const cleanupBefore = lease.expiresAt + 5_000;
  const effects = {
    enabled: false,
    readRuntime: t.mock.fn(async scope => ({ lease: scope, running: false })),
    readCleanupGrant: t.mock.fn(async scope => ({ lease: scope, before: cleanupBefore })),
    fenceAndDrain: t.mock.fn(async scope => ({ lease: scope, publicationDrained: true, startupDrained: true })),
    stop: t.mock.fn(async () => true),
    request: t.mock.fn(async (url, init) => {
      assert.equal(init.method, "GET");
      if (url === "https://discord.com/api/v10/users/@me") return Response.json(publication.identity);
      assert.equal(url, `https://discord.com/api/v10/applications/${target.applicationId}/guilds/${target.guildId}/commands?with_localizations=true`);
      return Response.json([]);
    }),
  } satisfies ProtectedAutomodEffects;
  const invoke = t.mock.fn<Inputs[3]>(call => runProtectedAutomod(call, authority, effects));
  const recover = { action: "recover", issuedLease } as const;
  const approval = { ...recover, target };
  const submit = (body: ProtectedAutomodCall = recover, approved: Approval = approval) =>
    invokeProtectedAutomodJob(request(JSON.stringify(body)), env, approved, invoke);
  const scoped = () => ({ ...receipt(), action: "recover", lease: issuedLease });

  t.mock.timers.setTime(lease.expiresAt + 1);
  const other = { ...recover, issuedLease: { ...issuedLease, runAttempt: lease.runAttempt + 1 } };
  assert.deepEqual(await submit(other), refused());
  assert.equal(invoke.mock.callCount(), 0);
  assert.deepEqual(await submit(other, { ...other, target }),
    { ...scoped(), lease: other.issuedLease, verdict: "failed", code: "operation_unconfirmed" });
  assert.equal(effects.readCleanupGrant.mock.callCount(), 0);
  assert.equal(effects.readRuntime.mock.callCount(), 0);
  assert.equal(effects.fenceAndDrain.mock.callCount(), 0);
  assert.equal(effects.stop.mock.callCount(), 0);
  assert.equal(effects.request.mock.callCount(), 0);
  assert.deepEqual(await staging.read(), issuedLease);

  assert.deepEqual(await submit(), { ...scoped(), verdict: "failed", code: "operation_unconfirmed" });
  assert.equal(await journal.completedFor(issuedLease), true);
  assert.deepEqual(await staging.read(), issuedLease);
  assert.equal(effects.fenceAndDrain.mock.callCount(), 1);
  assert.equal(effects.stop.mock.callCount(), 1);
  assert.equal(effects.request.mock.callCount(), 2);

  failRetirement = false;
  t.mock.timers.setTime(cleanupBefore + 1);
  assert.deepEqual(await submit(), { ...scoped(), verdict: "passed" });
  assert.deepEqual(await staging.read(), { ...issuedLease, phase: "released" });
  assert.equal(effects.fenceAndDrain.mock.callCount(), 1);
  assert.equal(effects.stop.mock.callCount(), 1);
  assert.equal(effects.request.mock.callCount(), 2);
});
