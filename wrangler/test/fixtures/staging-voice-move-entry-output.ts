import assert from "node:assert/strict";
import { writeFileSync } from "node:fs";
import { runStagingVoiceMoveEntry } from "../../scripts/staging-voice-move-entry.mjs";
import { mockVoiceMoveWindow } from "./staging-voice-move.ts";

const scenario = process.argv[2];
const { args, calls, signed } = mockVoiceMoveWindow();
const { caller, evidence, fixture, lease, now, ready } = args;
const { runId, runAttempt, expiresAt, deploymentId, sha } = lease;
const receiptPath = process.env.STAGING_USE_RECEIPT!;
writeFileSync(receiptPath, JSON.stringify(ready) + "\n");
Object.assign(process.env, {
  GITHUB_RUN_ID: runId,
  GITHUB_RUN_ATTEMPT: String(runAttempt),
  STAGING_USE_EXPIRES_AT: String(expiresAt),
  STAGING_USE_DEPLOYMENT: deploymentId,
  STAGING_USE_SHA: sha,
});

let fetchCalls = 0;
globalThis.fetch = async () => {
  fetchCalls += 1;
  throw new Error("unexpected network request in synthetic entry test");
};

const expectedRevision = { version_id: "11111111-2222-3333-4444-555555555555", commit: "b".repeat(40) };
let providerCalls = 0;
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
        const response = Response.json({
          matched: true, revision: expectedRevision, observed_at: evidence.session.observedAt,
        }, { headers: { "cache-control": "private, no-store" } });
        return Object.defineProperty(response, "url", { value: url });
      },
    },
    observer: {
      async readChannels({ signal }: { signal: AbortSignal }) {
        signal.throwIfAborted();
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
const reachesCleanup = scenario === "coherent" || scenario === "cleanup-uncertain";
assert.equal(signed.length, reachesCleanup ? 3 : 0);
assert.deepEqual(calls, reachesCleanup ? ["assert", "assert", "assert", "status", "release", "status"] : []);

// Export only the entry's return value to the parent. Any other writes to
// stdout, stderr or the GitHub files belong to the code under test.
process.stdout.write(`${verdict}\n`);
