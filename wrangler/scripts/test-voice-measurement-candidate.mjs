import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { buildReceipt } from './voice-rest-receipt.mjs';
import { combinedDecision, firstIntervalDecision } from './voice-measurement-decision.mjs';
import { verifyCandidate } from './voice-measurement-candidate.mjs';

const metadata = {
  runId: '123456', runAttempt: '1', sha: 'a'.repeat(40), deploymentId: 'dep-1',
  expiresAt: '2030-01-01T01:00:00Z',
};
const now = () => new Date('2030-01-01T00:10:00Z');
function receipt(count) {
  const samples = [{ name: 'two_bot_rest_requests_total', labels: { result: '2xx' }, value: count }];
  [1, 2, 3, 4].forEach((value, i) => samples.push({
    name: 'two_bot_process_incarnation', labels: { part: String(i) }, value,
  }));
  return buildReceipt({ samples, metadata, now });
}

test('candidate verification recomputes both paths and binds the original grant', () => {
  const before = receipt(5), middle = receipt(9), after = receipt(16);
  const one = firstIntervalDecision(before, after, { now });
  const two = combinedDecision(before, middle, after, { now });
  assert.equal(verifyCandidate([before, after], one, false, metadata, { now }), true);
  assert.equal(verifyCandidate([before, middle, after], two, true, metadata, { now }), true);
  assert.equal(verifyCandidate([before, middle], firstIntervalDecision(before, middle, { now }), false, metadata, { now }), false);
  assert.equal(verifyCandidate([before, middle, after], { ...two, combined: two.first }, true, metadata, { now }), false);
  for (const key of ['runId', 'runAttempt', 'sha', 'deploymentId', 'expiresAt']) {
    assert.equal(verifyCandidate([before, middle, after], two, true, { ...metadata, [key]: 'wrong' }, { now }), false);
    const incompleteGrant = { ...metadata };
    delete incompleteGrant[key];
    assert.equal(verifyCandidate([before, middle, after], two, true, incompleteGrant, { now }), false);
  }
  assert.equal(verifyCandidate([before, middle, after], two, true, null, { now }), false);
  assert.equal(verifyCandidate([before, middle, after], two, false, metadata, { now }), false);
  assert.equal(verifyCandidate([before, middle, after], two, true, metadata, { now: () => new Date('2030-01-01T01:00:00Z') }), false);
});

test('candidate CLI is silent on success and refuses missing or mismatched evidence', () => {
  const dir = mkdtempSync(join(tmpdir(), 'voice-candidate-'));
  try {
    const paths = ['before', 'middle', 'after', 'candidate', 'grant'].map((name) => join(dir, name));
    const [before, middle, after, candidate, grant] = paths;
    const capturedAt = new Date().toISOString();
    const receipts = [5, 9, 16].map((count) => ({ ...receipt(count), capturedAt }));
    receipts.forEach((value, i) => writeFileSync(paths[i], JSON.stringify(value)));
    writeFileSync(candidate, JSON.stringify(combinedDecision(...receipts)));
    writeFileSync(grant, JSON.stringify(metadata));
    const script = fileURLToPath(new URL('./voice-measurement-candidate.mjs', import.meta.url));
    const run = (...args) => spawnSync(process.execPath, [script, ...args], { encoding: 'utf8' });
    const verified = run('true', grant, candidate, before, middle, after);
    assert.equal(verified.status, 0);
    assert.equal(verified.stdout, '');
    assert.equal(verified.stderr, '');
    const missing = join(dir, 'missing');
    for (const args of [
      ['false', grant, candidate, before, middle, after],
      ['true', grant, before, before, middle, after],
      ['true', grant, candidate, before, middle],
      ['true', grant, missing, before, middle, after],
      ['true', missing, candidate, before, middle, after],
      ['true', grant, candidate, before, middle, missing],
    ]) {
      const refused = run(...args);
      assert.equal(refused.status, 1);
      assert.equal(refused.stdout, '');
      assert.equal(refused.stderr, 'voice measurement candidate refused\n');
    }
    writeFileSync(grant, JSON.stringify({ ...metadata, runId: '7' }));
    assert.equal(run('true', grant, candidate, before, middle, after).status, 1);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
