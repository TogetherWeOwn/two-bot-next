// Successful-response contract for both ownership control clients: a 200 body
// is validated before it is trusted and projected to the supported fields
// before it is returned or printed. Fields the Worker never sends must not
// pass framing, and arbitrary extra fields must never reach logs or receipts.
// The documented legacy owner shape (no epoch) stays valid; a supplied epoch
// or phase must use the current vocabulary (wrangler/src/ownership.ts).
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import test from 'node:test';
import { promisify } from 'node:util';
import { control as stagingControl } from './ownership-control.mjs';
import { control as productionControl } from './production-ownership-control.mjs';

const runFile = promisify(execFile);

const STAGING = 'https://two-bot-next-staging.5150.workers.dev/';
const PRODUCTION = 'https://two-bot-next.5150.workers.dev/';
const TOKEN = 'x'.repeat(32);
const ACTOR = 'github-actions:1:abc';
const LEAK = `synthetic-sentinel-${'q'.repeat(32)}`;

// The full owner record the Worker persists and the staging status wrapper
// validates (wrangler/src/ownership.ts OwnerRecord). Every field here is a
// supported result field and must survive the projection verbatim.
function fullOwner(epoch) {
  return {
    deploymentId: 'new-deployment',
    epoch,
    phase: 'active',
    actor: ACTOR,
    timestamp: '2026-10-19T00:00:00.000Z',
    oldEpoch: epoch - 1,
    oldDeploymentId: 'old-deployment',
  };
}

function fixture(controlFn, base, script) {
  const calls = [];
  const waits = [];
  const send = async (_endpoint, init) => {
    calls.push(init.method);
    const entry = script[calls.length - 1] ?? {};
    const { status, body } = entry;
    if (status === undefined) throw new Error(`no scripted response for call ${calls.length}`);
    return new Response(typeof body === 'string' ? body : JSON.stringify(body), { status });
  };
  const wait = async (ms) => { waits.push(ms); };
  const invoke = (params) => controlFn({ token: TOKEN, actor: ACTOR, ...base, ...params }, send, wait);
  return { calls, waits, invoke };
}

const CLIENTS = [
  {
    name: 'staging client',
    make: (script) => fixture(stagingControl, { url: STAGING }, script),
    takeover: { action: 'deployment-takeover' },
  },
  {
    name: 'production client',
    make: (script) => fixture(productionControl, { url: PRODUCTION, productionUrl: PRODUCTION, stagingUrl: STAGING }, script),
    takeover: { action: 'takeover', expectedEpoch: 80 },
  },
];

const ok = (epoch) => ({ status: 200, body: { deploymentId: 'new-deployment', owner: fullOwner(epoch), running: false } });
const stateWith = (patch) => ({ status: 200, body: { deploymentId: 'new-deployment', owner: fullOwner(80), running: false, ...patch } });
const ownerWith = (patch) => stateWith({ owner: { ...fullOwner(80), ...patch } });

async function failure(promise) {
  try {
    await promise;
  } catch (error) {
    return error.message;
  }
  assert.fail('expected the control call to reject');
}

// Every entry is a 200 status body the clients must refuse with the fixed
// invalid-response text, without echoing any response content.
const INVALID_STATUS_BODIES = [
  ['owner is false', stateWith({ owner: false })],
  ['owner is a string', stateWith({ owner: 'new-deployment' })],
  ['owner is a number', stateWith({ owner: 42 })],
  ['owner is an array', stateWith({ owner: [fullOwner(80)] })],
  ['deploymentId is a number', stateWith({ deploymentId: 123 })],
  ['deploymentId is an object', stateWith({ deploymentId: { id: 'new-deployment' } })],
  ['deploymentId is empty', stateWith({ deploymentId: '' })],
  ['deploymentId is not a deployment id', stateWith({ deploymentId: 'not a deployment id!' })],
  ['supplied epoch is negative', ownerWith({ epoch: -1 })],
  ['supplied epoch is zero on an owner record', ownerWith({ epoch: 0, oldEpoch: -1 })],
  ['supplied epoch is fractional', ownerWith({ epoch: 80.5 })],
  ['supplied epoch is a string', ownerWith({ epoch: '80' })],
  ['supplied epoch is null', ownerWith({ epoch: null })],
  ['supplied epoch is beyond the safe integer range', ownerWith({ epoch: 2 ** 53 })],
  ['supplied phase is unknown', ownerWith({ phase: 'paused' })],
  ['supplied phase is empty', ownerWith({ phase: '' })],
  ['supplied phase is a number', ownerWith({ phase: 42 })],
  ['supplied phase is null', ownerWith({ phase: null })],
  ['owner deploymentId is a number', ownerWith({ deploymentId: 42 })],
  ['owner deploymentId is an object', ownerWith({ deploymentId: {} })],
  ['active owner has a null deploymentId', ownerWith({ deploymentId: null })],
  ['body is JSON null', { status: 200, body: 'null' }],
  ['body is a JSON number', { status: 200, body: '42' }],
  ['body is a JSON string', { status: 200, body: '"new-deployment"' }],
];

for (const client of CLIENTS) {
  for (const [label, entry] of INVALID_STATUS_BODIES) {
    test(`${client.name}: status refuses a 200 body where ${label}`, async () => {
      const f = client.make([entry]);
      const message = await failure(f.invoke({ action: 'status' }));
      assert.match(message, /^Invalid ownership control response/);
      assert.equal(message.includes(LEAK), false);
      assert.deepEqual(f.calls, ['GET']);
      assert.deepEqual(f.waits, []);
    });
  }

  test(`${client.name}: invalid GET state during takeover causes zero POSTs and no retry`, async () => {
    for (const entry of [stateWith({ owner: false }), stateWith({ owner: [] }), ownerWith({ epoch: '80' }), stateWith({ deploymentId: 123 })]) {
      const f = client.make([entry]);
      const message = await failure(f.invoke(client.takeover));
      assert.match(message, /^Invalid ownership control response/);
      assert.deepEqual(f.calls, ['GET']);
      assert.deepEqual(f.waits, []);
    }
  });

  test(`${client.name}: status returns only the supported fields, dropping sentinels`, async () => {
    const body = {
      deploymentId: 'new-deployment',
      owner: { ...fullOwner(80), debugOwnerField: LEAK },
      running: true,
      debugSentinel: LEAK,
      tokenEcho: TOKEN,
    };
    const f = client.make([{ status: 200, body }]);
    const result = await f.invoke({ action: 'status' });
    assert.deepEqual(result, { deploymentId: 'new-deployment', owner: fullOwner(80), running: true });
    assert.equal(JSON.stringify(result).includes(LEAK), false);
    assert.equal(JSON.stringify(result).includes(TOKEN), false);
  });

  test(`${client.name}: takeover result is projected, dropping POST sentinels`, async () => {
    const posted = {
      status: 200,
      body: {
        deploymentId: 'new-deployment',
        owner: { ...fullOwner(81), debugOwnerField: LEAK },
        running: false,
        debugSentinel: LEAK,
      },
    };
    const f = client.make([ok(80), posted]);
    const result = await f.invoke(client.takeover);
    assert.deepEqual(result, { deploymentId: 'new-deployment', owner: fullOwner(81), running: false });
    assert.equal(JSON.stringify(result).includes(LEAK), false);
    assert.deepEqual(f.calls, ['GET', 'POST']);
  });

  test(`${client.name}: a committed takeover confirmed on retry read is projected`, async () => {
    const refusal = { status: 503, body: { error: 'ownership_fenced', reason: 'deployment_mismatch' } };
    const committed = {
      status: 200,
      body: { deploymentId: 'new-deployment', owner: { ...fullOwner(81), debugOwnerField: LEAK }, running: false, debugSentinel: LEAK },
    };
    const f = client.make([ok(80), refusal, committed]);
    const result = await f.invoke(client.takeover);
    assert.deepEqual(result, { deploymentId: 'new-deployment', owner: fullOwner(81), running: false });
    assert.equal(JSON.stringify(result).includes(LEAK), false);
    assert.deepEqual(f.calls, ['GET', 'POST', 'GET']);
  });

  test(`${client.name}: malformed POST confirmation cannot claim transition success`, async () => {
    const confirmations = [
      { status: 200, body: { deploymentId: 'new-deployment', owner: false, running: false } },
      { status: 200, body: { deploymentId: 'new-deployment', owner: 'new-deployment', running: false } },
      { status: 200, body: { deploymentId: 'new-deployment', owner: { ...fullOwner(81), epoch: '81' }, running: false } },
      { status: 200, body: { deploymentId: 123, owner: fullOwner(81), running: false } },
      { status: 200, body: 'null' },
    ];
    for (const confirmation of confirmations) {
      const f = client.make([ok(80), confirmation]);
      const message = await failure(f.invoke(client.takeover));
      assert.match(message, /^(Invalid ownership control response|Ownership transition not confirmed)/);
      assert.equal(message.includes(LEAK), false);
      assert.deepEqual(f.calls, ['GET', 'POST']);
      assert.deepEqual(f.waits, []);
    }
  });

  // Valid controls: these pass today and must keep passing after validation.
  test(`${client.name}: status accepts a full owner record and a null owner`, async () => {
    const full = client.make([ok(80)]);
    assert.deepEqual(await full.invoke({ action: 'status' }),
      { deploymentId: 'new-deployment', owner: fullOwner(80), running: false });

    const pristine = client.make([{ status: 200, body: { deploymentId: 'new-deployment', owner: null, running: false } }]);
    const result = await pristine.invoke({ action: 'status' });
    assert.deepEqual(result, { deploymentId: 'new-deployment', owner: null, running: false });
  });

  test(`${client.name}: status accepts a fenced owner with a null deploymentId`, async () => {
    const fenced = { ...fullOwner(81), phase: 'fenced', deploymentId: null };
    const f = client.make([{ status: 200, body: { deploymentId: 'new-deployment', owner: fenced, running: false } }]);
    const result = await f.invoke({ action: 'status' });
    assert.deepEqual(result, { deploymentId: 'new-deployment', owner: fenced, running: false });
  });

  test(`${client.name}: status accepts the largest safe epoch`, async () => {
    const epoch = Number.MAX_SAFE_INTEGER;
    const f = client.make([{ status: 200, body: { deploymentId: 'new-deployment', owner: fullOwner(epoch), running: false } }]);
    const result = await f.invoke({ action: 'status' });
    assert.equal(result.owner.epoch, epoch);
  });

  test(`${client.name}: status still refuses a non-boolean running flag`, async () => {
    const f = client.make([stateWith({ running: 'false' })]);
    assert.match(await failure(f.invoke({ action: 'status' })), /^Invalid ownership control response/);
    assert.deepEqual(f.calls, ['GET']);
  });
}

test('staging client: legacy owner without an epoch still takes over at epoch zero', async () => {
  // Documented compatibility: an owner record with no epoch is not read as a
  // committed takeover; the client posts expectedEpoch 0. Validation must not
  // turn the missing epoch into a refusal.
  const legacy = { status: 200, body: { deploymentId: 'new-deployment', owner: { deploymentId: 'new-deployment', phase: 'active' }, running: false } };
  const f = fixture(stagingControl, { url: STAGING }, [legacy, ok(1)]);
  const result = await f.invoke({ action: 'deployment-takeover' });
  assert.deepEqual(result, { deploymentId: 'new-deployment', owner: fullOwner(1), running: false });
  assert.deepEqual(f.calls, ['GET', 'POST']);
});

test('staging client: status accepts the legacy owner without an epoch, projected as supplied', async () => {
  const legacyOwner = { deploymentId: 'new-deployment', phase: 'active' };
  const f = fixture(stagingControl, { url: STAGING }, [
    { status: 200, body: { deploymentId: 'new-deployment', owner: { ...legacyOwner, debugSentinel: LEAK }, running: false } },
  ]);
  const result = await f.invoke({ action: 'status' });
  assert.deepEqual(result, { deploymentId: 'new-deployment', owner: legacyOwner, running: false });
  assert.equal(JSON.stringify(result).includes(LEAK), false);
});

test('production client: takeover with an explicit epoch accepts a GET owner without an epoch', async () => {
  const legacy = { status: 200, body: { deploymentId: 'new-deployment', owner: { deploymentId: 'new-deployment', phase: 'active' }, running: false } };
  const f = fixture(productionControl, { url: PRODUCTION, productionUrl: PRODUCTION, stagingUrl: STAGING }, [legacy, ok(1)]);
  const result = await f.invoke({ action: 'takeover', expectedEpoch: 0 });
  assert.deepEqual(result, { deploymentId: 'new-deployment', owner: fullOwner(1), running: false });
  assert.deepEqual(f.calls, ['GET', 'POST']);
});

test('staging client: fence result is projected, dropping POST sentinels', async () => {
  const fenced = { ...fullOwner(81), phase: 'fenced', deploymentId: null };
  const posted = { status: 200, body: { deploymentId: 'new-deployment', owner: { ...fenced, debugOwnerField: LEAK }, running: false, debugSentinel: LEAK } };
  const f = fixture(stagingControl, { url: STAGING }, [ok(80), posted]);
  const result = await f.invoke({ action: 'fence', expectedEpoch: 80 });
  assert.deepEqual(result, { deploymentId: 'new-deployment', owner: fenced, running: false });
  assert.equal(JSON.stringify(result).includes(LEAK), false);
  assert.deepEqual(f.calls, ['GET', 'POST']);
});

test('both CLIs print only the fixed preflight receipt and make no request', async () => {
  const scripts = new URL('./', import.meta.url).pathname;
  const staging = await runFile('node', [`${scripts}ownership-control.mjs`, 'preflight'], {
    env: { ...process.env, STAGING_WORKER_URL: STAGING, OWNERSHIP_CONTROL_TOKEN: TOKEN },
  });
  assert.deepEqual(JSON.parse(staging.stdout), { configured: true });

  const production = await runFile('node', [`${scripts}production-ownership-control.mjs`, 'preflight'], {
    env: { ...process.env, PRODUCTION_WORKER_URL: PRODUCTION, STAGING_WORKER_URL: STAGING, OWNERSHIP_CONTROL_TOKEN: TOKEN },
  });
  assert.deepEqual(JSON.parse(production.stdout), { configured: true });
});
