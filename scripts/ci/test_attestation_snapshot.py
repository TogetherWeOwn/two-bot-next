"""Direct metadata snapshot regressions adapted from the base attestation gate."""
import copy
import importlib.util
from pathlib import Path
import unittest
from unittest import mock


spec = importlib.util.spec_from_file_location(
    "attestation_inventory", Path(__file__).resolve().parents[2] / ".github/scripts/attestation_inventory.py"
)
inventory = importlib.util.module_from_spec(spec)
spec.loader.exec_module(inventory)


def pull_request():
    return {"number": 1, "state": "open", "title": "title", "body": None,
            "user": {"login": "author"}, "updated_at": "timestamp", "changed_files": 3000,
            "base": {"ref": "main", "sha": "a" * 40, "repo": {"id": 1, "full_name": "org/repo"}},
            "head": {"ref": "branch", "sha": "b" * 40, "repo": {"id": 2, "full_name": "fork/repo"}}}


class Snapshot(unittest.TestCase):
    def test_snapshot_includes_all_fields_and_fork_identities(self):
        pr = pull_request()
        self.assertEqual(inventory.snapshot(pr),
                         (1, "open", "title", None, "author", "timestamp", 3000,
                          "main", "a" * 40, (1, "org/repo"),
                          "branch", "b" * 40, (2, "fork/repo")))
        for path, replacement in (("number", 2), ("title", "other"), ("body", "body"),
                                  ("user.login", "other"), ("updated_at", "later"),
                                  ("changed_files", 1), ("base.ref", "other"),
                                  ("base.sha", "c" * 40), ("base.repo.id", 3),
                                  ("base.repo.full_name", "other/repo"), ("head.ref", "other"),
                                  ("head.sha", "d" * 40), ("head.repo.id", 4),
                                  ("head.repo.full_name", "other/repo")):
            changed = copy.deepcopy(pr)
            keys = path.split(".")
            target = changed
            for key in keys[:-1]:
                target = target[key]
            target[keys[-1]] = replacement
            with self.subTest(path=path):
                self.assertNotEqual(inventory.snapshot(pr), inventory.snapshot(changed))

    def test_invalid_shapes_are_rejected(self):
        for path, values in (("number", (0, True, "1")), ("state", ("closed", None)),
                             ("title", ("", None)), ("body", (1, [])),
                             ("user", (None, {}, [])), ("updated_at", ("", None)),
                             ("changed_files", (0, 3001, True, 1.0, None)),
                             ("base", (None, [])), ("head", (None, [])),
                             ("base.ref", ("", None)), ("head.ref", ("", None)),
                             ("base.sha", ("A" * 40, "a" * 39, None)),
                             ("head.sha", ("g" * 40, None)),
                             ("base.repo", (None, [])), ("head.repo", (None, [])),
                             ("base.repo.id", (0, True, "1")),
                             ("head.repo.id", (0, True, "1")),
                             ("base.repo.full_name", ("org", "/repo", "org/repo/extra")),
                             ("head.repo.full_name", (None, "org/", "org/repo\n"))):
            for value in values:
                pr = pull_request()
                keys = path.split(".")
                target = pr
                for key in keys[:-1]:
                    target = target[key]
                target[keys[-1]] = value
                with self.subTest(path=path, value=value), self.assertRaises(inventory.InventoryError):
                    inventory.snapshot(pr)
        for pr in (None, [], {}):
            with self.subTest(pr=pr), self.assertRaises(inventory.InventoryError):
                inventory.snapshot(pr)
        pr = pull_request()
        del pr["body"]
        with self.assertRaises(inventory.InventoryError):
            inventory.snapshot(pr)


class Freshness(unittest.TestCase):
    def test_equal_snapshot_and_encoded_base_route(self):
        pr = pull_request()
        pr["base"]["ref"] = "release/candidate"
        routes = []

        def api(route):
            routes.append(route)
            return copy.deepcopy(pr) if len(routes) == 1 else {"object": {"sha": "a" * 40}}

        self.assertIsNone(inventory.require_fresh(api, pr))
        self.assertEqual(routes, ["repos/org/repo/pulls/1",
                                  "repos/org/repo/git/ref/heads/release%2Fcandidate"])

    def test_changed_snapshot_fields_fail_closed(self):
        for path, value in (("title", "new"), ("body", "new"), ("updated_at", "later"),
                            ("number", 2), ("changed_files", 1), ("user.login", "other"),
                            ("head.sha", "c" * 40), ("head.ref", "other"),
                            ("head.repo.id", 5), ("base.sha", "d" * 40),
                            ("base.ref", "other"), ("base.repo.full_name", "other/repo")):
            pr = pull_request()
            changed = copy.deepcopy(pr)
            keys = path.split(".")
            target = changed
            for key in keys[:-1]:
                target = target[key]
            target[keys[-1]] = value
            routes = []

            def api(route):
                routes.append(route)
                if route == "repos/org/repo/pulls/1":
                    return changed
                return {"object": {"sha": "a" * 40}}

            with self.subTest(path=path):
                with self.assertRaises(inventory.InventoryError):
                    inventory.require_fresh(api, pr)
                self.assertEqual(routes, ["repos/org/repo/pulls/1"])

    def test_changed_snapshot_regression_detects_comparison_bypass(self):
        expected = inventory.snapshot(pull_request())
        result = unittest.TestResult()
        # Force equal snapshots to model bypassing the inequality rejection.
        with mock.patch.object(inventory, "snapshot", return_value=expected):
            Freshness("test_changed_snapshot_fields_fail_closed").run(result)
        self.assertEqual(len(result.failures), 12)
        self.assertEqual(result.errors, [])
        for _, failure in result.failures:
            self.assertIn("InventoryError not raised", failure)

    def test_missing_malformed_and_failed_reads_are_redacted(self):
        pr = pull_request()
        for response in (None, {}, [], {"object": None}, {"object": {}},
                         {"object": {"sha": "c" * 40}}):
            for bad_read in (0, 1):
                responses = [copy.deepcopy(pr), {"object": {"sha": "a" * 40}}]
                responses[bad_read] = response
                with self.subTest(response=response, bad_read=bad_read):
                    with self.assertRaises(inventory.InventoryError) as error:
                        inventory.require_fresh(lambda route: responses.pop(0), pr)
                    self.assertEqual(str(error.exception), "Freshness comparison failed")
                    self.assertIsNone(error.exception.__cause__)
        for bad_read in (0, 1):
            calls = []

            def failing(route):
                calls.append(route)
                if len(calls) - 1 == bad_read:
                    raise RuntimeError("sensitive sentinel")
                return copy.deepcopy(pr)

            with self.assertRaises(inventory.InventoryError) as error:
                inventory.require_fresh(failing, pr)
            self.assertNotIn("sensitive sentinel", str(error.exception))
            self.assertTrue(error.exception.__suppress_context__)

    def test_invalid_baseline_does_not_read_and_mutation_cannot_change_base(self):
        pr = pull_request()
        del pr["body"]
        calls = []
        with self.assertRaises(inventory.InventoryError):
            inventory.require_fresh(lambda route: calls.append(route), pr)
        self.assertEqual(calls, [])
        pr = pull_request()

        def api(route):
            result = copy.deepcopy(pr)
            pr["base"]["sha"] = "c" * 40
            return result if "/pulls/" in route else {"object": {"sha": "c" * 40}}

        with self.assertRaises(inventory.InventoryError):
            inventory.require_fresh(api, pr)


if __name__ == "__main__":
    unittest.main()
