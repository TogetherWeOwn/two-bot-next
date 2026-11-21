// Decide whether another voice invocation is needed from validated bookends.
import { readFileSync, writeFileSync } from 'node:fs';
import { isDirectInvocation } from './promotion-error-gate.mjs';
import { compareReceipts } from './voice-rest-receipt.mjs';

export function firstIntervalDecision(before, after, options) {
  const comparison = compareReceipts(before, after, options);
  return { comparison, secondRequired: comparison.restTotalDelta < 10 };
}

// Validate both intervals independently so an intermediate reset cannot be hidden
// by a later increase. The combined comparison is derived from the bookends.
export function combinedDecision(before, middle, after, options) {
  const first = firstIntervalDecision(before, middle, options);
  if (!first.secondRequired) throw new Error('second interval not required');
  const second = compareReceipts(middle, after, options);
  const combined = compareReceipts(before, after, options);
  return { first: first.comparison, second, combined };
}

export function runCli(args) {
  try {
    if (args.some((value) => !value)) throw new Error();
    const read = (path) => JSON.parse(readFileSync(path, 'utf8'));
    if (![3, 4].includes(args.length)) throw new Error();
    const output = args.at(-1);
    const decision = args.length === 3
      ? firstIntervalDecision(read(args[0]), read(args[1]))
      : combinedDecision(read(args[0]), read(args[1]), read(args[2]));
    writeFileSync(output, `${JSON.stringify(decision)}\n`, { flag: 'wx', mode: 0o600 });
    if (args.length === 3) console.log(String(decision.secondRequired));
    return 0;
  } catch {
    console.error('voice measurement decision refused');
    return 1;
  }
}

if (isDirectInvocation(process.argv[1], import.meta.url)) {
  process.exitCode = runCli(process.argv.slice(2));
}
