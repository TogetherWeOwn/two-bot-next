import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { runStagingVoiceMoveEntry } from "../../scripts/staging-voice-move-entry.mjs";
import { mockVoiceMoveWindow } from "./staging-voice-move.ts";

const scenario = process.argv[2];
const run = process.argv[3];
const restart = scenario === "restart-failed" || scenario === "restart-ready";
const { args, calls, signed } = mockVoiceMoveWindow(1000,
  restart ? { runId: run, deploymentId: `synthetic-deployment-${run}` } : undefined);
const { caller, evidence, fixture, lease, now, ready } = args;
const { runId, runAttempt, expiresAt, deploymentId, sha } = lease;
const receiptPath = process.env.STAGING_USE_RECEIPT!;
const prior = restart ? { ...ready, observedAt: new Date(ready.use.acquiredAt).toISOString() } : ready;
const priorLine = JSON.stringify(prior) + "\n";
writeFileSync(receiptPath, priorLine);
const runIdentity = { GITHUB_RUN_ID: runId, GITHUB_RUN_ATTEMPT: String(runAttempt) };
let outputs = { expires_at: String(expiresAt), deployment_id: deploymentId, sha };

if (restart) {
  // Keep the earlier same-window ready receipt. The real begin command appends
  // the restart outcome to that file, as on a runner with prior evidence.
  const failed = scenario === "restart-failed";
  const transport = `
    import assert from "node:assert/strict";
    const use = ${JSON.stringify(ready.use)};
    const { acquiredAt, deploymentId, sha, epoch, runId, runAttempt, expiresAt } = use;
    Date.now = () => acquiredAt + 1000;
    let running = false;
    globalThis.fetch = async (url, { method, body }) => {
      if (url.pathname === "/readyz") {
        assert.equal(method, "GET");
        assert.equal(running, true);
        return Response.json({
          build_revision: ${failed} ? "synthetic-private-restart-diagnostic" : sha,
          components: [["gateway", "ready"]],
        }, { headers: { "x-two-worker-version": deploymentId } });
      }
      assert.equal(url.pathname, "/internal/staging-use");
      if (method === "POST") {
        assert.equal(running, false);
        assert.deepEqual(JSON.parse(body), { action: "acquire", environment: "staging",
          deploymentId, sha, expectedEpoch: epoch, runId, runAttempt, expiresAt, voiceMove: true });
        running = true;
      } else {
        assert.equal(method, "GET");
      }
      return Response.json({ deploymentId, sha, owner: { deploymentId, epoch, phase: "active" },
        running, use, voiceMoveEnabled: true });
    };
  `;
  const cli = fileURLToPath(new URL("../../scripts/staging-use-pipeline.mjs", import.meta.url));
  const beginOutput = receiptPath + ".output";
  const result = spawnSync(process.execPath,
    ["--import", "data:text/javascript," + encodeURIComponent(transport), cli, "begin"], {
      env: {
        ...runIdentity, GITHUB_OUTPUT: beginOutput,
        STAGING_USE_RECEIPT: receiptPath, STAGING_USE_VOICE_MOVE: "1", STAGING_USE_MINUTES: "15",
        STAGING_USE_EXPECTED_SHA: sha, OWNERSHIP_CONTROL_TOKEN: "synthetic".repeat(4),
        STAGING_WORKER_URL: "https://two-bot-next-staging.example.workers.dev/",
      },
      encoding: "utf8",
      timeout: 5000,
    });
  assert.ifError(result.error);
  assert.equal(result.status, failed ? 1 : 0, result.stderr);
  assert.equal(result.stderr, failed ? "staging_readiness_identity_mismatch\n" : "");
  assert.equal(readFileSync(receiptPath, "utf8"), priorLine + result.stdout);
  const rows = result.stdout.trimEnd().split("\n").map((line) => JSON.parse(line));
  assert.deepEqual(rows.map(({ action }) => action),
    ["attempt", "status", "window", "acquire", failed ? "failed" : "ready"]);
  assert.equal(rows.at(-1).runId, runId);
  assert.equal(rows[2].expiresAt, expiresAt);
  assert.doesNotMatch(result.stdout, /synthetic-private-restart-diagnostic/);
  const collected = Object.fromEntries(readFileSync(beginOutput, "utf8").trimEnd().split("\n")
    .map((line) => line.split("=")));
  assert.deepEqual(collected, outputs);
  outputs = collected;
}

Object.assign(process.env, {
  ...runIdentity,
  STAGING_USE_EXPIRES_AT: outputs.expires_at,
  STAGING_USE_DEPLOYMENT: outputs.deployment_id,
  STAGING_USE_SHA: outputs.sha,
});

let fetchCalls = 0;
globalThis.fetch = async () => {
  fetchCalls += 1;
  throw new Error("unexpected network request in synthetic entry test");
};

const expectedRevision = { version_id: "11111111-2222-3333-4444-555555555555", commit: "b".repeat(40) };
let providerCalls = 0;
const reads: string[] = [];
const verdict = await runStagingVoiceMoveEntry(process.env, async () => {
  providerCalls += 1;
  if (scenario === "provider-refused") {
    throw new Error(`synthetic provider diagnostic for ${fixture.discordId}\nfixture=${fixture.channelIds.join(",")}`);
  }
  return {
    fixture, now, expectedRevision,
    previousReady: evidence.previousReady,
    previousOff: evidence.previousOff,
    webSession: {
      ...caller,
      origin: "https://next.togetherweown.com",
      async request(url: string, init: RequestInit) {
        init.signal?.throwIfAborted();
        reads.push("session");
        const response = Response.json({
          matched: true, revision: expectedRevision, observed_at: evidence.session.observedAt,
        }, { headers: { "cache-control": "private, no-store" } });
        return Object.defineProperty(response, "url", { value: url });
      },
    },
    observer: {
      async readChannels({ signal }: { signal: AbortSignal }) {
        signal.throwIfAborted();
        reads.push("channels");
        return evidence.readback;
      },
      async observe(request: Parameters<typeof args.observe>[0]) {
        return { ...await args.observe(request), channelType: 2, observedAt: new Date(now()).toISOString() };
      },
    },
    async control(request: Parameters<typeof args.control>[0]) {
      const state = await args.control(request);
      if (scenario === "cleanup-uncertain" && request.action === "status" && state.use.phase === "released") {
        return { ...state, voiceMoveEnabled: true };
      }
      return state;
    },
  };
});

assert.equal(fetchCalls, 0);
assert.equal(providerCalls, scenario === "disabled" ? 0 : 1);
const reachesCleanup = scenario === "coherent" || scenario === "cleanup-uncertain" || scenario === "restart-ready";
assert.deepEqual(reads, reachesCleanup ? ["session", "channels"] : []);
assert.equal(signed.length, reachesCleanup ? 3 : 0);
assert.deepEqual(calls, reachesCleanup ? ["assert", "assert", "assert", "status", "release", "status"] : []);

// Export only the entry's return value to the parent. Any other writes to
// stdout, stderr or the GitHub files belong to the code under test.
process.stdout.write(`${verdict}\n`);
