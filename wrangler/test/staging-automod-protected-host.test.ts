import assert from "node:assert/strict";
import { test, type TestContext } from "node:test";
import { runStagingAutomodProtectedHost } from "../scripts/staging-automod-protected-host.ts";
import { STAGING_AUTOMOD_TARGET as target } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE } from "../src/staging-use-window.ts";
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
  const pin = { workerDeploymentId: inventory.worker.deployment_id, workerVersionId: deploymentId, sourceSha: sha,
    applicationId: inventory.container.application_id, namespaceId: inventory.container.namespace_id,
    rolloutId: inventory.container.rollout_id, imageDigest: inventory.container.image_digest };
  assert.deepEqual(calls, [{ pin, call: { action: "start", issuedLease } }, { pin, call: { action: "recover", issuedLease } }]);
  assert.equal(calls[0]?.pin, calls[1]?.pin);
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

test("a separately approved recovery can still use the original serving pin", async t => {
  t.mock.timers.enable({ apis: ["Date"], now });
  const { host, invokePrivate } = fixture(t);
  const authority: Host["authority"] = { ...host.authority, readApproval: async action => action === "recover"
    ? { action, target, issuedLease } : undefined };
  assert.deepEqual(await runStagingAutomodProtectedHost(options, { ...host, authority }),
    { verdict: "failed", start: false, recover: true });
  assert.deepEqual(invokePrivate.mock.calls.map(({ arguments: [, call] }) => call), [{ action: "recover", issuedLease }]);
});
