// Offline construction tests for sanitized voice REST receipts.
import assert from 'node:assert/strict';
import test from 'node:test';
import { buildReceipt, compareReceipts, resultCountsFrom, validateMetadata } from './voice-rest-receipt.mjs';

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

test('compare reports per-result, total and 5xx deltas', () => {
  const later = at('2030-01-01T00:10:00Z');
  const after = receipt({ counts: { '2xx': 15, '4xx': 2, '5xx': 4, transport: 3, '429': 2 } }, META, later);
  const out = compareReceipts(receipt(), after, { now: later });
  assert.deepEqual(out.resultDeltas, { '2xx': 5, '429': 2, '4xx': 0, '5xx': 3, transport: 0 });
  assert.equal(out.restTotalDelta, 10);
  assert.equal(out.rest5xxDelta, 3);
});

test('compare refuses changed identity, grant, counters and time', () => {
  const later = at('2030-01-01T00:10:00Z');
  const before = receipt();
  const after = (over = {}, meta = META) => receipt(over, meta, later);
  for (const change of [
    { runId: '123457' }, { runAttempt: '2' }, { sha: 'b'.repeat(40) },
    { deploymentId: 'dep-2' }, { expiresAt: '2030-01-01T02:00:00Z' },
  ]) assert.equal(code(() => compareReceipts(before, after({}, { ...META, ...change }), { now: later })), 'metadata_changed');
  assert.equal(code(() => compareReceipts(before, after({ inc: [9, 2, 3, 4] }), { now: later })), 'incarnation_changed');
  assert.equal(code(() => compareReceipts(before, after(), { now: at('2030-01-01T01:00:00Z') })), 'grant_expired');
  assert.equal(code(() => compareReceipts(before, after({ counts: { '2xx': 9, '4xx': 2, '5xx': 1, transport: 3 } }), { now: later })), 'counter_decreased');
  assert.equal(code(() => compareReceipts(before, after({ counts: { '2xx': 20, '4xx': 2, '5xx': 1 } }), { now: later })), 'counter_decreased');
  assert.equal(code(() => compareReceipts(after(), before, { now: later })), 'time_regressed');
  assert.equal(code(() => compareReceipts(before, after(), { now: T0 })), 'time_regressed');
});

test('compare treats prototype-named results as own counters', () => {
  const later = at('2030-01-01T00:10:00Z');
  for (const label of ['constructor', '__proto__']) {
    const counts = JSON.parse(`{"2xx":10,"${label}":2}`);
    const before = JSON.parse(JSON.stringify(receipt({ counts })));
    const after = JSON.parse(JSON.stringify(receipt({ counts: { '2xx': 15 } }, META, later)));
    assert.equal(code(() => compareReceipts(before, after, { now: later })), 'counter_decreased');
    const added = compareReceipts(receipt({ counts: { '2xx': 10 } }),
      receipt({ counts }, META, later), { now: later });
    assert.equal(added.resultDeltas[label], 2);
  }
});

test('compare refuses malformed or unavailable persisted receipts', () => {
  const before = receipt();
  const compare = (value) => code(() => compareReceipts(value, before, { now: T0 }));
  assert.equal(compare({ ...before, available: false }), 'receipt_unavailable');
  assert.equal(compare({ ...before, extra: 1 }), 'receipt_invalid');
  assert.equal(compare({ ...before, restTotal: 99 }), 'counter_sum_mismatch');
  assert.equal(compare({ ...before, incarnation: '0'.repeat(32) }), 'identity_missing');
  assert.equal(compare({ ...before, capturedAt: '2030-02-30T00:00:00Z' }), 'receipt_invalid');
  assert.equal(compare({ ...before, results: { '2xx': Number.MAX_SAFE_INTEGER, '5xx': 1 } }), 'receipt_invalid');
  assert.equal(compare({ ...before, capturedAt: META.expiresAt }), 'grant_expired');
});
