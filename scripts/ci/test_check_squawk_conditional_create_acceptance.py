"""Acceptance fixtures: conditional CREATE TABLE earns no index exemption.

A table created unconditionally earlier in the same migration file is
empty, so a non-concurrent index on it needs no CONCURRENTLY. A
conditional ``CREATE TABLE IF NOT EXISTS`` proves no such thing: when
the relation already exists the statement is a no-op and the table may
hold production data, so a following non-concurrent index takes the
same lock the creating-index rule exists to flag. Only unconditional
creation may qualify a relation for the same-file empty-table
exemption. Conditional creation itself stays legal; the exemption is
what it cannot grant.

Offline ``lint_text`` fixtures plus strict/advisory CLI disposition
only; no database, migration execution, Rust compilation or network.
"""

import contextlib
import importlib.util
import io
import os
from pathlib import Path
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("check-squawk-migrations.py")
spec = importlib.util.spec_from_file_location("check_squawk_migrations", SCRIPT)
squawk = importlib.util.module_from_spec(spec)
spec.loader.exec_module(squawk)

FIRST = "crates/cutover/migrations/0001_first.sql"


class ConditionalCreateExemptionTests(unittest.TestCase):
    def rules(self, sql):
        return [rule for _, rule in squawk.lint_text(sql)]

    # --- Conditional creation must not exempt -------------------------

    def test_conditional_create_does_not_exempt_plain_index(self):
        self.assertEqual(
            squawk.lint_text(
                "CREATE TABLE IF NOT EXISTS guilds (id bigint PRIMARY KEY);\n"
                "CREATE INDEX guilds_idx ON guilds (id);\n"
            ),
            [(2, "creating-index")],
        )

    def test_conditional_create_does_not_exempt_unique_index(self):
        self.assertEqual(
            self.rules(
                "CREATE TABLE IF NOT EXISTS guilds (id bigint PRIMARY KEY);\n"
                "CREATE UNIQUE INDEX guilds_uidx ON guilds (id);\n"
            ),
            ["creating-index"],
        )

    def test_conditional_create_lowercase_does_not_exempt(self):
        self.assertEqual(
            self.rules(
                "create table if not exists guilds (id bigint primary key);\n"
                "create index guilds_idx on guilds (id);\n"
            ),
            ["creating-index"],
        )

    def test_conditional_create_multiline_does_not_exempt(self):
        self.assertEqual(
            squawk.lint_text(
                "CREATE TABLE IF NOT\n"
                "  EXISTS guilds (id bigint PRIMARY KEY);\n"
                "CREATE INDEX guilds_idx ON guilds (id);\n"
            ),
            [(3, "creating-index")],
        )

    def test_conditional_schema_qualified_create_does_not_exempt_bare_index(self):
        self.assertEqual(
            self.rules(
                "CREATE TABLE IF NOT EXISTS public.guilds "
                "(id bigint PRIMARY KEY);\n"
                "CREATE INDEX guilds_idx ON guilds (id);\n"
            ),
            ["creating-index"],
        )

    def test_conditional_bare_create_does_not_exempt_qualified_index(self):
        self.assertEqual(
            self.rules(
                "CREATE TABLE IF NOT EXISTS guilds (id bigint PRIMARY KEY);\n"
                "CREATE INDEX guilds_idx ON public.guilds (id);\n"
            ),
            ["creating-index"],
        )

    def test_conditional_quoted_create_does_not_exempt_same_relation(self):
        self.assertEqual(
            self.rules(
                'CREATE TABLE IF NOT EXISTS "guilds" '
                "(id bigint PRIMARY KEY);\n"
                "CREATE INDEX guilds_idx ON guilds (id);\n"
            ),
            ["creating-index"],
        )
        self.assertEqual(
            self.rules(
                'CREATE TABLE IF NOT EXISTS "Guilds" '
                "(id bigint PRIMARY KEY);\n"
                'CREATE UNIQUE INDEX guilds_uidx ON "Guilds" (id);\n'
            ),
            ["creating-index"],
        )

    def test_every_index_after_conditional_create_flags(self):
        self.assertEqual(
            squawk.lint_text(
                "CREATE TABLE IF NOT EXISTS guilds "
                "(id bigint PRIMARY KEY, name text);\n"
                "CREATE INDEX guilds_id_idx ON guilds (id);\n"
                "CREATE UNIQUE INDEX guilds_name_uidx ON guilds (name);\n"
            ),
            [(2, "creating-index"), (3, "creating-index")],
        )

    def test_conditional_and_unconditional_creates_are_independent(self):
        # The conditional create earns guilds no exemption, while the
        # unconditional create of members still exempts members.
        self.assertEqual(
            squawk.lint_text(
                "CREATE TABLE IF NOT EXISTS guilds (id bigint PRIMARY KEY);\n"
                "CREATE TABLE members (id bigint PRIMARY KEY);\n"
                "CREATE INDEX members_idx ON members (id);\n"
                "CREATE INDEX guilds_idx ON guilds (id);\n"
            ),
            [(4, "creating-index")],
        )

    # --- Controls: behaviour that must not change ---------------------

    def test_unconditional_create_still_exempts_plain_index(self):
        self.assertEqual(
            self.rules(
                "CREATE TABLE guilds (id bigint PRIMARY KEY);\n"
                "CREATE INDEX guilds_idx ON guilds (id);\n"
            ),
            [],
        )

    def test_unconditional_create_still_exempts_unique_index(self):
        self.assertEqual(
            self.rules(
                "CREATE TABLE guilds (id bigint PRIMARY KEY);\n"
                "CREATE UNIQUE INDEX guilds_uidx ON guilds (id);\n"
            ),
            [],
        )

    def test_unconditional_qualified_create_still_exempts_bare_index(self):
        self.assertEqual(
            self.rules(
                "CREATE TABLE public.guilds (id bigint PRIMARY KEY);\n"
                "CREATE INDEX guilds_idx ON guilds (id);\n"
            ),
            [],
        )

    def test_unconditional_quoted_create_still_exempts_same_relation(self):
        self.assertEqual(
            self.rules(
                'CREATE TABLE "Guilds" (id bigint PRIMARY KEY);\n'
                'CREATE INDEX guilds_idx ON "Guilds" (id);\n'
            ),
            [],
        )

    def test_conditional_create_alone_is_clean(self):
        # No blanket ban on IF NOT EXISTS: without a following
        # non-concurrent index there is nothing to flag.
        self.assertEqual(
            self.rules(
                "CREATE TABLE IF NOT EXISTS guilds (id bigint PRIMARY KEY);\n"
            ),
            [],
        )

    def test_concurrent_index_after_conditional_create_is_clean(self):
        self.assertEqual(
            self.rules(
                "CREATE TABLE IF NOT EXISTS guilds (id bigint PRIMARY KEY);\n"
                "CREATE INDEX CONCURRENTLY guilds_idx ON guilds (id);\n"
            ),
            [],
        )
        self.assertEqual(
            self.rules(
                "CREATE TABLE IF NOT EXISTS guilds (id bigint PRIMARY KEY);\n"
                "CREATE UNIQUE INDEX CONCURRENTLY guilds_uidx ON guilds (id);\n"
            ),
            [],
        )

    def test_index_without_any_create_still_flags(self):
        self.assertEqual(
            self.rules("CREATE INDEX guilds_idx ON guilds (id);\n"),
            ["creating-index"],
        )

    def test_index_before_conditional_create_still_flags(self):
        self.assertEqual(
            self.rules(
                "CREATE INDEX guilds_idx ON guilds (id);\n"
                "CREATE TABLE IF NOT EXISTS guilds (id bigint PRIMARY KEY);\n"
            ),
            ["creating-index"],
        )

    def test_conditional_create_does_not_exempt_other_table(self):
        self.assertEqual(
            self.rules(
                "CREATE TABLE IF NOT EXISTS guilds (id bigint PRIMARY KEY);\n"
                "CREATE INDEX members_idx ON members (id);\n"
            ),
            ["creating-index"],
        )

    def test_other_rules_unaffected_by_conditional_create(self):
        self.assertEqual(
            squawk.lint_text(
                "CREATE TABLE IF NOT EXISTS guilds (id bigint PRIMARY KEY);\n"
                "ALTER TABLE guilds ADD COLUMN flags bigint NOT NULL;\n"
                "ALTER TABLE guilds DROP COLUMN flags;\n"
            ),
            [(2, "adding-required-field"), (3, "dropping-object")],
        )


class ConditionalCreateCliTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(
            dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
        )
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def write_first(self, sql):
        target = self.root / FIRST
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(sql)

    def run_checker(self, *args):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = squawk.main(["--root", str(self.root), *args])
        return code, out.getvalue()

    CONDITIONAL = (
        "CREATE TABLE IF NOT EXISTS guilds (id bigint PRIMARY KEY);\n"
        "CREATE INDEX guilds_idx ON guilds (id);\n"
    )
    UNCONDITIONAL = (
        "CREATE TABLE guilds (id bigint PRIMARY KEY);\n"
        "CREATE INDEX guilds_idx ON guilds (id);\n"
    )

    def test_strict_fails_on_conditional_create_index(self):
        self.write_first(self.CONDITIONAL)
        code, out = self.run_checker("--strict")
        self.assertEqual(code, 1)
        self.assertIn("::error file=", out)
        self.assertIn("[squawk:creating-index]", out)

    def test_advisory_reports_conditional_create_index_but_passes(self):
        self.write_first(self.CONDITIONAL)
        code, out = self.run_checker()
        self.assertEqual(code, 0)
        self.assertIn("::warning file=", out)
        self.assertIn("[squawk:creating-index]", out)
        self.assertIn("(advisory)", out)

    def test_strict_still_passes_unconditional_create_index(self):
        self.write_first(self.UNCONDITIONAL)
        code, out = self.run_checker("--strict")
        self.assertEqual(code, 0)
        self.assertIn("no risky migration statements", out)

    def test_advisory_still_clean_for_unconditional_create_index(self):
        self.write_first(self.UNCONDITIONAL)
        code, out = self.run_checker()
        self.assertEqual(code, 0)
        self.assertIn("no risky migration statements", out)


if __name__ == "__main__":
    unittest.main()
