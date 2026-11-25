import assert from "node:assert/strict";
import { test } from "node:test";
import { runStagingAutomodDryRun } from "../scripts/staging-automod-workflow.ts";
import { runProtectedAutomod } from "../src/staging-automod-caller.ts";
import { runProtectedAutomodJob } from "../src/staging-automod-job.ts";
import { StagingAutomodLifecycle } from "../src/staging-automod-lifecycle.ts";
import { STAGING_AUTOMOD_TARGET as target } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE, STAGING_USE_KEY, StagingLifecycle, type StagingUse } from "../src/staging-lifecycle.ts";
import publication from "./fixtures/staging-automod-publication.json" with { type: "json" };
import { lease, now, Storage } from "./fixtures/staging-automod-recovery.ts";

const publicationLease = { ...lease, runAttempt: 1 } as const;
const issuedLease = { ...publicationLease, purpose: AUTOMOD_STAGING_PURPOSE } as const;
const { runId, runAttempt, sha } = issuedLease;
const job = { runId, runAttempt, sha };
const passed = { verdict: "passed", start: true, recover: true };
const startupUnconfirmed = { verdict: "failed", start: false, recover: true };
type Invoke = Parameters<typeof runStagingAutomodDryRun>[2];
type Receipt = Awaited<ReturnType<typeof runProtectedAutomodJob>>;

function syntheticBoundary(effect: (action: "start" | "recover") => void = () => {},
  reply: (receipt: Receipt) => Response | Promise<Response> = receipt => Response.json(receipt)): Invoke {
  const approvals = {
    start: { action: "start", issuedLease, target },
    recover: { action: "recover", issuedLease, target },
  } as const;
  return async request => {
    const receipt = await runProtectedAutomodJob(request, approvals[request.action], call => {
      assert.deepEqual(call, { action: request.action, issuedLease });
      effect(call.action);
      return Promise.resolve();
    });
    return reply(receipt);
  };
}

test("the exact protected staging job uses the sole AutoMod lease for startup and recovery", async t => {
  t.mock.timers.enable({ apis: ["Date"], now: now() });
  const calls: string[] = [];
  const invoke = t.mock.fn(syntheticBoundary(action => calls.push(action)));
  for (const wrongLease of [{ ...issuedLease, purpose: undefined }, { ...issuedLease, voiceMove: true },
    { ...issuedLease, environment: "production" }, { ...issuedLease, sha: "b".repeat(40) },
    { ...issuedLease, runId: "999" }, { ...issuedLease, runAttempt: 2 }]) {
    assert.deepEqual(await runStagingAutomodDryRun(job, wrongLease as typeof issuedLease, invoke),
      { verdict: "failed", code: "lease_unconfirmed" });
  }
  assert.deepEqual(await runStagingAutomodDryRun({ ...job, runAttempt: 2 },
    { ...issuedLease, runAttempt: 2 }, invoke), { verdict: "failed", code: "lease_unconfirmed" });
  assert.equal(invoke.mock.callCount(), 0);
  assert.deepEqual(await runStagingAutomodDryRun(job, issuedLease, invoke), passed);
  assert.deepEqual(calls, ["start", "recover"]);
});

test("a failed startup still recovers the original lease after expiry and keeps the job red", async t => {
  t.mock.timers.enable({ apis: ["Date"], now: now() });
  let released = false;
  const invoke = syntheticBoundary(action => {
    if (action === "start") {
      t.mock.timers.setTime(issuedLease.expiresAt + 1);
      throw new Error("synthetic startup failure after taking effect");
    }
    released = true;
  });
  assert.deepEqual(await runStagingAutomodDryRun(job, issuedLease, invoke), startupUnconfirmed);
  assert.equal(released, true);
});

test("the startup deadline reaches recovery when the invoker loses the reply", async t => {
  t.mock.timers.enable({ apis: ["Date", "setTimeout"], now: now() });
  let started!: () => void;
  const attempted = new Promise<void>(resolve => { started = resolve; });
  const calls: string[] = [];
  const invoke = syntheticBoundary(action => calls.push(action), receipt => {
    if ("action" in receipt && receipt.action === "start") {
      started();
      return new Promise(() => {});
    }
    return Response.json(receipt);
  });
  const run = runStagingAutomodDryRun(job, issuedLease, invoke);
  await attempted;
  t.mock.timers.tick(15_000);
  assert.deepEqual(await run, startupUnconfirmed);
  assert.deepEqual(calls, ["start", "recover"]);
});

test("only bounded, exact receipt evidence can confirm either action", async t => {
  t.mock.timers.enable({ apis: ["Date"], now: now() });
  for (const mutate of [
    (receipt: Receipt) => new Response(JSON.stringify(receipt) + " ".repeat(1024)),
    () => new Response(Uint8Array.of(0xff)),
    () => new Response("{"),
    (receipt: Receipt) => Response.json({ ...receipt, detail: "synthetic debug" }),
    (receipt: Receipt) => Response.json({ ...receipt, lease: { ...issuedLease, runId: "999" } }),
    (receipt: Receipt) => Response.json({ ...receipt, lease: { ...issuedLease, epoch: issuedLease.epoch + 1 } }),
    (receipt: Receipt) => Response.json({ ...receipt, lease: { ...issuedLease, purpose: null } }),
    (receipt: Receipt) => Response.json({ ...receipt,
      action: "action" in receipt && receipt.action === "recover" ? "start" : "recover" }),
    (receipt: Receipt) => Response.json({ ...receipt, observedAt: "yesterday" }),
    (receipt: Receipt) => Response.json(receipt, { status: 503 }),
  ]) {
    for (const action of ["start", "recover"] as const) {
      const invoke = syntheticBoundary(undefined, receipt =>
        "action" in receipt && receipt.action === action ? mutate(receipt) : Response.json(receipt));
      assert.deepEqual(await runStagingAutomodDryRun(job, issuedLease, invoke),
        { ...passed, verdict: "failed", [action]: false });
    }
  }
  const refusedCleanup = syntheticBoundary(action => {
    if (action === "recover") throw new Error("synthetic cleanup refusal");
  });
  assert.deepEqual(await runStagingAutomodDryRun(job, issuedLease, refusedCleanup),
    { verdict: "failed", start: true, recover: false });
});

test("the pinned rehearsal passes only with published preview and verified cleanup receipts", async t => {
  t.mock.timers.enable({ apis: ["Date"], now: now() });
  for (const cleanupProven of [true, false]) {
    await t.test(cleanupProven ? "independent absence confirmed" : "independent absence reply lost", async () => {
      const { definition, identity, replies: { help, preview } } = publication;
      const env = { ...publication.env, STAGING_RUNTIME_POLICY: "bounded", TWO_MODERATION: "1", TWO_OWEN_USER_ID: preview.id };
      const journalStorage = new Storage();
      let storedLease: StagingUse = issuedLease;
      const storage = {
        get: async (key: string) => key === STAGING_USE_KEY ? structuredClone(storedLease) : journalStorage.get(key),
        put: async (key: string | Record<typeof STAGING_USE_KEY, StagingUse>, value?: unknown) => {
          if (typeof key === "string") await journalStorage.put(key, value);
          else storedLease = structuredClone(key[STAGING_USE_KEY]);
        },
        sync: () => journalStorage.sync(),
      } as unknown as DurableObjectStorage;
      const staging = new StagingLifecycle(storage);
      const owner = { deploymentId: issuedLease.deploymentId, epoch: issuedLease.epoch, phase: "active" as const,
        oldEpoch: issuedLease.epoch - 1, oldDeploymentId: null, actor: "fixture", timestamp: new Date().toISOString() };
      const authority = { storage, staging, env,
        identity: async () => ({ deploymentId: issuedLease.deploymentId, sha, owner }),
        exclusive: <T>(operation: () => Promise<T>) => operation() };
      const cleanupBefore = issuedLease.expiresAt + 5_000;
      const policy = { memberIntake: false, enforcement: false, resume: false, registrySync: false,
        interactionDispatch: false, dedicatedIntents: true };
      const api = "https://discord.com/api/v10";
      const commands = `${api}/applications/${target.applicationId}/guilds/${target.guildId}/commands`;
      const detail = `${commands}/${preview.id}`;
      const effectsSeen: string[] = [];
      let command: typeof preview | undefined;
      let running = false;
      let absenceReads = 0;
      const rows = () => command ? [help, command] : [help];
      const effects = {
        enabled: true,
        dryRun: { policy, start: async () => { effectsSeen.push("start"); running = true; } },
        readRuntime: async () => ({ lease: issuedLease, running, policy }),
        readCleanupGrant: async () => ({ lease: issuedLease, before: cleanupBefore }),
        fenceAndDrain: async () => {
          effectsSeen.push("drain");
          return { lease: issuedLease, publicationDrained: true, startupDrained: true };
        },
        stop: async () => { effectsSeen.push("stop"); running = false; return true; },
        request: async (url: string, init: RequestInit) => {
          assert.equal(running, false);
          if (init.method === "POST") {
            assert.equal(url, commands);
            assert.deepEqual(JSON.parse(String(init.body)), definition);
            effectsSeen.push("publish");
            command = preview;
            return Response.json(command, { status: 201 });
          }
          if (init.method === "DELETE") {
            assert.equal(url, detail);
            assert.deepEqual(command, preview);
            effectsSeen.push("delete");
            command = undefined;
            return new Response(null, { status: 204 });
          }
          assert.equal(init.method, "GET");
          if (url === `${api}/users/@me`) return Response.json(identity);
          if (url === `${commands}?with_localizations=true`) return Response.json(rows());
          assert.equal(url, detail);
          if (command) return Response.json(command);
          // The delete and its readback succeed. The journal still owes an independent absence check.
          if (++absenceReads === 2 && !cleanupProven) throw new Error("synthetic absence reply lost");
          return Response.json({ code: 10063 }, { status: 404 });
        },
      };
      const lifecycle = new StagingAutomodLifecycle({ ...authority, ...effects,
        observe: async () => ({
          ...await staging.automodBinding(issuedLease, await authority.identity(), Date.now()),
          purpose: AUTOMOD_STAGING_PURPOSE, running,
        }),
        get: async url => {
          if (url === `${api}/users/@me`) return identity;
          assert.equal(url, commands);
          return rows();
        },
      });
      const { intent, id, version } = await lifecycle.publish({
        lease: publicationLease, origin: "https://two-bot-next-staging.example.workers.dev", purposes: [AUTOMOD_STAGING_PURPOSE],
        verifiedRecoveryPair: true, verifiedRuntimeDatabase: true, verifiedGatewayIntents: true, cleanupReserveMs: 5_000,
      });
      assert.deepEqual({ id, version }, { id: preview.id, version: preview.version });

      const approvals = {
        start: { action: "start", issuedLease, target },
        recover: { action: "recover", issuedLease, target },
      } as const;
      const receipts: Receipt[] = [];
      const result = await runStagingAutomodDryRun(job, issuedLease, async request => {
        const receipt = await runProtectedAutomodJob(request, approvals[request.action],
          call => runProtectedAutomod(call, authority, effects));
        receipts.push(receipt);
        return Response.json(receipt);
      });
      const metadata = { schema: "two-bot-staging-automod-job.v1", environment: "staging",
        observedAt: new Date().toISOString(), lease: issuedLease };
      assert.deepEqual(receipts, [
        { ...metadata, action: "start", verdict: "passed" },
        { ...metadata, action: "recover", ...(cleanupProven ? { verdict: "passed" }
          : { verdict: "failed", code: "operation_unconfirmed" }) },
      ]);
      assert.deepEqual(result, cleanupProven ? passed : { verdict: "failed", start: true, recover: false });
      assert.deepEqual(effectsSeen, ["publish", "start", "drain", "stop", "delete"]);
      assert.equal(absenceReads, 2);
      assert.deepEqual(rows(), [help]);
      assert.equal(running, false);
      // Read the committed value directly so the assertion cannot flush a staged write.
      assert.deepEqual(journalStorage.value, { intent, created: { id, version }, phase: cleanupProven ? "complete" : "cleanup" });
      assert.equal(journalStorage.staged, undefined);
      assert.deepEqual(await staging.read(), { ...issuedLease, phase: cleanupProven ? "released" : "active" });
    });
  }
});
