import { test } from "node:test";
import assert from "node:assert/strict";
import { build } from "esbuild";
import { Miniflare } from "miniflare";
import { AUDIT_PREFIX } from "../src/ownership.ts";

// Official Miniflare DO fixture API (memory persistence survives setOptions):
// https://developers.cloudflare.com/workers/testing/miniflare/storage/durable-objects/
const bundle = await build({
  entryPoints: [new URL("./ownership-fixture.ts", import.meta.url).pathname],
  bundle: true, write: false, format: "esm", platform: "neutral",
  external: ["cloudflare:workers"],
});
// Miniflare 5's installed schema uses workers/config/manifest, unlike the
// older flat options still shown in the public fixture documentation.
const options = (suffix = "") => ({
  telemetry: { enabled: false },
  cf: false,
  workers: [{
    config: {
      name: "ownership-fixture", compatibilityDate: "2026-09-29",
      manifest: { mainModule: "fixture.js", modules: {
        "fixture.js": { type: "esm" as const, contents: bundle.outputFiles[0]!.text + suffix },
      } },
      env: { FIXTURE: { type: "durable-object" as const, worker: "ownership-fixture", exportName: "OwnershipFixture" } },
      exports: { OwnershipFixture: { type: "durable-object" as const, storage: "sqlite" as const } },
    },
  }],
});

async function fixture(t: { after: (fn: () => Promise<void>) => void }) {
  const mf = new Miniflare(options());
  t.after(() => mf.dispose());
  const call = async (path: string, body: object = {}) => {
    // setOptions invalidates Node-side stubs; reacquire against the same ID.
    const ns = await mf.getDurableObjectNamespace("FIXTURE");
    const stub = ns.getByName("singleton");
    const res = await stub.fetch(`https://fixture.invalid${path}`, {
      method: "POST", body: JSON.stringify(body),
    });
    return { status: res.status, body: await res.json() as any };
  };
  return { mf, call };
}

test("real DO storage: two deployment ids never start concurrently; explicit handoff is audited", async (t) => {
  const { call } = await fixture(t);
  assert.equal((await call("/probe", { id: "A" })).status, 503, "no implicit initial owner");
  const first = await call("/change", { id: "A", epoch: 0 });
  assert.equal(first.status, 200);
  assert.equal(first.body.epoch, 1);
  const probes = await Promise.all([
    call("/probe", { id: "A" }), call("/probe", { id: "B" }),
  ]);
  assert.deepEqual(probes.map((r) => r.status), [200, 503]);
  assert.deepEqual((await call("/state")).body.starts, ["A"]);

  const second = await call("/change", { id: "B", epoch: 1 });
  assert.equal(second.status, 200);
  assert.equal(second.body.oldEpoch, 1);
  assert.equal(second.body.epoch, 2);
  assert.equal(second.body.actor, "fixture-operator");
  assert.ok(Number.isFinite(Date.parse(second.body.timestamp)));
  assert.equal(second.body.oldDeploymentId, "A");
  assert.equal((await call("/probe", { id: "A" })).status, 503);
  assert.equal((await call("/probe", { id: "B" })).status, 200);
  const state = (await call("/state")).body;
  assert.deepEqual(state.starts, ["A", "B"]);
  assert.equal(state.running, "B");
  assert.equal(state.audit[`${AUDIT_PREFIX}2:active`].epoch, 2);
  assert.equal(state.audit[`${AUDIT_PREFIX}2:fenced`].phase, "fenced");
  assert.equal((await call("/change", { id: "A", epoch: 1 })).body.reason, "epoch_conflict");
});

test("real DO storage: a read failure cannot invoke the synthetic container start", async (t) => {
  const { call } = await fixture(t);
  await call("/change", { id: "A", epoch: 0 });
  assert.equal((await call("/probe", { id: "A", readError: true })).body.reason, "storage_unavailable");
  assert.deepEqual((await call("/state")).body.starts, []);
});

test("real DO storage: failed teardown persists denial across Worker/DO reload", async (t) => {
  const { mf, call } = await fixture(t);
  await call("/change", { id: "A", epoch: 0 });
  await call("/probe", { id: "A" });
  assert.equal((await call("/change", { id: "B", epoch: 1, stopError: true })).status, 503);
  assert.equal((await call("/state")).body.owner.phase, "fenced");
  // Changing the bundle creates a new isolate; storage is not cleared.
  await mf.setOptions(options("\n// reload receipt"));
  assert.equal((await call("/probe", { id: "A" })).status, 503);
  assert.equal((await call("/probe", { id: "B" })).status, 503);
  assert.deepEqual((await call("/state")).body.starts, ["A"]);
  assert.equal((await call("/change", { id: "B", epoch: 2 })).status, 200);
});

test("real DO storage: failed active write leaves a durable fence and stopped process", async (t) => {
  const { mf, call } = await fixture(t);
  await call("/change", { id: "A", epoch: 0 });
  await call("/probe", { id: "A" });
  const result = await call("/change", { id: "B", epoch: 1, releaseWriteError: true });
  assert.equal(result.body.reason, "storage_unavailable");
  await mf.setOptions(options("\n// failed release reload"));
  const state = (await call("/state")).body;
  assert.equal(state.owner.phase, "fenced");
  assert.equal(state.owner.epoch, 2);
  assert.equal(state.running, null);
  assert.ok(state.audit[`${AUDIT_PREFIX}2:fenced`]);
  assert.equal(state.audit[`${AUDIT_PREFIX}2:active`], undefined);
  assert.equal((await call("/probe", { id: "A" })).status, 503);
  assert.equal((await call("/probe", { id: "B" })).status, 503);
  assert.deepEqual((await call("/state")).body.starts, ["A"]);
});

test("real DO storage: parking owner fences both deployments and survives reload", async (t) => {
  const { mf, call } = await fixture(t);
  await call("/change", { id: "A", epoch: 0 });
  await call("/probe", { id: "A" });
  const park = await call("/change", { id: "A", epoch: 1, action: "fence" });
  assert.equal(park.body.deploymentId, null);
  assert.equal(park.body.epoch, 2);
  await mf.setOptions(options("\n// fenced reload"));
  const denied = await Promise.all([call("/probe", { id: "A" }), call("/probe", { id: "B" })]);
  assert.deepEqual(denied.map((r) => r.status), [503, 503]);
  assert.equal((await call("/state")).body.running, null);
});

test("real DO storage: same-owner exact-epoch takeover repeat is a read-only no-op", async (t) => {
  const { call } = await fixture(t);
  const first = await call("/change", { id: "A", epoch: 0 });
  assert.equal(first.status, 200);
  assert.equal(first.body.epoch, 1);
  await call("/probe", { id: "A" });
  const before = (await call("/state")).body;
  const repeat = await call("/change", { id: "A", epoch: 1 });
  assert.equal(repeat.status, 200);
  assert.deepEqual(repeat.body, first.body);
  const after = (await call("/state")).body;
  assert.deepEqual(Object.keys(after.audit).sort(), Object.keys(before.audit).sort(), "no-op writes no audit row");
  assert.equal(after.running, "A", "no-op runs no teardown");
  assert.deepEqual(after.starts, ["A"]);
  assert.equal((await call("/change", { id: "A", epoch: 0 })).body.reason, "epoch_conflict");
});

test("real DO storage: forced same-owner exact-epoch takeover still commits (recovery path)", async (t) => {
  const { call } = await fixture(t);
  await call("/change", { id: "A", epoch: 0 });
  await call("/probe", { id: "A" });
  const forced = await call("/change", { id: "A", epoch: 1, force: true });
  assert.equal(forced.status, 200);
  assert.equal(forced.body.epoch, 2);
  assert.equal(forced.body.deploymentId, "A");
  assert.equal(forced.body.phase, "active");
  const state = (await call("/state")).body;
  assert.equal(state.running, null, "recovery re-runs teardown");
  assert.ok(state.audit[`${AUDIT_PREFIX}2:active`]);
});

test("real DO storage: fenced-pending same-id retry still commits", async (t) => {
  const { call } = await fixture(t);
  await call("/change", { id: "A", epoch: 0 });
  await call("/probe", { id: "A" });
  assert.equal((await call("/change", { id: "B", epoch: 1, stopError: true })).status, 503);
  const retry = await call("/change", { id: "B", epoch: 2 });
  assert.equal(retry.status, 200);
  assert.equal(retry.body.epoch, 3);
  assert.equal(retry.body.phase, "active");
  assert.equal(retry.body.deploymentId, "B");
});

test("real DO storage: different-deployment takeover at the fresh epoch still commits", async (t) => {
  const { call } = await fixture(t);
  await call("/change", { id: "A", epoch: 0 });
  const handoff = await call("/change", { id: "B", epoch: 1 });
  assert.equal(handoff.status, 200);
  assert.equal(handoff.body.epoch, 2);
  assert.equal(handoff.body.deploymentId, "B");
  assert.equal(handoff.body.phase, "active");
});

test("real DO storage: malformed persisted records fail closed", async (t) => {
  const { call } = await fixture(t);
  await call("/corrupt");
  assert.equal((await call("/probe", { id: "A" })).body.reason, "storage_invalid");
});
