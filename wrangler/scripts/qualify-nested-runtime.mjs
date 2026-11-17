import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { readFileSync, realpathSync } from 'node:fs';
import { createRequire } from 'node:module';
import { resolve, sep } from 'node:path';
import { checkBaseline } from './qualification-baseline.mjs';

const [major, minor] = process.versions.node.split('.').map(Number);
assert(major > 22 || (major === 22 && minor >= 12), 'Node 22.12.0 or later is required');

const lock = JSON.parse(readFileSync('package-lock.json', 'utf8')).packages;
const manifest = JSON.parse(readFileSync('package.json', 'utf8'));
checkBaseline(manifest, lock);
const projectRequire = createRequire(resolve('package.json'));
const wranglerRoot = realpathSync('node_modules/wrangler');
const wranglerRequire = createRequire(resolve(wranglerRoot, 'package.json'));
const nestedRoot = realpathSync('node_modules/wrangler/node_modules/miniflare');
const nestedRequire = createRequire(resolve(nestedRoot, 'package.json'));
const workerdRoot = realpathSync('node_modules/wrangler/node_modules/workerd');

function inside(path, root) {
  return realpathSync(path).startsWith(`${root}${sep}`);
}

for (const path of [
  'node_modules/wrangler',
  'node_modules/@cloudflare/workers-types',
  'node_modules/miniflare',
  'node_modules/wrangler/node_modules/miniflare',
  'node_modules/wrangler/node_modules/workerd',
]) {
  const installed = JSON.parse(readFileSync(resolve(path, 'package.json'), 'utf8'));
  assert.equal(installed.version, lock[path].version, `${path} differs from lockfile`);
}
assert(inside(wranglerRequire.resolve('miniflare'), nestedRoot), 'Wrangler did not resolve nested Miniflare');
assert(!inside(projectRequire.resolve('miniflare'), nestedRoot), 'direct Miniflare resolved to nested copy');
assert(inside(wranglerRequire.resolve('workerd/package.json'), workerdRoot), 'Wrangler did not resolve nested workerd');
assert(inside(nestedRequire.resolve('workerd/package.json'), workerdRoot), 'nested Miniflare did not resolve Wrangler workerd');

const workerdVersion = lock['node_modules/wrangler/node_modules/workerd'].version;
const versionOutput = execFileSync(resolve(workerdRoot, 'bin/workerd'), ['--version'], {
  encoding: 'utf8',
  timeout: 10000,
}).trim();
// workerd prints the release date, not the npm package version.
assert.equal(workerdVersion, '1.20261023.1');
assert.equal(versionOutput, 'workerd 2026-10-23');

const { Miniflare } = wranglerRequire('miniflare');
const runtime = new Miniflare({
  modules: true,
  script: 'export default { fetch() { return new Response("nested-runtime-ok") } }',
  compatibilityDate: '2026-10-23',
});
try {
  const response = await runtime.dispatchFetch('http://example.test/');
  assert.equal(response.status, 200);
  assert.equal(await response.text(), 'nested-runtime-ok');
} finally {
  await runtime.dispose();
}
console.log(`Nested Miniflare and workerd ${workerdVersion} qualified`);
