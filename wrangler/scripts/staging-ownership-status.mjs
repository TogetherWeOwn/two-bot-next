#!/usr/bin/env node
// Staging-only ownership status read. Bounded GET /internal/ownership only.
// Secrets come from env, never argv or stdout. Emits only a validated
// owner id and a UTC observation timestamp; nothing else is printable.
import { control } from './ownership-control.mjs';
import { isDirectInvocation } from './is-direct-invocation.mjs';

class StatusError extends Error {}

const VALID_ID = /^[a-zA-Z0-9_-]{1,128}$/;
const PRINTABLE = /^(Expected|OWNERSHIP_CONTROL_TOKEN|Ownership control failed|Ownership status|Invalid ownership|Invalid observation|Singleton is)/;

function isPrintableControlError(error) {
  // Parser/transport errors are never printable, whatever their text.
  if (!(error instanceof Error)) return false;
  if (error instanceof SyntaxError || error instanceof TypeError) return false;
  if (error.name === 'AbortError' || error.name === 'TimeoutError') return false;
  return PRINTABLE.test(error.message);
}
const VALID_ACTOR = /^[a-zA-Z0-9_.:@/-]{1,128}$/;

function isValidId(value) {
  return typeof value === 'string' && VALID_ID.test(value);
}

function validOwnerRecord(owner) {
  if (owner === null) return true;
  if (!owner || typeof owner !== 'object' || Array.isArray(owner)) return false;
  const { deploymentId, epoch, phase, actor, timestamp, oldEpoch, oldDeploymentId } = owner;
  if (!(deploymentId === null || isValidId(deploymentId))) return false;
  if (!Number.isSafeInteger(epoch) || epoch <= 0) return false;
  if (phase !== 'active' && phase !== 'fenced') return false;
  if (phase === 'active' && !isValidId(deploymentId)) return false;
  if (typeof actor !== 'string' || !VALID_ACTOR.test(actor)) return false;
  if (typeof timestamp !== 'string' || !Number.isFinite(Date.parse(timestamp))) return false;
  if (!Number.isSafeInteger(oldEpoch) || oldEpoch !== epoch - 1) return false;
  if (!(oldDeploymentId === null || isValidId(oldDeploymentId))) return false;
  return true;
}

export function sanitizeStatus(state, observedAt) {
  if (!state || typeof state !== 'object' || Array.isArray(state)) {
    throw new StatusError('Invalid ownership status response; stop without substituting credentials');
  }
  const { deploymentId, owner, running } = state;
  if (!isValidId(deploymentId)) {
    throw new StatusError('Invalid ownership status response; stop without substituting credentials');
  }
  if (!('owner' in state) || !validOwnerRecord(owner)) {
    throw new StatusError('Invalid ownership status response; stop without substituting credentials');
  }
  if (typeof running !== 'boolean') {
    throw new StatusError('Invalid ownership status response; stop without substituting credentials');
  }
  if (typeof observedAt !== 'string' || !/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?Z$/.test(observedAt) ||
      !Number.isFinite(Date.parse(observedAt))) {
    throw new StatusError('Invalid observation timestamp; stop without substituting credentials');
  }
  // Only an active record owns the singleton. A fenced record keeps its
  // pending deploymentId through crash-recovery, but require() refuses it,
  // so reporting it would name an owner where nobody owns the singleton.
  const ownerId = owner?.phase === 'active' ? owner.deploymentId : null;
  if (!(ownerId === null || isValidId(ownerId))) {
    throw new StatusError('Invalid ownership status response; stop without substituting credentials');
  }
  return { owner_id: ownerId, observed_at: observedAt };
}

export async function readOwnershipStatus({ url, token }, send = fetch, now = () => new Date()) {
  if (!url || !token) {
    throw new StatusError('Ownership status is not configured; stop without substituting credentials');
  }
  let state;
  try {
    state = await control({ action: 'status', url, token }, send);
  } catch (error) {
    // Fail closed on transport/redirect failures without echoing raw text.
    // Only the control client's fixed-text validation errors are printable.
    if (isPrintableControlError(error)) {
      if (error.message === 'Invalid ownership control response') {
        throw new StatusError('Invalid ownership status response; stop without substituting credentials');
      }
      throw error;
    }
    throw new StatusError('Ownership status check failed; stop and report without substituting credentials');
  }
  const observedAt = now() instanceof Date ? now().toISOString() : new Date(now()).toISOString();
  return sanitizeStatus(state, observedAt);
}

if (isDirectInvocation(process.argv[1], import.meta.url)) {
  try {
    if (process.argv.length !== 2) {
      throw new StatusError('Ownership status takes no arguments; stop without substituting credentials');
    }
    const receipt = await readOwnershipStatus({
      url: process.env.STAGING_WORKER_URL ?? '',
      token: process.env.OWNERSHIP_CONTROL_TOKEN,
    });
    console.log(JSON.stringify(receipt));
  } catch (error) {
    // Only fixed-text validation errors are printable. Provider/JSON errors
    // are untrusted, even when their message starts with "Expected" or
    // "Invalid". Never echo response, headers, token, environment or raw errors.
    const message = isPrintableControlError(error)
      ? error.message
      : 'Ownership status check failed; stop and report without substituting credentials';
    // Guard against a token-looking fixed string ever reaching the log.
    if (/[A-Za-z0-9_-]{32,}/.test(message) && (process.env.OWNERSHIP_CONTROL_TOKEN ?? '') !== '' &&
        message.includes(process.env.OWNERSHIP_CONTROL_TOKEN ?? '\u0000-never-present')) {
      console.error('Ownership status check failed; stop and report without substituting credentials');
    } else {
      console.error(message);
    }
    process.exitCode = 1;
  }
}
