"""Relative links in the cutover runbooks resolve to a real file, heading or line.

Run: python3 -m unittest discover -s scripts -p 'test_cutover_doc_links.py' -v
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CUTOVER_DOCS = sorted((ROOT / "docs").glob("cutover*.md"))
LINK = re.compile(r"\]\(([^)\s]+)\)")
SCHEME = re.compile(r"[a-z][a-z0-9+.-]*:", re.IGNORECASE)
LINE_TARGET = re.compile(r"L(\d+)(?:-L\d+)?")
HEADING = re.compile(r"#{1,6}\s+(.*?)\s*#*\s*$")
# Internal tracker links are a repo-wide convention that only resolves inside Paperclip.
INTERNAL_TRACKER = "/TOG/issues/"


def _prose_lines(text):
    fenced = False
    for number, line in enumerate(text.splitlines(), start=1):
        if line.lstrip().startswith("```"):
            fenced = not fenced
        elif not fenced:
            yield number, line


def _slug(heading):
    return re.sub(r"[^\w\- ]", "", heading.strip().lower()).replace(" ", "-")


def _heading_anchors(path):
    anchors, seen = set(), {}
    for _, line in _prose_lines(path.read_text(encoding="utf-8")):
        match = HEADING.match(line)
        if match:
            slug = _slug(match.group(1))
            count = seen.get(slug, 0)
            anchors.add(slug if count == 0 else f"{slug}-{count}")
            seen[slug] = count + 1
    return anchors


def _problems(doc, text):
    found = []
    for number, line in _prose_lines(text):
        for target in LINK.findall(line):
            if SCHEME.match(target) or target.startswith(INTERNAL_TRACKER):
                continue
            path, _, anchor = target.partition("#")
            resolved = (doc.parent / path).resolve() if path else doc
            where = f"{doc.name}:{number}: {target}"
            if not resolved.exists():
                found.append(f"{where} -> missing file")
            elif resolved.suffix == ".md" and anchor:
                if anchor not in _heading_anchors(resolved):
                    found.append(f"{where} -> no such heading")
            elif anchor and LINE_TARGET.fullmatch(anchor):
                lines = resolved.read_text(encoding="utf-8", errors="replace").splitlines()
                line_no = int(anchor[1:].split("-")[0])
                if line_no > len(lines):
                    found.append(f"{where} -> line beyond end of file ({len(lines)} lines)")
                elif not re.search(r"\w", lines[line_no - 1]):
                    found.append(f"{where} -> points at a blank or punctuation-only line")
            elif anchor:
                found.append(f"{where} -> unsupported anchor")
    return found


class CutoverDocLinks(unittest.TestCase):
    def test_cutover_runbook_is_found(self):
        self.assertIn(ROOT / "docs" / "cutover.md", CUTOVER_DOCS)

    def test_every_relative_link_in_cutover_docs_resolves(self):
        problems = []
        for doc in CUTOVER_DOCS:
            problems += _problems(doc, doc.read_text(encoding="utf-8"))
        self.assertEqual(problems, [])

    def test_the_checker_reports_stale_targets_and_accepts_live_ones(self):
        doc = ROOT / "docs" / "cutover-link-fixture.md"
        stale = (
            "[backup](backup.md#coverage-and-recovery-semantics-v4-tog-11142)\n"
            "[overrun](../crates/core/src/backup/dump_file.rs#L99999)\n"
            "[closer](../crates/core/src/config.rs#L34)\n"
        )
        live = (
            "[backup](backup.md#coverage-and-recovery-semantics-v4-and-v5)\n"
            "[table](../crates/core/src/backup/dump_file.rs#L44)\n"
            "[tracker](/TOG/issues/TOG-9699)\n"
        )
        self.assertEqual(len(_problems(doc, stale)), 3)
        self.assertEqual(_problems(doc, live), [])


if __name__ == "__main__":
    unittest.main()
