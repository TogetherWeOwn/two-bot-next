'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const {execFileSync} = require('node:child_process');
const branch = 'release-please--branches--main';

function findReleasePr(pulls, repository) {
  const candidates = pulls.filter(pr => pr.state === 'open' &&
    pr.base.ref === 'main' && pr.base.repo.full_name === repository &&
    pr.head.ref === branch && pr.head.repo?.full_name === repository &&
    pr.labels.some(label => label.name === 'autorelease: pending'));
  assert(candidates.length <= 1, 'Ambiguous open release PR');
  return candidates[0];
}

function canReuseReleasePr(compare, mainSha) {
  // The native release branch contains the main snapshot that generated it.
  // A later main commit must regenerate it; our migration commit must not.
  return compare.merge_base_commit.sha === mainSha &&
    ['ahead', 'identical'].includes(compare.status);
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
  return {reuse_pr: String(canReuseReleasePr(compare, mainSha))};
}

module.exports = {findReleasePr, canReuseReleasePr, inspectReleasePr};

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
