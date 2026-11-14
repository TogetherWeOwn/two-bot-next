"""Direct metadata snapshot regressions adapted from the base attestation gate."""
import copy
import importlib.util
from pathlib import Path
import unittest


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


if __name__ == "__main__":
    unittest.main()
