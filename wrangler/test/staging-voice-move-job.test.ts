import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { appendFileSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test, type TestContext } from "node:test";
import { fileURLToPath } from "node:url";
import { pipelineUse } from "../scripts/staging-use-pipeline.mjs";
import { runStagingVoiceMoveJob } from "../scripts/staging-voice-move-job.mjs";
import { mockVoiceMoveWindow } from "./fixtures/staging-voice-move.ts";

function beginFixture(ready: ReturnType<typeof mockVoiceMoveWindow>["args"]["ready"], dir: string) {
  const { use, deploymentId, sha } = ready;
  const path = join(dir, "start.jsonl");
  const output = join(dir, "output");
  const run = { GITHUB_RUN_ID: use.runId, GITHUB_RUN_ATTEMPT: String(use.runAttempt) };
  // Replace fetch before loading the real CLI. Only synthetic responses exist
  // in this child. Begin writes the actual receipt and GitHub output files.
  const transport = `
    import assert from "node:assert/strict";
    const use = ${JSON.stringify(use)};
    const { acquiredAt, deploymentId, sha, epoch, runId, runAttempt, expiresAt } = use;
    Date.now = () => acquiredAt;
    let active = false;
    globalThis.fetch = async (url, init) => {
      if (url.pathname === "/readyz") {
        assert.equal(init.method, "GET");
        assert.equal(active, true);
        return Response.json({ build_revision: sha, components: [["gateway", "ready"]] },
          { headers: { "x-two-worker-version": deploymentId } });
      }
      assert.equal(url.pathname, "/internal/staging-use");
      if (init.method === "POST") {
        assert.equal(active, false);
        assert.deepEqual(JSON.parse(init.body), { action: "acquire", environment: "staging",
          deploymentId, sha, expectedEpoch: epoch, runId, runAttempt, expiresAt, voiceMove: true });
        active = true;
      } else {
        assert.equal(init.method, "GET");
      }
      return Response.json({ deploymentId, sha, owner: { deploymentId, epoch, phase: "active" },
        running: active, use: active ? use : null, voiceMoveEnabled: active });
    };
  `;
  const cli = fileURLToPath(new URL("../scripts/staging-use-pipeline.mjs", import.meta.url));
  const result = spawnSync(process.execPath, ["--import", "data:text/javascript," + encodeURIComponent(transport), cli, "begin"],
    { env: { ...run, GITHUB_OUTPUT: output, STAGING_USE_RECEIPT: path, STAGING_USE_VOICE_MOVE: "1",
      STAGING_USE_MINUTES: "15", STAGING_USE_EXPECTED_SHA: sha,
      OWNERSHIP_CONTROL_TOKEN: "synthetic".repeat(4),
      STAGING_WORKER_URL: "https://two-bot-next-staging.example.workers.dev/" }, encoding: "utf8", timeout: 5000 });
  assert.equal(result.status, 0, result.stderr);
  const outputs = Object.fromEntries(readFileSync(output, "utf8").trimEnd().split("\n").map((line) => line.split("=")));
  assert.deepEqual(outputs, { expires_at: String(use.expiresAt), deployment_id: deploymentId, sha });
  assert.deepEqual(JSON.parse(readFileSync(path, "utf8").trimEnd().split("\n").at(-1)!), {
    ...ready, runId: use.runId, runAttempt: use.runAttempt, observedAt: new Date(use.acquiredAt).toISOString(),
  });
  return { ...run, STAGING_USE_RECEIPT: path, STAGING_USE_EXPIRES_AT: outputs.expires_at,
    STAGING_USE_SHA: outputs.sha, STAGING_USE_DEPLOYMENT: outputs.deployment_id };
}

function jobInputs(t: TestContext) {
  const dir = mkdtempSync(join(tmpdir(), "staging-voice-move-job-"));
  t.after(() => rmSync(dir, { recursive: true }));
  const { args, calls, signed, moves } = mockVoiceMoveWindow();
  const { ready, fixture, caller, evidence, now } = args;
  const env = { ...beginFixture(ready, dir), STAGING_VOICE_MOVE_PROOF: "true" };
  const expectedRevision = { version_id: "11111111-2222-3333-4444-555555555555", commit: "b".repeat(40) };
  const reads: string[] = [];
  const inputs = {
    fixture, expectedRevision, now, control: args.control,
    previousReady: evidence.previousReady, previousOff: evidence.previousOff,
    webSession: {
      ...caller,
      origin: "https://next.togetherweown.com",
      async request(url: string, init: RequestInit) {
        init.signal?.throwIfAborted();
        reads.push("session");
        return Object.defineProperty(new Response(JSON.stringify({
          matched: true, revision: expectedRevision, observed_at: evidence.session.observedAt,
        }), { headers: { "content-type": "application/json", "cache-control": "private, no-store" } }),
        "url", { value: url });
      },
    },
    observer: {
      async readChannels({ signal }: { signal: AbortSignal }) {
        signal.throwIfAborted();
        reads.push("channels");
        return evidence.readback;
      },
      async observe(request: Parameters<typeof args.observe>[0]) {
        const reading = await args.observe(request);
        return { ...reading, observedAt: new Date(now()).toISOString(), channelType: 2 };
      },
    },
  };
  async function finish() {
    // The workflow contract pins this as an always() step with the original
    // expiry from begin, independent of the proof's exit status.
    const off = await pipelineUse({
      action: "finish", voiceMove: true, runId: env.GITHUB_RUN_ID,
      runAttempt: Number(env.GITHUB_RUN_ATTEMPT), expiresAt: Number(env.STAGING_USE_EXPIRES_AT),
    }, args.control, now);
    assert.deepEqual(off.use, { ...ready.use, phase: "released" });
    assert.equal(off.epoch, ready.epoch);
    assert.equal(off.voiceMoveEnabled, false);
    assert.equal(off.running, false);
  }
  return { env, inputs, signed, moves, calls, reads, finish };
}

test("job defaults off before reading proof inputs", async (t) => {
  const { env, inputs, signed, calls, reads, finish } = jobInputs(t);
  for (const selected of [undefined, "false", "TRUE", true]) {
    assert.equal(await runStagingVoiceMoveJob({ ...env, STAGING_VOICE_MOVE_PROOF: selected,
      STAGING_USE_RECEIPT: undefined }, inputs), "disabled");
  }
  assert.deepEqual({ signed, calls, reads }, { signed: [], calls: [], reads: [] });
  await finish();
  assert.deepEqual(calls, ["status", "release", "status"]);
});

test("selected job refuses incomplete or mismatched inputs before any self-move", async (t) => {
  const { env, inputs, signed, calls, reads, finish } = jobInputs(t);
  for (const change of [
    { GITHUB_RUN_ID: "54321" },
    { GITHUB_RUN_ATTEMPT: "01" },
    { STAGING_USE_EXPIRES_AT: "" },
    { STAGING_USE_EXPIRES_AT: String(Number(env.STAGING_USE_EXPIRES_AT) + 1) },
    { STAGING_USE_SHA: "c".repeat(40) },
    { STAGING_USE_DEPLOYMENT: "other-deployment" },
    { STAGING_USE_RECEIPT: undefined },
  ]) {
    assert.equal(await runStagingVoiceMoveJob({ ...env, ...change }, inputs), "failed");
  }
  assert.equal(await runStagingVoiceMoveJob(env, { ...inputs, previousOff: undefined }), "failed");
  // A ready line followed by a failure or malformed row must not authorize the proof.
  appendFileSync(env.STAGING_USE_RECEIPT, '{"action":"failed"}\n');
  assert.equal(await runStagingVoiceMoveJob(env, inputs), "failed");
  appendFileSync(env.STAGING_USE_RECEIPT, "{\n");
  assert.equal(await runStagingVoiceMoveJob(env, inputs), "failed");
  assert.deepEqual({ signed, calls, reads }, { signed: [], calls: [], reads: [] });
  await finish();
  assert.deepEqual(calls, ["status", "release", "status"]);
});

test("workflow CLI has no adapter provider even with explicit selection", (t) => {
  const { env } = jobInputs(t);
  const cli = fileURLToPath(new URL("../scripts/staging-voice-move-job.mjs", import.meta.url));
  const result = spawnSync(process.execPath, [cli], { env, encoding: "utf8" });
  assert.equal(result.status, 1);
  assert.equal(result.stdout, "");
  assert.equal(result.stderr, "staging_voice_move_proof_failed\n");
});

test("selected synthetic job finishes the exact lease after either proof outcome", async (t) => {
  for (const fail of [false, true]) {
    const { env, inputs, signed, moves, calls, reads, finish } = jobInputs(t);
    if (fail) {
      const observe = inputs.observer.observe;
      inputs.observer.observe = async (request) => {
        if (request.checkpoint === "after:moved") throw new Error("private observer error");
        return observe(request);
      };
    }
    try {
      assert.equal(await runStagingVoiceMoveJob(env, inputs), fail ? "failed" : "passed");
      assert.deepEqual(reads, ["session", "channels"]);
      const [source, destination] = inputs.fixture.channelIds;
      assert.deepEqual(signed.map(({ channel_id }) => channel_id), fail ? [destination] : [destination, source, destination]);
      assert.deepEqual(moves, [{ discordId: inputs.fixture.discordId, channelId: destination }]);
      assert.deepEqual(calls, [...Array(fail ? 1 : 3).fill("assert"), "status", "release", "status"]);
    } finally {
      await finish();
    }
    assert.deepEqual(calls.slice(-3), ["status", "release", "status"]);
  }
});
