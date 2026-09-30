'use strict';

// Offline fixture for the exact release-please library bundled by action v5.0.0.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const library = path.dirname(require.resolve('release-please/package.json'));
const {Manifest, setLogger} = require('release-please');
const {Version} = require(path.join(library, 'build/src/version'));
const {parseConventionalCommits} = require(path.join(library, 'build/src/commit'));
const {DefaultVersioningStrategy} = require(path.join(library, 'build/src/versioning-strategies/default'));
const {parseCargoManifest, parseCargoLockfile} = require(path.join(library, 'build/src/updaters/rust/common'));
const root = path.resolve(__dirname, '..');
const read = file => fs.readFileSync(path.join(root, file), 'utf8');
const config = JSON.parse(read('release-please-config.json'));
const seed = JSON.parse(read('.release-please-manifest.json'))['.'];
const cargo = parseCargoManifest(read('Cargo.toml'));
const members = cargo.workspace.members;
const files = Object.fromEntries([
  'Cargo.toml', 'Cargo.lock', 'CHANGELOG.md', 'src/lib.rs',
  'release-please-config.json', '.release-please-manifest.json',
  ...members.map(member => `${member}/Cargo.toml`),
].map(file => [file, read(file)]));
const logger = {info() {}, debug() {}, warn() {}, error() {}};
setLogger(logger);

assert.equal(require('release-please/package.json').version, '17.6.0');
assert.deepEqual(Object.keys(config.packages), ['.']);
assert.equal(config.packages['.']['release-type'], 'rust');
assert.equal(config.packages['.']['include-component-in-tag'], false);
assert.equal(cargo.package.version, seed);
assert.equal(cargo.package.publish, false);

async function simulate(message, file, tagged) {
  const content = {...files};
  const state = {merged: []};
  const github = {
    repository: {owner: 'fixture', repo: 'two-bot-next'},
    async getFileJson(file) { return JSON.parse(content[file]); },
    async getFileContentsOnBranch(file) {
      assert.equal(typeof content[file], 'string', `Missing mocked file: ${file}`);
      return {content: Buffer.from(content[file]).toString('base64'), parsedContent: content[file], sha: 'fixture-content'};
    },
    async findFilesByGlobAndRef(glob) {
      if (glob === 'crates/*/Cargo.toml') return members.map(member => `${member}/Cargo.toml`);
      assert(members.includes(glob), `Unexpected manifest glob: ${glob}`);
      return [glob];
    },
    async *releaseIterator() {
      if (tagged) yield {tagName: `v${seed}`, sha: 'previous', notes: '', name: `v${seed}`};
    },
    async *tagIterator() {},
    async *mergeCommitIterator() {
      yield {sha: 'feature', message, files: [file]};
      if (tagged) yield {sha: 'previous', message: `chore: release ${seed}`, files: []};
    },
    async *pullRequestIterator() { yield* state.merged; },
  };
  // Any unmocked operation (including a remote mutation) fails closed.
  const client = new Proxy(github, {
    get(target, key) {
      assert(key in target, `Unmocked GitHub operation: ${String(key)}`);
      return target[key];
    },
  });
  const manifest = await Manifest.fromManifest(client, 'main', undefined, undefined, {logger});
  const prs = await manifest.buildPullRequests();
  assert.equal(prs.length, 1);
  const pr = prs[0];
  for (const update of pr.updates) {
    if (content[update.path] === undefined && !update.createIfMissing) continue;
    content[update.path] = update.updater.updateContent(content[update.path] || '', logger);
  }
  const version = parseCargoManifest(content['Cargo.toml']).package.version;
  assert.deepEqual(JSON.parse(content['.release-please-manifest.json']), {'.': version});
  for (const member of members) {
    const parsed = parseCargoManifest(content[`${member}/Cargo.toml`]);
    assert.equal(parsed.package.version, version);
    for (const dependency of Object.values(parsed.dependencies || {})) {
      if (dependency.path) assert.equal(dependency.version, version);
    }
  }
  const packages = parseCargoLockfile(content['Cargo.lock']).package;
  const names = [cargo.package.name, ...members.map(member => parseCargoManifest(files[`${member}/Cargo.toml`]).package.name)];
  for (const name of names) assert.equal(packages.find(pkg => pkg.name === name).version, version);
  assert(content['CHANGELOG.md'].includes(version));
  assert(pr.body.toString().includes('Refs: TOG-9865'));
  if (message.startsWith('feat')) assert(content['CHANGELOG.md'].includes('### Added'));
  if (message.startsWith('fix')) assert(content['CHANGELOG.md'].includes('### Fixed'));
  state.merged = [{
    number: 999, title: pr.title.toString(), body: pr.body.toString(),
    headBranchName: pr.headRefName, baseBranchName: 'main',
    labels: ['autorelease: pending'], sha: 'merged-fixture', files: pr.updates.map(update => update.path),
  }];
  const releases = await manifest.buildReleases();
  assert.deepEqual(releases.map(release => release.tag.toString()), [`v${version}`]);
  assert.equal(releases[0].path, '.');
  // The manifest seed is an explicit version even before a real tag exists.
  const strategy = new DefaultVersioningStrategy({
    bumpMinorPreMajor: config.packages['.']['bump-minor-pre-major'],
    bumpPatchForMinorPreMajor: config.packages['.']['bump-patch-for-minor-pre-major'],
    logger,
  });
  const expected = strategy.bump(Version.parse(seed), parseConventionalCommits([{sha: 'feature', message}], logger));
  assert.equal(version, expected.toString());
  console.log(`PASS ${tagged ? 'tagged' : 'untagged'} ${message.split(':')[0]} ${file}: one v${version} release, synchronized manifests/dependencies/lock`);
}

(async () => {
  const scopes = ['src/lib.rs', ...members.map(member => `${member}/src/${member === 'crates/bot' ? 'main' : 'lib'}.rs`), 'wrangler/src/index.ts'];
  for (const tagged of [false, true]) {
    for (const scope of scopes) await simulate('feat: scoped feature', scope, tagged);
    await simulate('feat!: breaking workspace change', 'crates/core/src/lib.rs', tagged);
    await simulate('fix: repair workspace behavior', 'crates/core/src/lib.rs', tagged);
  }
})().catch(error => { console.error(error.stack); process.exitCode = 1; });
