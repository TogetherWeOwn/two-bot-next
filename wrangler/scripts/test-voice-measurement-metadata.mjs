import assert from 'node:assert/strict';
import test from 'node:test';
import { existsSync, mkdtempSync, readFileSync, rmSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { metadataFromGrant, writeGrantMetadata } from './voice-measurement-metadata.mjs';

const grant = { runId: '12', runAttempt: '1', sha: 'a'.repeat(40), deploymentId: 'dep', expiresAt: '4070908800000' };

test('preserves the grant binding and converts its millisecond expiry', () => {
  assert.deepEqual(metadataFromGrant(grant), { ...grant, expiresAt: '2099-01-01T00:00:00.000Z' });
});

test('writes only validated grant metadata and refuses existing files', () => {
  const dir = mkdtempSync(join(tmpdir(), 'voice-metadata-'));
  try {
    const path = join(dir, 'metadata.json');
    const env = { GITHUB_RUN_ID: grant.runId, GITHUB_RUN_ATTEMPT: grant.runAttempt,
      STAGING_USE_SHA: grant.sha, STAGING_USE_DEPLOYMENT: grant.deploymentId,
      STAGING_USE_EXPIRES_AT: grant.expiresAt, UNRELATED_SECRET: 'not-persisted' };
    writeGrantMetadata(path, env);
    assert.deepEqual(JSON.parse(readFileSync(path, 'utf8')), metadataFromGrant(grant));
    assert.equal(statSync(path).mode & 0o777, 0o600);
    assert.throws(() => writeGrantMetadata(path, env));
    assert.throws(() => writeGrantMetadata(join(dir, 'invalid.json'), { ...env, STAGING_USE_SHA: '' }));
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('CLI refuses invalid metadata and existing output without exposing input', () => {
  const dir = mkdtempSync(join(tmpdir(), 'voice-metadata-cli-'));
  try {
    const path = join(dir, 'metadata.json');
    const script = fileURLToPath(new URL('./voice-measurement-metadata.mjs', import.meta.url));
    const env = { ...process.env, GITHUB_RUN_ID: grant.runId, GITHUB_RUN_ATTEMPT: grant.runAttempt,
      STAGING_USE_SHA: grant.sha, STAGING_USE_DEPLOYMENT: grant.deploymentId,
      STAGING_USE_EXPIRES_AT: grant.expiresAt };
    const run = (overrides) => spawnSync(process.execPath, [script, path], {
      env: { ...env, ...overrides }, encoding: 'utf8', timeout: 5000,
    });
    const bad = run({ STAGING_USE_DEPLOYMENT: 'private value' });
    assert.equal(bad.status, 1);
    assert.equal(bad.stdout, '');
    assert.equal(bad.stderr, 'voice measurement metadata refused\n');
    assert.equal(existsSync(path), false);
    const missingPath = spawnSync(process.execPath, [script], { env, encoding: 'utf8', timeout: 5000 });
    assert.equal(missingPath.status, 1);
    assert.equal(missingPath.stdout, '');
    assert.equal(missingPath.stderr, 'voice measurement metadata refused\n');
    assert.equal(run({}).status, 0);
    const original = readFileSync(path, 'utf8');
    const duplicate = run({});
    assert.equal(duplicate.status, 1);
    assert.equal(duplicate.stdout, '');
    assert.equal(duplicate.stderr, 'voice measurement metadata refused\n');
    assert.equal(readFileSync(path, 'utf8'), original);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('refuses malformed grant expiries and identities', () => {
  for (const expiresAt of ['', '0', '-1', '1.5', '1e3', ' 42', '9007199254740992', '8640000000000001']) {
    assert.throws(() => metadataFromGrant({ ...grant, expiresAt }));
  }
  for (const field of ['runId', 'runAttempt', 'sha', 'deploymentId']) {
    assert.throws(() => metadataFromGrant({ ...grant, [field]: '' }));
  }
});
