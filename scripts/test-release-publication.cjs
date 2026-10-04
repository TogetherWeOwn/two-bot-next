'use strict';

// Real PR #60 notes with the template outside the native notes delimiters.
// Only the GitHub transport is mocked; parsing and publication use the pin.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const {Manifest, setLogger} = require('release-please');
const library = path.dirname(require.resolve('release-please/package.json'));
const {PullRequestBody} = require(path.join(library, 'build/src/util/pull-request-body'));
const {migrateReleaseNotes, parseOverflowLink, resolveNotesBody, buildOverflowBody, NATIVE_NOTES_BRANCH} = require('./migrate-release-notes.cjs');
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
const SECTIONS = ['Thinking Path', 'Linked Issues or Issue Description', 'What Changed', 'Verification', 'Risks', 'Model Used', 'Checklist'];
assert.equal(SECTIONS.length, 7);
for (const section of SECTIONS) {
  assert.equal(header.split(`## ${section}\n`).length - 1, 1, `Header must carry exactly one ## ${section}`);
  assert(!expectedNotes.includes(`## ${section}\n`), 'Template must not enter published notes');
}
// Overflow canonical form: native stores this same full body in release-notes.md
// and leaves a single-line link visible. The stored body must carry the same
// seven sections before its first delimiter and parse to the same payload.
const overflowVisible = buildOverflowBody('TogetherWeOwn/two-bot-next', NATIVE_NOTES_BRANCH);
assert(!overflowVisible.includes('\n'), 'Overflow visible body is a single line');
assert.deepEqual(parseOverflowLink(overflowVisible).branchName, NATIVE_NOTES_BRANCH);
const overflowStored = body;
const overflowResolved = resolveNotesBody(overflowVisible, () => overflowStored);
assert.equal(overflowResolved, overflowStored);
for (const section of SECTIONS) {
  assert.equal(overflowResolved.split(`## ${section}\n`).length - 1, 1, `Stored overflow body must carry ## ${section}`);
}
assert.deepEqual(
  PullRequestBody.parse(overflowResolved, logger).releaseData.map(data => data.version.toString()),
  PullRequestBody.parse(body, logger).releaseData.map(data => data.version.toString()),
  'Overflow stored body must parse to the same release payload',
);
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
  // TOG-12931: a push to main runs the action with skip-github-pull-request, so
  // the merge of the release PR must still publish while no PR write exists.
  // Mirrors release-please-action v5.0.0 src/index.ts main(): releases unless
  // skip-github-release, pull requests unless skip-github-pull-request.
  const workflow = read('.github/workflows/release.yml');
  assert(workflow.includes("skip-github-pull-request: ${{ github.event_name == 'push' || steps.plan.outputs.reuse_pr == 'true' }}"), 'push must skip PR generation');
  assert(!workflow.includes('skip-github-release:'), 'publication must stay enabled on every event');
  const pushInputs = {skipGitHubRelease: undefined, skipGitHubPullRequest: true};
  const pushState = {labels: ['autorelease: pending'], created: [], comments: [], removed: [], added: []};
  const pushTarget = {
    ...github,
    async *pullRequestIterator() {
      yield {
        number: 60, title: 'chore(main): release 0.2.0', body,
        headBranchName: 'release-please--branches--main--components--two-bot-next',
        baseBranchName: 'main', labels: pushState.labels, sha: 'merged-fixture',
        files: ['.release-please-manifest.json', 'Cargo.toml', 'Cargo.lock', 'CHANGELOG.md'],
      };
    },
    async createRelease(candidate, options) {
      pushState.created.push({tag: candidate.tag.toString(), notes: candidate.notes, options});
      return {id: 1, name: candidate.name, tagName: candidate.tag.toString(), sha: candidate.sha, notes: candidate.notes, url: 'https://example.invalid/release', draft: false, uploadUrl: ''};
    },
    async commentOnIssue(comment, number) { pushState.comments.push([comment, number]); },
    async removeIssueLabels(labels, number) { pushState.removed.push([labels, number]); pushState.labels = pushState.labels.filter(label => !labels.includes(label)); },
    async addIssueLabels(labels, number) { pushState.added.push([labels, number]); pushState.labels = [...pushState.labels, ...labels]; },
  };
  // Only release publication is mocked: any PR/branch write is an unmocked operation and throws.
  const pushGithub = new Proxy(pushTarget, {
    get(target, key) { assert(key in target, `Unmocked GitHub operation: ${String(key)}`); return target[key]; },
  });
  const runAction = async inputs => {
    const loaded = await Manifest.fromManifest(pushGithub, 'main', undefined, undefined, {logger});
    const created = inputs.skipGitHubRelease ? [] : await loaded.createReleases();
    const prs = inputs.skipGitHubPullRequest ? [] : await loaded.createPullRequests();
    return {created, prs};
  };
  const pushRun = await runAction(pushInputs);
  assert.deepEqual(pushRun.prs, [], 'A push run must not regenerate the release PR');
  assert.equal(pushRun.created.length, 1, 'The release PR merge publishes exactly one release');
  assert.equal(pushRun.created[0].tagName, 'v0.2.0');
  assert.equal(pushState.created.length, 1);
  assert.equal(pushState.created[0].notes, expectedNotes, 'Push publication carries the full notes region');
  assert.deepEqual(pushState.removed, [[['autorelease: pending'], 60]]);
  assert.deepEqual(pushState.added, [[['autorelease: tagged'], 60]]);
  assert.equal(pushState.comments.length, 1);
  assert.deepEqual((await runAction(pushInputs)).created, [], 'A rerun after tagging publishes nothing');
  assert.equal(pushState.created.length, 1);
  // Negative control: without the skip the same run reaches for GitHub operations
  // beyond publication, which this fail-closed mock rejects, so the guard above
  // is what keeps a push from regenerating.
  await assert.rejects(() => runAction({skipGitHubRelease: true, skipGitHubPullRequest: false}), /Unmocked GitHub operation/);

  // Reproduce the adverse-review input: template ahead of version INSIDE notes.
  const robot = header.split('\n\n')[0];
  const template = header.slice(robot.length).trim();
  mergedBody = `${robot}\n---\n\n${template}\n\n${expectedNotes}\n\n---${footer}`;
  assert.equal(PullRequestBody.parse(mergedBody, logger).releaseData.length, 0);
  const [broken] = await manifest.buildReleases();
  assert.equal(broken.notes, '', 'Negative control reproduces the original empty-publication bug');
  console.log(`PASS real template-bearing publication: 1 v0.2.0 payload, ${Buffer.byteLength(release.notes)} notes bytes, historical notes once; misplaced-template negative control empty; retry and seed-link repair idempotent; push run publishes v0.2.0 once and cannot regenerate`);
})().catch(error => { console.error(error.stack); process.exitCode = 1; });
