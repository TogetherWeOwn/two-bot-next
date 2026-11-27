import assert from "node:assert/strict";
import { test, type TestContext } from "node:test";
import { runStagingAutomodBridgeRehearsal } from "../scripts/staging-automod-bridge-runner.ts";
import { STAGING_AUTOMOD_BRIDGE_URL } from "../scripts/staging-automod-instance-client.ts";
import { STAGING_AUTOMOD_AUDIENCE, STAGING_AUTOMOD_OIDC_HEADER } from "../src/staging-automod-bridge.ts";
import { STAGING_AUTOMOD_PLAN } from "../src/staging-automod-plan.ts";
import { STAGING_AUTOMOD_TARGET as target } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE } from "../src/staging-lifecycle.ts";

type Dependencies = NonNullable<Parameters<typeof runStagingAutomodBridgeRehearsal>[1]>;
type Inventory = NonNullable<Awaited<ReturnType<Dependencies["readAuthenticatedInventory"]>>>;
const now = Date.parse("2026-11-27T12:00:00Z");
const plan = { ...STAGING_AUTOMOD_PLAN, enabled: true, allowStart: true, notAfter: now + 30_000 };
const dispatch = { sha: "a".repeat(40), runId: "12345", runAttempt: 1 };
const inventorySource = { workflowSha: dispatch.sha, runId: 12346 };
const repository = { id: 1396224242 };
const path = ".github/workflows/staging-automod-preview.yml";
const runner = {
  GITHUB_SHA: dispatch.sha, GITHUB_RUN_ID: dispatch.runId, GITHUB_RUN_ATTEMPT: "1",
  GITHUB_REPOSITORY_ID: String(repository.id), GITHUB_WORKFLOW_REF: `TogetherWeOwn/two-bot-next/${path}@refs/heads/main`,
  GITHUB_WORKFLOW_SHA: dispatch.sha, GITHUB_REF: "refs/heads/main", GITHUB_REF_PROTECTED: "true",
  GITHUB_EVENT_NAME: "workflow_dispatch",
};
const options = { enabled: true, runner, plan };
const unconfirmed = { verdict: "failed", code: "context_unconfirmed" };
const hostUnavailable = { verdict: "failed", code: "host_unavailable" };
const runUrl = `https://api.github.com/repos/TogetherWeOwn/two-bot-next/actions/runs/${dispatch.runId}`;
const oidcUrl = "https://pipelines.actions.githubusercontent.com/token";
const env = {
  ACTIONS_ID_TOKEN_REQUEST_URL: oidcUrl,
  ACTIONS_ID_TOKEN_REQUEST_TOKEN: "synthetic-oidc-request",
  GITHUB_TOKEN: "synthetic-github-read",
  OWNERSHIP_CONTROL_TOKEN: "synthetic-ownership-control-token",
};
const pin = {
  workerDeploymentId: "11111111-1111-1111-1111-111111111111",
  workerVersionId: "22222222-2222-2222-2222-222222222222",
  sourceSha: dispatch.sha,
  applicationId: "33333333-3333-3333-3333-333333333333",
  namespaceId: "d".repeat(32),
  rolloutId: "44444444-4444-4444-4444-444444444444",
  imageDigest: `sha256:${"b".repeat(64)}`,
};
const context = { ...dispatch, environment: "staging", deploymentId: pin.workerVersionId } as const;
const change = { ...context, action: "acquire", expectedEpoch: 7, expiresAt: now + 10_000 };
const issuedLease = { ...context, purpose: AUTOMOD_STAGING_PURPOSE, phase: "active", epoch: change.expectedEpoch,
  acquiredAt: now, expiresAt: change.expiresAt };
const candidate = { dispatch, pin, target, expiresAt: change.expiresAt, inventoryRun: { ...inventorySource, runAttempt: 1 } };
const inventory: Inventory = {
  schema_version: 1, workflow_sha: inventorySource.workflowSha, workflow_run_id: inventorySource.runId,
  observed_at: new Date(now).toISOString(),
  worker: { deployment_id: pin.workerDeploymentId, version_id: pin.workerVersionId, source_sha: pin.sourceSha,
    traffic_percent: 100, runtime_policy: "bounded" },
  container: { application_id: pin.applicationId, namespace_id: pin.namespaceId, rollout_id: pin.rolloutId,
    image_digest: pin.imageDigest, rollout_status: "completed" },
  ownership: { phase: "active", epoch: change.expectedEpoch },
};
const run = { id: Number(dispatch.runId), event: "workflow_dispatch", run_attempt: 1, status: "in_progress",
  path, head_branch: "main", head_sha: dispatch.sha, repository, head_repository: repository };

function unexpectedFetch(): never {
  return assert.fail("unexpected fetch");
}

function fixture(t: TestContext) {
  t.mock.timers.enable({ apis: ["Date", "setTimeout"], now });
  t.mock.method(globalThis, "fetch", unexpectedFetch);
  const calls: string[] = [];
  const deps = {
    env,
    fetcher: t.mock.fn<Dependencies["fetcher"]>(unexpectedFetch),
    readPostDeployRuns: t.mock.fn<Dependencies["readPostDeployRuns"]>(async (scope, signal) => {
      assert.deepEqual(scope, dispatch);
      assert.equal(signal.aborted, false);
      calls.push("post-deploy");
      return [candidate];
    }),
    readAuthenticatedInventory: t.mock.fn<Dependencies["readAuthenticatedInventory"]>(async (source, signal) => {
      assert.deepEqual(source, inventorySource);
      assert.equal(signal.aborted, false);
      calls.push("inventory");
      return inventory;
    }),
  };
  return { deps, calls };
}

test("disabled and default plan paths perform no reads or fetches", async t => {
  const { deps, calls } = fixture(t);
  assert.deepEqual(await runStagingAutomodBridgeRehearsal(), { verdict: "disabled" });
  assert.deepEqual(await runStagingAutomodBridgeRehearsal({ ...options, enabled: false }, deps), { verdict: "disabled" });
  assert.equal(STAGING_AUTOMOD_PLAN.enabled, false);
  assert.deepEqual(await runStagingAutomodBridgeRehearsal({ enabled: true, runner }, deps), unconfirmed);
  assert.deepEqual(calls, []);
  assert.equal(deps.fetcher.mock.callCount(), 0);
});

test("invalid runner metadata refuses before either readback", async t => {
  const { deps, calls } = fixture(t);
  assert.deepEqual(await runStagingAutomodBridgeRehearsal({ enabled: true, plan }, deps), unconfirmed);
  for (const invalid of [
    { GITHUB_SHA: "main" }, { GITHUB_SHA: "A".repeat(40) }, { GITHUB_SHA: dispatch.sha + "\n" },
    { GITHUB_RUN_ID: "0" }, { GITHUB_RUN_ID: "01" }, { GITHUB_RUN_ID: "1e3" },
    { GITHUB_RUN_ID: "9007199254740992" }, { GITHUB_RUN_ATTEMPT: "2" }, { GITHUB_RUN_ATTEMPT: "01" },
  ]) {
    assert.deepEqual(await runStagingAutomodBridgeRehearsal({ ...options, runner: { ...runner, ...invalid } }, deps), unconfirmed);
  }
  assert.deepEqual(calls, []);
  assert.equal(deps.fetcher.mock.callCount(), 0);
});

test("missing, conflicting or rejected selection evidence never reaches the bridge", async t => {
  const { deps, calls } = fixture(t);
  for (const runs of [undefined, [candidate, candidate]]) {
    const readers = { ...deps, readPostDeployRuns: async () => runs };
    assert.deepEqual(await runStagingAutomodBridgeRehearsal(options, readers), unconfirmed);
  }
  const rejected = { ...deps, readPostDeployRuns: async () => { throw new Error("synthetic readback refusal"); } };
  assert.deepEqual(await runStagingAutomodBridgeRehearsal(options, rejected), unconfirmed);
  assert.deepEqual(calls, []);
  assert.equal(deps.fetcher.mock.callCount(), 0);
});

test("caller cancellation or the selection deadline refuses a stalled reader", { timeout: 1_000 }, async t => {
  const { deps, calls } = fixture(t);
  const controller = new AbortController();
  assert.deepEqual(await runStagingAutomodBridgeRehearsal({ ...options, signal: AbortSignal.abort() }, deps), unconfirmed);
  for (const [signal, abort] of [
    [controller.signal, () => controller.abort()],
    [new AbortController().signal, () => t.mock.timers.tick(2_000)],
  ] as const) {
    const entered = Promise.withResolvers<AbortSignal>();
    const pending = runStagingAutomodBridgeRehearsal({ ...options, signal }, {
      ...deps,
      readPostDeployRuns: (_scope, selectionSignal) => {
        entered.resolve(selectionSignal);
        return new Promise<never>(() => {});
      },
    });
    const seen = await entered.promise;
    assert.equal(seen.aborted, false);
    abort();
    assert.deepEqual(await pending, unconfirmed);
    assert.equal(seen.aborted, true);
  }
  assert.deepEqual(calls, []);
  assert.equal(deps.fetcher.mock.callCount(), 0);
});

test("missing or invalid bridge credentials refuse without fetching", async t => {
  const { deps, calls } = fixture(t);
  assert.deepEqual(await runStagingAutomodBridgeRehearsal(options), hostUnavailable);
  const invalidCredentials = [
    {}, { ...env, ACTIONS_ID_TOKEN_REQUEST_TOKEN: "" }, { ...env, OWNERSHIP_CONTROL_TOKEN: "short" },
    { ...env, ACTIONS_ID_TOKEN_REQUEST_URL: "https://pipelines.actions.githubusercontent.com.example.test/token" },
    { ...env, GITHUB_TOKEN: "not a token" },
  ];
  for (const credentials of invalidCredentials) {
    assert.deepEqual(await runStagingAutomodBridgeRehearsal(options, { ...deps, env: credentials }), hostUnavailable);
  }
  assert.deepEqual(calls, invalidCredentials.flatMap(() => ["post-deploy", "inventory"]));
  assert.equal(deps.fetcher.mock.callCount(), 0);
});

test("missing fetch wiring cannot fall back to ambient fetch", async t => {
  const { deps } = fixture(t);
  const { fetcher: _, ...withoutFetch } = deps;
  assert.deepEqual(await runStagingAutomodBridgeRehearsal(options, withoutFetch as unknown as Dependencies), hostUnavailable);
  assert.equal(deps.fetcher.mock.callCount(), 0);
});

test("a pin and inventory mismatch refuses selection before any request", async t => {
  const { deps } = fixture(t);
  deps.readAuthenticatedInventory.mock.mockImplementation(async () => ({
    ...inventory, worker: { ...inventory.worker, version_id: pin.workerDeploymentId },
  }));
  assert.deepEqual(await runStagingAutomodBridgeRehearsal(options, deps), unconfirmed);
  assert.equal(deps.readAuthenticatedInventory.mock.callCount(), 1);
  assert.equal(deps.fetcher.mock.callCount(), 0);
});

test("inventory drift after selection refuses before acquiring or invoking", async t => {
  const { deps } = fixture(t);
  const operations: string[] = [];
  deps.readAuthenticatedInventory.mock.mockImplementationOnce(async () => inventory, 0);
  deps.readAuthenticatedInventory.mock.mockImplementation(async () => ({
    ...inventory, ownership: { ...inventory.ownership, epoch: change.expectedEpoch + 1 },
  }));
  deps.fetcher.mock.mockImplementation(async (url, init) => {
    if (url === runUrl) return Response.json(run);
    if (url instanceof URL && url.origin === new URL(oidcUrl).origin) return Response.json({ value: "synthetic-oidc" });
    if (url !== STAGING_AUTOMOD_BRIDGE_URL) return unexpectedFetch();
    operations.push(JSON.parse(String(init.body)).op);
    return Response.json({ value: null });
  });
  assert.deepEqual(await runStagingAutomodBridgeRehearsal(options, deps), unconfirmed);
  assert.equal(deps.readAuthenticatedInventory.mock.callCount(), 2);
  assert.deepEqual(operations, ["read-lease", "read-lease"]);
});

test("the protected host still refuses unprotected Actions metadata", async t => {
  const { deps, calls } = fixture(t);
  assert.deepEqual(await runStagingAutomodBridgeRehearsal({
    ...options, runner: { ...runner, GITHUB_REF_PROTECTED: "false" },
  }, deps), unconfirmed);
  assert.deepEqual(calls, ["post-deploy", "inventory"]);
  assert.equal(deps.fetcher.mock.callCount(), 0);
});

test("the mocked rehearsal pins each bridge operation and isolates request credentials", async t => {
  const { deps, calls } = fixture(t);
  const operations = [
    { op: "read-lease", context: { ...context, target } },
    { op: "acquire", pin, change },
    { op: "read-lease", context: { ...context, target } },
    { op: "read-approval", action: "start", issuedLease },
    { op: "read-approval", action: "recover", issuedLease },
    { op: "invoke", pin, call: { action: "start", issuedLease } },
    { op: "invoke", pin, call: { action: "recover", issuedLease } },
  ];
  const replies = [null, issuedLease, issuedLease, { action: "start", target, issuedLease },
    { action: "recover", target, issuedLease }, null, null];
  let mintCount = 0;
  let bridgeCount = 0;
  deps.fetcher.mock.mockImplementation(async (url, init) => {
    assert.equal(init.redirect, "error");
    assert.equal(init.signal?.aborted, false);
    const headers = Object.fromEntries(new Headers(init.headers));
    if (url === runUrl) {
      assert.equal(init.method, "GET");
      assert.deepEqual(headers, { accept: "application/vnd.github+json", "x-github-api-version": "2022-11-28",
        authorization: `Bearer ${env.GITHUB_TOKEN}` });
      calls.push("run");
      return Response.json(run);
    }
    if (url instanceof URL) {
      assert.equal(url.href, `${oidcUrl}?audience=${encodeURIComponent(STAGING_AUTOMOD_AUDIENCE)}`);
      assert.equal(init.method, "GET");
      assert.deepEqual(headers, { accept: "application/json", authorization: `Bearer ${env.ACTIONS_ID_TOKEN_REQUEST_TOKEN}` });
      calls.push(`oidc:${++mintCount}`);
      return Response.json({ value: `synthetic-oidc-${mintCount}` });
    }
    assert.equal(url, STAGING_AUTOMOD_BRIDGE_URL);
    const sequence = bridgeCount++;
    assert.equal(init.method, "POST");
    assert.deepEqual(headers, { authorization: `Bearer ${env.OWNERSHIP_CONTROL_TOKEN}`, "content-type": "application/json",
      [STAGING_AUTOMOD_OIDC_HEADER]: `synthetic-oidc-${sequence + 1}` });
    assert.deepEqual(JSON.parse(String(init.body)), operations[sequence]);
    calls.push(`bridge:${sequence + 1}`);
    return Response.json({ value: replies[sequence] });
  });

  assert.deepEqual(await runStagingAutomodBridgeRehearsal(options, deps), { verdict: "passed", start: true, recover: true });
  assert.deepEqual(calls, [
    "post-deploy", "inventory", "run", "oidc:1", "bridge:1", "inventory", "oidc:2", "bridge:2", "oidc:3", "bridge:3",
    "oidc:4", "oidc:5", "bridge:4", "bridge:5", "inventory", "oidc:6", "bridge:6", "oidc:7", "bridge:7",
  ]);
});
