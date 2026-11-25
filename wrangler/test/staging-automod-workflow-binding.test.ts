import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { runStagingAutomodBoundJob } from "../scripts/staging-automod-private-job.ts";
import { STAGING_AUTOMOD_TARGET as target } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE } from "../src/staging-lifecycle.ts";
import { lease, now } from "./fixtures/staging-automod-recovery.ts";

const issuedLease = { ...lease, purpose: AUTOMOD_STAGING_PURPOSE, runAttempt: 1 } as const;
const { environment, runId, runAttempt, sha, deploymentId } = issuedLease;
const context = { environment, runId, runAttempt, sha, deploymentId, target };
const approvals = {
  start: { action: "start", target, issuedLease },
  recover: { action: "recover", target, issuedLease },
} as const;
const controlBearer = "fixture-ownership-control";
const env = {
  TWO_DEPLOY_ENV: "staging", STAGING_RUNTIME_POLICY: "bounded",
  DISCORD_APPLICATION_ID: target.applicationId, GUILD_ID: target.guildId,
  OWNERSHIP_CONTROL_TOKEN: controlBearer, CF_VERSION_METADATA: { id: deploymentId }, CONTAINER_RELEASE: sha,
};
type Invoke = Parameters<typeof runStagingAutomodBoundJob>[3]["invoke"];
const start = { action: "start", issuedLease };
const recover = { action: "recover", issuedLease };

test("the protected preview job stays dormant and names the synthetic binding step", () => {
  const workflow = readFileSync(new URL("../../.github/workflows/staging-automod-preview.yml", import.meta.url), "utf8")
    .split("\n").filter(line => !/^\s*(?:#|$)/.test(line)).join("\n");
  assert.match(workflow, /^on:\n  workflow_call:\npermissions: \{\}\n/m);
  assert.match(workflow, /^concurrency:\n  group: staging-lifecycle\n  cancel-in-progress: false\n  queue: max\n/m);
  assert.ok(workflow.includes(`\njobs:
  dry-run:
    if: >-
      github.workflow_ref == 'TogetherWeOwn/two-bot-next/.github/workflows/staging-automod-preview.yml@refs/heads/main' &&
      github.event_name == 'workflow_dispatch' && github.ref_protected && github.run_attempt == 1
    environment: {name: staging, deployment: false}
    permissions:
      contents: read
`), "preview job admission changed");
  assert.ok(workflow.includes(`\n          ref: \${{ github.sha }}
          persist-credentials: false
          fetch-depth: 1
`), "preview checkout scope changed");
  assert.ok(workflow.endsWith(`\n      - name: Exercise the private AutoMod binding with synthetic authority
        run: node --test wrangler/test/staging-automod-workflow-binding.test.ts`), "synthetic binding step changed");
  assert.doesNotMatch(workflow, /\bsecrets\b/);
});

test("the workflow binding confirms both actions on the issued lease", async t => {
  t.mock.timers.enable({ apis: ["Date"], now: now() });
  const fetch = t.mock.method(globalThis, "fetch", async () => { throw new Error("unexpected network request"); });
  const invoke = t.mock.fn<Invoke>(async () => {});
  assert.deepEqual(await runStagingAutomodBoundJob(context, issuedLease, approvals, { env, controlBearer, invoke }),
    { verdict: "passed", start: true, recover: true });
  assert.deepEqual(invoke.mock.calls.map(call => call.arguments[0]), [start, recover]);

  invoke.mock.resetCalls();
  assert.deepEqual(await runStagingAutomodBoundJob(context, issuedLease, { start: approvals.start },
    { env, controlBearer, invoke }), { verdict: "failed", start: true, recover: false });
  assert.deepEqual(invoke.mock.calls.map(call => call.arguments[0]), [start]);
  assert.equal(fetch.mock.callCount(), 0);
});

test("the workflow binding preserves scope and reports unconfirmed actions", async t => {
  t.mock.timers.enable({ apis: ["Date"], now: now() });
  const invoke = t.mock.fn<Invoke>(async call => {
    if (call.action === "start") {
      t.mock.timers.setTime(issuedLease.expiresAt + 1);
      throw new Error("synthetic startup confirmation lost");
    }
  });
  const worker = { env, controlBearer, invoke };

  assert.deepEqual(await runStagingAutomodBoundJob({ ...context, runAttempt: 2 }, issuedLease, approvals, worker),
    { verdict: "failed", code: "lease_unconfirmed" });
  assert.deepEqual(await runStagingAutomodBoundJob(context, issuedLease, approvals,
    { ...worker, controlBearer: "unrelated-fixture-credential" }), { verdict: "failed", start: false, recover: false });
  assert.deepEqual(await runStagingAutomodBoundJob(context, issuedLease,
    { start: approvals.recover, recover: approvals.start }, worker), { verdict: "failed", start: false, recover: false });
  assert.equal(invoke.mock.callCount(), 0);

  assert.deepEqual(await runStagingAutomodBoundJob(context, issuedLease, approvals, worker),
    { verdict: "failed", start: false, recover: true });
  assert.deepEqual(invoke.mock.calls.map(call => call.arguments[0]), [start, recover]);
});
