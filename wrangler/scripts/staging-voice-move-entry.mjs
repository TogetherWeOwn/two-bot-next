import { isDirectInvocation } from "./is-direct-invocation.mjs";
import { stagingVoiceFixturePin } from "./staging-voice-move-caller.mjs";
import { runStagingVoiceMoveJob } from "./staging-voice-move-job.mjs";
import { stagingVoiceSessionScopeMatches } from "./staging-voice-session.mjs";
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
  return runEntry(env, provideProofInputs);
}

/** Rehearse with mock adapters and a frozen, in-process approval. The approval
 * pins the current run and attempt (as runner strings), ordered fixture, Web
 * revision and exact scoped Web session before the job reads its ready row.
 */
export function runStagingVoiceMoveRehearsal(env = {}, approval, dependencies) {
  return runEntry(env, bindStagingVoiceMoveProvider(dependencies), (inputs) => {
    const { runId, runAttempt, fixture, expectedRevision, webSession } = approval ?? {};
    if (!Object.isFrozen(approval) || !runId || runId !== env.GITHUB_RUN_ID
      || !runAttempt || runAttempt !== env.GITHUB_RUN_ATTEMPT
      || fixture !== inputs.fixture || expectedRevision !== inputs.expectedRevision || webSession !== inputs.webSession) {
      return false;
    }
    const pinnedFixture = stagingVoiceFixturePin(fixture);
    return pinnedFixture !== null && [fixture, fixture.channelIds, expectedRevision, webSession].every(Object.isFrozen)
      && stagingVoiceSessionScopeMatches({ session: webSession, fixture: pinnedFixture, expectedRevision });
  });
}

async function runEntry(env, provideProofInputs, admit) {
  if (env.STAGING_VOICE_MOVE_PROOF !== "true") return "disabled";
  if (typeof provideProofInputs !== "function") return "failed";

  try {
    const inputs = await provideProofInputs();
    if (admit && !admit(inputs)) return "failed";
    return await runStagingVoiceMoveJob(env, inputs);
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
