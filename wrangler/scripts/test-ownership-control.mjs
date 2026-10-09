// Takeover retry contract: retry only 5xx; 4xx, auth, validation and the fenced phase are never retried.
import assert from 'node:assert/strict';
import { readdir, readFile } from 'node:fs/promises';
import test from 'node:test';
import { REFUSAL_REASONS, control } from './ownership-control.mjs';

const STAGING = 'https://two-bot-next-staging.5150.workers.dev/';
const TOKEN = 'x'.repeat(32);
const ACTOR = 'github-actions:1:abc';
const LEAK = `synthetic-leak-${'q'.repeat(40)}`;

function owner(epoch) {
  return { deploymentId: 'new-deployment', epoch, phase: 'active' };
}

// Each scripted entry is one response; a string body is sent verbatim, anything else as JSON.
function fixture(script) {
  const calls = [];
  const waits = [];
  const send = async (_endpoint, init) => {
    calls.push(init.method);
    const entry = script[calls.length - 1] ?? {};
    if (entry.throws) throw entry.throws;
    const { status, body } = entry;
    if (status === undefined) throw new Error(`no scripted response for call ${calls.length}`);
    return new Response(typeof body === 'string' ? body : JSON.stringify(body), { status });
  };
  const wait = async (ms) => { waits.push(ms); };
  const invoke = (params) => control({ url: STAGING, token: TOKEN, actor: ACTOR, ...params }, send, wait);
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

test('deployment-takeover succeeds first try without waiting', async () => {
  const f = fixture([ok(80), ok(81)]);
  const result = await f.invoke({ action: 'deployment-takeover' });
  assert.equal(result.owner.epoch, 81);
  assert.deepEqual(f.calls, ['GET', 'POST']);
  assert.deepEqual(f.waits, []);
});

test('deployment-takeover retries GET 503s then succeeds', async () => {
  const f = fixture([http(503), http(503), ok(80), ok(81)]);
  const result = await f.invoke({ action: 'deployment-takeover' });
  assert.equal(result.owner.epoch, 81);
  assert.deepEqual(f.calls, ['GET', 'GET', 'GET', 'POST']);
  assert.deepEqual(f.waits, [10000, 10000]);
});

test('deployment-takeover retries POST 503 with a fresh state read', async () => {
  const f = fixture([ok(80), http(503), ok(80), ok(81)]);
  const result = await f.invoke({ action: 'deployment-takeover' });
  assert.equal(result.owner.epoch, 81);
  assert.deepEqual(f.calls, ['GET', 'POST', 'GET', 'POST']);
  assert.deepEqual(f.waits, [10000]);
});

test('deployment-takeover retry after a committed takeover does not take over again', async () => {
  const f = fixture([ok(80), http(503), ok(81)]);
  const result = await f.invoke({ action: 'deployment-takeover' });
  assert.equal(result.owner.epoch, 81);
  assert.deepEqual(f.calls, ['GET', 'POST', 'GET']);
  assert.deepEqual(f.waits, [10000]);
});

test('deployment-takeover retry that finds a committed takeover running is not confirmed and not posted again', async () => {
  const f = fixture([ok(80), http(503), { status: 200, body: { deploymentId: 'new-deployment', owner: owner(81), running: true } }]);
  await assert.rejects(f.invoke({ action: 'deployment-takeover' }), /Ownership transition not confirmed/);
  assert.deepEqual(f.calls, ['GET', 'POST', 'GET']);
});

test('deployment-takeover refuses to re-post when the posted epoch is owned by another deployment', async () => {
  const other = { status: 200, body: { deploymentId: 'new-deployment', owner: { ...owner(81), deploymentId: 'other-deployment' }, running: false } };
  const f = fixture([ok(80), http(503), other]);
  await assert.rejects(f.invoke({ action: 'deployment-takeover' }), /posted epoch is owned by another deployment/);
  assert.deepEqual(f.calls, ['GET', 'POST', 'GET']);
  assert.deepEqual(f.waits, [10000]);
});

test('deployment-takeover does not post while retry reads flap between versions', async () => {
  const stale = { status: 200, body: { deploymentId: 'old-deployment', owner: owner(80), running: false } };
  const f = fixture([ok(80), http(503), stale, ok(80), ok(81)]);
  const result = await f.invoke({ action: 'deployment-takeover' });
  assert.equal(result.owner.epoch, 81);
  assert.deepEqual(f.calls, ['GET', 'POST', 'GET', 'GET', 'POST']);
  assert.deepEqual(f.waits, [10000, 10000]);
});

test('deployment-takeover keeps waiting across versions, then reports the version change', async () => {
  const stale = { status: 200, body: { deploymentId: 'old-deployment', owner: owner(80), running: false } };
  const f = fixture([ok(80), http(503), stale, stale]);
  const message = await failure(f.invoke({ action: 'deployment-takeover', takeoverAttempts: 3 }));
  assert.match(message, /answering deployment changed across takeover retries/);
  assert.match(message, /earlier takeover refusal HTTP 503 reason=deployment_mismatch attempts=3 /);
  assert.deepEqual(f.calls, ['GET', 'POST', 'GET', 'GET']);
  assert.deepEqual(f.waits, [10000, 10000]);
});

test('deployment-takeover accepts a commit answered by a different version than the first read', async () => {
  const first = { status: 200, body: { deploymentId: 'old-deployment', owner: { ...owner(80), deploymentId: 'old-deployment' }, running: false } };
  const f = fixture([first, ok(81)]);
  const result = await f.invoke({ action: 'deployment-takeover' });
  assert.equal(result.owner.epoch, 81);
  assert.deepEqual(f.calls, ['GET', 'POST']);
  assert.deepEqual(f.waits, []);
});

test('deployment-takeover retry after a fenced pending takeover posts again', async () => {
  const pending = { status: 200, body: { deploymentId: 'new-deployment', owner: { ...owner(81), phase: 'fenced' }, running: false } };
  const f = fixture([ok(80), http(503), pending, ok(82)]);
  const result = await f.invoke({ action: 'deployment-takeover', releaseFence: true });
  assert.equal(result.owner.epoch, 82);
  assert.deepEqual(f.calls, ['GET', 'POST', 'GET', 'POST']);
});

test('deployment-takeover does not read an owner without an epoch as a committed takeover', async () => {
  const f = fixture([{ status: 200, body: { deploymentId: 'new-deployment', owner: { deploymentId: 'new-deployment', phase: 'active' }, running: false } }, ok(1)]);
  const result = await f.invoke({ action: 'deployment-takeover' });
  assert.equal(result.owner.epoch, 1);
  assert.deepEqual(f.calls, ['GET', 'POST']);
});

test('deployment-takeover gives up after thirty-four attempts inside the bounded window', async () => {
  const f = fixture(Array.from({ length: 34 }, () => http(503)));
  assert.match(await failure(f.invoke({ action: 'deployment-takeover' })),
    /^Ownership control failed \(HTTP 503\) reason=deployment_mismatch attempts=34 elapsed=\d+s; stop, do not change credentials$/);
  assert.equal(f.calls.length, 34);
  assert.deepEqual(f.waits, Array(33).fill(10000));
});

test('deployment-takeover default bound covers the deferred code update window', async (t) => {
  let now = 0;
  t.mock.method(Date, 'now', () => now);
  const send = async () => {
    now += 15000;
    return new Response(JSON.stringify(http(503).body), { status: 503 });
  };
  const wait = async (ms) => { now += ms; };
  const error = await control({ url: STAGING, token: TOKEN, actor: ACTOR, action: 'deployment-takeover' }, send, wait).then(() => null, (e) => e);
  assert.match(error.message, /^Ownership control failed \(HTTP 503\) reason=deployment_mismatch attempts=\d+ elapsed=\d+s/);
  assert.ok(now >= 300000 && now <= 360000, `last refusal ended at ${now} ms`);
});

test('deployment-takeover succeeds on the last attempt inside the window', async () => {
  const f = fixture([...Array.from({ length: 33 }, () => http(503)), ok(80), ok(81)]);
  const result = await f.invoke({ action: 'deployment-takeover' });
  assert.equal(result.owner.epoch, 81);
  assert.equal(f.calls.length, 35);
  assert.deepEqual(f.waits, Array(33).fill(10000));
});

test('deployment-takeover never retries the deliberate fence refusal', async () => {
  const f = fixture([{
    status: 200,
    body: { deploymentId: 'new-deployment', owner: { ...owner(81), phase: 'fenced', deploymentId: null }, running: false },
  }]);
  await assert.rejects(f.invoke({ action: 'deployment-takeover' }), /explicit staging release required/);
  assert.deepEqual(f.calls, ['GET']);
  assert.deepEqual(f.waits, []);
});

test('deployment-takeover never retries auth failures', async () => {
  const f = fixture([http(401)]);
  await assert.rejects(f.invoke({ action: 'deployment-takeover' }), /HTTP 401/);
  assert.deepEqual(f.calls, ['GET']);
  assert.deepEqual(f.waits, []);
});

test('deployment-takeover never retries any non-5xx failure', async () => {
  for (const [status, reason] of [[400, 'not_owner'], [403, 'not_owner'], [404, 'not_owner'], [409, 'epoch_conflict'], [429, 'not_owner']]) {
    const f = fixture([http(status, reason)]);
    assert.match(await failure(f.invoke({ action: 'deployment-takeover' })),
      new RegExp(`HTTP ${status}\\) reason=${reason} attempts=1 `));
    assert.deepEqual(f.calls, ['GET']);
    assert.deepEqual(f.waits, []);
  }
});

test('explicit takeover never retries', async () => {
  const f = fixture([ok(80), http(503)]);
  await assert.rejects(f.invoke({ action: 'takeover', expectedEpoch: 80 }), /HTTP 503/);
  assert.deepEqual(f.calls, ['GET', 'POST']);
  assert.deepEqual(f.waits, []);
});

test('failure prints an allow-listed refusal reason with attempts and elapsed time', async () => {
  const message = await failure(fixture([http(503, 'deployment_mismatch')]).invoke({ action: 'status' }));
  assert.match(message, /^Ownership control failed \(HTTP 503\) reason=deployment_mismatch attempts=1 elapsed=\d+s; stop, do not change credentials$/);
});

test('unknown, oversized or non-JSON refusal bodies print unrecognized and never echo', async () => {
  const oversized = JSON.stringify({ reason: 'deployment_mismatch', padding: 'p'.repeat(2048) });
  for (const body of [{ reason: 'synthetic-unknown-reason' }, oversized, 'synthetic-private-body']) {
    const message = await failure(fixture([{ status: 503, body }]).invoke({ action: 'status' }));
    assert.match(message, /\(HTTP 503\) reason=unrecognized attempts=1 /);
    assert.doesNotMatch(message, /synthetic|padding/);
  }
});

test('token-looking refusal text never reaches the printed failure', async () => {
  const cases = [
    { body: { reason: TOKEN }, reason: 'unrecognized' },
    { body: { reason: `Bearer ${LEAK}` }, reason: 'unrecognized' },
    { body: { error: LEAK, reason: 'not_owner', detail: TOKEN }, reason: 'not_owner' },
  ];
  for (const { body, reason } of cases) {
    const message = await failure(fixture([{ status: 503, body }]).invoke({ action: 'status' }));
    assert.match(message, new RegExp(`reason=${reason} `));
    assert.equal(message.includes(LEAK) || message.includes(TOKEN), false);
  }
});

test('refusal body read stops near 1 KiB and cancels the rest of the stream', async () => {
  let pulled = 0;
  let cancelled = false;
  const endless = new ReadableStream({
    pull(controller) {
      pulled += 1;
      if (pulled > 50) return controller.close();
      controller.enqueue(new TextEncoder().encode('a'.repeat(512)));
    },
    cancel() { cancelled = true; },
  });
  const message = await failure(control(
    { url: STAGING, token: TOKEN, actor: ACTOR, action: 'status' },
    async () => new Response(endless, { status: 503 }),
  ));
  assert.match(message, /reason=unrecognized/);
  assert.ok(pulled <= 6, `read ${pulled} chunks`);
  assert.equal(cancelled, true);
});

test('allow-list matches the Worker refusal vocabulary', async () => {
  const srcDir = new URL('../src/', import.meta.url);
  const sources = await Promise.all((await readdir(srcDir)).filter((name) => name.endsWith('.ts'))
    .map((name) => readFile(new URL(name, srcDir), 'utf8')));
  assert.ok(sources.some((source) => source.includes(': "operation_failed";')), 'fallback reason missing');
  const thrown = sources.flatMap((source) => [...source.matchAll(/new OwnershipRefused\("([a-z_]+)"\)/g)].map((match) => match[1]));
  const constructed = sources.reduce((count, source) => count + (source.match(/new OwnershipRefused\(/g) ?? []).length, 0);
  assert.equal(constructed, thrown.length, 'every OwnershipRefused construction needs a literal reason');
  assert.deepEqual([...REFUSAL_REASONS].sort(), [...new Set([...thrown, 'operation_failed'])].sort());
});

const fenced = () => ({ status: 200, body: { deploymentId: 'new-deployment', owner: { ...owner(81), phase: 'fenced', deploymentId: null }, running: false } });

test('a fence left by a failed takeover keeps the earlier refusal reason', async () => {
  const f = fixture([ok(80), http(503, 'shutdown_unconfirmed'), fenced()]);
  assert.match(await failure(f.invoke({ action: 'deployment-takeover' })),
    /^Singleton is intentionally fenced or uninitialized; explicit staging release required; earlier takeover refusal HTTP 503 reason=shutdown_unconfirmed attempts=2 elapsed=\d+s$/);
  assert.deepEqual(f.calls, ['GET', 'POST', 'GET']);
  assert.deepEqual(f.waits, [10000]);
});

test('a network error after an earlier refusal keeps the reason and drops the raw error text', async () => {
  const f = fixture([http(503), { throws: new TypeError('synthetic-network-text') }]);
  const message = await failure(f.invoke({ action: 'deployment-takeover' }));
  assert.match(message, /^Ownership control failed; earlier takeover refusal HTTP 503 reason=deployment_mismatch attempts=2 elapsed=\d+s; stop, do not change credentials$/);
  assert.equal(message.includes('synthetic-network-text'), false);
});

test('takeover stops before a wait that would end past the window', async () => {
  const f = fixture([http(503)]);
  assert.match(await failure(f.invoke({ action: 'deployment-takeover', takeoverWindowMs: 0 })), /\(HTTP 503\) reason=deployment_mismatch attempts=1 /);
  assert.deepEqual(f.calls, ['GET']);
  assert.deepEqual(f.waits, []);
});
