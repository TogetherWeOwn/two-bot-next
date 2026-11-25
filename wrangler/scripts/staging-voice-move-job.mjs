import { readFileSync } from "node:fs";
import { isDirectInvocation } from "./is-direct-invocation.mjs";
import { runProtectedStagingVoiceMove } from "./staging-voice-move-caller.mjs";

function decimalOutput(value) {
  return typeof value === "string" && /^[1-9][0-9]{0,15}$/.test(value) ? Number(value) : NaN;
}

/** Run after the protected job's successful `begin`. The job always owns `finish`.
 * A protected provider must supply proof inputs in process. The CLI has no provider.
 */
export async function runStagingVoiceMoveJob(env = {}, proofInputs) {
  if (env.STAGING_VOICE_MOVE_PROOF !== "true") return "disabled";

  try {
    // `begin` appends only bounded, redacted projections in this runner's temp
    // directory. Use its final row so a failure cannot fall back to an older ready.
    const rows = readFileSync(env.STAGING_USE_RECEIPT, "utf8").trimEnd().split("\n");
    const ready = JSON.parse(rows.at(-1));
    const result = await runProtectedStagingVoiceMove({
      ...proofInputs,
      enabled: true,
      ready,
      window: {
        runId: env.GITHUB_RUN_ID,
        runAttempt: decimalOutput(env.GITHUB_RUN_ATTEMPT),
        expiresAt: decimalOutput(env.STAGING_USE_EXPIRES_AT),
        sha: env.STAGING_USE_SHA,
        deploymentId: env.STAGING_USE_DEPLOYMENT,
      },
    });
    if (result.verdict === "passed" && result.mode === "mock" && result.cleanup?.memberDisconnected === true
      && result.cleanup.off === true && result.cleanup.stopped === true) return "passed";
  } catch {
    // Private input and adapter diagnostics stay out of job logs.
  }
  return "failed";
}

if (isDirectInvocation(process.argv[1], import.meta.url)) {
  const verdict = await runStagingVoiceMoveJob(process.env);
  if (verdict === "passed") console.log("staging_voice_move_mock_proof_passed");
  if (verdict === "failed") {
    console.error("staging_voice_move_proof_failed");
    process.exitCode = 1;
  }
}
