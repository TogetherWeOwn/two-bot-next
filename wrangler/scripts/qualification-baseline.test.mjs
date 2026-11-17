import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { test } from 'node:test';
import { mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { baseline, checkArtifact, checkBaseline, checkInstalledTree, checkProvenance, wranglerIntegrity } from './qualification-baseline.mjs';

function fixture() {
  return {
    manifest: { devDependencies: { ...baseline } },
    lock: {
      '': { devDependencies: { ...baseline } },
      ...Object.fromEntries(Object.entries(baseline).map(([name, version]) => [
        `node_modules/${name}`, { version },
      ])),
      'node_modules/wrangler': {
        version: baseline.wrangler, dependencies: { miniflare: '5.20261023.3-beta', workerd: '1.20261023.1' },
      },
      'node_modules/wrangler/node_modules/miniflare': {
        version: '5.20261023.3-beta', dependencies: { workerd: '1.20261023.1' },
      },
      'node_modules/wrangler/node_modules/workerd': { version: '1.20261023.1' },
    },
  };
}

test('accepts the held baseline and refuses drift in each input', () => {
  const valid = fixture();
  assert.doesNotThrow(() => checkBaseline(valid.manifest, valid.lock));
  for (const name of Object.keys(baseline)) {
    for (const part of ['manifest', 'root', 'installed']) {
      const { manifest, lock } = fixture();
      if (part === 'manifest') manifest.devDependencies[name] = '0.0.0';
      if (part === 'root') lock[''].devDependencies[name] = '0.0.0';
      if (part === 'installed') lock[`node_modules/${name}`].version = '0.0.0';
      assert.throws(() => checkBaseline(manifest, lock), `${name}: ${part}`);
    }
  }
});

test('refuses missing or inconsistent nested runtime edges', () => {
  const wrangler = 'node_modules/wrangler';
  const miniflare = `${wrangler}/node_modules/miniflare`;
  const workerd = `${wrangler}/node_modules/workerd`;
  for (const change of [
    lock => { delete lock[miniflare]; },
    lock => { delete lock[workerd]; },
    lock => { lock[wrangler].dependencies.miniflare = 'wrong'; },
    lock => { lock[wrangler].dependencies.workerd = 'wrong'; },
    lock => { lock[miniflare].dependencies.workerd = 'wrong'; },
    lock => {
      lock[miniflare].version = '5.20261024.1-beta';
      lock[wrangler].dependencies.miniflare = '5.20261024.1-beta';
    },
    lock => {
      lock[workerd].version = '1.20261024.1';
      lock[wrangler].dependencies.workerd = '1.20261024.1';
      lock[miniflare].dependencies.workerd = '1.20261024.1';
    },
  ]) {
    const { manifest, lock } = fixture();
    change(lock);
    assert.throws(() => checkBaseline(manifest, lock));
  }
});

test('binds artifact bytes to the pinned lock integrity', () => {
  const bytes = Buffer.from('fixture tarball');
  const integrity = `sha512-${createHash('sha512').update(bytes).digest('base64')}`;
  assert.doesNotThrow(() => checkArtifact(bytes, integrity, integrity));
  assert.throws(() => checkArtifact(Buffer.from('tampered'), integrity, integrity));
  assert.throws(() => checkArtifact(bytes, integrity, 'sha512-different'));
  assert.throws(() => checkArtifact(bytes, integrity));
});

test('compares installed files without ignoring nested package contents', () => {
  const temp = mkdtempSync(join(tmpdir(), 'qualification-test-'));
  const archive = join(temp, 'archive');
  const installed = join(temp, 'installed');
  try {
    for (const path of [archive, installed]) {
      mkdirSync(join(path, 'lib'), { recursive: true });
      writeFileSync(join(path, 'lib', 'file'), 'original');
      symlinkSync('file', join(path, 'lib', 'link'));
    }
    mkdirSync(join(installed, 'node_modules'));
    checkInstalledTree(archive, installed);
    writeFileSync(join(installed, 'lib', 'file'), 'tampered');
    assert.throws(() => checkInstalledTree(archive, installed));
    writeFileSync(join(installed, 'lib', 'file'), 'original');
    mkdirSync(join(installed, 'lib', 'node_modules'));
    assert.throws(() => checkInstalledTree(archive, installed));
    rmSync(join(installed, 'lib', 'node_modules'), { recursive: true });
    rmSync(join(installed, 'lib', 'link'));
    symlinkSync('other', join(installed, 'lib', 'link'));
    assert.throws(() => checkInstalledTree(archive, installed));
    mkdirSync(join(archive, 'node_modules'));
    assert.throws(() => checkInstalledTree(archive, installed));
  } finally {
    rmSync(temp, { recursive: true, force: true });
  }
});

test('refuses provenance claim drift after authentication', () => {
  const statement = {
    _type: 'https://in-toto.io/Statement/v1', predicateType: 'https://slsa.dev/provenance/v1',
    subject: [{ name: 'pkg:npm/wrangler@4.156.0', digest: { sha512: Buffer.from(wranglerIntegrity.slice(7), 'base64').toString('hex') } }],
    predicate: { buildDefinition: {
      buildType: 'https://slsa-framework.github.io/github-actions-buildtypes/workflow/v1',
      externalParameters: { workflow: { ref: 'refs/heads/main', repository: 'https://github.com/cloudflare/workers-sdk', path: '.github/workflows/changesets.yml' } },
      resolvedDependencies: [{ uri: 'git+https://github.com/cloudflare/workers-sdk@refs/heads/main', digest: { gitCommit: 'b1ced56e11378efe3742c6eb51f96e70d572f0a4' } }],
    } },
  };
  checkProvenance(statement);
  for (const change of [
    s => { s._type = 'wrong'; },
    s => { s.predicateType = 'wrong'; },
    s => { s.subject[0].name = 'wrong'; },
    s => { s.subject[0].digest.sha512 = 'wrong'; },
    s => { s.subject.push(s.subject[0]); },
    s => { s.predicate.buildDefinition.buildType = 'wrong'; },
    s => { s.predicate.buildDefinition.externalParameters.workflow.path = 'wrong'; },
    s => { s.predicate.buildDefinition.externalParameters.workflow.repository = 'wrong'; },
    s => { s.predicate.buildDefinition.externalParameters.workflow.ref = 'wrong'; },
    s => { s.predicate.buildDefinition.resolvedDependencies[0].digest.gitCommit = 'wrong'; },
  ]) {
    const invalid = structuredClone(statement);
    change(invalid);
    assert.throws(() => checkProvenance(invalid));
  }
});

test('refuses missing lock and manifest entries', () => {
  const { manifest, lock } = fixture();
  delete manifest.devDependencies.wrangler;
  assert.throws(() => checkBaseline(manifest, lock));
  delete lock['node_modules/wrangler'];
  assert.throws(() => checkBaseline(fixture().manifest, lock));
});
