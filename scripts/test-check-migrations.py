"""Offline migration fixtures, including the actual CLI against a temporary Git repo."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("check-migrations.py")
spec = importlib.util.spec_from_file_location("check_migrations", SCRIPT)
checker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)
FIRST = "crates/cutover/migrations/0001_first.sql"
SECOND = "crates/store/migrations/0002_second.sql"
SQL = b"CREATE TABLE first (id bigint PRIMARY KEY);\n"


class MigrationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR"))
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.write(FIRST, SQL)
        self.entries = {FIRST: {"sha256": checker.digest(SQL), "justification": "Initial fixture baseline."}}
        self.lock()

    def write(self, path, contents):
        file = self.root / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_bytes(contents)

    def lock(self):
        self.write("migrations.lock", json.dumps({"version": 1, "migrations": self.entries}).encode())

    def validate(self, previous=None, previous_lock=None):
        checker.validate(checker.read_migrations(self.root), checker.read_lock((self.root / "migrations.lock").read_bytes()), previous, previous_lock)

    def fails(self, message, previous=None, previous_lock=None):
        with self.assertRaisesRegex(checker.MigrationError, message):
            self.validate(previous, previous_lock)

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.root), *args], stderr=subprocess.PIPE).decode().strip()

    def commit_baseline(self, with_lock=True):
        self.git("init", "--initial-branch=main")
        self.git("config", "user.name", "Migration fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        self.git("config", "commit.gpgsign", "false")
        self.git("add", "crates")
        if with_lock:
            self.git("add", "migrations.lock")
        self.git("commit", "-m", "Fixture baseline")
        return self.git("rev-parse", "HEAD")

    def cli(self, *args, success=True):
        result = subprocess.run([sys.executable, str(SCRIPT), "--root", str(self.root), *args], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0 if success else 1, result.stdout + result.stderr)
        return result.stdout + result.stderr

    def test_current_migration(self):
        self.validate()
        self.assertIn("1 locked bot migrations", self.cli())

    def test_duplicate_number_across_crates(self):
        self.write("crates/store/migrations/0001_other.sql", SQL)
        self.fails("duplicate migration number 0001")

    def test_duplicate_number_inside_crate(self):
        self.write("crates/cutover/migrations/0001_other.sql", SQL)
        self.fails("duplicate migration number 0001")

    def test_out_of_range(self):
        for number in ("0000", "1000", "1999", "9999"):
            with self.subTest(number=number):
                file = f"crates/store/migrations/{number}_web.sql"
                self.write(file, SQL)
                self.fails("bot range 0001-0999")
                (self.root / file).unlink()

    def test_malformed_filenames(self):
        for name in ("1_first.sql", "00001_first.sql", "0002.sql", "0002_.sql", "0002_upper.SQL", "0002_bad-name.sql", "٠٠٠٢_other.sql", "notes.txt", "nested/0002_other.sql"):
            with self.subTest(name=name):
                file = f"crates/store/migrations/{name}"
                self.write(file, SQL)
                self.fails("NNNN_name.sql")
                (self.root / file).unlink()

    def test_boundary_numbers(self):
        path = "crates/store/migrations/0999_last.sql"
        self.write(path, SQL)
        self.entries[path] = {"sha256": checker.digest(SQL), "justification": "Add boundary fixture."}
        self.lock()
        self.validate()

    def test_edited_migration_fails(self):
        self.write(FIRST, SQL + b"-- changed bytes\n")
        self.fails("checksum changed")

    def test_line_endings_are_bytes_not_normalized_text(self):
        self.write(FIRST, SQL.replace(b"\n", b"\r\n"))
        self.fails("checksum changed")

    def test_new_migration_needs_lock_entry(self):
        self.write(SECOND, b"CREATE TABLE second (id bigint);\n")
        self.fails("unlocked=.*0002_second")
        self.entries[SECOND] = {"sha256": checker.digest((self.root / SECOND).read_bytes()), "justification": "Add second table."}
        self.lock()
        self.validate({FIRST: checker.digest(SQL)}, {FIRST: self.entries[FIRST]})

    def test_edit_and_checksum_rewrite_needs_new_justification(self):
        ref = self.commit_baseline()
        changed = SQL + b"-- reviewed exception\n"
        self.write(FIRST, changed)
        self.entries[FIRST]["sha256"] = checker.digest(changed)
        self.lock()
        self.assertIn("fresh justification", self.cli("--base-ref", ref, success=False))
        self.entries[FIRST]["justification"] = "   Initial fixture baseline.   "
        self.lock()
        self.cli("--base-ref", ref, success=False)
        self.entries[FIRST]["justification"] = "Correct fixture DDL before rollout; no applied database changed."
        self.lock()
        self.cli("--base-ref", ref)

    def test_internal_whitespace_rewrite_is_not_a_fresh_justification(self):
        ref = self.commit_baseline()
        changed = SQL + b"-- reviewed exception\n"
        self.write(FIRST, changed)
        self.entries[FIRST]["sha256"] = checker.digest(changed)
        for reason in ("Initial  fixture baseline.", "Initial\tfixture baseline.", "Initial fixture baseline."):
            with self.subTest(reason=reason):
                self.entries[FIRST]["justification"] = reason
                self.lock()
                self.assertIn("fresh justification", self.cli("--base-ref", ref, success=False))

    def test_removing_file_and_lock_entry_fails_against_baseline(self):
        ref = self.commit_baseline()
        (self.root / FIRST).unlink()
        self.entries.clear()
        self.lock()
        self.assertIn("cannot be removed or renamed", self.cli("--base-ref", ref, success=False))

    def test_renaming_file_and_lock_entry_fails_against_baseline(self):
        ref = self.commit_baseline()
        (self.root / FIRST).rename((self.root / FIRST).with_name("0001_renamed.sql"))
        entry = self.entries.pop(FIRST)
        self.entries["crates/cutover/migrations/0001_renamed.sql"] = entry
        self.lock()
        self.cli("--base-ref", ref, success=False)

    def test_stale_lock_entry(self):
        (self.root / FIRST).unlink()
        self.fails("missing files=.*0001_first")

    def test_missing_lock_fails_cli(self):
        (self.root / "migrations.lock").unlink()
        self.cli(success=False)

    def test_missing_git_baseline_fails_closed(self):
        self.commit_baseline()
        self.assertIn("cannot read Git baseline", self.cli("--base-ref", "does-not-exist", success=False))

    def test_initial_lock_can_bootstrap_existing_migrations(self):
        ref = self.commit_baseline(with_lock=False)
        self.cli("--base-ref", ref)
        self.write(FIRST, SQL + b"-- unrecorded edit\n")
        self.cli("--base-ref", ref, success=False)

    def test_baseline_tracks_new_crate_directory(self):
        self.write(SECOND, SQL)
        self.entries[SECOND] = {"sha256": checker.digest(SQL), "justification": "Add second crate fixture."}
        self.lock()
        ref = self.commit_baseline()
        self.write(SECOND, SQL + b"-- changed\n")
        self.entries[SECOND]["sha256"] = checker.digest((self.root / SECOND).read_bytes())
        self.lock()
        self.assertIn("fresh justification", self.cli("--base-ref", ref, success=False))

    def test_invalid_lock_schema(self):
        entry = self.entries[FIRST]
        fixtures = [
            {"version": 2, "migrations": self.entries},
            {"version": True, "migrations": self.entries},
            {"version": 1, "migrations": []},
            {"version": 1, "migrations": {FIRST: {**entry, "sha256": "bad"}}},
            {"version": 1, "migrations": {FIRST: {**entry, "justification": "  "}}},
            {"version": 1, "migrations": {FIRST: {**entry, "justification": "reason\nsecond line"}}},
            {"version": 1, "migrations": {FIRST: {"sha256": entry["sha256"]}}},
            {"version": 1, "migrations": {"../outside.sql": entry}},
        ]
        for fixture in fixtures:
            with self.subTest(fixture=fixture), self.assertRaises(checker.MigrationError):
                checker.read_lock(json.dumps(fixture))

    def test_duplicate_lock_json_keys_fail(self):
        for contents in ('{"version":1,"version":1,"migrations":{}}', '{"version":1,"migrations":{"' + FIRST + '":{},"' + FIRST + '":{}}}'):
            with self.subTest(contents=contents), self.assertRaisesRegex(checker.MigrationError, "duplicate JSON key"):
                checker.read_lock(contents)

    def test_symlink_migration_fails(self):
        (self.root / FIRST).unlink()
        (self.root / FIRST).symlink_to(self.root / "migrations.lock")
        self.fails("cannot be symlinks")

    def test_symlinked_migration_ancestors_fail(self):
        for path in ("crates", "crates/cutover", "crates/cutover/migrations"):
            with self.subTest(path=path):
                directory = self.root / path
                moved = self.root / "real_directory"
                directory.rename(moved)
                directory.symlink_to(moved, target_is_directory=True)
                try:
                    self.assertIn("cannot be symlinks", self.cli(success=False))
                finally:
                    directory.unlink()
                    moved.rename(directory)

    def test_committed_symlinked_crates_cannot_hide_an_edit(self):
        directory = self.root / "crates"
        moved = self.root / "real_crates"
        directory.rename(moved)
        directory.symlink_to(moved, target_is_directory=True)
        ref = self.commit_baseline()
        changed = SQL + b"-- hidden from Git baseline\n"
        self.write(FIRST, changed)
        self.entries[FIRST]["sha256"] = checker.digest(changed)
        self.lock()
        self.assertIn("cannot be symlinks", self.cli("--base-ref", ref, success=False))

    def test_symlinked_baseline_fails_after_restoring_regular_paths(self):
        self.commit_baseline()
        for path in ("crates", "crates/cutover", "crates/cutover/migrations", FIRST):
            with self.subTest(path=path):
                original = self.root / path
                moved = self.root / "real_path"
                original.rename(moved)
                original.symlink_to(moved, target_is_directory=moved.is_dir())
                self.git("add", "crates")
                self.git("commit", "-m", "Fixture symlink")
                ref = self.git("rev-parse", "HEAD")
                original.unlink()
                moved.rename(original)
                self.assertIn("cannot be symlinks", self.cli("--base-ref", ref, success=False))
                self.git("add", "crates")
                self.git("commit", "-m", "Fixture regular paths")

    def test_workflow_compares_event_baseline_and_runs_fixtures(self):
        workflow = (SCRIPT.parent.parent / ".github/workflows/check.yml").read_text()
        step = workflow.split("- name: Check migration numbering and immutability", 1)[1].split("- name:", 1)[0]
        self.assertIn("github.event.pull_request.base.sha", step)
        self.assertIn("github.event.before", step)
        self.assertIn("'origin/main'", step)
        self.assertIn('python3 scripts/check-migrations.py --base-ref "$MIGRATION_BASE"', step)
        self.assertIn("python3 scripts/test-check-migrations.py", step)
        checkout = workflow.split("steps:", 1)[1].split("- name:", 1)[0]
        self.assertIn("fetch-depth: 0", checkout)


if __name__ == "__main__":
    unittest.main()
