#!/usr/bin/env node
'use strict';

// Offline tests for the daily-digest adapter around the shared CalVer
// releaser: the plan request, skip classification, completed replays, the
// unfinished-today repair, owed-digest outputs, SBOM redispatch, the
// stalled-production warning and dry runs. gh is a fixture mock on PATH; no
// network and no live gh calls.

const assert = require('node:assert/strict');
const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const SCRIPT = process.env.DAILY_DIGEST || path.join(__dirname, '..', '.github', 'scripts', 'daily-digest.cjs');
const { SBOM_ASSETS, assetsMissing } = require(SCRIPT);

const SHA = 'b'.repeat(40);
const dir = fs.mkdtempSync(path.join(process.env.PAPERCLIP_RUN_SCRATCH_DIR || os.tmpdir(), 'daily-digest-'));
const ghLog = path.join(dir, 'gh-calls.log');
const fixtures = path.join(dir, 'fixtures.json');
// The mock answers an exact argument string from the fixture map; anything
// else fails like a 404 or transport error.
fs.writeFileSync(path.join(dir, 'gh'), `#!${process.execPath}
const fs = require('node:fs');
const key = process.argv.slice(2).join(' ');
fs.appendFileSync(${JSON.stringify(ghLog)}, key + '\\n');
const reply = JSON.parse(fs.readFileSync(${JSON.stringify(fixtures)}, 'utf8'))[key];
if (reply === undefined) process.exit(1);
process.stdout.write(typeof reply === 'string' ? reply : JSON.stringify(reply));
`);
fs.chmodSync(path.join(dir, 'gh'), 0o755);

const LATEST = 'release view --json tagName,assets,publishedAt';
const VIEW = tag => `release view ${tag} --json tagName,assets,publishedAt`;
const AHEAD = `api repos/o/r/compare/${SHA}...main --jq .ahead_by`;
const today = new Date();
const TODAY = `v${today.getUTCFullYear()}.${today.getUTCMonth() + 1}${String(today.getUTCDate()).padStart(2, '0')}.0`;
const REFS = `api repos/o/r/git/matching-refs/tags/${TODAY.slice(0, -1)}`;

function run(mode, env = {}, gh = {}) {
  const out = path.join(dir, 'outputs');
  const sum = path.join(dir, 'summary');
  fs.writeFileSync(out, '');
  fs.writeFileSync(sum, '');
  fs.writeFileSync(ghLog, '');
  fs.writeFileSync(fixtures, JSON.stringify(gh));
  // Never inherit a host credential, GITHUB_OUTPUT or DRY_RUN.
  const result = spawnSync('node', [SCRIPT, mode], {
    encoding: 'utf8',
    env: {
      PATH: `${dir}${path.delimiter}${process.env.PATH}`, HOME: dir,
      GH_REPO: 'o/r', GITHUB_OUTPUT: out, GITHUB_STEP_SUMMARY: sum, PROD_SHA: SHA, ...env,
    },
  });
  const outputs = Object.fromEntries(fs.readFileSync(out, 'utf8').split('\n').filter(Boolean)
    .map(line => [line.slice(0, line.indexOf('=')), line.slice(line.indexOf('=') + 1)]));
  const calls = fs.readFileSync(ghLog, 'utf8').split('\n').filter(Boolean);
  return { ...result, outputs, calls, summary: fs.readFileSync(sum, 'utf8') };
}

const full = SBOM_ASSETS.map(name => ({ name }));
const release = (tagName, extra = {}) => ({ tagName, publishedAt: new Date().toISOString(), assets: full, ...extra });
const partial = [{ name: 'SHA256SUMS' }];
const old = new Date(Date.now() - 8 * 24 * 60 * 60 * 1000).toISOString();

// Asset completeness.
assert.equal(assetsMissing({ assets: full }), false);
assert.equal(assetsMissing({ assets: partial }), true);
assert.equal(assetsMissing({}), true);

// request: the live-digest plan binds the SHA as its operation, so a retry
// for the same build recovers the frozen plan. It also carries the bot's
// changelog sections from release-please-config.json, so the shared
// releaser keeps the Common Changelog headings (security under Fixed).
const CONFIG_SECTIONS = JSON.parse(
  fs.readFileSync(path.join(__dirname, '..', 'release-please-config.json'), 'utf8'),
).packages['.']['changelog-sections'];
let r = run('request');
assert.equal(r.status, 0, r.stderr);
assert.deepEqual(JSON.parse(r.outputs.request), {
  phase: 'plan',
  input: {
    channel: 'stable', policy: 'live-digest', sha: SHA,
    operation: `daily-digest/${SHA}`, sections: CONFIG_SECTIONS,
  },
});
assert.deepEqual(
  JSON.parse(r.outputs.request).input.sections.find(s => s.type === 'security'),
  { type: 'security', section: 'Fixed' },
);
assert.deepEqual(r.calls, []);
for (const bad of ['', 'abc', 'z'.repeat(40), `${SHA}0`]) {
  r = run('request', { PROD_SHA: bad });
  assert.notEqual(r.status, 0, `refuses PROD_SHA=${bad}`);
  assert.match(r.stderr, /PROD_SHA must be a full 40-character commit SHA/);
  assert.deepEqual(r.outputs, {});
}
r = run('bogus');
assert.notEqual(r.status, 0);
assert.match(r.stderr, /usage/);

// adapt, new plan: owed state before publication; the publish request
// carries the exact plan, repair and the dry-run flag.
const plan = { schema: 'calver-plan-v1', sha: SHA, policy: 'live-digest', tag: 'v2026.1102.0', id: 'x' };
const planEnv = { PLAN: JSON.stringify(plan), PREV_TAG: 'v2026.1101.0' };
for (const dry of [false, true]) {
  r = run('adapt', { ...planEnv, DRY_RUN: String(dry) }, { [LATEST]: release('v2026.1101.0') });
  assert.equal(r.status, 0, r.stderr);
  assert.equal(r.outputs.publish, 'true');
  assert.equal(r.outputs.prod_changed, 'true');
  assert.equal(r.outputs.released, 'false');
  assert.equal(r.outputs.prev_tag, 'v2026.1101.0');
  assert.equal(r.outputs.sbom_tags, '');
  assert.deepEqual(JSON.parse(r.outputs.publish_request), { phase: 'publish', plan, dryRun: dry, repair: true });
  assert.deepEqual(r.calls, dry ? [] : [LATEST, VIEW(plan.tag)]);
}
// A new plan still reconciles the Latest release's missing assets.
r = run('adapt', planEnv, { [LATEST]: release('v2026.1101.0', { assets: partial }) });
assert.equal(r.outputs.publish, 'true');
assert.equal(r.outputs.sbom_tags, 'v2026.1101.0');
// A plan for another SHA, policy or tag shape, or no plan at all, refuses.
for (const bad of [{ ...plan, sha: 'c'.repeat(40) }, { ...plan, policy: 'staged-promotion' }, { ...plan, tag: 'canary/v2026.1102.0-canary.0' }]) {
  r = run('adapt', { PLAN: JSON.stringify(bad) });
  assert.notEqual(r.status, 0);
  assert.match(r.stderr, /does not bind the live production SHA/);
  assert.equal(r.outputs.prod_changed, undefined);
}
r = run('adapt', {});
assert.notEqual(r.status, 0);
assert.match(r.stderr, /neither a skip nor a JSON plan/);

// adapt, completed replay: the planner recovered a SHA whose Release
// exists (a quiet day or a rollback onto a CalVer digest). Nothing is owed
// and nothing is re-published; missing assets on either release redispatch.
r = run('adapt', planEnv, { [LATEST]: release('v2026.1102.0'), [VIEW(plan.tag)]: release('v2026.1102.0') });
assert.equal(r.status, 0, r.stderr);
assert.equal(r.outputs.publish, 'false');
assert.equal(r.outputs.prod_changed, 'false');
assert.equal(r.outputs.sbom_tags, '');
assert.match(r.summary, /skipped: already-published/);
r = run('adapt', planEnv, {
  [LATEST]: release('v2026.1103.0', { assets: partial }),
  [VIEW(plan.tag)]: release('v2026.1102.0', { assets: partial }),
});
assert.equal(r.outputs.publish, 'false');
assert.equal(r.outputs.sbom_tags, 'v2026.1103.0 v2026.1102.0', 'rollback replay keeps Latest owed too');
r = run('adapt', planEnv, { [LATEST]: release('v2026.1102.0', { assets: partial }), [VIEW(plan.tag)]: release('v2026.1102.0', { assets: partial }) });
assert.equal(r.outputs.sbom_tags, 'v2026.1102.0', 'one dispatch per tag');
// A quiet replay on a stale Latest digest warns like a classified quiet day.
r = run('adapt', planEnv, {
  [LATEST]: release('v2026.1102.0', { publishedAt: old }), [VIEW(plan.tag)]: release('v2026.1102.0'), [AHEAD]: '42',
});
assert.match(r.stdout, /::warning title=daily-digest stalled production::/);

// adapt, skipped: every classified reason is a green no-tag day.
for (const reason of ['quiet-day', 'rollback', 'digest-already-published']) {
  r = run('adapt', { SKIPPED: 'true', REASON: reason }, { [LATEST]: release('v2026.1101.0'), [REFS]: [] });
  assert.equal(r.status, 0, r.stderr);
  assert.equal(r.outputs.publish, 'false');
  assert.equal(r.outputs.prod_changed, 'false');
  assert.equal(r.outputs.released, 'false');
  assert.equal(r.outputs.prev_tag, 'v2026.1101.0');
  assert.equal(r.outputs.sbom_tags, '', 'complete assets: no dispatch');
  assert.match(r.summary, new RegExp(`skipped: ${reason}`));
}
r = run('adapt', { SKIPPED: 'true', REASON: 'surprise' });
assert.notEqual(r.status, 0);
assert.match(r.stderr, /unclassified skip reason/);
// Missing inventories on the Latest release redispatch the SBOM chain.
r = run('adapt', { SKIPPED: 'true', REASON: 'rollback' }, { [LATEST]: release('v2026.1101.0', { assets: partial }) });
assert.equal(r.status, 0, r.stderr);
assert.equal(r.outputs.sbom_tags, 'v2026.1101.0');
// A failed lookup stays green with no dispatch.
r = run('adapt', { SKIPPED: 'true', REASON: 'quiet-day' });
assert.equal(r.status, 0, r.stderr);
assert.equal(r.outputs.sbom_tags, '');
assert.equal(r.outputs.prev_tag, '');
// A dry-run skip makes no gh calls at all.
r = run('adapt', { SKIPPED: 'true', REASON: 'digest-already-published', DRY_RUN: 'true' }, { [LATEST]: release('v2026.1101.0', { assets: [] }) });
assert.equal(r.status, 0, r.stderr);
assert.equal(r.outputs.sbom_tags, '');
assert.deepEqual(r.calls, []);

// adapt, unfinished today: today's tag exists without its Release and
// production has since moved (digest-already-published for the new SHA).
// The adapter finishes that tag from its own frozen plan, never a new one.
const frozen = { schema: 'calver-plan-v1', sha: 'a'.repeat(40), policy: 'live-digest', tag: TODAY, previousStableTag: 'v2026.10.23', id: 'y' };
const tagObject = `api repos/o/r/git/tags/${'f'.repeat(40)}`;
const unfinished = {
  [LATEST]: release('v2026.10.23'),
  [REFS]: [{ ref: `refs/tags/${TODAY}`, object: { type: 'tag', sha: 'f'.repeat(40) } }],
  [tagObject]: { message: `calver-plan-v1\n${JSON.stringify({ ...frozen, publicationEvidence: null })}` },
};
r = run('adapt', { SKIPPED: 'true', REASON: 'digest-already-published' }, unfinished);
assert.equal(r.status, 0, r.stderr);
assert.equal(r.outputs.publish, 'true');
assert.equal(r.outputs.prod_changed, 'true');
assert.equal(r.outputs.prev_tag, 'v2026.10.23');
assert.deepEqual(JSON.parse(r.outputs.publish_request), { phase: 'publish', plan: frozen, dryRun: false, repair: true });
assert.ok(r.calls.includes(VIEW(TODAY)) && r.calls.includes(tagObject));
// Its Release exists after all, the annotation is not CalVer, or the
// lookup fails: an ordinary green skip.
for (const gh of [
  { ...unfinished, [VIEW(TODAY)]: release(TODAY) },
  { ...unfinished, [tagObject]: { message: 'manual tag' } },
  { [LATEST]: release('v2026.10.23') },
]) {
  r = run('adapt', { SKIPPED: 'true', REASON: 'digest-already-published' }, gh);
  assert.equal(r.status, 0, r.stderr);
  assert.equal(r.outputs.publish, 'false');
  assert.equal(r.outputs.prod_changed, 'false');
}
// Only the already-published skip looks for an unfinished tag.
r = run('adapt', { SKIPPED: 'true', REASON: 'rollback' }, unfinished);
assert.equal(r.outputs.publish, 'false');
assert.ok(!r.calls.includes(REFS));

// Stalled production: a quiet day on a digest older than 3 days while main
// moved ahead warns (green). Fresh digests, a level main and failed lookups
// stay silent; only quiet days check.
r = run('adapt', { SKIPPED: 'true', REASON: 'quiet-day' }, { [LATEST]: release('v2026.10.23', { publishedAt: old }), [AHEAD]: '42' });
assert.equal(r.status, 0, r.stderr);
assert.match(r.stdout, /::warning title=daily-digest stalled production::Production still serves bbbbbbb \(v2026\.10\.23, 8 days old\) while main is 42 commits ahead/);
assert.match(r.summary, /### Stalled production/);
for (const [reason, gh] of [
  ['quiet-day', { [LATEST]: release('v2026.10.23'), [AHEAD]: '42' }],
  ['quiet-day', { [LATEST]: release('v2026.10.23', { publishedAt: old }), [AHEAD]: '0' }],
  ['quiet-day', { [LATEST]: release('v2026.10.23', { publishedAt: old }) }],
  ['rollback', { [LATEST]: release('v2026.10.23', { publishedAt: old }), [AHEAD]: '42' }],
]) {
  r = run('adapt', { SKIPPED: 'true', REASON: reason }, gh);
  assert.equal(r.status, 0, r.stderr);
  assert.doesNotMatch(r.stdout, /::warning/, JSON.stringify(gh));
}

// record: a fresh Release always dispatches SBOM; a repair or replay only
// when its inventories are incomplete; a dry run records nothing.
r = run('record', { CREATED: 'true', TAG: 'v2026.1101.0' });
assert.equal(r.status, 0, r.stderr);
assert.deepEqual(r.outputs, { tag_name: 'v2026.1101.0', released: 'true', sbom_tags: 'v2026.1101.0' });
assert.deepEqual(r.calls, []);
r = run('record', { CREATED: 'false', TAG: 'v2026.1101.0' }, { [VIEW('v2026.1101.0')]: release('v2026.1101.0') });
assert.deepEqual(r.outputs, { tag_name: 'v2026.1101.0', released: 'true', sbom_tags: '' });
r = run('record', { CREATED: 'false', TAG: 'v2026.1101.0' }, { [VIEW('v2026.1101.0')]: release('v2026.1101.0', { assets: [] }) });
assert.equal(r.outputs.sbom_tags, 'v2026.1101.0');
r = run('record', { CREATED: 'false', TAG: 'v2026.1101.0' });
assert.equal(r.status, 0, r.stderr);
assert.equal(r.outputs.sbom_tags, '', 'lookup failure stays green without dispatch');
r = run('record', { CREATED: 'false', TAG: 'v2026.1101.0', DRY_RUN: 'true' });
assert.deepEqual(r.outputs, { tag_name: '', released: 'false', sbom_tags: '' });
assert.deepEqual(r.calls, []);
for (const tag of ['', 'v2026.1101.0-canary.0', 'canary/v2026.1101.0']) {
  r = run('record', { CREATED: 'true', TAG: tag });
  assert.notEqual(r.status, 0, `refuses TAG=${tag}`);
  assert.deepEqual(r.outputs, {});
}

fs.rmSync(dir, { recursive: true, force: true });
console.log('daily-digest adapter: ok');
