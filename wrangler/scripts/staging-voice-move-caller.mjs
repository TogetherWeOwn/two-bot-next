import { priorVoiceCleanupMatches, runStagingVoiceMoveProof } from "./staging-voice-move-acceptance.mjs";
import { bindStagingVoiceSession, stagingVoiceSessionScopeMatches } from "./staging-voice-session.mjs";

const LEASE_MS = 15 * 60_000;
const READY_MAX_AGE_MS = 60_000;
const SHA = /^[0-9a-f]{40}$/;
const OBSERVED_AT = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/;
const positive = (value) => Number.isSafeInteger(value) && value > 0;
const matches = (value, pattern) => typeof value === "string" && pattern.test(value);
const snowflake = (value) => matches(value, /^[1-9][0-9]{0,19}$/) && BigInt(value) <= 0xffffffffffffffffn;
const receiptFor = (receipt, action) => receipt?.schema === "two-bot-staging-use.v1"
  && receipt.action === action && receipt.environment === "staging";

function leaseFromJob(window, ready, at) {
  const { runId, runAttempt, expiresAt, sha, deploymentId } = window ?? {};
  const { epoch, use, observedAt } = ready ?? {};
  if (!matches(runId, /^[1-9][0-9]{0,19}$/) || !positive(runAttempt) || !positive(expiresAt)
    || !matches(sha, SHA) || !matches(deploymentId, /^[a-zA-Z0-9_-]{1,128}$/)
    || !positive(epoch) || !positive(at)) return null;

  const lease = { runId, runAttempt, expiresAt, sha, deploymentId, epoch };
  if (!receiptFor(ready, "ready") || ready.running !== true || ready.voiceMoveEnabled !== true
    || ready.sha !== sha || ready.deploymentId !== deploymentId
    || use?.environment !== "staging" || use.phase !== "active"
    || !Object.entries(lease).every(([key, value]) => use[key] === value)) return null;

  const readyAt = matches(observedAt, OBSERVED_AT) ? Date.parse(observedAt) : NaN;
  if (!positive(readyAt) || new Date(readyAt).toISOString() !== observedAt
    || !positive(use.acquiredAt) || use.acquiredAt > readyAt || readyAt > at
    || at - readyAt > READY_MAX_AGE_MS || at >= expiresAt - 1000
    || expiresAt - use.acquiredAt > LEASE_MS) return null;
  return lease;
}

export function stagingVoiceFixturePin(fixture) {
  const { environment, guildId, discordId, channelIds } = fixture ?? {};
  if (environment !== "staging" || !snowflake(guildId) || !Array.isArray(channelIds)
    || channelIds.length !== 2) return null;
  const [source, destination] = channelIds;
  if (![discordId, source, destination].every((id) => snowflake(id) && id.length >= 17)
    || new Set([guildId, discordId, source, destination]).size !== 4) return null;
  return { environment, guildId, discordId, channelIds: [source, destination] };
}

// Compose only approved in-process inputs. The job supplies the lease separately.
function provideStagingVoiceMoveInputs(options) {
  const { fixture, previousReady, previousOff, webSession, expectedRevision, observer, control, now = Date.now } = options;
  const pinnedFixture = stagingVoiceFixturePin(fixture);
  if (!pinnedFixture || webSession?.environment !== "staging" || webSession.discordId !== pinnedFixture.discordId) {
    return { verdict: "failed", code: "fixture_unconfirmed" };
  }

  if (!stagingVoiceSessionScopeMatches({ session: webSession, fixture: pinnedFixture, expectedRevision })
    || !receiptFor(previousReady, "ready") || !receiptFor(previousOff, "status")
    || typeof observer?.readChannels !== "function" || typeof observer.observe !== "function"
    || typeof control !== "function" || typeof now !== "function") {
    return { verdict: "failed", code: "proof_inputs_incomplete" };
  }

  const { version_id, commit } = expectedRevision;
  return { fixture: pinnedFixture, previousReady, previousOff, webSession,
    expectedRevision: { version_id, commit }, observer, control, now };
}

/**
 * Call between a successful protected `begin` and the parent job's unconditional `finish`.
 * `window` uses the current run ID and numeric attempt, plus the `use` step's
 * `expires_at` (converted to a number), `sha` and `deployment_id` outputs. Pass
 * its unmodified ready receipt as `ready`; the epoch comes from that receipt.
 * Supply the approved ordered fixture and Web `expectedRevision` independently
 * of those Bot outputs. Pass the prior lease receipts and scoped adapters from
 * the protected context. Keep activation and any receipt retention in that job.
 */
export async function runProtectedStagingVoiceMove(options, runProof = runStagingVoiceMoveProof) {
  if (options?.enabled !== true) return { verdict: "disabled" };

  const inputs = provideStagingVoiceMoveInputs(options);
  if ("verdict" in inputs) return inputs;
  const { window, ready } = options;
  const { now, previousReady, previousOff } = inputs;
  const lease = leaseFromJob(window, ready, now());
  if (!lease) return { verdict: "failed", code: "lease_unconfirmed" };
  if (!priorVoiceCleanupMatches(lease, ready, previousReady, previousOff)) {
    return { verdict: "failed", code: "prior_cleanup_unconfirmed" };
  }

  return runProof({ ...inputs, enabled: true, lease, ready, bindSession: bindStagingVoiceSession });
}
