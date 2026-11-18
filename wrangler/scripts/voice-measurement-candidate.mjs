// Independently recompute candidate evidence against the original grant.
import { readFileSync } from 'node:fs';
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

export function runCli(args) {
  try {
    if (args.some((value) => !value)) throw new Error();
    const [branch, grantPath, candidatePath, ...receiptPaths] = args;
    if (!['true', 'false'].includes(branch)
      || receiptPaths.length !== (branch === 'true' ? 3 : 2)) throw new Error();
    const read = (path) => JSON.parse(readFileSync(path, 'utf8'));
    if (!verifyCandidate(receiptPaths.map(read), read(candidatePath), branch === 'true', read(grantPath))) {
      throw new Error();
    }
    return 0;
  } catch {
    console.error('voice measurement candidate refused');
    return 1;
  }
}

if (isDirectInvocation(process.argv[1], import.meta.url)) {
  process.exitCode = runCli(process.argv.slice(2));
}
