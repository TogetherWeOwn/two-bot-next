"""Pin scratch archive compatibility to the preserved full legacy definitions."""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
TABLES = {
    "containment_events", "containment_incidents", "join_risk_flags",
    "automation_commands", "scheduled_messages", "automod_violations",
    "automod_processed_messages",
}
FIXTURES = ROOT / "crates/cutover/tests/fixtures/legacy_migrations"


def statements(path):
    sql = re.sub(r"--[^\n]*", "", path.read_text())
    return [" ".join(stmt.split()) for stmt in sql.split(";") if stmt.strip()]


def archive_ddl(stmt):
    table = re.match(r"CREATE TABLE IF NOT EXISTS ([a-z_]+) ", stmt)
    index = re.match(r"CREATE (?:UNIQUE )?INDEX IF NOT EXISTS [a-z_]+ ON ([a-z_]+) ", stmt)
    nonce = stmt == "ALTER TABLE scheduled_messages ADD COLUMN IF NOT EXISTS occurrence_nonce TEXT"
    return nonce or any(match and match[1] in TABLES for match in (table, index))


class DrillSchemaTests(unittest.TestCase):
    def test_exact_full_legacy_ddl_without_backfill(self):
        expected = set()
        for name in ["0013_automod.sql", "0014_automod_idempotency.sql",
                     "0015_anti_nuke_containment.sql", "0015_automations.sql",
                     "0017_scheduled_occurrence_nonce.sql"]:
            expected.update(stmt for stmt in statements(FIXTURES / name) if archive_ddl(stmt))
        actual = statements(ROOT / "crates/bot/src/restore_drill_schema.sql")
        self.assertEqual(set(actual), expected)
        self.assertEqual(len(actual), len(expected), "no duplicate statements")

    def test_only_seven_missing_archive_tables(self):
        actual = statements(ROOT / "crates/bot/src/restore_drill_schema.sql")
        self.assertTrue(all(archive_ddl(stmt) for stmt in actual))
        tables = {match[1] for stmt in actual
                  if (match := re.match(r"CREATE TABLE IF NOT EXISTS ([a-z_]+) ", stmt))}
        self.assertEqual(tables, TABLES)

    def test_compatibility_is_only_in_fresh_scratch_path(self):
        source = (ROOT / "crates/bot/src/restore_drill.rs").read_text()
        fresh = source.split("async fn restore_fresh(", 1)[1].split("async fn run(", 1)[0]
        self.assertLess(fresh.index('CREATE DATABASE'), fresh.index('migrate_pool'))
        self.assertLess(fresh.index('migrate_pool'), fresh.index('restore_drill_schema.sql'))
        self.assertLess(fresh.index('restore_drill_schema.sql'), fresh.index('dump::restore'))
        self.assertEqual(source.count('include_str!("restore_drill_schema.sql")'), 1)

    def test_archive_hash_uses_bounded_reader(self):
        source = (ROOT / "crates/bot/src/restore_drill.rs").read_text()
        self.assertIn("s3::sha256_reader_hex(", source)
        self.assertNotIn("std::fs::read(&archive)", source)

    def test_retained_directory_entry_is_synced_before_allocation(self):
        source = (ROOT / "crates/bot/src/restore_drill.rs").read_text()
        run = source.split("async fn run(", 1)[1].split("pub async fn dispatch(", 1)[0]
        self.assertLess(run.index("if !evidence_root.is_dir()"), run.index(".create(&dir)"))
        self.assertLess(run.index('receipt(&dir, "planned"'), run.index("File::open(evidence_root)"))
        parent_sync = run.split("File::open(evidence_root)", 1)[1]
        self.assertLess(parent_sync.index("root.sync_all()"), parent_sync.index("match restore_fresh("))


if __name__ == "__main__":
    unittest.main()
