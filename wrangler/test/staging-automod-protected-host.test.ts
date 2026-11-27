import assert from "node:assert/strict";
import { test, type TestContext } from "node:test";
import { runStagingAutomodDispatch } from "../scripts/staging-automod-dispatch.ts";
import { bindStagingAutomodInventoryReader } from "../scripts/staging-automod-inventory-reader.ts";
import { runStagingAutomodProtectedHost } from "../scripts/staging-automod-protected-host.ts";
import { runProtectedAutomod, type ProtectedAutomodEffects } from "../src/staging-automod-caller.ts";
import { StagingAutomodLifecycle } from "../src/staging-automod-lifecycle.ts";
import { STAGING_AUTOMOD_TARGET as target, StagingAutomodRecovery } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE, StagingLifecycle, type StagingIdentity } from "../src/staging-lifecycle.ts";
import publication from "./fixtures/staging-automod-publication.json" with { type: "json" };
import { lease } from "./fixtures/staging-automod-recovery.ts";

type Host = NonNullable<Parameters<typeof runStagingAutomodProtectedHost>[1]>;
type Inventory = NonNullable<Awaited<ReturnType<Host["readAuthenticatedInventory"]>>>;
const now = Date.parse("2026-01-01T00:00:30Z");
const deploymentId = "11111111-1111-1111-1111-111111111111";
const issuedLease = { ...lease, deploymentId, runAttempt: 1, purpose: AUTOMOD_STAGING_PURPOSE,
  acquiredAt: now - 1_000, expiresAt: now + 1_000 } as const;
const sha = issuedLease.sha;
const serving = { deploymentId, sha };
const path = ".github/workflows/staging-automod-preview.yml";
const head = "e".repeat(40);
const repository = { id: 1396224242 };
const runner = { GITHUB_REPOSITORY_ID: String(repository.id), GITHUB_WORKFLOW_REF: `TogetherWeOwn/two-bot-next/${path}@refs/heads/main`,
  GITHUB_REF: "refs/heads/main", GITHUB_REF_PROTECTED: "true", GITHUB_EVENT_NAME: "workflow_dispatch",
  GITHUB_RUN_ID: issuedLease.runId, GITHUB_RUN_ATTEMPT: "1", GITHUB_SHA: head, GITHUB_WORKFLOW_SHA: head };
const run = { id: Number(issuedLease.runId), event: "workflow_dispatch", run_attempt: 1, status: "in_progress",
  path, head_branch: "main", head_sha: head, repository, head_repository: repository };
const inventorySource = { workflowSha: "c".repeat(40), runId: run.id + 1 };
const options = { enabled: true, runner, serving, inventorySource };
const unconfirmed = { verdict: "failed", code: "context_unconfirmed" };
const noActions = { verdict: "failed", start: false, recover: false };
const inventory: Inventory = {
  schema_version: 1, workflow_sha: inventorySource.workflowSha, workflow_run_id: inventorySource.runId,
  observed_at: new Date(now).toISOString(),
  worker: { deployment_id: "22222222-2222-2222-2222-222222222222", version_id: deploymentId,
    source_sha: sha, traffic_percent: 100, runtime_policy: "bounded" },
  container: { application_id: "33333333-3333-3333-3333-333333333333", namespace_id: "d".repeat(32),
    rollout_id: "44444444-4444-4444-4444-444444444444", rollout_status: "completed", image_digest: `sha256:${"b".repeat(64)}` },
  ownership: { phase: "active", epoch: issuedLease.epoch },
};
const pin = { workerDeploymentId: inventory.worker.deployment_id, workerVersionId: deploymentId, sourceSha: sha,
  applicationId: inventory.container.application_id, namespaceId: inventory.container.namespace_id,
  rolloutId: inventory.container.rollout_id, imageDigest: inventory.container.image_digest };

function fixture(t: TestContext) {
  const access: string[] = [];
  const controlBearer = "synthetic-automod-host-control-value";
  const invokePrivate = t.mock.fn<Host["invokePrivate"]>(async (_pin, call, signal) => {
    access.push(`invoke:${call.action}`);
    assert.equal(signal.aborted, false);
  });
  const host: Host = {
    readRun: async (id, signal) => {
      access.push("run");
      assert.equal(id, issuedLease.runId);
      assert.equal(signal.aborted, false);
      return run;
    },
    authority: {
      readIssuedLease: async (context, signal) => {
        access.push("lease");
        assert.deepEqual(context, { ...serving, target, environment: "staging", runId: issuedLease.runId, runAttempt: 1 });
        assert.equal(signal.aborted, false);
        return issuedLease;
      },
      readApproval: async (action, original, signal) => {
        access.push(`approval:${action}`);
        assert.deepEqual(original, issuedLease);
        assert.equal(signal.aborted, false);
        return { action, target, issuedLease };
      },
    },
    readWorker: () => {
      access.push("worker");
      return { controlBearer, env: { TWO_DEPLOY_ENV: "staging", STAGING_RUNTIME_POLICY: "bounded",
        DISCORD_APPLICATION_ID: target.applicationId, GUILD_ID: target.guildId, CONTAINER_RELEASE: sha,
        CF_VERSION_METADATA: { id: deploymentId }, OWNERSHIP_CONTROL_TOKEN: controlBearer } };
    },
    readAuthenticatedInventory: async (source, signal) => {
      access.push("inventory");
      assert.deepEqual(source, inventorySource);
      assert.equal(signal.aborted, false);
      return inventory;
    },
    invokePrivate,
  };
  return { access, host, invokePrivate };
}

test("the protected host composes both actions on one authenticated serving pin", async t => {
  t.mock.timers.enable({ apis: ["Date"], now });
  const network = t.mock.method(globalThis, "fetch", async () => assert.fail("unexpected network access"));
  const { access, host, invokePrivate } = fixture(t);
  assert.deepEqual(await runStagingAutomodProtectedHost(options, host), { verdict: "passed", start: true, recover: true });
  assert.deepEqual(access, ["run", "lease", "approval:start", "approval:recover", "worker", "inventory", "invoke:start", "invoke:recover"]);
  const calls = invokePrivate.mock.calls.map(({ arguments: [pin, call] }) => ({ pin, call }));
  assert.deepEqual(calls, [{ pin, call: { action: "start", issuedLease } }, { pin, call: { action: "recover", issuedLease } }]);
  assert.equal(calls[0]?.pin, calls[1]?.pin);
  assert.equal(network.mock.callCount(), 0);
});

test("dispatch preserves producer observation age through inventory verification and delivery", async t => {
  t.mock.timers.enable({ apis: ["Date"], now });
  const network = t.mock.method(globalThis, "fetch", async () => assert.fail("unexpected network access"));
  const env = { ...runner, STAGING_AUTOMOD_REHEARSAL: "true" };
  const verificationMs = 100;
  const handoffMs = 101;
  const deliveryMs = verificationMs + handoffMs;
  const cases = [
    { label: "expired in handoff", age: 59_800, readTimes: [now, now + deliveryMs], result: noActions, calls: [] },
    { label: "fresh at delivery", age: 1_000, readTimes: [now],
      result: { verdict: "passed", start: true, recover: true },
      calls: [[pin, { action: "start", issuedLease }], [pin, { action: "recover", issuedLease }]] },
  ];

  for (const { label, age, readTimes, result, calls } of cases) {
    t.mock.timers.setTime(now);
    const { host, invokePrivate } = fixture(t);
    const receipt = { ...inventory, observed_at: new Date(now - age).toISOString() };
    const verify = t.mock.fn(async (_sha: string, _runId: string) => {
      t.mock.timers.setTime(Date.now() + verificationMs);
      return JSON.stringify(receipt) + "\n";
    });
    const read = bindStagingAutomodInventoryReader(verify);
    const handoffs: { acceptedAt: number; deliveredAt: number; readback: Inventory | undefined }[] = [];
    const readAuthenticatedInventory: Host["readAuthenticatedInventory"] = async (source, signal) => {
      const readback = await read(source, signal);
      const acceptedAt = Date.now();
      // The first old receipt is 59,900 ms old after verification and 60,001 ms old at delivery.
      t.mock.timers.setTime(acceptedAt + handoffMs);
      handoffs.push({ acceptedAt, deliveredAt: Date.now(), readback });
      return readback;
    };

    assert.deepEqual(await runStagingAutomodDispatch(env, { serving, inventorySource,
      host: { ...host, readAuthenticatedInventory } }), result, label);
    assert.deepEqual(verify.mock.calls.map(({ arguments: [workflowSha, runId] }) => [workflowSha, runId]),
      readTimes.map(() => [inventorySource.workflowSha, String(inventorySource.runId)]), label);
    assert.deepEqual(handoffs, readTimes.map(at => ({
      acceptedAt: at + verificationMs, deliveredAt: at + deliveryMs, readback: receipt,
    })), label);
    assert.deepEqual(invokePrivate.mock.calls.map(({ arguments: [pin, call] }) => [pin, call]), calls, label);
  }
  assert.equal(network.mock.callCount(), 0);
});

test("disabled operation and missing or invalid host pins read no providers", async t => {
  const { access, host, invokePrivate } = fixture(t);
  assert.deepEqual(await runStagingAutomodProtectedHost(), { verdict: "disabled" });
  assert.deepEqual(await runStagingAutomodProtectedHost({ ...options, enabled: false }, host), { verdict: "disabled" });
  assert.deepEqual(await runStagingAutomodProtectedHost(options), unconfirmed);
  const { inventorySource: _, ...unpinned } = options;
  assert.deepEqual(await runStagingAutomodProtectedHost(unpinned, host), unconfirmed);
  for (const source of [{ ...inventorySource, workflowSha: "main" }, { ...inventorySource, runId: 0 },
    { ...inventorySource, runId: Number.MAX_SAFE_INTEGER + 1 }]) {
    assert.deepEqual(await runStagingAutomodProtectedHost({ ...options, inventorySource: source }, host), unconfirmed);
  }
  assert.deepEqual(access, []);
  assert.equal(invokePrivate.mock.callCount(), 0);
});

test("unconfirmed run lease or approvals stop before Worker bindings and private invocation", async t => {
  t.mock.timers.enable({ apis: ["Date"], now });
  const { access, host, invokePrivate } = fixture(t);
  assert.deepEqual(await runStagingAutomodProtectedHost(options, { ...host, readRun: async () => undefined }), unconfirmed);
  assert.deepEqual(await runStagingAutomodProtectedHost(options, { ...host, readRun: async () => ({ ...run, head_sha: sha }) }), unconfirmed);
  for (const readback of [undefined, { ...issuedLease, runId: "99" }]) {
    const authority = { ...host.authority, readIssuedLease: async () => readback };
    assert.deepEqual(await runStagingAutomodProtectedHost(options, { ...host, authority }), { verdict: "failed", code: "lease_unconfirmed" });
  }
  const authority: Host["authority"] = { ...host.authority, readApproval: async action => action === "recover"
    ? { action, target, issuedLease: { ...issuedLease, epoch: issuedLease.epoch + 1 } } : undefined };
  assert.deepEqual(await runStagingAutomodProtectedHost(options, { ...host, authority }), noActions);
  assert.deepEqual(access, ["run", "run", "run", "lease"]);
  assert.equal(invokePrivate.mock.callCount(), 0);
});

test("missing or mismatched staging inventory refuses before private invocation", async t => {
  t.mock.timers.enable({ apis: ["Date"], now });
  const { host, invokePrivate } = fixture(t);
  for (const readback of [undefined, { ...inventory, workflow_sha: head }, { ...inventory, workflow_run_id: run.id },
    { ...inventory, worker: { ...inventory.worker, source_sha: head } },
    { ...inventory, ownership: { ...inventory.ownership, epoch: issuedLease.epoch + 1 } }]) {
    const readAuthenticatedInventory = t.mock.fn<Host["readAuthenticatedInventory"]>(async () => readback);
    assert.deepEqual(await runStagingAutomodProtectedHost(options, { ...host, readAuthenticatedInventory }), noActions);
    assert.equal(readAuthenticatedInventory.mock.callCount(), 2);
  }
  assert.equal(invokePrivate.mock.callCount(), 0);
});

test("authenticated inventory reader exceptions are redacted before private invocation", async t => {
  t.mock.timers.enable({ apis: ["Date"], now });
  const network = t.mock.method(globalThis, "fetch", async () => assert.fail("unexpected network access"));
  const { access, host, invokePrivate } = fixture(t);
  const detail = new Error("synthetic private inventory diagnostics");
  const beforeInventory = ["run", "lease", "approval:start", "approval:recover", "worker"];
  const failureModes = {
    rejection: () => Promise.reject(detail),
    throw: () => { throw detail; },
  };

  for (const [mode, fail] of Object.entries(failureModes)) {
    const reads: { source: typeof inventorySource; aborted: boolean }[] = [];
    const readAuthenticatedInventory: Host["readAuthenticatedInventory"] = (source, signal) => {
      access.push("inventory");
      reads.push({ source, aborted: signal.aborted });
      return fail();
    };
    assert.deepEqual(await runStagingAutomodProtectedHost(options, { ...host, readAuthenticatedInventory }), noActions, mode);
    assert.deepEqual(access.splice(0), [...beforeInventory, "inventory", "inventory"], mode);
    assert.deepEqual(reads, [{ source: inventorySource, aborted: false }, { source: inventorySource, aborted: false }], mode);
    assert.equal(invokePrivate.mock.callCount(), 0, mode);
  }

  assert.deepEqual(await runStagingAutomodProtectedHost(options, host), { verdict: "passed", start: true, recover: true });
  assert.deepEqual(access, [...beforeInventory, "inventory", "invoke:start", "invoke:recover"]);
  assert.deepEqual(invokePrivate.mock.calls.map(({ arguments: [pin, call] }) => [pin, call]),
    [[pin, { action: "start", issuedLease }], [pin, { action: "recover", issuedLease }]]);
  assert.equal(network.mock.callCount(), 0);
});

test("a separately approved recovery can still use the original serving pin", async t => {
  t.mock.timers.enable({ apis: ["Date"], now });
  const { host, invokePrivate } = fixture(t);
  const authority: Host["authority"] = { ...host.authority, readApproval: async action => action === "recover"
    ? { action, target, issuedLease } : undefined };
  assert.deepEqual(await runStagingAutomodProtectedHost(options, { ...host, authority }),
    { verdict: "failed", start: false, recover: true });
  assert.deepEqual(invokePrivate.mock.calls.map(({ arguments: [, call] }) => call), [{ action: "recover", issuedLease }]);
});

test("the protected host holds a lost publication acknowledgement until the scoped registry is reconciled", async t => {
  t.mock.timers.enable({ apis: ["Date"], now: issuedLease.acquiredAt });
  const network = t.mock.method(globalThis, "fetch", async () => assert.fail("unexpected network access"));
  const values = new Map<string, unknown>();
  const storage = {
    get: async (key: string) => structuredClone(values.get(key)),
    put: async (key: string | Record<string, unknown>, value?: unknown) => {
      for (const [name, record] of Object.entries(structuredClone(typeof key === "string" ? { [key]: value } : key))) {
        values.set(name, record);
      }
    },
    sync: async () => {},
  } as unknown as DurableObjectStorage;
  const staging = new StagingLifecycle(storage);
  const journal = new StagingAutomodRecovery(storage);
  const { purpose, ...publicationLease } = issuedLease;
  const identity: StagingIdentity = { deploymentId, sha, owner: { deploymentId, epoch: issuedLease.epoch,
    phase: "active", actor: "synthetic", timestamp: new Date().toISOString(),
    oldEpoch: issuedLease.epoch - 1, oldDeploymentId: null } };
  assert.deepEqual(await staging.acquire({ ...publicationLease, action: "acquire", expectedEpoch: issuedLease.epoch },
    identity, async () => {}, async () => {}, Date.now(), purpose), issuedLease);
  t.mock.timers.setTime(now);

  const { host: hostFixture } = fixture(t);
  const host: Host = { ...hostFixture, authority: { ...hostFixture.authority,
    readIssuedLease: context => staging.readIssuedAutomodLease(context, identity) } };
  const { help, preview } = publication.replies;
  const bot = "https://discord.com/api/v10/users/@me";
  const commands = `https://discord.com/api/v10/applications/${target.applicationId}/guilds/${target.guildId}/commands`;
  const list = `${commands}?with_localizations=true`;
  const registry = new Map<string, typeof help | typeof preview>([[help.id, help]]);
  const env = { ...publication.env, ...host.readWorker().env, TWO_MODERATION: "1", TWO_OWEN_USER_ID: preview.id };
  // The fixture awaits each operation before issuing the next one.
  const authority = { storage, staging, env, identity: async () => identity,
    exclusive: <T>(operation: () => Promise<T>) => operation() };
  const calls: string[] = [];
  const policy = { memberIntake: false, enforcement: false, resume: false, registrySync: false,
    interactionDispatch: false, dedicatedIntents: true };
  const start = t.mock.fn(async () => assert.fail("publication was never confirmed"));
  const effects: ProtectedAutomodEffects = {
    enabled: true, dryRun: { policy, start },
    readRuntime: async () => ({ lease: issuedLease, running: false }),
    readCleanupGrant: async () => ({ lease: issuedLease, before: issuedLease.expiresAt + 5_000 }),
    fenceAndDrain: async () => {
      calls.push("drain");
      return { lease: issuedLease, publicationDrained: true, startupDrained: true };
    },
    stop: async () => { calls.push("stop"); return true; },
    request: async (url, init) => {
      calls.push(`${init.method} ${url}`);
      if (init.method === "POST" && url === commands) {
        assert.deepEqual(JSON.parse(String(init.body)), publication.definition);
        registry.set(preview.id, preview);
        throw new Error("synthetic publication acknowledgement lost after the registry write");
      }
      if (init.method === "GET" && url === bot) return Response.json(publication.identity);
      if (init.method === "GET" && url === list) {
        assert.deepEqual(await staging.read(), issuedLease);
        assert.equal(await journal.completedFor(issuedLease), false);
        return Response.json([...registry.values()]);
      }
      assert.fail("unexpected scoped registry request");
    },
  };
  const publisher = new StagingAutomodLifecycle({ ...authority, ...effects,
    observe: async () => ({ ...await staging.automodBinding(issuedLease, identity, Date.now()), purpose, running: false }),
    get: async url => {
      if (url === bot) return publication.identity;
      assert.equal(url, commands);
      return [...registry.values()];
    },
  });
  await assert.rejects(publisher.publish({ lease: publicationLease, purposes: [purpose],
    origin: "https://two-bot-next-staging.example.workers.dev", cleanupReserveMs: 500,
    verifiedRecoveryPair: true, verifiedRuntimeDatabase: true, verifiedGatewayIntents: true }),
  { message: "staging_automod_publication_uncertain" });
  assert.deepEqual(calls.splice(0), [`GET ${bot}`, `GET ${list}`, `GET ${list}`, `POST ${commands}`]);
  assert.deepEqual(registry.get(preview.id), preview);
  const pending = await journal.pending();
  assert.ok(pending && "target" in pending);
  assert.deepEqual(pending.lease, publicationLease);
  assert.deepEqual(pending.target, target);

  const invokePrivate = t.mock.fn<Host["invokePrivate"]>(async (_pin, call) => runProtectedAutomod(call, authority, effects));
  const assertRecovery = async (actions: ("start" | "recover")[], recover: boolean, protectedHost: Host) => {
    assert.deepEqual(await runStagingAutomodProtectedHost(options, { ...protectedHost, invokePrivate }),
      { verdict: "failed", start: false, recover });
    assert.deepEqual(invokePrivate.mock.calls.map(({ arguments: [pin, call] }) => ({ pin, call })),
      actions.map(action => ({ pin, call: { action, issuedLease } })));
    invokePrivate.mock.resetCalls();
    assert.deepEqual(calls.splice(0), ["drain", "stop", `GET ${bot}`, `GET ${list}`]);
  };
  const assertHeld = async () => {
    assert.deepEqual(await staging.read(), issuedLease);
    assert.deepEqual(await journal.pending(), pending);
    assert.equal(await journal.completedFor(issuedLease), false);
    await assert.rejects(publisher.otherUse(async () => assert.fail("competing use ran")),
      { message: "staging_automod_lifecycle_refused" });
  };
  await assertRecovery(["start", "recover"], false, host);
  await assertHeld();
  assert.deepEqual(registry.get(preview.id), preview);

  // Independent reconciliation changes the synthetic registry outside the
  // caller. With no durable receipt, recovery cannot attribute or delete the row.
  t.mock.timers.setTime(issuedLease.expiresAt + 1);
  const recoveryHost: Host = { ...host, authority: { ...host.authority,
    readApproval: async (action, ...args) => action === "recover" ? host.authority.readApproval(action, ...args) : undefined } };
  assert.equal(registry.delete(preview.id), true);
  registry.set(help.id, { ...help, description: "synthetic registry drift" });
  await assertRecovery(["recover"], false, recoveryHost);
  await assertHeld();

  registry.set(help.id, help);
  await assertRecovery(["recover"], true, recoveryHost);
  assert.deepEqual([...registry.values()], [help]);
  assert.equal(await journal.completedFor(issuedLease), true);
  assert.equal(await journal.pending(), undefined);
  assert.deepEqual(await staging.read(), { ...issuedLease, phase: "released" });
  assert.equal(start.mock.callCount(), 0);
  assert.equal(network.mock.callCount(), 0);
});
