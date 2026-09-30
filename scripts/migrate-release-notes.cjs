'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const releaseHeading = /^##? \[?v?\d+\.\d+\.\d+.*$/gm;

// Native 17.6.0 stores oversized full release notes in `release-notes.md` on
// a derived notes branch and replaces the visible PR body with a single-line
// link: `release-please--branches--main--components--two-bot-next--release-notes`
// (see native FilePullRequestOverflowHandler.handleOverflow, which returns
// `${OVERFLOW_MESSAGE} ${url}`, and OVERFLOW_MESSAGE_REGEX which matches that
// one line). Reconciliation must resolve the stored notes through that
// representation instead of asserting on the visible link body; otherwise
// every overflow run fails before check dispatch. parseOverflowLink returns
// the {url, branchName} for a native overflow link body, or null for any
// other body. resolveNotesBody(visibleBody, fetchNotesFile) returns the full
// stored notes text for a native overflow link (fetchNotesFile receives the
// notes branch name and returns the file text), or the visible body unchanged.
// Both retain the native URL-branch parsing contract and required metadata:
// only the exact native overflow sentence, a single-line release-notes.md
// blob URL whose branch is this release's notes branch, and a nonempty
// fetched body pass. Anything else fails closed or passes through untouched.
const NATIVE_NOTES_BRANCH = 'release-please--branches--main--components--two-bot-next--release-notes';
const NATIVE_NOTES_FILE = 'release-notes.md';
const NATIVE_OVERFLOW_SENTENCE = 'This release is too large to preview in the pull request body. View the full release notes here:';

function parseOverflowLink(visibleBody) {
  const normalized = visibleBody.trim().replace(/\r\n/g, '\n');
  const prefix = `${NATIVE_OVERFLOW_SENTENCE} `;
  if (!normalized.startsWith(prefix)) return null;
  const url = normalized.slice(prefix.length).trim();
  if (url.length === 0 || /\s/.test(url)) return null;
  let branchName = null;
  try {
    const pathname = new URL(url).pathname;
    const match = pathname.match(new RegExp(`/blob/(?<branchName>.+)/${NATIVE_NOTES_FILE}$`));
    branchName = match?.groups?.branchName ?? null;
  } catch {
    branchName = null;
  }
  if (!branchName) return null;
  return {url, branchName};
}

function resolveNotesBody(visibleBody, fetchNotesFile) {
  const link = parseOverflowLink(visibleBody);
  if (!link) return visibleBody;
  assert.equal(link.branchName, NATIVE_NOTES_BRANCH, 'Unexpected release-notes branch in overflow PR body');
  const stored = fetchNotesFile(link.branchName);
  assert(typeof stored === 'string' && stored.length > 0, 'Missing stored release notes');
  return stored;
}

// Reconcile both sides independently: a retry may find the changelog pushed but
// the PR-body PATCH unfinished, or a migrated body with the old changelog.
function migrateReleaseNotes(changelog, body) {
  assert(changelog.startsWith('# Changelog\n\n'), 'Missing changelog title');
  if (/^## Unreleased\s*$/m.test(changelog)) {
    const marker = '\n## Changelog\n\n## Unreleased\n';
    const boundary = changelog.indexOf(marker);
    assert(boundary >= 0, 'Unexpected bootstrap changelog layout');
    assert.equal(changelog.indexOf(marker, boundary + marker.length), -1, 'Duplicate bootstrap notes');
    const original = changelog.slice('# Changelog\n\n'.length, boundary).trim();
    const manual = changelog.slice(boundary + marker.length).trim();
    assert.match(original, /^##? \[?v?\d+\.\d+\.\d+/);
    assert.equal((original.match(/^##? /gm) || []).length, 1, 'Expected one bootstrap release');
    assert(!/^##? /m.test(manual), 'Unexpected historical release in bootstrap notes');

    const sections = new Map();
    function collect(text, manualNotes) {
      const chunks = text.split(/^### (.+)\n/gm);
      const preamble = chunks.shift().trim();
      if (manualNotes) assert.equal(preamble, '', 'Unsectioned bootstrap notes');
      for (let index = 0; index < chunks.length; index += 2) {
        const heading = chunks[index].trim();
        if (manualNotes) assert(['Added', 'Fixed', 'Changed'].includes(heading), 'Unsupported bootstrap section');
        const notes = chunks[index + 1].trim();
        sections.set(heading, [sections.get(heading), notes].filter(Boolean).join('\n\n'));
      }
      return preamble;
    }
    const header = collect(original, false);
    collect(manual, true);
    const migrated = [header, ...[...sections].map(([heading, notes]) => `### ${heading}\n\n${notes}`)].join('\n\n');
    changelog = `# Changelog\n\n${migrated}\n`;
  }

  const releases = [...changelog.matchAll(releaseHeading)];
  assert(releases.length > 0, 'Missing versioned changelog entry');
  const start = releases[0].index;
  const end = releases[1]?.index ?? changelog.length;
  const notes = changelog.slice(start, end).trim();
  const heading = releases[0][0];
  const bodyIndex = body.indexOf(heading);
  assert(bodyIndex >= 0, 'Generated release notes missing from PR body');
  assert.equal(body.indexOf(heading, bodyIndex + heading.length), -1, 'Ambiguous release notes in PR body');
  // The single-root manifest body ends its notes with a horizontal rule and
  // footer. Keep the surrounding robot header, whitespace and card footer.
  const separator = body.indexOf('\n---', bodyIndex);
  const bodyEnd = separator < 0 ? body.length : separator;
  const original = body.slice(bodyIndex, bodyEnd).trimEnd();
  assert.equal([...original.matchAll(releaseHeading)].length, 1, 'Unexpected multi-release PR body');
  return {
    changelog,
    body: body.slice(0, bodyIndex) + notes + body.slice(bodyIndex + original.length),
  };
}

module.exports = {migrateReleaseNotes, parseOverflowLink, resolveNotesBody, NATIVE_NOTES_BRANCH, NATIVE_NOTES_FILE, NATIVE_OVERFLOW_SENTENCE};

if (require.main === module) {
  const [changelogPath, bodyPath, notesPath] = process.argv.slice(2);
  assert(changelogPath && bodyPath, 'Usage: migrate-release-notes.cjs CHANGELOG.md pr-body.md [release-notes.md]');
  const changelog = fs.readFileSync(changelogPath, 'utf8');
  const visibleBody = fs.readFileSync(bodyPath, 'utf8');
  // Preserve the native overflow contract: stored notes are the source of
  // truth, so a migrated overflow reconciles the notes file, never the link.
  // A stale notes branch alongside a normal body is left alone entirely.
  const link = parseOverflowLink(visibleBody);
  if (link && !notesPath) assert.fail('Overflow PR body without stored release notes file');
  const body = link
    ? resolveNotesBody(visibleBody, () => fs.readFileSync(notesPath, 'utf8'))
    : visibleBody;
  const migrated = migrateReleaseNotes(changelog, body);
  if (migrated.changelog !== changelog) fs.writeFileSync(changelogPath, migrated.changelog);
  if (link) {
    if (migrated.body !== body) fs.writeFileSync(notesPath, migrated.body);
  } else if (migrated.body !== visibleBody) {
    fs.writeFileSync(bodyPath, migrated.body);
  }
}
