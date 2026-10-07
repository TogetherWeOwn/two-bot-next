#!/usr/bin/env node
'use strict';

// release-please parses every commit on main with a strict conventional-commits
// parser. A message the parser rejects is dropped from the release notes (and
// from the version bump) with only a debug-level log line, and the run still
// succeeds. This guard runs the same library code over the same text and
// fails instead, in two places (docs/releases.md, "Unparseable commits"):
//
//   pr     the squash commit the PR will become (title + body). Runs in the
//          required `pr-lint` job, so a body that would be dropped never merges.
//   range  the commits since the last release tag. Runs in the release
//          workflow before the release PR is regenerated, so a commit that
//          reached main anyway (a body edited at merge time) fails the run.
//
// Needs release-please@17.6.0 resolvable through NODE_PATH, the version the
// pinned release action bundles. Writes nothing and needs no token.

const fs = require('node:fs');
const path = require('node:path');
const {execFileSync} = require('node:child_process');

const EXCEPTIONS_FILE = path.join(__dirname, '..', 'release-parse-exceptions.txt');
// release-please opens these itself; their merge commit is the release, so it
// is never parsed for notes.
const RELEASE_PR_HEAD_PREFIX = 'release-please--branches--';
const PARSE_FAILURE = 'commit could not be parsed:';

function loadParser() {
  return require('release-please/build/src/commit').parseConventionalCommits;
}

// Same call release-please makes; its only failure signal is a debug log line,
// so capture the logger instead of the (silently shorter) result.
function findUnparseable(commits, parseConventionalCommits = loadParser()) {
  const failures = [];
  const logger = {
    info() {},
    warn() {},
    error() {},
    debug(message) {
      if (String(message).startsWith(PARSE_FAILURE)) {
        // "<sha> <header>": keep the header; the sha is attached below.
        failures.push({subject: String(message).slice(PARSE_FAILURE.length).trim().replace(/^\S+\s*/, ''), error: ''});
      } else if (failures.length && !failures[failures.length - 1].error) {
        failures[failures.length - 1].error = String(message).replace(/^error message: (Error: )?/, '');
      }
    },
  };
  for (const commit of commits) {
    const before = failures.length;
    parseConventionalCommits([{files: [], ...commit}], logger);
    for (const failure of failures.slice(before)) failure.sha = commit.sha;
  }
  return failures;
}

// GitHub squash merges use PR_TITLE (+ " (#N)") and PR_BODY (repo setting).
function squashMessage(title, number, body) {
  const header = number ? `${title} (#${number})` : title;
  return body && body.trim() ? `${header}\n\n${body}` : header;
}

function checkPullRequest({title, body, number, headRef}, parseConventionalCommits) {
  if ((headRef || '').startsWith(RELEASE_PR_HEAD_PREFIX)) return [];
  // The body may carry CRLF from the web editor. Check both spellings so the
  // gate cannot pass a text the merge commit would store differently.
  const variants = new Set([body || '', (body || '').replace(/\r\n?/g, '\n')]);
  const failures = [];
  for (const variant of variants) {
    const message = squashMessage(title, number, variant);
    failures.push(...findUnparseable([{
      sha: 'PR', message, pullRequest: {body: variant},
    }], parseConventionalCommits));
  }
  return failures.filter((failure, index) => failures.findIndex(other => other.error === failure.error) === index);
}

function readExceptions(file) {
  if (!fs.existsSync(file)) return new Map();
  const entries = new Map();
  for (const [index, raw] of fs.readFileSync(file, 'utf8').split('\n').entries()) {
    const line = raw.trim();
    if (!line || line.startsWith('#')) continue;
    const [sha, ...reason] = line.split(/\s+/);
    if (!/^[0-9a-f]{40}$/.test(sha)) {
      throw new Error(`${path.basename(file)}:${index + 1}: expected a full 40-character commit SHA, got "${sha}"`);
    }
    entries.set(sha, reason.join(' '));
  }
  return entries;
}

function git(args) {
  return execFileSync('git', args, {encoding: 'utf8', maxBuffer: 1 << 28});
}

function lastReleaseTag() {
  try {
    return git(['describe', '--tags', '--abbrev=0', '--match', 'v[0-9]*', 'HEAD']).trim();
  } catch {
    return '';
  }
}

function listCommits(base) {
  const range = base ? [`${base}..HEAD`] : ['HEAD'];
  return git(['log', '-z', '--format=%H%x1f%B', ...range]).split('\0').filter(Boolean).map(record => {
    const [sha, ...rest] = record.split('\x1f');
    return {sha, message: rest.join('\x1f').replace(/\n$/, '')};
  });
}

function checkRange({base, exceptions}, parseConventionalCommits) {
  const commits = listCommits(base);
  const failures = findUnparseable(commits, parseConventionalCommits);
  return {
    checked: commits.length,
    failed: failures.filter(failure => !exceptions.has(failure.sha)),
    acknowledged: failures.filter(failure => exceptions.has(failure.sha)),
  };
}

const ADVICE = [
  'release-please will drop this change from the release notes (and from the version bump) without any error.',
  'The strict parser rejects a line that starts with a word and "(" and holds a second "(" before its first ")",',
  'for example `call(a(b))`. Start such a line with "- " or a space, or reword it, then re-run this check.',
];

function report(failures, what, note = '') {
  for (const failure of failures) {
    const where = failure.sha && failure.sha !== 'PR' ? ` ${failure.sha.slice(0, 12)}` : '';
    console.log(`::error title=Unparseable ${what}::${failure.subject || what}${where}: ${failure.error}`);
    console.error(`${what}${where} cannot be parsed by release-please: ${failure.error}`);
  }
  for (const line of ADVICE) console.error(line);
  if (note) console.error(note);
}

function main(argv, env = process.env) {
  const mode = argv[0];
  const parse = loadParser();
  if (mode === 'pr') {
    const title = env.TITLE || '';
    const body = env.BODY_FILE ? fs.readFileSync(env.BODY_FILE, 'utf8') : env.BODY || '';
    const failures = checkPullRequest({title, body, number: env.PR_NUMBER, headRef: env.HEAD_REF}, parse);
    if (!failures.length) {
      console.log('The squash commit for this PR parses for release-please.');
      return 0;
    }
    report(failures, 'squash commit',
      'Positions count the title as line 1 and a blank line as line 2, so PR body line N is position N+2.');
    return 1;
  }
  if (mode === 'range') {
    const base = env.RELEASE_BASE_REF !== undefined ? env.RELEASE_BASE_REF : lastReleaseTag();
    const {checked, failed, acknowledged} = checkRange({base, exceptions: readExceptions(env.EXCEPTIONS_FILE || EXCEPTIONS_FILE)}, parse);
    console.log(`Parsed ${checked} commit(s) since ${base || 'the first commit'} with release-please.`);
    for (const failure of acknowledged) {
      console.log(`::warning title=Acknowledged unparseable commit::${failure.sha} ${failure.subject}`);
    }
    if (!failed.length) return 0;
    report(failed, 'commit');
    console.error('If the note was added to the release by hand, record the SHA in .github/release-parse-exceptions.txt.');
    return 1;
  }
  console.error('usage: commit-parse-guard.cjs pr | range');
  return 2;
}

if (require.main === module) {
  try {
    process.exitCode = main(process.argv.slice(2));
  } catch (error) {
    console.error(`::error title=commit-parse-guard::${error.message}`);
    process.exitCode = 1;
  }
}

module.exports = {
  findUnparseable, squashMessage, checkPullRequest, checkRange, readExceptions,
  RELEASE_PR_HEAD_PREFIX, EXCEPTIONS_FILE,
};
