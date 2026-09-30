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
const cargo = parseCargoManifest(read('Cargo.toml'));
const members = cargo.workspace.members;
// Keep bootstrap coverage after the real checkout has published its first release.
// Only Rust/config inputs come from the checkout; never read its live changelog.
const bootstrapChangelog = read('scripts/fixtures/bootstrap-changelog.md');
const originalNotes = bootstrapChangelog.split('\n').filter(line => line.startsWith('- '));
assert.equal(originalNotes.length, 3, 'The immutable fixture must preserve all three RSVP notes');
const bootstrapSnapshot = Object.freeze({
  ...Object.fromEntries([
    'Cargo.toml', 'Cargo.lock', 'src/lib.rs',
    'release-please-config.json', '.release-please-manifest.json',
    ...members.map(member => `${member}/Cargo.toml`),
  ].map(file => [file, read(file)])),
  'CHANGELOG.md': bootstrapChangelog,
});
const logger = {info() {}, debug() {}, warn() {}, error() {}};
setLogger(logger);

assert.equal(require('release-please/package.json').version, '17.6.0');
assert.deepEqual(Object.keys(config.packages), ['.']);
assert.equal(config.packages['.']['release-type'], 'rust');
assert.equal(config.packages['.']['include-component-in-tag'], false);
assert.equal(cargo.package.publish, false);

function assertSynchronizedSnapshot(snapshot) {
  const cargo = parseCargoManifest(snapshot['Cargo.toml']);
  const version = cargo.package.version;
  assert.deepEqual(JSON.parse(snapshot['.release-please-manifest.json']), {'.': version});
  const manifests = ['Cargo.toml', ...cargo.workspace.members.map(member => `${member}/Cargo.toml`)];
  const packages = parseCargoLockfile(snapshot['Cargo.lock']).package;
  for (const file of manifests) {
    const parsed = parseCargoManifest(snapshot[file]);
    assert.equal(parsed.package.version, version, `Unsynchronized package in ${file}`);
    for (const section of ['dependencies', 'dev-dependencies', 'build-dependencies']) {
      for (const dependency of Object.values(parsed[section] || {})) {
        if (dependency.path) assert.equal(dependency.version, version, `Unsynchronized dependency in ${file}`);
      }
    }
    assert.equal(packages.find(pkg => pkg.name === parsed.package.name).version, version, `Unsynchronized lock entry for ${file}`);
  }
  return version;
}

async function simulate(snapshot, {message, file, tagged, bootstrap}) {
  // Everything, including the prior version/tag, is derived from this snapshot.
  // The next lifecycle receives the previous native PR's generated files intact.
  const seed = assertSynchronizedSnapshot(snapshot);
  const config = JSON.parse(snapshot['release-please-config.json']);
  const members = parseCargoManifest(snapshot['Cargo.toml']).workspace.members;
  assert.equal(/^## Unreleased$/m.test(snapshot['CHANGELOG.md']), bootstrap);
  for (const note of originalNotes) {
    assert.equal(snapshot['CHANGELOG.md'].split(note).length - 1, 1, 'Input snapshot must preserve each RSVP note exactly once');
  }
  const content = {...snapshot};
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
  const originalBody = pr.body.toString();
  const migrated = migrateReleaseNotes(content['CHANGELOG.md'], originalBody);
  if (bootstrap) {
    assert.notEqual(migrated.changelog, content['CHANGELOG.md'], 'Bootstrap changelog must be migrated');
    assert.notEqual(migrated.body, originalBody, 'Bootstrap PR body must be migrated');
    assert.throws(() => migrateReleaseNotes(content['CHANGELOG.md'], 'unrelated body'), /missing from PR body/);
  } else {
    assert.deepEqual(migrated, {changelog: content['CHANGELOG.md'], body: originalBody}, 'Post-release migration must be a no-op');
    assert(content['CHANGELOG.md'].endsWith(snapshot['CHANGELOG.md'].slice('# Changelog\n\n'.length)), 'Next release must retain the full previous changelog');
  }
  assert.deepEqual(migrateReleaseNotes(migrated.changelog, migrated.body), migrated, 'Migration must be idempotent');
  content['CHANGELOG.md'] = migrated.changelog;
  const body = migrated.body;
  const [latestNotes] = migrated.changelog.slice('# Changelog\n\n'.length).split(/\n##? /);
  assert.equal((migrated.changelog.match(/^# Changelog$/gm) || []).length, 1);
  assert(!/^## (Changelog|Unreleased)$/m.test(migrated.changelog));
  for (const note of originalNotes) {
    assert.equal(migrated.changelog.split(note).length - 1, 1, 'Historical note must appear once in the versioned changelog');
    assert.equal(latestNotes.split(note).length - 1, bootstrap ? 1 : 0, 'Only the first release section must include each historical note');
    assert.equal(body.split(note).length - 1, bootstrap ? 1 : 0, 'Only the first release PR body must include each historical note');
  }
  if (bootstrap) {
    for (const heading of ['Added', 'Fixed']) {
      assert.equal((migrated.changelog.match(new RegExp(`^### ${heading}$`, 'gm')) || []).length, 1);
    }
  }
  const version = assertSynchronizedSnapshot(content);
  assert.notEqual(version, seed, 'Native release must advance the input snapshot version');
  assert(latestNotes.includes(version));
  assert(body.includes('Refs: TOG-9865'));
  if (message.startsWith('feat')) assert(latestNotes.includes('### Added'));
  if (/^(fix|security)/.test(message)) assert(latestNotes.includes('### Fixed'));
  state.merged = [{
    number: 999, title: pr.title.toString(), body,
    headBranchName: pr.headRefName, baseBranchName: 'main',
    labels: ['autorelease: pending'], sha: 'merged-fixture', files: pr.updates.map(update => update.path),
  }];
  const releases = await manifest.buildReleases();
  assert.deepEqual(releases.map(release => release.tag.toString()), [`v${version}`]);
  assert.equal(releases[0].path, '.');
  for (const note of originalNotes) {
    assert.equal(releases[0].notes.split(note).length - 1, bootstrap ? 1 : 0, 'Only the first published release payload must include each historical note');
  }
  // The manifest seed is an explicit version even before a real tag exists.
  const strategy = new DefaultVersioningStrategy({
    bumpMinorPreMajor: config.packages['.']['bump-minor-pre-major'],
    bumpPatchForMinorPreMajor: config.packages['.']['bump-patch-for-minor-pre-major'],
    logger,
  });
  const expected = strategy.bump(Version.parse(seed), parseConventionalCommits([{sha: 'feature', message}], logger));
  assert.equal(version, expected.toString());
  console.log(`PASS ${bootstrap ? 'bootstrap' : 'post-release'} ${tagged ? 'tagged' : 'untagged'} ${message.split(':')[0]} ${file}: one v${version} release, synchronized manifests/dependencies/lock`);
  return Object.freeze(content);
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
  const cases = [
    ...scopes.map(file => ({message: 'feat: scoped feature', file})),
    {message: 'feat!: breaking workspace change', file: 'crates/core/src/lib.rs'},
    {message: 'fix: repair workspace behavior', file: 'crates/core/src/lib.rs'},
    {message: 'security: repair permission handling', file: 'crates/core/src/lib.rs'},
  ];
  let bootstrapCount = 0;
  let postReleaseCount = 0;
  for (const tagged of [false, true]) {
    for (const testCase of cases) {
      const releasedSnapshot = await simulate(bootstrapSnapshot, {...testCase, tagged, bootstrap: true});
      bootstrapCount++;
      // Do not reconstruct just the changelog or seed: use every generated file
      // (root/member manifests, dependency versions, lock, and migrated notes).
      await simulate(releasedSnapshot, {...testCase, tagged: true, bootstrap: false});
      postReleaseCount++;
    }
  }
  assert.equal(bootstrapCount, 18, 'Retain all existing bootstrap lifecycle cases');
  assert.equal(postReleaseCount, 18, 'Exercise the actual next native release for every generated snapshot');
  console.log(`PASS ${bootstrapCount} bootstrap + ${postReleaseCount} generated post-release native lifecycles; 5 migration guards`);
})().catch(error => { console.error(error.stack); process.exitCode = 1; });
