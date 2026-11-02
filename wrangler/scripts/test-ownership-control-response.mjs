import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import test from 'node:test';
import { promisify } from 'node:util';
import { parseOwnershipResponse } from './ownership-control-response.mjs';

const runFile = promisify(execFile);
const owner = { deploymentId: 'new', epoch: 2, phase: 'active' };
const state = (patch = {}) => ({ deploymentId: 'new', owner, running: false, ...patch });
const LEAK = 'synthetic-response-extra-field';

for (const patch of [
  { oldEpoch: -1 }, { oldEpoch: 0 }, { oldEpoch: '1' }, { oldEpoch: null },
  { oldEpoch: 1.5 }, { oldEpoch: Number.MAX_SAFE_INTEGER + 1 },
  { oldDeploymentId: false }, { oldDeploymentId: '' }, { oldDeploymentId: {} },
  { actor: null }, { actor: [] }, { actor: 'not an actor!' },
  { timestamp: null }, { timestamp: 123 }, { timestamp: 'not a date' },
  { phase: undefined }, { deploymentId: '' },
]) {
  test(`rejects malformed supplied owner fields: ${JSON.stringify(patch)}`, () => {
    assert.equal(parseOwnershipResponse(state({ owner: { ...owner, ...patch } })), null);
  });
}

// Omit oldEpoch so its independent checks cannot mask an invalid epoch.
for (const epoch of [0, -1, 80.5, Number.MAX_SAFE_INTEGER + 1]) {
  test(`rejects supplied epoch ${epoch} on an abbreviated owner`, () => {
    assert.equal(parseOwnershipResponse(state({ owner: { ...owner, epoch } })), null);
  });
}

for (const epoch of [1, Number.MAX_SAFE_INTEGER]) {
  test(`preserves valid boundary epoch ${epoch} on an abbreviated owner`, () => {
    const response = state({ owner: { ...owner, epoch } });
    assert.deepEqual(parseOwnershipResponse(response), response);
  });
}

test('missing epoch is allowed only on an active legacy record', () => {
  assert.deepEqual(parseOwnershipResponse(state({ owner: { deploymentId: 'new', phase: 'active' } })),
    state({ owner: { deploymentId: 'new', phase: 'active' } }));
  assert.equal(parseOwnershipResponse(state({ owner: { deploymentId: null, phase: 'fenced' } })), null);
});

test('abbreviated records and supplied valid audit fields survive without synthesized fields', () => {
  assert.deepEqual(parseOwnershipResponse(state()), state());
  const audit = { ...owner, oldEpoch: 1, oldDeploymentId: null, actor: 'github-actions:1:abc', timestamp: '2026-10-19T00:00:00.000Z' };
  assert.deepEqual(parseOwnershipResponse(state({ owner: audit })), state({ owner: audit }));
  // A pending takeover is fenced while still naming its intended deployment.
  assert.deepEqual(parseOwnershipResponse(state({ owner: { ...audit, phase: 'fenced' } })),
    state({ owner: { ...audit, phase: 'fenced' } }));
});

for (const script of ['ownership-control.mjs', 'production-ownership-control.mjs']) {
  const scriptPath = new URL(script, import.meta.url).pathname;
  const env = {
    PATH: process.env.PATH,
    STAGING_WORKER_URL: 'https://two-bot-next-staging.5150.workers.dev/',
    PRODUCTION_WORKER_URL: 'https://two-bot-next.5150.workers.dev/',
    OWNERSHIP_CONTROL_TOKEN: 'x'.repeat(32),
    OWNERSHIP_ACTOR: 'github-actions:1:abc',
  };
  const invoke = (body, args = ['status']) => {
    // The preload is a fixed string: the probe body travels via environment,
    // never interpolated into constructed code.
    const preload = 'globalThis.fetch = async () => new Response(process.env.OWNERSHIP_PROBE_BODY, {status: 200});';
    // execFile timeout kills a hung child; the node:test timeout alone only
    // marks the test failed while open pipes keep the runner alive.
    return runFile(process.execPath, ['--import', `data:text/javascript,${encodeURIComponent(preload)}`, scriptPath, ...args], { env: { ...env, OWNERSHIP_PROBE_BODY: body }, timeout: 30_000 });
  };

  test(`${script}: real CLI status prints only allowlisted fields`, { timeout: 30_000 }, async () => {
    const result = await invoke(JSON.stringify(state({ owner: { ...owner, debug: LEAK }, debug: LEAK })));
    assert.deepEqual(JSON.parse(result.stdout), state());
    assert.equal(result.stderr, '');
    assert.equal(result.stdout.includes(LEAK), false);
  });

  test(`${script}: real CLI rejects invalid envelopes without output or remote details`, { timeout: 30_000 }, async () => {
    await assert.rejects(invoke(JSON.stringify(state({ owner: [LEAK] }))), (error) => {
      assert.equal(error.code, 1);
      assert.equal(error.stdout, '');
      assert.equal(error.stderr, 'Invalid ownership control response\n');
      return true;
    });
  });

  test(`${script}: malformed JSON never prints parser details`, { timeout: 30_000 }, async () => {
    // Production `status` retries an unparseable answer through its ~330 s
    // propagation budget by design, which would stall CI here. Probe the same
    // real-CLI print path via `takeover` instead: its 5xx-only retry refuses a
    // failed GET at once, so the fixed-vocabulary refusal is asserted fast.
    const args = script === 'production-ownership-control.mjs' ? ['takeover'] : ['status'];
    await assert.rejects(invoke(`{"debug":"${LEAK}"`, args), (error) => {
      assert.equal(error.code, 1);
      assert.equal(error.stdout, '');
      assert.equal(error.stderr.includes(LEAK), false);
      assert.doesNotMatch(error.stderr, /SyntaxError|JSON|position/);
      return true;
    });
  });
}
