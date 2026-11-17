import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { lstatSync, readdirSync, readFileSync, readlinkSync } from 'node:fs';
import { join } from 'node:path';

export function checkInstalledTree(archive, installed, root = true) {
  const a = lstatSync(archive);
  const b = lstatSync(installed);
  assert.equal(b.isDirectory(), a.isDirectory(), `directory mismatch: ${installed}`);
  assert.equal(b.isFile(), a.isFile(), `file mismatch: ${installed}`);
  assert.equal(b.isSymbolicLink(), a.isSymbolicLink(), `symlink mismatch: ${installed}`);
  if (a.isDirectory()) {
    if (root) assert(!readdirSync(archive).includes('node_modules'), 'archive contains bundled dependencies');
    const names = path => readdirSync(path).filter(name => !(root && name === 'node_modules')).sort();
    assert.deepEqual(names(installed), names(archive), `entries mismatch: ${installed}`);
    for (const name of names(archive)) checkInstalledTree(join(archive, name), join(installed, name), false);
  } else if (a.isFile()) {
    assert(readFileSync(archive).equals(readFileSync(installed)), `content mismatch: ${installed}`);
  } else if (a.isSymbolicLink()) {
    assert.equal(readlinkSync(installed), readlinkSync(archive), `link mismatch: ${installed}`);
  } else {
    assert.fail(`unsupported archive entry: ${archive}`);
  }
}

export const baseline = Object.freeze({
  wrangler: '4.156.0',
  '@cloudflare/workers-types': '5.20261030.1',
  miniflare: '5.20260926.1-alpha',
});

export const wranglerIntegrity = 'sha512-kVLAShuLiNgE3jcrmXcymDf/iPKDPrSxACe5ueugW/33HeNB9E9CkKYchudCeBwgmsnl4k+1zt4NO7vqb/fRuQ==';

export function checkArtifact(bytes, integrity, expectedIntegrity = wranglerIntegrity) {
  assert.equal(integrity, expectedIntegrity, 'Wrangler integrity baseline changed');
  const actual = `sha512-${createHash('sha512').update(bytes).digest('base64')}`;
  assert.equal(actual, integrity, 'Wrangler tarball differs from lockfile');
}

// Call only after authenticating the DSSE envelope and its tarball with Cosign.
export function checkProvenance(statement, integrity = wranglerIntegrity) {
  assert.equal(statement._type, 'https://in-toto.io/Statement/v1');
  assert.equal(statement.predicateType, 'https://slsa.dev/provenance/v1');
  assert.equal(statement.subject?.length, 1);
  assert.equal(statement.subject[0].name, 'pkg:npm/wrangler@4.156.0');
  assert.equal(statement.subject[0].digest?.sha512, Buffer.from(integrity.slice(7), 'base64').toString('hex'));
  const definition = statement.predicate?.buildDefinition;
  assert.equal(definition?.buildType, 'https://slsa-framework.github.io/github-actions-buildtypes/workflow/v1');
  assert.deepEqual(definition.externalParameters?.workflow, {
    ref: 'refs/heads/main', repository: 'https://github.com/cloudflare/workers-sdk',
    path: '.github/workflows/changesets.yml',
  });
  const dependencies = definition.resolvedDependencies;
  assert(Array.isArray(dependencies) && dependencies.some(dependency =>
    dependency.uri === 'git+https://github.com/cloudflare/workers-sdk@refs/heads/main' &&
    dependency.digest?.gitCommit === 'b1ced56e11378efe3742c6eb51f96e70d572f0a4'), 'source commit mismatch');
}

export function checkBaseline(manifest, lock) {
  for (const [name, version] of Object.entries(baseline)) {
    assert.equal(manifest.devDependencies?.[name], version, `${name} manifest baseline changed`);
    assert.equal(lock['']?.devDependencies?.[name], version, `${name} lock baseline changed`);
    assert.equal(lock[`node_modules/${name}`]?.version, version, `${name} locked version changed`);
  }
  const wrangler = lock['node_modules/wrangler'];
  const nestedMiniflare = lock['node_modules/wrangler/node_modules/miniflare'];
  const nestedWorkerd = lock['node_modules/wrangler/node_modules/workerd'];
  assert(nestedMiniflare?.version && nestedWorkerd?.version, 'nested runtime missing from lock');
  assert.equal(nestedMiniflare.version, '5.20261023.3-beta', 'nested Miniflare baseline changed');
  assert.equal(nestedWorkerd.version, '1.20261023.1', 'nested workerd baseline changed');
  assert.equal(wrangler.dependencies?.miniflare, nestedMiniflare.version, 'Wrangler Miniflare lock mismatch');
  assert.equal(wrangler.dependencies?.workerd, nestedWorkerd.version, 'Wrangler workerd lock mismatch');
  assert.equal(nestedMiniflare.dependencies?.workerd, nestedWorkerd.version, 'Miniflare workerd lock mismatch');
}
