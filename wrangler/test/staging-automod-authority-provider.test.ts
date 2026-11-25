import assert from "node:assert/strict";
import { test, type TestContext } from "node:test";
import { TwoBotContainer, type Env } from "../src/index.ts";
import { OWNER_KEY } from "../src/ownership.ts";
import { bindStagingAutomodAuthorityProvider, type ReadProtectedAutomodApproval } from "../src/staging-automod-authority-provider.ts";
import { boundStagingAutomodAuthority } from "../src/staging-automod-bounded-authority.ts";
import { readStagingAutomodJobInputs } from "../src/staging-automod-inputs.ts";
import { STAGING_AUTOMOD_TARGET as target } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE, STAGING_USE_KEY, StagingLifecycle, type StagingIdentity } from "../src/staging-lifecycle.ts";
import { lease, now } from "./fixtures/staging-automod-recovery.ts";

const { environment, deploymentId, sha, runId, expiresAt, epoch } = lease;
type Context = NonNullable<Parameters<typeof readStagingAutomodJobInputs>[0]>;
const context: Context = { environment, deploymentId, sha, runId, runAttempt: 1, target };
const owner = { deploymentId, epoch, phase: "active", actor: "synthetic", timestamp: new Date(now()).toISOString(),
  oldEpoch: epoch - 1, oldDeploymentId: null } as const;
const token = "synthetic-automod-control-token-fixture";
const request = new Request("https://worker.invalid", { headers: { authorization: `Bearer ${token}` } });
const env = { TWO_DEPLOY_ENV: "staging", STAGING_RUNTIME_POLICY: "bounded", OWNERSHIP_CONTROL_TOKEN: token,
  DISCORD_APPLICATION_ID: target.applicationId, GUILD_ID: target.guildId, CF_VERSION_METADATA: { id: deploymentId },
  CONTAINER_RELEASE: sha } as const;

async function fixture(t: TestContext) {
  t.mock.timers.enable({ apis: ["Date"], now: now() });
  const records = new Map<string, unknown>();
  const storage = {
    get: async (key: string) => structuredClone(records.get(key)),
    put: async (entries: Record<string, unknown>) => {
      for (const [key, value] of Object.entries(entries)) records.set(key, structuredClone(value));
    },
    sync: async () => {},
  };
  const lifecycle = new StagingLifecycle(storage as unknown as DurableObjectStorage);
  const identity: StagingIdentity = { deploymentId, sha, owner };
  const issued = await lifecycle.acquire({ environment, deploymentId, sha, runId, runAttempt: 1, expiresAt,
    expectedEpoch: epoch, action: "acquire" }, identity, async () => {}, async () => {}, now(), AUTOMOD_STAGING_PURPOSE);
  const issuedRecords = structuredClone(records);
  const readLease = t.mock.fn((scope: typeof context) => lifecycle.readIssuedAutomodLease(scope, identity));
  const approvals = {
    start: { action: "start", target, issuedLease: issued }, recover: { action: "recover", target, issuedLease: issued },
  } as const;
  function bind(read: ReadProtectedAutomodApproval, control = request) {
    const approve = t.mock.fn(read);
    const provider = bindStagingAutomodAuthorityProvider(control, env, readLease, approve);
    return { provider, approve, inputs: (scope = context) => readStagingAutomodJobInputs(scope, boundStagingAutomodAuthority(provider)) };
  }
  return { issued, issuedRecords, records, identity, readLease, approvals, bind };
}

test("authenticated authority reads the issued tuple and both protected approvals", async t => {
  const h = await fixture(t);
  const authority = h.bind(async action => h.approvals[action]);
  const inputs = await authority.inputs();
  assert.deepEqual(inputs, { context, issuedLease: h.issued, approvals: h.approvals });
  assert.deepEqual(authority.approve.mock.calls.map(({ arguments: [action, issued, signal] }) =>
    [action, issued, signal.aborted]).sort(), [["recover", h.issued, false], ["start", h.issued, false]]);
  assert.deepEqual(h.records, h.issuedRecords);
});

test("authority refuses unauthenticated, borrowed and stale scope", async t => {
  const h = await fixture(t);
  const unauthenticated = h.bind(async action => h.approvals[action], new Request(request.url));
  assert.equal(await unauthenticated.inputs(), undefined);
  assert.equal(h.readLease.mock.callCount(), 0);
  const authority = h.bind(async action => h.approvals[action]);
  const signal = new AbortController().signal;
  assert.equal(await authority.inputs({ ...context, runId: "99999" }), undefined);
  for (const changed of [{ epoch: epoch + 1 }, { acquiredAt: h.issued.acquiredAt + 1 }, { expiresAt: expiresAt + 1 }]) {
    assert.equal(await authority.provider.readApproval("recover", { ...h.issued, ...changed }, signal), undefined);
  }
  h.identity.owner = { ...owner, epoch: epoch + 1 };
  assert.equal(await authority.inputs(), undefined);
  assert.equal(authority.approve.mock.callCount(), 0);
});

test("wrong-action, wrong-target and mismatched tuple records deny only the affected action", async t => {
  const h = await fixture(t);
  const signal = new AbortController().signal;
  for (const invalid of [h.approvals.recover, { ...h.approvals.start, target: { ...target, guildId: "123" } },
    { ...h.approvals.start, issuedLease: { ...h.issued, expiresAt: expiresAt + 1 } }]) {
    const authority = h.bind(async action => action === "start" ? invalid : h.approvals.recover);
    assert.equal(await authority.provider.readApproval("start", h.issued, signal), undefined);
    const inputs = await authority.inputs();
    assert.deepEqual(inputs?.approvals, { recover: h.approvals.recover });
  }
});

test("unavailable start preserves recovery on the original lease through expiry", async t => {
  const h = await fixture(t);
  const authority = h.bind(async action => {
    if (action === "start") throw new Error("synthetic start read unavailable");
    return h.approvals.recover;
  });
  const expected = { context, issuedLease: h.issued, approvals: { recover: h.approvals.recover } };
  assert.deepEqual(await authority.inputs(), expected);
  t.mock.timers.setTime(expiresAt);
  assert.deepEqual(await authority.inputs(), expected);
  assert.equal(authority.approve.mock.calls.filter(call => call.arguments[0] === "start").length, 1);
  assert.deepEqual(h.records, h.issuedRecords);
});

test("lease retirement during an approval read invalidates the result", async t => {
  const h = await fixture(t);
  const authority = h.bind(async action => {
    h.records.set(STAGING_USE_KEY, { ...h.issued, phase: "released" });
    return h.approvals[action];
  });
  assert.deepEqual((await authority.inputs())?.approvals, {});
});

test("Container authority reads under the gate and refuses a fenced owner", async t => {
  const h = await fixture(t);
  h.records.set(OWNER_KEY, owner);
  const initialization: Promise<unknown>[] = [];
  let insideGate = false;
  const reads = t.mock.fn((key: string) => {
    if (key === OWNER_KEY || key === STAGING_USE_KEY) assert.equal(insideGate, true);
    return structuredClone(h.records.get(key));
  });
  const bot = new TwoBotContainer({
    container: { running: false },
    storage: {
      get: async (key: string) => reads(key),
      put: async (key: string, value: unknown) => { h.records.set(key, structuredClone(value)); },
      kv: { get: () => undefined },
      sql: { exec: () => [] },
      deleteAlarm: async () => {},
      sync: async () => {},
    },
    blockConcurrencyWhile<T>(operation: () => Promise<T>) {
      // Serialize the real SDK and Bot constructor gates as a DO does.
      const result = Promise.all(initialization.slice(-1)).then(async () => {
        insideGate = true;
        try { return await operation(); }
        finally { insideGate = false; }
      });
      initialization.push(result);
      return result;
    },
  } as unknown as DurableObjectState<{}>, env as Env);
  await Promise.all(initialization);
  reads.mock.resetCalls();

  const approve = t.mock.fn<ReadProtectedAutomodApproval>(async action => h.approvals[action]);
  const authority = TwoBotContainer.bindProtectedAutomodAuthority(bot, request, approve);
  assert.deepEqual(await readStagingAutomodJobInputs(context, boundStagingAutomodAuthority(authority)),
    { context, issuedLease: h.issued, approvals: h.approvals });
  assert.ok(reads.mock.calls.some(call => call.arguments[0] === OWNER_KEY));
  assert.equal(approve.mock.callCount(), 2);

  h.records.set(OWNER_KEY, { ...owner, phase: "fenced" });
  assert.equal(await readStagingAutomodJobInputs(context, boundStagingAutomodAuthority(authority)), undefined);
  assert.equal(approve.mock.callCount(), 2);
});
