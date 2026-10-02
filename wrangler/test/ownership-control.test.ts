import { test } from "node:test";
import assert from "node:assert/strict";
import { control } from "../scripts/ownership-control.mjs";

const args = {
  url: "https://two-bot-next-staging.fixture.workers.dev", token: "synthetic-control-token-not-a-secret-12345",
  actor: "fixture-operator", expectedEpoch: 1,
};
function sender(owner: object | null) {
  const calls: RequestInit[] = [];
  return {
    calls,
    send: async (_url: URL, init: RequestInit) => {
      calls.push(init);
      if (init.method === "GET") return Response.json({ deploymentId: "B", owner, running: false });
      const change = JSON.parse(String(init.body));
      return Response.json({ deploymentId: "B", running: false, owner: {
        deploymentId: change.action === "fence" ? null : "B", epoch: change.expectedEpoch + 1,
        phase: change.action === "fence" ? "fenced" : "active",
      } });
    },
  };
}

test("client validates staging origin and configuration before any request", async () => {
  for (const url of ["https://two-bot-next-production.fixture.workers.dev", "https://attacker.invalid", args.url + "/unexpected", args.url + "?secret=x"]) {
    await assert.rejects(control({ ...args, url, action: "takeover" }, () => { throw new Error("must not send"); }));
  }
  await assert.rejects(control({ ...args, token: "", action: "preflight" }));
  assert.deepEqual(await control({ ...args, action: "preflight" }), { configured: true });
});

test("routine deploy refuses to unpark a fenced/pristine singleton", async () => {
  for (const owner of [null, { epoch: 2, deploymentId: null, phase: "fenced" }]) {
    const s = sender(owner);
    await assert.rejects(control({ ...args, action: "deployment-takeover" }, s.send), /explicit staging release required/);
    assert.equal(s.calls.length, 1);
  }
});

test("routine deploy explicitly hands off the active epoch without redirects", async () => {
  const s = sender({ epoch: 3, deploymentId: "A", phase: "active" });
  const result = await control({ ...args, action: "deployment-takeover" }, s.send);
  assert.equal(result.owner.epoch, 4);
  assert.equal(s.calls.length, 2);
  assert.equal(s.calls[0]!.redirect, "error");
  assert.deepEqual(JSON.parse(String(s.calls[1]!.body)), { action: "takeover", actor: args.actor, expectedEpoch: 3 });
});

test("explicit release can initialize but requires an actor and confirmed stop", async () => {
  const s = sender(null);
  assert.equal((await control({ ...args, action: "deployment-takeover", releaseFence: true }, s.send)).owner.epoch, 1);
  await assert.rejects(control({ ...args, actor: "", action: "takeover" }, sender(null).send));
  await assert.rejects(control({ ...args, action: "takeover" }, async (_url, init) =>
    Response.json(init.method === "GET" ? { deploymentId: "B", owner: null, running: false }
      : { deploymentId: "B", owner: { epoch: 2, deploymentId: "B", phase: "active" }, running: true })), /not confirmed/);
});

test("client never retries authentication or CAS failures or reflects response bodies", async () => {
  for (const status of [401, 403, 409, 503]) {
    let calls = 0;
    await assert.rejects(control({ ...args, action: "takeover" }, async () => {
      calls++;
      return new Response("synthetic-private-body", { status });
    }), new RegExp(`HTTP ${status}`));
    assert.equal(calls, 1);
  }
});
