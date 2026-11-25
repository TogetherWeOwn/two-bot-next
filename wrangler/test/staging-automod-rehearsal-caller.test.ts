import assert from "node:assert/strict";
import { test } from "node:test";
import { runStagingAutomodRehearsal } from "../scripts/staging-automod-rehearsal-caller.ts";
import { STAGING_AUTOMOD_TARGET as target } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE } from "../src/staging-use-window.ts";
import { lease } from "./fixtures/staging-automod-recovery.ts";

type Options = Parameters<typeof runStagingAutomodRehearsal>[0];
type Providers = Parameters<typeof runStagingAutomodRehearsal>[1];
const now = Date.parse("2026-01-01T00:00:30Z");
const issuedLease = { ...lease, deploymentId: "11111111-1111-1111-1111-111111111111",
  purpose: AUTOMOD_STAGING_PURPOSE, runAttempt: 1, acquiredAt: now - 1_000, expiresAt: now + 1_000 } as const;
const { environment, deploymentId, sha, runId, runAttempt } = issuedLease;
const authorityContext = { environment, deploymentId, sha, runId, runAttempt, target };
const context = { ...authorityContext, refProtected: true, eventName: "workflow_dispatch",
  workflowRef: "TogetherWeOwn/two-bot-next/.github/workflows/staging-automod-preview.yml@refs/heads/main" } as const;
const inventory = {
  schema_version: 1, observed_at: new Date(now - 1_000).toISOString(),
  worker: { deployment_id: "22222222-2222-2222-2222-222222222222", version_id: deploymentId,
    source_sha: sha, traffic_percent: 100, runtime_policy: "bounded" },
  container: { application_id: "33333333-3333-3333-3333-333333333333", namespace_id: "a".repeat(32),
    rollout_id: "44444444-4444-4444-4444-444444444444", rollout_status: "completed", image_digest: `sha256:${"b".repeat(64)}` },
  ownership: { phase: "active", epoch: issuedLease.epoch },
} as const;

function fixture(overrides?: Partial<Providers["authority"]>) {
  const access: string[] = [];
  const calls: Array<Parameters<Providers["invoke"]>> = [];
  const controlBearer = "synthetic-automod-rehearsal-control-value";
  const providers: Providers = {
    get authority(): Providers["authority"] {
      access.push("authority");
      return {
        readIssuedLease: async (expected, signal) => {
          access.push("lease");
          assert.deepEqual(expected, authorityContext);
          assert.equal(signal.aborted, false);
          return issuedLease;
        },
        readApproval: async (action, lease, signal) => {
          access.push(`approval:${action}`);
          assert.deepEqual(lease, issuedLease);
          assert.equal(signal.aborted, false);
          return { action, target, issuedLease };
        },
        ...overrides,
      };
    },
    readWorker: () => {
      access.push("credentials");
      return { controlBearer, env: { TWO_DEPLOY_ENV: "staging", STAGING_RUNTIME_POLICY: "bounded",
        DISCORD_APPLICATION_ID: target.applicationId, GUILD_ID: target.guildId, CONTAINER_RELEASE: sha,
        CF_VERSION_METADATA: { id: deploymentId }, OWNERSHIP_CONTROL_TOKEN: controlBearer } };
    },
    get readInventory() {
      access.push("inventory-binding");
      return async () => { access.push("inventory"); return inventory; };
    },
    get invoke(): Providers["invoke"] {
      access.push("invoke-binding");
      return async (...args) => { access.push(`invoke:${args[1].action}`); calls.push(args); };
    },
  };
  return { access, calls, providers };
}

test("default off and invalid protected context never access providers", async () => {
  const { access, providers } = fixture();
  for (const options of [{ context }, { enabled: false, context }, { enabled: "true", context }] as Options[]) {
    assert.deepEqual(await runStagingAutomodRehearsal(options, providers), { verdict: "disabled" });
  }
  for (const invalid of [undefined, { ...context, environment: "production" }, { ...context, refProtected: "true" },
    { ...context, refProtected: false }, { ...context, eventName: "pull_request" }, { ...context, runAttempt: 2 },
    { ...context, workflowRef: context.workflowRef.replace("main", "other") }, { ...context, target: null },
    { ...context, target: { ...target, guildId: "1" } }, { ...context, sha: "main" },
    { ...context, runId: 123 }, { ...context, deploymentId: "latest" }]) {
    const options = { enabled: true, context: invalid } as Options;
    assert.deepEqual(await runStagingAutomodRehearsal(options, providers), { verdict: "failed", code: "context_unconfirmed" });
  }
  assert.deepEqual(access, []);
});

test("opted in staging uses the original authority and exact serving pin for both actions", async t => {
  t.mock.timers.enable({ apis: ["Date"], now });
  const network = t.mock.method(globalThis, "fetch", async () => { throw new Error("unexpected network access"); });
  const { access, calls, providers } = fixture();

  assert.deepEqual(await runStagingAutomodRehearsal({ enabled: true, context }, providers),
    { verdict: "passed", start: true, recover: true });
  assert.deepEqual(access, ["authority", "lease", "approval:start", "approval:recover", "credentials",
    "inventory-binding", "invoke-binding", "inventory", "invoke:start", "invoke:recover"]);
  assert.deepEqual(calls.map(([, call]) => call), [{ action: "start", issuedLease }, { action: "recover", issuedLease }]);
  assert.deepEqual(calls[0]?.[0], { workerDeploymentId: inventory.worker.deployment_id, workerVersionId: deploymentId,
    sourceSha: sha, applicationId: inventory.container.application_id, namespaceId: inventory.container.namespace_id,
    rolloutId: inventory.container.rollout_id, imageDigest: inventory.container.image_digest });
  assert.equal(calls[0]?.[0], calls[1]?.[0]);
  assert.equal(network.mock.callCount(), 0);
});

test("an unconfirmed lease stops before approval and transport", async () => {
  const { access, providers } = fixture({
    readIssuedLease: async () => ({ ...issuedLease, runId: "1" }),
  });
  assert.deepEqual(await runStagingAutomodRehearsal({ enabled: true, context }, providers),
    { verdict: "failed", code: "lease_unconfirmed" });
  assert.deepEqual(access, ["authority"]);
});

test("an unavailable start approval still permits separately approved recovery", async t => {
  t.mock.timers.enable({ apis: ["Date"], now });
  const { access, calls, providers } = fixture({
    readApproval: async action => action === "recover" ? { action, target, issuedLease } : undefined,
  });
  assert.deepEqual(await runStagingAutomodRehearsal({ enabled: true, context }, providers),
    { verdict: "failed", start: false, recover: true });
  assert.deepEqual(access, ["authority", "lease", "credentials", "inventory-binding", "invoke-binding", "inventory", "invoke:recover"]);
  assert.deepEqual(calls.map(([, call]) => call), [{ action: "recover", issuedLease }]);
});

test("missing approvals leave credentials and transport untouched", async () => {
  const { access, providers } = fixture({ readApproval: async () => undefined });
  assert.deepEqual(await runStagingAutomodRehearsal({ enabled: true, context }, providers),
    { verdict: "failed", start: false, recover: false });
  assert.deepEqual(access, ["authority", "lease"]);
});
