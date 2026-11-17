// Offline construction tests for sanitized voice REST receipts.
import assert from 'node:assert/strict';
import test from 'node:test';
import { buildReceipt, resultCountsFrom, validateMetadata } from './voice-rest-receipt.mjs';

const META = {
  runId: '123456', runAttempt: '1', sha: 'a'.repeat(40), deploymentId: 'dep-1',
  expiresAt: '2030-01-01T01:00:00Z',
};
const at = (iso) => () => new Date(iso);
const T0 = at('2030-01-01T00:00:00Z');
function samples({ counts = { '2xx': 10, '4xx': 2, '5xx': 1, transport: 3 }, inc = [1, 2, 3, 4] } = {}) {
  const rows = Object.entries(counts).map(([result, value]) => (
    { name: 'two_bot_rest_requests_total', labels: { route: 'x', result }, value }));
  inc.forEach((value, i) => rows.push({ name: 'two_bot_process_incarnation', labels: { part: String(i) }, value }));
  return rows;
}
const receipt = (over = {}, meta = META, now = T0) => buildReceipt({ samples: samples(over), metadata: meta, now });
const code = (fn) => { try { fn(); } catch (e) { return e.code; } return 'no throw'; };

test('receipt carries per-label cumulative counters and metadata', () => {
  const r = receipt();
  assert.equal(r.restTotal, 16);
  assert.equal(r.rest5xx, 1);
  assert.deepEqual(r.results, { '2xx': 10, '4xx': 2, '5xx': 1, transport: 3 });
  assert.equal(r.incarnation, '00000001000000020000000300000004');
  assert.equal(r.capturedAt, '2030-01-01T00:00:00.000Z');
  assert.equal(r.runId, '123456');
  assert.equal(r.sha, META.sha);
});

test('same label across routes sums; unlabeled kept', () => {
  const rows = samples();
  rows.push({ name: 'two_bot_rest_requests_total', labels: { result: '2xx' }, value: 5 });
  rows.push({ name: 'two_bot_rest_requests_total', labels: {}, value: 1 });
  const r = resultCountsFrom(rows);
  assert.equal(r['2xx'], 15);
  assert.equal(r.unlabeled, 1);
});

test('capture refusals: samples, identity, labels, metadata, expiry', () => {
  assert.equal(code(() => buildReceipt({ samples: [], metadata: META })), 'samples_missing');
  assert.equal(code(() => buildReceipt({ samples: samples().filter((s) => s.name !== 'two_bot_rest_requests_total'), metadata: META })), 'rest_samples_missing');
  assert.equal(code(() => receipt({ inc: [] })), 'identity_missing');
  assert.equal(code(() => receipt({ inc: [0, 0, 0, 0] })), 'identity_missing');
  assert.equal(code(() => receipt({ counts: { 'bad label!': 1 } })), 'result_label_invalid');
  assert.equal(code(() => receipt({ counts: { '2xx': -1 } })), 'invalid_rest_counters');
  assert.equal(code(() => receipt({}, { ...META, sha: 'zz' })), 'sha_invalid');
  assert.equal(code(() => receipt({}, { ...META, expiresAt: 'tomorrow' })), 'expiry_invalid');
  assert.equal(code(() => receipt({}, { ...META, expiresAt: '2030-02-30T01:00:00Z' })), 'expiry_invalid');
  assert.equal(code(() => receipt({}, { ...META, expiresAt: '2030-02-29T01:00:00Z' })), 'expiry_invalid');
  assert.equal(validateMetadata({ ...META, expiresAt: '2032-02-29T01:00:00.1Z' }).expiresAt, '2032-02-29T01:00:00.1Z');
  assert.equal(code(() => receipt({}, META, at('2030-01-01T01:00:00Z'))), 'grant_expired');
  assert.equal(code(() => validateMetadata(null)), 'metadata_invalid');
});
