import { pipelineUse } from "./staging-use-pipeline.mjs";
import { isRequestId } from "../src/request-id.ts";

const LEASE_MS = 15 * 60_000; // The protected voice-move workflow's finite window.
const CHECK_MS = 20_000;
export const READBACK_MAX_AGE_MS = 60_000;
const GUILD_VOICE = 2;
const positive = (value) => Number.isSafeInteger(value) && value > 0;
const matches = (value, pattern) => typeof value === "string" && pattern.test(value);
const snowflake = (value) => matches(value, /^[1-9][0-9]{0,19}$/) && BigInt(value) <= 0xffffffffffffffffn;
const exactObject = (value, keys) => value !== null && typeof value === "object" && !Array.isArray(value)
  && Object.keys(value).length === keys.length && keys.every((key) => Object.hasOwn(value, key));

class AcceptanceFailure extends Error {}
function require(condition, code) {
  if (!condition) throw new AcceptanceFailure(code);
}

function leaseBinding(lease) {
  const { runId, runAttempt, sha, deploymentId, epoch, expiresAt } = lease ?? {};
  if (!matches(runId, /^[1-9][0-9]{0,19}$/) || !positive(runAttempt)
    || !matches(sha, /^[0-9a-f]{40}$/) || !matches(deploymentId, /^[a-zA-Z0-9_-]{1,128}$/)
    || !positive(epoch) || !positive(expiresAt)) return null;
  return { runId, runAttempt, sha, deploymentId, epoch, expiresAt };
}

function leaseMatches(state, lease, action) {
  const { use } = state ?? {};
  return state?.schema === "two-bot-staging-use.v1" && state.action === action && state.environment === "staging"
    && use?.environment === "staging"
    && state.sha === lease.sha && state.deploymentId === lease.deploymentId && state.epoch === lease.epoch
    && Object.entries(lease).every(([key, value]) => use[key] === value);
}

function active(state, lease, action, at) {
  return leaseMatches(state, lease, action) && state.use.phase === "active"
    && state.running === true && state.voiceMoveEnabled === true
    && positive(state.use.acquiredAt) && state.use.acquiredAt <= at && at < lease.expiresAt
    && lease.expiresAt - state.use.acquiredAt <= LEASE_MS;
}

function observedAt(evidence) {
  const value = evidence?.observedAt;
  if (!matches(value, /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/)) return NaN;
  const at = Date.parse(value);
  return positive(at) && new Date(at).toISOString() === value ? at : NaN;
}

function observationMatches(state, fixture, channelId, connectedChannelId, canConnect) {
  return state?.guildId === fixture.guildId && state.discordId === fixture.discordId
    && state.channelId === channelId && state.connectedChannelId === connectedChannelId
    && state.canConnect === canConnect;
}

function assertReady(binding, ready, at) {
  const readyAt = observedAt(ready);
  require(positive(at) && active(ready, binding, "ready", readyAt) && readyAt <= at
    && at - readyAt <= READBACK_MAX_AGE_MS && at < binding.expiresAt - 1000, "lease_unconfirmed");
}

function assertFixture(fixture) {
  const { environment, guildId, discordId, channelIds } = fixture ?? {};
  require(environment === "staging" && snowflake(guildId) && Array.isArray(channelIds)
    && channelIds.length === 2, "fixture_invalid");
  const [source, destination] = channelIds;
  // The Web staging caller accepts only 17–20 digit member/channel snowflakes.
  require([discordId, source, destination].every((id) => snowflake(id) && id.length >= 17)
    && new Set([guildId, discordId, source, destination]).size === 4, "fixture_invalid");
  return [source, destination];
}

function assertCaller(caller, discordId, observe) {
  require(caller?.environment === "staging" && caller.discordId === discordId
    && typeof caller.move === "function" && typeof observe === "function", "caller_unconfirmed");
}

export function priorVoiceCleanupMatches(binding, ready, previousReady, previousOff) {
  const previous = leaseBinding(previousReady?.use);
  const previousReadyAt = observedAt(previousReady);
  const offAt = observedAt(previousOff);
  return previous !== null && (previous.runId !== binding.runId || previous.runAttempt !== binding.runAttempt)
    && previous.sha === binding.sha && previous.deploymentId === binding.deploymentId && previous.epoch === binding.epoch
    && active(previousReady, previous, "ready", previousReadyAt)
    && offAt >= previousReadyAt && offAt <= ready.use.acquiredAt && ready.use.acquiredAt - offAt <= LEASE_MS
    && leaseMatches(previousOff, previous, "status") && previousOff.running === false
    && previousOff.voiceMoveEnabled === false && previousOff.use.acquiredAt === previousReady.use.acquiredAt
    && (previousOff.use.phase === "released" || (previousOff.use.phase === "active" && offAt >= previous.expiresAt));
}

function assertPreflight(binding, ready, fixture, evidence, at) {
  assertReady(binding, ready, at);
  const [source, destination] = assertFixture(fixture);
  const { discordId } = fixture;
  const { session, readback, previousReady, previousOff } = evidence ?? {};
  const fresh = (value) => {
    const captured = observedAt(value);
    return captured >= ready.use.acquiredAt && captured <= at && at - captured <= READBACK_MAX_AGE_MS;
  };
  require(fresh(session) && session.environment === "staging" && session.authenticated === true
    && session.action === "voice.move_member" && session.discordId === discordId
    && Array.isArray(session.channelIds) && session.channelIds.length === 2
    && session.channelIds[0] === source && session.channelIds[1] === destination, "session_unconfirmed");
  const confirmsChannel = (state, channelId, canConnect) => fresh(state) && state.channelType === GUILD_VOICE
    && observationMatches(state, fixture, channelId, source, canConnect);
  require(confirmsChannel(readback?.source, source, false) && confirmsChannel(readback?.destination, destination, true),
    "discord_unconfirmed");

  require(priorVoiceCleanupMatches(binding, ready, previousReady, previousOff), "prior_cleanup_unconfirmed");
  return [source, destination];
}

/** Validate private projections from trusted readbacks without invoking adapters. */
export function preflightVoiceMoveAcceptance({ enabled = false, lease, ready, fixture, evidence, now = Date.now } = {}) {
  if (enabled !== true) return { verdict: "disabled" };
  try {
    const binding = leaseBinding(lease);
    require(binding, "lease_invalid");
    assertPreflight(binding, ready, fixture, evidence, now());
    return { verdict: "passed" };
  } catch (error) {
    return { verdict: "failed", code: error instanceof AcceptanceFailure ? error.message : "preflight_failed" };
  }
}

async function withinLease(lease, now, operation) {
  // The relay stops forwarding in the final second of the lease.
  const budget = Math.min(CHECK_MS, lease.expiresAt - now() - 1000);
  require(budget > 0, "lease_elapsed");
  const controller = new AbortController();
  let timer;
  try {
    const result = await Promise.race([
      new Promise((_, reject) => {
        timer = setTimeout(() => {
          reject(new AcceptanceFailure("probe_timeout"));
          controller.abort();
        }, budget);
      }),
      operation(controller.signal),
    ]);
    require(now() < lease.expiresAt, "lease_elapsed");
    return result;
  } finally {
    clearTimeout(timer);
  }
}

/**
 * Exercise a previously opened #2427 window. The enclosing protected job must
 * retain ownership of the window and keep its protected finish fallback for
 * runner loss. The pin, including epoch, comes from the protected ready receipt.
 *
 * control uses the staging-use client's projections and bounded deadlines,
 * including during finish. caller binds its one staging session and returns
 * {status, body} for the Bot envelope. observe reads independent staging
 * account/channel facts at each named checkpoint.
 * Both probe adapters must honor the supplied signal. This source ships only
 * mock evidence and no transport.
 */
export function runVoiceMoveAcceptance(options) {
  return runAcceptance(options);
}

async function runAcceptance({
  enabled = false, lease, ready, fixture, evidence, caller, observe, control, now = Date.now,
} = {}, bindReaders) {
  const receipt = {
    schema: "two-bot-staging-voice-move-acceptance.v1",
    environment: "staging",
    mode: "mock",
    verdict: "disabled",
    lease: null,
    cases: [],
    cleanup: { memberDisconnected: null, off: null, stopped: null },
  };
  if (enabled !== true) return receipt;

  const binding = leaseBinding(lease);
  receipt.verdict = "failed";
  if (!binding || typeof control !== "function") return { ...receipt, code: "lease_invalid" };
  receipt.lease = binding;
  const { runId, runAttempt, expiresAt, sha, deploymentId } = binding;

  try {
    let currentEvidence = evidence;
    if (bindReaders !== undefined) {
      assertReady(binding, ready, now());
      assertFixture(fixture);
      const readers = bindReaders();
      caller = readers.session;
      observe = readers.observe;
      assertCaller(caller, fixture.discordId, observe);
      currentEvidence = await withinLease(binding, now, async (signal) => {
        const currentSession = await caller.readCurrent({ signal });
        signal.throwIfAborted();
        const readback = await readers.readChannels({ signal });
        return {
          session: currentSession, readback, previousReady: readers.previousReady, previousOff: readers.previousOff,
        };
      });
    }
    const [source, destination] = assertPreflight(binding, ready, fixture, currentEvidence, now());
    const { guildId, discordId } = fixture;
    assertCaller(caller, discordId, observe);

    const cases = [
      { outcome: "moved", before: source, target: destination, after: destination, canConnect: true },
      { outcome: "not_permitted", before: destination, target: source, after: destination, canConnect: false },
      { outcome: "not_connected", before: null, target: destination, after: null, canConnect: true },
    ];
    const requestIds = new Set();
    for (const { outcome, before, target, after, canConnect } of cases) {
      const check = async (phase, connectedChannelId) => {
        const state = await withinLease(binding, now, (signal) => observe({
          checkpoint: `${phase}:${outcome}`, channelId: target, signal,
        }));
        require(observationMatches(state, { guildId, discordId }, target, connectedChannelId, canConnect),
          `${phase}_unconfirmed`);
        receipt.cleanup.memberDisconnected = connectedChannelId === null;
      };
      await check("before", before);
      const current = await withinLease(binding, now, () => control({
        action: "assert", runId, runAttempt, expiresAt, sha, deploymentId,
      }));
      require(active(current, binding, "assert", now()), "lease_unconfirmed");
      receipt.cleanup.memberDisconnected = null;
      // The caller gets a destination only. Never accept a member override or retry a move.
      const response = await withinLease(binding, now, (signal) => caller.move({ channelId: target, signal }));
      const { body, status } = response ?? {};
      const id = body?.request_id;
      const result = body?.result;
      const keys = outcome === "moved" ? ["outcome", "from_channel_id"] : ["outcome"];
      require(status === 200 && exactObject(body, ["ok", "request_id", "result"]) && body.ok === true
        && typeof id === "string" && isRequestId(id) && !requestIds.has(id)
        && exactObject(result, keys) && result.outcome === outcome
        && (outcome !== "moved" || result.from_channel_id === source), "outcome_unconfirmed");
      requestIds.add(id);
      await check("after", after);
      receipt.cases.push({ outcome, requestId: id });
    }
  } catch (error) {
    receipt.code = error instanceof AcceptanceFailure ? error.message : "probe_failed";
  } finally {
    try {
      const off = await pipelineUse({ action: "finish", runId, runAttempt, expiresAt, voiceMove: true },
        async (args) => {
          const state = await control(args);
          // Refuse even before release if the attempt was rebound to another build/epoch.
          if (args.action === "status") require(leaseMatches(state, binding, "status"), "cleanup_unconfirmed");
          return state;
        }, now);
      receipt.cleanup.off = off.voiceMoveEnabled === false;
      receipt.cleanup.stopped = off.running === false;
    } catch {
      receipt.code ??= "cleanup_unconfirmed";
    }
  }
  if (!receipt.code && receipt.cleanup.memberDisconnected === true
    && receipt.cleanup.off === true && receipt.cleanup.stopped === true) receipt.verdict = "passed";
  return receipt;
}

/**
 * Entry for the protected job that owns the active window and its finish fallback.
 * session.readCurrent and session.move use the same existing scoped Web session.
 * readChannels and observe use the independent reader bound to the pinned fixture.
 * Readbacks carry the producers' timestamps. Both readers must honor the signal.
 */
export function runProtectedVoiceMoveAcceptance({
  enabled = false, lease, ready, fixture, previousReady, previousOff,
  session, readChannels, observe, control, now,
} = {}) {
  return runAcceptance({ enabled, lease, ready, fixture, control, now },
    () => ({ session, readChannels, observe, previousReady, previousOff }));
}

/**
 * Compose the protected proof with the staging Web session binder and the
 * independent, fixture-scoped Discord observer. The job still owns its finish fallback.
 */
export function runStagingVoiceMoveProof({
  enabled = false, lease, ready, fixture, previousReady, previousOff,
  bindSession, webSession, expectedRevision, observer, control, now = Date.now,
} = {}) {
  return runAcceptance({ enabled, lease, ready, fixture, control, now }, () => ({
    // Binding can refuse. Do it inside the runner's same-window finish path.
    session: bindSession({ session: webSession, fixture, expectedRevision }),
    previousReady, previousOff,
    readChannels: (request) => observer.readChannels(request),
    async observe(request) {
      const startedAt = now();
      const state = await observer.observe(request);
      const captured = observedAt(state);
      // Each checkpoint needs a new guild-voice observation, even if occupancy stayed the same.
      require(state?.channelType === GUILD_VOICE && captured >= startedAt && captured <= now(), "discord_unconfirmed");
      return state;
    },
  }));
}
