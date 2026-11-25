import assert from "node:assert/strict";
import { test, type TestContext } from "node:test";
import { setImmediate } from "node:timers/promises";
import { runStagingAutomodRehearsal } from "../scripts/staging-automod-rehearsal-caller.ts";
import { runProtectedAutomod, type ProtectedAutomodEffects } from "../src/staging-automod-caller.ts";
import { STAGING_AUTOMOD_TARGET as target, StagingAutomodRecovery } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE, StagingLifecycle, type StagingIdentity } from "../src/staging-lifecycle.ts";
import publication from "./fixtures/staging-automod-publication.json" with { type: "json" };
import { intent, lease, other } from "./fixtures/staging-automod-recovery.ts";

type Providers = Parameters<typeof runStagingAutomodRehearsal>[1];
type Inventory = NonNullable<Awaited<ReturnType<Providers["readInventory"]>>>;
const deploymentId = "11111111-1111-1111-1111-111111111111";
const { environment, sha, epoch, runId } = lease;
const runAttempt = 1;
const context = { environment, deploymentId, sha, runId, runAttempt, target, refProtected: true,
  eventName: "workflow_dispatch", workflowRef: "TogetherWeOwn/two-bot-next/.github/workflows/staging-automod-preview.yml@refs/heads/main" };
const policy = { memberIntake: false, enforcement: false, resume: false, registrySync: false,
  interactionDispatch: false, dedicatedIntents: true };
const { help, preview } = publication.replies;
const commands = `https://discord.com/api/v10/applications/${target.applicationId}/guilds/${target.guildId}/commands`;
const list = `${commands}?with_localizations=true`;
const detail = `${commands}/${preview.id}`;

async function rehearsal(t: TestContext, startup: "lost" | "confirmed" = "lost") {
  t.mock.timers.enable({ apis: ["Date", "setTimeout"], now: Date.parse("2026-01-01T00:00:00Z") });
  const network = t.mock.method(globalThis, "fetch", async () => assert.fail("unexpected network access"));
  const values = new Map<string, unknown>();
  const storage = {
    get: async (key: string) => structuredClone(values.get(key)),
    put: async (entries: string | Record<string, unknown>, value?: unknown) => {
      const write = typeof entries === "string" ? { [entries]: value } : entries;
      for (const [key, record] of Object.entries(structuredClone(write))) values.set(key, record);
    },
    sync: async () => {},
  } as unknown as DurableObjectStorage;
  const staging = new StagingLifecycle(storage);
  const journal = new StagingAutomodRecovery(storage);
  const identity: StagingIdentity = { deploymentId, sha, owner: { deploymentId, epoch, phase: "active",
    actor: "synthetic", timestamp: new Date().toISOString(), oldEpoch: epoch - 1, oldDeploymentId: null } };
  const issuedLease = await staging.acquire({ environment, action: "acquire", runId, runAttempt,
    expiresAt: Date.now() + 30_000, deploymentId, sha, expectedEpoch: epoch }, identity,
  async () => {}, async () => {}, Date.now(), AUTOMOD_STAGING_PURPOSE);

  // Publication precedes the rehearsal. Seed its verified receipt through the
  // same journal API used by the existing protected caller fixtures.
  const publishBefore = issuedLease.expiresAt - 5_000;
  const published = { ...intent, lease: issuedLease, publishBefore };
  await journal.persistIntent(published, async () => ({ lease: issuedLease, publishBefore, baselineAbsent: true }));
  const receipt = { intent: published, id: preview.id, version: preview.version };
  await journal.recordCreated(receipt);
  await journal.confirmPublication(receipt);
  const publicationHold = await journal.pending();
  assert.ok(publicationHold);

  let running = false;
  let command: typeof preview | undefined = preview;
  const effectsTaken: string[] = [];
  const env = { ...publication.env, STAGING_RUNTIME_POLICY: "bounded", CONTAINER_RELEASE: sha,
    CF_VERSION_METADATA: { id: deploymentId }, OWNERSHIP_CONTROL_TOKEN: "synthetic".repeat(4) };
  const authority = { storage, env, staging, identity: async () => identity,
    // All callee actions in the fixture are awaited serially.
    exclusive: <T>(operation: () => Promise<T>) => operation() };
  const effects: ProtectedAutomodEffects = {
    enabled: true,
    dryRun: { policy, start: async () => { effectsTaken.push("start"); running = true; } },
    readRuntime: async lease => ({ lease, running, policy }),
    readCleanupGrant: async lease => ({ lease, before: issuedLease.expiresAt + 60_000 }),
    fenceAndDrain: async lease => {
      effectsTaken.push("drain");
      return { lease, publicationDrained: true, startupDrained: true };
    },
    stop: async () => { effectsTaken.push("stop"); running = false; return true; },
    request: async (url, init) => {
      if (init.method === "DELETE") {
        assert.equal(url, detail);
        assert.equal(running, false);
        assert.deepEqual(command, preview);
        effectsTaken.push("delete");
        command = undefined;
        return new Response(null, { status: 204 });
      }
      assert.equal(init.method, "GET");
      if (url === "https://discord.com/api/v10/users/@me") return Response.json(publication.identity);
      if (url === list) return Response.json(command ? [help, command] : [help]);
      assert.equal(url, detail);
      if (command) return Response.json(command);
      effectsTaken.push("absent");
      return Response.json({ code: 10063 }, { status: 404 });
    },
  };

  const pin = { workerDeploymentId: "22222222-2222-2222-2222-222222222222", workerVersionId: deploymentId,
    sourceSha: sha, applicationId: "33333333-3333-3333-3333-333333333333", namespaceId: "a".repeat(32),
    rolloutId: "44444444-4444-4444-4444-444444444444", imageDigest: `sha256:${"b".repeat(64)}` };
  const observe = (): Inventory => ({ schema_version: 1, observed_at: new Date().toISOString(),
    worker: { deployment_id: pin.workerDeploymentId, version_id: pin.workerVersionId, source_sha: pin.sourceSha,
      traffic_percent: 100, runtime_policy: "bounded" },
    container: { application_id: pin.applicationId, namespace_id: pin.namespaceId, rollout_id: pin.rolloutId,
      rollout_status: "completed", image_digest: pin.imageDigest },
    ownership: { phase: "active", epoch },
  });

  let recoveryReachable = true;
  const invoke = t.mock.fn<Providers["invoke"]>(async (_pin, call) => {
    if (call.action === "recover" && !recoveryReachable) throw new Error("synthetic private route unavailable");
    await runProtectedAutomod(call, authority, effects);
    if (call.action === "start" && startup === "lost") {
      t.mock.timers.setTime(issuedLease.expiresAt + 1);
      throw new Error("synthetic private startup result lost after the effect");
    }
  });
  const run = (approveStart: boolean, readInventory: Providers["readInventory"] = async () => observe()) =>
    runStagingAutomodRehearsal({ enabled: true, context }, {
      authority: { readIssuedLease: async () => issuedLease,
        readApproval: async action => action === "recover" || approveStart ? { action, target, issuedLease } : undefined },
      readWorker: () => ({ env, controlBearer: env.OWNERSHIP_CONTROL_TOKEN }),
      readInventory, invoke,
    });
  const assertOriginalCalls = () => {
    assert.deepEqual(invoke.mock.calls.map(({ arguments: [pin, call] }) => ({ pin, call })), [
      { pin, call: { action: "start", issuedLease } }, { pin, call: { action: "recover", issuedLease } },
    ]);
    assert.equal(invoke.mock.calls[0]?.arguments[0], invoke.mock.calls[1]?.arguments[0]);
  };
  const assertHeld = async () => {
    assert.deepEqual(await staging.read(), issuedLease);
    assert.deepEqual(await journal.pending(), publicationHold);
    assert.equal(await journal.completedFor(issuedLease), false);
    assert.equal(running, true);
    assert.deepEqual(command, preview);
    assert.deepEqual(effectsTaken, ["start"]);
    assert.equal(network.mock.callCount(), 0);
  };
  const assertReleased = async () => {
    assert.deepEqual(effectsTaken, ["start", "drain", "stop", "delete", "absent", "absent"]);
    assert.equal(running, false);
    assert.equal(command, undefined);
    assert.equal(await journal.completedFor(issuedLease), true);
    assert.equal(await journal.pending(), undefined);
    assert.deepEqual(await staging.read(), { ...issuedLease, phase: "released" });
    assert.equal(network.mock.callCount(), 0);
  };
  const loseStartupAndHold = async (readInventory?: Providers["readInventory"]) => {
    recoveryReachable = false;
    assert.deepEqual(await run(true, readInventory), { verdict: "failed", start: false, recover: false });
    recoveryReachable = true;
    assertOriginalCalls();
    await assertHeld();
    invoke.mock.resetCalls();
  };
  return { issuedLease, pin, run, observe, invoke, assertOriginalCalls, assertHeld, assertReleased, loseStartupAndHold,
    effects, effectsTaken, publicationHold, journal, staging, network };
}

test("a replacement Worker version cannot recover after the private startup result is lost", async t => {
  const h = await rehearsal(t);
  const readInventory = t.mock.fn<Providers["readInventory"]>(async () => {
    const original = h.observe();
    if (Date.now() < h.issuedLease.expiresAt) return original;
    return { ...original, worker: { ...original.worker, version_id: "55555555-5555-5555-5555-555555555555" } };
  });
  await h.loseStartupAndHold(readInventory);
  assert.equal(readInventory.mock.callCount(), 1);

  // A fresh recovery-only caller has no cached pin. Its independent recovery
  // approval still names the original lease.
  assert.deepEqual(await h.run(false, readInventory), { verdict: "failed", start: false, recover: false });
  assert.equal(readInventory.mock.callCount(), 2);
  assert.equal(h.invoke.mock.callCount(), 0);
  await h.assertHeld();
});

test("a deployment observation arriving after the recovery deadline cannot release the lease", async t => {
  const h = await rehearsal(t);
  await h.loseStartupAndHold();

  const entered = Promise.withResolvers<AbortSignal>();
  const pending = Promise.withResolvers<Inventory>();
  const run = h.run(false, signal => { entered.resolve(signal); return pending.promise; });
  try {
    const signal = await Promise.race([entered.promise, run.then(() => undefined)]);
    assert.ok(signal, "recovery must reach the inventory read");
    assert.equal(signal.aborted, false);
    t.mock.timers.tick(15_000);
    assert.deepEqual(await run, { verdict: "failed", start: false, recover: false });
    assert.equal(signal.aborted, true);
  } finally {
    // The late observation is otherwise fresh and matches the original pin.
    pending.resolve(h.observe());
    await setImmediate();
  }
  assert.equal(h.invoke.mock.callCount(), 0);
  await h.assertHeld();

  // A fresh recovery-only caller with timely evidence for the same pin can
  // reconcile the hold that the timed-out caller left behind.
  assert.deepEqual(await h.run(false), { verdict: "failed", start: false, recover: true });
  assert.deepEqual(h.invoke.mock.calls.map(({ arguments: [pin, call] }) => ({ pin, call })), [
    { pin: h.pin, call: { action: "recover", issuedLease: h.issuedLease } },
  ]);
  await h.assertReleased();
});

test("the original deployment reconciles and releases the original lease after startup result loss", async t => {
  const h = await rehearsal(t);
  const readInventory = t.mock.fn<Providers["readInventory"]>(async () => {
    const original = h.observe();
    if (Date.now() < h.issuedLease.expiresAt) return original;
    // A new Container rollout can keep the same Worker version. Within this
    // caller the recovery still uses the pin frozen before startup.
    return { ...original, container: { ...original.container, rollout_id: "66666666-6666-6666-6666-666666666666" } };
  });

  assert.deepEqual(await h.run(true, readInventory), { verdict: "failed", start: false, recover: true });
  assert.equal(readInventory.mock.callCount(), 1);
  h.assertOriginalCalls();
  await h.assertReleased();
});

test("protected AutoMod requires terminal readback of the exact rehearsed command", async t => {
  for (const terminal of [
    { name: "alternate command in the registry", rows: [help, { ...preview, ...other }], recover: false },
    { name: "no exact command absence", detailCode: 10004, recover: false },
    { name: "clean original command absence", recover: true },
  ]) {
    await t.test(terminal.name, async t => {
      const h = await rehearsal(t, "confirmed");
      const originalRequest = h.effects.request;
      const terminalReads: string[] = [];
      const request = t.mock.method(h.effects, "request", async (url: string, init: RequestInit) => {
        // The delete and its own verification have already succeeded. Vary
        // only the journal's separate terminal read of the scoped registry.
        if (init.method === "GET" && h.effectsTaken.includes("absent")) {
          if (url === list) {
            terminalReads.push(url);
            if (terminal.rows) return Response.json(terminal.rows);
          }
          if (url === detail) {
            terminalReads.push(url);
            if (terminal.detailCode) return Response.json({ code: terminal.detailCode }, { status: 404 });
          }
        }
        return originalRequest(url, init);
      });

      assert.deepEqual(await h.run(true), {
        verdict: terminal.recover ? "passed" : "failed", start: true, recover: terminal.recover,
      });
      h.assertOriginalCalls();
      assert.deepEqual(terminalReads, terminal.rows ? [list] : [list, detail]);
      assert.deepEqual(request.mock.calls.filter(({ arguments: [, init] }) => init.method !== "GET")
        .map(({ arguments: [url, init] }) => [init.method, url]), [["DELETE", detail]]);
      if (terminal.recover) {
        await h.assertReleased();
      } else {
        assert.deepEqual(h.effectsTaken, ["start", "drain", "stop", "delete", "absent"]);
        assert.deepEqual(await h.journal.pending(), h.publicationHold);
        assert.equal(await h.journal.completedFor(h.issuedLease), false);
        assert.deepEqual(await h.staging.read(), h.issuedLease);
        assert.equal(h.network.mock.callCount(), 0);
      }
    });
  }
});
