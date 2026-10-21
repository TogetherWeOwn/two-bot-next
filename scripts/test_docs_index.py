"""Offline navigation, orphan and byte-budget regressions; synthetic fixtures only."""

import json
import os
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from check_docs_index import DOC_LIMIT, INDEX_LIMIT, LARGE_DOCS, ROOT, check


class DocsIndexTests(unittest.TestCase):
    def test_repository_index_and_budgets(self):
        self.assertEqual(check(ROOT), {"missing": [], "stale": [], "oversize": []})
        self.assertLess((ROOT / "docs/README.md").stat().st_size, INDEX_LIMIT)

    def test_allowlist_is_explicit_and_tight(self):
        for name, reason in LARGE_DOCS.items():
            with self.subTest(doc=name):
                self.assertTrue(reason.strip())
                self.assertGreater((ROOT / name).stat().st_size, DOC_LIMIT)

    def test_root_entry_points_link_the_index(self):
        for name in ("README.md", "AGENTS.md"):
            with self.subTest(doc=name):
                self.assertRegex((ROOT / name).read_text(), r"\[[^\]]+\]\(docs/README\.md\)")

    def test_large_documents_have_complete_start_here_navigation(self):
        for name in ("parity.md", "soak-checklist.md", "runbook.md"):
            with self.subTest(doc=name):
                text = (ROOT / "docs" / name).read_text()
                toc = text.split("<!-- start-here -->")[1].split("<!-- /start-here -->")[0]
                self.assertIn("Start here", toc)
                self.assertLess(text.index("<!-- start-here -->"), 500)
                headings = re.findall(r"^## (.+)$", text, re.M)
                if name == "runbook.md":
                    headings += re.findall(r"^#### (Alert: .+)$", text, re.M)
                for heading in headings:
                    anchor = re.sub(r"\s", "-", re.sub(r"[^\w\s-]", "", heading.lower()))
                    self.assertIn(f"](#{anchor})", toc, heading)

    def test_alert_navigation_does_not_shadow_section_readers(self):
        text = (ROOT / "docs/runbook.md").read_text()
        toc = text.split("<!-- start-here -->")[1].split("<!-- /start-here -->")[0]
        # Existing alert conformance readers locate bodies by their title text.
        # Short labels under "Alert anchors" must not become the first match.
        self.assertNotIn("Alert:", toc)


class IndexFixtureTests(unittest.TestCase):
    def setUp(self):
        scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("PAPERCLIP_SCRATCH_DIR")
        self.temp = tempfile.TemporaryDirectory(dir=scratch)
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "docs/nested").mkdir(parents=True)
        self.index = self.root / "docs/README.md"
        self.index.write_text("# Docs\n\n## Area\n- [Navigation](README.md)\n- [Fixture](nested/one.md)\n")
        self.doc = self.root / "docs/nested/one.md"
        self.doc.write_text("# Fixture\n")

    def test_recursive_inventory_includes_index_itself(self):
        self.assertEqual(check(self.root, large_docs={}), {"missing": [], "stale": [], "oversize": []})
        self.index.write_text(self.index.read_text().replace("- [Navigation](README.md)\n", ""))
        self.assertEqual(check(self.root, large_docs={})["missing"], ["docs/README.md"])

    def test_removed_row_and_new_nested_orphan_are_named(self):
        self.index.write_text(self.index.read_text().replace("- [Fixture](nested/one.md)\n", ""))
        (self.root / "docs/nested/orphan.md").write_text("# New doc\n")
        self.assertEqual(check(self.root, large_docs={})["missing"],
                         ["docs/nested/one.md", "docs/nested/orphan.md"])

    def test_deleted_target_is_stale(self):
        self.doc.unlink()
        self.assertEqual(check(self.root, large_docs={})["stale"], ["docs/nested/one.md"])

    def test_budget_counts_bytes_not_characters_and_allows_exact_boundary(self):
        self.doc.write_bytes(b"x" * DOC_LIMIT)
        self.assertEqual(check(self.root, large_docs={})["oversize"], [])
        self.doc.write_text("é" * (DOC_LIMIT // 2 + 1), encoding="utf-8")
        self.assertEqual(check(self.root, large_docs={})["oversize"], ["docs/nested/one.md"])
        self.assertEqual(check(self.root, large_docs={"docs/nested/one.md": "fixture"})["oversize"], [])

    def test_index_budget_cannot_be_allowlisted(self):
        self.index.write_text(self.index.read_text().ljust(INDEX_LIMIT))
        self.assertEqual(check(self.root, large_docs={"docs/README.md": "fixture"})["oversize"], ["docs/README.md"])

    def test_malformed_empty_duplicate_or_escaping_rows_fail_closed(self):
        for row in ("- [](nested/one.md)", "- [   ](nested/one.md)",
                    "- [Escape](../outside.md)", "- [Absolute](/outside.md)",
                    "- [URL](https://example.com/doc.md)", "- [No purpose] nested/one.md",
                    "- [Duplicate](nested/one.md)"):
            with self.subTest(row=row):
                self.index.write_text("# Docs\n\n## Area\n- [Navigation](README.md)\n- [Fixture](nested/one.md)\n" + row + "\n")
                self.assertTrue(check(self.root, large_docs={})["stale"])

    def test_json_cli_has_only_stable_arrays_and_nonzero_failure(self):
        command = [sys.executable, str(ROOT / "scripts/check_docs_index.py"), "--root", str(self.root), "--json"]
        passed = subprocess.run(command, capture_output=True, text=True)
        self.assertEqual(passed.returncode, 0, passed.stderr)
        self.assertEqual(json.loads(passed.stdout), {"missing": [], "stale": [], "oversize": []})
        self.doc.unlink()
        failed = subprocess.run(command, capture_output=True, text=True)
        self.assertEqual(failed.returncode, 1, failed.stderr)
        self.assertEqual(json.loads(failed.stdout), {"missing": [], "stale": ["docs/nested/one.md"], "oversize": []})


if __name__ == "__main__":
    unittest.main()
