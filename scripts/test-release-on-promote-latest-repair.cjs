#!/usr/bin/env node
'use strict';

// Acceptance tests: repairing a missing GitHub Release for an older tag must
// not move the repository's "Latest" release backward.
//
// Break each test catches:
// - older repair: `gh release create <old> --latest` (or omitting the flag
//   and letting GitHub's automatic date/version default pick the fresh
//   repair) demotes the newer release. The repair must *explicitly* opt out
//   of latest (e.g. `--latest=false`), or explicitly re-mark the newer
//   release as latest afterwards.
// - newest repair: the highest semver tag must stay eligible for latest.
//   v0.10.0 vs v0.9.0 also pins numeric (not lexicographic) ordering.
// - no-op / rollback / dry-run: the existing guards must not regress while
//   the latest handling changes.
//
// Runs the real release-on-promote.cjs against a throwaway git repository
// with only `gh` mocked (a PATH shim recording argv). No network, no live
// tag/release mutation.

const assert = require('node:assert/strict');
const { execFileSync, spawnSync } = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { test, before, after } = require('node:test');

const SCRIPT =
  process.env.RELEASE_ON_PROMOTE ||
  path.join(__dirname, '..', '.github', 'scripts', 'release-on-promote.cjs');

const sections = [
  { type: 'feat', section: 'Added' },
  { type: 'fix', section: 'Fixed' },
  { type: 'chore', section: 'Miscellaneous', hidden: true },
];

let dir;
let binDir;
let shas;

const MOCK_GH = `#!/usr/bin/env node
'use strict';
const fs = require('node:fs');
const args = process.argv.slice(2);
fs.appendFileSync(process.env.GH_LOG, JSON.stringify(args) + '\\n');
const existing = (process.env.GH_EXISTING || '').split(',').filter(Boolean);
const latest = process.env.GH_LATEST || '';
const allTags = (process.env.GH_ALL_TAGS || '').split(',').filter(Boolean);
const has = (t) => existing.includes(t);
const relJson = (t) => ({
  tagName: t, name: t, isDraft: false, isPrerelease: false,
  isLatest: t === latest, createdAt: '2026-10-10T00:00:00Z',
  publishedAt: '2026-10-10T00:00:00Z', url: 'https://github.com/o/r/releases/tag/' + t,
});
if (args[0] === 'release' && args[1] === 'view') {
  const tag = args.slice(2).find((a) => /^v\\d/.test(a)) || latest;
  if (!tag || !has(tag)) process.exit(1);
  if (args.includes('--json')) process.stdout.write(JSON.stringify(relJson(tag)));
  process.exit(0);
}
if (args[0] === 'release' && args[1] === 'list') {
  if (args.includes('--json')) {
    process.stdout.write(JSON.stringify(existing.map(relJson)));
  } else {
    process.stdout.write(existing.map((t) => t + '\\t' + (t === latest ? 'Latest' : '')).join('\\n'));
  }
  process.exit(0);
}
if (args[0] === 'api') {
  const route = args.find((a) => a.includes('/releases') || a.includes('/tags')) || '';
  if (route.endsWith('/releases/latest')) {
    if (!latest) process.exit(1);
    process.stdout.write(JSON.stringify({ tag_name: latest, draft: false, prerelease: false }));
    process.exit(0);
  }
  const byTag = /\\/releases\\/tags\\/(v[\\d.]+)/.exec(route);
  if (byTag) {
    if (!has(byTag[1])) process.exit(1);
    process.stdout.write(JSON.stringify({ tag_name: byTag[1], draft: false, prerelease: false }));
    process.exit(0);
  }
  if (route.endsWith('/tags')) {
    process.stdout.write(JSON.stringify(allTags.map((name) => ({ name }))));
    process.exit(0);
  }
  if (route.includes('/releases')) {
    process.stdout.write(JSON.stringify(existing.map((t) => ({ tag_name: t, draft: false, prerelease: false }))));
    process.exit(0);
  }
  process.exit(0);
}
// release create / edit / anything else: record-only success.
process.exit(0);
`;

before(() => {
  dir = fs.mkdtempSync(path.join(os.tmpdir(), 'release-latest-repair-'));
  binDir = path.join(dir, 'bin');
  fs.mkdirSync(binDir);
  fs.writeFileSync(path.join(binDir, 'gh'), MOCK_GH, { mode: 0o755 });

  const g = (...a) => execFileSync('git', ['-C', dir, ...a], { encoding: 'utf8' }).trim();
  g('init', '-q', '-b', 'main');
  g('config', 'user.email', 't@example.invalid');
  g('config', 'user.name', 't');
  fs.writeFileSync(
    path.join(dir, 'release-please-config.json'),
    JSON.stringify({ packages: { '.': { 'bump-minor-pre-major': true, 'changelog-sections': sections } } }),
  );
  const commit = (msg) => {
    g('commit', '-q', '--allow-empty', '-m', msg);
    return g('rev-parse', 'HEAD');
  };
  g('add', '.');
  const base = commit('chore(main): release 0.4.0 (#1)');
  g('tag', 'v0.4.0', base);
  const mid = commit('fix(voice): untagged work (#2)');
  const older = commit('feat(voice): older tagged work (#3)');
  g('tag', 'v0.9.0', older);
  const newest = commit('feat(voice): newest tagged work (#4)');
  g('tag', 'v0.10.0', newest);
  g('update-ref', 'refs/remotes/origin/main', newest);
  shas = { base, mid, older, newest };
});

after(() => {
  fs.rmSync(dir, { recursive: true, force: true });
});

function run(sha, { existing = [], latest = '', dryRun = false } = {}) {
  const log = path.join(dir, `gh-${Date.now()}-${Math.random().toString(36).slice(2)}.log`);
  fs.writeFileSync(log, '');
  const out = `${log}.out`;
  fs.writeFileSync(out, '');
  const res = spawnSync('node', [SCRIPT], {
    cwd: dir,
    encoding: 'utf8',
    env: {
      ...process.env,
      PATH: `${binDir}${path.delimiter}${process.env.PATH}`,
      RELEASE_SHA: sha,
      GH_REPO: 'o/r',
      DRY_RUN: dryRun ? 'true' : 'false',
      GITHUB_OUTPUT: out,
      RUNNER_TEMP: dir,
      GH_LOG: log,
      GH_EXISTING: existing.join(','),
      GH_LATEST: latest,
      GH_ALL_TAGS: 'v0.4.0,v0.9.0,v0.10.0',
    },
  });
  const calls = fs
    .readFileSync(log, 'utf8')
    .split('\n')
    .filter(Boolean)
    .map((l) => JSON.parse(l));
  return { ...res, calls, out: fs.readFileSync(out, 'utf8') };
}

const creates = (calls) => calls.filter((a) => a[0] === 'release' && a[1] === 'create');
const mutating = (calls) =>
  calls.filter(
    (a) =>
      (a[0] === 'release' && (a[1] === 'create' || a[1] === 'edit' || a[1] === 'delete')) ||
      (a[0] === 'api' && (a.includes('PATCH') || a.includes('POST') || a.includes('-X'))),
  );

// What latest directive does one gh invocation carry? 'true' | 'false' | null
// (flag absent: GitHub's automatic default may pick the fresh repair).
function latestDirective(args) {
  const joined = args.join('\n');
  if (/--latest=false\b/.test(joined) || /make_latest[=:]false/.test(joined)) return 'false';
  if (args.includes('--latest') || /--latest=true\b/.test(joined) || /make_latest[=:]true/.test(joined))
    return 'true';
  return null;
}

// A later call that explicitly re-marks `tag` as latest also preserves it.
function remarksLatest(calls, tag, afterIdx) {
  return calls.slice(afterIdx + 1).some((a) => a.includes(tag) && latestDirective(a) === 'true');
}

test('repairing an older missing release explicitly keeps the newer Latest', () => {
  const r = run(shas.older, { existing: ['v0.10.0'], latest: 'v0.10.0' });
  assert.equal(r.status, 0, r.stderr);
  const cs = creates(r.calls);
  assert.equal(cs.length, 1, `older tag must still be repaired once; gh calls: ${JSON.stringify(r.calls)}`);
  const create = cs[0];
  assert.ok(create.includes('v0.9.0'), 'creates the older tag release');
  const ti = create.indexOf('--target');
  assert.equal(create[ti + 1], shas.older, 'targets the already-tagged older commit');
  assert.notEqual(
    latestDirective(create),
    'true',
    `older repair must not be marked latest; got: ${create.join(' ')}`,
  );
  assert.ok(
    latestDirective(create) === 'false' || remarksLatest(r.calls, 'v0.10.0', r.calls.indexOf(create)),
    'older repair must explicitly preserve the newer Latest ' +
      '(--latest=false on create, or an explicit re-mark of v0.10.0); ' +
      `merely omitting the flag is not enough. gh calls: ${JSON.stringify(r.calls)}`,
  );
  assert.match(r.stdout, /## \[0\.9\.0\]/, 'existing release notes are kept');
  assert.match(r.out, /tag_name=v0\.9\.0/);
  assert.match(r.out, /released=true/);
});

test('repairing the newest missing release keeps it eligible for latest', () => {
  const r = run(shas.newest, { existing: ['v0.9.0'], latest: 'v0.9.0' });
  assert.equal(r.status, 0, r.stderr);
  const cs = creates(r.calls);
  assert.equal(cs.length, 1, `gh calls: ${JSON.stringify(r.calls)}`);
  const create = cs[0];
  assert.ok(create.includes('v0.10.0'), 'creates the newest tag release (0.10 > 0.9 numerically)');
  const ti = create.indexOf('--target');
  assert.equal(create[ti + 1], shas.newest);
  assert.ok(
    latestDirective(create) === 'true' || remarksLatest(r.calls, 'v0.10.0', r.calls.indexOf(create)),
    `newest release must be marked latest; got: ${create.join(' ')}`,
  );
  assert.match(r.out, /tag_name=v0\.10\.0/);
  assert.match(r.out, /released=true/);
});

test('an existing release is an idempotent no-op (no create, no latest churn)', () => {
  const r = run(shas.older, { existing: ['v0.9.0', 'v0.10.0'], latest: 'v0.10.0' });
  assert.equal(r.status, 0, r.stderr);
  assert.equal(creates(r.calls).length, 0, `gh calls: ${JSON.stringify(r.calls)}`);
  assert.equal(mutating(r.calls).length, 0, 'no mutating gh call when the release already exists');
  assert.match(r.out, /tag_name=v0\.9\.0/);
  assert.match(r.out, /released=true/);
});

test('untagged rollback commit still gets no new tag or release', () => {
  const r = run(shas.mid, { existing: ['v0.10.0'], latest: 'v0.10.0' });
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stdout, /rollback promote, no new tag/);
  assert.equal(mutating(r.calls).length, 0, `gh calls: ${JSON.stringify(r.calls)}`);
  assert.match(r.out, /tag_name=\n/);
  assert.match(r.out, /released=false/);
});

test('dry run publishes nothing, even for a repairable older tag', () => {
  const r = run(shas.older, { existing: ['v0.10.0'], latest: 'v0.10.0', dryRun: true });
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stdout, /## \[0\.9\.0\]/);
  assert.equal(mutating(r.calls).length, 0, `gh calls: ${JSON.stringify(r.calls)}`);
  assert.match(r.out, /released=false/);
});
