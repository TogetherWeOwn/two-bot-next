'use strict';

// Real PR #60 notes with the template outside the native notes delimiters.
// Only the GitHub transport is mocked; parsing and publication use the pin.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const {Manifest, setLogger} = require('release-please');
const library = path.dirname(require.resolve('release-please/package.json'));
const {PullRequestBody} = require(path.join(library, 'build/src/util/pull-request-body'));
const {migrateReleaseNotes} = require('./migrate-release-notes.cjs');
const root = path.resolve(__dirname, '..');
const read = file => fs.readFileSync(path.join(root, file), 'utf8');
const config = JSON.parse(read('release-please-config.json'));
const body = read('scripts/fixtures/first-release-pr-body.md');
const header = config.packages['.']['pull-request-header'];
const logger = {info() {}, debug() {}, warn() {}, error() {}};
setLogger(logger);
assert.equal(require('release-please/package.json').version, '17.6.0');
const [fixtureHeader, notes, footer] = body.split('\n---');
assert.equal(fixtureHeader, header);
const expectedNotes = notes.trim();
assert.match(expectedNotes, /^## \[0\.2\.0\]\(https:\/\/github\.com\/TogetherWeOwn\/two-bot-next\/releases\/tag\/v0\.2\.0\)/);
assert.equal(footer.trim(), 'Refs: TOG-9865');
for (const section of ['Summary', 'Changes', 'Testing']) {
  assert(header.includes(`## ${section}\n`));
  assert(!expectedNotes.includes(`## ${section}\n`), 'Template must not enter published notes');
}
for (const section of ['Added', 'Fixed', 'Notes']) assert(expectedNotes.includes(`### ${section}\n`));
const historicalNotes = read('scripts/fixtures/bootstrap-changelog.md').split('\n').filter(line => line.startsWith('- '));
for (const note of historicalNotes) assert(expectedNotes.includes(note));

const changelog = `# Changelog\n\n${expectedNotes}\n`;
assert.deepEqual(migrateReleaseNotes(changelog, body), {changelog, body}, 'Corrected template-bearing pair is retry-idempotent');
const seedLink = 'https://github.com/TogetherWeOwn/two-bot-next/compare/v0.1.0...v0.2.0';
const releaseLink = 'https://github.com/TogetherWeOwn/two-bot-next/releases/tag/v0.2.0';
assert.deepEqual(migrateReleaseNotes(changelog.replace(releaseLink, seedLink), body.replace(releaseLink, seedLink)), {changelog, body}, 'Native regeneration must not restore an unpublished-seed comparison');
assert.deepEqual(migrateReleaseNotes(changelog, body.replace(releaseLink, seedLink)), {changelog, body}, 'Retry repairs the body independently');
assert.deepEqual(migrateReleaseNotes(changelog.replace(releaseLink, seedLink), body), {changelog, body}, 'Retry repairs the changelog independently');
assert.throws(() => migrateReleaseNotes(changelog, body.replace('## [0.2.0]', '## [0.3.0]')), /missing from PR body/, 'A different release still fails closed');

let mergedBody = body;
const files = {
  'release-please-config.json': JSON.stringify(config),
  '.release-please-manifest.json': JSON.stringify({'.': '0.2.0'}),
  'Cargo.toml': read('Cargo.toml'),
};
const github = new Proxy({
  repository: {owner: 'TogetherWeOwn', repo: 'two-bot-next'},
  async getFileJson(file) { assert(file in files); return JSON.parse(files[file]); },
  async getFileContentsOnBranch(file) {
    assert(file in files, `Unexpected file: ${file}`);
    return {content: Buffer.from(files[file]).toString('base64'), parsedContent: files[file], sha: 'fixture-content'};
  },
  async *releaseIterator() {},
  async *tagIterator() {},
  async *pullRequestIterator() {
    yield {
      number: 60, title: 'chore(main): release 0.2.0', body: mergedBody,
      headBranchName: 'release-please--branches--main--components--two-bot-next',
      baseBranchName: 'main', labels: ['autorelease: pending'],
      sha: 'merged-fixture', files: ['.release-please-manifest.json', 'Cargo.toml', 'Cargo.lock', 'CHANGELOG.md'],
    };
  },
}, {
  get(target, key) { assert(key in target, `Unmocked GitHub operation: ${String(key)}`); return target[key]; },
});

(async () => {
  const parsed = PullRequestBody.parse(body, logger);
  assert.equal(parsed.releaseData.length, 1);
  assert.equal(parsed.releaseData[0].version.toString(), '0.2.0');
  const manifest = await Manifest.fromManifest(github, 'main', undefined, undefined, {logger});
  const releases = await manifest.buildReleases();
  assert.equal(releases.length, 1);
  const [release] = releases;
  assert.equal(release.tag.toString(), 'v0.2.0');
  assert.equal(release.notes, expectedNotes, 'Native publication must retain the entire real notes region');
  for (const note of historicalNotes) assert.equal(release.notes.split(note).length - 1, 1);
  // Reproduce the adverse-review input: template ahead of version INSIDE notes.
  const robot = header.split('\n\n')[0];
  const template = header.slice(robot.length).trim();
  mergedBody = `${robot}\n---\n\n${template}\n\n${expectedNotes}\n\n---${footer}`;
  assert.equal(PullRequestBody.parse(mergedBody, logger).releaseData.length, 0);
  const [broken] = await manifest.buildReleases();
  assert.equal(broken.notes, '', 'Negative control reproduces the original empty-publication bug');
  console.log(`PASS real template-bearing publication: 1 v0.2.0 payload, ${Buffer.byteLength(release.notes)} notes bytes, historical notes once; misplaced-template negative control empty; retry and seed-link repair idempotent`);
})().catch(error => { console.error(error.stack); process.exitCode = 1; });
