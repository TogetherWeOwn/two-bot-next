"""Offline fixtures for the advisory squawk migration lint.

Temporary local files and Git repositories only; no database, Rust
compilation or network access.
"""

import contextlib
import importlib.util
import io
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("check-squawk-migrations.py")
spec = importlib.util.spec_from_file_location("check_squawk_migrations", SCRIPT)
squawk = importlib.util.module_from_spec(spec)
spec.loader.exec_module(squawk)

FIRST = "crates/cutover/migrations/0001_first.sql"


class RuleTests(unittest.TestCase):
    def rules(self, sql):
        return [rule for _, rule in squawk.lint_text(sql)]

    def test_plain_ddl_is_clean(self):
        self.assertEqual(
            self.rules(
                "CREATE TABLE guilds (id bigint PRIMARY KEY);\n"
                "ALTER TABLE guilds ADD COLUMN name text;\n"
                "CREATE INDEX CONCURRENTLY guilds_name_idx ON guilds (name);\n"
            ),
            [],
        )

    def test_required_field_without_default(self):
        self.assertEqual(
            self.rules("ALTER TABLE guilds ADD COLUMN flags bigint NOT NULL;\n"),
            ["adding-required-field"],
        )

    def test_required_field_with_default_is_clean(self):
        self.assertEqual(
            self.rules(
                "ALTER TABLE guilds ADD COLUMN flags bigint NOT NULL DEFAULT 0;\n"
            ),
            [],
        )

    def test_index_without_concurrently(self):
        self.assertEqual(
            self.rules("CREATE INDEX guilds_name_idx ON guilds (name);\n"),
            ["creating-index"],
        )
        self.assertEqual(
            self.rules(
                "CREATE UNIQUE INDEX guilds_name_uidx ON guilds (name);\n"
            ),
            ["creating-index"],
        )

    def test_concurrent_index_is_clean(self):
        self.assertEqual(
            self.rules(
                "CREATE INDEX CONCURRENTLY guilds_name_idx ON guilds (name);\n"
            ),
            [],
        )
        self.assertEqual(
            self.rules(
                "CREATE UNIQUE INDEX CONCURRENTLY guilds_name_uidx ON guilds (name);\n"
            ),
            [],
        )

    def test_check_without_not_valid(self):
        self.assertEqual(
            self.rules(
                "ALTER TABLE guilds ADD CONSTRAINT positive_flags CHECK (flags >= 0);\n"
            ),
            ["adding-check-constraint"],
        )

    def test_check_not_valid_is_clean(self):
        self.assertEqual(
            self.rules(
                "ALTER TABLE guilds ADD CONSTRAINT positive_flags "
                "CHECK (flags >= 0) NOT VALID;\n"
            ),
            [],
        )

    def test_column_type_change(self):
        self.assertEqual(
            self.rules("ALTER TABLE guilds ALTER COLUMN flags TYPE text;\n"),
            ["changing-column-type"],
        )

    def test_drops_are_flagged(self):
        self.assertEqual(
            self.rules("ALTER TABLE guilds DROP COLUMN flags;\n"),
            ["dropping-object"],
        )
        self.assertEqual(
            self.rules("DROP TABLE guilds;\n"),
            ["dropping-object"],
        )

    def test_multiline_and_lowercase_statements(self):
        findings = squawk.lint_text(
            "alter table guilds\n"
            "  add column flags bigint\n"
            "  not null;\n"
            "create table ok (id bigint primary key);\n"
        )
        self.assertEqual(findings, [(1, "adding-required-field")])

    def test_comments_and_strings_never_flag(self):
        self.assertEqual(
            self.rules(
                "-- ALTER TABLE guilds ADD COLUMN flags bigint NOT NULL;\n"
                "/* CREATE INDEX bad_idx ON guilds (name); */\n"
                "CREATE TABLE probes (note text DEFAULT 'NOT NULL; CREATE INDEX');\n"
            ),
            [],
        )

    def test_dollar_quoted_bodies_never_flag(self):
        self.assertEqual(
            self.rules(
                "CREATE OR REPLACE FUNCTION bump() RETURNS void AS $$\n"
                "BEGIN\n"
                "  EXECUTE 'ALTER TABLE guilds ADD COLUMN flags bigint NOT NULL';\n"
                "END;\n"
                "$$ LANGUAGE plpgsql;\n"
            ),
            [],
        )

    def test_semicolons_inside_strings_do_not_split(self):
        self.assertEqual(
            self.rules(
                "CREATE TABLE probes (a text, b text);\n"
                "ALTER TABLE guilds ADD COLUMN flags bigint NOT NULL;\n"
            ),
            ["adding-required-field"],
        )

    def test_add_constraint_check_is_not_a_required_field(self):
        # 0423: the only backtest hit for this rule was ADD CONSTRAINT with
        # IS NOT NULL inside the CHECK expression, not a new NOT NULL column.
        self.assertEqual(
            self.rules(
                "ALTER TABLE internal_actions ADD CONSTRAINT "
                "internal_idempotency_check CHECK (response_code IS NOT NULL);\n"
            ),
            ["adding-check-constraint"],
        )

    def test_default_elsewhere_does_not_silence_required_field(self):
        self.assertEqual(
            self.rules(
                "ALTER TABLE guilds ADD COLUMN b bigint NOT NULL, "
                "ALTER COLUMN c SET DEFAULT 0;\n"
            ),
            ["adding-required-field"],
        )

    def test_column_named_type_is_not_a_type_change(self):
        self.assertEqual(
            self.rules("ALTER TABLE t ADD COLUMN type text;\n"),
            [],
        )
        self.assertEqual(
            self.rules(
                "ALTER TABLE t ADD CONSTRAINT c CHECK (type IN ('a')) NOT VALID;\n"
            ),
            [],
        )

    def test_set_data_type_change_is_flagged(self):
        self.assertEqual(
            self.rules(
                "ALTER TABLE guilds ALTER COLUMN flags SET DATA TYPE text;\n"
            ),
            ["changing-column-type"],
        )

    def test_quoted_constraint_name_still_flags_check(self):
        self.assertEqual(
            self.rules(
                'ALTER TABLE t ADD CONSTRAINT "c" CHECK (x > 0);\n'
            ),
            ["adding-check-constraint"],
        )

    def test_several_rules_in_one_file_report_each_line(self):
        self.assertEqual(
            squawk.lint_text(
                "CREATE INDEX bad_idx ON guilds (name);\n"
                "ALTER TABLE guilds DROP COLUMN flags;\n"
            ),
            [(1, "creating-index"), (2, "dropping-object")],
        )


class SelectionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(
            dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
        )
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def write(self, path, contents):
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(contents)

    def git(self, *args):
        return subprocess.check_output(
            ["git", "-C", str(self.root), *args], stderr=subprocess.PIPE
        ).decode().strip()

    def commit_all(self, message):
        self.git("add", "-A")
        self.git("-c", "user.name=fixture", "-c", "user.email=f@x.invalid", "commit", "-m", message)

    def init_repo(self):
        self.git("init", "--initial-branch=main")
        self.write(FIRST, "CREATE TABLE first (id bigint PRIMARY KEY);\n")
        self.commit_all("baseline")

    def test_changed_file_selection_only_lints_the_diff(self):
        self.init_repo()
        base = self.git("rev-parse", "HEAD")
        self.write(
            "crates/store/migrations/0002_second.sql",
            "ALTER TABLE first ADD COLUMN flags bigint NOT NULL;\n",
        )
        self.commit_all("risky")
        findings = squawk.check(self.root, [squawk.DEFAULT_GLOB], [], base)
        self.assertEqual(
            findings,
            [("crates/store/migrations/0002_second.sql", 1, "adding-required-field")],
        )
        clean = squawk.check(self.root, [squawk.DEFAULT_GLOB], [], "HEAD")
        self.assertEqual(clean, [])

    def test_exclude_list_skips_matching_files(self):
        self.init_repo()
        base = self.git("rev-parse", "HEAD")
        self.write(
            "crates/store/migrations/0002_second.sql",
            "ALTER TABLE first ADD COLUMN flags bigint NOT NULL;\n",
        )
        self.commit_all("risky")
        findings = squawk.check(
            self.root,
            [squawk.DEFAULT_GLOB],
            ["crates/store/migrations/*"],
            base,
        )
        self.assertEqual(findings, [])

    def test_unresolvable_baseline_fails_closed(self):
        self.init_repo()
        with self.assertRaisesRegex(squawk.SquawkError, "unresolvable Git baseline"):
            squawk.check(self.root, [squawk.DEFAULT_GLOB], [], "no-such-ref")

    def test_all_mode_lints_every_migration(self):
        self.write(FIRST, "DROP TABLE first;\n")
        findings = squawk.check(self.root, [squawk.DEFAULT_GLOB], [], None)
        self.assertEqual(findings, [(FIRST, 1, "dropping-object")])

    def test_diff_mode_lints_shallow_custom_glob(self):
        self.git("init", "--initial-branch=main")
        self.write("README.md", "base\n")
        self.commit_all("baseline")
        base = self.git("rev-parse", "HEAD")
        self.write(
            "migrations/0001_x.sql", "CREATE INDEX bad_idx ON t (a);\n"
        )
        self.commit_all("risky")
        findings = squawk.check(
            self.root, ["migrations/*.sql"], [], base
        )
        self.assertEqual(
            findings, [("migrations/0001_x.sql", 1, "creating-index")]
        )

    def test_custom_glob_replaces_default(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = squawk.main(
                [
                    "--root",
                    str(self.root),
                    "--migration-glob",
                    "migrations/*.sql",
                ]
            )
        self.assertEqual(code, 0)
        self.write(
            "crates/a/migrations/0001_y.sql",
            "CREATE INDEX bad_idx ON t (a);\n",
        )
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = squawk.main(
                [
                    "--root",
                    str(self.root),
                    "--migration-glob",
                    "migrations/*.sql",
                ]
            )
        self.assertEqual(code, 0)
        self.assertIn("no risky migration statements", out.getvalue())


class ExitCodeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(
            dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
        )
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def run_checker(self, *args):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = squawk.main(["--root", str(self.root), *args])
        return code, out.getvalue()

    def test_advisory_reports_but_passes(self):
        target = self.root / FIRST
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text("CREATE INDEX bad_idx ON guilds (name);\n")
        code, out = self.run_checker()
        self.assertEqual(code, 0)
        self.assertIn("::warning file=", out)
        self.assertIn("[squawk:creating-index]", out)
        self.assertIn("(advisory)", out)

    def test_strict_fails_on_findings(self):
        target = self.root / FIRST
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text("CREATE INDEX bad_idx ON guilds (name);\n")
        code, out = self.run_checker("--strict")
        self.assertEqual(code, 1)
        self.assertIn("::error file=", out)

    def test_clean_tree_passes_in_both_modes(self):
        target = self.root / FIRST
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text("CREATE TABLE first (id bigint PRIMARY KEY);\n")
        for args in ([], ["--strict"]):
            code, out = self.run_checker(*args)
            self.assertEqual(code, 0)
            self.assertIn("no risky migration statements", out)

    def test_broken_tooling_fails_even_when_advisory(self):
        code, _ = self.run_checker("--base-ref", "no-such-ref")
        self.assertEqual(code, 1)


if __name__ == "__main__":
    unittest.main()
