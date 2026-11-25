import assert from "node:assert/strict";
import { test } from "node:test";
import { runProtectedAutomod, type ProtectedAutomodEffects } from "../src/staging-automod-caller.ts";
import { runProtectedAutomodJob } from "../src/staging-automod-job.ts";
import { STAGING_AUTOMOD_TARGET, StagingAutomodRecovery } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE, MAX_STAGING_USE_MS, STAGING_USE_KEY, StagingLifecycle } from "../src/staging-lifecycle.ts";
import publication from "./fixtures/staging-automod-publication.json" with { type: "json" };
import { intent, lease, now, observe } from "./fixtures/staging-automod-recovery.ts";

type Approval = NonNullable<Parameters<typeof runProtectedAutomodJob>[1]>;
type Lease = Approval["issuedLease"];
const issued = { ...lease, purpose: AUTOMOD_STAGING_PURPOSE } as const;
const target = STAGING_AUTOMOD_TARGET;
const approve = (action: Approval["action"], issuedLease: Lease = issued) => ({ action, issuedLease, target });
const { env, identity, replies: { help, preview } } = publication;
const policy = { memberIntake: false, enforcement: false, resume: false, registrySync: false,
  interactionDispatch: false, dedicatedIntents: true };

test("AutoMod job refuses malformed, stale and mismatched requests before handoff", async t => {
  t.mock.timers.enable({ apis: ["Date"], now: now() });
  const invoke = t.mock.fn(async () => {});
  const request = { action: "start", issuedLease: issued };
  const approved = approve("start");
  const refused = { schema: "two-bot-staging-automod-job.v1", environment: "staging",
    observedAt: new Date().toISOString(), verdict: "failed", code: "request_refused" };

  for (const bad of [null, [], {}, { ...request, token: "must-not-echo" }, { ...request, action: "publish" },
    { ...request, action: "recover" }, { ...request, issuedLease: null }, { ...request, issuedLease: {} }]) {
    assert.deepEqual(await runProtectedAutomodJob(bad, approved, invoke), refused);
  }
  for (const change of [{ voiceMove: true }, { purpose: undefined }, { phase: "released" }, { environment: "production" },
    { runId: "12346" }, { runAttempt: 3 }, { sha: "f".repeat(40) }, { deploymentId: "other" }, { epoch: 4 },
    { acquiredAt: issued.acquiredAt + 1 }, { expiresAt: issued.expiresAt + 1 }, { token: "must-not-echo" }]) {
    assert.deepEqual(await runProtectedAutomodJob({ ...request, issuedLease: { ...issued, ...change } }, approved, invoke), refused);
  }
  for (const invalid of [{ ...issued, runId: "0" }, { ...issued, acquiredAt: now() + 1 },
    { ...issued, expiresAt: issued.acquiredAt + MAX_STAGING_USE_MS + 1 }]) {
    assert.deepEqual(await runProtectedAutomodJob({ ...request, issuedLease: invalid }, approve("start", invalid), invoke), refused);
  }
  for (const scope of [undefined, { ...approved, target: { ...target, guildId: "other" } },
    { ...approved, target: { ...target, applicationId: "other" } }]) {
    assert.deepEqual(await runProtectedAutomodJob(request, scope, invoke), refused);
  }
  t.mock.timers.setTime(issued.expiresAt);
  assert.deepEqual(await runProtectedAutomodJob(request, approved, invoke), { ...refused, observedAt: new Date().toISOString() });
  assert.equal(invoke.mock.callCount(), 0);
});

test("AutoMod job scopes startup and cleanup receipts to the existing caller", async t => {
  t.mock.timers.enable({ apis: ["Date"], now: now() });
  const values = new Map<string, unknown>([[STAGING_USE_KEY, issued]]);
  const storage = {
    get: async (key: string) => structuredClone(values.get(key)),
    put: async (key: string | Record<string, unknown>, value?: unknown) => {
      for (const [name, entry] of Object.entries(typeof key === "string" ? { [key]: value } : key)) {
        values.set(name, structuredClone(entry));
      }
    },
    sync: async () => {},
  } as unknown as DurableObjectStorage;
  const journal = new StagingAutomodRecovery(storage);
  const owned = { intent, id: preview.id, version: preview.version };
  await journal.persistIntent(intent, observe);
  await journal.recordCreated(owned);
  await journal.confirmPublication(owned);

  const staging = new StagingLifecycle(storage);
  const owner = { deploymentId: issued.deploymentId, epoch: issued.epoch, phase: "active" as const,
    oldEpoch: issued.epoch - 1, oldDeploymentId: null, actor: "fixture", timestamp: new Date().toISOString() };
  const authority = { storage, staging, env: { ...env, STAGING_RUNTIME_POLICY: "bounded" },
    identity: async () => ({ deploymentId: issued.deploymentId, sha: issued.sha, owner }),
    exclusive: async <T>(operation: () => Promise<T>) => operation() };
  const calls: string[] = [];
  const other = { ...issued, runAttempt: issued.runAttempt + 1 };
  const cleanupBefore = issued.expiresAt + 5_000;
  const commands = `https://discord.com/api/v10/applications/${target.applicationId}/guilds/${target.guildId}/commands`;
  let running = false;
  let present = true;
  let failure: "stale-grant" | "other-grant" | "other-drain" | "stop-refused" | "still-running" | "other-revision" | undefined;
  const effects: ProtectedAutomodEffects = {
    enabled: true,
    readRuntime: async scope => {
      assert.deepEqual(scope, issued);
      return { lease: scope, running, policy };
    },
    dryRun: { policy, start: async (scope, options, before) => {
      assert.deepEqual(scope, issued);
      assert.deepEqual(options, policy);
      assert.equal(before, intent.publishBefore);
      calls.push("start");
      running = true;
    } },
    readCleanupGrant: async scope => {
      assert.deepEqual(scope, issued);
      return { lease: failure === "other-grant" ? other : scope,
        before: failure === "stale-grant" ? Date.now() : cleanupBefore };
    },
    fenceAndDrain: async (scope, before) => {
      assert.deepEqual(scope, issued);
      assert.equal(before, cleanupBefore);
      calls.push("drain");
      return { lease: failure === "other-drain" ? other : scope, publicationDrained: true, startupDrained: true };
    },
    stop: async (scope, before) => {
      assert.deepEqual(scope, issued);
      assert.equal(before, cleanupBefore);
      calls.push("stop");
      if (failure === "stop-refused") return false;
      if (failure !== "still-running") running = false;
      return true;
    },
    request: async (url, init) => {
      assert.equal(running, false);
      if (init.method === "DELETE") {
        calls.push("delete");
        assert.equal(url, `${commands}/${preview.id}`);
        present = false;
        return new Response(null, { status: 204 });
      }
      assert.equal(init.method, "GET");
      calls.push("read");
      if (url === "https://discord.com/api/v10/users/@me") return Response.json(identity);
      if (url === `${commands}?with_localizations=true`) return Response.json(present ? [help, preview] : [help]);
      assert.equal(url, `${commands}/${preview.id}`);
      if (failure === "other-revision") return Response.json({ ...preview, version: help.version });
      return present ? Response.json(preview) : Response.json({ code: 10063 }, { status: 404 });
    },
  };
  const run = (action: Approval["action"], scope: Lease = issued) =>
    runProtectedAutomodJob({ action, issuedLease: scope }, approve(action, scope), call => runProtectedAutomod(call, authority, effects));
  const receipt = (action: Approval["action"], scope: Lease = issued) => ({ schema: "two-bot-staging-automod-job.v1",
    environment: "staging", observedAt: new Date().toISOString(), action, lease: scope });
  const unconfirmed = { verdict: "failed", code: "operation_unconfirmed" };

  assert.deepEqual(await run("start", other), { ...receipt("start", other), ...unconfirmed });
  assert.deepEqual(calls, []);
  assert.deepEqual(await run("start"), { ...receipt("start"), verdict: "passed" });
  assert.deepEqual(calls.splice(0), ["start"]);
  assert.equal(running, true);

  t.mock.timers.setTime(issued.expiresAt + 1);
  for (const [caseName, expected] of [["stale-grant", []], ["other-grant", []], ["other-drain", ["drain"]],
    ["stop-refused", ["drain", "stop"]], ["still-running", ["drain", "stop"]],
    ["other-revision", ["drain", "stop", "read", "read", "read"]]] as const) {
    failure = caseName;
    assert.deepEqual(await run("recover"), { ...receipt("recover"), ...unconfirmed }, caseName);
    assert.deepEqual(calls.splice(0), expected, caseName);
    assert.equal(await journal.completedFor(issued), false, caseName);
    assert.deepEqual(await staging.read(), issued, caseName);
  }
  failure = undefined;
  assert.deepEqual(await run("recover"), { ...receipt("recover"), verdict: "passed" });
  assert.deepEqual(calls.filter(call => call !== "read"), ["drain", "stop", "delete"]);
  assert.equal(running, false);
  assert.equal(present, false);
  assert.equal(await journal.completedFor(issued), true);
  assert.deepEqual(await staging.read(), { ...issued, phase: "released" });
});
