// Offline decision tests; the caller remains disabled.
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { buildReceipt } from './voice-rest-receipt.mjs';
import { combinedDecision, firstIntervalDecision } from './voice-measurement-decision.mjs';

const metadata = {
  runId: '123456', runAttempt: '1', sha: 'a'.repeat(40), deploymentId: 'dep-1',
  expiresAt: '2030-01-01T01:00:00Z',
};
const now = () => new Date('2030-01-01T00:10:00Z');
function receipt(counts, overrides = {}, incarnation = [1, 2, 3, 4]) {
  const samples = Object.entries(counts).map(([result, value]) =>
    ({ name: 'two_bot_rest_requests_total', labels: { result }, value }));
  incarnation.forEach((value, i) => samples.push({
    name: 'two_bot_process_incarnation', labels: { part: String(i) }, value,
  }));
  return buildReceipt({ samples, metadata: { ...metadata, ...overrides }, now });
}

test('second invocation is required only below ten across all result labels', () => {
  const before = receipt({ '2xx': 5, '4xx': 3, '5xx': 1 });
  for (const [increase, expected] of [[0, true], [9, true], [10, false], [11, false]]) {
    const after = receipt({ '2xx': 5, '4xx': 3 + increase, '5xx': 1 });
    const decision = firstIntervalDecision(before, after, { now });
    assert.equal(decision.secondRequired, expected);
    assert.equal(decision.comparison.restTotalDelta, increase);
  }
});

test('combined evidence validates both intervals and refuses hidden resets', () => {
  const before = receipt({ '2xx': 5, '5xx': 1 });
  const middle = receipt({ '2xx': 9, '5xx': 2 });
  const after = receipt({ '2xx': 17, '5xx': 3 });
  const result = combinedDecision(before, middle, after, { now });
  assert.equal(result.first.restTotalDelta, 5);
  assert.equal(result.second.restTotalDelta, 9);
  assert.equal(result.combined.restTotalDelta, 14);
  assert.equal(result.combined.rest5xxDelta, 2);
  assert.throws(() => combinedDecision(before, middle, receipt({ '2xx': 8, '5xx': 3 }), { now }));
  assert.throws(() => combinedDecision(before, receipt({ '2xx': 15, '5xx': 1 }), after, { now }));
  assert.throws(() => combinedDecision(before, middle, receipt({ '2xx': 17, '5xx': 3 }, {}, [5, 2, 3, 4]), { now }));
});

test('CLI emits only a decision and refuses invalid input or an existing output', () => {
  const dir = mkdtempSync(join(tmpdir(), 'voice-decision-'));
  try {
    const before = join(dir, 'before.json');
    const after = join(dir, 'after.json');
    const output = join(dir, 'decision.json');
    const capturedAt = new Date().toISOString();
    writeFileSync(before, JSON.stringify({ ...receipt({ '2xx': 5 }), capturedAt }));
    writeFileSync(after, JSON.stringify({ ...receipt({ '2xx': 14 }), capturedAt }));
    const script = fileURLToPath(new URL('./voice-measurement-decision.mjs', import.meta.url));
    const run = (...args) => spawnSync(process.execPath, [script, ...args], { encoding: 'utf8' });
    const success = run(before, after, output);
    assert.equal(success.status, 0);
    assert.equal(success.stdout, 'true\n');
    assert.equal(success.stderr, '');
    assert.equal(JSON.parse(readFileSync(output, 'utf8')).comparison.restTotalDelta, 9);
    const last = join(dir, 'last.json');
    const combined = join(dir, 'combined.json');
    writeFileSync(last, JSON.stringify({ ...receipt({ '2xx': 19 }), capturedAt }));
    const final = run(before, after, last, combined);
    assert.equal(final.status, 0);
    assert.equal(final.stdout, '');
    assert.equal(final.stderr, '');
    assert.equal(JSON.parse(readFileSync(combined, 'utf8')).combined.restTotalDelta, 14);
    const repeated = run(before, after, last, combined);
    assert.equal(repeated.status, 1);
    assert.equal(repeated.stderr, 'voice measurement decision refused\n');
    writeFileSync(last, JSON.stringify({ ...receipt({ '2xx': 13 }), capturedAt }));
    const resetOutput = join(dir, 'reset.json');
    const reset = run(before, after, last, resetOutput);
    assert.equal(reset.status, 1);
    assert.equal(reset.stdout, '');
    assert.equal(reset.stderr, 'voice measurement decision refused\n');
    assert.throws(() => readFileSync(resetOutput));
    const existing = run(before, after, output);
    assert.equal(existing.status, 1);
    assert.equal(existing.stdout, '');
    assert.equal(existing.stderr, 'voice measurement decision refused\n');
    assert.equal(JSON.parse(readFileSync(output, 'utf8')).comparison.restTotalDelta, 9);
    writeFileSync(after, JSON.stringify({ ...receipt({ '2xx': 15 }), capturedAt }));
    writeFileSync(last, JSON.stringify({ ...receipt({ '2xx': 19 }), capturedAt }));
    const unnecessaryOutput = join(dir, 'unnecessary.json');
    const unnecessary = run(before, after, last, unnecessaryOutput);
    assert.equal(unnecessary.status, 1);
    assert.equal(unnecessary.stdout, '');
    assert.equal(unnecessary.stderr, 'voice measurement decision refused\n');
    assert.throws(() => readFileSync(unnecessaryOutput));
    writeFileSync(after, 'untrusted input');
    const invalidOutput = join(dir, 'invalid.json');
    const invalid = run(before, after, invalidOutput);
    assert.equal(invalid.status, 1);
    assert.equal(invalid.stdout, '');
    assert.equal(invalid.stderr, 'voice measurement decision refused\n');
    assert.throws(() => readFileSync(invalidOutput));
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('invalid identity, counters, and expired grants refuse a decision', () => {
  const before = receipt({ '2xx': 5 });
  const changedGrant = receipt({ '2xx': 15 }, { deploymentId: 'other' });
  const changedProcess = receipt({ '2xx': 15 }, {}, [5, 2, 3, 4]);
  const decreased = receipt({ '2xx': 4 });
  for (const after of [changedGrant, changedProcess, decreased, null]) {
    assert.throws(() => firstIntervalDecision(before, after, { now }));
  }
  assert.throws(() => firstIntervalDecision(before, before, {
    now: () => new Date('2030-01-01T01:00:00Z'),
  }));
});
