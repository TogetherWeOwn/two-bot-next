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

    def test_transactional_event_store_index_waiver_is_exact(self):
        path = "crates/cutover/migrations/0444_event_store_expiry_index.sql"
        statement = (
            "CREATE INDEX IF NOT EXISTS idx_event_store_expiry_event_time_id "
            "ON event_store (event_time, id)"
        )
        self.assertTrue(
            squawk.is_waived_finding(path, "creating-index", statement)
        )
        self.assertFalse(
            squawk.is_waived_finding(
                path,
                "creating-index",
                "CREATE INDEX unrelated_idx ON event_store (event_time)",
            )
        )
        self.assertFalse(
            squawk.is_waived_finding(
                "crates/cutover/migrations/0445_other.sql",
                "creating-index",
                statement,
            )
        )
        self.assertFalse(
            squawk.is_waived_finding(path, "dropping-object", statement)
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

    def test_bare_drop_column_without_keyword_is_flagged(self):
        # Postgres lets ALTER TABLE omit COLUMN: DROP [COLUMN] [IF EXISTS].
        for sql in (
            "ALTER TABLE t DROP c;\n",
            "ALTER TABLE t DROP IF EXISTS c;\n",
            'ALTER TABLE t DROP "c" CASCADE;\n',
            "ALTER TABLE t ADD COLUMN a int, DROP b;\n",
            "ALTER TABLE IF EXISTS t DROP c;\n",
            "ALTER TABLE ONLY t DROP c;\n",
            "ALTER TABLE ONLY public.t DROP c;\n",
            "ALTER TABLE t * DROP c;\n",
        ):
            self.assertEqual(self.rules(sql), ["dropping-object"], sql)

    def test_drop_constraint_and_alter_column_drops_stay_clean(self):
        for sql in (
            "ALTER TABLE t DROP CONSTRAINT k;\n",
            "ALTER TABLE t ALTER COLUMN c DROP NOT NULL;\n",
            "ALTER TABLE t ALTER COLUMN c DROP DEFAULT;\n",
            "ALTER TABLE t ALTER c DROP IDENTITY;\n",
            "DROP INDEX IF EXISTS idx_tickets_one_open;\n",
            "DROP TRIGGER IF EXISTS trg ON guild_settings;\n",
        ):
            self.assertEqual(self.rules(sql), [], sql)

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

    def test_column_named_type_actions_are_not_type_changes(self):
        # The COLUMN keyword must not be consumed as the column name: a
        # column literally named `type` with SET/DROP actions is lock-free.
        for sql in (
            "ALTER TABLE t ALTER COLUMN type SET NOT NULL;\n",
            "ALTER TABLE t ALTER COLUMN type SET DEFAULT 'a';\n",
            "ALTER TABLE t ALTER COLUMN type DROP DEFAULT;\n",
            "ALTER TABLE type ADD COLUMN x int;\n",
        ):
            self.assertEqual(self.rules(sql), [], sql)
        # True positives still flag, with and without the COLUMN keyword.
        self.assertEqual(
            self.rules("ALTER TABLE t ALTER COLUMN c TYPE text;\n"),
            ["changing-column-type"],
        )
        self.assertEqual(
            self.rules("ALTER TABLE t ALTER c TYPE text;\n"),
            ["changing-column-type"],
        )

    def test_not_valid_on_sibling_action_does_not_silence_check(self):
        self.assertEqual(
            self.rules(
                "ALTER TABLE t ADD CONSTRAINT a CHECK (x > 0), "
                "ADD CONSTRAINT b CHECK (y > 0) NOT VALID;\n"
            ),
            ["adding-check-constraint"],
        )
        self.assertEqual(
            self.rules(
                "ALTER TABLE t ADD CONSTRAINT a CHECK (x > 0), "
                "ADD CONSTRAINT b FOREIGN KEY (y) REFERENCES u (id) NOT VALID;\n"
            ),
            ["adding-check-constraint"],
        )

    def test_quoted_constraint_name_still_flags_check(self):
        self.assertEqual(
            self.rules(
                'ALTER TABLE t ADD CONSTRAINT "c" CHECK (x > 0);\n'
            ),
            ["adding-check-constraint"],
        )

    def test_index_on_same_file_table_is_clean(self):
        # Empty tables need no CONCURRENTLY: the index follows the CREATE
        # TABLE in the same migration file.
        self.assertEqual(
            self.rules(
                "CREATE TABLE guilds (id bigint PRIMARY KEY, name text);\n"
                "CREATE INDEX guilds_name_idx ON guilds (name);\n"
            ),
            [],
        )
        # Conditional creation proves nothing about emptiness: when the
        # relation already exists the statement is a no-op and the table
        # may hold production data, so the following index must flag.
        self.assertEqual(
            self.rules(
                "CREATE TABLE IF NOT EXISTS guilds (id bigint PRIMARY KEY);\n"
                "CREATE UNIQUE INDEX guilds_uidx ON guilds (id);\n"
            ),
            ["creating-index"],
        )

    def test_index_on_other_table_still_flags(self):
        self.assertEqual(
            self.rules(
                "CREATE TABLE guilds (id bigint PRIMARY KEY);\n"
                "CREATE INDEX members_name_idx ON members (name);\n"
            ),
            ["creating-index"],
        )

    def test_index_before_create_table_still_flags(self):
        # Only tables created EARLIER in the same file are empty and safe.
        self.assertEqual(
            self.rules(
                "CREATE INDEX guilds_name_idx ON guilds (name);\n"
                "CREATE TABLE guilds (id bigint PRIMARY KEY, name text);\n"
            ),
            ["creating-index"],
        )

    def test_e_string_escaped_quote_does_not_hide_index(self):
        # E'it\\'s' must not end the literal early and swallow the index.
        self.assertEqual(
            self.rules(
                "SELECT E'it\\'s';\n"
                "CREATE INDEX bad_idx ON t (a);\n"
            ),
            ["creating-index"],
        )
        self.assertEqual(
            self.rules(
                "SELECT e'xy\\'z';\n"
                "CREATE INDEX bad_idx ON t (a);\n"
            ),
            ["creating-index"],
        )

    def test_ordinary_string_backslash_keeps_behavior(self):
        # Ordinary strings have no backslash escapes: 'a\\' ends at the
        # quote, so the following index stays visible.
        self.assertEqual(
            self.rules(
                "SELECT 'a\\';\n"
                "CREATE INDEX bad_idx ON t (a);\n"
            ),
            ["creating-index"],
        )
        # '' doubling still escapes in both string kinds.
        self.assertEqual(
            self.rules(
                "SELECT 'it''s';\n"
                "CREATE INDEX bad_idx ON t (a);\n"
            ),
            ["creating-index"],
        )
        self.assertEqual(
            self.rules("SELECT E'it''s';\n"),
            [],
        )

    def test_quoted_create_then_other_quoted_table_still_flags(self):
        self.assertEqual(
            self.rules(
                'CREATE TABLE "Alpha" (a int);\n'
                'CREATE INDEX ix ON "Beta" (a);\n'
            ),
            ["creating-index"],
        )

    def test_quoted_create_then_same_quoted_table_is_clean(self):
        self.assertEqual(
            self.rules(
                'CREATE TABLE "Alpha" (a int);\n'
                'CREATE INDEX ix ON "Alpha" (a);\n'
            ),
            [],
        )

    def test_quoted_names_differing_only_by_case_still_flag(self):
        # Quoted identifiers are case-sensitive.
        self.assertEqual(
            self.rules(
                'CREATE TABLE "Alpha" (a int);\n'
                'CREATE INDEX ix ON "alpha" (a);\n'
            ),
            ["creating-index"],
        )

    def test_identifier_ending_in_e_does_not_start_escape_string(self):
        # The E in ELSE is part of an identifier, not an E'...' prefix:
        # the ordinary string ends at \' so the index stays visible.
        self.assertEqual(
            self.rules(
                "SELECT CASE WHEN x THEN 1 ELSE'a\\' END;\n"
                "CREATE INDEX bad ON t (a);\n"
            ),
            ["creating-index"],
        )

    def test_unquoted_names_fold_case(self):
        self.assertEqual(
            self.rules(
                "CREATE TABLE Guilds (id bigint PRIMARY KEY);\n"
                "CREATE INDEX x ON guilds (id);\n"
            ),
            [],
        )

    def test_schema_qualified_create_matches_bare_index(self):
        self.assertEqual(
            self.rules(
                "CREATE TABLE public.t (id bigint PRIMARY KEY);\n"
                "CREATE INDEX x ON t (id);\n"
            ),
            [],
        )

    def test_quoted_lowercase_matches_unquoted(self):
        # Postgres folds unquoted names to lowercase, so "guilds" == guilds.
        self.assertEqual(
            self.rules(
                'CREATE TABLE "guilds" (id bigint PRIMARY KEY);\n'
                "CREATE INDEX x ON guilds (id);\n"
            ),
            [],
        )
        self.assertEqual(
            self.rules(
                "CREATE TABLE guilds (id bigint PRIMARY KEY);\n"
                'CREATE INDEX x ON "guilds" (id);\n'
            ),
            [],
        )

    def test_quoted_mixed_case_does_not_match_unquoted(self):
        # Quoted identifiers stay case-sensitive: "Guilds" != guilds.
        self.assertEqual(
            self.rules(
                'CREATE TABLE "Guilds" (id bigint PRIMARY KEY);\n'
                "CREATE INDEX x ON guilds (id);\n"
            ),
            ["creating-index"],
        )
        self.assertEqual(
            self.rules(
                "CREATE TABLE guilds (id bigint PRIMARY KEY);\n"
                'CREATE INDEX x ON "Guilds" (id);\n'
            ),
            ["creating-index"],
        )

    def test_multiline_quoted_identifier_preserves_line_numbers(self):
        findings = squawk.lint_text(
            'SELECT "a\nb";\nCREATE INDEX bad ON t (a);\n'
        )
        self.assertEqual(findings, [(3, "creating-index")])

    def test_quoted_keyword_names_do_not_hide_rules(self):
        # A quoted name that is itself a keyword must never read as that
        # keyword: "default" is a column name, not a DEFAULT clause.
        self.assertEqual(
            self.rules('ALTER TABLE t ADD COLUMN "default" int NOT NULL;\n'),
            ["adding-required-field"],
        )
        self.assertEqual(
            self.rules('ALTER TABLE t ADD "unique" int NOT NULL;\n'),
            ["adding-required-field"],
        )
        self.assertEqual(
            self.rules('ALTER TABLE t ADD "check" int NOT NULL;\n'),
            ["adding-required-field"],
        )
        self.assertEqual(
            self.rules('CREATE INDEX i ON "concurrently" (a);\n'),
            ["creating-index"],
        )

    def test_required_field_ignores_parens_and_referential_actions(self):
        # NOT NULL inside parenthesised expressions is not a column-level
        # NOT NULL; ON DELETE/UPDATE SET DEFAULT is not a column DEFAULT.
        self.assertEqual(
            self.rules("ALTER TABLE t ADD COLUMN a int CHECK (a IS NOT NULL);\n"),
            [],
        )
        self.assertEqual(
            self.rules(
                "ALTER TABLE t ADD COLUMN a int NOT NULL "
                "REFERENCES u(id) ON DELETE SET DEFAULT;\n"
            ),
            ["adding-required-field"],
        )
        self.assertEqual(
            self.rules(
                "ALTER TABLE t ADD COLUMN a int NOT NULL "
                "REFERENCES u(id) ON UPDATE SET DEFAULT;\n"
            ),
            ["adding-required-field"],
        )
        self.assertEqual(
            self.rules(
                "ALTER TABLE t ADD COLUMN a int NOT NULL DEFAULT (1+1);\n"
            ),
            [],
        )
        self.assertEqual(
            self.rules("ALTER TABLE t ADD COLUMN a int DEFAULT 0 NOT NULL;\n"),
            [],
        )
        self.assertEqual(
            self.rules(
                "ALTER TABLE t ADD COLUMN a numeric(10,2) NOT NULL;\n"
            ),
            ["adding-required-field"],
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

    def test_diff_mode_star_star_glob_matches_full_mode(self):
        # `**/migrations/*.sql` must lint a root-level migrations/ file in
        # both full mode (Path.glob) and diff mode (changed paths).
        self.git("init", "--initial-branch=main")
        self.write("README.md", "base\n")
        self.commit_all("baseline")
        base = self.git("rev-parse", "HEAD")
        self.write("migrations/0001_x.sql", "CREATE INDEX bad_idx ON t (a);\n")
        self.commit_all("risky")
        globs = ["**/migrations/*.sql"]
        full = squawk.check(self.root, globs, [], None)
        self.assertEqual(full, [("migrations/0001_x.sql", 1, "creating-index")])
        diff = squawk.check(self.root, globs, [], base)
        self.assertEqual(diff, full)

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

    def test_strict_drop_only_passes_with_warning(self):
        target = self.root / FIRST
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text("DROP TABLE legacy_table;\n")
        code, out = self.run_checker("--strict")
        self.assertEqual(code, 0)
        self.assertIn("::warning file=", out)
        self.assertIn("[squawk:dropping-object]", out)
        self.assertIn("(advisory)", out)

    def test_strict_drop_and_blocking_finding_fails(self):
        target = self.root / FIRST
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(
            "DROP TABLE legacy_table;\n"
            "CREATE INDEX bad_idx ON guilds (name);\n"
        )
        code, out = self.run_checker("--strict")
        self.assertEqual(code, 1)
        self.assertIn("::warning file=", out)
        self.assertIn("[squawk:dropping-object]", out)
        self.assertIn("::error file=", out)
        self.assertIn("[squawk:creating-index]", out)

    def test_clean_tree_passes_in_both_modes(self):
        target = self.root / FIRST
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text("CREATE TABLE first (id bigint PRIMARY KEY);\n")
        for args in ([], ["--strict"]):
            code, out = self.run_checker(*args)
            self.assertEqual(code, 0)
            self.assertIn("no risky migration statements", out)

    def test_exact_transactional_index_waivers_stay_strict_for_other_indexes(self):
        approved = {
            "crates/cutover/migrations/0444_event_store_expiry_index.sql": (
                "CREATE INDEX IF NOT EXISTS idx_event_store_expiry_event_time_id "
                "ON event_store (event_time, id);\n"
            ),
            "crates/cutover/migrations/0445_event_store_expiry_class_index.sql": (
                "CREATE INDEX IF NOT EXISTS idx_event_store_expiry_class_time_id "
                "ON event_store (event_type, event_time, id);\n"
            ),
        }
        for relative, sql in approved.items():
            path = self.root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(sql)

        code, out = self.run_checker("--strict")
        self.assertEqual(code, 0)
        self.assertIn("no risky migration statements", out)

        with (self.root / next(iter(approved))).open("a") as migration:
            migration.write("CREATE INDEX unreviewed_idx ON event_store (id);\n")
        code, out = self.run_checker("--strict")
        self.assertEqual(code, 1)
        self.assertIn("[squawk:creating-index]", out)
        self.assertIn("1 finding(s) in 1 file(s)", out)

    def test_broken_tooling_fails_even_when_advisory(self):
        code, _ = self.run_checker("--base-ref", "no-such-ref")
        self.assertEqual(code, 1)


if __name__ == "__main__":
    unittest.main()
