#!/usr/bin/env node
'use strict';

// Offline tests for release-on-promote.cjs: bump rules, note rendering, and a
// dry run against a throwaway git repository (no network, no gh calls).

const assert = require('node:assert/strict');
const { execFileSync } = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const SCRIPT = process.env.RELEASE_ON_PROMOTE || path.join(__dirname, '..', '.github', 'scripts', 'release-on-promote.cjs');
const { parseCommit, nextVersion, renderNotes } = require(SCRIPT);

const c = (subject, body = '') => parseCommit('a'.repeat(40), subject, body);

// Parsing.
assert.deepEqual(c('feat(voice): add rooms (#12)'),
  { sha: 'a'.repeat(40), type: 'feat', scope: 'voice', breaking: false, desc: 'add rooms', pr: 12 });
assert.equal(c('fix!: drop legacy flag').breaking, true);
assert.equal(c('fix: x', 'BREAKING CHANGE: removed y').breaking, true);
assert.equal(c('Merge branch main'), null);
assert.equal(c('feat(): empty scope'), null);

// Bumps (pre-1.0 with release-please's flags, then post-1.0).
const pre = { bumpMinorPreMajor: true, bumpPatchForMinorPreMajor: false };
assert.deepEqual(nextVersion([0, 4, 0], [c('fix: a')], pre), [0, 4, 1]);
assert.deepEqual(nextVersion([0, 4, 0], [c('fix: a'), c('feat: b')], pre), [0, 5, 0]);
assert.deepEqual(nextVersion([0, 4, 2], [c('feat!: b')], pre), [0, 5, 0]);
assert.deepEqual(nextVersion([0, 4, 2], [c('feat!: b')], { bumpMinorPreMajor: false }), [1, 0, 0]);
assert.deepEqual(nextVersion([0, 4, 2], [c('feat: b')], { bumpMinorPreMajor: true, bumpPatchForMinorPreMajor: true }), [0, 4, 3]);
assert.deepEqual(nextVersion([0, 4, 0], [c('chore: a')], pre), [0, 4, 1]);
assert.deepEqual(nextVersion([0, 4, 0], [], pre), [0, 4, 1]);
assert.deepEqual(nextVersion([1, 2, 3], [c('feat: a')], pre), [1, 3, 0]);
assert.deepEqual(nextVersion([1, 2, 3], [c('fix!: a')], pre), [2, 0, 0]);
assert.deepEqual(nextVersion([1, 2, 3], [c('perf: a')], pre), [1, 2, 4]);

// Notes: visible sections only, scope-sorted, hidden types dropped.
const sections = [
  { type: 'feat', section: 'Added' }, { type: 'fix', section: 'Fixed' },
  { type: 'security', section: 'Fixed' }, { type: 'chore', section: 'Miscellaneous', hidden: true },
];
const notes = renderNotes({
  version: [0, 5, 0], prevTag: 'v0.4.0', date: '2026-10-10', sections, repoUrl: 'https://github.com/o/r',
  commits: [c('fix(b): second (#2)'), c('feat(z): last (#3)'), c('fix(a): first'), c('chore: hidden (#9)'), c('security: patch')],
});
assert.match(notes, /^## \[0\.5\.0\]\(https:\/\/github\.com\/o\/r\/compare\/v0\.4\.0\.\.\.v0\.5\.0\) \(2026-10-10\)/);
assert.ok(notes.indexOf('### Added') < notes.indexOf('### Fixed'));
assert.ok(notes.indexOf('**a:** first') < notes.indexOf('**b:** second'));
assert.ok(notes.includes('* patch ([aaaaaaa]'));
assert.ok(notes.includes('([#2](https://github.com/o/r/issues/2))'));
assert.ok(!notes.includes('hidden'));
assert.ok(!notes.includes('BREAKING'));
assert.match(renderNotes({ version: [0, 4, 1], prevTag: 'v0.4.0', date: 'd', sections, repoUrl: 'u', commits: [c('chore: x')] }),
  /No user-facing changes/);
assert.match(renderNotes({ version: [0, 5, 0], prevTag: 'v0.4.0', date: 'd', sections, repoUrl: 'u', commits: [c('feat!: x')] }),
  /### ⚠ BREAKING CHANGES\n\n\* x/);

// Dry run on a real repository: version, rollback guard, off-main refusal.
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'release-on-promote-'));
const g = (...a) => execFileSync('git', ['-C', dir, ...a], { encoding: 'utf8' }).trim();
g('init', '-q', '-b', 'main');
g('config', 'user.email', 't@example.invalid');
g('config', 'user.name', 't');
fs.writeFileSync(path.join(dir, 'release-please-config.json'), JSON.stringify({
  packages: { '.': { 'bump-minor-pre-major': true, 'changelog-sections': sections } },
}));
const commit = (msg) => { g('commit', '-q', '--allow-empty', '-m', msg); return g('rev-parse', 'HEAD'); };
g('add', '.');
const first = commit('chore(main): release 0.4.0 (#1)');
g('tag', 'v0.4.0');
const fixSha = commit('fix(voice): retry renames (#2)');
const featSha = commit('feat(voice): name pools (#3)');
g('update-ref', 'refs/remotes/origin/main', featSha);
g('checkout', '-q', '-b', 'side', first);
const offMain = commit('feat: not on main');
g('checkout', '-q', 'main');

const run = (sha) => {
  const out = path.join(dir, `out-${sha.slice(0, 7)}`);
  fs.writeFileSync(out, '');
  const res = require('node:child_process').spawnSync('node', [SCRIPT], {
    cwd: dir, encoding: 'utf8',
    env: { ...process.env, RELEASE_SHA: sha, GH_REPO: 'o/r', DRY_RUN: 'true', GITHUB_OUTPUT: out },
  });
  return { ...res, out: fs.readFileSync(out, 'utf8') };
};
let r = run(featSha);
assert.equal(r.status, 0, r.stderr);
assert.match(r.stdout, /## \[0\.5\.0\]/);
assert.match(r.stdout, /### Added\n\n\* \*\*voice:\*\* name pools/);
assert.match(r.stdout, /### Fixed\n\n\* \*\*voice:\*\* retry renames/);
r = run(fixSha);
assert.equal(r.status, 0, r.stderr);
assert.match(r.stdout, /## \[0\.4\.1\]/);
g('tag', 'v0.5.0', featSha);
r = run(fixSha);
assert.equal(r.status, 0, r.stderr);
assert.match(r.stdout, /rollback promote, no new tag/);
r = run(featSha);
assert.equal(r.status, 0, r.stderr);
assert.match(r.stdout, /## \[0\.5\.0\]/, 'an already tagged commit keeps its tag');
r = run(offMain);
assert.notEqual(r.status, 0);
assert.match(r.stderr, /not on origin\/main/);
r = run('not-a-sha');
assert.notEqual(r.status, 0);
fs.rmSync(dir, { recursive: true, force: true });

console.log('release-on-promote: all tests passed');
