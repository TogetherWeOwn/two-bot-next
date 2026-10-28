"""Blind acceptance for README-linked docs sections; no checker implementation here.

This file owns the fragment contract. Keep test_docs_index.py unchanged: its
single-index fixtures are the backwards-compatibility control. Run with:
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_index_fragments.py' -v
"""

import json
import os
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from check_docs_index import DOC_LIMIT, INDEX_LIMIT, ROOT, check


CLEAN = {"missing": [], "stale": [], "oversize": []}
ROW = re.compile(r"- \[([^\]\n]+)\]\(([A-Za-z0-9_./-]+\.md)\)")
FRAGMENT = re.compile(r"index-[^/]+\.md")


class FragmentContractTests(unittest.TestCase):
    def setUp(self):
        scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("PAPERCLIP_SCRATCH_DIR")
        self.temp = tempfile.TemporaryDirectory(dir=scratch)
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.write("README.md", "# Docs\n\n- [Navigation](README.md)\n"
                   "- [Z section](index-z.md)\n- [A section](index-a.md)\n")
        self.write("index-z.md", "# Z\n\n- [One](one.md)\n- [Nested](nested/two.md)\n")
        self.write("index-a.md", "# A\n\n- [Three](three.md)\n")
        for name in ("one.md", "nested/two.md", "three.md"):
            self.write(name, "# Guide\n")

    def write(self, name, text):
        path = self.root / "docs" / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        return path

    def assert_report(self, *, missing=(), stale=(), oversize=(), **kwargs):
        self.assertEqual(check(self.root, **kwargs), {
            "missing": sorted(missing), "stale": sorted(stale), "oversize": sorted(oversize),
        })

    def cli(self, *extra):
        result = subprocess.run(
            [sys.executable, str(ROOT / "scripts/check_docs_index.py"),
             "--root", str(self.root), "--json", *extra],
            capture_output=True, text=True, timeout=30,
        )
        self.assertEqual(result.stderr, "", result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(set(report), set(CLEAN))
        for entries in report.values():
            self.assertIsInstance(entries, list)
            self.assertEqual(entries, sorted(entries))
        self.assertEqual(result.returncode, int(any(report.values())), result.stdout)
        return report

    def test_legacy_single_index_still_passes(self):
        self.write("README.md", "# Docs\n\n- [Navigation](README.md)\n"
                   "- [One](one.md)\n- [Nested](nested/two.md)\n- [Three](three.md)\n")
        for name in ("index-z.md", "index-a.md"):
            (self.root / "docs" / name).unlink()
        self.assert_report()
        self.assertEqual(self.cli(), CLEAN)

    def test_linked_sections_preserve_every_guide_row(self):
        self.assert_report()
        self.assertEqual(self.cli(), CLEAN)
        for section, row, target in (
            ("index-z.md", "- [One](one.md)\n", "one.md"),
            ("index-z.md", "- [Nested](nested/two.md)\n", "nested/two.md"),
            ("index-a.md", "- [Three](three.md)\n", "three.md"),
        ):
            with self.subTest(target=target):
                path = self.root / "docs" / section
                original = path.read_text(encoding="utf-8")
                path.write_text(original.replace(row, ""), encoding="utf-8")
                self.assert_report(missing=[f"docs/{target}"])
                path.write_text(original, encoding="utf-8")

    def test_removed_guide_row_names_only_that_missing_guide(self):
        self.write("index-z.md", "# Z\n\n- [One](one.md)\n")
        self.assert_report(missing=["docs/nested/two.md"])

    def test_unlinked_fragment_is_missing_and_its_rows_are_not_assembled(self):
        self.write("index-orphan.md", "# Orphan\n\n- [Hidden](hidden.md)\n"
                   "- [Broken](not-on-disk.md)\n- [Duplicate](one.md)\n")
        self.write("hidden.md", "# Hidden\n")
        self.assert_report(missing=["docs/hidden.md", "docs/index-orphan.md"])

    def test_linked_but_absent_fragment_is_stale(self):
        self.write("README.md", (self.root / "docs/README.md").read_text()
                   + "- [Absent](index-absent.md)\n")
        self.assert_report(stale=["docs/index-absent.md"])

    def test_broken_destination_inside_fragment_is_stale(self):
        self.write("index-a.md", "# A\n\n- [Three](three.md)\n- [Broken](absent.md)\n")
        self.assert_report(stale=["docs/absent.md"])

    def test_duplicate_is_at_later_fragment_in_readme_order_not_filename_order(self):
        self.write("index-a.md", "# A\n\n- [Three](three.md)\n- [Again](one.md)\n")
        self.assert_report(stale=["docs/index-a.md:4"])
        self.write("README.md", "# Docs\n\n- [Navigation](README.md)\n"
                   "- [A section](index-a.md)\n- [Z section](index-z.md)\n")
        self.assert_report(stale=["docs/index-z.md:3"])

    def test_duplicate_after_fragment_is_at_later_readme_row(self):
        self.write("README.md", (self.root / "docs/README.md").read_text()
                   + "- [Again](one.md)\n")
        self.assert_report(stale=["docs/README.md:6"])

    def test_only_docs_root_anchored_fragment_links_are_assembled(self):
        for target in ("nested/index-extra.md", "guide-index-extra.md"):
            with self.subTest(target=target):
                path = self.write(target, "# Ordinary guide\n\n- [Broken](not-on-disk.md)\n")
                index = self.root / "docs/README.md"
                original = index.read_text()
                index.write_text(original + f"- [Ordinary]({target})\n")
                self.assert_report()
                index.write_text(original)
                path.unlink()

    def test_fragment_rows_are_not_recursive_section_links(self):
        self.write("index-a.md", "# A\n\n- [Three](three.md)\n- [Leaf](index-leaf.md)\n")
        self.write("index-leaf.md", "# Leaf\n\n- [Hidden](hidden.md)\n")
        self.write("hidden.md", "# Hidden\n")
        self.assert_report(missing=["docs/hidden.md", "docs/index-leaf.md"])

    def test_fragment_uses_the_same_strict_full_line_row_rule(self):
        cases = (
            "- [](fresh.md)", "- [   ](fresh.md)", "- [Escape](../outside.md)",
            "- [Absolute](/outside.md)", "- [URL](https://example.com/doc.md)",
            "- [No purpose] fresh.md", "- [Trailing](fresh.md) junk",
            "- [Dot](nested/./two.md)", "- [Slash](nested//two.md)",
        )
        for row in cases:
            with self.subTest(row=row):
                self.write("index-a.md", "# A\n\n- [Three](three.md)\n" + row + "\n")
                self.assert_report(stale=["docs/index-a.md:4"])

    def test_line_based_rows_in_fences_and_comments_count_just_like_readme(self):
        self.write("index-a.md", "# A\n\n```md\n- [Three](three.md)\n```\n"
                   "<!--\n- [Four](four.md)\n-->\n")
        self.write("four.md", "# Four\n")
        self.assert_report()

    def test_non_row_mentions_never_index_a_guide(self):
        self.write("index-a.md", "# A\n\nSee [Three](three.md).\n"
                   "  - [Three](three.md)\n* [Three](three.md)\n")
        self.assert_report(missing=["docs/three.md"])

    def test_fragment_utf8_failure_has_stable_json_path(self):
        (self.root / "docs/index-a.md").write_bytes(b"# A\n\xff\xfe\n")
        expected = {"missing": ["docs/three.md"],
                    "stale": ["docs/index-a.md"], "oversize": []}
        self.assertEqual(check(self.root), expected)
        self.assertEqual(self.cli(), expected)

    def test_readme_and_linked_fragment_budget_is_strict_and_not_allowlistable(self):
        self.assertEqual(INDEX_LIMIT, 12_000)
        for name in ("README.md", "index-z.md", "index-a.md"):
            path = self.root / "docs" / name
            original = path.read_bytes()
            for size in (INDEX_LIMIT - 1, INDEX_LIMIT, INDEX_LIMIT + 1):
                with self.subTest(name=name, size=size):
                    # Multibyte padding must be measured as bytes, not characters.
                    remaining = size - len(original)
                    path.write_bytes(original + "é".encode() * (remaining // 2)
                                     + b"x" * (remaining % 2))
                    self.assert_report(
                        oversize=[] if size < INDEX_LIMIT else [f"docs/{name}"],
                        large_docs={f"docs/{name}": "must not bypass index budget"},
                    )
            path.write_bytes(original)

    def test_other_docs_keep_doc_limit_and_allowlist_behavior(self):
        self.assertEqual(DOC_LIMIT, 65_536)
        path = self.root / "docs/one.md"
        path.write_bytes(b"x" * DOC_LIMIT)
        self.assert_report()
        path.write_bytes(b"x" * (DOC_LIMIT + 1))
        self.assert_report(oversize=["docs/one.md"])
        self.assert_report(large_docs={"docs/one.md": "fixture exception"})

    def test_check_is_read_only_and_dedupe_is_idempotent_for_each_linked_section(self):
        original = "# A\n\n\n- [Three](three.md)\n- [Three](three.md)\n\n"
        self.write("index-a.md", original)
        self.assertEqual(self.cli()["stale"], ["docs/index-a.md:5"])
        self.assertEqual((self.root / "docs/index-a.md").read_text(), original)
        orphan = "# Orphan\n\n\n- [No rewrite](absent.md)\n- [No rewrite](absent.md)\n"
        orphan_path = self.write("index-orphan.md", orphan)
        expected = {"missing": ["docs/index-orphan.md"], "stale": [], "oversize": []}
        self.assertEqual(self.cli("--dedupe"), expected)
        self.assertEqual((self.root / "docs/index-a.md").read_text(),
                         "# A\n\n- [Three](three.md)\n")
        self.assertEqual(orphan_path.read_text(), orphan)
        before = {p: p.read_bytes() for p in (self.root / "docs").rglob("*.md")}
        self.assertEqual(self.cli("--dedupe"), expected)
        self.assertEqual({p: p.read_bytes() for p in before}, before)

    def test_dedupe_repairs_readme_and_every_linked_fragment_without_reordering(self):
        originals = {}
        for name in ("README.md", "index-z.md", "index-a.md"):
            path = self.root / "docs" / name
            original = path.read_text()
            originals[name] = original
            last_row = original.splitlines()[-1]
            path.write_text(original + last_row + "\n\n\n")
        self.assertEqual(self.cli("--dedupe"), CLEAN)
        for name, original in originals.items():
            self.assertEqual((self.root / "docs" / name).read_text(), original)
        self.assertEqual(self.cli("--dedupe"), CLEAN)

    def test_dedupe_leaves_undecodable_fragment_untouched_and_fails_closed(self):
        path = self.root / "docs/index-a.md"
        original = b"# A\n\xff\xfe\n"
        path.write_bytes(original)
        self.assertEqual(self.cli("--dedupe"), {
            "missing": ["docs/three.md"], "stale": ["docs/index-a.md"], "oversize": [],
        })
        self.assertEqual(path.read_bytes(), original)

    def test_json_failure_arrays_are_exact_sorted_and_check_never_writes(self):
        self.write("index-a.md", "# A\n\n- [Three](three.md)\n"
                   "- [Z broken](z-absent.md)\n- [A broken](a-absent.md)\n")
        self.write("z-orphan.md", "# Orphan\n")
        self.write("a-orphan.md", "# Orphan\n")
        for name in ("index-z.md", "index-a.md"):
            path = self.root / "docs" / name
            path.write_text(path.read_text().ljust(INDEX_LIMIT))
        before = {p: p.read_bytes() for p in (self.root / "docs").rglob("*.md")}
        self.assertEqual(self.cli(), {
            "missing": ["docs/a-orphan.md", "docs/z-orphan.md"],
            "stale": ["docs/a-absent.md", "docs/z-absent.md"],
            "oversize": ["docs/index-a.md", "docs/index-z.md"],
        })
        self.assertEqual({p: p.read_bytes() for p in before}, before)

    def test_dedupe_preserves_order_and_does_not_guess_different_labels(self):
        original = "# A\n\n- [Three](three.md)\n- [Renamed](three.md)\n"
        self.write("index-a.md", original)
        self.assertEqual(self.cli("--dedupe"), {
            "missing": [], "stale": ["docs/index-a.md:4"], "oversize": [],
        })
        self.assertEqual((self.root / "docs/index-a.md").read_text(), original)

    def test_dedupe_does_not_remove_cross_fragment_target_ambiguity(self):
        self.write("index-a.md", "# A\n\n- [Three](three.md)\n- [One](one.md)\n")
        before = (self.root / "docs/index-a.md").read_bytes()
        self.assertEqual(self.cli("--dedupe")["stale"], ["docs/index-a.md:4"])
        self.assertEqual((self.root / "docs/index-a.md").read_bytes(), before)

    def test_two_independent_section_additions_union_merge_without_central_changes(self):
        central = (self.root / "docs/README.md").read_bytes()
        for fragment, addition in (("index-z.md", "four.md"), ("index-a.md", "five.md")):
            base = (self.root / "docs" / fragment).read_text()
            self.write(addition, "# New guide\n")
            # One branch changes Z, the other A; merge each path against its
            # unchanged peer, rather than appending both rows to the central map.
            ours, theirs = (base + f"- [Added]({addition})\n", base)
            if fragment == "index-a.md":
                ours, theirs = theirs, ours
            paths = []
            for label, text in (("ours", ours), ("base", base), ("theirs", theirs)):
                path = self.root / label
                path.write_text(text)
                paths.append(str(path))
            merged = subprocess.run(["git", "merge-file", "--union", "-p", *paths],
                                    capture_output=True, text=True, timeout=30)
            self.assertEqual(merged.returncode, 0, merged.stderr)
            self.assertFalse(any(marker in merged.stdout for marker in ("<<<<<<<", "=======", ">>>>>>>")))
            self.assertEqual(merged.stdout, base + f"- [Added]({addition})\n")
            self.write(fragment, merged.stdout)
        self.assertEqual((self.root / "docs/README.md").read_bytes(), central)
        self.assertEqual(self.cli("--dedupe"), CLEAN)


class RepositoryFragmentContractTests(unittest.TestCase):
    def test_union_driver_covers_section_fragments(self):
        lines = (ROOT / ".gitattributes").read_text().splitlines()
        self.assertIn("docs/index-*.md merge=union", [line.strip() for line in lines])
        result = subprocess.run(
            ["git", "check-attr", "merge", "--", "docs/index-fixture.md"],
            cwd=ROOT, capture_output=True, text=True, timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "docs/index-fixture.md: merge: union")

    def test_repository_entrypoint_links_sections_and_every_guide_is_discoverable(self):
        index = ROOT / "docs/README.md"
        sections = []
        for line in index.read_text(encoding="utf-8").splitlines():
            row = ROW.fullmatch(line)
            if row and FRAGMENT.fullmatch(row[2]):
                sections.append(ROOT / "docs" / row[2])
        self.assertGreaterEqual(len(sections), 2, "ship section fragments, not only checker support")
        for path in [index, *sections]:
            with self.subTest(index=path.name):
                self.assertTrue(path.is_file())
                self.assertLess(path.stat().st_size, INDEX_LIMIT)
        self.assertEqual(check(ROOT), CLEAN, "the split must leave no orphaned guide")


if __name__ == "__main__":
    unittest.main()
