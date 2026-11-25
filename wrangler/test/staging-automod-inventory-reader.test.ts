import assert from "node:assert/strict";
import test from "node:test";
import { bindStagingAutomodInventoryReader } from "../scripts/staging-automod-inventory-reader.ts";

const source = { workflowSha: "a".repeat(40), runId: 1234 };
const inventory = {
  schema_version: 1,
  workflow_sha: source.workflowSha,
  workflow_run_id: source.runId,
  observed_at: "2026-11-25T12:00:00.123456+00:00",
  worker: {
    deployment_id: "11111111-1111-1111-1111-111111111111",
    version_id: "22222222-2222-2222-2222-222222222222",
    source_sha: "c".repeat(40),
    traffic_percent: 100,
    runtime_policy: "bounded",
  },
  container: {
    application_id: "33333333-3333-3333-3333-333333333333",
    namespace_id: "e".repeat(32),
    rollout_id: "44444444-4444-4444-4444-444444444444",
    rollout_status: "completed",
    image_digest: `sha256:${"f".repeat(64)}`,
  },
  ownership: { phase: "active", epoch: 7 },
} as const;

test("the dormant inventory reader returns the typed v1 readback for the approved pin", async () => {
  const { signal } = new AbortController();
  const read = bindStagingAutomodInventoryReader(async (sha, runId, receivedSignal) => {
    assert.equal(sha, source.workflowSha);
    assert.equal(runId, String(source.runId));
    assert.equal(receivedSignal, signal);
    return JSON.stringify(inventory) + "\n";
  });
  assert.deepEqual(await read(source, signal), inventory);
  for (const observed_at of ["2026-11-25T12:00:00+00:00", "2026-11-25T12:00:00Z"]) {
    const value = { ...inventory, observed_at };
    const readAlternate = bindStagingAutomodInventoryReader(async () => JSON.stringify(value));
    assert.deepEqual(await readAlternate(source, signal), value);
  }
});

test("the inventory reader refuses partial or malformed verifier output", async () => {
  const { signal } = new AbortController();
  for (const output of [
    "{", "null", " ".repeat(4096) + JSON.stringify(inventory),
    ...[
      { ...inventory, schema_version: 2 },
      { ...inventory, workflow_sha: "b".repeat(40) },
      { ...inventory, workflow_run_id: source.runId + 1 },
      { ...inventory, observed_at: "2026-02-30T12:00:00Z" },
      { ...inventory, worker: { ...inventory.worker, traffic_percent: 50 } },
      { ...inventory, worker: { version_id: inventory.worker.version_id } },
      { ...inventory, container: undefined },
      { ...inventory, container: { ...inventory.container, rollout_status: "queued" } },
      { ...inventory, ownership: { phase: "active", epoch: 0 } },
      { ...inventory, ownership: { phase: "active", epoch: Number.MAX_SAFE_INTEGER + 1 } },
      { ...inventory, ownership: { ...inventory.ownership, running: false } },
      { ...inventory, plan: null },
    ].map(value => JSON.stringify(value)),
  ]) {
    const read = bindStagingAutomodInventoryReader(async () => output);
    assert.equal(await read(source, signal), undefined, output);
  }
  const read = bindStagingAutomodInventoryReader(async () => { throw new Error("verifier failed"); });
  assert.equal(await read(source, signal), undefined);
});

test("the inventory reader refuses invalid pins and forwards the caller's abort", async () => {
  const controller = new AbortController();
  const { signal } = controller;
  let calls = 0;
  let finish: (output: string) => void = () => {};
  const read = bindStagingAutomodInventoryReader((_, __, forwarded) => {
    calls += 1;
    assert.equal(forwarded, signal);
    return new Promise(resolve => { finish = resolve; });
  });
  assert.equal(await read({ ...source, workflowSha: "main" }, signal), undefined);
  assert.equal(await read({ ...source, runId: 0 }, signal), undefined);
  assert.equal(await read({ ...source, runId: Number.MAX_SAFE_INTEGER + 1 }, signal), undefined);
  assert.equal(calls, 0);

  const pending = read(source, signal);
  controller.abort();
  assert.equal(await pending, undefined);
  finish(JSON.stringify(inventory));
  assert.equal(await read(source, signal), undefined);
  assert.equal(calls, 1);
});
