import { isDirectInvocation } from "./is-direct-invocation.mjs";
import { runStagingVoiceMoveJob } from "./staging-voice-move-job.mjs";
import { stagingVoiceWebHandoffInputs } from "./staging-voice-web-handoff.mjs";

/** Bind dependencies already held by the protected caller. The job supplies
 * the active lease and clock. Its existing admission checks validate the inputs.
 */
export function bindStagingVoiceMoveProvider(dependencies) {
  return () => {
    const { fixture, expectedRevision, webSession, observer, previousReady, previousOff, control } = dependencies ?? {};
    return { fixture, expectedRevision, webSession, observer, previousReady, previousOff, control };
  };
}

/** Bind a completed protected Web handoff at the existing provider boundary. */
export function bindStagingVoiceMoveHandoffProvider(dependencies, evidence) {
  const provide = bindStagingVoiceMoveProvider(dependencies);
  return () => stagingVoiceWebHandoffInputs(provide(), evidence);
}

/** The trusted caller supplies a provider for the prepared, in-process proof
 * inputs. The provider assembles the prior receipts, fixture and scoped
 * adapters. Protected I/O belongs in those adapters, under the proof's
 * deadlines. The job reads its own ready receipt and the workflow owns finish.
 */
export async function runStagingVoiceMoveEntry(env = {}, provideProofInputs) {
  if (env.STAGING_VOICE_MOVE_PROOF !== "true") return "disabled";
  if (typeof provideProofInputs !== "function") return "failed";

  try {
    return await runStagingVoiceMoveJob(env, await provideProofInputs());
  } catch {
    // Provider failures can include private inputs. Report only the verdict.
    return "failed";
  }
}

if (isDirectInvocation(process.argv[1], import.meta.url)) {
  // The protected fixture dependencies are supplied by an in-process caller.
  const verdict = await runStagingVoiceMoveEntry(process.env);
  if (verdict === "passed") console.log("staging_voice_move_mock_proof_passed");
  if (verdict === "failed") {
    console.error("staging_voice_move_proof_failed");
    process.exitCode = 1;
  }
}
