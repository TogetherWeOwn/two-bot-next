import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { runCli } from './voice-rest-measurement.mjs';
import { metricsLines } from '../test/fixtures/metrics.ts';

const metadata = { runId: '12', runAttempt: '1', sha: 'a'.repeat(40), deploymentId: 'dep', expiresAt: '2099-01-01T00:00:00Z' };
const env = { STAGING_URL: 'https://staging.example', PRODUCTION_URL: 'https://production.example', METRICS_SCRAPE_TOKEN: 't'.repeat(32) };
const body = `${metricsLines().join('\n')}\n`;

test('captures and compares sanitized receipts without replacing output', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'voice-measure-'));
  try {
    const meta = join(dir, 'meta'); const a = join(dir, 'a'); const b = join(dir, 'b'); const out = join(dir, 'out');
    writeFileSync(meta, JSON.stringify(metadata));
    const calls = [];
    const fetchImpl = async (url, options) => { calls.push({ url, options }); return new Response(body); };
    const capture = (path) => runCli(['capture', '--metadata', meta, '--metrics-url', env.STAGING_URL, '--out', path], env, { fetchImpl });
    assert.equal(await capture(a), 0);
    assert.equal(await capture(b), 0);
    assert.equal(calls[0].url, 'https://staging.example/ops/metrics');
    assert.equal(calls[0].options.redirect, 'error');
    assert.equal(await runCli(['compare', '--before', a, '--after', b, '--out', out], env), 0);
    assert.equal(JSON.parse(readFileSync(out)).restTotalDelta, 0);
    assert.equal(readFileSync(a, 'utf8').includes(env.METRICS_SCRAPE_TOKEN), false);
    assert.equal(await capture(a), 1);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('refuses invalid input, production, missing token, auth and malformed metrics', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'voice-refuse-'));
  try {
    const meta = join(dir, 'meta'); const out = join(dir, 'out');
    writeFileSync(meta, JSON.stringify(metadata));
    let calls = 0;
    const attempt = (url, environment, response) => runCli(['capture', '--metadata', meta, '--metrics-url', url, '--out', out], environment, { fetchImpl: async () => { calls++; return response; }, retrySleepSeconds: 0 });
    assert.equal(await attempt(env.PRODUCTION_URL, env, new Response(body)), 1);
    assert.equal(await attempt(env.STAGING_URL, { ...env, METRICS_SCRAPE_TOKEN: '' }, new Response(body)), 1);
    assert.equal(calls, 0);
    assert.equal(await attempt(env.STAGING_URL, env, new Response('secret response', { status: 401 })), 1);
    assert.equal(await attempt(env.STAGING_URL, env, new Response('not metrics')), 1);
    writeFileSync(meta, JSON.stringify({ ...metadata, expiresAt: '2000-01-01T00:00:00Z' }));
    assert.equal(await attempt(env.STAGING_URL, env, new Response(body)), 1);
    assert.equal(calls, 2);
    assert.equal(await runCli(['compare', '--before', meta, '--after', meta, '--out', out], env), 1);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
