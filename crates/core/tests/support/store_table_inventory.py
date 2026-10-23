"""Fail-closed store DDL inventory, consumed by backup_schema_roundtrip.rs.

This is deliberately not a SQL parser. Bare persistent CREATEs and one complete,
transaction-local authority reference are the supported migration conventions.
Run --self-test offline; the Rust target runs those same controls in CI.
"""

import json
from pathlib import Path
import re
import sys
import unittest


# Account for the whole lifecycle, not a table-name exclusion. A different
# derivation, template, reassignment or DROP must earn an explicit review.
AUTHORITY_REFERENCE = """
DO $$
DECLARE
  reference_name TEXT;
  reference_table REGCLASS;
BEGIN
  SELECT format('%I.admin_api_token_authority_contract', n.nspname)
    INTO reference_name
    FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE c.oid = 'admin_api_tokens'::regclass;
  EXECUTE format($contract$
    CREATE TABLE %s (
      holder_id TEXT,
      holder_kind TEXT,
      scopes TEXT[],
      issued_at TIMESTAMPTZ,
      expires_at TIMESTAMPTZ,
      CONSTRAINT admin_api_tokens_holder_named
        CHECK (holder_id ~ '[^[:space:]]'),
      CONSTRAINT admin_api_tokens_holder_kind_closed
        CHECK (holder_kind IN ('human', 'ai_agent')),
      CONSTRAINT admin_api_tokens_admin_human
        CHECK (holder_kind = 'human' OR NOT ('admin' = ANY(scopes))),
      CONSTRAINT admin_api_tokens_lifetime_bounded
        CHECK (isfinite(issued_at) AND isfinite(expires_at)
               AND expires_at > issued_at
               AND EXTRACT(EPOCH FROM expires_at) - EXTRACT(EPOCH FROM issued_at) <= 7776000)
    )
  $contract$, reference_name);
  reference_table := reference_name::regclass;
  IF EXISTS (
    SELECT 1 FROM pg_constraint actual
    JOIN pg_constraint canonical ON canonical.conname = actual.conname
    WHERE actual.conrelid = 'admin_api_tokens'::regclass
      AND canonical.conrelid = reference_table
      AND (actual.contype <> 'c' OR actual.connoinherit
           OR pg_get_expr(actual.conbin, actual.conrelid)
              IS DISTINCT FROM pg_get_expr(canonical.conbin, canonical.conrelid))
  ) THEN
    RAISE EXCEPTION 'admin_api_tokens authority constraints must match canonical inheritable CHECKs';
  END IF;
  EXECUTE format('DROP TABLE %s', reference_name);
END $$;
"""
REFERENCE_MIGRATION = "0463_admin_api_token_authority.sql"
LOCK = "LOCK TABLE admin_api_tokens IN ACCESS EXCLUSIVE MODE;"
LEGACY_ALTERS = (
    "EXECUTE format('ALTER TABLE %I.events ALTER COLUMN recorded_at DROP DEFAULT', c.table_schema);",
    "EXECUTE format('ALTER TABLE %I.%I ALTER COLUMN %I TYPE timestamptz USING %I::timestamptz', c.table_schema, c.table_name, c.column_name, c.column_name);",
)
# Keep SQL string contents: dynamic DDL is real DDL too. Ignore comments without
# accidentally treating '--' inside a quoted string as a comment delimiter.
COMMENTS_OR_STRINGS = re.compile(r"'(?:''|[^'])*'|--[^\n]*|/\*.*?\*/", re.DOTALL)
CREATE_TABLE = re.compile(
    r"\bCREATE\s+(?:(?:GLOBAL|LOCAL)\s+)?(?:(?:TEMPORARY|TEMP|UNLOGGED|FOREIGN)\s+)?TABLE\b",
    re.IGNORECASE,
)
BARE_TABLE = re.compile(
    r"\s+(?:IF\s+NOT\s+EXISTS\s+)?([a-z_][a-z_0-9]*)\s*\(", re.IGNORECASE
)


def normalized(sql):
    def keep_string(match):
        value = match.group()
        return value if value.startswith("'") else " "

    return " ".join(COMMENTS_OR_STRINGS.sub(keep_string, sql).split())


def migration_inventory(name, sql):
    text = normalized(sql)
    contract = normalized(AUTHORITY_REFERENCE)
    references = []
    start = text.find(contract)
    if name == REFERENCE_MIGRATION and start == -1:
        raise ValueError(f"{name}: unsupported authority reference lifecycle")
    if start != -1:
        if name != REFERENCE_MIGRATION or text.count(contract) != 1:
            raise ValueError(f"{name}: unexpected/duplicated authority reference")
        # SQLx's file transaction is part of the accounting, not an assumption
        # justified by the helper's name. No opt-out or transaction boundary.
        if re.search(r"^\s*--\s*no-transaction\b", sql, re.MULTILINE | re.IGNORECASE):
            raise ValueError(f"{name}: reference requires SQLx file transaction")
        if re.search(r"\b(?:COMMIT|ROLLBACK)\b|\bSTART\s+TRANSACTION\b", text, re.IGNORECASE):
            raise ValueError(f"{name}: transaction boundary around reference")
        if LOCK not in text[:start]:
            raise ValueError(f"{name}: reference must follow locked registry")
        references.append({"migration": name, "table": "admin_api_token_authority_contract"})
    # Existing timestamp conversions dynamically ALTER tables, never create
    # them. Every other EXECUTE must be the explicitly accounted reference;
    # concatenated templates/SQL variables cannot silently escape CREATE scans.
    for execute in re.finditer(r"\bEXECUTE\b", text, re.IGNORECASE):
        if start != -1 and start <= execute.start() < start + len(contract):
            continue
        if not any(text[execute.start():].startswith(alter) for alter in LEGACY_ALTERS):
            raise ValueError(f"{name}: unknown/malformed dynamic DDL")
    for drop in re.finditer(r"\bDROP\s+TABLE\b", text, re.IGNORECASE):
        if start == -1 or not start <= drop.start() < start + len(contract):
            raise ValueError(f"{name}: unaccounted DROP TABLE outside reference lifecycle")
    tables = set()
    for create in CREATE_TABLE.finditer(text):
        if start != -1 and start <= create.start() < start + len(contract):
            continue  # Explicitly accounted above, never a persistent exclusion.
        tail = text[create.end():]
        match = BARE_TABLE.match(tail)
        if not match:
            raise ValueError(f"{name}: unknown/malformed CREATE TABLE: {tail[:80]}")
        if create.group().upper() not in ("CREATE TABLE",):
            raise ValueError(f"{name}: unsupported table persistence")
        tables.add(match.group(1).lower())
    return tables, references


def inventory(migrations, classified):
    if not migrations:
        raise ValueError("no store migrations found")
    tables = set()
    references = []
    for name, sql in migrations:
        found, transient = migration_inventory(name, sql)
        tables.update(found)
        references.extend(transient)
    unknown = tables - set(classified)
    if unknown:
        raise ValueError(f"store-chain tables neither covered nor excluded: {sorted(unknown)}")
    return {"tables": sorted(tables), "transaction_local": references}


class InventoryControls(unittest.TestCase):
    # These mutate actual migration inputs to the inventory used by Rust, not a
    # duplicate scanner or an expected list manufactured from scanner output.
    def setUp(self):
        self.root = Path(__file__).resolve().parents[3]
        self.sql = (self.root / "store/migrations" / REFERENCE_MIGRATION).read_text()

    def scan(self, sql, classified=(), name=REFERENCE_MIGRATION):
        return inventory([(name, sql)], classified)

    def changed(self, old, new):
        self.assertIn(old, self.sql, "mutation must hit real migration")
        return self.sql.replace(old, new)

    def test_real_reference_is_accounted_not_persistently_excluded(self):
        self.assertEqual(self.scan(self.sql), {
            "tables": [], "transaction_local": [{
                "migration": REFERENCE_MIGRATION,
                "table": "admin_api_token_authority_contract",
            }],
        })

    def test_cutover_mirror_has_identical_supported_lifecycle(self):
        sql = (self.root / "cutover/migrations/0464_admin_api_token_authority.sql").read_text()
        # Same SQL contract, but this API inventories the store chain only.
        self.assertEqual(self.scan(sql), self.scan(self.sql))

    def test_bare_tables_and_journals_remain_discoverable(self):
        result = self.scan(self.sql + "\nCREATE TABLE rollback_journal (id BIGINT);\n"
                           "CREATE TABLE IF NOT EXISTS rollback_watermarks (id BIGINT);\n",
                           ("rollback_journal", "rollback_watermarks"))
        self.assertEqual(result["tables"], ["rollback_journal", "rollback_watermarks"])
        self.assertEqual(len(result["transaction_local"]), 1)

    def test_persistent_helper_name_is_not_an_exemption(self):
        with self.assertRaisesRegex(ValueError, "neither covered nor excluded"):
            self.scan(self.sql + "\nCREATE TABLE admin_api_token_authority_contract (id TEXT);")

    def test_new_unclassified_persistent_table_fails_actual_inventory(self):
        with self.assertRaisesRegex(ValueError, "unclassified_fixture"):
            self.scan(self.sql + "\nCREATE TABLE unclassified_fixture (id TEXT);")

    def test_unknown_dynamic_templates_are_not_ignored(self):
        for ddl in (
            "EXECUTE format('CREATE TABLE %I (id TEXT)', table_name);",
            "EXECUTE format($other$CREATE TABLE %s (id TEXT)$other$, reference_name);",
            "EXECUTE 'create table public.unknown_fixture (id TEXT)';",
            "EXECUTE format('CREATE ' || 'TABLE %I (id TEXT)', table_name);",
            "EXECUTE statement_from_variable;",
            "EXECUTE format('ALTER TABLE %I ADD COLUMN id TEXT; CREATE ' || 'TABLE hidden (id TEXT)', table_name);",
        ):
            with self.subTest(ddl=ddl), self.assertRaisesRegex(ValueError, "unknown/malformed"):
                self.scan(self.sql + "\n" + ddl)

    def test_malformed_qualified_and_quoted_names_refuse(self):
        for ddl in ("CREATE TABLE (id TEXT);", "CREATE TABLE public.unknown_fixture (id TEXT);",
                    'CREATE TABLE "unknown_fixture" (id TEXT);', "CREATE TABLE 123bad (id TEXT);"):
            with self.subTest(ddl=ddl), self.assertRaisesRegex(ValueError, "unknown/malformed"):
                self.scan(self.sql + "\n" + ddl)

    def test_unsupported_table_persistence_is_not_silently_skipped(self):
        for modifier in ("TEMP", "TEMPORARY", "GLOBAL TEMP", "LOCAL TEMPORARY", "UNLOGGED", "FOREIGN"):
            with self.subTest(modifier=modifier), self.assertRaisesRegex(ValueError, "unsupported table persistence"):
                self.scan(self.sql + f"\nCREATE {modifier} TABLE rollback_journal (id TEXT);", ("rollback_journal",))

    def test_existing_dynamic_alters_do_not_hide_table_creations(self):
        name = "0405_timestamptz_and_boolean.sql"
        sql = (self.root / "store/migrations" / name).read_text()
        self.assertEqual(self.scan(sql, name=name), {"tables": [], "transaction_local": []})
        with self.assertRaisesRegex(ValueError, "unknown/malformed"):
            self.scan(sql + "\nEXECUTE format('CREATE ' || 'TABLE hidden (id TEXT)');", name=name)

    def test_standalone_drop_outside_real_reference_lifecycle_refuses(self):
        for ddl in (
            "DROP TABLE rollback_journal;",
            "drop table rollback_journal;",
            "DrOp TaBlE rollback_journal;",
            "DROP TABLE IF EXISTS rollback_journal;",
            "drop table if exists rollback_journal;",
            "DrOp TaBlE If ExIsTs rollback_journal;",
        ):
            with self.subTest(ddl=ddl), self.assertRaisesRegex(
                ValueError,
                "^" + re.escape(REFERENCE_MIGRATION
                    + ": unaccounted DROP TABLE outside reference lifecycle") + "$",
            ):
                self.scan(self.sql + "\n" + ddl)

    def test_standalone_drop_in_ordinary_migration_refuses(self):
        name = "0405_timestamptz_and_boolean.sql"
        sql = (self.root / "store/migrations" / name).read_text()
        self.assertEqual(self.scan(sql, name=name), {"tables": [], "transaction_local": []})
        for ddl in (
            "DROP TABLE rollback_journal;",
            "drop table rollback_journal;",
            "DrOp TaBlE rollback_journal;",
            "DROP TABLE IF EXISTS rollback_journal;",
            "drop table if exists rollback_journal;",
            "DrOp TaBlE If ExIsTs rollback_journal;",
        ):
            with self.subTest(ddl=ddl), self.assertRaisesRegex(
                ValueError,
                "^" + re.escape(name
                    + ": unaccounted DROP TABLE outside reference lifecycle") + "$",
            ):
                self.scan(sql + "\n" + ddl, name=name)

    def test_identity_derivation_template_and_drop_must_match(self):
        for old, new in (
            ("%I.admin_api_token_authority_contract", "%I.other_authority_reference"),
            ("%I.admin_api_token_authority_contract", "public.admin_api_token_authority_contract"),
            ("n.nspname)", "current_schema())"),
            ("WHERE c.oid = 'admin_api_tokens'::regclass;", "WHERE c.relname = 'admin_api_tokens';"),
            ("CREATE TABLE %s (", "CREATE TABLE %I ("),
            ("$contract$, reference_name);", "$contract$, 'public.other_reference');"),
            ("EXECUTE format('DROP TABLE %s', reference_name);", ""),
            ("'DROP TABLE %s', reference_name", "'DROP TABLE %s', 'public.other_reference'"),
            ("reference_table := reference_name::regclass;", "reference_name := 'public.other_reference';"),
            ("END $$;\n\nALTER TABLE", "END $$;\n\nEXECUTE format('DROP TABLE %s', reference_name);\nALTER TABLE"),
        ):
            with self.subTest(old=old), self.assertRaises(ValueError):
                self.scan(self.changed(old, new))

    def test_reference_requires_lock_file_transaction_and_single_lifecycle(self):
        for sql in (
            self.changed(LOCK, ""),
            "-- no-transaction\n" + self.sql,
            self.sql + "\nCOMMIT;",
            self.sql + "\nROLLBACK;",
            self.sql + "\nSTART TRANSACTION;",
            self.sql + AUTHORITY_REFERENCE,
        ):
            with self.subTest(sql=sql[-80:]), self.assertRaises(ValueError):
                self.scan(sql)
        with self.assertRaisesRegex(ValueError, "unexpected"):
            self.scan(self.sql, name="9999_other_migration.sql")

    def test_comments_do_not_manufacture_tables_and_inline_ddl_is_visible(self):
        result = self.scan(self.sql + "\n-- CREATE TABLE ignored_comment (id TEXT);\n"
                           "/* CREATE TABLE ignored_block (id TEXT); */\n"
                           "DO $$ BEGIN CREATE TABLE rollback_journal (id TEXT); END $$;",
                           ("rollback_journal",))
        self.assertEqual(result["tables"], ["rollback_journal"])

    def test_disabled_scanning_cannot_pass_the_negative_control(self):
        # An empty scan must not satisfy either the ordinary-table or reference
        # accounting positive controls; unknown tables must raise, not disappear.
        with self.assertRaisesRegex(ValueError, "no store migrations"):
            inventory([], ())
        with self.assertRaisesRegex(ValueError, "unclassified_fixture"):
            self.scan("CREATE TABLE unclassified_fixture (id TEXT);", name="9998_fixture.sql")


def main():
    if sys.argv[1:] == ["--self-test"]:
        unittest.main(argv=[sys.argv[0]], verbosity=2)
        return
    directory = Path(sys.argv[1])
    migrations = [(p.name, p.read_text()) for p in sorted(directory.glob("*.sql"))]
    try:
        result = inventory(migrations, sys.argv[2:])
    except ValueError as error:
        print(str(error), file=sys.stderr)
        raise SystemExit(1) from None
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    main()
