#!/usr/bin/env node
'use strict';

// Offline tests for the daily-digest adapter around the shared CalVer
// releaser: the plan request, skip classification, owed-digest outputs,
// SBOM redispatch, the stalled-production warning and dry runs. gh is a
// mock on PATH; no network and no live gh calls.

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
fs.writeFileSync(path.join(dir, 'gh'), [
  '#!/bin/sh',
  `echo "$@" >> "${ghLog}"`,
  'if [ "$1" = "release" ] && [ "$2" = "view" ]; then',
  '  [ -n "$RELEASE_VIEW_JSON" ] || exit 1',
  '  printf "%s" "$RELEASE_VIEW_JSON"; exit 0',
  'fi',
  'if [ "$1" = "api" ]; then [ -n "$AHEAD_BY" ] || exit 1; echo "$AHEAD_BY"; exit 0; fi',
  'exit 1',
  '',
].join('\n'));
fs.chmodSync(path.join(dir, 'gh'), 0o755);

function run(mode, env = {}) {
  const out = path.join(dir, 'outputs');
  const sum = path.join(dir, 'summary');
  fs.writeFileSync(out, '');
  fs.writeFileSync(sum, '');
  fs.writeFileSync(ghLog, '');
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

const full = { assets: SBOM_ASSETS.map(name => ({ name })) };
const view = (extra = {}) => JSON.stringify({ tagName: 'v2026.1101.0', publishedAt: new Date().toISOString(), ...full, ...extra });

// Asset completeness.
assert.equal(assetsMissing(full), false);
assert.equal(assetsMissing({ assets: [{ name: 'SHA256SUMS' }] }), true);
assert.equal(assetsMissing({}), true);

// request: the live-digest plan binds the SHA as its operation, so a retry
// for the same build recovers the frozen plan.
let r = run('request');
assert.equal(r.status, 0, r.stderr);
assert.deepEqual(JSON.parse(r.outputs.request), {
  phase: 'plan',
  input: { channel: 'stable', policy: 'live-digest', sha: SHA, operation: `daily-digest/${SHA}` },
});
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

// adapt, planned: owed state before publication, publish request carries the
// exact plan, repair, and the dry-run flag.
const plan = { schema: 'calver-plan-v1', sha: SHA, policy: 'live-digest', tag: 'v2026.1101.0', id: 'x' };
for (const dry of [false, true]) {
  r = run('adapt', { PLAN: JSON.stringify(plan), PREV_TAG: 'v2026.10.23', DRY_RUN: String(dry) });
  assert.equal(r.status, 0, r.stderr);
  assert.equal(r.outputs.publish, 'true');
  assert.equal(r.outputs.prod_changed, 'true');
  assert.equal(r.outputs.released, 'false');
  assert.equal(r.outputs.prev_tag, 'v2026.10.23');
  assert.equal(r.outputs.sbom_tag, '');
  assert.deepEqual(JSON.parse(r.outputs.publish_request), { phase: 'publish', plan, dryRun: dry, repair: true });
  assert.deepEqual(r.calls, []);
}
// A plan for another SHA or policy, or no plan at all, refuses.
for (const bad of [{ ...plan, sha: 'c'.repeat(40) }, { ...plan, policy: 'staged-promotion' }]) {
  r = run('adapt', { PLAN: JSON.stringify(bad) });
  assert.notEqual(r.status, 0);
  assert.match(r.stderr, /does not bind the live production SHA/);
}
r = run('adapt', {});
assert.notEqual(r.status, 0);
assert.match(r.stderr, /neither a skip nor a JSON plan/);

// adapt, skipped: every classified reason is a green no-tag day.
for (const reason of ['quiet-day', 'rollback', 'digest-already-published']) {
  r = run('adapt', { SKIPPED: 'true', REASON: reason, RELEASE_VIEW_JSON: view() });
  assert.equal(r.status, 0, r.stderr);
  assert.equal(r.outputs.publish, 'false');
  assert.equal(r.outputs.prod_changed, 'false');
  assert.equal(r.outputs.released, 'false');
  assert.equal(r.outputs.prev_tag, 'v2026.1101.0');
  assert.equal(r.outputs.sbom_tag, '', 'complete assets: no dispatch');
  assert.match(r.summary, new RegExp(`skipped: ${reason}`));
}
r = run('adapt', { SKIPPED: 'true', REASON: 'surprise' });
assert.notEqual(r.status, 0);
assert.match(r.stderr, /unclassified skip reason/);
// Missing inventories on the Latest release redispatch the SBOM chain.
r = run('adapt', { SKIPPED: 'true', REASON: 'rollback', RELEASE_VIEW_JSON: view({ assets: [{ name: 'SHA256SUMS' }] }) });
assert.equal(r.status, 0, r.stderr);
assert.equal(r.outputs.sbom_tag, 'v2026.1101.0');
assert.ok(r.calls.includes('release view --json tagName,assets,publishedAt'));
// A failed lookup stays green with no dispatch.
r = run('adapt', { SKIPPED: 'true', REASON: 'quiet-day' });
assert.equal(r.status, 0, r.stderr);
assert.equal(r.outputs.sbom_tag, '');
assert.equal(r.outputs.prev_tag, '');
// A dry-run skip makes no gh calls at all.
r = run('adapt', { SKIPPED: 'true', REASON: 'quiet-day', DRY_RUN: 'true', RELEASE_VIEW_JSON: view({ assets: [] }) });
assert.equal(r.status, 0, r.stderr);
assert.equal(r.outputs.sbom_tag, '');
assert.deepEqual(r.calls, []);

// Stalled production: a quiet day on a digest older than 3 days while main
// moved ahead warns (green). Fresh digests, a level main and failed lookups
// stay silent; only quiet days check.
const old = new Date(Date.now() - 8 * 24 * 60 * 60 * 1000).toISOString();
r = run('adapt', { SKIPPED: 'true', REASON: 'quiet-day', RELEASE_VIEW_JSON: view({ publishedAt: old }), AHEAD_BY: '42' });
assert.equal(r.status, 0, r.stderr);
assert.match(r.stdout, /::warning title=daily-digest stalled production::Production still serves bbbbbbb \(v2026\.1101\.0, 8 days old\) while main is 42 commits ahead/);
assert.match(r.summary, /### Stalled production/);
assert.ok(r.calls.includes(`api repos/o/r/compare/${SHA}...main --jq .ahead_by`));
for (const env of [
  { REASON: 'quiet-day', RELEASE_VIEW_JSON: view(), AHEAD_BY: '42' },
  { REASON: 'quiet-day', RELEASE_VIEW_JSON: view({ publishedAt: old }), AHEAD_BY: '0' },
  { REASON: 'quiet-day', RELEASE_VIEW_JSON: view({ publishedAt: old }) },
  { REASON: 'rollback', RELEASE_VIEW_JSON: view({ publishedAt: old }), AHEAD_BY: '42' },
]) {
  r = run('adapt', { SKIPPED: 'true', ...env });
  assert.equal(r.status, 0, r.stderr);
  assert.doesNotMatch(r.stdout, /::warning/, JSON.stringify(env));
}

// record: a fresh Release always dispatches SBOM; a replay only when its
// inventories are incomplete; a dry run records nothing.
r = run('record', { CREATED: 'true', TAG: 'v2026.1101.0' });
assert.equal(r.status, 0, r.stderr);
assert.deepEqual(r.outputs, { tag_name: 'v2026.1101.0', released: 'true', sbom_tag: 'v2026.1101.0' });
assert.deepEqual(r.calls, []);
r = run('record', { CREATED: 'false', TAG: 'v2026.1101.0', RELEASE_VIEW_JSON: view() });
assert.deepEqual(r.outputs, { tag_name: 'v2026.1101.0', released: 'true', sbom_tag: '' });
assert.deepEqual(r.calls, ['release view v2026.1101.0 --json tagName,assets,publishedAt']);
r = run('record', { CREATED: 'false', TAG: 'v2026.1101.0', RELEASE_VIEW_JSON: view({ assets: [] }) });
assert.equal(r.outputs.sbom_tag, 'v2026.1101.0');
r = run('record', { CREATED: 'false', TAG: 'v2026.1101.0' });
assert.equal(r.status, 0, r.stderr);
assert.equal(r.outputs.sbom_tag, '', 'lookup failure stays green without dispatch');
r = run('record', { CREATED: 'false', TAG: 'v2026.1101.0', DRY_RUN: 'true' });
assert.deepEqual(r.outputs, { tag_name: '', released: 'false', sbom_tag: '' });
assert.deepEqual(r.calls, []);
for (const tag of ['', 'v2026.1101.0-canary.0', 'canary/v2026.1101.0']) {
  r = run('record', { CREATED: 'true', TAG: tag });
  assert.notEqual(r.status, 0, `refuses TAG=${tag}`);
  assert.deepEqual(r.outputs, {});
}

fs.rmSync(dir, { recursive: true, force: true });
console.log('daily-digest adapter: ok');
