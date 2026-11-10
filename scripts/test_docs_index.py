"""Offline navigation, orphan and byte-budget regressions; synthetic fixtures only."""

import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from check_docs_index import DOC_LIMIT, INDEX_LIMIT, LARGE_DOCS, ROOT, check, dedupe, dedupe_text


class DocsIndexTests(unittest.TestCase):
    def test_repository_index_and_budgets(self):
        self.assertEqual(check(ROOT), {"missing": [], "stale": [], "oversize": []})
        self.assertLess((ROOT / "docs/README.md").stat().st_size, INDEX_LIMIT)

    def test_allowlist_is_explicit_and_tight(self):
        for name, reason in LARGE_DOCS.items():
            with self.subTest(doc=name):
                self.assertTrue(reason.strip())
                self.assertGreater((ROOT / name).stat().st_size, DOC_LIMIT)

    def test_gitattributes_pins_union_merge_for_the_index(self):
        lines = (ROOT / ".gitattributes").read_text().splitlines()
        self.assertIn("docs/README.md merge=union", [line.strip() for line in lines])

    def test_repository_index_is_dedupe_canonical(self):
        text = (ROOT / "docs/README.md").read_text(encoding="utf-8")
        self.assertEqual(dedupe_text(text), text)

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
        # Each malformed row that names a plain relative path gets a dedicated
        # existing target, so a removed guard cannot hide behind the valid
        # nested/one.md row; the assertion pins the exact stale line form.
        cases = (
            ("- [](fresh-blank.md)", "fresh-blank.md", False),
            ("- [   ](fresh-spaces.md)", "fresh-spaces.md", False),
            ("- [Escape](../outside.md)", None, False),
            ("- [Absolute](/outside.md)", None, False),
            ("- [URL](https://example.com/doc.md)", None, False),
            ("- [No purpose] nested/one.md", None, False),
            ("- [Fixture](fresh-trailing.md) trailing junk", "fresh-trailing.md", False),
            ("- [Dot](nested/./one.md)", None, False),
            ("- [Duplicate](nested/one.md)", None, True),
        )
        for row, fresh, keep_valid in cases:
            with self.subTest(row=row):
                if fresh:
                    (self.root / "docs" / fresh).write_text("# Fresh\n")
                body = "# Docs\n\n## Area\n- [Navigation](README.md)\n"
                if keep_valid:
                    body += "- [Fixture](nested/one.md)\n"
                self.index.write_text(body + row + "\n")
                self.assertEqual(check(self.root, large_docs={})["stale"],
                                 [f"docs/README.md:{body.count(chr(10)) + 1}"])

    def test_uppercase_extension_orphan_is_named_missing(self):
        (self.root / "docs/UPPER.MD").write_text("# Upper\n")
        self.assertEqual(check(self.root, large_docs={})["missing"], ["docs/UPPER.MD"])
        self.index.write_text(self.index.read_text() + "- [Upper](UPPER.MD)\n")
        report = check(self.root, large_docs={})
        self.assertEqual(report["missing"], ["docs/UPPER.MD"])
        self.assertEqual(report["stale"], ["docs/README.md:6"])

    def test_undecodable_index_reports_stale_and_keeps_json_shape(self):
        self.index.write_bytes(b"# Docs\n- [Bad](nested/one.md)\n\xff\xfe\n")
        report = check(self.root, large_docs={})
        self.assertIn("docs/README.md", report["stale"])
        command = [sys.executable, str(ROOT / "scripts/check_docs_index.py"), "--root", str(self.root), "--json"]
        failed = subprocess.run(command, capture_output=True, text=True)
        self.assertEqual(failed.returncode, 1, failed.stderr)
        self.assertEqual(failed.stderr, "")
        self.assertEqual(json.loads(failed.stdout), report)

    def test_json_cli_has_only_stable_arrays_and_nonzero_failure(self):
        command = [sys.executable, str(ROOT / "scripts/check_docs_index.py"), "--root", str(self.root), "--json"]
        passed = subprocess.run(command, capture_output=True, text=True)
        self.assertEqual(passed.returncode, 0, passed.stderr)
        self.assertEqual(json.loads(passed.stdout), {"missing": [], "stale": [], "oversize": []})
        self.doc.unlink()
        failed = subprocess.run(command, capture_output=True, text=True)
        self.assertEqual(failed.returncode, 1, failed.stderr)
        self.assertEqual(json.loads(failed.stdout), {"missing": [], "stale": ["docs/nested/one.md"], "oversize": []})


class DedupeTextTests(unittest.TestCase):
    def test_exact_duplicates_removed_first_occurrence_wins_order_preserved(self):
        text = ("# Docs\n\n## Area\n- [One](one.md)\n- [Two](two.md)\n"
                "## Area\n- [One](one.md)\n- [Three](three.md)\n")
        self.assertEqual(dedupe_text(text),
                         "# Docs\n\n## Area\n- [One](one.md)\n- [Two](two.md)\n- [Three](three.md)\n")

    def test_blank_runs_collapse_and_exactly_one_trailing_newline(self):
        self.assertEqual(dedupe_text("# Docs\n\n\n- [One](one.md)\n\n\n"), "# Docs\n\n- [One](one.md)\n")
        self.assertEqual(dedupe_text("# Docs"), "# Docs\n")
        self.assertEqual(dedupe_text(""), "")

    def test_dedupe_is_idempotent(self):
        text = "# Docs\n\n\n## Area\n- [One](one.md)\n## Area\n- [One](one.md)"
        self.assertEqual(dedupe_text(dedupe_text(text)), dedupe_text(text))

    def test_same_target_with_different_labels_is_never_guessed(self):
        text = "# Docs\n\n- [Old label](one.md)\n- [New label](one.md)\n"
        self.assertEqual(dedupe_text(text), text)


class DedupeFixtureTests(unittest.TestCase):
    def setUp(self):
        scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("PAPERCLIP_SCRATCH_DIR")
        self.temp = tempfile.TemporaryDirectory(dir=scratch)
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "docs").mkdir(parents=True)
        self.index = self.root / "docs/README.md"
        (self.root / "docs/one.md").write_text("# One\n")
        (self.root / "docs/two.md").write_text("# Two\n")

    def cli(self, *extra):
        command = [sys.executable, str(ROOT / "scripts/check_docs_index.py"),
                   "--root", str(self.root), *extra]
        return subprocess.run(command, capture_output=True, text=True)

    def test_union_artifact_is_flagged_then_repaired_end_to_end(self):
        self.index.write_text("# Docs\n\n## Area\n- [Navigation](README.md)\n- [One](one.md)\n"
                              "## Area\n- [One](one.md)\n- [Two](two.md)\n")
        self.assertEqual(check(self.root, large_docs={})["stale"], ["docs/README.md:7"])
        self.assertTrue(dedupe(self.root))
        self.assertEqual(check(self.root, large_docs={}), {"missing": [], "stale": [], "oversize": []})
        self.assertFalse(dedupe(self.root), "second dedupe must be a no-op")

    def test_dedupe_cli_keeps_json_shape_and_exit_zero_after_repair(self):
        self.index.write_text("# Docs\n\n- [Navigation](README.md)\n- [One](one.md)\n- [One](one.md)\n- [Two](two.md)\n")
        repaired = self.cli("--dedupe", "--json")
        self.assertEqual(repaired.returncode, 0, repaired.stderr)
        self.assertEqual(json.loads(repaired.stdout), {"missing": [], "stale": [], "oversize": []})

    def test_dedupe_cli_still_fails_on_rows_it_must_not_guess(self):
        self.index.write_text("# Docs\n\n- [Navigation](README.md)\n- [Old](one.md)\n- [New](one.md)\n- [Two](two.md)\n")
        failed = self.cli("--dedupe", "--json")
        self.assertEqual(failed.returncode, 1, failed.stderr)
        self.assertEqual(json.loads(failed.stdout)["stale"], ["docs/README.md:5"])
        self.assertIn("- [Old](one.md)", self.index.read_text())
        self.assertIn("- [New](one.md)", self.index.read_text())

    def test_dedupe_cli_leaves_undecodable_index_untouched(self):
        original = b"# Docs\n- [One](one.md)\n\xff\xfe\n"
        self.index.write_bytes(original)
        failed = self.cli("--dedupe", "--json")
        self.assertEqual(failed.returncode, 1, failed.stderr)
        self.assertEqual(self.index.read_bytes(), original)
        self.assertIn("docs/README.md", json.loads(failed.stdout)["stale"])

    def test_dedupe_cli_with_missing_or_directory_index_keeps_json_shape(self):
        failed = self.cli("--dedupe", "--json")
        self.assertEqual(failed.returncode, 1, failed.stderr)
        self.assertNotIn("Traceback", failed.stderr)
        self.assertIn("docs/README.md", json.loads(failed.stdout)["missing"])
        self.index.mkdir()
        failed = self.cli("--dedupe", "--json")
        self.assertEqual(failed.returncode, 1, failed.stderr)
        self.assertNotIn("Traceback", failed.stderr)
        self.assertIn("docs/README.md", json.loads(failed.stdout)["missing"])
        self.assertTrue(self.index.is_dir())

    def test_check_cli_without_dedupe_flag_never_writes(self):
        body = "# Docs\n\n- [Navigation](README.md)\n- [One](one.md)\n- [One](one.md)\n- [Two](two.md)\n"
        self.index.write_text(body)
        failed = self.cli("--json")
        self.assertEqual(failed.returncode, 1, failed.stderr)
        self.assertEqual(self.index.read_text(), body)

    @unittest.skipUnless(shutil.which("git"), "git is required for the union merge proof")
    def test_git_union_merge_of_parallel_row_insertions_has_no_markers(self):
        scratch = Path(self.temp.name)
        base = "# Docs\n\n## Area\n- [Navigation](README.md)\n- [One](one.md)\n"
        ours = base + "- [Two](two.md)\n"
        (self.root / "docs/three.md").write_text("# Three\n")
        theirs = base + "- [Three](three.md)\n"
        for name, text in (("base", base), ("ours", ours), ("theirs", theirs)):
            (scratch / name).write_text(text)
        merged = subprocess.run(
            ["git", "merge-file", "--union", "-p",
             str(scratch / "ours"), str(scratch / "base"), str(scratch / "theirs")],
            capture_output=True, text=True)
        self.assertEqual(merged.returncode, 0, merged.stderr)
        self.assertNotIn("<<<<<<<", merged.stdout)
        self.index.write_text(dedupe_text(merged.stdout))
        self.assertEqual(check(self.root, large_docs={}), {"missing": [], "stale": [], "oversize": []})


if __name__ == "__main__":
    unittest.main()
