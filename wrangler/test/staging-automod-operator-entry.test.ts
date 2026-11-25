import assert from "node:assert/strict";
import { test } from "node:test";
import { runStagingAutomodOperatorEntry } from "../scripts/staging-automod-operator-entry.ts";
import { STAGING_AUTOMOD_TARGET as target } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE } from "../src/staging-use-window.ts";
import { lease } from "./fixtures/staging-automod-recovery.ts";

type Providers = Parameters<typeof runStagingAutomodOperatorEntry>[2];
const path = ".github/workflows/staging-automod-preview.yml";
const head = "a".repeat(40);
const now = Date.parse("2026-01-01T00:00:30Z");
const issuedLease = { ...lease, deploymentId: "11111111-1111-1111-1111-111111111111",
  purpose: AUTOMOD_STAGING_PURPOSE, runAttempt: 1, acquiredAt: now - 1_000, expiresAt: now + 1_000 } as const;
const serving = { deploymentId: issuedLease.deploymentId, sha: issuedLease.sha };
const repository = { id: 1396224242 };
const runner = { GITHUB_REPOSITORY_ID: String(repository.id), GITHUB_WORKFLOW_REF: `TogetherWeOwn/two-bot-next/${path}@refs/heads/main`,
  GITHUB_REF: "refs/heads/main", GITHUB_REF_PROTECTED: "true", GITHUB_EVENT_NAME: "workflow_dispatch",
  GITHUB_RUN_ID: issuedLease.runId, GITHUB_RUN_ATTEMPT: "1", GITHUB_SHA: head, GITHUB_WORKFLOW_SHA: head };
const run = { id: Number(issuedLease.runId), event: "workflow_dispatch", run_attempt: 1, status: "in_progress",
  path, head_branch: "main", head_sha: head, repository, head_repository: repository };
const unconfirmed = { verdict: "failed", code: "context_unconfirmed" };

function unreachable(): never {
  return assert.fail("protected providers accessed before admission");
}

const blockedProviders: Providers = {
  get authority() { return unreachable(); }, readWorker: unreachable, readInventory: unreachable, invoke: unreachable,
};

test("default off and an untrusted runner access no providers", async () => {
  assert.deepEqual(await runStagingAutomodOperatorEntry({ runner, serving }, unreachable, blockedProviders),
    { verdict: "disabled" });
  for (const changes of [{ GITHUB_RUN_ATTEMPT: "2" }, { GITHUB_EVENT_NAME: "workflow_call" },
    { GITHUB_REF_PROTECTED: "false" }, { GITHUB_WORKFLOW_REF: runner.GITHUB_WORKFLOW_REF.replace(path, "other.yml") },
    { GITHUB_WORKFLOW_SHA: "b".repeat(40) }, { GITHUB_RUN_ID: "9007199254740993" }, { GITHUB_REPOSITORY_ID: "1" }]) {
    assert.deepEqual(await runStagingAutomodOperatorEntry({ enabled: true, runner: { ...runner, ...changes }, serving },
      unreachable, blockedProviders), unconfirmed);
  }
});

test("wrong or unreadable GitHub run evidence leaves rehearsal providers untouched", async () => {
  const options = { enabled: true, runner, serving };
  for (const changes of [{ id: run.id + 1 }, { run_attempt: 2 }, { event: "push" }, { status: "completed" },
    { path: `${path}@other` }, { head_branch: "other" }, { head_sha: "b".repeat(40) }, { head_repository: { id: 1 } }]) {
    const readRun = async () => ({ ...run, ...changes });
    assert.deepEqual(await runStagingAutomodOperatorEntry(options, readRun, blockedProviders), unconfirmed);
  }
  const unavailable = async () => { throw new Error("unavailable"); };
  assert.deepEqual(await runStagingAutomodOperatorEntry(options, unavailable, blockedProviders), unconfirmed);
});

test("the protected operator entry reaches the caller with the original serving lease", async t => {
  t.mock.timers.enable({ apis: ["Date"], now });
  const calls: Array<Parameters<Providers["invoke"]>[1]> = [];
  const controlBearer = "synthetic-staging-control-bearer";
  const providers: Providers = {
    authority: {
      readIssuedLease: async expected => {
        assert.deepEqual(expected, { ...serving, environment: "staging", target, runId: issuedLease.runId, runAttempt: 1 });
        return issuedLease;
      },
      readApproval: async (action, original) => {
        assert.deepEqual(original, issuedLease);
        return { action, target, issuedLease };
      },
    },
    readWorker: () => ({ controlBearer, env: { TWO_DEPLOY_ENV: "staging", STAGING_RUNTIME_POLICY: "bounded",
      DISCORD_APPLICATION_ID: target.applicationId, GUILD_ID: target.guildId, CONTAINER_RELEASE: serving.sha,
      CF_VERSION_METADATA: { id: serving.deploymentId }, OWNERSHIP_CONTROL_TOKEN: controlBearer } }),
    readInventory: async () => ({ schema_version: 1, observed_at: new Date(now).toISOString(),
      worker: { deployment_id: "22222222-2222-2222-2222-222222222222", version_id: serving.deploymentId,
        source_sha: serving.sha, traffic_percent: 100, runtime_policy: "bounded" },
      container: { application_id: "33333333-3333-3333-3333-333333333333", namespace_id: "a".repeat(32),
        rollout_id: "44444444-4444-4444-4444-444444444444", rollout_status: "completed", image_digest: `sha256:${"b".repeat(64)}` },
      ownership: { phase: "active", epoch: issuedLease.epoch } }),
    invoke: async (pin, call) => {
      assert.equal(pin.workerVersionId, serving.deploymentId);
      calls.push(call);
    },
  };
  const readRun = async (id: string, signal: AbortSignal) => {
    assert.equal(id, issuedLease.runId);
    assert.equal(signal.aborted, false);
    return run;
  };
  assert.deepEqual(await runStagingAutomodOperatorEntry({ enabled: true, runner, serving }, readRun, providers),
    { verdict: "passed", start: true, recover: true });
  assert.deepEqual(calls, [{ action: "start", issuedLease }, { action: "recover", issuedLease }]);
});
