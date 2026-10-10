#!/usr/bin/env node
'use strict';

// Release on production promote: tag the promoted commit vX.Y.Z and publish a
// GitHub Release whose notes come from the Conventional Commit subjects since
// the previous vX.Y.Z tag. No release PR: the version and notes are derived
// from commits already reviewed on main, so nothing new needs CI or review.
//
// Bump rules and note sections are read from release-please-config.json (the
// same keys release-please used), so versions continue the existing tag line.
//
// Env: RELEASE_SHA (40-hex, must be on origin/main), GH_REPO, GH_TOKEN,
// GITHUB_SERVER_URL, GITHUB_OUTPUT, DRY_RUN=true to print without publishing.
// Needs a full-history checkout with tags.

const { execFileSync } = require('node:child_process');
const fs = require('node:fs');

const HEADER = /^(?<type>[a-z]+)(?:\((?<scope>[^()\r\n]+)\))?(?<bang>!)?: (?<desc>\S.*)$/;
const SEMVER_TAG = /^v(\d+)\.(\d+)\.(\d+)$/;

function parseCommit(sha, subject, body) {
  const m = HEADER.exec(String(subject).trim());
  if (!m) return null;
  let desc = m.groups.desc.trim();
  let pr = null;
  const prMatch = / \(#(\d+)\)$/.exec(desc);
  if (prMatch) {
    pr = Number(prMatch[1]);
    desc = desc.slice(0, prMatch.index);
  }
  return {
    sha,
    type: m.groups.type,
    scope: m.groups.scope ? m.groups.scope.trim() : null,
    breaking: Boolean(m.groups.bang) || /^BREAKING[ -]CHANGE: /m.test(body || ''),
    desc,
    pr,
  };
}

function parseTag(tag) {
  const m = SEMVER_TAG.exec(tag);
  return m ? m.slice(1, 4).map(Number) : null;
}

function compareVersions(a, b) {
  for (let i = 0; i < 3; i += 1) if (a[i] !== b[i]) return a[i] - b[i];
  return 0;
}

// release-please semantics: before 1.0.0, bump-minor-pre-major turns a breaking
// change into a minor bump, and bump-patch-for-minor-pre-major turns a feat into
// a patch bump. Every promote that reaches here gets at least a patch bump, so
// each production deploy of new commits has its own tag.
function nextVersion(prev, commits, opts) {
  const [major, minor, patch] = prev;
  const breaking = commits.some((c) => c.breaking);
  const feat = commits.some((c) => c.type === 'feat');
  if (major === 0) {
    if (breaking) return opts.bumpMinorPreMajor ? [0, minor + 1, 0] : [1, 0, 0];
    if (feat) return opts.bumpPatchForMinorPreMajor ? [0, minor, patch + 1] : [0, minor + 1, 0];
    return [0, minor, patch + 1];
  }
  if (breaking) return [major + 1, 0, 0];
  if (feat) return [major, minor + 1, 0];
  return [major, minor, patch + 1];
}

function loadOptions(path) {
  const pkg = JSON.parse(fs.readFileSync(path, 'utf8')).packages['.'];
  return {
    bumpMinorPreMajor: pkg['bump-minor-pre-major'] === true,
    bumpPatchForMinorPreMajor: pkg['bump-patch-for-minor-pre-major'] === true,
    sections: pkg['changelog-sections'] || [],
  };
}

function entry(c, repoUrl) {
  const scope = c.scope ? `**${c.scope}:** ` : '';
  const pr = c.pr ? ` ([#${c.pr}](${repoUrl}/issues/${c.pr}))` : '';
  return `* ${scope}${c.desc}${pr} ([${c.sha.slice(0, 7)}](${repoUrl}/commit/${c.sha}))`;
}

function renderNotes({ version, prevTag, date, commits, sections, repoUrl }) {
  const tag = `v${version.join('.')}`;
  const lines = [`## [${version.join('.')}](${repoUrl}/compare/${prevTag}...${tag}) (${date})`];
  const bySort = (a, b) => (a.scope || '').localeCompare(b.scope || '') || a.desc.localeCompare(b.desc);
  const breaking = commits.filter((c) => c.breaking).sort(bySort);
  if (breaking.length) lines.push('', '', '### ⚠ BREAKING CHANGES', '', ...breaking.map((c) => entry(c, repoUrl)));
  const visible = new Map();
  for (const s of sections) {
    if (s.hidden) continue;
    if (!visible.has(s.section)) visible.set(s.section, new Set());
    visible.get(s.section).add(s.type);
  }
  let any = breaking.length > 0;
  for (const [section, types] of visible) {
    const items = commits.filter((c) => types.has(c.type)).sort(bySort);
    if (!items.length) continue;
    any = true;
    lines.push('', '', `### ${section}`, '', ...items.map((c) => entry(c, repoUrl)));
  }
  if (!any) lines.push('', 'No user-facing changes in this release; the compare link lists every commit.');
  return `${lines.join('\n')}\n`;
}

function git(...args) {
  return execFileSync('git', args, { encoding: 'utf8' }).trim();
}

function releaseExists(tag) {
  try {
    execFileSync('gh', ['release', 'view', tag, '--json', 'tagName'], { stdio: 'ignore' });
    return true;
  } catch {
    return false;
  }
}

function output(values) {
  const text = Object.entries(values).map(([k, v]) => `${k}=${v}\n`).join('');
  if (process.env.GITHUB_OUTPUT) fs.appendFileSync(process.env.GITHUB_OUTPUT, text);
  process.stdout.write(text);
}

function main() {
  const sha = (process.env.RELEASE_SHA || '').trim().toLowerCase();
  const repo = process.env.GH_REPO;
  const dryRun = process.env.DRY_RUN === 'true';
  if (!/^[0-9a-f]{40}$/.test(sha)) throw new Error('RELEASE_SHA must be a full 40-character commit SHA');
  if (!repo) throw new Error('GH_REPO is required');
  const repoUrl = `${process.env.GITHUB_SERVER_URL || 'https://github.com'}/${repo}`;
  try {
    git('merge-base', '--is-ancestor', sha, 'origin/main');
  } catch {
    throw new Error(`${sha} is not on origin/main; refusing to tag it`);
  }

  const tagsAt = git('tag', '--points-at', sha, '--list', 'v*').split('\n').filter((t) => parseTag(t));
  const reachable = git('tag', '--merged', sha, '--list', 'v*').split('\n')
    .filter((t) => parseTag(t) && !tagsAt.includes(t))
    .sort((a, b) => compareVersions(parseTag(b), parseTag(a)));
  if (!reachable.length) throw new Error(`no vX.Y.Z tag is reachable from ${sha}`);
  const prevTag = reachable[0];
  const opts = loadOptions('release-please-config.json');
  const commits = git('log', '--format=%H%x1f%s%x1f%b%x1e', `${prevTag}..${sha}`)
    .split('\x1e').map((r) => r.trim()).filter(Boolean)
    .map((r) => { const [h, s, b] = r.split('\x1f'); return parseCommit(h, s, b); })
    .filter(Boolean);

  let version;
  if (tagsAt.length) {
    // Re-promote of an already tagged commit: reuse its tag and only repair a
    // missing GitHub Release.
    version = tagsAt.map(parseTag).sort((a, b) => compareVersions(b, a))[0];
  } else {
    version = nextVersion(parseTag(prevTag), commits, opts);
    const highest = git('tag', '--list', 'v*').split('\n').map(parseTag).filter(Boolean)
      .sort((a, b) => compareVersions(b, a))[0];
    if (highest && compareVersions(version, highest) <= 0) {
      // Promoting a commit older than the newest release (a rollback): its
      // next version already exists on a newer commit, so it is not tagged.
      console.log(`${sha} is older than v${highest.join('.')}; rollback promote, no new tag`);
      output({ tag_name: '', released: 'false' });
      return;
    }
  }
  const tag = `v${version.join('.')}`;
  const notes = renderNotes({
    version, prevTag, date: new Date().toISOString().slice(0, 10), commits, sections: opts.sections, repoUrl,
  });
  console.log(notes);
  if (dryRun) {
    output({ tag_name: '', released: 'false' });
    return;
  }
  if (!releaseExists(tag)) {
    const notesFile = `${process.env.RUNNER_TEMP || '/tmp'}/release-notes-${tag}.md`;
    fs.writeFileSync(notesFile, notes);
    // Creates the tag on the promoted commit when it does not exist yet.
    execFileSync('gh', ['release', 'create', tag, '--target', sha, '--title', tag, '--notes-file', notesFile, '--latest'],
      { stdio: 'inherit' });
  }
  output({ tag_name: tag, released: 'true' });
}

module.exports = { parseCommit, parseTag, nextVersion, renderNotes, compareVersions };

if (require.main === module) {
  try {
    main();
  } catch (err) {
    console.error(`::error title=release-on-promote::${err.message}`);
    process.exit(1);
  }
}
