'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const releaseHeading = /^##? \[?v?\d+\.\d+\.\d+.*$/gm;

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

module.exports = {migrateReleaseNotes};

if (require.main === module) {
  const [changelogPath, bodyPath] = process.argv.slice(2);
  assert(changelogPath && bodyPath, 'Usage: migrate-release-notes.cjs CHANGELOG.md pr-body.md');
  const changelog = fs.readFileSync(changelogPath, 'utf8');
  const body = fs.readFileSync(bodyPath, 'utf8');
  const migrated = migrateReleaseNotes(changelog, body);
  if (migrated.changelog !== changelog) fs.writeFileSync(changelogPath, migrated.changelog);
  if (migrated.body !== body) fs.writeFileSync(bodyPath, migrated.body);
}
