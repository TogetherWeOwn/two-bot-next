"""Offline contracts for target-qualified, fail-closed nightly partitioning."""
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "nightly_partition", ROOT / ".github/scripts/nightly_partition.py"
)
nightly = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(nightly)


class InventoryTests(unittest.TestCase):
    def setUp(self):
        self.inventory = {
            "bot|bin|two-bot": [f"tests::case_{i}" for i in range(60)],
            "core|lib|two_bot_core": ["tests::case_0", "tests::unicodé"],
            "core|test|empty": [],
        }

    def test_complete_disjoint_and_splits_inside_bot_binary(self):
        parts = nightly.partition(self.inventory)
        expected = {(target, name) for target, names in self.inventory.items() for name in names}
        actual = [tuple(identity) for part in parts for identity in part]
        self.assertEqual(set(actual), expected)
        self.assertEqual(len(actual), len(expected))
        self.assertEqual(len(parts), 6)
        self.assertTrue(all(any(target == "bot|bin|two-bot" for target, _ in part) for part in parts))
        self.assertEqual(parts, nightly.partition(dict(reversed(list(self.inventory.items())))))

    def test_duplicate_names_across_targets_remain_distinct(self):
        actual = [tuple(identity) for part in nightly.partition(self.inventory) for identity in part]
        self.assertIn(("bot|bin|two-bot", "tests::case_0"), actual)
        self.assertIn(("core|lib|two_bot_core", "tests::case_0"), actual)
        self.assertEqual(actual.count(("core|lib|two_bot_core", "tests::case_0")), 1)

    def test_rejects_empty_unknown_or_duplicate_inventory(self):
        for inventory in [{}, {"x": []}, {"x": ["same", "same"]}, {"x": [""]}]:
            with self.subTest(inventory=inventory), self.assertRaises(ValueError):
                nightly.partition(inventory)

    def test_list_parser_rejects_unknown_output_and_duplicate_test_names(self):
        self.assertEqual(nightly.parse_list("tests::one: test\ntests::two: test\n"), ["tests::one", "tests::two"])
        for output in ["not libtest\n", "same: test\nsame: test\n", "one: benchmark\n"]:
            with self.subTest(output=output), self.assertRaises(ValueError):
                nightly.parse_list(output)

    def test_execution_evidence_rejects_missing_extra_or_ignored_tests(self):
        expected = ["tests::one", "tests::two"]
        good = "test tests::one ... ok\ntest tests::two ... ok\n\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.01s\n"
        nightly.check_execution(good, expected)
        for bad in ["", good.replace("tests::two", "unknown"), good.replace("2 passed", "1 passed"), good.replace("0 ignored", "1 ignored"), good + "test tests::one ... ok\n"]:
            with self.subTest(output=bad), self.assertRaises(ValueError):
                nightly.check_execution(bad, expected)

    def test_expected_panic_output_uses_original_test_identity(self):
        good = "test tests::rejects_input - should panic ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n"
        nightly.check_execution(good, ["tests::rejects_input"])
        with self.assertRaises(ValueError):
            nightly.check_execution(good, ["tests::other"])


class AggregateTests(unittest.TestCase):
    def setUp(self):
        self.inventory = {"bot|bin|two-bot": [f"case_{i}" for i in range(60)]}
        self.plan = {"version": 1, "sha": "abc", "inventory": self.inventory,
                     "partitions": nightly.partition(self.inventory)}
        self.needs = {job: {"result": "success"} for job in nightly.JOBS}
        self.needs["changes"]["outputs"] = {"rust": "true"}
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.receipts = []
        for i, identities in enumerate(self.plan["partitions"]):
            receipt = {"version": 1, "sha": "abc", "partition": i,
                       "plan_digest": nightly.digest(self.plan), "executed": identities}
            self.receipts.append(receipt)
        self.write_receipts()

    def write_receipts(self):
        for i, receipt in enumerate(self.receipts):
            (self.root / f"partition-{i}.json").write_text(json.dumps(receipt))

    def test_all_selected_jobs_and_exact_inventory_are_required(self):
        nightly.aggregate(self.needs, self.plan, self.root, "abc")
        for job in nightly.JOBS:
            for result in ["failure", "cancelled", "skipped", ""]:
                needs = json.loads(json.dumps(self.needs))
                needs[job]["result"] = result
                with self.subTest(job=job, result=result), self.assertRaises(ValueError):
                    nightly.aggregate(needs, self.plan, self.root, "abc")

    def test_missing_partition_cannot_pass(self):
        (self.root / "partition-3.json").unlink()
        with self.assertRaises((ValueError, FileNotFoundError)):
            nightly.aggregate(self.needs, self.plan, self.root, "abc")

    def test_wrong_sha_digest_duplicate_and_missing_execution_cannot_pass(self):
        for field, value in [("sha", "wrong"), ("plan_digest", "wrong"), ("executed", []),
                             ("executed", self.receipts[0]["executed"] * 2)]:
            saved = self.receipts[0][field]
            self.receipts[0][field] = value
            self.write_receipts()
            with self.subTest(field=field), self.assertRaises(ValueError):
                nightly.aggregate(self.needs, self.plan, self.root, "abc")
            self.receipts[0][field] = saved
        self.write_receipts()

    def test_only_explicit_deselection_allows_skipped_heavy_jobs(self):
        self.needs["changes"]["outputs"]["rust"] = "false"
        for job in nightly.JOBS - {"changes"}:
            self.needs[job]["result"] = "skipped"
        nightly.aggregate(self.needs, None, self.root, "abc")
        self.needs["sweep-docs"]["result"] = "cancelled"
        with self.assertRaises(ValueError):
            nightly.aggregate(self.needs, None, self.root, "abc")


if __name__ == "__main__":
    unittest.main()
