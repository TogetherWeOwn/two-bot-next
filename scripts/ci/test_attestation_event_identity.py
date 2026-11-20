"""Synthetic, inactive event/run identity binding regressions."""
import copy
import unittest

from test_attestation_snapshot import inventory, pull_request


def event():
    return {"repository": {"id": 1, "full_name": "org/repo"},
            "number": 1, "pull_request": pull_request()}


class EventIdentity(unittest.TestCase):
    def test_valid_fork_and_same_repository_heads(self):
        for head_repo in ({"id": 2, "full_name": "fork/repo"},
                          {"id": 1, "full_name": "org/repo"}):
            data = event()
            data["pull_request"]["head"]["repo"] = head_repo
            self.assertEqual(inventory.validate_event_identity(data, "org/repo", "a" * 40),
                             inventory.snapshot(data["pull_request"]))

    def test_mismatched_and_malformed_fields_fail_closed(self):
        cases = (("repository", None), ("repository.id", True), ("repository.id", 2),
                 ("repository.full_name", "other/repo"), ("number", True),
                 ("number", 2), ("number", "1"), ("pull_request", None),
                 ("pull_request.number", 2), ("pull_request.base.ref", "dev"),
                 ("pull_request.base.sha", "b" * 40),
                 ("pull_request.base.repo.id", 2),
                 ("pull_request.base.repo.full_name", "other/repo"),
                 ("pull_request.head.repo", None), ("pull_request.head.sha", "A" * 40))
        for path, value in cases:
            data = event()
            target = data
            keys = path.split(".")
            for key in keys[:-1]:
                target = target[key]
            target[keys[-1]] = value
            with self.subTest(path=path), self.assertRaises(inventory.InventoryError) as error:
                inventory.validate_event_identity(data, "org/repo", "a" * 40)
            self.assertNotIn("other/repo", str(error.exception))

    def test_missing_fields_and_untrusted_context_rejected(self):
        for path in ("repository", "number", "pull_request", "repository.id",
                     "repository.full_name", "pull_request.base.repo", "pull_request.body"):
            data = copy.deepcopy(event())
            keys = path.split(".")
            target = data
            for key in keys[:-1]:
                target = target[key]
            del target[keys[-1]]
            with self.subTest(path=path), self.assertRaises(inventory.InventoryError):
                inventory.validate_event_identity(data, "org/repo", "a" * 40)
        for repo, sha in ((None, "a" * 40), ("org/repo\n", "a" * 40),
                          ("ORG/repo", "a" * 40), ("org/repo", None),
                          ("org/repo", "A" * 40), ("org/repo", "b" * 40)):
            with self.subTest(repo=repo, sha=sha), self.assertRaises(inventory.InventoryError):
                inventory.validate_event_identity(event(), repo, sha)
        for data in (None, [], {}):
            with self.subTest(data=data), self.assertRaises(inventory.InventoryError):
                inventory.validate_event_identity(data, "org/repo", "a" * 40)


if __name__ == "__main__":
    unittest.main()
