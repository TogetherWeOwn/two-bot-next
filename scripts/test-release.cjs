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
const {migrateReleaseNotes} = require('./migrate-release-notes.cjs');
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
  const originalNotes = files['CHANGELOG.md'].split('\n').filter(line => line.startsWith('- '));
  const originalBody = pr.body.toString();
  const migrated = migrateReleaseNotes(content['CHANGELOG.md'], originalBody);
  assert.notEqual(migrated.changelog, content['CHANGELOG.md']);
  assert.deepEqual(migrateReleaseNotes(migrated.changelog, migrated.body), migrated, 'Migration must be idempotent');
  assert.throws(() => migrateReleaseNotes(content['CHANGELOG.md'], 'unrelated body'), /missing from PR body/);
  content['CHANGELOG.md'] = migrated.changelog;
  const body = migrated.body;
  assert.equal((migrated.changelog.match(/^# Changelog$/gm) || []).length, 1);
  assert(!/^## (Changelog|Unreleased)$/m.test(migrated.changelog));
  for (const note of originalNotes) {
    assert.equal(migrated.changelog.split(note).length - 1, 1, 'Historical note must appear once in the versioned changelog');
    assert.equal(body.split(note).length - 1, 1, 'Historical note must appear once in the release PR body');
  }
  for (const heading of ['Added', 'Fixed']) {
    assert.equal((migrated.changelog.match(new RegExp(`^### ${heading}$`, 'gm')) || []).length, 1);
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
  if (/^(fix|security)/.test(message)) assert(content['CHANGELOG.md'].includes('### Fixed'));
  state.merged = [{
    number: 999, title: pr.title.toString(), body,
    headBranchName: pr.headRefName, baseBranchName: 'main',
    labels: ['autorelease: pending'], sha: 'merged-fixture', files: pr.updates.map(update => update.path),
  }];
  const releases = await manifest.buildReleases();
  assert.deepEqual(releases.map(release => release.tag.toString()), [`v${version}`]);
  assert.equal(releases[0].path, '.');
  for (const note of originalNotes) assert(releases[0].notes.includes(note), 'Historical note missing from published release payload');
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

const bootstrap = '# Changelog\n\n## 0.2.0\n\n### Added\n\n* generated feature\n\n## Changelog\n\n## Unreleased\n\n### Fixed\n\n- historical repair\n';
const bootstrapBody = '## 0.2.0\n\n### Added\n\n* generated feature\n';
assert.throws(() => migrateReleaseNotes(bootstrap.replace('## Changelog\n\n## Unreleased', '## Unreleased'), bootstrapBody), /layout/);
assert.throws(() => migrateReleaseNotes(bootstrap + '\n## 0.1.0\n', bootstrapBody), /historical release/);
assert.throws(() => migrateReleaseNotes(bootstrap.replace('### Fixed', '### Unknown'), bootstrapBody), /Unsupported/);
assert.throws(() => migrateReleaseNotes(bootstrap, bootstrapBody + bootstrapBody), /Ambiguous/);
const {Changelog} = require(path.join(library, 'build/src/updaters/changelog'));
const firstRelease = migrateReleaseNotes(bootstrap, bootstrapBody);
const nextBody = '## 0.3.0\n\n### Fixed\n\n* later repair';
const nextChangelog = new Changelog({version: Version.parse('0.3.0'), changelogEntry: nextBody}).updateContent(firstRelease.changelog);
assert.deepEqual(migrateReleaseNotes(nextChangelog, nextBody), {changelog: nextChangelog, body: nextBody});
assert.equal((nextChangelog.match(/^# Changelog$/gm) || []).length, 1);
assert.equal(nextChangelog.split('- historical repair').length - 1, 1);
assert(!nextBody.includes('- historical repair'), 'Later release must not repeat bootstrap notes');
console.log('PASS 5 bootstrap migration guards: layout, history, section, ambiguous body, subsequent release');

(async () => {
  const scopes = ['src/lib.rs', ...members.map(member => `${member}/src/${member === 'crates/bot' ? 'main' : 'lib'}.rs`), 'wrangler/src/index.ts'];
  for (const tagged of [false, true]) {
    for (const scope of scopes) await simulate('feat: scoped feature', scope, tagged);
    await simulate('feat!: breaking workspace change', 'crates/core/src/lib.rs', tagged);
    await simulate('fix: repair workspace behavior', 'crates/core/src/lib.rs', tagged);
    await simulate('security: repair permission handling', 'crates/core/src/lib.rs', tagged);
  }
})().catch(error => { console.error(error.stack); process.exitCode = 1; });
