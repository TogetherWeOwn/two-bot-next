'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const {execFileSync} = require('node:child_process');
// Native release-please 17.6.0 derives this branch's component from the root
// package name (`two-bot-next`), so its head is the component branch below,
// not the plain target branch. The offline lifecycle fixture asserts the live
// library still generates this head, so selection fails loudly on drift.
const branch = 'release-please--branches--main--components--two-bot-next';

// The exact title native release-please 17.6.0 commits when it (re)generates
// this branch, e.g. `chore(main): release 0.1.1`. The native commit message
// IS the PR title (manifest creates the branch commit with
// `message = pullRequest.title.toString()`), and the fixture asserts the
// live library still produces this shape, so reuse below fails loudly on
// drift instead of silently mis-skipping regeneration.
const NATIVE_RELEASE_COMMIT_PATTERN = /^chore\(main\): release \d+\.\d+\.\d+/;

function findReleasePr(pulls, repository) {
  const candidates = pulls.filter(pr => pr.state === 'open' &&
    pr.base.ref === 'main' && pr.base.repo.full_name === repository &&
    pr.head.ref === branch && pr.head.repo?.full_name === repository &&
    pr.labels.some(label => label.name === 'autorelease: pending'));
  assert(candidates.length <= 1, 'Ambiguous open release PR');
  return candidates[0];
}

function canReuseReleasePr(compare, mainSha) {
  // Ancestry alone: the release branch contains the main snapshot, so an
  // unchanged-main rerun must not regenerate. This is necessary but NOT
  // sufficient: after "Update branch" merges a newer main into the release
  // branch, ancestry also holds while the generated metadata is stale.
  return compare.merge_base_commit.sha === mainSha &&
    ['ahead', 'identical'].includes(compare.status);
}

// Newest native generation commit in an oldest-first [{sha, message}] list,
// or null when no native generation commit is present (fail toward
// regeneration, never reuse).
function findNewestNativeCommit(commits) {
  let newest = null;
  for (const commit of commits) {
    assert.match(commit.sha, /^[a-f0-9]{40}$/);
    assert.equal(typeof commit.message, 'string');
    if (NATIVE_RELEASE_COMMIT_PATTERN.test(commit.message)) newest = commit.sha;
  }
  return newest;
}

// Generation snapshot: the main commit whose snapshot the newest native
// release commit was generated from. Native force-replaces the release
// branch with one commit parented on the primary head (verified against the
// pinned code-suggester branch/commit/updateRef path), so the first parent
// of the newest `chore(main): release X` commit IS its generation snapshot,
// whatever later commits (migration, Update-branch merges) were added on top.
// commits is oldest-first [{sha, message}]; parentOf maps sha to its
// first-parent sha. Returns the snapshot sha, or null when no native
// generation commit is present (fail toward regeneration, never reuse).
function findGenerationSnapshot(commits, parentOf) {
  const newest = findNewestNativeCommit(commits);
  if (!newest) return null;
  assert(parentOf.has(newest), 'Missing parent evidence for native release commit');
  const snapshot = parentOf.get(newest);
  assert.match(snapshot, /^[a-f0-9]{40}$/);
  return snapshot;
}

function inspectReleasePr(repository, mainSha, request, mode) {
  assert.match(repository, /^[\w.-]+\/[\w.-]+$/);
  const owner = repository.split('/')[0];
  const pages = request(`repos/${repository}/pulls?state=open&base=main&head=${owner}:${branch}`, true);
  const pr = findReleasePr(pages.flat(), repository);
  if (mode === 'select') return {
    pr_available: String(Boolean(pr)),
    pr: pr ? JSON.stringify({number: pr.number, headBranchName: pr.head.ref}) : '',
  };
  assert.equal(mode, 'plan', 'Expected plan or select mode');
  assert.match(mainSha, /^[a-f0-9]{40}$/);
  if (!pr) return {reuse_pr: 'false'};
  assert.match(pr.head.sha, /^[a-f0-9]{40}$/);
  const compare = request(`repos/${repository}/compare/${mainSha}...${pr.head.sha}`);
  if (!canReuseReleasePr(compare, mainSha)) return {reuse_pr: 'false'};
  // Bind reuse to the snapshot actually used to generate the release
  // metadata, not mere ancestry. A merged-but-unregenerated main (e.g. via
  // "Update branch") must regenerate: native would bump 0.1.1 to 0.2.0 for
  // the new feature, while reuse would freeze the stale patch. Only the
  // newest native commit needs its parent fetched; migration/merge commits
  // on top never change which snapshot generated the metadata.
  const commitPages = request(`repos/${repository}/pulls/${pr.number}/commits`, true);
  const commits = commitPages.flat().map(entry => ({sha: entry.sha, message: entry.commit.message}));
  const newest = findNewestNativeCommit(commits);
  if (!newest) return {reuse_pr: 'false'};
  const generated = request(`repos/${repository}/commits/${newest}`);
  assert(Array.isArray(generated.parents) && generated.parents.length > 0, 'Native release commit must have a parent');
  assert.match(generated.parents[0].sha, /^[a-f0-9]{40}$/);
  const snapshot = findGenerationSnapshot(commits, new Map([[newest, generated.parents[0].sha]]));
  return {reuse_pr: String(snapshot === mainSha)};
}

module.exports = {findReleasePr, canReuseReleasePr, findNewestNativeCommit, findGenerationSnapshot, NATIVE_RELEASE_COMMIT_PATTERN, inspectReleasePr};

if (require.main === module) {
  const outputs = inspectReleasePr(process.env.GH_REPO, process.env.GITHUB_SHA, (endpoint, paginate) => {
    const args = ['api', endpoint];
    if (paginate) args.push('--paginate', '--slurp');
    return JSON.parse(execFileSync('gh', args, {encoding: 'utf8'}));
  }, process.argv[2]);
  for (const [name, value] of Object.entries(outputs)) {
    assert(!value.includes('\n'), 'Unexpected multiline output');
    fs.appendFileSync(process.env.GITHUB_OUTPUT, `${name}=${value}\n`);
  }
}
