"""Coverage guard regressions: local fixtures only, no Discord or databases."""

import copy
import json
from pathlib import Path
import subprocess
import unittest

from check_soak_checklist import parity_rows, render, validate

ROOT = Path(__file__).resolve().parents[1]


class SoakChecklistTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.parity = (ROOT / "docs/parity.md").read_text()
        cls.checklist = json.loads((ROOT / "docs/soak-checklist.json").read_text())

    def test_repository_coverage_and_render(self):
        counts = validate(self.parity, self.checklist)
        self.assertEqual(set(counts), set(range(1, 9)))
        self.assertEqual((ROOT / "docs/soak-checklist.md").read_text(), render(self.checklist))

    def test_missing_row_fails(self):
        data = copy.deepcopy(self.checklist)
        data["entries"].pop()
        with self.assertRaisesRegex(ValueError, "missing="):
            validate(self.parity, data)

    def test_new_mapped_row_fails(self):
        changed = self.parity.replace("## 9. Drops", "| Behaviour | Detail | Map |\n|---|---|---|\n|new observable | new effect | **S5** |\n\n## 9. Drops")
        with self.assertRaisesRegex(ValueError, "missing="):
            validate(changed, self.checklist)

    def test_drop_ignored_but_mixed_drop_retained(self):
        rows = parity_rows(self.parity)
        self.assertFalse(any("/rota-acknowledge" in str(row) for row in rows))
        self.assertTrue(any("community_facts" in str(row) for row in rows))
        self.assertTrue(any("Operator scripts" in str(row) for row in rows))
        changed = self.parity.replace("## 9. Drops", "| Behaviour | Detail | Map |\n|---|---|---|\n|discarded | reason | **DROP** — intentionally absent |\n\n## 9. Drops")
        self.assertEqual(set(rows), set(parity_rows(changed)))

    def test_mixed_drop_requires_coverage_in_both_orders(self):
        for mapping in (
            "**DROP** (runtime); **S6** (shape-check)",
            "**S6** (shape-check); **DROP** (runtime)",
            "**DROP** (runtime); **B4** (acceptance)",
            "**DROP** (runtime); [TOG-9881](/TOG/issues/TOG-9881)",
            "**DROP** — replaced by session persistence (**S5**)",
            "**DROP** (runtime); **NEW-42** (shape-check)",
        ):
            with self.subTest(mapping=mapping):
                changed = self.parity.replace("## 9. Drops", "| Behaviour | Detail | Map |\n|---|---|---|\n|mixed observable | mapped remainder | " + mapping + " |\n\n## 9. Drops")
                self.assertIn((8, ("mixed observable", "mapped remainder")), parity_rows(changed))
                with self.assertRaisesRegex(ValueError, "missing="):
                    validate(changed, self.checklist)

    def test_drop_only_clauses_are_excluded(self):
        changed = self.parity.replace("## 9. Drops", "| Behaviour | Detail | Map |\n|---|---|---|\n|discarded | reason | **DROP** (runtime); **DROP** (shape-check) |\n\n## 9. Drops")
        self.assertEqual(set(parity_rows(self.parity)), set(parity_rows(changed)))

    def test_duplicate_rows_and_ids_fail(self):
        data = copy.deepcopy(self.checklist)
        data["entries"].append(copy.deepcopy(data["entries"][0]))
        with self.assertRaisesRegex(ValueError, "Duplicate"):
            validate(self.parity, data)
        data["entries"][-1]["parity"]["row"] = ["bogus"]
        with self.assertRaisesRegex(ValueError, "Duplicate"):
            validate(self.parity, data)

    def test_changed_options_and_stale_entry_fail(self):
        changed = self.parity.replace("`member` User opt", "`member` User req", 1)
        with self.assertRaisesRegex(ValueError, "stale="):
            validate(changed, self.checklist)

    def test_repeated_surfaces_are_distinct(self):
        rows = parity_rows(self.parity)
        ready = [row for row in rows if row[0] == 3 and row[1][0] == "`ClientReady`"]
        self.assertEqual(len(ready), 2)
        data = copy.deepcopy(self.checklist)
        data["entries"] = [e for e in data["entries"] if (e["parity"]["section"], tuple(e["parity"]["row"])) != ready[0]]
        with self.assertRaisesRegex(ValueError, "Coverage mismatch"):
            validate(self.parity, data)

    def test_invalid_status_and_blank_fields_fail(self):
        for field, value in (("status", "passed"), ("action", " "), ("evidence", ""), ("expected", "")):
            with self.subTest(field=field):
                data = copy.deepcopy(self.checklist)
                data["entries"][0][field] = value
                with self.assertRaises(ValueError):
                    validate(self.parity, data)

    def test_waiver_requires_reason_and_automation_requires_command(self):
        for status, required in (("waived", "reason"), ("automated", "verification")):
            data = copy.deepcopy(self.checklist)
            data["entries"][0]["status"] = status
            data["entries"][0].pop(required, None)
            with self.assertRaisesRegex(ValueError, "requires"):
                validate(self.parity, data)

    def test_voice_requires_external_reference(self):
        data = copy.deepcopy(self.checklist)
        voice = next(e for e in data["entries"] if "VoiceStateUpdate" in str(e["parity"]))
        voice.pop("reference")
        with self.assertRaisesRegex(ValueError, "TOG-10119"):
            validate(self.parity, data)

    def test_rest_verification_propagates_either_failure(self):
        entry = next(e for e in self.checklist["entries"] if e["id"] == "s6-01")
        prefix = "python3 scripts/cargo_cache.py run -- test -p two-bot-discord --test "
        for first, second in ((0, 0), (17, 0), (0, 23)):
            with self.subTest(first=first, second=second):
                command = entry["verification"].replace(prefix + "executor_acceptance", f"(exit {first})")
                command = command.replace(prefix + "executor_regressions", f"(exit {second})")
                self.assertNotIn("python3", command)
                # Only exit-status doubles execute: no Cargo, DB or network.
                result = subprocess.run(["sh", "-c", command], check=False)
                self.assertEqual(result.returncode, first or second)

    def test_config_prose_is_covered(self):
        data = copy.deepcopy(self.checklist)
        data["entries"] = [e for e in data["entries"] if e["parity"]["section"] != 7]
        with self.assertRaisesRegex(ValueError, "Config / env catalogue"):
            validate(self.parity, data)
        with self.assertRaisesRegex(ValueError, "config classes"):
            parity_rows(self.parity.replace("env_only", "unknown_class"))

    def test_malformed_table_and_missing_section_fail_closed(self):
        with self.assertRaisesRegex(ValueError, "malformed"):
            parity_rows(self.parity.replace("| 1 | `/rank` |", "| `/rank` |", 1))
        with self.assertRaisesRegex(ValueError, "sections 1–8"):
            parity_rows(self.parity.replace("## 8. Observable behaviours", "## 80. Observable behaviours"))
        with self.assertRaisesRegex(ValueError, "must end in Map"):
            parity_rows(self.parity.replace("| Behaviour | Detail | Map |", "| Behaviour | Detail | Mapping |"))

    def test_escaped_pipe_stays_in_cell(self):
        changed = self.parity.replace("## 9. Drops", "| Behaviour | Detail | Map |\n|---|---|---|\n|new observable | one \\| two | **S5** |\n\n## 9. Drops")
        self.assertIn((8, ("new observable", "one | two")), parity_rows(changed))


if __name__ == "__main__":
    unittest.main()
