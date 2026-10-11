import { test } from "node:test";
import assert from "node:assert/strict";
import { control } from "../scripts/production-ownership-control.mjs";

const PRODUCTION = "https://two-bot-next-production.fixture.workers.dev/";
const STAGING = "https://two-bot-next-staging.fixture.workers.dev/";
const args = {
  url: PRODUCTION, productionUrl: PRODUCTION, stagingUrl: STAGING,
  token: "synthetic-control-token-not-a-secret-12345",
  actor: "fixture-operator", expectedEpoch: 1, expectedDeploymentId: "B",
};
function sender(owner: object | null, deploymentId = "B", running = false) {
  const calls: RequestInit[] = [];
  return {
    calls,
    send: async (_url: URL, init: RequestInit) => {
      calls.push(init);
      if (init.method === "GET") return Response.json({ deploymentId, owner, running });
      const change = JSON.parse(String(init.body));
      return Response.json({ deploymentId, running: false, owner: {
        deploymentId: change.action === "fence" ? null : deploymentId, epoch: change.expectedEpoch + 1,
        phase: change.action === "fence" ? "fenced" : "active",
      } });
    },
  };
}

test("client pins the approved production origin before any request", async () => {
  const noSend = () => { throw new Error("must not send"); };
  // Staging, attacker, path/query/port/credentials, non-https: all refused.
  for (const url of [STAGING, "https://attacker.invalid/", `${PRODUCTION}unexpected`, `${PRODUCTION}?secret=x`,
      "http://two-bot-next-production.fixture.workers.dev/", "https://two-bot-next-production.fixture.workers.dev:8443/"]) {
    await assert.rejects(control({ ...args, url, action: "takeover" }, noSend));
  }
  // URL that is not the approved production origin: refused.
  await assert.rejects(control({ ...args, url: "https://other-production.fixture.workers.dev/" }, noSend));
  // Production URL equal to the staging URL: refused.
  await assert.rejects(control({ ...args, stagingUrl: PRODUCTION }, noSend));
  // Missing/short token and unknown action: refused without sending.
  await assert.rejects(control({ ...args, token: "", action: "preflight" }, noSend));
  await assert.rejects(control({ ...args, action: "deploy" }, noSend));
  assert.deepEqual(await control({ ...args, action: "preflight" }, noSend), { configured: true });
});

test("status reads without starting and refuses deployment or running mismatches", async () => {
  const s = sender({ epoch: 3, deploymentId: "B", phase: "active" });
  const state = await control({ ...args, action: "status" }, s.send);
  assert.equal(state.owner.epoch, 3);
  assert.equal(s.calls.length, 1);
  assert.equal(s.calls[0]!.redirect, "error");
  // Serving version is still not the deployed one after the propagation
  // window: NO-GO, no POST.
  const moved = sender({ epoch: 3, deploymentId: "B", phase: "active" }, "C");
  await assert.rejects(control({ ...args, action: "status" }, moved.send, async () => {}), /deployment mismatch/);
  assert.equal(moved.calls.length, 34);
  assert.ok(moved.calls.every((call) => call.method === "GET"));
  // Container already running before takeover: NO-GO.
  const running = sender({ epoch: 3, deploymentId: "B", phase: "active" }, "B", true);
  await assert.rejects(control({ ...args, action: "status" }, running.send), /not confirmed/);
  assert.equal(running.calls.length, 1);
  // Malformed control response: refused.
  await assert.rejects(control({ ...args, action: "status" }, async () => Response.json({ bogus: true })));
});

test("takeover posts exactly the read epoch and confirms the transition", async () => {
  const s = sender({ epoch: 3, deploymentId: "B", phase: "active" });
  const result = await control({ ...args, action: "takeover", expectedEpoch: 3 }, s.send);
  assert.equal(result.owner.epoch, 4);
  assert.equal(result.owner.phase, "active");
  assert.equal(s.calls.length, 2);
  assert.deepEqual(JSON.parse(String(s.calls[1]!.body)), { action: "takeover", actor: args.actor, expectedEpoch: 3 });
});

test("routine takeover refuses to unpark a fenced/pristine singleton", async () => {
  for (const owner of [null, { epoch: 2, deploymentId: null, phase: "fenced" }]) {
    const s = sender(owner);
    await assert.rejects(control({ ...args, action: "takeover", expectedEpoch: 2 }, s.send), /explicit production release required/);
    assert.equal(s.calls.length, 1);
  }
  // Explicit release initializes at epoch 0 and requires an actor.
  const s = sender(null);
  assert.equal((await control({ ...args, action: "takeover", expectedEpoch: 0, releaseFence: true }, s.send)).owner.epoch, 1);
  await assert.rejects(control({ ...args, actor: "", action: "takeover" }, sender(null).send));
});

test("fence parks all versions and confirms the parked shape", async () => {
  const s = sender({ epoch: 3, deploymentId: "B", phase: "active" });
  const result = await control({ ...args, action: "fence", expectedEpoch: 3 }, s.send);
  assert.equal(result.owner.phase, "fenced");
  assert.equal(result.owner.deploymentId, null);
  assert.deepEqual(JSON.parse(String(s.calls[1]!.body)), { action: "fence", actor: args.actor, expectedEpoch: 3 });
});

test("client never retries authentication, validation or epoch conflicts", async () => {
  for (const status of [401, 400, 405, 409]) {
    let calls = 0;
    await assert.rejects(control({ ...args, action: "takeover", takeoverRetryDelayMs: 1 }, async () => {
      calls++;
      return new Response("synthetic-private-body", { status });
    }), new RegExp(`HTTP ${status}`));
    assert.equal(calls, 1);
  }
  // A POST conflict is not retried either.
  let calls = 0;
  await assert.rejects(control({ ...args, action: "takeover", takeoverRetryDelayMs: 1 }, async (_url, init) => {
    calls++;
    if (init.method === "GET") return Response.json({ deploymentId: "B", owner: { epoch: 3, deploymentId: "B", phase: "active" }, running: false });
    return new Response("conflict", { status: 409 });
  }), /HTTP 409/);
  assert.equal(calls, 2);
});

test("client retries only 5xx takeover POSTs, re-reading first", async () => {
  const script = [
    { method: "GET", status: 200, body: { deploymentId: "B", owner: { epoch: 3, deploymentId: "B", phase: "active" }, running: false } },
    { method: "POST", status: 503, body: { error: "ownership_fenced", reason: "storage_unavailable" } },
    { method: "GET", status: 200, body: { deploymentId: "B", owner: { epoch: 3, deploymentId: "B", phase: "active" }, running: false } },
    { method: "POST", status: 200, body: { deploymentId: "B", owner: { epoch: 4, deploymentId: "B", phase: "active" }, running: false } },
  ];
  const calls: string[] = [];
  const waits: number[] = [];
  const result = await control({ ...args, action: "takeover", expectedEpoch: 3, takeoverRetryDelayMs: 5 }, async (_url, init) => {
    calls.push(init.method!);
    const entry = script.shift()!;
    assert.equal(entry.method, init.method);
    return new Response(JSON.stringify(entry.body), { status: entry.status });
  }, async (ms) => { waits.push(ms); });
  assert.equal(result.owner.epoch, 4);
  assert.deepEqual(calls, ["GET", "POST", "GET", "POST"]);
  assert.deepEqual(waits, [5]);
});

test("a 5xx after a committed takeover is confirmed by GET, never re-posted", async () => {
  const calls: string[] = [];
  const result = await control({ ...args, action: "takeover", expectedEpoch: 3, takeoverRetryDelayMs: 5 }, async (_url, init) => {
    calls.push(init.method!);
    if (calls.length === 1) return Response.json({ deploymentId: "B", owner: { epoch: 3, deploymentId: "B", phase: "active" }, running: false });
    if (calls.length === 2) return new Response(JSON.stringify({ error: "ownership_fenced", reason: "storage_unavailable" }), { status: 503 });
    return Response.json({ deploymentId: "B", owner: { epoch: 4, deploymentId: "B", phase: "active" }, running: false });
  }, async () => {});
  assert.equal(result.owner.epoch, 4);
  assert.deepEqual(calls, ["GET", "POST", "GET"]);
});

test("client refuses unconfirmed transitions and never reflects response bodies", async () => {
  const leak = `synthetic-leak-${"q".repeat(40)}`;
  await assert.rejects(control({ ...args, action: "takeover" }, async (_url, init) =>
    Response.json(init.method === "GET" ? { deploymentId: "B", owner: { epoch: 3, deploymentId: "B", phase: "active" }, running: false }
      : { deploymentId: "B", owner: { epoch: 4, deploymentId: "B", phase: "active" }, running: true })), /not confirmed/);
  // Unknown refusal reasons and hostile bodies print as unrecognized, verbatim-free.
  for (const body of [{ error: "ownership_fenced", reason: leak }, `${leak} plain text`, { bogus: true }]) {
    const message = await control({ ...args, action: "status" }, async () =>
      new Response(typeof body === "string" ? body : JSON.stringify(body), { status: 503 })).then(
        () => assert.fail("expected rejection"), (error: Error) => error.message);
    assert.doesNotMatch(message, new RegExp(leak));
  }
  await assert.rejects(control({ ...args, action: "status", expectedDeploymentId: "not valid!!" }, sender(null).send), /Expected deployment id is invalid/);
});

test("status waits out deploy propagation, then reads the new version", async () => {
  const owner = { epoch: 3, deploymentId: "A", phase: "active" };
  const answers = [
    () => Response.json({ error: "ownership_fenced", reason: "deployment_mismatch" }, { status: 503 }),
    () => Response.json({ deploymentId: "A", owner, running: true }),
    () => Response.json({ deploymentId: "B", owner, running: false }),
  ];
  const calls: RequestInit[] = [];
  const waits: number[] = [];
  const state = await control({ ...args, action: "status" }, async (_url: URL, init: RequestInit) => {
    calls.push(init);
    return answers.shift()!();
  }, async (ms: number) => { waits.push(ms); });
  assert.equal(state.deploymentId, "B");
  assert.equal(calls.length, 3);
  assert.deepEqual(waits, [10000, 10000]);
});

test("status never retries other refusals", async () => {
  for (const [status, reason] of [[503, "storage_unavailable"], [401, "deployment_mismatch"], [409, "epoch_conflict"]] as const) {
    let calls = 0;
    await assert.rejects(control({ ...args, action: "status" }, async () => {
      calls += 1;
      return Response.json({ reason }, { status });
    }, async () => {}), /Ownership control failed/);
    assert.equal(calls, 1, `${status} ${reason}`);
  }
  // A running container on the new version is a NO-GO, not propagation.
  let calls = 0;
  await assert.rejects(control({ ...args, action: "status" }, async () => {
    calls += 1;
    return Response.json({ deploymentId: "B", owner: null, running: true });
  }, async () => {}), /not confirmed/);
  assert.equal(calls, 1);
});
