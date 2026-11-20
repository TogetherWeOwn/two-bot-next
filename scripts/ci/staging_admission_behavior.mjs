// Offline pipeline behavior only; this does not execute the native alarm or Actions scheduler.
import assert from "node:assert/strict";
import { pipelineUse } from "../../wrangler/scripts/staging-use-pipeline.mjs";

const now = 1_800_000_000_000;
const sha = "a".repeat(40);
const base = { runId: "123", runAttempt: 1, url: "fixture", token: "fixture" };
const grant = { ...base, expiresAt: now + 60_000, phase: "active" };

function fixture(use = grant) {
  const calls = [];
  const invoke = async (args) => {
    calls.push(args);
    if (args.action === "status") return { use, sha, deploymentId: "dep-1" };
    return { action: args.action, use: { ...grant, expiresAt: args.expiresAt }, sha, deploymentId: "dep-1" };
  };
  return { calls, invoke };
}

export async function assertOwnership() {
  for (const use of [null, { ...grant, runId: "456" }, { ...grant, runAttempt: 2 }]) {
    const f = fixture(use);
    const result = await pipelineUse({ ...base, action: "finish", expiresAt: grant.expiresAt }, f.invoke, () => now);
    assert.equal(result.action, "not-owned");
    assert.deepEqual(f.calls.map((c) => c.action), ["status"]);
  }
  const owned = fixture();
  await pipelineUse({ ...base, action: "finish", expiresAt: grant.expiresAt }, owned.invoke, () => now);
  assert.deepEqual(owned.calls.map((c) => c.action), ["status", "release"]);
  assert.equal(owned.calls[1].expiresAt, grant.expiresAt);
  const mismatch = fixture();
  await assert.rejects(pipelineUse({ ...base, action: "finish", expiresAt: grant.expiresAt + 1 },
    mismatch.invoke, () => now), (error) => error.code === "staging_window_mismatch");
  assert.deepEqual(mismatch.calls.map((c) => c.action), ["status"]);
}

export async function assertExpiryAndStandalone() {
  for (const use of [{ ...grant, expiresAt: now }, { ...grant, phase: "released" }]) {
    const f = fixture(use);
    await assert.rejects(pipelineUse({ ...base, action: "begin", minutes: 5 }, f.invoke, () => now),
      (error) => error.code === "staging_use_ended");
    assert.deepEqual(f.calls.map((c) => c.action), ["status"]);
  }
  const standalone = fixture(null);
  await pipelineUse({ ...base, action: "begin", minutes: 5 }, standalone.invoke, () => now);
  assert.deepEqual(standalone.calls.map((c) => c.action), ["status", "acquire", "ready"]);
  assert.equal(standalone.calls[1].expiresAt, now + 300_000);
  assert.equal(standalone.calls[2].expiresAt, now + 300_000);
  const inherited = fixture();
  await pipelineUse({ ...base, action: "consume", sha, deploymentId: "dep-1", expiresAt: grant.expiresAt },
    inherited.invoke, () => now);
  assert.deepEqual(inherited.calls.map((c) => c.action), ["ready"]);
  const invalid = fixture();
  await assert.rejects(pipelineUse({ ...base, action: "consume", sha: "bad", deploymentId: "dep-1",
    expiresAt: grant.expiresAt }, invalid.invoke, () => now), (error) => error.code === "staging_identity_invalid");
  assert.deepEqual(invalid.calls, []);
}

await assertOwnership();
await assertExpiryAndStandalone();
