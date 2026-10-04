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
const {FilePullRequestOverflowHandler} = require(path.join(library, 'build/src/util/pull-request-overflow-handler'));
const {migrateReleaseNotes, parseOverflowLink, resolveNotesBody, selectBodyWrite, buildOverflowBody, NATIVE_NOTES_BRANCH, NATIVE_OVERFLOW_SENTENCE, MAX_ISSUE_BODY_SIZE} = require('./migrate-release-notes.cjs');
const {findNewestNativeCommit, findGenerationSnapshot, NATIVE_RELEASE_COMMIT_PATTERN} = require('./release-pr-state.cjs');
// Native 17.6.0 derives the component from the root package name; the live
// library assertions below fail loudly if either constant drifts.
const EXPECTED_HEAD = 'release-please--branches--main--components--two-bot-next';
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
    'fuzz/Cargo.toml', 'CONTRIBUTING.md',
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

function assertExcludedConsumers(snapshot, version) {
  const fuzz = parseCargoManifest(snapshot['fuzz/Cargo.toml']);
  assert.equal(fuzz.package.version, '0.0.0', 'The unpublished fuzz harness keeps its own version');
  const original = parseCargoManifest(bootstrapSnapshot['fuzz/Cargo.toml']);
  assert.equal(fuzz.bin.length, 6, 'Retain all six fuzz targets');
  assert.deepEqual(fuzz.bin, original.bin, 'Release updates must preserve the fuzz target definitions');
  for (const [name, dependency] of Object.entries(original.dependencies)) {
    if (dependency.path) {
      assert.deepEqual(fuzz.dependencies[name], {...dependency, version}, `Unsynchronized fuzz dependency ${name}`);
    } else {
      assert.deepEqual(fuzz.dependencies[name], dependency, `Release updates must preserve external fuzz dependency ${name}`);
    }
  }
  const examples = snapshot['CONTRIBUTING.md'].match(/^two-bot-testsupport = .*$/gm);
  assert.equal(examples?.length, 1, 'Retain one copyable testsupport dependency example');
  const example = parseCargoManifest(`[dev-dependencies]\n${examples[0]}`);
  assert.equal(example['dev-dependencies']['two-bot-testsupport'].version, version, 'Unsynchronized testsupport dependency example');
}

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
  assertExcludedConsumers(snapshot, version);
  return version;
}

const seedVersion = assertSynchronizedSnapshot(bootstrapSnapshot);
for (const name of ['two-bot-core', 'two-bot-cutover']) {
  const stale = bootstrapSnapshot['fuzz/Cargo.toml'].replace(
    new RegExp(`(${name} = .*version = ")[^"]+`), (_, prefix) => `${prefix}0.0.0`);
  assert.throws(() => assertExcludedConsumers({...bootstrapSnapshot, 'fuzz/Cargo.toml': stale}, seedVersion), new RegExp(`Unsynchronized fuzz dependency ${name}`));
}
const staleExample = bootstrapSnapshot['CONTRIBUTING.md'].replace(
  /^(two-bot-testsupport = .*version = ")[^"]+/m, (_, prefix) => `${prefix}0.0.0`);
assert.throws(() => assertExcludedConsumers({...bootstrapSnapshot, 'CONTRIBUTING.md': staleExample}, seedVersion), /Unsynchronized testsupport dependency example/);
console.log('PASS 3 excluded-consumer drift guards: both fuzz dependencies and the contributor example');

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
  assert.equal(pr.headRefName, EXPECTED_HEAD, 'Native head drift: selection constant must match the live library');
  assert.match(pr.title.toString(), NATIVE_RELEASE_COMMIT_PATTERN, 'Native title drift: snapshot tracking depends on this shape');
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
  assert(!/\b(TOG|PAP)-\d+\b/.test(body), 'Generated release PR body must carry no internal tracker ID');
  assert(!body.includes('Refs:'), 'Generated release PR body must carry no footer trailer');
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

const bootstrap = '# Changelog\n\n## 0.2.0\n\n### Added\n\n* generated feature\n\n## Changelog\n\n## Unreleased\n\n### Fixed\n\n- historical repair\n\n### Security\n\n- historical hardening\n\n### Notes\n\n- historical caveat\n';
const bootstrapBody = '## 0.2.0\n\n### Added\n\n* generated feature\n';
assert.throws(() => migrateReleaseNotes(bootstrap.replace('## Changelog\n\n## Unreleased', '## Unreleased'), bootstrapBody), /layout/);
assert.throws(() => migrateReleaseNotes(bootstrap + '\n## 0.1.0\n', bootstrapBody), /historical release/);
assert.throws(() => migrateReleaseNotes(bootstrap.replace('### Fixed', '### Unknown'), bootstrapBody), /Unsupported/);
assert.throws(() => migrateReleaseNotes(bootstrap.replace('### Security', '### Unknown'), bootstrapBody), /Unsupported/);
assert.throws(() => migrateReleaseNotes(bootstrap, bootstrapBody + bootstrapBody), /Ambiguous/);
const {Changelog} = require(path.join(library, 'build/src/updaters/changelog'));
const firstRelease = migrateReleaseNotes(bootstrap, bootstrapBody);
assert(firstRelease.changelog.includes('### Notes'), 'Migrated changelog must preserve the bootstrap Notes tail');
assert(firstRelease.body.includes('- historical caveat'), 'Migrated PR body must preserve the bootstrap Notes tail');
assert(firstRelease.changelog.includes('### Security'), 'Migrated changelog must preserve the bootstrap Security section');
assert(firstRelease.body.includes('- historical hardening'), 'Migrated PR body must preserve the bootstrap Security section');
const nextBody = '## 0.3.0\n\n### Fixed\n\n* later repair';
const nextChangelog = new Changelog({version: Version.parse('0.3.0'), changelogEntry: nextBody}).updateContent(firstRelease.changelog);
assert.deepEqual(migrateReleaseNotes(nextChangelog, nextBody), {changelog: nextChangelog, body: nextBody});
assert.equal((nextChangelog.match(/^# Changelog$/gm) || []).length, 1);
assert.equal(nextChangelog.split('- historical repair').length - 1, 1);
assert(!nextBody.includes('- historical repair'), 'Later release must not repeat bootstrap notes');
assert(!nextBody.includes('- historical hardening'), 'Later release must not repeat bootstrap Security');
assert(!nextBody.includes('- historical caveat'), 'Later release must not repeat bootstrap Notes');
console.log('PASS 7 bootstrap migration guards: layout, history, 2 section, ambiguous body, notes tail, security tail, subsequent release');

// Live 0.3.0 regression: contributors add Unreleased above published 0.2.0,
// and the pinned native updater keeps that prefix above its generated entry.
const unreleasedNotes = '### Added\n\n- pending sticky runtime\n\n### Security\n\n- pending hardening\n\n### Notes\n\n- pending caveat';
const pendingSnapshot = firstRelease.changelog.replace('# Changelog\n\n', `# Changelog\n\n## Unreleased\n\n${unreleasedNotes}\n\n`);
const pendingChangelog = new Changelog({version: Version.parse('0.3.0'), changelogEntry: nextBody}).updateContent(pendingSnapshot);
assert(pendingChangelog.startsWith('# Changelog\n\n## Unreleased\n'), 'Native updater must reproduce the live prefix layout');
const pendingBody = `:robot: release\n---\n\n${nextBody}\n\n---\nRefs: TOG-9865\n`;
const pendingRelease = migrateReleaseNotes(pendingChangelog, pendingBody);
const publishedHistory = firstRelease.changelog.slice('# Changelog\n\n'.length);
assert(pendingRelease.changelog.endsWith(publishedHistory), 'Published history must remain byte-for-byte intact');
assert(!/^## Unreleased$/m.test(pendingRelease.changelog));
for (const note of ['- pending sticky runtime', '- pending hardening', '- pending caveat']) {
  assert.equal(pendingRelease.changelog.split(note).length - 1, 1);
  assert.equal(pendingRelease.body.split(note).length - 1, 1);
}
assert(!pendingRelease.body.includes('- historical repair'), 'Latest PR must not repeat published notes');
assert(!pendingRelease.body.includes('- historical caveat'), 'Latest PR must not repeat the bootstrap Notes tail');
assert(pendingRelease.body.startsWith(':robot: release\n---\n\n'));
assert(pendingRelease.body.endsWith('\n\n---\nRefs: TOG-9865\n'));
assert.deepEqual(migrateReleaseNotes(pendingRelease.changelog, pendingRelease.body), pendingRelease, 'Prefix migration must be idempotent');
assert.deepEqual(migrateReleaseNotes(pendingRelease.changelog, pendingBody), pendingRelease, 'Retry after changelog push repairs only the body');
assert.deepEqual(migrateReleaseNotes(pendingChangelog, pendingRelease.body), pendingRelease, 'Already migrated body still repairs the changelog');
const afterPendingBody = '## 0.4.0\n\n### Added\n\n* future feature';
const afterPendingChangelog = new Changelog({version: Version.parse('0.4.0'), changelogEntry: afterPendingBody}).updateContent(pendingRelease.changelog);
assert.deepEqual(migrateReleaseNotes(afterPendingChangelog, afterPendingBody), {changelog: afterPendingChangelog, body: afterPendingBody});
assert(afterPendingChangelog.endsWith(pendingRelease.changelog.slice('# Changelog\n\n'.length)));
assert(!afterPendingBody.includes('- pending sticky runtime'), 'Following release must not repeat the consumed Unreleased notes');
const emptyPending = pendingChangelog.replace(`${unreleasedNotes}\n\n`, '');
assert.deepEqual(migrateReleaseNotes(emptyPending, pendingBody), {changelog: nextChangelog, body: pendingBody}, 'Empty Unreleased prefix is consumed without inventing notes');
assert.throws(() => migrateReleaseNotes('# Changelog\n\n## Unreleased\n', pendingBody), /Missing versioned/);
assert.throws(() => migrateReleaseNotes(pendingChangelog.replace('### Added\n\n- pending', '## Unreleased\n\n### Added\n\n- pending'), pendingBody), /Duplicate unreleased/);
assert.throws(() => migrateReleaseNotes(pendingChangelog.replace('### Added\n\n- pending', '### Unknown\n\n- pending'), pendingBody), /Unsupported/);
assert.throws(() => migrateReleaseNotes(pendingChangelog.replace('### Added\n\n- pending', '- pending'), pendingBody), /Unsectioned/);
assert.throws(() => migrateReleaseNotes(pendingChangelog.replace('### Added\n\n- pending', '## Changelog\n\n### Added\n\n- pending'), pendingBody), /historical release/);
console.log('PASS post-release Unreleased prefix: native layout, history, body/footer, partial retries, next release, empty prefix, 5 fail-closed guards');

// Native treats a version-shaped line in a pending fence as a release boundary
// and can insert its generated entry inside the fence. Refuse this admission
// before returning either output, rather than silently publishing partial notes.
for (const fence of ['```markdown', '````markdown', '~~~markdown', '   ```markdown']) {
  const pending = `### Notes\n\n- Pending formatting example:\n\n${fence}\n## 1.2.3\n${fence.trim().startsWith('~') ? '~~~' : '````'}\n\n- pending caveat AFTER example`;
  const snapshot = `# Changelog\n\n## Unreleased\n\n${pending}\n\n${publishedHistory}`;
  const generated = new Changelog({version: Version.parse('0.3.0'), changelogEntry: nextBody}).updateContent(snapshot);
  assert(generated.includes(`${fence}\n${nextBody}`), 'Pinned native updater must reproduce insertion inside the pending fence');
  assert(generated.includes('- pending caveat AFTER example'));
  assert(generated.endsWith(publishedHistory));
  assert.throws(() => migrateReleaseNotes(generated, pendingBody), /Ambiguous fenced Unreleased notes/);
  assert.throws(() => migrateReleaseNotes(generated, pendingBody), /Ambiguous fenced Unreleased notes/, 'Retry must fail closed too');
}
const fencedWithoutVersion = pendingChangelog.replace('- pending caveat', '```text\nexample\n```\n\n- pending caveat');
assert.throws(() => migrateReleaseNotes(fencedWithoutVersion, pendingBody), /Ambiguous fenced Unreleased notes/, 'The supported prefix contract requires unfenced notes, even without a version-shaped example');
console.log('PASS fenced pending notes: 4 native version-heading reproductions and retries fail closed; ordinary fenced prefix refused explicitly');

// Overflow link parsing retains the exact native single-line contract.
const overflowUrl = `https://github.com/fixture/two-bot-next/blob/${NATIVE_NOTES_BRANCH}/release-notes.md`;
const overflowBody = `${NATIVE_OVERFLOW_SENTENCE} ${overflowUrl}`;
assert.deepEqual(parseOverflowLink(overflowBody), {url: overflowUrl, branchName: NATIVE_NOTES_BRANCH});
assert.equal(parseOverflowLink(':robot: release\n---\n\n## 0.2.0\n\n---\nRefs: TOG-9865\n'), null);
assert.equal(parseOverflowLink('unrelated body'), null);
assert.equal(parseOverflowLink(`${overflowBody}\ntrailing line`), null, 'Multiline bodies are never overflow links');
assert.equal(parseOverflowLink(`prefix ${overflowBody}`), null, 'Only the exact native sentence parses');
assert.throws(() => resolveNotesBody(overflowBody.replace(NATIVE_NOTES_BRANCH, 'other--release-notes'), () => 'stored'), /Unexpected release-notes branch/);
assert.throws(() => resolveNotesBody(overflowBody, () => ''), /Missing stored release notes/);
assert.equal(resolveNotesBody('plain body', () => { throw new Error('must not fetch for normal bodies'); }), 'plain body');
console.log('PASS 8 overflow link guards: exact sentence, single line, branch match, fail-closed fetch');

// Body write-path selection shares the native 65,536-char PR-body limit: the
// workflow routes reconciled output at or under the limit through PATCH and
// anything larger through the native overflow representation (stored notes +
// single-line link), so GitHub never sees a rejected oversized PATCH.
assert.equal(MAX_ISSUE_BODY_SIZE, 65536, 'Write-path limit must match the native PR-body limit');
assert.equal(selectBodyWrite('x'.repeat(65536)), 'patch', 'At-limit output takes the PATCH path');
assert.equal(selectBodyWrite('x'.repeat(65537)), 'overflow', 'Over-limit output takes the overflow path');
assert.equal(selectBodyWrite('normal body'), 'patch');
const migrationLink = buildOverflowBody('TogetherWeOwn/two-bot-next', NATIVE_NOTES_BRANCH);
assert(!migrationLink.includes('\n'), 'Migration-created overflow body is a single line');
assert.deepEqual(parseOverflowLink(migrationLink), {url: migrationLink.slice(NATIVE_OVERFLOW_SENTENCE.length + 1), branchName: NATIVE_NOTES_BRANCH});
assert.throws(() => buildOverflowBody('TogetherWeOwn/two-bot-next', 'other--release-notes'), /Unexpected release-notes branch/);
assert.throws(() => buildOverflowBody('not a repo', NATIVE_NOTES_BRANCH), /match/);
console.log('PASS 5 body write-path guards: shared native limit, boundary, overflow-link form, fail-closed branch');

// Generation-snapshot tracking binds reuse to the snapshot that produced the
// metadata, not mere ancestry (Update-branch merges keep ancestry while stale).
const shaA = 'a'.repeat(40);
const shaB = 'b'.repeat(40);
const shaC = 'c'.repeat(40);
const shaM = 'd'.repeat(40);
const staleCommits = [
  {sha: shaA, message: 'chore(main): release 0.1.1'},
  {sha: shaB, message: 'chore(release): preserve bootstrap release notes'},
  {sha: shaC, message: 'Merge branch \'main\' of fixture into release branch'},
];
assert.equal(findNewestNativeCommit(staleCommits), shaA);
assert.equal(findGenerationSnapshot(staleCommits, new Map([[shaA, shaM]])), shaM);
assert.equal(findGenerationSnapshot([{sha: shaB, message: 'chore: something else'}], new Map()), null, 'No native commit fails toward regeneration');
assert.throws(() => findGenerationSnapshot([{sha: shaA, message: 'chore(main): release 0.1.1'}], new Map()), /Missing parent evidence/);
console.log('PASS 4 generation-snapshot guards: newest native commit, first-parent snapshot, fail toward regeneration');

async function overflowLifecycle() {
  // The reviewer's 490-commit / 88k-char native overflow, end to end through
  // our code path: stored full notes resolve, migrate, and stay idempotent.
  const snapshot = {...bootstrapSnapshot};
  const content = {...snapshot};
  const pad = i => `feat: scoped release item ${String(i).padStart(3, '0')} ${'x'.repeat(60)}`;
  const github = {
    repository: {owner: 'fixture', repo: 'two-bot-next'},
    async getFileJson(file) { return JSON.parse(content[file]); },
    async getFileContentsOnBranch(file) {
      return {content: Buffer.from(content[file]).toString('base64'), parsedContent: content[file], sha: 'fixture-content'};
    },
    async findFilesByGlobAndRef(glob) {
      if (glob === 'crates/*/Cargo.toml') return members.map(member => `${member}/Cargo.toml`);
      return [glob];
    },
    async *releaseIterator() {},
    async *tagIterator() {},
    async *mergeCommitIterator() {
      for (let i = 0; i < 490; i++) yield {sha: i.toString(16).padStart(40, '0'), message: pad(i), files: ['crates/core/src/lib.rs']};
    },
    async *pullRequestIterator() {},
  };
  const manifest = await Manifest.fromManifest(github, 'main', undefined, undefined, {logger});
  const pr = (await manifest.buildPullRequests())[0];
  assert.equal(pr.headRefName, EXPECTED_HEAD);
  for (const update of pr.updates) {
    if (content[update.path] === undefined && !update.createIfMissing) continue;
    content[update.path] = update.updater.updateContent(content[update.path] || '', logger);
  }
  const fullBody = pr.body.toString();
  assert(fullBody.length > 65536, 'Fixture must actually overflow the native body limit');
  let stored = null;
  let storedBranch = null;
  const client = {
    repository: {defaultBranch: 'main'},
    async createFileOnNewBranch(file, contents, branchName) {
      stored = contents;
      storedBranch = branchName;
      return `https://github.com/fixture/two-bot-next/blob/${branchName}/${file}`;
    },
  };
  const handler = new FilePullRequestOverflowHandler(client, logger);
  const visible = await handler.handleOverflow(pr);
  assert(!visible.includes('\n'), 'Native overflow body is a single-line link');
  assert.equal(storedBranch, NATIVE_NOTES_BRANCH, 'Notes-branch constant must match the native derived branch');
  assert.equal(storedBranch, `${pr.headRefName}--release-notes`);
  const resolved = resolveNotesBody(visible, () => stored);
  assert.equal(resolved, fullBody, 'Stored notes must equal the full native body');
  const migrated = migrateReleaseNotes(content['CHANGELOG.md'], resolved);
  assert.notEqual(migrated.changelog, content['CHANGELOG.md'], 'Overflow changelog must be migrated');
  assert.notEqual(migrated.body, resolved, 'Overflow stored notes must be migrated');
  assert.deepEqual(migrateReleaseNotes(migrated.changelog, migrated.body), migrated, 'Overflow migration must be idempotent');
  for (const note of originalNotes) {
    assert.equal(migrated.changelog.split(note).length - 1, 1, 'Overflow changelog must preserve each RSVP note once');
    assert.equal(migrated.body.split(note).length - 1, 1, 'Overflow stored notes must preserve each RSVP note once');
  }
  assert(!/\b(TOG|PAP)-\d+\b/.test(migrated.body), 'Overflow release PR body must carry no internal tracker ID');
  console.log(`PASS overflow lifecycle: 490 commits, ${fullBody.length}-char native body, stored-notes migration idempotent`);
}

async function migrationGrowthOverflowLifecycle() {
  // The reviewer's P2: a real native normal body (351 conventional commits)
  // that bootstrap migration grows past the 65,536-char PR limit. Include
  // the configured template header in the boundary calibration. Reconciled
  // output must overflow, and the next run must resolve it like native overflow.
  const snapshot = {...bootstrapSnapshot};
  const content = {...snapshot};
  // Calibrated so the real native body lands just under the 65,536-char
  // limit while the migrated body (native notes + bootstrap RSVP notes)
  // crosses it: 351 commits at this padding yield ~65.3k chars in-suite.
  const pad = i => `feat: scoped release item ${String(i).padStart(3, '0')} ${'x'.repeat(56)}`;
  const github = {
    repository: {owner: 'fixture', repo: 'two-bot-next'},
    async getFileJson(file) { return JSON.parse(content[file]); },
    async getFileContentsOnBranch(file) {
      return {content: Buffer.from(content[file]).toString('base64'), parsedContent: content[file], sha: 'fixture-content'};
    },
    async findFilesByGlobAndRef(glob) {
      if (glob === 'crates/*/Cargo.toml') return members.map(member => `${member}/Cargo.toml`);
      return [glob];
    },
    async *releaseIterator() {},
    async *tagIterator() {},
    async *mergeCommitIterator() {
      for (let i = 0; i < 351; i++) yield {sha: i.toString(16).padStart(40, '0'), message: pad(i), files: ['crates/core/src/lib.rs']};
    },
    async *pullRequestIterator() {},
  };
  const manifest = await Manifest.fromManifest(github, 'main', undefined, undefined, {logger});
  const pr = (await manifest.buildPullRequests())[0];
  for (const update of pr.updates) {
    if (content[update.path] === undefined && !update.createIfMissing) continue;
    content[update.path] = update.updater.updateContent(content[update.path] || '', logger);
  }
  const normalBody = pr.body.toString();
  assert(normalBody.length <= MAX_ISSUE_BODY_SIZE, `Fixture must start as a normal body, got ${normalBody.length}`);
  const migrated = migrateReleaseNotes(content['CHANGELOG.md'], normalBody);
  assert.notEqual(migrated.changelog, content['CHANGELOG.md'], 'Migration must consume the bootstrap tail');
  assert(migrated.body.length > MAX_ISSUE_BODY_SIZE, `Migrated body must exceed the limit, got ${migrated.body.length}`);
  assert.equal(selectBodyWrite(migrated.body), 'overflow', 'Grown output takes the overflow path, never PATCH');
  // The workflow's overflow representation: stored notes + single-line link.
  const storedNotes = migrated.body;
  const visible = buildOverflowBody('fixture/two-bot-next', NATIVE_NOTES_BRANCH);
  assert(!visible.includes('\n'), 'Visible overflow body is a single line');
  const resolved = resolveNotesBody(visible, () => storedNotes);
  assert.equal(resolved, storedNotes, 'Next run resolves the stored notes');
  assert.equal(migrateReleaseNotes(migrated.changelog, resolved).body, storedNotes, 'Retry reconciliation is a no-op');
  for (const note of originalNotes) {
    assert.equal(storedNotes.split(note).length - 1, 1, 'Stored notes preserve each RSVP note once');
  }
  console.log(`PASS migration-growth overflow: normal ${normalBody.length} chars -> migrated ${migrated.body.length} chars -> overflow representation`);
}

(async () => {
  await overflowLifecycle();
  await migrationGrowthOverflowLifecycle();
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
  for (const file of [
    'src/lib.rs', 'crates/core/src/lib.rs', 'crates/discord/src/lib.rs',
    'crates/bot/src/main.rs', 'crates/cutover/src/lib.rs', 'crates/store/src/lib.rs',
    'crates/testsupport/src/lib.rs', 'wrangler/src/index.ts',
  ]) {
    assert(scopes.includes(file), `Retain release lifecycle coverage for ${file}`);
  }
  // Two tag modes for root, every member, worker, and three change types.
  const expectedLifecycleCount = 2 * (members.length + 5);
  assert(expectedLifecycleCount >= 18, 'Retain all existing workspace lifecycle coverage');
  assert.equal(bootstrapCount, expectedLifecycleCount, 'Exercise every bootstrap lifecycle case');
  assert.equal(postReleaseCount, expectedLifecycleCount, 'Exercise the actual next native release for every generated snapshot');
  console.log(`PASS ${bootstrapCount} bootstrap + ${postReleaseCount} generated post-release native lifecycles; 5 migration guards; 8 overflow guards; 4 snapshot guards; 1 overflow lifecycle`);
})().catch(error => { console.error(error.stack); process.exitCode = 1; });
