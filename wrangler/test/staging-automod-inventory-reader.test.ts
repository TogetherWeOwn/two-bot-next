import assert from "node:assert/strict";
import { ChildProcess, spawn } from "node:child_process";
import { once } from "node:events";
import test from "node:test";
import { fileURLToPath, URL } from "node:url";
import { bindStagingAutomodInventoryReader } from "../scripts/staging-automod-inventory-reader.ts";
import { bindStagingAutomodInventoryVerifier } from "../scripts/staging-automod-inventory-verifier.ts";
import { syntheticVerifier, verifierWithDescendant } from "./fixtures/staging-automod-verifier.ts";

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
  const result: { schema_version: 1 } | undefined = await read(source, signal);
  assert.deepEqual(result, inventory);
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
  for (const invalid of [{ workflowSha: "main" }, { runId: 0 }, { runId: Number.MAX_SAFE_INTEGER + 1 }])
    assert.equal(await read({ ...source, ...invalid }, signal), undefined);
  assert.equal(calls, 0);
  const pending = read(source, signal);
  controller.abort();
  assert.equal(await pending, undefined);
  finish(JSON.stringify(inventory));
  assert.equal(await read(source, signal), undefined);
  assert.equal(calls, 1);
});

const receipt = JSON.stringify(inventory) + "\n";
const cappedReceipt = receipt.padEnd(4_096);
const writeCappedReceipt = `process.stdout.write(${JSON.stringify(cappedReceipt)});`;

test("the verifier invokes the checked-in script with the exact pins and accepts 4096 bytes on exit zero", async () => {
  const { signal } = new AbortController();
  const run = bindStagingAutomodInventoryVerifier((file, args, options) => {
    assert.equal(file, "python3");
    assert.deepEqual(args, [fileURLToPath(new URL("../../scripts/staging_automod_verifier_supervisor.py", import.meta.url)),
      "python3", fileURLToPath(new URL("../../scripts/staging_automod_inventory_reader.py", import.meta.url)),
      source.workflowSha, String(source.runId)]);
    assert.deepEqual(options, { env: {}, stdio: "pipe", detached: true });
    return syntheticVerifier(writeCappedReceipt)(file, args, options);
  });
  assert.deepEqual(await bindStagingAutomodInventoryReader(run)(source, signal), inventory);
});

test("the verifier refuses excess output even after a complete receipt", async () => {
  const { signal } = new AbortController();
  const multibyteOutput = receipt + "é".repeat(2_048);
  for (const [program, overflow] of [
    [`${writeCappedReceipt} process.stdout.write(" ");`, "stdout"],
    [`process.stdout.write(${JSON.stringify(multibyteOutput)});`, "stdout"],
    [`${writeCappedReceipt} process.stderr.write(" ".repeat(4097));`, "stderr"],
  ] as const) {
    const bytes = { stdout: 0, stderr: 0 };
    const run = bindStagingAutomodInventoryVerifier(syntheticVerifier(program, child => {
      child.stdout.on("data", chunk => { bytes.stdout += chunk.length; });
      child.stderr.on("data", chunk => { bytes.stderr += chunk.length; });
    }));
    await assert.rejects(run(source.workflowSha, String(source.runId), signal), { message: "Staging inventory verifier failed" });
    assert.ok(bytes[overflow] > 4_096);
  }
});

test("the verifier returns its fixed refusal when spawn fails without pipes", { timeout: 2_000 }, async () => {
  type Launch = NonNullable<Parameters<typeof bindStagingAutomodInventoryVerifier>[0]>;
  const run = bindStagingAutomodInventoryVerifier(() => {
    const child = new ChildProcess();
    Object.assign(child, { stdin: null, stdout: null, stderr: null });
    queueMicrotask(() => {
      child.emit("error", Object.assign(new Error("synthetic descriptor limit"), { code: "EMFILE" }));
    });
    // Node declares piped stdio at launch even when no process or pipes are created.
    return child as ReturnType<Launch>;
  });
  await assert.rejects(run(source.workflowSha, String(source.runId), new AbortController().signal),
    { message: "Staging inventory verifier failed" });
});

test("the verifier returns its fixed refusal when spawn throws", async () => {
  const run = bindStagingAutomodInventoryVerifier(() => { throw new Error("synthetic launch failure"); });
  await assert.rejects(run(source.workflowSha, String(source.runId), new AbortController().signal),
    { message: "Staging inventory verifier failed" });
});

test("the verifier drains a cancelled descendant without signalling an unrelated child", { timeout: 10_000 }, async () => {
  const controller = new AbortController();
  const { signal } = controller;
  const unrelated = spawn(process.execPath, ["-e", "setTimeout(() => {}, 7_000);"], { stdio: "ignore", env: {} });
  const unrelatedClosed = once(unrelated, "close");
  let descendant = 0;
  let closed = false;
  let calls = 0;
  const run = bindStagingAutomodInventoryVerifier(syntheticVerifier(verifierWithDescendant(receipt), child => {
    calls += 1;
    child.stderr.once("data", chunk => { descendant = Number(chunk); controller.abort(); });
    child.once("close", () => { closed = true; });
  }));
  try {
    assert.equal(await bindStagingAutomodInventoryReader(run)(source, signal), undefined);
    assert.equal(closed, true);
    assert.ok(descendant > 0);
    assert.throws(() => process.kill(descendant, 0), { code: "ESRCH" });
    assert.equal(unrelated.exitCode, null);
    assert.equal(unrelated.signalCode, null);
    await assert.rejects(run(source.workflowSha, String(source.runId), signal), { message: "Staging inventory verifier failed" });
    assert.equal(calls, 1);
  } finally {
    unrelated.kill();
    await unrelatedClosed;
  }
});

test("an exited verifier drains the descendant holding its pipe and preserves exit seven", { timeout: 5_000 }, async () => {
  let descendant = 0;
  let bytes = 0;
  let status: number | null | undefined;
  const program = verifierWithDescendant(cappedReceipt, { exitCode: 7, holdStdout: true });
  const run = bindStagingAutomodInventoryVerifier(syntheticVerifier(program, child => {
    child.stdout.on("data", chunk => { bytes += chunk.length; });
    child.stderr.once("data", chunk => { descendant = Number(chunk); });
    child.once("close", code => { status = code; });
  }));
  await assert.rejects(run(source.workflowSha, String(source.runId), new AbortController().signal),
    { message: "Staging inventory verifier failed" });
  assert.equal(status, 7);
  assert.equal(bytes, 4_096);
  assert.ok(descendant > 0);
  assert.throws(() => process.kill(descendant, 0), { code: "ESRCH" });
});

test("abort after supervisor exit still discards the completed receipt", async () => {
  const controller = new AbortController();
  let status: number | null | undefined;
  let bytes = 0;
  const run = bindStagingAutomodInventoryVerifier(syntheticVerifier(writeCappedReceipt, child => {
    child.stdout.on("data", chunk => { bytes += chunk.length; });
    child.once("exit", code => { status = code; controller.abort(); });
  }));
  await assert.rejects(run(source.workflowSha, String(source.runId), controller.signal),
    { message: "Staging inventory verifier failed" });
  assert.equal(status, 0);
  assert.equal(bytes, 4_096);
});
