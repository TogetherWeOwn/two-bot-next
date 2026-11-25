import { runStagingVoiceMoveProof } from "./staging-voice-move-acceptance.mjs";
import { bindStagingVoiceSession } from "./staging-voice-session.mjs";

const LEASE_MS = 15 * 60_000;
const READY_MAX_AGE_MS = 60_000;
const SHA = /^[0-9a-f]{40}$/;
const WORKER_VERSION = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
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

function fixturePin(fixture) {
  const { environment, guildId, discordId, channelIds } = fixture ?? {};
  if (environment !== "staging" || !snowflake(guildId) || !Array.isArray(channelIds)
    || channelIds.length !== 2) return null;
  const [source, destination] = channelIds;
  if (![discordId, source, destination].every((id) => snowflake(id) && id.length >= 17)
    || new Set([guildId, discordId, source, destination]).size !== 4) return null;
  return { environment, guildId, discordId, channelIds: [source, destination] };
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

  const { window, ready, fixture, previousReady, previousOff, webSession, expectedRevision,
    observer, control, now = Date.now } = options;
  const pinnedFixture = fixturePin(fixture);
  if (!pinnedFixture || webSession?.environment !== "staging" || webSession.discordId !== pinnedFixture.discordId) {
    return { verdict: "failed", code: "fixture_unconfirmed" };
  }

  const { version_id, commit } = expectedRevision ?? {};
  if (!matches(version_id, WORKER_VERSION) || !matches(commit, SHA)
    || !receiptFor(previousReady, "ready") || !receiptFor(previousOff, "status")
    || typeof webSession.request !== "function" || typeof webSession.move !== "function"
    || typeof observer?.readChannels !== "function" || typeof observer.observe !== "function"
    || typeof control !== "function" || typeof now !== "function") {
    return { verdict: "failed", code: "proof_inputs_incomplete" };
  }

  const lease = leaseFromJob(window, ready, now());
  if (!lease) return { verdict: "failed", code: "lease_unconfirmed" };

  return runProof({ enabled: true, lease, ready, fixture: pinnedFixture, previousReady, previousOff,
    bindSession: bindStagingVoiceSession, webSession, expectedRevision: { version_id, commit }, observer, control, now });
}
