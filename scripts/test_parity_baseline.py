#!/usr/bin/env python3
"""Offline Git fixtures for the parity ancestry and link guard."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

from check_parity_baseline import BEGIN, END, ParityError, check


class ParityBaselineTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(dir=os.getenv("PAPERCLIP_RUN_SCRATCH_DIR"))
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = self.root / "legacy"
        self.repo.mkdir()
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        (self.repo / "src").mkdir()
        source = self.repo / "src/example.ts"
        source.write_text("root\n")
        self.git("add", ".")
        self.git("commit", "-qm", "root")
        root = self.git("rev-parse", "HEAD")
        self.git("checkout", "-qb", "old")
        source.write_text("frozen\n")
        self.git("commit", "-qam", "old freeze")
        self.old = self.git("rev-parse", "HEAD")
        self.git("checkout", "-q", "main")
        self.assertEqual(self.git("rev-parse", "HEAD"), root)
        source.write_text("frozen\n")
        self.git("commit", "-qam", "rewritten freeze")
        self.snapshot = self.git("rev-parse", "HEAD")
        source.write_text("frozen\nfix\n")
        self.git("commit", "-qam", "post-freeze fix")
        self.baseline = self.git("rev-parse", "HEAD")
        self.config = {"version": 1, "repository": "TogetherWeOwn/two-bot",
                       "historyStart": self.old, "registrySnapshot": self.snapshot,
                       "baseline": self.baseline}
        (self.root / "docs").mkdir()
        (self.root / "crates/core/tests/fixtures").mkdir(parents=True)
        self.fixture = self.root / "crates/core/tests/fixtures/legacy_registry.json"
        self.fixture.write_text(json.dumps({"revision": self.snapshot}))
        self.rows = [self.row(self.snapshot), self.row(self.baseline)]
        self.links = [f"https://github.com/TogetherWeOwn/two-bot/tree/{self.baseline}",
                      f"https://github.com/TogetherWeOwn/two-bot/blob/{self.baseline}/src/example.ts#L1-L2"]
        self.save()

    def git(self, *args):
        return subprocess.check_output(
            ["git", "-C", str(self.repo), *args], text=True, stderr=subprocess.PIPE
        ).strip()

    def row(self, sha, status="carded", disposition="[TOG-1](/TOG/issues/TOG-1) — fixture"):
        return f"| [{sha[:7]}](https://github.com/TogetherWeOwn/two-bot/commit/{sha}) | `src/` | fixture | {status} | {disposition} |"

    def save(self):
        (self.root / "docs/parity-baseline.json").write_text(json.dumps(self.config))
        text = "\n".join(f"[source]({url})" for url in self.links)
        (self.root / "docs/parity.md").write_text(
            text + "\n" + BEGIN + "\n" + "\n".join(self.rows) + "\n" + END + "\n"
        )

    def fails(self, message):
        self.save()
        with self.assertRaisesRegex(ParityError, message):
            check(self.root, self.repo)

    def test_rewritten_history_and_complete_ledger_pass(self):
        result = check(self.root, self.repo)
        self.assertEqual(result["ledgerRows"], 2)
        self.assertEqual(result["legacyLinks"], 4)
        self.assertNotEqual(self.old, self.snapshot)

    def test_orphaned_baseline_fails(self):
        self.git("update-ref", "refs/heads/main", self.snapshot)
        self.fails("merge-base")

    def test_orphaned_permalink_fails(self):
        self.links.append(f"https://github.com/TogetherWeOwn/two-bot/tree/{self.old}")
        self.fails("merge-base")

    def test_missing_ledger_row_fails(self):
        self.rows.pop()
        self.fails("coverage mismatch")

    def test_extra_ledger_row_fails(self):
        self.rows.append(self.row(self.git("rev-parse", self.snapshot + "^")))
        self.fails("coverage mismatch")

    def test_duplicate_ledger_row_fails(self):
        self.rows.append(self.rows[0])
        self.fails("duplicate")

    def test_unowned_gap_fails(self):
        self.rows[1] = self.row(self.baseline, "gap", "not ported")
        self.fails("gap needs a card")

    def test_gap_with_card_passes(self):
        self.rows[1] = self.row(self.baseline, "gap")
        self.save()
        check(self.root, self.repo)

    def test_gap_with_drop_reason_passes(self):
        self.rows[1] = self.row(self.baseline, "gap", "drop: replaced staging driver")
        self.save()
        check(self.root, self.repo)

    def test_carded_without_card_fails(self):
        self.rows[1] = self.row(self.baseline, "carded", "someone should port this")
        self.fails("carded row needs a card")

    def test_mismatched_card_label_and_target_fail(self):
        self.rows[1] = self.row(self.baseline, "carded", "[TOG-1](/TOG/issues/TOG-2)")
        self.fails("carded row needs a card")

    def test_drop_without_reason_fails(self):
        self.rows[1] = self.row(self.baseline, "dropped", "drop: ")
        self.fails("explicit drop reason")

    def test_unknown_status_fails(self):
        self.rows[1] = self.row(self.baseline, "complete")
        self.fails("unknown status")

    def test_missing_source_file_fails(self):
        self.links.append(f"https://github.com/TogetherWeOwn/two-bot/blob/{self.baseline}/src/missing.ts")
        self.fails("cat-file")

    def test_invalid_line_anchor_fails(self):
        self.links[1] = self.links[1].replace("#L1-L2", "#L1-L999")
        self.fails("outside file")

    def test_short_or_mutable_link_fails(self):
        for revision in (self.baseline[:7], "main"):
            with self.subTest(revision=revision):
                self.links.append(f"https://github.com/TogetherWeOwn/two-bot/tree/{revision}")
                self.fails("full SHAs")
                self.links.pop()

    def test_missing_local_document_fails(self):
        self.links.append("missing.md")
        self.fails("missing relative link")

    def test_registry_metadata_drift_fails(self):
        self.fixture.write_text(json.dumps({"revision": self.old}))
        self.fails("golden revision differs")

    def test_rewrite_tree_drift_fails(self):
        self.config["registrySnapshot"] = self.baseline
        self.fails("differs from historical src tree")

    def test_missing_baseline_source_link_fails(self):
        self.links.pop(0)
        self.fails("must link the manifest baseline")


if __name__ == "__main__":
    unittest.main()
