// Independently recompute candidate evidence against the original grant.
import { closeSync, constants, fstatSync, openSync, readSync } from 'node:fs';
import { isDeepStrictEqual } from 'node:util';
import { isDirectInvocation } from './promotion-error-gate.mjs';
import { combinedDecision, firstIntervalDecision } from './voice-measurement-decision.mjs';

export function verifyCandidate(receipts, candidate, secondRequired, grant, options) {
  if (!Array.isArray(receipts) || ![2, 3].includes(receipts.length)
    || typeof secondRequired !== 'boolean' || receipts.length !== (secondRequired ? 3 : 2)
    || !grant || typeof grant !== 'object') return false;
  try {
    const expected = secondRequired
      ? combinedDecision(...receipts, options)
      : firstIntervalDecision(...receipts, options);
    if (!secondRequired && expected.secondRequired) return false;
    // The comparisons validate timestamps; each interval still needs a fresh capture.
    if (receipts.some((receipt, i) => i > 0
      && Date.parse(receipt.capturedAt) <= Date.parse(receipts[i - 1].capturedAt))) return false;
    const first = receipts[0];
    if (!['runId', 'runAttempt', 'sha', 'deploymentId', 'expiresAt']
      .every((key) => typeof grant[key] === 'string' && first[key] === grant[key])) return false;
    return isDeepStrictEqual(candidate, expected);
  } catch {
    return false;
  }
}

const MAX_EVIDENCE_BYTES = 65536;

function readEvidence(path) {
  const fd = path === 0 ? 0 : openSync(path, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
  try {
    if (path !== 0) {
      const file = fstatSync(fd);
      if (!file.isFile() || file.size > MAX_EVIDENCE_BYTES) throw new Error();
    }
    const bytes = Buffer.alloc(MAX_EVIDENCE_BYTES + 1);
    let length = 0;
    while (length < bytes.length) {
      const count = readSync(fd, bytes, length, bytes.length - length, null);
      if (count === 0) break;
      length += count;
    }
    if (length > MAX_EVIDENCE_BYTES) throw new Error();
    const text = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(bytes.subarray(0, length));
    const value = JSON.parse(text);
    // On valid JSON, a quoted token followed by a colon belongs to the innermost object.
    const objects = [];
    for (const [token, key, colon] of text.matchAll(/("(?:[^"\\]|\\[\s\S])*")(\s*:)?|[{}]/g)) {
      if (token === '{') objects.push(new Set());
      else if (token === '}') objects.pop();
      else if (colon) {
        const name = JSON.parse(key);
        const names = objects.at(-1);
        if (names.has(name)) throw new Error();
        names.add(name);
      }
    }
    return value;
  } finally {
    if (path !== 0) closeSync(fd);
  }
}

export function runCli(args) {
  try {
    if (args.some((value) => !value)) throw new Error();
    const [branch, grantPath, candidatePath, ...receiptPaths] = args;
    if (!['true', 'false'].includes(branch)
      || receiptPaths.length !== (branch === 'true' ? 3 : 2)) throw new Error();
    // The terminal supplies the validated grant and cleanup window on stdin.
    const fromStdin = grantPath === '-';
    const grant = readEvidence(fromStdin ? 0 : grantPath);
    const receipts = receiptPaths.map(readEvidence);
    if (!verifyCandidate(receipts, readEvidence(candidatePath), branch === 'true', grant)) throw new Error();
    // The comparator already verifies strict capture ordering and expiry.
    if (fromStdin && (!Number.isSafeInteger(grant.acquiredAt) || grant.acquiredAt < 0
      || !Number.isSafeInteger(grant.cleanupStartedAt)
      || Date.parse(receipts[0].capturedAt) < grant.acquiredAt
      || Date.parse(receipts.at(-1).capturedAt) > grant.cleanupStartedAt)) throw new Error();
    return 0;
  } catch {
    console.error('voice measurement candidate refused');
    return 1;
  }
}

if (isDirectInvocation(process.argv[1], import.meta.url)) {
  process.exitCode = runCli(process.argv.slice(2));
}
