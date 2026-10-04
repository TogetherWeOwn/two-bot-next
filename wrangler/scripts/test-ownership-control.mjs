// Offline takeover-retry contract: a takeover POST fired within a second of
// `wrangler deploy` can land on a still-propagating older Worker version and
// draw HTTP 503. deployment-takeover retries only that transient 5xx with a
// fresh state read per attempt. No network, no secrets, no timers.
import assert from 'node:assert/strict';
import test from 'node:test';
import { control } from './ownership-control.mjs';

const URL = 'https://two-bot-next-staging.5150.workers.dev/';
const TOKEN = 'x'.repeat(32);
const ACTOR = 'github-actions:1:abc';

function owner(epoch, phase = 'active') {
  return {
    deploymentId: 'new-deployment',
    epoch,
    phase,
    actor: ACTOR,
    timestamp: '2026-10-04T04:00:00.000Z',
    oldEpoch: epoch - 1,
    oldDeploymentId: epoch > 1 ? 'old-deployment' : null,
  };
}

// Scripted transport: each entry is consumed in order; entries are either
// { status, body } responses or Error instances thrown as transport failures.
function fixture(script) {
  const calls = [];
  const waits = [];
  let epoch = 80;
  const send = async (_endpoint, init) => {
    calls.push(init.method);
    const next = script[calls.length - 1];
    if (next instanceof Error) throw next;
    const { status, body } = next ?? {};
    if (status === undefined) throw new Error(`no scripted response for call ${calls.length}`);
    if (status === 200 && init.method === 'POST') epoch += 1;
    return {
      ok: status >= 200 && status < 300,
      status,
      json: async () => (typeof body === 'function' ? body(epoch) : body),
    };
  };
  const wait = async (ms) => { waits.push(ms); };
  const invoke = (params) => control(
    { url: URL, token: TOKEN, actor: ACTOR, ...params },
    send,
    wait,
  );
  return { calls, waits, invoke };
}

const getActive = (epoch) => ({
  status: 200,
  body: { deploymentId: 'new-deployment', owner: owner(epoch), running: false },
});
const postTakeover = (epoch) => ({
  status: 200,
  body: () => ({ deploymentId: 'new-deployment', owner: owner(epoch), running: false }),
});
const http = (status) => ({ status, body: { error: 'ownership_fenced', reason: 'x' } });

test('deployment-takeover succeeds first try without waiting', async () => {
  const f = fixture([getActive(80), postTakeover(81)]);
  const result = await f.invoke({ action: 'deployment-takeover' });
  assert.equal(result.owner.epoch, 81);
  assert.deepEqual(f.calls, ['GET', 'POST']);
  assert.deepEqual(f.waits, []);
});

test('deployment-takeover retries GET 503s then succeeds', async () => {
  const f = fixture([http(503), http(503), getActive(80), postTakeover(81)]);
  const result = await f.invoke({ action: 'deployment-takeover' });
  assert.equal(result.owner.epoch, 81);
  assert.deepEqual(f.calls, ['GET', 'GET', 'GET', 'POST']);
  assert.deepEqual(f.waits, [10000, 10000]);
});

test('deployment-takeover retries POST 503 with a fresh epoch read', async () => {
  const f = fixture([getActive(80), http(503), getActive(80), postTakeover(81)]);
  const result = await f.invoke({ action: 'deployment-takeover' });
  assert.equal(result.owner.epoch, 81);
  assert.deepEqual(f.calls, ['GET', 'POST', 'GET', 'POST']);
  assert.deepEqual(f.waits, [10000]);
});

test('deployment-takeover gives up after five attempts', async () => {
  const f = fixture([http(503), http(503), http(503), http(503), http(503), http(503)]);
  await assert.rejects(f.invoke({ action: 'deployment-takeover' }), /HTTP 503/);
  assert.deepEqual(f.calls, ['GET', 'GET', 'GET', 'GET', 'GET']);
  assert.deepEqual(f.waits, [10000, 10000, 10000, 10000]);
});

test('deployment-takeover never retries the deliberate fence refusal', async () => {
  const f = fixture([{
    status: 200,
    body: { deploymentId: 'new-deployment', owner: { ...owner(81), phase: 'fenced', deploymentId: null }, running: false },
  }]);
  await assert.rejects(
    f.invoke({ action: 'deployment-takeover' }),
    /explicit staging release required/,
  );
  assert.deepEqual(f.calls, ['GET']);
  assert.deepEqual(f.waits, []);
});

test('deployment-takeover never retries auth failures', async () => {
  const f = fixture([http(401), http(401)]);
  await assert.rejects(f.invoke({ action: 'deployment-takeover' }), /HTTP 401/);
  assert.deepEqual(f.calls, ['GET']);
  assert.deepEqual(f.waits, []);
});

test('explicit takeover never retries', async () => {
  const f = fixture([getActive(80), http(503), getActive(80)]);
  await assert.rejects(
    f.invoke({ action: 'takeover', expectedEpoch: 80 }),
    /HTTP 503/,
  );
  assert.deepEqual(f.calls, ['GET', 'POST']);
  assert.deepEqual(f.waits, []);
});
