#!/usr/bin/env node
'use strict';

// Offline tests for daily-digest.cjs: parsing, note rendering, quiet-day skip,
// rollback guard, off-main refusal, invalid dates, and the 1 MiB ENOBUFS
// regression (a 200-commit range above 1 MiB must not crash).
//
// Dry runs against a throwaway git repository, plus one non-dry-run fresh cut
// against a mocked gh (no network, no live gh calls).

const assert = require('node:assert/strict');
const { execFileSync, spawnSync } = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const SCRIPT = process.env.DAILY_DIGEST || path.join(__dirname, '..', '.github', 'scripts', 'daily-digest.cjs');
const { parseCommit, parseDigestTag, parseTag, renderNotes, LOG_MAX_BUFFER } = require(SCRIPT);

const c = (subject, body = '') => parseCommit('a'.repeat(40), subject, body);

// Parsing (reused rendering).
assert.deepEqual(c('feat(voice): add rooms (#12)'),
  { sha: 'a'.repeat(40), type: 'feat', scope: 'voice', breaking: false, desc: 'add rooms', pr: 12 });
assert.equal(c('fix!: drop legacy flag').breaking, true);
assert.equal(c('fix: x', 'BREAKING CHANGE: removed y').breaking, true);
assert.equal(c('Merge branch main'), null);
assert.equal(c('feat(): empty scope'), null);

// Digest tag parsing: real calendar dates only.
assert.equal(parseDigestTag('v2026.10.21'), 'v2026.10.21');
assert.equal(parseDigestTag('v2026.02.29'), null, '2026 is not a leap year');
assert.equal(parseDigestTag('v2024.02.29'), 'v2024.02.29');
assert.equal(parseDigestTag('v2026.13.01'), null);
assert.equal(parseDigestTag('v1.2.3'), null, 'semver is not a digest');
assert.equal(parseDigestTag('v2026.1.1'), null, 'unpadded is not canonical');
assert.deepEqual(parseTag('v1.2.3'), [1, 2, 3]);

// The log buffer covers ~20x the 1.36 MB range that crashed the 1 MiB default.
assert.ok(LOG_MAX_BUFFER >= 20 * 1024 * 1024, `LOG_MAX_BUFFER=${LOG_MAX_BUFFER}`);

// Notes: digest title, visible sections only, scope-sorted, hidden dropped.
const sections = [
  { type: 'feat', section: 'Added' }, { type: 'fix', section: 'Fixed' },
  { type: 'security', section: 'Fixed' }, { type: 'chore', section: 'Miscellaneous', hidden: true },
];
const notes = renderNotes({
  tag: 'v2026.10.21', prevTag: 'v2026.10.20', date: '2026-10-21', sections, repoUrl: 'https://github.com/o/r',
  commits: [c('fix(b): second (#2)'), c('feat(z): last (#3)'), c('fix(a): first'), c('chore: hidden (#9)'), c('security: patch')],
});
assert.match(notes, /^## \[v2026\.10\.21\]\(https:\/\/github\.com\/o\/r\/compare\/v2026\.10\.20\.\.\.v2026\.10\.21\) \(2026-10-21\)/);
assert.ok(notes.indexOf('### Added') < notes.indexOf('### Fixed'));
assert.ok(notes.indexOf('**a:** first') < notes.indexOf('**b:** second'));
assert.ok(notes.includes('* patch ([aaaaaaa]'));
assert.ok(notes.includes('([#2](https://github.com/o/r/issues/2))'));
assert.ok(!notes.includes('hidden'));
assert.ok(!notes.includes('BREAKING'));
assert.match(renderNotes({ tag: 'v2026.10.21', prevTag: 'v2026.10.20', date: 'd', sections, repoUrl: 'u', commits: [c('chore: x')] }),
  /No user-facing changes/);
assert.match(renderNotes({ tag: 'v2026.10.21', prevTag: 'v2026.10.20', date: 'd', sections, repoUrl: 'u', commits: [c('feat!: x')] }),
  /### ⚠ BREAKING CHANGES\n\n\* x/);

// Dry runs on a real repository: fresh cut, quiet day, rollback guard,
// already-cut today, semver continuity, off-main refusal.
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'daily-digest-'));
const g = (...a) => execFileSync('git', ['-C', dir, ...a], { encoding: 'utf8' }).trim();
g('init', '-q', '-b', 'main');
g('config', 'user.email', 't@example.invalid');
g('config', 'user.name', 't');
fs.writeFileSync(path.join(dir, 'release-please-config.json'), JSON.stringify({
  packages: { '.': { 'changelog-sections': sections } },
}));
const commit = (msg, body = '') => {
  const args = ['commit', '-q', '--allow-empty', '-m', msg];
  if (body) args.push('-m', body);
  g(...args);
  return g('rev-parse', 'HEAD');
};
g('add', '.');
const first = commit('chore(main): release 0.4.0 (#1)');
g('tag', 'v0.4.0');
const fixSha = commit('fix(voice): retry renames (#2)');
g('tag', 'v2026.10.20', fixSha);
const featSha = commit('feat(voice): name pools (#3)');
g('update-ref', 'refs/remotes/origin/main', featSha);
g('checkout', '-q', '-b', 'side', first);
const offMain = commit('feat: not on main');
g('checkout', '-q', 'main');

const run = (sha, extra = {}) => {
  const out = path.join(dir, `out-${sha.slice(0, 7)}-${Math.random().toString(36).slice(2)}`);
  fs.writeFileSync(out, '');
  const res = spawnSync('node', [SCRIPT], {
    cwd: dir, encoding: 'utf8',
    env: {
      ...process.env, PROD_SHA: sha, GH_REPO: 'o/r', DRY_RUN: 'true',
      DIGEST_DATE: '2026-10-21', GITHUB_OUTPUT: out, ...extra,
    },
  });
  return { ...res, out: fs.readFileSync(out, 'utf8') };
};
let r = run(featSha);
assert.equal(r.status, 0, r.stderr);
assert.match(r.stdout, /## \[v2026\.10\.21\]/);
assert.match(r.stdout, /### Added\n\n\* \*\*voice:\*\* name pools/);
assert.match(r.stdout, /prod_changed=true/);
assert.match(r.out, /prod_changed=true/);
assert.match(r.out, /released=false/);
// Quiet day: production already matches the last digest.
g('tag', 'v2026.10.21', featSha);
r = run(featSha);
assert.equal(r.status, 0, r.stderr);
assert.match(r.stdout, /already exists|quiet day/, 'today cut or quiet skips');
assert.match(r.out, /prod_changed=false/);
g('tag', '-d', 'v2026.10.21');
// Rollback: production serves an older commit than the last digest.
r = run(first);
assert.equal(r.status, 0, r.stderr);
assert.match(r.stdout, /rollback serving, no new tag/);
assert.match(r.out, /prod_changed=false/);
// Off-main and invalid SHAs refuse.
r = run(offMain);
assert.notEqual(r.status, 0);
assert.match(r.stderr, /not on origin\/main/);
r = run('deadbeef');
assert.notEqual(r.status, 0);
assert.match(r.stderr, /PROD_SHA must be a full 40-character/);
r = run(featSha, { DIGEST_DATE: '21-10-2026' });
assert.notEqual(r.status, 0);
assert.match(r.stderr, /DIGEST_DATE must be YYYY-MM-DD/);
// Semver continuity: with no digest tag, notes range from the latest vX.Y.Z.
g('tag', '-d', 'v2026.10.20');
r = run(featSha);
assert.equal(r.status, 0, r.stderr);
assert.match(r.stdout, /compare\/v0\.4\.0\.\.\.v2026\.10\.21/);
g('tag', 'v2026.10.20', fixSha);

// ENOBUFS regression: a 200-commit range above 1 MiB must not crash.
// Node's default maxBuffer is 1 MiB; the retired script crashed on a 1.36 MB
// range. Each commit carries a 7 KiB body, so the range exceeds 1.4 MB.
const big = fs.mkdtempSync(path.join(os.tmpdir(), 'daily-digest-big-'));
const gb = (...a) => execFileSync('git', ['-C', big, ...a], { encoding: 'utf8' }).trim();
gb('init', '-q', '-b', 'main');
gb('config', 'user.email', 't@example.invalid');
gb('config', 'user.name', 't');
fs.writeFileSync(path.join(big, 'release-please-config.json'), JSON.stringify({
  packages: { '.': { 'changelog-sections': sections } },
}));
gb('add', '.');
gb('commit', '-q', '--allow-empty', '-m', 'chore: seed');
gb('tag', 'v2026.10.20');
const filler = 'x'.repeat(7 * 1024);
for (let i = 0; i < 200; i += 1) {
  gb('commit', '-q', '--allow-empty', '-m', `fix(scope): change ${i} (#${1000 + i})`, '-m', filler);
}
const bigHead = gb('rev-parse', 'HEAD');
gb('update-ref', 'refs/remotes/origin/main', bigHead);
const raw = execFileSync('git', ['-C', big, 'log', '--format=%H%x1f%s%x1f%b%x1e', 'v2026.10.20..HEAD'],
  { encoding: 'utf8', maxBuffer: LOG_MAX_BUFFER });
assert.ok(Buffer.byteLength(raw, 'utf8') > 1024 * 1024,
  `regression range must exceed 1 MiB, got ${Buffer.byteLength(raw, 'utf8')}`);
const bigOut = path.join(big, 'out-big');
fs.writeFileSync(bigOut, '');
const bigRes = spawnSync('node', [SCRIPT], {
  cwd: big, encoding: 'utf8',
  env: {
    ...process.env, PROD_SHA: bigHead, GH_REPO: 'o/r', DRY_RUN: 'true',
    DIGEST_DATE: '2026-10-21', GITHUB_OUTPUT: bigOut,
  },
});
assert.equal(bigRes.status, 0, bigRes.stderr);
assert.match(bigRes.stdout, /## \[v2026\.10\.21\]/);
assert.match(fs.readFileSync(bigOut, 'utf8'), /prod_changed=true/);

// Non-dry-run fresh cut against a mocked gh: pins the single release create
// call's target and its --latest flag.
const mockBin = fs.mkdtempSync(path.join(os.tmpdir(), 'daily-digest-gh-'));
const ghLog = path.join(mockBin, 'gh-calls.log');
fs.writeFileSync(path.join(mockBin, 'gh'),
  `#!/bin/sh\necho "$@" >> "${ghLog}"\nif [ "$1" = "release" ] && [ "$2" = "view" ]; then exit 1; fi\nexit 0\n`);
fs.chmodSync(path.join(mockBin, 'gh'), 0o755);
const cutOut = path.join(dir, 'out-cut');
fs.writeFileSync(cutOut, '');
const cut = spawnSync('node', [SCRIPT], {
  cwd: dir, encoding: 'utf8',
  env: {
    ...process.env, PROD_SHA: featSha, GH_REPO: 'o/r', DIGEST_DATE: '2026-10-21',
    GITHUB_OUTPUT: cutOut, RUNNER_TEMP: dir,
    PATH: `${mockBin}${path.delimiter}${process.env.PATH}`,
  },
});
assert.equal(cut.status, 0, cut.stderr);
const calls = fs.readFileSync(ghLog, 'utf8');
assert.match(calls, new RegExp(`release create v2026\\.10\\.21 --target ${featSha} --title v2026\\.10\\.21 --notes-file .* --latest`));
assert.match(fs.readFileSync(cutOut, 'utf8'), /tag_name=v2026\.10\.21/);
assert.match(fs.readFileSync(cutOut, 'utf8'), /released=true/);

// Repair of a missing Release runs before the quiet-day skip, even when
// production already serves the tag's SHA, and ranges its notes from the
// previous digest rather than the tag against itself.
g('tag', 'v2026.10.21', featSha);
fs.writeFileSync(ghLog, '');
const repairOut = path.join(dir, 'out-repair');
fs.writeFileSync(repairOut, '');
const repair = spawnSync('node', [SCRIPT], {
  cwd: dir, encoding: 'utf8',
  env: {
    ...process.env, PROD_SHA: featSha, GH_REPO: 'o/r', DIGEST_DATE: '2026-10-21',
    GITHUB_OUTPUT: repairOut, RUNNER_TEMP: dir,
    PATH: `${mockBin}${path.delimiter}${process.env.PATH}`,
  },
});
assert.equal(repair.status, 0, repair.stderr);
assert.match(repair.stdout, /compare\/v2026\.10\.20\.\.\.v2026\.10\.21/);
assert.doesNotMatch(repair.stdout, /compare\/v2026\.10\.21\.\.\.v2026\.10\.21/);
const repairCalls = fs.readFileSync(ghLog, 'utf8');
assert.match(repairCalls, new RegExp(`release create v2026\\.10\\.21 --target ${featSha} --title v2026\\.10\\.21 --notes-file .* --latest`));
assert.match(fs.readFileSync(repairOut, 'utf8'), /tag_name=v2026\.10\.21/);
assert.match(fs.readFileSync(repairOut, 'utf8'), /released=true/);

console.log('daily-digest: all tests passed');
