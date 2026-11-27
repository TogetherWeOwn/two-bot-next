import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { test } from "node:test";
import { URL, fileURLToPath } from "node:url";
import { bindStagingAutomodDispatch, runStagingAutomodDispatch } from "../scripts/staging-automod-dispatch.ts";
import { STAGING_AUTOMOD_TARGET as target } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE } from "../src/staging-use-window.ts";
import { lease } from "./fixtures/staging-automod-recovery.ts";

type Binding = NonNullable<Parameters<typeof runStagingAutomodDispatch>[1]>;
const workflow = ".github/workflows/staging-automod-preview.yml";
const head = "a".repeat(40);
const runner = { STAGING_AUTOMOD_REHEARSAL: "true", GITHUB_REPOSITORY_ID: "1396224242",
  GITHUB_WORKFLOW_REF: `TogetherWeOwn/two-bot-next/${workflow}@refs/heads/main`,
  GITHUB_REF: "refs/heads/main", GITHUB_REF_PROTECTED: "true", GITHUB_EVENT_NAME: "workflow_dispatch",
  GITHUB_RUN_ID: "11223344", GITHUB_RUN_ATTEMPT: "1", GITHUB_SHA: head, GITHUB_WORKFLOW_SHA: head };
const repo = { id: Number(runner.GITHUB_REPOSITORY_ID) };
const run = { id: Number(runner.GITHUB_RUN_ID), event: "workflow_dispatch", run_attempt: 1, status: "in_progress",
  path: workflow, head_branch: "main", head_sha: head, repository: repo, head_repository: repo };
const serving = { deploymentId: "11111111-1111-1111-1111-111111111111", sha: "b".repeat(40) };
const inventorySource = { workflowSha: "c".repeat(40), runId: run.id + 1 };
const unconfirmed = { verdict: "failed", code: "context_unconfirmed" };

function unexpected(): never {
  return assert.fail("unexpected rehearsal provider access");
}

test("the rehearsal dispatch requires explicit opt in and an approved host binding", async () => {
  const unused: Binding = { get host() { return unexpected(); }, get serving() { return unexpected(); },
    get inventorySource() { return unexpected(); } };
  for (const selection of ["", "false", "1", "TRUE"]) {
    assert.deepEqual(await runStagingAutomodDispatch({ ...runner, STAGING_AUTOMOD_REHEARSAL: selection }, unused),
      { verdict: "disabled" });
  }
  assert.deepEqual(await runStagingAutomodDispatch({}, unused), { verdict: "disabled" });
  assert.deepEqual(await runStagingAutomodDispatch(runner), { verdict: "failed", code: "host_unavailable" });
});

test("the bridge binding needs an explicit selection and the OIDC request variables", async t => {
  const oidc = { ACTIONS_ID_TOKEN_REQUEST_URL: "https://pipelines.actions.githubusercontent.com/token",
    ACTIONS_ID_TOKEN_REQUEST_TOKEN: "synthetic-request-token", OWNERSHIP_CONTROL_TOKEN: "c".repeat(43) };
  const fetch = t.mock.method(globalThis, "fetch", unexpected);
  assert.equal(await bindStagingAutomodDispatch({ ...runner, ...oidc }), undefined);
  const pin = { workerVersionId: serving.deploymentId, sourceSha: serving.sha, workerDeploymentId: "deployment",
    applicationId: "application", namespaceId: "namespace", rolloutId: "rollout", imageDigest: "sha256:fixture" };
  const change = { ...serving, action: "acquire", environment: "staging", runId: runner.GITHUB_RUN_ID,
    runAttempt: 1, expectedEpoch: lease.epoch, expiresAt: lease.expiresAt } as const;
  const plan = { serving, inventorySource, reservation: { pin, change } };
  assert.equal(await bindStagingAutomodDispatch({ ...runner, ...oidc, STAGING_AUTOMOD_REHEARSAL: "false" }, plan), undefined);
  assert.equal(await bindStagingAutomodDispatch(runner, plan), undefined);
  assert.equal(await bindStagingAutomodDispatch({ ...runner, ...oidc, ACTIONS_ID_TOKEN_REQUEST_TOKEN: "" }, plan), undefined);
  const binding = await bindStagingAutomodDispatch({ ...runner, ...oidc }, plan);
  assert.deepEqual([binding?.serving, binding?.inventorySource], [serving, inventorySource]);
  assert.equal(typeof binding?.wiring?.instance.acquireLease, "function");
  assert.deepEqual(binding?.wiring?.reservation, plan.reservation);
  assert.equal(fetch.mock.callCount(), 0);

  // A fetch-only stub is not an in-process host.
  const stub = { fetch: t.mock.fn(unexpected) };
  const result = await runStagingAutomodDispatch(runner, { serving, inventorySource,
    host: stub as unknown as Binding["host"] });
  assert.equal(result.verdict, "failed");
  assert.equal(stub.fetch.mock.callCount(), 0);
});

test("indirect or unprotected dispatch never reads the Actions run or rehearsal authority", async t => {
  const readRun = t.mock.fn(unexpected);
  const binding: Binding = { serving, inventorySource, host: { readRun, get authority() { return unexpected(); },
    readWorker: unexpected, readAuthenticatedInventory: unexpected, invokePrivate: unexpected } };
  for (const drift of [{ GITHUB_WORKFLOW_REF: runner.GITHUB_WORKFLOW_REF.replace(workflow, ".github/workflows/other.yml") },
    { GITHUB_REF: "refs/heads/other" }, { GITHUB_EVENT_NAME: "workflow_call" }, { GITHUB_REF_PROTECTED: "false" },
    { GITHUB_RUN_ATTEMPT: "2" }, { GITHUB_REPOSITORY_ID: "1" }]) {
    assert.deepEqual(await runStagingAutomodDispatch({ ...runner, ...drift }, binding), unconfirmed);
  }
  assert.equal(readRun.mock.callCount(), 0);
});

test("direct dispatch authenticates the current run before asking for the exact lease", async () => {
  const reads: unknown[] = [];
  let evidence = { ...run, head_sha: "d".repeat(40) };
  const binding: Binding = { serving, inventorySource, host: {
    readRun: async (id, signal) => { reads.push(id); assert.equal(signal.aborted, false); return evidence; },
    authority: { readIssuedLease: async (context, signal) => {
      reads.push(context); assert.equal(signal.aborted, false); return undefined;
    }, readApproval: unexpected },
    readWorker: unexpected, readAuthenticatedInventory: unexpected, invokePrivate: unexpected,
  } };
  assert.deepEqual(await runStagingAutomodDispatch(runner, binding), unconfirmed);
  assert.deepEqual(reads, [runner.GITHUB_RUN_ID]);
  evidence = run;
  assert.deepEqual(await runStagingAutomodDispatch(runner, binding), { verdict: "failed", code: "lease_unconfirmed" });
  assert.deepEqual(reads, [runner.GITHUB_RUN_ID, runner.GITHUB_RUN_ID, {
    ...serving, environment: "staging", runId: runner.GITHUB_RUN_ID, runAttempt: 1, target,
  }]);
});

test("direct dispatch refuses borrowed guild or lease approvals for a matching deployment", async t => {
  const now = Date.parse("2026-01-01T00:00:30Z");
  t.mock.timers.enable({ apis: ["Date"], now });
  const fetch = t.mock.method(globalThis, "fetch", unexpected);
  const issuedLease = { ...lease, ...serving, purpose: AUTOMOD_STAGING_PURPOSE,
    runId: runner.GITHUB_RUN_ID, runAttempt: 1, acquiredAt: now - 1_000, expiresAt: now + 1_000 } as const;
  const controlBearer = "synthetic-staging-control-bearer";
  type Host = Binding["host"];
  const readAuthenticatedInventory = t.mock.fn<Host["readAuthenticatedInventory"]>(async source => {
    assert.deepEqual(source, inventorySource);
    return { schema_version: 1, workflow_sha: inventorySource.workflowSha, workflow_run_id: inventorySource.runId,
      observed_at: new Date(now).toISOString(),
      worker: { deployment_id: "22222222-2222-2222-2222-222222222222", version_id: serving.deploymentId,
        source_sha: serving.sha, traffic_percent: 100, runtime_policy: "bounded" },
      container: { application_id: "33333333-3333-3333-3333-333333333333", namespace_id: "a".repeat(32),
        rollout_id: "44444444-4444-4444-4444-444444444444", rollout_status: "completed", image_digest: `sha256:${"b".repeat(64)}` },
      ownership: { phase: "active", epoch: issuedLease.epoch } };
  });
  const invokePrivate = t.mock.fn<Host["invokePrivate"]>(async () => {});
  const host: Host = {
    readRun: async () => run,
    authority: { readIssuedLease: async () => issuedLease, readApproval: async action => ({ action, target, issuedLease }) },
    readWorker: () => ({ controlBearer, env: { TWO_DEPLOY_ENV: "staging", STAGING_RUNTIME_POLICY: "bounded",
      DISCORD_APPLICATION_ID: target.applicationId, GUILD_ID: target.guildId, CONTAINER_RELEASE: serving.sha,
      CF_VERSION_METADATA: { id: serving.deploymentId }, OWNERSHIP_CONTROL_TOKEN: controlBearer } }),
    readAuthenticatedInventory,
    invokePrivate,
  };
  const binding = { serving, inventorySource, host };

  // The other run has the same serving build and ownership epoch. Inventory
  // alone cannot distinguish its lease or confirm an approval's guild.
  for (const [label, scope] of [
    ["another guild", { target: { ...target, guildId: "1000000000000000000" }, issuedLease }],
    ["another lease", { target, issuedLease: { ...issuedLease, runId: String(run.id + 2) } }],
  ] as const) {
    const readApproval = t.mock.fn<Host["authority"]["readApproval"]>(async action => ({ action, ...scope }));
    const authority = { ...host.authority, readApproval };
    assert.deepEqual(await runStagingAutomodDispatch(runner, { ...binding, host: { ...host, authority } }),
      { verdict: "failed", start: false, recover: false }, label);
    assert.deepEqual(readApproval.mock.calls.map(({ arguments: [action, original] }) => [action, original]),
      [["start", issuedLease], ["recover", issuedLease]], label);
    assert.equal(readAuthenticatedInventory.mock.callCount(), 0, label);
    assert.equal(invokePrivate.mock.callCount(), 0, label);
  }

  assert.deepEqual(await runStagingAutomodDispatch(runner, binding), { verdict: "passed", start: true, recover: true });
  assert.equal(readAuthenticatedInventory.mock.callCount(), 1);
  assert.deepEqual(invokePrivate.mock.calls.map(({ arguments: [, call] }) => call),
    [{ action: "start", issuedLease }, { action: "recover", issuedLease }]);
  assert.equal(fetch.mock.callCount(), 0);
});

test("the checked-in workflow command fails closed without an in-process host", () => {
  const script = fileURLToPath(new URL("../scripts/staging-automod-dispatch.ts", import.meta.url));
  const disabled = spawnSync(process.execPath, [script], { encoding: "utf8", env: {} });
  assert.deepEqual([disabled.status, disabled.stdout, disabled.stderr], [0, "", ""]);
  const selected = spawnSync(process.execPath, [script], { encoding: "utf8", env: runner });
  assert.deepEqual([selected.status, selected.stdout, selected.stderr], [1, "", "staging_automod_rehearsal_failed\n"]);
});
