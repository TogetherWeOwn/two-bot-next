"""Acceptance fixtures: schema identity in new-table lint exemptions.

A table created unconditionally earlier in the same migration file is
empty, so a non-concurrent index on it needs no CONCURRENTLY. That
exemption belongs to the exact relation that was created -- schema and
table together, after Postgres identifier normalisation. Creating
``scratch.items`` must never exempt an index on ``public.items``: the
two relations only share a final name, and the second may already hold
production data. ``CREATE TABLE IF NOT EXISTS`` earns no exemption even
for the exact same relation: when it already exists the statement is a
no-op.

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


def create_then_index(created, indexed, *, unique=False):
    unique_sql = "UNIQUE " if unique else ""
    return (
        f"CREATE TABLE {created} (id bigint);\n"
        f"CREATE {unique_sql}INDEX items_idx ON {indexed} (id);\n"
    )


class CrossSchemaExemptionTests(unittest.TestCase):
    def rules(self, sql):
        return [rule for _, rule in squawk.lint_text(sql)]

    def test_other_named_schema_does_not_exempt_public_index(self):
        self.assertEqual(
            squawk.lint_text(create_then_index("scratch.items", "public.items")),
            [(2, "creating-index")],
        )

    def test_public_create_does_not_exempt_other_named_schema_index(self):
        self.assertEqual(
            self.rules(create_then_index("public.items", "scratch.items")),
            ["creating-index"],
        )

    def test_unique_index_cross_schema_still_flags(self):
        self.assertEqual(
            self.rules(
                create_then_index("scratch.items", "public.items", unique=True)
            ),
            ["creating-index"],
        )

    def test_if_not_exists_create_in_other_schema_still_flags(self):
        self.assertEqual(
            self.rules(
                "CREATE TABLE IF NOT EXISTS scratch.items (id bigint);\n"
                "CREATE INDEX items_idx ON public.items (id);\n"
            ),
            ["creating-index"],
        )

    def test_only_qualified_index_cross_schema_still_flags(self):
        self.assertEqual(
            self.rules(
                "CREATE TABLE scratch.items (id bigint);\n"
                "CREATE INDEX items_idx ON ONLY public.items (id);\n"
            ),
            ["creating-index"],
        )

    def test_schema_qualified_index_name_does_not_change_table_identity(self):
        # The schema on the index name is not the indexed relation.
        self.assertEqual(
            self.rules(
                "CREATE TABLE scratch.items (id bigint);\n"
                "CREATE INDEX scratch.items_idx ON public.items (id);\n"
            ),
            ["creating-index"],
        )

    def test_unrelated_public_table_does_not_supply_the_missing_relation(self):
        # scratch.items exists and public.* was touched, but public.items
        # itself was never created in this file.
        self.assertEqual(
            self.rules(
                "CREATE TABLE scratch.items (id bigint);\n"
                "CREATE TABLE public.other (id bigint);\n"
                "CREATE INDEX items_idx ON public.items (id);\n"
            ),
            ["creating-index"],
        )

    def test_multiline_cross_schema_index_reports_its_own_line(self):
        self.assertEqual(
            squawk.lint_text(
                "CREATE TABLE scratch.items (\n"
                "  id bigint\n"
                ");\n"
                "CREATE INDEX items_idx ON public.items (id);\n"
            ),
            [(4, "creating-index")],
        )

    def test_index_before_any_create_still_flags(self):
        self.assertEqual(
            squawk.lint_text(
                "CREATE INDEX items_idx ON public.items (id);\n"
                "CREATE TABLE scratch.items (id bigint);\n"
            ),
            [(1, "creating-index")],
        )


class SameQualifiedTableControlTests(unittest.TestCase):
    """Positive controls: the valid same-relation exemption must survive."""

    def rules(self, sql):
        return [rule for _, rule in squawk.lint_text(sql)]

    def test_same_named_schema_and_table_is_clean(self):
        for relation in ("scratch.items", "public.items"):
            with self.subTest(relation=relation):
                self.assertEqual(
                    self.rules(create_then_index(relation, relation)), []
                )

    def test_same_qualified_unique_index_is_clean(self):
        self.assertEqual(
            self.rules(
                create_then_index("scratch.items", "scratch.items", unique=True)
            ),
            [],
        )

    def test_same_qualified_if_not_exists_create_still_flags(self):
        # Conditional creation proves nothing about emptiness even for the
        # exact same qualified relation: when scratch.items already exists
        # the statement is a no-op and the table may hold production data.
        self.assertEqual(
            self.rules(
                "CREATE TABLE IF NOT EXISTS scratch.items (id bigint);\n"
                "CREATE INDEX items_idx ON scratch.items (id);\n"
            ),
            ["creating-index"],
        )

    def test_same_qualified_only_index_is_clean(self):
        self.assertEqual(
            self.rules(
                "CREATE TABLE scratch.items (id bigint);\n"
                "CREATE INDEX items_idx ON ONLY scratch.items (id);\n"
            ),
            [],
        )

    def test_both_schemas_created_then_each_may_be_indexed(self):
        self.assertEqual(
            self.rules(
                "CREATE TABLE scratch.items (id bigint);\n"
                "CREATE TABLE public.items (id bigint);\n"
                "CREATE INDEX a_idx ON public.items (id);\n"
                "CREATE INDEX b_idx ON scratch.items (id);\n"
            ),
            [],
        )


class BareAndPublicCompatibilityTests(unittest.TestCase):
    """The one justified ambiguity: an unqualified name means public.

    That compatibility is limited to the public schema in either
    direction. A bare name paired with any other explicitly named
    schema is ambiguous identity and must not broaden the exemption.
    """

    def rules(self, sql):
        return [rule for _, rule in squawk.lint_text(sql)]

    def test_public_create_matches_bare_index(self):
        self.assertEqual(
            self.rules(create_then_index("public.items", "items")), []
        )

    def test_bare_create_matches_public_index(self):
        self.assertEqual(
            self.rules(create_then_index("items", "public.items")), []
        )

    def test_bare_create_matches_bare_index(self):
        self.assertEqual(self.rules(create_then_index("items", "items")), [])

    def test_unquoted_public_schema_folds_before_bare_match(self):
        self.assertEqual(
            self.rules(create_then_index("PUBLIC.items", "items")), []
        )

    def test_quoted_lowercase_public_matches_bare_both_ways(self):
        self.assertEqual(
            self.rules(create_then_index('"public".items', "items")), []
        )
        self.assertEqual(
            self.rules(create_then_index("items", '"public".items')), []
        )

    def test_other_schema_create_does_not_exempt_bare_index(self):
        self.assertEqual(
            self.rules(create_then_index("scratch.items", "items")),
            ["creating-index"],
        )

    def test_bare_create_does_not_exempt_other_schema_index(self):
        self.assertEqual(
            self.rules(create_then_index("items", "scratch.items")),
            ["creating-index"],
        )

    def test_quoted_capitalised_public_is_not_the_public_schema(self):
        # "Public" is a distinct, case-sensitive schema name, so the
        # public/bare compatibility must not apply to it.
        self.assertEqual(
            self.rules(create_then_index('"Public".items', "items")),
            ["creating-index"],
        )
        self.assertEqual(
            self.rules(create_then_index("items", '"Public".items')),
            ["creating-index"],
        )


class QuotedAndCaseIdentityTests(unittest.TestCase):
    """Postgres normalisation, applied to schema and table alike:
    unquoted identifiers fold to lowercase; quoted ones keep case."""

    def rules(self, sql):
        return [rule for _, rule in squawk.lint_text(sql)]

    def test_unquoted_schema_and_table_fold_case(self):
        self.assertEqual(
            self.rules(create_then_index("Scratch.Items", "SCRATCH.items")),
            [],
        )

    def test_quoted_lowercase_parts_match_unquoted(self):
        self.assertEqual(
            self.rules(create_then_index('"scratch"."items"', "scratch.items")),
            [],
        )
        self.assertEqual(
            self.rules(create_then_index("scratch.items", '"scratch"."items"')),
            [],
        )

    def test_same_quoted_mixed_case_relation_is_clean(self):
        self.assertEqual(
            self.rules(
                create_then_index('"Scratch"."Items"', '"Scratch"."Items"')
            ),
            [],
        )

    def test_quoted_schema_case_distinguishes_relations(self):
        self.assertEqual(
            self.rules(create_then_index('"Scratch".items', "scratch.items")),
            ["creating-index"],
        )
        self.assertEqual(
            self.rules(create_then_index("scratch.items", '"Scratch".items')),
            ["creating-index"],
        )

    def test_quoted_table_case_distinguishes_within_one_schema(self):
        self.assertEqual(
            self.rules(create_then_index('scratch."Items"', "scratch.items")),
            ["creating-index"],
        )

    def test_quoted_lowercase_other_schema_is_still_another_schema(self):
        self.assertEqual(
            self.rules(create_then_index('"scratch".items', "public.items")),
            ["creating-index"],
        )
        self.assertEqual(
            self.rules(create_then_index("public.items", '"scratch".items')),
            ["creating-index"],
        )


class LexerProtectionControlTests(unittest.TestCase):
    """Schema identity work must not weaken comment/literal handling."""

    def rules(self, sql):
        return [rule for _, rule in squawk.lint_text(sql)]

    def test_commented_create_never_exempts(self):
        self.assertEqual(
            self.rules(
                "-- CREATE TABLE scratch.items (id bigint);\n"
                "CREATE INDEX items_idx ON public.items (id);\n"
            ),
            ["creating-index"],
        )
        self.assertEqual(
            self.rules(
                "/* CREATE TABLE public.items (id bigint); */\n"
                "CREATE INDEX items_idx ON public.items (id);\n"
            ),
            ["creating-index"],
        )

    def test_create_inside_string_literal_never_exempts(self):
        self.assertEqual(
            self.rules(
                "SELECT 'CREATE TABLE public.items (id bigint);';\n"
                "CREATE INDEX items_idx ON public.items (id);\n"
            ),
            ["creating-index"],
        )

    def test_create_inside_dollar_quoted_body_never_exempts(self):
        self.assertEqual(
            self.rules(
                "SELECT $$CREATE TABLE public.items (id bigint);$$;\n"
                "CREATE INDEX items_idx ON public.items (id);\n"
            ),
            ["creating-index"],
        )

    def test_commented_index_never_flags(self):
        self.assertEqual(
            self.rules(
                "CREATE TABLE scratch.items (id bigint);\n"
                "/* CREATE INDEX items_idx ON public.items (id); */\n"
            ),
            [],
        )

    def test_other_rules_still_fire_beside_qualified_creates(self):
        self.assertEqual(
            self.rules(
                "CREATE TABLE scratch.items (id bigint);\n"
                "ALTER TABLE public.items ADD COLUMN flags bigint NOT NULL;\n"
            ),
            ["adding-required-field"],
        )


class StrictDispositionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(
            dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
        )
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def run_checker(self, sql, *args):
        target = self.root / FIRST
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(sql)
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = squawk.main(["--root", str(self.root), *args])
        return code, out.getvalue()

    def test_cross_schema_index_fails_strict_mode(self):
        code, out = self.run_checker(
            create_then_index("scratch.items", "public.items"), "--strict"
        )
        self.assertEqual(code, 1)
        self.assertIn("::error file=", out)
        self.assertIn("[squawk:creating-index]", out)

    def test_cross_schema_index_is_advisory_warning_without_strict(self):
        code, out = self.run_checker(
            create_then_index("scratch.items", "public.items")
        )
        self.assertEqual(code, 0)
        self.assertIn("::warning file=", out)
        self.assertIn("[squawk:creating-index]", out)
        self.assertIn("(advisory)", out)

    def test_same_qualified_table_passes_strict_mode(self):
        code, out = self.run_checker(
            create_then_index("scratch.items", "scratch.items"), "--strict"
        )
        self.assertEqual(code, 0)
        self.assertIn("no risky migration statements", out)

    def test_public_bare_compatibility_passes_strict_mode(self):
        code, out = self.run_checker(
            create_then_index("public.items", "items"), "--strict"
        )
        self.assertEqual(code, 0)
        self.assertIn("no risky migration statements", out)


if __name__ == "__main__":
    unittest.main()
