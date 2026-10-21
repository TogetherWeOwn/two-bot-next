// Production status contract: the P2 read waits out version propagation AND a
// transient request failure (timeout, transport, unparseable answer) inside
// one bounded budget. Anything else stops at once. Every failure prints
// fixed vocabulary with attempts and elapsed time, never provider text.
import assert from 'node:assert/strict';
import test from 'node:test';
import { REFUSAL_REASONS, control } from './production-ownership-control.mjs';

const PRODUCTION = 'https://two-bot-next-production.5150.workers.dev/';
const STAGING = 'https://two-bot-next-staging.5150.workers.dev/';
const TOKEN = 'x'.repeat(32);
const ACTOR = 'github-actions:1:abc';
const LEAK = `synthetic-leak-${'q'.repeat(40)}`;

function owner(epoch, deploymentId = 'new-deployment') {
  return { deploymentId, epoch, phase: 'active' };
}

// Each scripted entry is one response; a string body is sent verbatim, anything else as JSON.
function fixture(script, hooks = {}) {
  const calls = [];
  const waits = [];
  const send = async (_endpoint, init) => {
    calls.push(init.method);
    hooks.onSend?.();
    const entry = script[calls.length - 1] ?? {};
    if (entry.throws) throw entry.throws;
    const { status, body } = entry;
    if (status === undefined) throw new Error(`no scripted response for call ${calls.length}`);
    return new Response(typeof body === 'string' ? body : JSON.stringify(body), { status });
  };
  const wait = async (ms) => { waits.push(ms); };
  const invoke = (params, now) => control({
    url: PRODUCTION, productionUrl: PRODUCTION, stagingUrl: STAGING,
    token: TOKEN, actor: ACTOR, ...params,
  }, send, wait, now);
  return { calls, waits, invoke };
}

async function failure(promise) {
  try {
    await promise;
  } catch (error) {
    return error.message;
  }
  assert.fail('expected the control call to reject');
}

const ok = (epoch) => ({ status: 200, body: { deploymentId: 'new-deployment', owner: owner(epoch), running: false } });
const http = (status, reason = 'deployment_mismatch') => ({ status, body: { error: 'ownership_fenced', reason } });
// AbortSignal.timeout rejects with TimeoutError (what fetch throws on a real
// 15 s timeout); a manual AbortController would reject with AbortError.
const timeout = () => ({ throws: new DOMException('The operation timed out', 'TimeoutError') });
const abort = () => ({ throws: new DOMException('The operation was aborted', 'AbortError') });
const transport = (text = 'fetch failed') => ({ throws: new TypeError(text) });

test('status succeeds first try without waiting', async () => {
  const f = fixture([ok(80)]);
  const result = await f.invoke({ action: 'status', expectedDeploymentId: 'new-deployment' });
  assert.equal(result.deploymentId, 'new-deployment');
  assert.deepEqual(f.calls, ['GET']);
  assert.deepEqual(f.waits, []);
});

test('status waits out 503 deployment_mismatch then succeeds', async () => {
  const f = fixture([http(503), http(503), ok(80)]);
  const result = await f.invoke({ action: 'status', expectedDeploymentId: 'new-deployment' });
  assert.equal(result.deploymentId, 'new-deployment');
  assert.deepEqual(f.calls, ['GET', 'GET', 'GET']);
  assert.deepEqual(f.waits, [10000, 10000]);
});

test('status waits out a 200 naming the old deployment then succeeds', async () => {
  const old = { status: 200, body: { deploymentId: 'old-deployment', owner: owner(80, 'old-deployment'), running: false } };
  const f = fixture([old, ok(80)]);
  const result = await f.invoke({ action: 'status', expectedDeploymentId: 'new-deployment' });
  assert.equal(result.deploymentId, 'new-deployment');
  assert.deepEqual(f.calls, ['GET', 'GET']);
  assert.deepEqual(f.waits, [10000]);
});

test('status retries a request timeout then succeeds', async () => {
  const f = fixture([http(503), timeout(), ok(80)]);
  const result = await f.invoke({ action: 'status', expectedDeploymentId: 'new-deployment' });
  assert.equal(result.deploymentId, 'new-deployment');
  assert.deepEqual(f.calls, ['GET', 'GET', 'GET']);
  assert.deepEqual(f.waits, [10000, 10000]);
});

test('status retries a transport failure then succeeds', async () => {
  const f = fixture([transport(), ok(80)]);
  const result = await f.invoke({ action: 'status', expectedDeploymentId: 'new-deployment' });
  assert.equal(result.deploymentId, 'new-deployment');
  assert.deepEqual(f.calls, ['GET', 'GET']);
  assert.deepEqual(f.waits, [10000]);
});

test('status retries an unparseable 200 answer then succeeds', async () => {
  const f = fixture([{ status: 200, body: '{"deploymentId": "new-deplo' }, ok(80)]);
  const result = await f.invoke({ action: 'status', expectedDeploymentId: 'new-deployment' });
  assert.equal(result.deploymentId, 'new-deployment');
  assert.deepEqual(f.calls, ['GET', 'GET']);
  assert.deepEqual(f.waits, [10000]);
});

test('status retries a null-shape 200 answer then succeeds', async () => {
  const f = fixture([{ status: 200, body: 'null' }, ok(80)]);
  const result = await f.invoke({ action: 'status', expectedDeploymentId: 'new-deployment' });
  assert.equal(result.deploymentId, 'new-deployment');
  assert.deepEqual(f.calls, ['GET', 'GET']);
  assert.deepEqual(f.waits, [10000]);
});

test('status reports a persistent timeout with attempts and elapsed time', async () => {
  const f = fixture([timeout(), timeout(), timeout()]);
  const message = await failure(f.invoke({ action: 'status', expectedDeploymentId: 'new-deployment', statusAttempts: 3 }));
  assert.match(message, /^Ownership control request timeout attempts=3 elapsed=\d+s; stop, do not change credentials$/);
  assert.deepEqual(f.calls, ['GET', 'GET', 'GET']);
  assert.deepEqual(f.waits, [10000, 10000]);
});

test('status classifies a manual abort as a timeout', async () => {
  const f = fixture([abort()]);
  const message = await failure(f.invoke({ action: 'status', statusAttempts: 1 }));
  assert.match(message, /^Ownership control request timeout attempts=1 elapsed=\d+s; stop, do not change credentials$/);
  assert.deepEqual(f.calls, ['GET']);
  assert.deepEqual(f.waits, []);
});

test('status stops retrying once the next wait would exceed the status window', async () => {
  // The rollback path reads status through a subprocess capped at 420 s; a
  // hanging endpoint must not outlive the caller. Wall time, not attempts,
  // ends this loop: each attempt consumes over a minute here.
  let now = 0;
  const script = [transport(), transport(), transport(), transport(),
    transport(), transport(), transport(), transport()];
  const f = fixture(script, { onSend: () => { now += 65000; } });
  const message = await failure(f.invoke({ action: 'status', statusAttempts: 34 }, () => now));
  assert.match(message, /^Ownership control request failed attempts=5 elapsed=325s; stop, do not change credentials$/);
  assert.deepEqual(f.calls, ['GET', 'GET', 'GET', 'GET', 'GET']);
  assert.deepEqual(f.waits, [10000, 10000, 10000, 10000]);
});

test('status reports a persistent transport failure with attempts and elapsed time', async () => {
  const f = fixture([transport(), transport(), transport()]);
  const message = await failure(f.invoke({ action: 'status', expectedDeploymentId: 'new-deployment', statusAttempts: 3 }));
  assert.match(message, /^Ownership control request failed attempts=3 elapsed=\d+s; stop, do not change credentials$/);
  assert.deepEqual(f.calls, ['GET', 'GET', 'GET']);
  assert.deepEqual(f.waits, [10000, 10000]);
});

test('status reports a persistently invalid answer with attempts and elapsed time', async () => {
  const garbage = { status: 200, body: 'not json at all' };
  const f = fixture([garbage, garbage]);
  const message = await failure(f.invoke({ action: 'status', expectedDeploymentId: 'new-deployment', statusAttempts: 2 }));
  assert.match(message, /^Ownership control invalid response attempts=2 elapsed=\d+s; stop, do not change credentials$/);
  assert.deepEqual(f.calls, ['GET', 'GET']);
  assert.deepEqual(f.waits, [10000]);
});

test('transient failure text never reaches the printed message', async () => {
  const throwing = new TypeError(`Bearer ${LEAK} transport blew up`);
  const f = fixture([{ throws: throwing }]);
  const message = await failure(f.invoke({ action: 'status', statusAttempts: 1 }));
  assert.match(message, /^Ownership control request failed attempts=1 /);
  assert.equal(message.includes(LEAK) || message.includes(TOKEN), false);
});

test('unparseable answer bytes never reach the printed message', async () => {
  const f = fixture([{ status: 200, body: `{"token": "${TOKEN}", "leak": "${LEAK}` }]);
  const message = await failure(f.invoke({ action: 'status', statusAttempts: 1 }));
  assert.match(message, /^Ownership control invalid response attempts=1 /);
  assert.equal(message.includes(LEAK) || message.includes(TOKEN), false);
});

test('status never retries auth failures', async () => {
  const f = fixture([http(401)]);
  await assert.rejects(f.invoke({ action: 'status', expectedDeploymentId: 'new-deployment' }), /HTTP 401/);
  assert.deepEqual(f.calls, ['GET']);
  assert.deepEqual(f.waits, []);
});

test('status never retries a non-propagation 5xx', async () => {
  const f = fixture([http(503, 'not_owner')]);
  const message = await failure(f.invoke({ action: 'status', expectedDeploymentId: 'new-deployment' }));
  assert.match(message, /\(HTTP 503\) reason=not_owner attempts=1 /);
  assert.deepEqual(f.calls, ['GET']);
  assert.deepEqual(f.waits, []);
});

test('status without a pinned deployment still survives a transient failure', async () => {
  // The rollback path reads without an expected deployment: propagation has
  // no meaning there, but a single blip must not fail the restore either.
  const f = fixture([transport(), ok(80)]);
  const result = await f.invoke({ action: 'status' });
  assert.equal(result.deploymentId, 'new-deployment');
  assert.deepEqual(f.calls, ['GET', 'GET']);
  assert.deepEqual(f.waits, [10000]);
});

test('takeover reports a transport failure instead of the generic catch-all', async () => {
  // Takeover keeps its 5xx-only retry policy (a POST may have committed);
  // classification only makes the fatal cause printable for triage.
  const f = fixture([ok(80), transport()]);
  const message = await failure(f.invoke({ action: 'takeover', expectedEpoch: 80, expectedDeploymentId: 'new-deployment' }));
  assert.match(message, /^Ownership control request failed attempts=1 elapsed=\d+s; stop, do not change credentials$/);
  assert.deepEqual(f.calls, ['GET', 'POST']);
  assert.deepEqual(f.waits, []);
});

test('allow-list matches the Worker refusal vocabulary', async () => {
  // Same pin as the staging client: every OwnershipRefused literal plus the
  // operation_failed fallback, nothing else.
  const { readdir, readFile } = await import('node:fs/promises');
  const srcDir = new URL('../src/', import.meta.url);
  const sources = await Promise.all((await readdir(srcDir)).filter((name) => name.endsWith('.ts'))
    .map((name) => readFile(new URL(name, srcDir), 'utf8')));
  assert.ok(sources.some((source) => source.includes(': "operation_failed";')), 'fallback reason missing');
  const thrown = sources.flatMap((source) => [...source.matchAll(/new OwnershipRefused\("([a-z_]+)"\)/g)].map((match) => match[1]));
  const constructed = sources.reduce((count, source) => count + (source.match(/new OwnershipRefused\(/g) ?? []).length, 0);
  assert.equal(constructed, thrown.length, 'every OwnershipRefused construction needs a literal reason');
  assert.deepEqual([...REFUSAL_REASONS].sort(), [...new Set([...thrown, 'operation_failed'])].sort());
});
