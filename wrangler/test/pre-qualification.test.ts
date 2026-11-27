import assert from "node:assert/strict";
import { test } from "node:test";
import worker, { PreQualification, PreQualificationContainer, PreStop } from "../src/pre-qualification.ts";

const actor = {
  startId: "10000000-0000-0000-0000-000000000001",
  version: "20000000-0000-0000-0000-000000000002",
  objectId: "a".repeat(64),
};
const image = `registry.cloudflare.com/${"b".repeat(32)}/two-bot-next-pre-123-456-c@sha256:${"c".repeat(64)}`;
const refusal = /pre_native_lifecycle_unconfirmed/;

function fixture() {
  const cutoff = Date.now() + 30_000;
  const id = { toString: () => actor.objectId };
  const calls: string[] = [];
  let value: unknown;
  let alarm: number | null = null;
  let lock: Promise<unknown> = Promise.resolve();
  const storage = {
    async get() { return structuredClone(value); },
    async put(_key: string, next: unknown) { value = structuredClone(next); },
    async setAlarm(next: number) { alarm = next; },
    async deleteAlarm() { alarm = null; },
    async transaction(fn: (txn: unknown) => Promise<void>) { await fn(storage); },
  };
  const container = {
    running: false,
    images: { fixture: image },
    start(options: unknown) {
      calls.push("start");
      assert.deepEqual(options, {
        image, enableInternet: false, entrypoint: ["/opt/pre/run"], labels: { pre_start_id: actor.startId },
        env: { PRE_SHAPE: env.PRE_SHAPE, PRE_START_ID: actor.startId, PRE_CUTOFF_MS: String(cutoff),
          DISCORD_GATEWAY_URL: "ws://127.0.0.1:8081", DISCORD_API_BASE: "http://127.0.0.1:8081" },
      });
    },
    async destroy() { calls.push("destroy"); },
    async inspect() { return { image, labels: { pre_start_id: actor.startId } }; },
    getTcpPort(port: number) {
      assert.equal(port, 8080);
      return { fetch: async () => new Response("{}", { status: 503 }) };
    },
  };
  const ctx = {
    id, storage, container,
    blockConcurrencyWhile<T>(fn: () => Promise<T>) {
      const next = lock.then(fn);
      lock = next.catch(() => undefined);
      return next;
    },
  };
  const env = {
    TWO_BOT: { idFromName: (name: string) => { assert.equal(name, actor.startId); return id; }, get: () => instance },
    CF_VERSION_METADATA: { id: actor.version }, PRE_START_ID: actor.startId, PRE_SHAPE: "0473/0465",
    PRE_CUTOFF_MS: String(cutoff), PRE_IMAGE: image,
  };
  const reconstruct = () => new PreQualificationContainer(ctx, env);
  const instance = reconstruct();
  return { instance, reconstruct, env, calls, cutoff, container, storage, alarm: () => alarm };
}

test("native starts once for each PRE shape through the private entrypoint", async () => {
  for (const shape of ["0473/0465", "0475/0465"]) {
    const { env, calls, cutoff, alarm } = fixture();
    env.PRE_SHAPE = shape;
    const runtime = new PreQualification({}, env);
    const custody = new PreStop({}, env);
    assert.equal(worker.fetch().status, 404);
    assert.deepEqual(runtime.identity(), actor);
    await assert.rejects(runtime.start(actor), refusal);
    assert.deepEqual(await custody.arm(actor), { actor, cutoff });
    assert.equal(alarm(), cutoff);
    await assert.rejects(runtime.start({ ...actor, objectId: "b".repeat(64) }), refusal);
    assert.deepEqual(await runtime.start(actor), actor);
    await assert.rejects(runtime.start(actor), refusal);
    await assert.rejects(runtime.probe(actor, "/internal/ownership"), refusal);
    const response = await runtime.probe(actor, "/readyz");
    assert.equal(response.status, 503);
    assert.equal(response.headers.get("x-two-worker-version"), actor.version);
    assert.deepEqual(calls, ["start"]);
  }
});

test("runner loss retains the exact fence across reconstruction and destroy failure", async t => {
  const { instance, reconstruct, env, calls, cutoff, container, alarm } = fixture();
  await instance.arm(actor);
  await instance.start(actor);
  const destroy = container.destroy.bind(container);
  container.destroy = async () => { throw new Error("upstream"); };
  t.mock.method(Date, "now", () => cutoff);
  env.CF_VERSION_METADATA.id = "30000000-0000-0000-0000-000000000003";
  await assert.rejects(reconstruct().alarm(), refusal);
  assert.ok(alarm() > cutoff);
  await assert.rejects(reconstruct().stop({ ...actor, version: env.CF_VERSION_METADATA.id }), refusal);
  container.destroy = destroy;
  await reconstruct().alarm();
  assert.equal((await reconstruct().stop(actor)).phase, "destroyed");
  assert.equal(alarm(), null);
  env.CF_VERSION_METADATA.id = actor.version;
  await assert.rejects(reconstruct().start(actor), refusal);
  await assert.rejects(reconstruct().probe(actor, "/health"), refusal);
  assert.deepEqual(calls, ["start", "destroy"]);
});

test("stop before arm and expiry during durable start intake both refuse delayed starts", async t => {
  const stopped = fixture();
  assert.equal((await stopped.instance.stop(actor)).phase, "never-issued");
  await assert.rejects(stopped.instance.arm(actor), refusal);
  await assert.rejects(stopped.instance.start(actor), refusal);
  assert.deepEqual(stopped.calls, []);

  const { instance, cutoff, calls, storage } = fixture();
  await instance.arm(actor);
  const put = storage.put.bind(storage);
  storage.put = async (...args) => {
    await put(...args);
    t.mock.method(Date, "now", () => cutoff);
  };
  await assert.rejects(instance.start(actor), refusal);
  assert.deepEqual(calls, []);
});
