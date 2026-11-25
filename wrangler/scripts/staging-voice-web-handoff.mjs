import { READBACK_MAX_AGE_MS } from "./staging-voice-move-acceptance.mjs";
import { bindStagingVoiceSession, stagingVoiceSessionScopeMatches } from "./staging-voice-session.mjs";

const completedHandoffs = new WeakMap();
const unconfirmed = () => new Error("staging_voice_web_handoff_unconfirmed");
const SOAK_FIELDS = ["schema", "status", "sha", "workerVersion", "deployedAt", "deployRunId", "deployAttempt",
  "e2eRunId", "e2eAttempt", "e2eCompletedAt", "floorAt"];
const SAMPLE_FIELDS = ["observedAt", "workerVersion", "requests", "errors"];

function sameTransport(scope, webSession) {
  return scope.webSession === webSession && scope.request === webSession.request && scope.move === webSession.move;
}

/**
 * Capture the authenticated companion match. The scoped adapter keeps the same
 * installed session throughout. After Web Operations restores the disabled
 * baseline, confirmDisabled reads its refusal through that same adapter.
 * Both reads need a caller-supplied deadline. Only the opaque completion is
 * handed to the provider.
 */
export async function readStagingVoiceWebReady(inputs, options) {
  try {
    const { fixture, expectedRevision, webSession } = inputs;
    const { signal } = options ?? {};
    if (!stagingVoiceSessionScopeMatches({ session: webSession, fixture, expectedRevision })) throw unconfirmed();
    const revision = Object.freeze({ version_id: expectedRevision.version_id, commit: expectedRevision.commit });
    const { environment, guildId, discordId, channelIds } = fixture;
    const pin = Object.freeze({ environment, guildId, discordId, channelIds: Object.freeze([channelIds[0], channelIds[1]]) });
    const { request, move } = webSession;
    const transport = Object.freeze({ environment: webSession.environment, origin: webSession.origin,
      discordId: webSession.discordId, request: request.bind(webSession), move: move.bind(webSession) });
    const session = bindStagingVoiceSession({ session: transport, fixture: pin, expectedRevision: revision });
    const scope = { webSession, request, move, fixture: pin, revision, transport };
    const { observedAt } = await session.readCurrent({ signal });
    const readyAt = Date.parse(observedAt);
    const now = Date.now();
    if (readyAt > now || now - readyAt > READBACK_MAX_AGE_MS || !sameTransport(scope, webSession)) throw unconfirmed();

    return Object.freeze({
      async confirmDisabled(options) {
        try {
          const { signal } = options ?? {};
          if (!sameTransport(scope, webSession)) throw unconfirmed();
          await session.readDisabled({ signal });
          if (!sameTransport(scope, webSession)) throw unconfirmed();
          const handoff = Object.freeze({});
          completedHandoffs.set(handoff, scope);
          return handoff;
        } catch {
          throw unconfirmed();
        }
      },
    });
  } catch {
    throw unconfirmed();
  }
}

/**
 * Web Operations validates smoke, E2E and soak before issuing the protected
 * qualification decision. Match its rollout and sample to the unmodified Web
 * soak receipt, then bind both to the approved revision and session.
 */
export function stagingVoiceWebHandoffInputs(inputs, evidence) {
  const { qualification, soakReceipt, webHandoff } = evidence ?? {};
  const { fixture, expectedRevision, webSession } = inputs;
  const scope = completedHandoffs.get(webHandoff);
  if (!scope || !sameTransport(scope, webSession)
    || !stagingVoiceSessionScopeMatches({ session: webSession, fixture, expectedRevision })) throw unconfirmed();
  const { commit, version_id: versionId } = scope.revision;
  const pin = scope.fixture;
  if (expectedRevision?.commit !== commit || expectedRevision.version_id !== versionId
    || fixture.guildId !== pin.guildId || fixture.discordId !== pin.discordId
    || fixture.channelIds[0] !== pin.channelIds[0] || fixture.channelIds[1] !== pin.channelIds[1]) throw unconfirmed();

  if (qualification?.verdict !== "qualified" || soakReceipt?.schema !== "staging-soak/1"
    || soakReceipt.status !== "sampled" || soakReceipt.sha !== commit
    || soakReceipt.workerVersion !== versionId || soakReceipt.sample?.workerVersion !== versionId
    || !SOAK_FIELDS.every((field) => Object.hasOwn(soakReceipt, field) && qualification[field] === soakReceipt[field])
    || !SAMPLE_FIELDS.every((field) => Object.hasOwn(soakReceipt.sample, field)
      && qualification.sample?.[field] === soakReceipt.sample[field])) throw unconfirmed();
  const { observedAt } = soakReceipt.sample;
  const sampledAt = typeof observedAt === "string" ? Date.parse(observedAt) : NaN;
  if (!(sampledAt > 0 && sampledAt <= Date.now() && new Date(sampledAt).toISOString() === observedAt)) throw unconfirmed();
  return { ...inputs, fixture: pin, expectedRevision: scope.revision, webSession: scope.transport };
}
