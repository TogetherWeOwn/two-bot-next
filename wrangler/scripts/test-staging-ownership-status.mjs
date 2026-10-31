// Status-only ownership read: bounded GET, sanitized owner/timestamp output only.
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import { readOwnershipStatus, sanitizeStatus } from './staging-ownership-status.mjs';

const STAGING = 'https://two-bot-next-staging.5150.workers.dev/';
const TOKEN = 'x'.repeat(32);
const LEAK = `synthetic-leak-${'q'.repeat(40)}`;
const FIXED_NOW = new Date('2026-10-18T08:30:00.000Z');

function record(overrides = {}) {
  return {
    deploymentId: 'new-deployment',
    epoch: 81,
    phase: 'active',
    actor: 'github-actions:1:abc',
    timestamp: '2026-10-18T08:29:00.000Z',
    oldEpoch: 80,
    oldDeploymentId: 'old-deployment',
    ...overrides,
  };
}

function state(owner) {
  return { deploymentId: 'new-deployment', owner, running: false };
}

function sendOnce(script, seen) {
  return async (endpoint, init) => {
    seen.push({ url: String(endpoint), method: init.method, redirect: init.redirect, hasAuth: Boolean(init.headers?.authorization) });
    const entry = script[seen.length - 1] ?? {};
    if (entry.throws) throw entry.throws;
    if (entry.status === undefined) throw new Error(`no scripted response for call ${seen.length}`);
    return new Response(typeof entry.body === 'string' ? entry.body : JSON.stringify(entry.body), { status: entry.status });
  };
}

test('status emits only owner id and UTC observation timestamp', async () => {
  const seen = [];
  const receipt = await readOwnershipStatus(
    { url: STAGING, token: TOKEN },
    sendOnce([{ status: 200, body: state(record()) }], seen),
    () => FIXED_NOW,
  );
  assert.deepEqual(receipt, { owner_id: 'new-deployment', observed_at: '2026-10-18T08:30:00.000Z' });
  assert.deepEqual(Object.keys(receipt).sort(), ['observed_at', 'owner_id']);
  assert.equal(seen.length, 1);
  assert.equal(seen[0].method, 'GET');
  assert.equal(seen[0].redirect, 'error');
  assert.ok(seen[0].url.endsWith('/internal/ownership'));
  assert.equal(seen[0].hasAuth, true);
});

test('status performs exactly one GET and never POSTs', async () => {
  const seen = [];
  await readOwnershipStatus(
    { url: STAGING, token: TOKEN },
    sendOnce([{ status: 200, body: state(null) }], seen),
    () => FIXED_NOW,
  );
  assert.deepEqual(seen.map((call) => call.method), ['GET']);
});

test('fenced owner with null deployment emits null owner id', async () => {
  const seen = [];
  const receipt = await readOwnershipStatus(
    { url: STAGING, token: TOKEN },
    sendOnce([{ status: 200, body: state(record({ phase: 'fenced', deploymentId: null })) }], seen),
    () => FIXED_NOW,
  );
  assert.deepEqual(receipt, { owner_id: null, observed_at: '2026-10-18T08:30:00.000Z' });
  assert.equal(seen.length, 1);
});

test('fenced record keeping a deployment id still emits null owner id', async () => {
  // Crash-recovery keeps the pending deploymentId on the fenced record, but
  // require() refuses it, so nobody owns the singleton.
  const seen = [];
  const receipt = await readOwnershipStatus(
    { url: STAGING, token: TOKEN },
    sendOnce([{ status: 200, body: state(record({ phase: 'fenced', deploymentId: 'dep-new' })) }], seen),
    () => FIXED_NOW,
  );
  assert.deepEqual(receipt, { owner_id: null, observed_at: '2026-10-18T08:30:00.000Z' });
  assert.equal(seen.length, 1);
});

test('malformed JSON body fails closed with generic text', async () => {
  const seen = [];
  const message = await readOwnershipStatus(
    { url: STAGING, token: TOKEN },
    sendOnce([{ status: 200, body: '{"a" 1}' }], seen),
    () => FIXED_NOW,
  ).then(() => null, (error) => error.message);
  assert.equal(message, 'Ownership status check failed; stop and report without substituting credentials');
  assert.equal(seen.length, 1);
});

test('status fails closed on missing credentials without a request', async () => {
  const seen = [];
  await assert.rejects(
    readOwnershipStatus({ url: '', token: TOKEN }, sendOnce([], seen), () => FIXED_NOW),
    /not configured/,
  );
  await assert.rejects(
    readOwnershipStatus({ url: STAGING, token: '' }, sendOnce([], seen), () => FIXED_NOW),
    /not configured/,
  );
  await assert.rejects(
    readOwnershipStatus({ url: STAGING, token: 'short' }, sendOnce([], seen), () => FIXED_NOW),
    /missing or invalid/,
  );
  assert.equal(seen.length, 0);
});

test('status fails closed on non-success without echoing the body', async () => {
  for (const status of [401, 403, 404, 503]) {
    const seen = [];
    const message = await readOwnershipStatus(
      { url: STAGING, token: TOKEN },
      sendOnce([{ status, body: { error: 'ownership_fenced', reason: 'not_owner', detail: LEAK } }], seen),
      () => FIXED_NOW,
    ).then(() => null, (error) => error.message);
    assert.match(message, new RegExp(`HTTP ${status}`));
    assert.equal(message.includes(LEAK), false);
    assert.equal(seen.length, 1);
    assert.equal(seen[0].method, 'GET');
  }
});

test('status fails closed on redirects and ambiguous targets', async () => {
  const seen = [];
  await assert.rejects(
    readOwnershipStatus(
      { url: STAGING, token: TOKEN },
      sendOnce([{ throws: new TypeError('Redirect failed') }], seen),
      () => FIXED_NOW,
    ),
    /stop and report without substituting credentials/,
  );
  assert.equal(seen.length, 1);
  for (const url of [
    'http://two-bot-next-staging.5150.workers.dev/',
    'https://two-bot-next-staging.5150.workers.dev/internal/ownership',
    'https://two-bot-next-staging.5150.workers.dev/?x=1',
    'https://two-bot-next.5150.workers.dev/',
    'https://example.com/',
  ]) {
    const calls = [];
    await assert.rejects(
      readOwnershipStatus({ url, token: TOKEN }, sendOnce([], calls), () => FIXED_NOW),
      /approved two-bot-next-staging/,
    );
    assert.equal(calls.length, 0);
  }
});

test('status fails closed on invalid owner data', async () => {
  const badOwners = [
    { ...record(), deploymentId: 'not a deployment id!' },
    { ...record(), epoch: -1 },
    { ...record(), phase: 'parked' },
    { ...record(), phase: 'active', deploymentId: null },
    { ...record(), actor: 'bad actor!' },
    { ...record(), timestamp: 'not-a-date' },
    { ...record(), oldEpoch: 0 },
    'not-an-object',
    42,
  ];
  for (const owner of badOwners) {
    const seen = [];
    await assert.rejects(
      readOwnershipStatus({ url: STAGING, token: TOKEN }, sendOnce([{ status: 200, body: state(owner) }], seen), () => FIXED_NOW),
      /Invalid ownership status response/,
    );
    assert.equal(seen.length, 1);
  }
  assert.throws(() => sanitizeStatus({ deploymentId: 'new-deployment', owner: record(), running: 'yes' }, FIXED_NOW.toISOString()), /Invalid ownership status response/);
  assert.throws(() => sanitizeStatus(state(record()), 'not-a-date'), /Invalid observation timestamp/);
});

test('sanitized receipt never carries actor, epochs, deployment or running flags', async () => {
  const receipt = sanitizeStatus(state(record({ actor: LEAK })), FIXED_NOW.toISOString());
  const text = JSON.stringify(receipt);
  assert.equal(text.includes(LEAK), false);
  assert.equal('actor' in receipt, false);
  assert.equal('deploymentId' in receipt, false);
  assert.equal('running' in receipt, false);
  assert.equal('epoch' in receipt, false);
});

test('wrapper source never posts, never reads token from argv and never prints env', async () => {
  const source = await readFile(new URL('./staging-ownership-status.mjs', import.meta.url), 'utf8');
  assert.doesNotMatch(source, /method:\s*["']POST["']/);
  assert.doesNotMatch(source, /process\.argv\[[2-9]/);
  assert.doesNotMatch(source, /process\.env\[[^\]]*STAGING_WORKER_URL[^\]]*\]\s*\+/);
  assert.ok(source.includes("action: 'status'"));
  assert.ok(source.includes('STAGING_WORKER_URL'));
  assert.ok(source.includes('OWNERSHIP_CONTROL_TOKEN'));
  assert.doesNotMatch(source, /console\.log\(JSON\.stringify\((state|result)\)\)/);
  assert.ok(source.includes('JSON.stringify(receipt)'));
});
