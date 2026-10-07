'use strict';

// Offline fixture for .github/scripts/commit-parse-guard.cjs. Runs the exact
// release-please library the pinned action bundles; no token, no network.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const {execFileSync, spawnSync} = require('node:child_process');
const root = path.resolve(__dirname, '..');
const read = file => fs.readFileSync(path.join(root, file), 'utf8');
const guardPath = path.join(root, '.github/scripts/commit-parse-guard.cjs');
const {parseConventionalCommits} = require('release-please/build/src/commit');
const {
  findUnparseable, squashMessage, checkPullRequest, readExceptions, RELEASE_PR_HEAD_PREFIX,
} = require(guardPath);

assert.equal(require('release-please/package.json').version, '17.6.0');

const silent = {info() {}, debug() {}, warn() {}, error() {}};
const releaseNotesEntries = message => parseConventionalCommits([{sha: 'x', message, files: []}], silent);

// A body shaped like the PR template, with the prose that real PRs carry.
const NORMAL_BODY = [
  '## Thinking Path', '', '> - The bot reads its gateway session before it acts.', '',
  '## What Changed', '', '- Serialize the gateway tests (`checkpoint` lock).',
  '- Call sites use `connect(pool)` and `send(message)`.', '',
  '## Verification', '', '- `cargo test --workspace --locked` passed.', '',
  'Closes #12', '',
].join('\n');
// The shape that dropped a feat commit from the 0.4.0 notes: a line that
// starts with a word and "(" and nests a second "(" before the first ")".
const HOSTILE_BODY = ['## What Changed', '', 'Cause: serialized on a shared key.',
  "`pg_advisory_xact_lock(hashtextextended('gateway', 0))` holds it.", ''].join('\n');
const HOSTILE_BARE = 'from_env(pool.clone(), token)\n';
const TITLE = 'fix(gateway): serialize checkpoint locks';

// 1. The premise: release-please really drops the hostile squash commit, and
//    keeps the normal one.
const normalMessage = squashMessage(TITLE, 7, NORMAL_BODY);
const hostileMessage = squashMessage(TITLE, 7, HOSTILE_BODY);
assert.equal(releaseNotesEntries(normalMessage).length, 1, 'A normal squash commit yields a release entry');
assert.equal(releaseNotesEntries(hostileMessage).length, 0, 'The hostile squash commit is silently dropped');
assert.equal(squashMessage(TITLE, 7, ''), `${TITLE} (#7)`, 'An empty body leaves the header alone');

// 2. The PR gate catches the hostile body and passes the normal one.
assert.deepEqual(checkPullRequest({title: TITLE, number: '7', body: NORMAL_BODY, headRef: 'fix/gateway'}), []);
const caught = checkPullRequest({title: TITLE, number: '7', body: HOSTILE_BODY, headRef: 'fix/gateway'});
assert.equal(caught.length, 1);
assert.match(caught[0].error, /unexpected token '\(' at 6:\d+/, 'Reports the parser position in the squash message');
assert.equal(caught[0].subject, `${TITLE} (#7)`);
assert.equal(checkPullRequest({title: TITLE, number: '7', body: HOSTILE_BARE, headRef: 'fix/x'}).length, 1, 'No footer needed');
assert.deepEqual(checkPullRequest({title: TITLE, number: '7', body: `- ${HOSTILE_BARE}`, headRef: 'fix/x'}), [], 'A list marker fixes it');

// 3. Web-editor CRLF is checked as written and as normalized.
assert.deepEqual(checkPullRequest({title: TITLE, number: '7', body: NORMAL_BODY.replace(/\n/g, '\r\n'), headRef: 'fix/x'}), []);
assert.equal(checkPullRequest({title: TITLE, number: '7', body: HOSTILE_BODY.replace(/\n/g, '\r\n'), headRef: 'fix/x'}).length, 1);

// 4. release-please's own commit override decides what is parsed, as native does.
const rescue = `${HOSTILE_BODY}\nBEGIN_COMMIT_OVERRIDE\nfix(gateway): serialize checkpoint locks\nEND_COMMIT_OVERRIDE\n`;
assert.deepEqual(checkPullRequest({title: TITLE, number: '7', body: rescue, headRef: 'fix/x'}), []);
const poisoned = `${NORMAL_BODY}\nBEGIN_COMMIT_OVERRIDE\nfix(gateway): ok\n\n${HOSTILE_BARE}END_COMMIT_OVERRIDE\n`;
assert.equal(checkPullRequest({title: TITLE, number: '7', body: poisoned, headRef: 'fix/x'}).length, 1);

// 5. The release PR is release-please's own and is exempt.
assert.deepEqual(checkPullRequest({
  title: 'chore(main): release 0.4.0', number: '9', body: HOSTILE_BODY,
  headRef: `${RELEASE_PR_HEAD_PREFIX}main--components--two-bot-next`,
}), []);

// 6. The captured failure carries the full sha and the bare header.
const sample = findUnparseable([{sha: 'a'.repeat(40), message: hostileMessage}]);
assert.equal(sample.length, 1);
assert.equal(sample[0].sha, 'a'.repeat(40));
assert.equal(sample[0].subject, `${TITLE} (#7)`, 'The header carries no sha prefix');

// 7. Range mode over a throwaway repository: tag, normal commit, hostile commit.
const scratch = fs.mkdtempSync(path.join(process.env.RUNNER_TEMP || os.tmpdir(), 'parse-guard-'));
try {
  const env = {...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_SYSTEM: '/dev/null', GIT_TERMINAL_PROMPT: '0'};
  const git = (...args) => execFileSync('git', ['-C', scratch, '-c', 'user.name=fixture', '-c', 'user.email=fixture@example.invalid',
    '-c', 'commit.gpgsign=false', ...args], {encoding: 'utf8', env}).trim();
  const commit = message => {
    fs.writeFileSync(path.join(scratch, 'message.txt'), message);
    fs.writeFileSync(path.join(scratch, 'f.txt'), `${message.length}${Math.random()}`);
    git('add', 'f.txt');
    git('commit', '-q', '-F', 'message.txt');
    return git('rev-parse', 'HEAD');
  };
  git('init', '-q', '-b', 'main');
  const before = commit(squashMessage('feat(old): shipped before the tag', 1, HOSTILE_BODY));
  git('tag', 'v0.1.0');
  commit(squashMessage('feat(good): parses', 2, NORMAL_BODY));
  const bad = commit(squashMessage(TITLE, 3, HOSTILE_BODY));
  const exceptionsFile = path.join(scratch, 'exceptions.txt');
  const run = (extra = {}) => {
    const result = spawnSync(process.execPath, [guardPath, 'range'], {
      cwd: scratch, encoding: 'utf8',
      env: {...env, NODE_PATH: process.env.NODE_PATH, EXCEPTIONS_FILE: exceptionsFile, ...extra},
    });
    return {code: result.status, out: result.stdout, err: result.stderr};
  };

  const failing = run();
  assert.equal(failing.code, 1, 'An unparseable commit since the tag fails the run');
  assert.match(failing.out, new RegExp(`::error title=Unparseable commit::.*${bad.slice(0, 12)}`));
  assert.doesNotMatch(failing.out, new RegExp(before.slice(0, 12)), 'Commits before the last tag are out of range');
  assert.match(failing.out, /Parsed 2 commit\(s\) since v0\.1\.0/);

  fs.writeFileSync(exceptionsFile, `# reviewed\n\n${bad} dropped, note added by hand\n`);
  const acknowledged = run();
  assert.equal(acknowledged.code, 0, 'A recorded SHA is acknowledged, not failed');
  assert.match(acknowledged.out, /::warning title=Acknowledged unparseable commit::/, 'It stays visible as a warning');

  fs.writeFileSync(exceptionsFile, `${bad.slice(0, 12)} short sha\n`);
  assert.equal(run().code, 1, 'A malformed exceptions entry fails closed');

  fs.rmSync(exceptionsFile);
  const all = run({RELEASE_BASE_REF: ''});
  assert.equal(all.code, 1);
  assert.match(all.out, /Parsed 3 commit\(s\) since the first commit/, 'No tag means the whole history');
  assert.match(all.out, new RegExp(before.slice(0, 12)));
} finally {
  fs.rmSync(scratch, {recursive: true, force: true});
}

// 8. The repository's own exceptions file is well formed.
assert.doesNotThrow(() => readExceptions(path.join(root, '.github/release-parse-exceptions.txt')));

// 9. CLI: pr mode exits nonzero on the hostile body and zero on the normal one.
const bodyFile = path.join(os.tmpdir(), `parse-guard-body-${process.pid}.md`);
try {
  const cli = body => {
    fs.writeFileSync(bodyFile, body);
    return spawnSync(process.execPath, [guardPath, 'pr'], {
      encoding: 'utf8', env: {...process.env, TITLE, BODY_FILE: bodyFile, PR_NUMBER: '7', HEAD_REF: 'fix/x'},
    });
  };
  assert.equal(cli(NORMAL_BODY).status, 0);
  const rejected = cli(HOSTILE_BODY);
  assert.equal(rejected.status, 1);
  assert.match(rejected.stdout, /::error title=Unparseable squash commit::/);
  assert.match(rejected.stderr, /will drop this change from the release notes/);
} finally {
  fs.rmSync(bodyFile, {force: true});
}

// 10. Wiring: one pinned parser version everywhere, the guard runs where claimed.
const supply = read('.github/workflows/supply-chain.yml');
const release = read('.github/workflows/release.yml');
const check = read('.github/workflows/check.yml');
const installs = text => [...text.matchAll(/release-please@(\d+\.\d+\.\d+)/g)].map(match => match[1]);
for (const [name, text] of [['supply-chain', supply], ['release', release], ['check', check]]) {
  assert(installs(text).length >= 1 && installs(text).every(version => version === '17.6.0'), `${name} must install release-please@17.6.0`);
}
const prStep = supply.slice(supply.indexOf('- name: Check the squash commit parses for release notes'));
assert(prStep.includes("if: steps.pr.outputs.event == 'pull_request'"), 'The PR guard skips push events');
assert(prStep.slice(0, prStep.indexOf('\n  gitleaks:')).includes('commit-parse-guard.cjs pr'), 'pr-lint runs the PR guard');
assert(supply.indexOf('commit-parse-guard.cjs pr') < supply.indexOf('\n  gitleaks:'), 'The PR guard is inside the pr-lint job');
const guardStep = release.indexOf('- name: Fail on commits release-please cannot parse');
assert(guardStep > 0 && guardStep < release.indexOf('googleapis/release-please-action@'), 'The range guard precedes release-please');
const guardBlock = release.slice(guardStep, release.indexOf('- name: Inspect existing release PR'));
assert(guardBlock.includes("if: github.event_name != 'push'") && guardBlock.includes('commit-parse-guard.cjs range'));
assert(release.includes("fetch-depth: ${{ github.event_name == 'push' && 1 || 0 }}"), 'The range guard needs the tag history');
assert(release.indexOf('dispatch-checks:') > guardStep && /dispatch-checks:[\s\S]*?needs: release-please/.test(release),
  'dispatch-checks needs the job that runs the guard');
assert(read('docs/releases.md').includes('commit-parse-guard.cjs'), 'docs/releases.md documents the guard');

console.log('commit parse guard: ok');
