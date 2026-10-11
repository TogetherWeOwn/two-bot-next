"""Relative links in the cutover runbooks resolve to a real file, heading or line.

Run: python3 -m unittest discover -s scripts -p 'test_cutover_doc_links.py' -v
"""

import re
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CUTOVER_DOCS = sorted((ROOT / "docs").glob("cutover*.md"))
LINK = re.compile(r"\]\(([^)\s]+)\)")
SCHEME = re.compile(r"[a-z][a-z0-9+.-]*:", re.IGNORECASE)
LINE_TARGET = re.compile(r"L(\d+)(?:-L(\d+))?")
HEADING = re.compile(r"#{1,6}\s+(.*?)\s*#*\s*$")
# Internal tracker links are a repo-wide convention that only resolves inside Paperclip.
INTERNAL_TRACKER = "/TOG/issues/"


def _prose_lines(text):
    fenced = False
    for number, line in enumerate(text.splitlines(), start=1):
        if line.lstrip().startswith(("```", "~~~")):
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
            elif resolved.is_dir():
                if anchor:
                    found.append(f"{where} -> anchor on a directory")
            elif resolved.suffix == ".md" and anchor:
                if anchor not in _heading_anchors(resolved):
                    found.append(f"{where} -> no such heading")
            elif anchor and LINE_TARGET.fullmatch(anchor):
                lines = resolved.read_text(encoding="utf-8", errors="replace").splitlines()
                match = LINE_TARGET.fullmatch(anchor)
                start = int(match.group(1))
                end = int(match.group(2) or start)
                if start < 1 or end < start:
                    found.append(f"{where} -> invalid line range")
                elif end > len(lines):
                    found.append(f"{where} -> line beyond end of file ({len(lines)} lines)")
                elif not re.search(r"\w", lines[start - 1]):
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
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "docs").mkdir()
            (root / "crates").mkdir()
            (root / "docs" / "backup-fixture.md").write_text(
                "# Backup\n\n## Coverage and recovery semantics (v4 and v5)\n", encoding="utf-8"
            )
            (root / "crates" / "table.rs").write_text(
                'pub const TABLES: &[&str] = &[\n    "a",\n];\n', encoding="utf-8"
            )
            doc = root / "docs" / "cutover-fixture.md"
            stale = (
                "[backup](backup-fixture.md#coverage-and-recovery-semantics-v4-old-suffix)\n"
                "[overrun](../crates/table.rs#L99)\n"
                "[closer](../crates/table.rs#L3)\n"
                "[reversed](../crates/table.rs#L3-L1)\n"
                "[range](../crates/table.rs#L1-L99)\n"
                "[directory](../crates#L1)\n"
                "~~~\n"
                "[fenced](../crates/table.rs#L99)\n"
                "~~~\n"
            )
            live = (
                "[backup](backup-fixture.md#coverage-and-recovery-semantics-v4-and-v5)\n"
                "[table](../crates/table.rs#L1)\n"
                "[tracker](/TOG/issues/fixture-link)\n"
                "[directory](../crates)\n"
            )
            self.assertEqual(len(_problems(doc, stale)), 6)
            self.assertEqual(_problems(doc, live), [])


if __name__ == "__main__":
    unittest.main()
