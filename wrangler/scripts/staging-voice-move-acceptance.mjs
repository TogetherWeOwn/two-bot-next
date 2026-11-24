import { pipelineUse } from "./staging-use-pipeline.mjs";
import { isRequestId } from "../src/request-id.ts";

const LEASE_MS = 15 * 60_000; // The protected voice-move workflow's finite window.
const CHECK_MS = 20_000;
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
 * runner loss. The pin comes from trusted lease outputs, never the caller.
 *
 * control returns the existing staging-use projections. caller binds its one
 * staging session and returns {status, body} for the Bot envelope. observe
 * reads independent staging account/channel facts at each named checkpoint.
 * Both probe adapters must honor the supplied signal. This source ships only
 * mock evidence and no transport.
 */
export async function runVoiceMoveAcceptance({
  enabled = false, lease, ready, fixture, caller, observe, control, now = Date.now,
} = {}) {
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
    require(active(ready, binding, "ready", now()), "lease_unconfirmed");
    const { environment, guildId, discordId, channelIds } = fixture ?? {};
    require(environment === "staging" && snowflake(guildId) && snowflake(discordId)
      && Array.isArray(channelIds) && channelIds.length === 2, "fixture_invalid");
    const [source, destination] = channelIds;
    require(snowflake(source) && snowflake(destination) && new Set([guildId, discordId, source, destination]).size === 4,
      "fixture_invalid");
    require(caller?.environment === "staging" && caller.discordId === discordId
      && typeof caller.move === "function" && typeof observe === "function", "caller_unconfirmed");

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
        require(state?.guildId === guildId && state.discordId === discordId
          && state.channelId === target && state.connectedChannelId === connectedChannelId
          && state.canConnect === canConnect, `${phase}_unconfirmed`);
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
