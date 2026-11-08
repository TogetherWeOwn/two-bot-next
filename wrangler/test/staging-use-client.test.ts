import { test } from "node:test";
import assert from "node:assert/strict";
import { parseStagingState, stagingUse } from "../scripts/staging-use.mjs";

const NOW = Date.now();
const SHA = "a".repeat(40);
const TOKEN = "synthetic-secret-".repeat(3);
const args = { action: "acquire", url: "https://two-bot-next-staging.example.workers.dev", token: TOKEN,
  deploymentId: "deployment-A", sha: SHA, runId: "123456", runAttempt: 2, expiresAt: NOW + 60000 };
const use = { environment: "staging", deploymentId: args.deploymentId, sha: SHA, epoch: 7,
  runId: args.runId, runAttempt: args.runAttempt, acquiredAt: NOW, expiresAt: args.expiresAt, phase: "active" };
const state = { deploymentId: args.deploymentId, sha: SHA, running: false, use,
  owner: { deploymentId: args.deploymentId, epoch: 7, phase: "active", actor: "fixture",
    oldEpoch: 6, oldDeploymentId: "old", timestamp: "2026-11-07T00:00:00Z" } };
const clock = () => NOW;
const ready = () => Response.json({ build_revision: SHA, components: [["gateway", "ready"], ["db", "ready"]] },
  { headers: { "x-two-worker-version": args.deploymentId } });

function responses(values: Response[]) {
  const calls: { url: URL; init: RequestInit }[] = [];
  const send = async (url: URL, init: RequestInit) => {
    calls.push({ url, init });
    const response = values.shift();
    assert.ok(response, "unexpected request");
    return response;
  };
  return { calls, send };
}

test("staging client: acquire binds version, build, epoch, run/attempt and immutable expiry", async () => {
  const h = responses([Response.json({ ...state, use: null }), Response.json(state)]);
  const receipt = await stagingUse(args, h.send, async () => {}, clock);
  assert.equal(h.calls.length, 2);
  assert.deepEqual(JSON.parse(String(h.calls[1].init.body)), { action: "acquire", environment: "staging",
    deploymentId: args.deploymentId, sha: SHA, expectedEpoch: 7, runId: args.runId, runAttempt: 2, expiresAt: args.expiresAt });
  assert.equal(receipt.use?.expiresAt, args.expiresAt);
  assert.equal(receipt.observedAt, new Date(NOW).toISOString());
  assert.equal(receipt.action, "acquire");
  assert.equal(receipt.environment, "staging");
  for (const call of h.calls) {
    assert.equal(call.url.pathname, "/internal/staging-use");
    assert.equal(call.init.redirect, "error");
    assert.equal(new Headers(call.init.headers).get("authorization"), `Bearer ${TOKEN}`);
    assert.ok(call.init.signal);
  }
});

test("staging client: a deployment resource UUID cannot replace the Worker version UUID", async () => {
  const versionId = "11111111-1111-4111-8111-111111111111";
  const deploymentResourceId = "22222222-2222-4222-8222-222222222222";
  const current = { ...state, deploymentId: versionId,
    owner: { ...state.owner, deploymentId: versionId }, use: { ...use, deploymentId: versionId } };
  const wrong = responses([Response.json(current)]);
  await assert.rejects(stagingUse({ ...args, deploymentId: deploymentResourceId }, wrong.send,
    async () => {}, clock), /staging_identity_mismatch/);
  assert.equal(wrong.calls.length, 1);
  assert.equal(wrong.calls[0].init.method, "GET", "wrong resource must never acquire");
  const h = responses([Response.json({ ...current, use: null }), Response.json(current)]);
  const receipt = await stagingUse({ ...args, deploymentId: versionId }, h.send, async () => {}, clock);
  assert.equal(receipt.deploymentId, versionId);
  assert.equal(receipt.use?.deploymentId, versionId);
  assert.equal(JSON.parse(String(h.calls[1].init.body)).deploymentId, versionId);
});

test("staging client: invalid origin, identities and windows refuse before any network", async () => {
  const leak = TOKEN;
  for (const patch of [
    { url: "https://two-bot-next.example.workers.dev" }, { url: "http://two-bot-next-staging.example.workers.dev" },
    { url: "https://two-bot-next-staging.example.workers.dev@evil.example" },
    { url: "https://two-bot-next-staging.example.workers.dev/path" },
    { url: "https://two-bot-next-staging.example.workers.dev?token=" + leak },
    { url: "https://two-bot-next-staging.example.workers.dev#" + leak }, { url: "not a URL " + leak },
    { url: "https://user:pass@two-bot-next-staging.example.workers.dev" },
    { token: "short" }, { sha: "A".repeat(40) }, { deploymentId: "../bad" },
    { runId: "01" }, { runAttempt: 0 }, { runAttempt: 1.5 }, { action: "renew" },
    { expiresAt: NOW }, { expiresAt: NOW + 6 * 60 * 60 * 1000 + 1 },
  ]) {
    let calls = 0;
    await assert.rejects(stagingUse({ ...args, ...patch }, async () => {
      calls++; throw new Error(leak);
    }, async () => {}, clock), (error: Error) => !error.message.includes(leak));
    assert.equal(calls, 0, JSON.stringify(patch));
  }
});

test("staging client: shape validation strips unknown success fields and audit data", () => {
  const projected = parseStagingState({ ...state, token: TOKEN, debug: TOKEN,
    owner: { ...state.owner, debug: TOKEN }, use: { ...use, token: TOKEN } });
  assert.ok(projected);
  assert.equal(JSON.stringify(projected).includes(TOKEN), false);
  assert.equal(Object.hasOwn(projected, "owner"), false);
  for (const patch of [
    { owner: null }, { owner: { ...state.owner, epoch: 0 } },
    { owner: { ...state.owner, phase: "fenced", deploymentId: null } },
    { sha: "unknown" }, { running: "false" }, { use: undefined },
  ]) assert.equal(parseStagingState({ ...state, ...patch }), null);
  for (const patch of [
    { environment: "production" }, { runId: "01" }, { epoch: 0 }, { phase: "renewed" }, { sha: "unknown" },
    { acquiredAt: NOW + 60001 }, { expiresAt: NOW + 6 * 60 * 60 * 1000 + 1 },
  ]) assert.equal(parseStagingState({ ...state, use: { ...use, ...patch } }), null);
});

test("staging client: status is control-only and grants no readiness or authorization", async () => {
  const h = responses([Response.json({ ...state, use: null })]);
  const receipt = await stagingUse({ action: "status", url: args.url, token: TOKEN }, h.send, async () => {}, clock);
  assert.equal(receipt.use, null);
  assert.equal(receipt.action, "status");
  assert.equal(h.calls.length, 1);
  assert.equal(h.calls[0].init.method, "GET");
});

test("staging client: periodic observation never starts or grants and distinguishes idle from health", async () => {
  const observer = { action: "observe", url: args.url, token: TOKEN };
  for (const value of [null, { ...use, phase: "released" }, { ...use, acquiredAt: NOW - 60000, expiresAt: NOW }]) {
    const h = responses([Response.json({ ...state, use: value })]);
    const result = await stagingUse(observer, h.send, async () => assert.fail("no retry"), clock);
    assert.equal(result.sample, false);
    assert.equal(result.code, "intentional_idle");
    assert.equal(result.health, "unknown");
    assert.equal(h.calls.length, 1);
    assert.equal(h.calls[0].init.method, "GET");
    assert.equal(h.calls[0].url.pathname, "/internal/staging-use");
    assert.equal(h.calls[0].init.body, undefined);
  }
  for (const running of [false, true]) {
    const h = responses([Response.json({ ...state, running })]);
    const result = await stagingUse(observer, h.send, async () => assert.fail("no retry"), clock);
    assert.equal(result.sample, true, "active startup/failure must still be monitored");
    assert.equal(result.code, "active_window");
    assert.equal(result.health, "unknown");
    assert.equal(result.use?.expiresAt, use.expiresAt);
    assert.equal(h.calls.length, 1);
    assert.equal(h.calls[0].init.method, "GET");
  }
});

test("staging client: periodic observation fails closed on unconfirmed stop or invalid authority", async () => {
  const observer = { action: "observe", url: args.url, token: TOKEN };
  for (const value of [null, { ...use, phase: "released" }, { ...use, acquiredAt: NOW - 60000, expiresAt: NOW }]) {
    const h = responses([Response.json({ ...state, running: true, use: value })]);
    await assert.rejects(stagingUse(observer, h.send, async () => {}, clock), /staging_stop_unconfirmed/);
    assert.equal(h.calls.length, 1);
  }
  for (const patch of [{ phase: "preparing" }, { acquiredAt: NOW + 1 }, { epoch: 8 },
    { sha: "b".repeat(40) }, { deploymentId: "deployment-B" }]) {
    const h = responses([Response.json({ ...state, use: { ...use, ...patch } })]);
    await assert.rejects(stagingUse(observer, h.send, async () => {}, clock), /staging_window_mismatch/);
    assert.equal(h.calls.length, 1);
  }
  for (const response of [new Response(null, { status: 404 }), Response.json({ running: false })]) {
    const h = responses([response]);
    await assert.rejects(stagingUse(observer, h.send, async () => {}, clock), /staging_(control_refused|response_invalid)/);
    assert.equal(h.calls.length, 1);
  }
});

test("staging client: stale deployment/build refuses before POST; changed POST response refuses", async () => {
  for (const patch of [{ deploymentId: "deployment-B" }, { sha: "b".repeat(40) }]) {
    const wrong = { ...state, ...patch, owner: { ...state.owner, deploymentId: patch.deploymentId ?? args.deploymentId } };
    const first = responses([Response.json(wrong)]);
    await assert.rejects(stagingUse(args, first.send, async () => {}, clock), /staging_identity_mismatch/);
    assert.equal(first.calls.length, 1);
    const second = responses([Response.json({ ...state, use: null }), Response.json(wrong)]);
    await assert.rejects(stagingUse(args, second.send, async () => {}, clock), /staging_identity_mismatch/);
  }
});

test("staging client: assert never acquires or probes on mismatched, released or expired authority", async () => {
  for (const patch of [
    { runId: "999" }, { runAttempt: 3 }, { expiresAt: args.expiresAt + 1 },
    { epoch: 8 }, { sha: "b".repeat(40) }, { deploymentId: "deployment-B" },
    { phase: "preparing" }, { phase: "released" }, { acquiredAt: NOW + 1 },
  ]) {
    const h = responses([Response.json({ ...state, use: { ...use, ...patch } })]);
    await assert.rejects(stagingUse({ ...args, action: "assert" }, h.send, async () => {}, clock), /staging_window_mismatch/);
    assert.equal(h.calls.length, 1);
  }
  const h = responses([]);
  await assert.rejects(stagingUse({ ...args, action: "assert" }, h.send, async () => {}, () => args.expiresAt), /staging_window_invalid/);
  assert.equal(h.calls.length, 0);
});

test("staging client: ready probes exact build without control secret and rechecks authority", async () => {
  const h = responses([Response.json(state), ready(), Response.json({ ...state, running: true })]);
  const receipt = await stagingUse({ ...args, action: "ready" }, h.send, async () => {}, clock);
  assert.equal(receipt.action, "ready");
  assert.equal(receipt.running, true);
  assert.equal(h.calls.length, 3);
  assert.equal(h.calls[1].url.pathname, "/readyz");
  assert.equal(new Headers(h.calls[1].init.headers).has("authorization"), false);
  assert.equal(h.calls[2].url.pathname, "/internal/staging-use");
});

test("staging client: startup retry stays inside the window and cannot mask identity/health failures", async () => {
  let pauses = 0;
  const h = responses([Response.json(state), new Response(null, { status: 503 }), ready(), Response.json({ ...state, running: true })]);
  await stagingUse({ ...args, action: "ready" }, h.send, async (ms: number) => { assert.equal(ms, 5000); pauses++; }, clock);
  assert.equal(pauses, 1);
  for (const response of [
    Response.json({ build_revision: SHA, components: [["gateway", "ready"]] }, { headers: { "x-two-worker-version": "deployment-B" } }),
    Response.json({ build_revision: "b".repeat(40), components: [["gateway", "ready"]] }, { headers: { "x-two-worker-version": args.deploymentId } }),
    Response.json({ build_revision: SHA, components: [["gateway", "down"]] }, { headers: { "x-two-worker-version": args.deploymentId } }),
    Response.json({ build_revision: SHA, components: [] }, { headers: { "x-two-worker-version": args.deploymentId } }),
    new Response(null, { status: 401 }),
  ]) {
    const bad = responses([Response.json(state), response]);
    await assert.rejects(stagingUse({ ...args, action: "ready" }, bad.send, async () => assert.fail("must not retry"), clock), /staging_readiness/);
    assert.equal(bad.calls.length, 2);
  }
});

test("staging client: a release/takeover during readiness invalidates the observation", async () => {
  for (const patch of [{ phase: "released" }, { epoch: 8 }, { runId: "999" }]) {
    const h = responses([Response.json(state), ready(), Response.json({ ...state, running: true, use: { ...use, ...patch } })]);
    await assert.rejects(stagingUse({ ...args, action: "ready" }, h.send, async () => {}, clock), /staging_window_mismatch/);
  }
  const rebound = { ...state, running: true, owner: { ...state.owner, epoch: 8, oldEpoch: 7 }, use: { ...use, epoch: 8 } };
  const h = responses([Response.json(state), ready(), Response.json(rebound)]);
  await assert.rejects(stagingUse({ ...args, action: "ready" }, h.send, async () => {}, clock), /staging_epoch_mismatch/);
});

test("staging client: release is idempotent, accepts preparing/expired use, and never stops another run", async () => {
  for (const phase of ["active", "preparing", "released"]) {
    const h = responses([Response.json({ ...state, use: { ...use, phase } }), Response.json({ ...state, use: { ...use, phase: "released" } })]);
    const receipt = await stagingUse({ ...args, action: "release" }, h.send, async () => {}, () => args.expiresAt + 1000);
    assert.equal(receipt.use?.phase, "released");
    assert.equal(receipt.running, false);
    assert.equal(JSON.parse(String(h.calls[1].init.body)).expiresAt, args.expiresAt);
  }
  const other = responses([Response.json({ ...state, use: { ...use, runId: "999" } })]);
  await assert.rejects(stagingUse({ ...args, action: "release" }, other.send, async () => {}, clock), /staging_window_mismatch/);
  assert.equal(other.calls.length, 1);
  const running = responses([Response.json(state), Response.json({ ...state, running: true, use: { ...use, phase: "released" } })]);
  await assert.rejects(stagingUse({ ...args, action: "release" }, running.send, async () => {}, clock), /staging_stop_unconfirmed/);
});

test("staging client: bounded transport, malformed success, redirect and remote failures never leak bodies", async () => {
  const hostile = [
    new Response(TOKEN, { status: 503 }), new Response(TOKEN, { status: 401 }),
    new Response(TOKEN, { status: 302, headers: { location: "https://evil.example/" + TOKEN } }),
    Response.json({ ...state, sha: TOKEN }), new Response("[", { headers: { "content-type": "application/json" } }),
    new Response(" ".repeat(16385), { headers: { "content-type": "application/json" } }),
    new Response(TOKEN, { headers: { "content-type": "text/plain" } }),
  ];
  for (const response of hostile) {
    await assert.rejects(stagingUse(args, async () => response, async () => {}, clock), (error: Error) =>
      error.message.startsWith("staging_") && !error.message.includes(TOKEN));
  }
  await assert.rejects(stagingUse(args, async () => { throw new Error(TOKEN); }, async () => {}, clock), /staging_transport_failed/);
});

test("staging client: header/body/cancel stalls settle at the request deadline", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const never = new Promise<Response>(() => {});
  const pending = stagingUse(args, async () => never, async () => {}, clock);
  const rejected = assert.rejects(pending, /staging_request_timeout/);
  t.mock.timers.tick(15001);
  await rejected;
  const body = new ReadableStream({ pull: () => new Promise<void>(() => {}), cancel: () => new Promise<void>(() => {}) });
  const reading = stagingUse(args, async () => new Response(body, { headers: { "content-type": "application/json" } }), async () => {}, clock);
  const bodyRejected = assert.rejects(reading, /staging_request_timeout/);
  await Promise.resolve();
  t.mock.timers.tick(15001);
  await bodyRejected;
});
