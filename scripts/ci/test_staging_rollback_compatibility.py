#!/usr/bin/env python3
"""Exercise both chain comparisons with a disposable Git repository."""

from contextlib import redirect_stderr, redirect_stdout
from io import StringIO
from pathlib import Path
import subprocess
from tempfile import TemporaryDirectory
import unittest

from staging_rollback_compatibility import MIGRATION_PATHS, Refused, main, verify


class StagingRollbackCompatibilityTests(unittest.TestCase):
    def setUp(self):
        scratch = TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.repo = Path(scratch.name)
        self.git("init", "--object-format=sha1", "--template=")
        self.git("config", "user.name", "Staging Fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        self.git("config", "commit.gpgsign", "false")
        cutover, store = (self.repo / path for path in MIGRATION_PATHS)
        cutover.mkdir(parents=True)
        store.mkdir(parents=True)
        (cutover / "0480_prior.sql").write_text("SELECT 1;\n")
        self.store = store / "0478_store.sql"
        self.store.write_text("SELECT 2;\n")
        self.prior = self.commit()
        self.cutover = cutover / "0481_current.sql"
        self.cutover.write_text("SELECT 3;\n")
        self.plan = self.commit()
        (self.repo / "build.txt").write_text("A different build\n")
        self.forward = self.commit()

    def git(self, *args):
        return subprocess.run(
            ["git", "-C", self.repo, *args], check=True, capture_output=True,
            text=True, timeout=10,
        ).stdout.strip()

    def commit(self):
        self.git("add", "--all")
        self.git("commit", "-m", "fixture")
        return self.git("rev-parse", "HEAD")

    def test_cli_accepts_distinct_commits_with_both_migration_trees_unchanged(self):
        out = StringIO()
        with redirect_stdout(out):
            code = main(["--repo", str(self.repo), "--plan-source-sha", self.plan,
                         "--forward-sha", self.forward, "--recovery-sha", self.plan])
        self.assertEqual(code, 0)
        self.assertEqual(out.getvalue(), "staging_migration_sources_match\n")

    def test_refuses_predecessor_missing_the_installed_cutover_migration(self):
        out, err = StringIO(), StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            code = main(["--repo", str(self.repo), "--plan-source-sha", self.plan,
                         "--forward-sha", self.forward, "--recovery-sha", self.prior])
        self.assertEqual(code, 1)
        self.assertEqual(out.getvalue(), "")
        self.assertEqual(err.getvalue(), "recovery_migration_source_mismatch\n")

    def test_refuses_changed_store_bytes_even_when_the_terminal_version_matches(self):
        self.store.write_text("SELECT 4;\n")
        changed = self.commit()
        with self.assertRaisesRegex(Refused, "^forward_migration_source_mismatch$"):
            verify(self.repo, self.plan, changed, self.forward)
        with self.assertRaisesRegex(Refused, "^recovery_migration_source_mismatch$"):
            verify(self.repo, self.plan, self.forward, changed)

    def test_refuses_absent_tree_and_unresolved_or_non_commit_sources(self):
        self.store.unlink()
        missing_store = self.commit()
        blob = self.git("rev-parse", f"{self.plan}:{self.cutover.relative_to(self.repo)}")
        for source, reason in ((missing_store, "migration_tree_unavailable"),
                               ("0" * 40, "migration_source_unavailable"),
                               (blob, "invalid_source_commit"), ("HEAD", "invalid_source_sha")):
            with self.subTest(reason=reason), self.assertRaisesRegex(Refused, f"^{reason}$"):
                verify(self.repo, self.plan, self.forward, source)

    def test_ignores_local_git_replacements_when_reading_immutable_commits(self):
        self.git("replace", self.prior, self.plan)
        with self.assertRaisesRegex(Refused, "^recovery_migration_source_mismatch$"):
            verify(self.repo, self.plan, self.forward, self.prior)


if __name__ == "__main__":
    unittest.main()
